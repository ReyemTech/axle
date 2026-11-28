//! Persistence: the `Store` trait and its SQLite / Postgres implementations.
//!
//! Every timestamp written is supplied by the caller (computed from a `Clock`); queries
//! never call `now()` / `CURRENT_TIMESTAMP`.

pub mod postgres;
pub mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;
use time::{Duration, OffsetDateTime};

use crate::config::Backend;
use crate::delivery::stages::{
    ChildState, CompletionTarget, completion_target, last_enabled, parent_stage,
};
use crate::domain::{
    Actor, Address, Agent, AgentId, Billing, BlockDetail, DeliverySignal, DeliveryStage,
    Enrollment, EnrollmentId, EnrollmentKind, Importance, InboxItem, LinkKind, Message, MessageId,
    MessageKind, PlanState, PrLinkedBy, Project, ProjectDelivery, ProjectId, Session, SessionId,
    Task, TaskEvent, TaskEventKind, TaskId, TaskLink, TaskPr, TaskStageRow, TaskStatus, TokenId,
    TokenInfo, UsageId, WaitingOn,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A stored value could not be decoded (bad uuid / timestamp text).
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A new enrollment row; `secret_hash` / `user_code_hash` are SHA-256 hex of the secrets.
#[derive(Debug, Clone)]
pub struct NewEnrollment<'a> {
    pub id: EnrollmentId,
    pub kind: EnrollmentKind,
    pub secret_hash: &'a str,
    pub user_code_hash: Option<&'a str>,
    pub label: &'a str,
    pub host: &'a str,
    pub requester_ip: Option<&'a str>,
    pub now: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// A token minted by redeeming an enrollment. It carries no label: the store derives it from
/// the enrollment row inside the consuming transaction.
#[derive(Debug, Clone)]
pub struct NewToken<'a> {
    pub id: TokenId,
    pub hash: &'a str,
}

/// What a device-flow poll found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Pending,
    Denied,
    Expired,
    /// The poll that won: the enrollment is now consumed and its token created.
    Consumed(Enrollment),
}

/// Label of the token minted for `e`: code `<label> (<host>)`, device `<host> (device login)`.
pub(crate) fn enrollment_token_label(e: &Enrollment) -> String {
    match e.kind {
        EnrollmentKind::Code => format!("{} ({})", e.label, e.host),
        EnrollmentKind::Device => format!("{} (device login)", e.host),
    }
}

#[async_trait]
pub trait Store: Send + Sync {
    /// A trivial round trip, for the readiness probe.
    async fn ping(&self) -> StoreResult<()>;

    // ---- tokens ----

    /// Stores a new token by the hex SHA-256 `hash` of its plaintext.
    async fn insert_token(
        &self,
        id: TokenId,
        label: &str,
        hash: &str,
        now: OffsetDateTime,
    ) -> StoreResult<()>;
    /// The unrevoked token whose hash is `hash`, stamping its `last_used_at = now`.
    async fn use_token(&self, hash: &str, now: OffsetDateTime) -> StoreResult<Option<TokenId>>;
    /// Sets `revoked_at` (once; revoking again is a no-op). `NotFound` for an unknown token.
    async fn revoke_token(&self, id: TokenId, now: OffsetDateTime) -> StoreResult<()>;
    /// Every token, oldest first.
    async fn list_tokens(&self) -> StoreResult<Vec<TokenInfo>>;

    // ---- enrollments ----

    /// Stores a new enrollment. In the same transaction, `pending` / `approved` rows whose
    /// `expires_at <= now` become `expired` and rows expired more than a day ago are deleted.
    async fn insert_enrollment(&self, e: &NewEnrollment<'_>) -> StoreResult<()>;
    /// Redeems a one-time code: one transaction that moves the `code` enrollment from
    /// `pending` to `consumed` (recording `host`) and inserts `token`, labelled from the row
    /// (see `enrollment_token_label`). `None` when the code is unknown, used or expired; no
    /// token is created then.
    async fn consume_code(
        &self,
        secret_hash: &str,
        host: &str,
        now: OffsetDateTime,
        token: &NewToken<'_>,
    ) -> StoreResult<Option<Enrollment>>;
    /// The pending, unexpired device enrollment with this user-code hash.
    async fn device_by_user_code(
        &self,
        user_code_hash: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Option<Enrollment>>;
    /// Approves or denies a pending, unexpired device enrollment; `None` if it is neither.
    async fn decide_device(
        &self,
        id: EnrollmentId,
        approve: bool,
        now: OffsetDateTime,
    ) -> StoreResult<Option<Enrollment>>;
    /// The device flow's poll: an `approved`, unexpired enrollment is consumed and `token`
    /// inserted in one transaction; otherwise the row's state is reported.
    async fn poll_device(
        &self,
        secret_hash: &str,
        now: OffsetDateTime,
        token: &NewToken<'_>,
    ) -> StoreResult<PollOutcome>;
    /// Moves a `pending`, unexpired enrollment to `denied` (the UI's Revoke); `false` if it
    /// was not pending.
    async fn cancel_enrollment(&self, id: EnrollmentId, now: OffsetDateTime) -> StoreResult<bool>;
    /// Pending, unexpired enrollments, newest first.
    async fn list_pending_enrollments(&self, now: OffsetDateTime) -> StoreResult<Vec<Enrollment>>;

    // ---- events ----

    /// Appends an event to the outbox. The MVP's one replica delivers it in-process right
    /// after the insert, so the row is written as published (`published_at = now`); a
    /// multi-replica relay would insert it unpublished instead.
    async fn insert_outbox(
        &self,
        kind: &str,
        project: Option<ProjectId>,
        payload: &serde_json::Value,
        now: OffsetDateTime,
    ) -> StoreResult<()>;

    // ---- projects ----

    /// Creates the project on first contact; an existing project is returned unchanged
    /// (its display name is never overwritten).
    async fn upsert_project(
        &self,
        key: &str,
        display_name: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Project>;
    async fn get_project_by_key(&self, key: &str) -> StoreResult<Option<Project>>;
    async fn get_project(&self, id: ProjectId) -> StoreResult<Option<Project>>;
    /// All projects ordered by key.
    async fn list_projects(&self) -> StoreResult<Vec<Project>>;
    /// Changes only the display name; `NotFound` if the project does not exist.
    async fn rename_project(&self, id: ProjectId, display_name: &str) -> StoreResult<()>;

    // ---- delivery settings ----

    /// The project's stored delivery settings; `None` when it has none (no stages).
    async fn get_project_delivery(
        &self,
        project: ProjectId,
    ) -> StoreResult<Option<ProjectDelivery>>;
    /// Every stored delivery settings row, ordered by project id.
    async fn list_project_delivery(&self) -> StoreResult<Vec<ProjectDelivery>>;
    /// Upserts `d` and, in the same transaction, recomputes every `delivering` task of the
    /// project with `completion_target(d.stages, …)`: a task whose target is `Done` becomes
    /// `done` (`done_at = updated_at = d.updated_at`, `version` bumped) with a `delivered`
    /// event (actor the human, note `delivered_note(&d.stages)`, `token` recorded). Done tasks
    /// are never reopened. The parent rule then runs for the parents of moved tasks (as after
    /// any child change). With `unblock`, every task that became `done` unblocks its waiting
    /// tasks in the same transaction (see `UnblockRule`; actor the human). Returns the tasks
    /// moved, as updated (those the stages finished and the parents the parent rule moved),
    /// the tasks the stages finished themselves (with any `children_complete` each recorded)
    /// and the tasks unblocked. A task with a child that is not `done` or `cancelled` is not
    /// finished.
    async fn set_project_delivery(
        &self,
        d: &ProjectDelivery,
        token: Option<TokenId>,
        unblock: Option<UnblockRule>,
    ) -> StoreResult<StagesApplied>;

    // ---- delivery: PR links and completion ----

    /// Links `req.pr` to `req.task` in one transaction (the task row is locked first). Refused
    /// (`Closed`) for a `done` / `cancelled` task. The task's own PR again is `Unchanged`;
    /// with a different PR linked, `replace = false` gives `HasLink`, and a task whose
    /// `delivery_stage` reached `merged` gives `AlreadyMerged` (the linked PR's number). A PR
    /// whose `(repo, number)` or `url` belongs to another task gives `TakenBy`. On link:
    /// replaces any previous row, sets `delivery_stage = max(current, in_pr)`, records
    /// `task_stages(in_pr)` if absent, bumps `version` / `updated_at = now`, and appends
    /// `pr_linked` (payload `url`) or `pr_relinked` (payload `url`, `previous_url`).
    async fn link_pr(&self, req: &LinkPrRequest) -> StoreResult<LinkOutcome>;
    /// Removes the task's PR link, guarded: the task is not `done` / `cancelled` /
    /// `delivering` and its stage is before `merged`. Sets `delivery_stage = NULL`, bumps
    /// `version`, appends `pr_unlinked` (payload `previous_url`). Returns the removed link;
    /// `None` when there was none or the guard did not match.
    async fn unlink_pr(
        &self,
        task: TaskId,
        actor: Actor,
        token: Option<TokenId>,
        now: OffsetDateTime,
    ) -> StoreResult<Option<TaskPr>>;
    /// The task's PR link.
    async fn task_pr(&self, task: TaskId) -> StoreResult<Option<TaskPr>>;
    /// The PR links of the listed tasks (tasks without one are left out), by task id.
    async fn task_prs(&self, tasks: &[TaskId]) -> StoreResult<Vec<TaskPr>>;
    /// The stages a task reached, in stage order.
    async fn task_stages(&self, task: TaskId) -> StoreResult<Vec<TaskStageRow>>;
    /// `task_complete`'s write: the guarded transition of `req.transition` (its `from`,
    /// `owner`, plan, lease and message fields; its `to` is replaced by the target) and, in
    /// the same transaction: links `req.link` (`complete_links`) when the project has enabled
    /// stages, the task has no PR and the PR is not taken (skipped silently otherwise; a
    /// project without stages completes exactly as before delivery), then decides with
    /// `completion_target` from the project's stages, the PR, the task's stage and its
    /// children. `done` sets `done_at`; `delivering` leaves it null and sets `delivery_stage`
    /// to the target stage. The `done` event is appended either way; for `delivering` its
    /// payload gains `"delivering": "<stage>"`. `None` (nothing written, the link included)
    /// when the guard did not match.
    async fn complete_task(&self, req: &CompleteRequest) -> StoreResult<Option<Completed>>;

    // ---- delivery: stage moves and signals ----

    /// Moves `m.task` forward to `m.to`, in one transaction that locks the task row first. The
    /// guard: the task is not `done` / `cancelled` and its own stage (its `task_stages`, see
    /// `ParentState`) is before `m.to`, so two moves racing for one task have one winner. On
    /// match: a task without children gets `delivery_stage = m.to` (the guarded
    /// `UPDATE … WHERE <STAGE_RANK_SQL> < :rank`); a `delivering` task with children keeps
    /// its derived minimum and is recomputed by the parent rule (forward only). Either bumps
    /// `version` / `updated_at = m.now`, records `task_stages` rows for `m.to` (with
    /// `signal_id` / `detail`) and for each of `m.implied` (`implied = true`), `ON CONFLICT DO
    /// NOTHING`, and appends `stage_reached` (payload `stage`, `implied`). A `delivering` task
    /// without children whose `m.to` reaches the project's last enabled stage becomes `done`
    /// (`done_at = m.now`) with a `delivered` event (payload `stage`); a task in any other
    /// status only records the stage (stages reached before completion). The parent rule
    /// then runs up the tree. With `m.unblock`, every task the move made `done` (the task, a
    /// parent the rule closed) unblocks its waiting tasks. `None` when the guard did not match
    /// (or no such task).
    async fn advance_stage(&self, m: &StageMove) -> StoreResult<Option<StageMoved>>;
    /// The human's **Mark done**: guarded `delivering → done` (`done_at = updated_at = now`,
    /// `version` bumped), a `delivered` event with note `marked done by the human`, then the
    /// parent rule; with `unblock`, the task and any parent it closed unblock their waiting
    /// tasks. `None` when the task is not `delivering`.
    async fn mark_delivered(
        &self,
        task: TaskId,
        actor: Actor,
        token: Option<TokenId>,
        now: OffsetDateTime,
        unblock: Option<UnblockRule>,
    ) -> StoreResult<Option<Transitioned>>;
    /// Appends `s` to the delivery log. `Duplicate` (nothing written) when the project already
    /// logged a signal with the same `delivery_id`; signals without one never collide.
    async fn insert_signal(&self, s: &DeliverySignal) -> StoreResult<SignalInsert>;

    // ---- agents ----

    /// Returns the agent for `(project, host, harness)`, creating it if needed. A `None`
    /// project is a real identity component (agents with no home project dedupe too).
    /// Creating an agent with a home project also starts its project-message cursor at the
    /// newest message, in the same transaction.
    async fn upsert_agent(
        &self,
        project: Option<ProjectId>,
        host: &str,
        harness: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Agent>;
    async fn get_agent(&self, id: AgentId) -> StoreResult<Option<Agent>>;
    /// Up to `limit` agents, newest first (one row per agent, however many sessions).
    async fn list_agents(&self, limit: u32) -> StoreResult<Vec<Agent>>;

    // ---- sessions ----

    /// Inserts the session with all timestamps set to `now`. If a session with this id
    /// already exists it is returned unchanged (idempotent; callers check `agent_id`).
    async fn create_session(
        &self,
        id: SessionId,
        agent: AgentId,
        token: TokenId,
        now: OffsetDateTime,
    ) -> StoreResult<Session>;
    /// Sets `last_heartbeat_at`; `NotFound` for an unknown session.
    async fn touch_heartbeat(&self, id: SessionId, now: OffsetDateTime) -> StoreResult<()>;
    /// Sets `last_tool_call_at` and `last_heartbeat_at` (a tool call is also proof of
    /// life, which matters for clients without a heartbeating bridge).
    async fn touch_tool_call(&self, id: SessionId, now: OffsetDateTime) -> StoreResult<()>;
    async fn get_session(&self, id: SessionId) -> StoreResult<Option<Session>>;
    /// Sessions with their agents, oldest first. `Some(p)`: only agents whose home project
    /// is `p`; `None`: every session.
    async fn list_sessions(&self, project: Option<ProjectId>)
    -> StoreResult<Vec<(Agent, Session)>>;

    // ---- tasks ----

    /// Inserts a new task, its extra participating projects, its `created` event and the first
    /// message of its thread in one transaction. `token` is the writer's token. A child task is
    /// refused (nothing written, `false`) while its parent is `planning`: the parent's status
    /// is read under the write lock, so the check and the insert are atomic.
    async fn insert_task(
        &self,
        task: &Task,
        extra_projects: &[ProjectId],
        token: Option<TokenId>,
        event: &TaskEvent,
        first_message: &Message,
    ) -> StoreResult<bool>;
    /// Appends participating projects to a top-level task, in one transaction: locks the task,
    /// applies only if it has no parent, is not `done` / `cancelled` and `req.editor` is `None`
    /// (the human), its creator or the session holding it. Projects already listed are skipped;
    /// when any is new it bumps `version` / `updated_at` and appends `req.event` (payload: the
    /// added project ids) and `req.note` (body: `projects_added_note` of their display names).
    /// `None` when the guard did not match.
    async fn add_task_projects(
        &self,
        req: &AddProjectsRequest<'_>,
    ) -> StoreResult<Option<ProjectsAdded>>;
    /// Adds and removes links between `req.task` and other tasks, in one transaction (on
    /// Postgres under `TASK_LOCK`, so link counts are exact): locks the task and applies only
    /// if `req.editor` is `None` (the human), its creator or the
    /// session holding it (`planning`, `claimed`, `blocked` or `review`). Removals run first and
    /// drop the pair's link whichever way it points; an addition replaces the pair's link
    /// unless it is the same one. Afterwards no task touched may have more than
    /// `req.max_links` links, or nothing is written. When anything changed it bumps `version` /
    /// `updated_at` and appends `req.event` (payload from `related_payload`).
    async fn relate_tasks(&self, req: &RelateRequest<'_>) -> StoreResult<Related>;
    /// Moves the top-level task `req.task` under `req.parent`, in one transaction that locks
    /// both: applies only if the task still has no parent and no extra participating projects,
    /// `req.editor` is `None` (the human), its creator or the session holding it (`planning`,
    /// `claimed`, `blocked` or `review`), the parent is not `planning`, `done` or `cancelled`,
    /// the task's project is the parent's or one the parent spans, and the task is not the
    /// parent or one of its ancestors. It is also refused (`Moved::WaitOnAncestor`) when the
    /// task or one of its subtasks is blocked waiting on the new parent or one of the parent's
    /// ancestors, since a task never waits on its own ancestor. Sets `parent_id`, bumps
    /// `version` / `updated_at` and appends `req.event`.
    async fn move_task(&self, req: &MoveRequest) -> StoreResult<Moved>;
    /// Every link of `task`, whichever way it points, oldest first.
    async fn task_links(&self, task: TaskId) -> StoreResult<Vec<TaskLink>>;
    async fn get_task(&self, id: TaskId) -> StoreResult<Option<Task>>;
    /// The extra participating projects listed for `id` (not its home project).
    async fn task_projects(&self, id: TaskId) -> StoreResult<Vec<ProjectId>>;
    /// Tasks matching every set filter field, in `filter.order` (oldest first by default). `project` also matches tasks
    /// that list it as an extra participating project.
    async fn list_tasks(&self, filter: &TaskFilter) -> StoreResult<Vec<Task>>;
    /// How many tasks `list_tasks(filter)` would return ignoring `after`, `limit` and `order`.
    async fn count_tasks(&self, filter: &TaskFilter) -> StoreResult<u64>;
    /// In one transaction: for each listed id whose task is `done`, sets `acked_at = now` on it
    /// and on every `done` descendant, wherever `acked_at` is still null. Ids that are unknown
    /// or not done are skipped (their descendants too). Returns the ids newly acked.
    async fn ack_done_tasks(&self, ids: &[TaskId], now: OffsetDateTime)
    -> StoreResult<Vec<TaskId>>;
    /// A task's history, oldest first.
    async fn task_events(&self, id: TaskId) -> StoreResult<Vec<TaskEvent>>;
    /// A page of a task's history, newest first: only events of `kind` when set, and only
    /// events older than the event `before` (in `(at, id)` order) when set.
    async fn task_events_page(
        &self,
        id: TaskId,
        kind: Option<TaskEventKind>,
        before: Option<uuid::Uuid>,
        limit: u32,
    ) -> StoreResult<Vec<TaskEvent>>;

    /// The spec §8 guarded claim. In one transaction: locks and reads the task, runs the
    /// guarded `UPDATE … RETURNING`, and on success appends the event from `claim_event`.
    async fn claim_task(&self, task: TaskId, req: &ClaimRequest) -> StoreResult<ClaimAttempt>;
    /// Claims the oldest task in `project` that is `open` or `claimed` with an expired
    /// lease and that the claimant is eligible for (Postgres: `FOR UPDATE SKIP LOCKED`),
    /// through the same guarded claim and event as `claim_task`. `None` if there is none.
    async fn claim_next_task(
        &self,
        project: ProjectId,
        req: &ClaimRequest,
    ) -> StoreResult<Option<Task>>;
    /// A guarded state change: applies only if the task's status is in `from`, (when
    /// `plan_from` is set) its plan state is in `plan_from` and (when `owner` is set)
    /// `owner_session_id = owner`. Appends `event`, and `message` when set, in the same
    /// transaction.
    /// `None` when the guard did not match (callers re-read to explain why).
    async fn transition_task(&self, req: &TransitionRequest) -> StoreResult<Option<Transitioned>>;
    /// A guarded edit, in one transaction: applies only if the task is not `done` /
    /// `cancelled` and `req.agent` created it or `req.session` holds it (`claimed`, `blocked`
    /// or `review`). Sets the title and/or body (`None` keeps each), bumps `version` /
    /// `updated_at`, and appends `req.event` with its payload set to the replaced values
    /// (`previous_title` / `previous_body`, read under the lock). `None` when the guard did not
    /// match (callers re-read to explain why).
    async fn edit_task(&self, req: &EditRequest<'_>) -> StoreResult<Option<Task>>;
    /// Whether `session` ever claimed `task` (distinguishes "lost the lease" from "never
    /// owned it").
    async fn session_held_task(&self, task: TaskId, session: SessionId) -> StoreResult<bool>;
    /// Sets `lease_expires_at = now + ttl` (the task's own TTL, else `default_ttl`) on every
    /// `claimed` or `planning` task owned by `session` whose lease has not yet expired at `now`
    /// (heartbeat renewal never revives an expired lease, and a submitted plan has no lease).
    /// Returns the number of leases extended.
    async fn extend_session_leases(
        &self,
        session: SessionId,
        now: OffsetDateTime,
        default_ttl: Duration,
    ) -> StoreResult<u64>;

    // ---- messages ----

    /// Inserts the message and, for agent-, session- and human-addressed messages, its
    /// recipient row. Project and task messages get recipient rows when read.
    async fn insert_message(&self, message: &Message, token: Option<TokenId>) -> StoreResult<()>;
    /// Inserts the human's reply `message` and, in the same transaction, archives (acks) the
    /// human's copy of the message it answers (`message.reply_to`).
    async fn insert_human_reply(&self, message: &Message, now: OffsetDateTime) -> StoreResult<()>;
    async fn get_message(&self, id: MessageId) -> StoreResult<Option<Message>>;
    /// The recipient's unacked messages matching `filter`, oldest first. In one write
    /// transaction: first gives the recipient a row for every project / task message now
    /// due to it, then stamps `delivered_at` (once) on the rows returned.
    async fn inbox(
        &self,
        to: Recipient,
        filter: &InboxFilter,
        now: OffsetDateTime,
    ) -> StoreResult<Vec<InboxItem>>;
    /// Acks the recipient's rows for `ids` (once per agent for agent rows, per session for
    /// session rows). Ids that are not the recipient's, or already acked, are skipped.
    /// Returns how many rows were newly acked.
    async fn ack_messages(
        &self,
        to: Recipient,
        ids: &[MessageId],
        now: OffsetDateTime,
    ) -> StoreResult<u64>;
    /// Sets `read_at` (once) on the recipient's rows for `ids`; returns how many changed.
    async fn mark_read(
        &self,
        to: Recipient,
        ids: &[MessageId],
        now: OffsetDateTime,
    ) -> StoreResult<u64>;
    /// How many messages `inbox` with no filter (`importance`: only that importance) would
    /// return, without writing anything.
    async fn unread_count(&self, to: Recipient, importance: Option<Importance>)
    -> StoreResult<u64>;
    /// Full-text match of every word of `query` over subject + body, newest first.
    /// `query` must contain at least one non-blank word.
    async fn search_messages(
        &self,
        query: &str,
        project: Option<ProjectId>,
        limit: u32,
    ) -> StoreResult<Vec<Message>>;
    /// A task's thread, oldest first.
    async fn task_thread(&self, task: TaskId) -> StoreResult<Vec<Message>>;
    /// A page of a task's thread, newest first; with `before`, only messages sent before
    /// that one (an unknown `before` gives nothing).
    async fn task_thread_page(
        &self,
        task: TaskId,
        before: Option<MessageId>,
        limit: u32,
    ) -> StoreResult<Vec<Message>>;
    /// Every message (or only those whose `project_id` is `project`), newest first; with
    /// `before`, only messages sent before that one. An unknown `before` gives nothing.
    async fn list_messages(
        &self,
        project: Option<ProjectId>,
        before: Option<MessageId>,
        limit: u32,
    ) -> StoreResult<Vec<Message>>;
    /// Delivery receipts for `ids`, in the same order (an unknown id gets an empty receipt).
    async fn message_receipts(&self, ids: &[MessageId]) -> StoreResult<Vec<MessageReceipt>>;
    /// What waits on the human's attention, oldest first, at most `limit`: unarchived
    /// human-due messages (their own rows, and thread posts not yet materialised) that are not
    /// plans and are of high or urgent importance or carry quick-reply options (a question).
    /// Read-only: nothing is marked delivered.
    async fn human_attention(&self, limit: u32) -> StoreResult<Vec<Message>>;
    /// The tasks whose thread holds urgent mail waiting on the human (the same messages
    /// `human_attention` would list, urgent only, with no page limit), each once, by id.
    async fn urgent_attention_tasks(&self) -> StoreResult<Vec<TaskId>>;

    // ---- usage ----

    /// In one transaction, for each record: upserts by `(agent.harness, key)`. A new row takes
    /// the batch's session/agent/project/token, `pricer`'s price and one allocation row per task
    /// `session` held at `occurred_at` (`parts` = how many), or one `(NULL, 1)` row. An existing
    /// row written by the same token keeps its session, model, billing, times and allocations,
    /// takes the larger value of each count, and is re-priced from the merged counts; one
    /// written by another token is left as it is (a duplicate).
    async fn insert_usage(
        &self,
        batch: &UsageBatch<'_>,
        pricer: &dyn Pricer,
    ) -> StoreResult<UsageWritten>;
    async fn get_usage(&self, harness: &str, key: &str) -> StoreResult<Option<StoredUsage>>;
    /// One row per group (one row for `Total`, even when nothing matches), unordered.
    async fn usage_summary(
        &self,
        filter: &UsageFilter,
        group: UsageGroup,
    ) -> StoreResult<Vec<UsageRow>>;
    /// Up to `limit` rows in id order after `after`, `occurred_at >= since` when set, only
    /// NULL-cost rows when `unpriced_only`.
    async fn usage_to_reprice(
        &self,
        since: Option<OffsetDateTime>,
        unpriced_only: bool,
        after: Option<UsageId>,
        limit: u32,
    ) -> StoreResult<Vec<RepriceRow>>;
    /// Sets `cost_micros` / `price_ref` (`None`: both NULL) for each id, in one transaction.
    async fn set_usage_costs(&self, costs: &[(UsageId, Option<Priced>)]) -> StoreResult<()>;
}

/// How far a message has got, from its recipient rows (spec §9 receipts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageReceipt {
    pub id: MessageId,
    /// Recipient rows so far (project and task messages get theirs when read).
    pub recipients: u32,
    pub delivered: u32,
    pub read: u32,
    /// Some message has `reply_to` = this one.
    pub replied: bool,
}

impl MessageReceipt {
    /// The furthest stage reached: `sent`, `delivered`, `read` or `replied`.
    pub fn state(&self) -> &'static str {
        if self.replied {
            "replied"
        } else if self.read > 0 {
            "read"
        } else if self.delivered > 0 {
            "delivered"
        } else {
            "sent"
        }
    }
}

/// Token counts of one model response. `input` is uncached input only; `output` includes
/// `reasoning`, which is informational and never priced twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub input: i64,
    pub output: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub cache_read: i64,
    pub reasoning: i64,
}

/// One reported usage record, keyed by the harness's own id for the response.
#[derive(Debug, Clone)]
pub struct NewUsage {
    pub key: String,
    pub model: String,
    pub counts: UsageCounts,
    pub speed: Option<String>,
    pub billing: Billing,
    pub occurred_at: OffsetDateTime,
}

/// Records reported by one session in one request (see `Store::insert_usage`).
#[derive(Debug, Clone)]
pub struct UsageBatch<'a> {
    pub session: SessionId,
    /// The session's agent: its harness keys the records, its home project is theirs.
    pub agent: &'a Agent,
    pub token: TokenId,
    pub parser: &'a str,
    pub harness_session_id: Option<&'a str>,
    pub records: &'a [NewUsage],
    pub now: OffsetDateTime,
}

/// An API-equivalent price: micro-USD and the price-table entry it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Priced {
    pub cost_micros: i64,
    pub price_ref: String,
}

/// Prices a record's counts; the store calls it inside the write so a merged row is priced
/// from its merged counts.
pub trait Pricer: Send + Sync {
    fn price(&self, model: &str, speed: Option<&str>, counts: &UsageCounts) -> Option<Priced>;
}

/// What `insert_usage` did: new rows, and records whose key was already stored (merged).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageWritten {
    pub inserted: u32,
    pub duplicate: u32,
}

/// A stored usage record with its allocation rows.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredUsage {
    pub id: UsageId,
    pub harness: String,
    pub session: SessionId,
    pub agent: AgentId,
    pub project: Option<ProjectId>,
    pub model: String,
    pub counts: UsageCounts,
    pub speed: Option<String>,
    pub billing: Billing,
    pub cost_micros: Option<i64>,
    pub price_ref: Option<String>,
    pub parser: String,
    pub harness_session_id: Option<String>,
    pub occurred_at: OffsetDateTime,
    pub received_at: OffsetDateTime,
    /// `(task, parts)`, `None` = "no task"; ordered by task id text, `None` first.
    pub allocations: Vec<(Option<TaskId>, i64)>,
}

/// Which allocation rows a usage query counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskScope {
    Task { id: TaskId, subtasks: bool },
    NoTask,
}

#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    /// `occurred_at >= from`
    pub from: Option<OffsetDateTime>,
    /// `occurred_at < to`
    pub to: Option<OffsetDateTime>,
    pub project: Option<ProjectId>,
    pub agent: Option<AgentId>,
    pub harness: Option<String>,
    pub host: Option<String>,
    pub task: Option<TaskScope>,
    pub model: Option<String>,
    pub billing: Option<Billing>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageGroup {
    Total,
    Project,
    Task,
    Agent,
    Harness,
    Host,
    Model,
    Day,
    Hour,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageRow {
    /// Project/agent/task id text, harness, host, model, `YYYY-MM-DD` (Day) or
    /// `YYYY-MM-DDTHH` (Hour), all UTC; `None`: no project / no task; always `None` for Total.
    pub key: Option<String>,
    /// Distinct records touching the group (not split by parts).
    pub requests: i64,
    pub sessions: i64,
    /// Token and cost sums divided by each allocation's `parts`.
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub cost_micros: f64,
    /// Distinct records with a NULL cost; distinct records with billing other than `api`.
    pub unpriced: i64,
    pub estimated: i64,
    /// The cost of the non-`api` records.
    pub estimated_cost_micros: f64,
}

/// A stored record as the repricer sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct RepriceRow {
    pub id: UsageId,
    pub model: String,
    pub speed: Option<String>,
    pub counts: UsageCounts,
}

/// Whose inbox an operation works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipient {
    /// Sees its agent's rows (agent-, project- and task-addressed mail) and its session's.
    Agent {
        agent: AgentId,
        session: SessionId,
    },
    Human,
}

impl Recipient {
    /// `(agent, session)` bind values; both `None` for the human.
    pub fn ids(&self) -> (Option<AgentId>, Option<SessionId>) {
        match *self {
            Recipient::Agent { agent, session } => (Some(agent), Some(session)),
            Recipient::Human => (None, None),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InboxFilter {
    /// Only messages whose `project_id` is this.
    pub project: Option<ProjectId>,
    pub task: Option<TaskId>,
    /// Only messages of exactly this importance.
    pub importance: Option<Importance>,
    /// Only messages of at least this importance (applied before `limit`, so mail that
    /// matters is never hidden behind a page of ordinary mail).
    pub min_importance: Option<Importance>,
    /// `None`: no limit.
    pub limit: Option<u32>,
}

/// The `messages.options` column for a message's quick replies: a JSON array, or NULL when
/// there are none.
pub fn options_to(options: &[String]) -> Option<String> {
    if options.is_empty() {
        return None;
    }
    Some(serde_json::to_string(options).expect("strings serialize"))
}

/// Reads `messages.options` back (NULL is no options).
pub fn options_from(column: Option<String>) -> StoreResult<Vec<String>> {
    match column {
        None => Ok(Vec::new()),
        Some(json) => serde_json::from_str(&json)
            .map_err(|e| StoreError::Corrupt(format!("message options: {e}"))),
    }
}

/// The recipient row a message gets at send time, if any: `(agent, session)`.
pub fn send_time_recipient(to: &Address) -> Option<(Option<AgentId>, Option<SessionId>)> {
    match *to {
        Address::Agent(a) => Some((Some(a), None)),
        Address::Session(s) => Some((None, Some(s))),
        Address::Human => Some((None, None)),
        Address::Project(_) | Address::Task(_) => None,
    }
}

/// The caller of a claim, with everything the eligibility guard needs.
#[derive(Debug, Clone)]
pub struct Claimant {
    pub agent: AgentId,
    pub session: SessionId,
    pub token: TokenId,
    /// The agent's home project (`None` is never eligible for project-scoped claims).
    pub project: Option<ProjectId>,
    pub harness: String,
    pub host: String,
}

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    pub claimant: Claimant,
    pub now: OffsetDateTime,
    /// Sessions whose last heartbeat is at or after this are live. A same-agent takeover
    /// needs the previous owner session to be stale (the `--resume` case); a live sibling
    /// session of the same agent is refused like any other agent.
    pub live_since: OffsetDateTime,
    /// Lease length for tasks without their own TTL.
    pub default_ttl: Duration,
    /// Id for the event appended on success.
    pub event_id: uuid::Uuid,
}

/// The row as it was (under lock) before the claim, and the row after a successful claim.
#[derive(Debug, Clone)]
pub struct ClaimAttempt {
    /// `None`: no such task.
    pub previous: Option<Task>,
    /// Whether the claimant passed the eligibility filter (harness, host, project).
    pub eligible: bool,
    /// `Some` iff the guarded update matched.
    pub claimed: Option<Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseUpdate {
    Keep,
    Clear,
    Set(OffsetDateTime),
}

#[derive(Debug, Clone)]
pub struct TransitionRequest {
    pub task: TaskId,
    pub from: Vec<TaskStatus>,
    /// `None` keeps the current status.
    pub to: Option<TaskStatus>,
    /// Required owner session; `None` skips the ownership guard.
    pub owner: Option<SessionId>,
    pub lease: LeaseUpdate,
    pub clear_owner: bool,
    pub now: OffsetDateTime,
    pub token: Option<TokenId>,
    pub event: TaskEvent,
    /// Required plan states; `None` skips the plan guard.
    pub plan_from: Option<Vec<PlanState>>,
    /// `None` keeps the plan state.
    pub plan_to: Option<PlanState>,
    /// A message written with the transition (a submitted plan, an approval).
    pub message: Option<Message>,
    /// Required latest plan: applies only while this is the task's newest `plan` message.
    pub latest_plan: Option<MessageId>,
    /// Archives (acks for the human) the task's existing plan messages, before `message` is
    /// written: a superseded or decided plan no longer waits on anyone.
    pub archive_plans: bool,
    /// The block detail to write when the task is (or stays) `blocked`; `None` keeps it.
    /// Whenever the resulting status is not `blocked`, the detail is cleared regardless.
    pub block: Option<BlockWrite>,
    /// When the transition makes the task `done`: unblocks, in the same transaction, every
    /// `blocked` task waiting on it (see `UnblockRule`), and likewise for every `delivering`
    /// ancestor the parent rule finished. `None` leaves them alone.
    pub unblock: Option<UnblockRule>,
}

/// Block detail written by a transition. Each `None` keeps the stored value (on entering
/// `blocked` the stored values are already empty). `blocked_at` is set on entering `blocked`.
///
/// A `WaitingOn::Task(t)` is guarded in the store: the write applies only while `t` exists,
/// is not `done` / `cancelled`, is not the task itself or one of its ancestors, and does not
/// (through the tasks it waits on) wait on this task. A guard miss writes nothing.
#[derive(Debug, Clone, Default)]
pub struct BlockWrite {
    pub reason: Option<String>,
    pub waiting_on: Option<WaitingOn>,
    pub unblock: Option<String>,
    /// An update of a block: adds the replaced values (`previous_reason`,
    /// `previous_waiting_on`, `previous_unblock`), read under the write's lock, to the
    /// event's payload.
    pub record_previous: bool,
}

/// `payload` (an object, or nothing) plus the `previous_*` values of `previous`.
pub fn block_previous_payload(
    payload: Option<&serde_json::Value>,
    previous: Option<&BlockDetail>,
) -> serde_json::Value {
    let mut map = payload
        .and_then(|p| p.as_object().cloned())
        .unwrap_or_default();
    let null = serde_json::Value::Null;
    map.insert(
        "previous_reason".into(),
        previous.map_or(null.clone(), |b| b.reason.clone().into()),
    );
    map.insert(
        "previous_waiting_on".into(),
        previous
            .and_then(|b| b.waiting_on.as_ref())
            .map_or(null.clone(), WaitingOn::to_json),
    );
    map.insert(
        "previous_unblock".into(),
        previous
            .and_then(|b| b.unblock.clone())
            .map_or(null, Into::into),
    );
    map.into()
}

/// How a finished task unblocks the tasks waiting on it: a dependent whose owner session's
/// last heartbeat is at or after `live_since` goes back to `claimed` with a fresh lease
/// (`now` + its own TTL, else `default_ttl`); otherwise it goes back to `open` with no owner,
/// as a release would. Either way its block detail is cleared, an `unblocked` event is
/// appended and a system note posted in its thread (`unblocked_note`).
#[derive(Debug, Clone, Copy)]
pub struct UnblockRule {
    pub live_since: OffsetDateTime,
    pub default_ttl: Duration,
}

/// A task a finished task unblocked, with the note posted in its thread.
#[derive(Debug, Clone)]
pub struct Unblocked {
    pub task: Task,
    pub note: Message,
}

/// The system note an unblocked task gets in its thread.
pub fn unblocked_note(done: &Task, reopened: bool) -> String {
    let base = format!("unblocked: \"{}\" ({}) is done", done.title, done.id);
    if reopened {
        format!(
            "{base}. The owner's session has ended, so the task is open again for any \
             eligible agent."
        )
    } else {
        format!("{base}. The task is claimed by its owner again, with a fresh lease.")
    }
}

/// The `unblocked` event and thread note for `dependent`, which waited on `done`. The note is
/// written by the server (no sending agent), so the owner's inbox shows it even when the owner
/// also finished `done`.
pub fn unblocked_records(
    dependent: &Task,
    done: &Task,
    reopened: bool,
    actor: Actor,
    now: OffsetDateTime,
) -> (TaskEvent, Message) {
    let text = unblocked_note(done, reopened);
    let event = TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: dependent.id,
        at: now,
        actor,
        kind: TaskEventKind::Unblocked,
        note: Some(text.clone()),
        payload: Some(serde_json::json!({
            "waiting_task_id": done.id.to_string(),
            "reopened": reopened,
        })),
    };
    let note = Message {
        id: MessageId::new(),
        project_id: Some(dependent.project_id),
        from: Actor::Human,
        to: Address::Task(dependent.id),
        subject: Some("unblocked".into()),
        body: text,
        importance: Importance::Normal,
        task_id: Some(dependent.id),
        reply_to: None,
        created_at: now,
        kind: MessageKind::System,
        options: Vec::new(),
    };
    (event, note)
}

/// A request to append participating projects to a task (see `Store::add_task_projects`).
#[derive(Debug, Clone)]
pub struct AddProjectsRequest<'a> {
    pub task: TaskId,
    pub projects: &'a [ProjectId],
    /// `None`: the human; else the agent (allowed as creator) and its session (as holder).
    pub editor: Option<(AgentId, SessionId)>,
    pub token: Option<TokenId>,
    /// Appended when something was added; its payload is filled in.
    pub event: TaskEvent,
    /// Posted to the thread when something was added; its body is filled in.
    pub note: Message,
}

/// What `move_task` did. Short-lived, so the large variant is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Moved {
    Moved(Task),
    /// The guard did not match (callers re-read to explain why).
    Refused,
    /// This task (the moved one or a subtask of it) waits on a task that would become its
    /// ancestor.
    WaitOnAncestor(TaskId),
}

/// A request to move a top-level task under a parent (see `Store::move_task`).
#[derive(Debug, Clone)]
pub struct MoveRequest {
    pub task: TaskId,
    pub parent: TaskId,
    /// `None`: the human; else the agent (allowed as creator) and its session (as holder).
    pub editor: Option<(AgentId, SessionId)>,
    pub token: Option<TokenId>,
    /// Appended on success (`at` is also the new `updated_at`).
    pub event: TaskEvent,
}

/// A request to add and remove links of a task (see `Store::relate_tasks`).
#[derive(Debug, Clone)]
pub struct RelateRequest<'a> {
    pub task: TaskId,
    /// Links to add, written from `task` to each target. Targets are distinct and not `task`.
    pub add: &'a [(TaskId, LinkKind)],
    /// Tasks whose link with `task` (either way) is removed.
    pub remove: &'a [TaskId],
    /// `None`: the human; else the agent (allowed as creator) and its session (as holder).
    pub editor: Option<(AgentId, SessionId)>,
    pub token: Option<TokenId>,
    /// The most links any one task may have.
    pub max_links: u32,
    /// Set when an addition is a `duplicate`: the cancellation of `task`, applied in the same
    /// transaction after the links unless the task is already cancelled. A `done` task is
    /// refused (`Related::DuplicateOfDone`) and nothing is written.
    pub cancel: Option<TransitionRequest>,
    /// Appended when something changed (`at` is also the links' `created_at` and the task's
    /// new `updated_at`); its payload is filled in.
    pub event: TaskEvent,
}

/// What `relate_tasks` did. Short-lived, so the large variant is not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Related {
    /// The task does not exist or the editor may not change it.
    Denied,
    /// An added target does not exist.
    UnknownTarget(TaskId),
    /// This task would have more than `max_links` links.
    TooMany(TaskId),
    /// `cancel` was set and the task is `done` (read under the lock).
    DuplicateOfDone,
    Applied {
        task: Task,
        /// The links written (empty when every addition was already there).
        added: Vec<TaskLink>,
        /// The tasks whose link was removed.
        removed: Vec<TaskId>,
        /// The duplicate's cancellation, when `cancel` applied.
        cancelled: Option<Transitioned>,
    },
}

/// The event payload for changed links.
pub fn related_payload(added: &[TaskLink], removed: &[TaskId]) -> serde_json::Value {
    serde_json::json!({
        "added": added
            .iter()
            .map(|l| serde_json::json!({ "id": l.target_id.to_string(), "kind": l.kind.as_str() }))
            .collect::<Vec<_>>(),
        "removed": removed.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
    })
}

/// What `add_task_projects` did.
#[derive(Debug, Clone)]
pub struct ProjectsAdded {
    pub task: Task,
    /// The projects that were new, in request order (empty: nothing changed).
    pub added: Vec<ProjectId>,
}

/// The thread note for added projects, from their display names.
pub fn projects_added_note(names: &[String]) -> String {
    match names {
        [one] => format!("Added project {one} to this task."),
        many => format!("Added projects {} to this task.", many.join(", ")),
    }
}

/// The event payload for added projects.
pub fn projects_added_payload(added: &[ProjectId]) -> serde_json::Value {
    serde_json::json!({
        "added_projects": added.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
    })
}

/// A PR to link to a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTaskPr {
    /// `owner/name`.
    pub repo: String,
    /// `None` for a link-only PR (a host Axle cannot read).
    pub number: Option<i64>,
    pub url: String,
    pub linked_by: PrLinkedBy,
}

/// A request to link a PR (see `Store::link_pr`).
#[derive(Debug, Clone)]
pub struct LinkPrRequest {
    pub task: TaskId,
    pub pr: NewTaskPr,
    pub actor: Actor,
    pub token: Option<TokenId>,
    pub now: OffsetDateTime,
    /// `false` for the fallback match: links only when the task has no PR.
    pub replace: bool,
}

/// What `link_pr` did.
// Outcomes carry whole rows by design and are short-lived values, so boxing would only add
// noise (as for `ClaimOutcome`).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked(Task),
    Relinked {
        task: Task,
        previous: TaskPr,
    },
    /// The task already has this PR.
    Unchanged(Task),
    /// The PR belongs to this other task.
    TakenBy(TaskId),
    /// The task reached `merged` (via the PR numbered `number`); new work needs a new task.
    AlreadyMerged {
        number: Option<i64>,
    },
    /// `replace = false` and the task has another PR.
    HasLink,
    Closed(TaskStatus),
    NotFound,
}

/// `task_complete`'s write (see `Store::complete_task`).
#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub transition: TransitionRequest,
    /// A PR from the completion's links, linked when the task has none.
    pub link: Option<NewTaskPr>,
}

/// A completion that applied, and where it sent the task.
#[derive(Debug, Clone)]
pub struct Completed {
    pub transitioned: Transitioned,
    pub target: CompletionTarget,
}

/// A forward stage move (see `Store::advance_stage`).
#[derive(Debug, Clone)]
pub struct StageMove {
    pub task: TaskId,
    pub to: DeliveryStage,
    /// Stages passed over on the way (`implied_between`), recorded as `implied`.
    pub implied: Vec<DeliveryStage>,
    /// The signal that caused the move, if one did.
    pub signal_id: Option<uuid::Uuid>,
    /// `version`, `environment` and `url` when known, stored on the `m.to` row.
    pub detail: Option<serde_json::Value>,
    pub actor: Actor,
    pub token: Option<TokenId>,
    pub now: OffsetDateTime,
    /// Unblocks the waiting tasks of every task the move made `done`; `None` leaves them.
    pub unblock: Option<UnblockRule>,
}

/// What `set_project_delivery` changed.
#[derive(Debug, Clone, Default)]
pub struct StagesApplied {
    /// The tasks moved to `done`, as updated, parents included.
    pub moved: Vec<Task>,
    /// The tasks the stages finished themselves (not through the parent rule), as updated,
    /// each with the task on which finishing it recorded `children_complete` (see
    /// `Transitioned::children_complete`).
    pub finished: Vec<(Task, Option<TaskId>)>,
    /// The tasks those unblocked.
    pub unblocked: Vec<Unblocked>,
}

/// A stage move that applied.
#[derive(Debug, Clone)]
pub struct StageMoved {
    /// The task as updated.
    pub task: Task,
    /// Whether the move finished the task (`delivering → done`).
    pub delivered: bool,
    /// The task on which the move recorded `children_complete` (it closed that task's last
    /// open child): the parent, or an ancestor the parent rule finished its way up to.
    pub children_complete: Option<TaskId>,
    /// Tasks unblocked because the move made a task `done` (see `StageMove::unblock`).
    pub unblocked: Vec<Unblocked>,
}

/// What `insert_signal` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalInsert {
    Inserted,
    /// The project already logged this `delivery_id`.
    Duplicate,
}

/// The `stage_reached` event of a stage move.
pub(crate) fn stage_reached_event(m: &StageMove) -> TaskEvent {
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: m.task,
        at: m.now,
        actor: m.actor,
        kind: TaskEventKind::StageReached,
        note: None,
        payload: Some(serde_json::json!({
            "stage": m.to.as_str(),
            "implied": m.implied.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        })),
    }
}

/// The `delivered` event of a stage move that reached the last enabled stage.
pub(crate) fn stage_delivered_event(m: &StageMove) -> TaskEvent {
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: m.task,
        at: m.now,
        actor: m.actor,
        kind: TaskEventKind::Delivered,
        note: None,
        payload: Some(serde_json::json!({ "stage": m.to.as_str() })),
    }
}

/// The `delivered` event of the human's **Mark done**.
pub(crate) fn marked_done_event(task: TaskId, actor: Actor, now: OffsetDateTime) -> TaskEvent {
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task,
        at: now,
        actor,
        kind: TaskEventKind::Delivered,
        note: Some("marked done by the human".to_string()),
        payload: None,
    }
}

/// Whether a stage move to `to` finishes a task in `status` (`has_children`: the parent rule
/// decides instead): a `delivering` leaf reaching the last enabled stage.
pub(crate) fn move_delivers(
    stages: &[DeliveryStage],
    status: TaskStatus,
    has_children: bool,
    to: DeliveryStage,
) -> bool {
    status == TaskStatus::Delivering
        && !has_children
        && last_enabled(stages).is_some_and(|last| to >= last)
}

/// The `pr_linked` (or, replacing `previous`, `pr_relinked`) event of a link.
pub(crate) fn pr_linked_event(
    task: TaskId,
    url: &str,
    previous: Option<&TaskPr>,
    actor: Actor,
    at: OffsetDateTime,
) -> TaskEvent {
    let (kind, payload) = match previous {
        None => (TaskEventKind::PrLinked, serde_json::json!({ "url": url })),
        Some(p) => (
            TaskEventKind::PrRelinked,
            serde_json::json!({ "url": url, "previous_url": p.url }),
        ),
    };
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task,
        at,
        actor,
        kind,
        note: None,
        payload: Some(payload),
    }
}

/// The `pr_unlinked` event, keeping the removed URL.
pub(crate) fn pr_unlinked_event(
    task: TaskId,
    previous: &TaskPr,
    actor: Actor,
    at: OffsetDateTime,
) -> TaskEvent {
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task,
        at,
        actor,
        kind: TaskEventKind::PrUnlinked,
        note: None,
        payload: Some(serde_json::json!({ "previous_url": previous.url })),
    }
}

/// Whether `existing` is the PR `new` names (same URL, or same repo and number).
pub(crate) fn same_pr(existing: &TaskPr, new: &NewTaskPr) -> bool {
    existing.url == new.url
        || (new.number.is_some() && existing.repo == new.repo && existing.number == new.number)
}

/// The completion's `done` event; completing into `delivering` adds `"delivering"` to it.
pub(crate) fn completion_event(event: &TaskEvent, target: CompletionTarget) -> TaskEvent {
    let mut event = event.clone();
    if let CompletionTarget::Delivering(stage) = target {
        let mut payload = match event.payload.take() {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        payload.insert("delivering".into(), stage.as_str().into());
        event.payload = Some(payload.into());
    }
    event
}

/// Whether a `done` event is a completion into `delivering` (its payload names the stage).
pub(crate) fn completes_into_delivering(event: &TaskEvent) -> bool {
    event.kind == TaskEventKind::Done
        && event
            .payload
            .as_ref()
            .is_some_and(|p| p.get("delivering").is_some())
}

/// What the parent rule reads about a `delivering` parent (under its row lock).
///
/// **Own stage vs shown stage (binding rule).** A task's *own* furthest stage is always the
/// greatest of its `task_stages` rows (for a task without children it equals
/// `tasks.delivery_stage`). `tasks.delivery_stage` of a `delivering` parent holds the derived
/// minimum the board shows, so it is never read as the parent's own stage. Every writer
/// keeps this: a stage reached by the task's own PR (link, `advance_stage`) is a
/// `task_stages` row, and on a `delivering` parent the move recomputes the parent
/// (`recompute_parent`, forward only) instead of writing the PR's stage into
/// `delivery_stage`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParentState {
    /// `tasks.delivery_stage`: the stage the parent shows (the minimum).
    pub stage: Option<DeliveryStage>,
    pub has_pr: bool,
    /// The furthest stage the parent's own PR reached (max of its `task_stages` rows).
    pub own: Option<DeliveryStage>,
}

/// Why `link_pr` refuses before looking for another holder of the PR, if it does: the
/// task's own stage is `own` (its `task_stages`), never a parent's derived `delivery_stage`.
pub(crate) fn link_refusal(
    task: &Task,
    existing: Option<&TaskPr>,
    own: Option<DeliveryStage>,
    req: &LinkPrRequest,
) -> Option<LinkOutcome> {
    if matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled) {
        return Some(LinkOutcome::Closed(task.status));
    }
    if let Some(e) = existing {
        if same_pr(e, &req.pr) {
            return Some(LinkOutcome::Unchanged(task.clone()));
        }
        if !req.replace {
            return Some(LinkOutcome::HasLink);
        }
    }
    (own >= Some(DeliveryStage::Merged)).then(|| LinkOutcome::AlreadyMerged {
        number: existing.and_then(|e| e.number),
    })
}

/// Where the parent rule moves a `delivering` parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParentMove {
    /// Forward to this stage (the minimum over its own PR and its delivering children).
    Stage(DeliveryStage),
    /// Every non-cancelled child is `done` and its own PR (if any) reached the last stage.
    Done,
}

/// The parent rule, shared by both stores and run after every change of a child whose
/// parent is `delivering`. Moves are forward only; `None` when nothing changes.
pub(crate) fn recompute_parent(
    stages: &[DeliveryStage],
    parent: &ParentState,
    children: &[ChildState],
) -> Option<ParentMove> {
    let own = parent.has_pr.then(|| {
        parent
            .own
            .map_or(DeliveryStage::InPr, |s| s.max(DeliveryStage::InPr))
    });
    let children_done = children
        .iter()
        .all(|c| matches!(c.status, TaskStatus::Done | TaskStatus::Cancelled));
    let own_done = match (own, last_enabled(stages)) {
        (Some(own), Some(last)) => own >= last,
        _ => true,
    };
    if children_done && own_done {
        return Some(ParentMove::Done);
    }
    let target = parent_stage(own, children)?;
    (Some(target) > parent.stage).then_some(ParentMove::Stage(target))
}

/// The event the parent rule records on a parent it moved.
pub(crate) fn parent_move_event(
    parent: TaskId,
    change: ParentMove,
    source: &TaskEvent,
) -> TaskEvent {
    let (kind, note, payload) = match change {
        ParentMove::Stage(stage) => (
            TaskEventKind::StageReached,
            None,
            Some(serde_json::json!({ "stage": stage.as_str() })),
        ),
        ParentMove::Done => (
            TaskEventKind::Delivered,
            Some("every child task is done".to_string()),
            None,
        ),
    };
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: parent,
        at: source.at,
        actor: source.actor,
        kind,
        note,
        payload,
    }
}

/// What one step of the parent rule did to a child's parent (both stores' `parent_step`).
#[derive(Debug, Clone)]
pub(crate) enum ParentStep {
    Unchanged,
    /// The last open child closed: `children_complete` was recorded on the parent, and a
    /// `claimed` parent moved to `review` (the row as updated, if it moved).
    ChildrenComplete(Option<Task>),
    /// The rule moved a `delivering` parent (as updated) and this event records it; it has
    /// not been appended yet, so the caller appends it and continues with the grandparent.
    Moved(Task, TaskEvent),
}

/// Adds `task` to the tasks a write moved, replacing an earlier entry for the same task.
pub(crate) fn push_moved(moved: &mut Vec<Task>, task: Task) {
    moved.retain(|t| t.id != task.id);
    moved.push(task);
}

/// A transition that applied.
#[derive(Debug, Clone)]
pub struct Transitioned {
    pub task: Task,
    /// The task on which this transition recorded `children_complete` (it closed that
    /// task's last open child, and a `claimed` one moved to `review`): the parent, or an
    /// ancestor the parent rule finished its way up to.
    pub children_complete: Option<TaskId>,
    /// Tasks this transition unblocked (it made the task `done`; see `UnblockRule`).
    pub unblocked: Vec<Unblocked>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub project: Option<ProjectId>,
    pub status: Option<TaskStatus>,
    pub owner: Option<AgentId>,
    pub parent: Option<TaskId>,
    /// Page cursor: only tasks after this one in `(created_at, id)` order.
    pub after: Option<TaskId>,
    /// `None`: no limit.
    pub limit: Option<u32>,
    /// Only tasks with no parent.
    pub top_level: bool,
    /// How `done` rows are filtered; rows of any other status always pass.
    pub done: DoneFilter,
    /// Substring of the title; `%`, `_` and `\` match literally. The match ignores case for
    /// ASCII letters on both backends (`LIKE` on SQLite); Postgres (`ILIKE`) folds other
    /// letters too.
    pub title: Option<String>,
    /// Row order; also what `after` is a cursor into.
    pub order: TaskOrder,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DoneFilter {
    #[default]
    Any,
    /// `acked_at IS NULL AND done_at >= since` (since = now - N).
    Unseen { since: OffsetDateTime },
    /// `acked_at IS NULL AND done_at < since`.
    AutoAckedUnseen { since: OffsetDateTime },
}

impl DoneFilter {
    /// The `(mode, since)` the stores bind: mode is `any`, `unseen` or `auto`.
    pub(crate) fn parts(self) -> (&'static str, Option<OffsetDateTime>) {
        match self {
            DoneFilter::Any => ("any", None),
            DoneFilter::Unseen { since } => ("unseen", Some(since)),
            DoneFilter::AutoAckedUnseen { since } => ("auto", Some(since)),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskOrder {
    /// `(created_at, id)` ascending; `after` = rows after the cursor.
    #[default]
    Created,
    /// `(done_at, id)` descending; `after` = rows older than the cursor. Only done rows are
    /// returned, whatever `status` says (the stores add a literal `status = 'done'`, which
    /// also lets them use the `tasks_done` index).
    DoneNewest,
}

/// `q` as a LIKE pattern with `\` as the escape: `%q%` with `% _ \` escaped.
pub fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[derive(Debug, Clone)]
pub struct EditRequest<'a> {
    pub task: TaskId,
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    /// The editor: allowed as the task's creator, or as the session holding it.
    pub agent: AgentId,
    pub session: SessionId,
    pub token: Option<TokenId>,
    /// Appended on success (`at` is also the new `updated_at`); its payload is filled in.
    pub event: TaskEvent,
}

/// `previous_title` / `previous_body` for the fields an edit replaces.
pub fn edit_payload(req: &EditRequest<'_>, title: String, body: String) -> serde_json::Value {
    let mut previous = serde_json::Map::new();
    if req.title.is_some() {
        previous.insert("previous_title".into(), title.into());
    }
    if req.body.is_some() {
        previous.insert("previous_body".into(), body.into());
    }
    previous.into()
}

impl ClaimRequest {
    /// When a claim of `task` (as read under the lock) expires.
    pub fn lease_until(&self, task: &Task) -> OffsetDateTime {
        self.now + task.lease_ttl.unwrap_or(self.default_ttl)
    }
}

/// The event a successful claim appends, given the row as it was before the claim.
pub fn claim_event(previous: &Task, req: &ClaimRequest) -> TaskEvent {
    let c = &req.claimant;
    let previous_owner = || {
        serde_json::json!({
            "previous_agent_id": previous.owner_agent_id.map(|a| a.to_string()),
            "previous_session_id": previous.owner_session_id.map(|s| s.to_string()),
        })
    };
    let (kind, note, payload) = match (previous.status, previous.owner_agent_id) {
        (TaskStatus::Open, _) | (_, None) => (TaskEventKind::Claimed, None, None),
        (status, Some(agent)) if agent == c.agent => {
            if previous.owner_session_id == Some(c.session) {
                if matches!(status, TaskStatus::Claimed | TaskStatus::Planning) {
                    (TaskEventKind::Renewed, None, None)
                } else {
                    (
                        TaskEventKind::Claimed,
                        Some(format!("resumed from {status}")),
                        None,
                    )
                }
            } else {
                (
                    TaskEventKind::Claimed,
                    Some(format!(
                        "same-agent takeover from session {}",
                        previous
                            .owner_session_id
                            .map_or_else(|| "-".to_string(), |s| s.to_string())
                    )),
                    Some(previous_owner()),
                )
            }
        }
        (_, Some(agent)) => (
            TaskEventKind::LeaseExpiredTakeover,
            Some(format!("lease expired, previous owner {agent}")),
            Some(previous_owner()),
        ),
    };
    TaskEvent {
        id: req.event_id,
        task_id: previous.id,
        at: req.now,
        actor: Actor::Agent(c.agent, c.session),
        kind,
        note,
        payload,
    }
}

/// The parent-thread event a child's own event bubbles up as (spec §8), if it does.
pub fn child_event(child: &Task, parent: TaskId, source: &TaskEvent) -> Option<TaskEvent> {
    let kind = match source.kind {
        TaskEventKind::Claimed | TaskEventKind::LeaseExpiredTakeover => TaskEventKind::ChildClaimed,
        TaskEventKind::Blocked => TaskEventKind::ChildBlocked,
        // A completion into `delivering` is not done yet: its later `delivered` bubbles once.
        TaskEventKind::Done if completes_into_delivering(source) => return None,
        TaskEventKind::Done | TaskEventKind::Delivered => TaskEventKind::ChildDone,
        TaskEventKind::Cancelled => TaskEventKind::ChildCancelled,
        _ => return None,
    };
    Some(TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: parent,
        at: source.at,
        actor: source.actor,
        kind,
        note: source.note.clone(),
        payload: Some(serde_json::json!({
            "child_id": child.id.to_string(),
            "child_title": child.title,
        })),
    })
}

/// Whether a child event can be the one that finishes the last open child (a `done` event
/// may also have sent the child to `delivering`, which still counts as open).
pub fn closes_child(source: &TaskEvent) -> bool {
    matches!(
        source.kind,
        TaskEventKind::Done | TaskEventKind::Cancelled | TaskEventKind::Delivered
    )
}

/// The event recorded on a parent whose children are all `done` / `cancelled`.
pub fn children_complete_event(parent: TaskId, source: &TaskEvent) -> TaskEvent {
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: parent,
        at: source.at,
        actor: source.actor,
        kind: TaskEventKind::ChildrenComplete,
        note: Some("all child tasks are done or cancelled".to_string()),
        payload: None,
    }
}

/// The `project_delivery.stages` column: a JSON array of stage strings.
pub fn stages_to_json(stages: &[DeliveryStage]) -> String {
    serde_json::Value::from(stages.iter().map(|s| s.as_str()).collect::<Vec<_>>()).to_string()
}

/// Reads `project_delivery.stages` back.
pub fn stages_from_json(text: &str) -> StoreResult<Vec<DeliveryStage>> {
    let names: Vec<String> = serde_json::from_str(text)
        .map_err(|e| StoreError::Corrupt(format!("delivery stages: {e}")))?;
    names
        .iter()
        .map(|s| s.parse().map_err(StoreError::Corrupt))
        .collect()
}

/// The history note of a task that a stage change finished.
pub fn delivered_note(stages: &[DeliveryStage]) -> String {
    match crate::delivery::stages::last_enabled(stages) {
        None => "delivery stages turned off".to_string(),
        Some(last) => format!("{} is now the last delivery stage", last.label()),
    }
}

/// One delivering task as the settings recompute reads it (under the write lock).
#[derive(Debug, Clone)]
pub(crate) struct DeliveringRow {
    pub task: TaskId,
    /// The task's own furthest stage (its `task_stages`; see `ParentState`).
    pub stage: Option<DeliveryStage>,
    pub has_pr: bool,
    pub children: Vec<ChildState>,
}

/// Whether new stages finish a delivering task. A task with a child that is not `done` or
/// `cancelled` (one reopened by a force release, say) stays delivering, as under the parent
/// rule (`recompute_parent`).
pub(crate) fn finished_by(stages: &[DeliveryStage], row: &DeliveringRow) -> bool {
    row.children
        .iter()
        .all(|c| matches!(c.status, TaskStatus::Done | TaskStatus::Cancelled))
        && completion_target(stages, row.stage, row.has_pr, &row.children) == CompletionTarget::Done
}

/// The `delivered` event a stage change records on a task it finished (the human acts).
pub(crate) fn stages_delivered_event(task: TaskId, d: &ProjectDelivery) -> TaskEvent {
    TaskEvent {
        id: uuid::Uuid::now_v7(),
        task_id: task,
        at: d.updated_at,
        actor: Actor::Human,
        kind: TaskEventKind::Delivered,
        note: Some(delivered_note(&d.stages)),
        payload: None,
    }
}

/// Connects to the configured backend and runs its migrations.
pub async fn open_store(backend: &Backend) -> StoreResult<Arc<dyn Store>> {
    Ok(match backend {
        Backend::Sqlite(url) => Arc::new(sqlite::SqliteStore::connect(url).await?),
        Backend::Postgres(url) => Arc::new(postgres::PostgresStore::connect(url).await?),
    })
}
