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
use crate::domain::{
    Actor, Address, Agent, AgentId, BlockDetail, Enrollment, EnrollmentId, EnrollmentKind,
    Importance, InboxItem, LinkKind, Message, MessageId, MessageKind, PlanState, Project,
    ProjectId, ProjectWakeSettings, ServerSettings, Session, SessionId, Task, TaskEvent,
    TaskEventKind, TaskId, TaskLink, TaskStatus, TokenId, TokenInfo, TurnState, WaitingOn,
    WakeAttempt, WakeChannel, WakeKind, WakeOutcome, WakeTrigger,
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

/// A stored count or limit as `u32`.
fn to_u32(n: i64) -> StoreResult<u32> {
    u32::try_from(n).map_err(|_| StoreError::Corrupt(format!("{n} out of range")))
}

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

    // ---- wake-up: requests, settings and the wake log ----

    /// The project's wake settings; the defaults when it has no row.
    async fn wake_settings(&self, project: ProjectId) -> StoreResult<ProjectWakeSettings>;
    /// Inserts or replaces the project's wake settings row.
    async fn put_wake_settings(
        &self,
        s: &ProjectWakeSettings,
        now: OffsetDateTime,
    ) -> StoreResult<()>;
    /// The server-wide switches; both off when never set.
    async fn server_settings(&self) -> StoreResult<ServerSettings>;
    async fn put_server_settings(&self, s: &ServerSettings, now: OffsetDateTime)
    -> StoreResult<()>;
    /// Appends one row to the wake log.
    async fn insert_wake_attempt(&self, a: &WakeAttempt) -> StoreResult<()>;
    /// The newest `sent` wake (kind `wake`) to `session`, if any: what a later outcome row
    /// for the same wake (claimed, expired, delivered, failed) takes its trigger from.
    async fn last_wake_sent(&self, session: SessionId) -> StoreResult<Option<WakeAttempt>>;
    /// How many wake-log rows match `q`.
    async fn count_wake_attempts(&self, q: &AttemptQuery) -> StoreResult<u32>;
    /// The newest task in `project` with this dedupe key that `from` created at or after
    /// `since`, whatever its status.
    async fn find_request(
        &self,
        from: AgentId,
        project: ProjectId,
        dedupe_key: &str,
        since: OffsetDateTime,
    ) -> StoreResult<Option<Task>>;
    /// Tasks with a dedupe key that `from` created at or after `since`.
    async fn count_requests_by(&self, from: AgentId, since: OffsetDateTime) -> StoreResult<u32>;
    /// When the oldest of the tasks `count_requests_by(from, since)` counts was created.
    async fn oldest_request_by(
        &self,
        from: AgentId,
        since: OffsetDateTime,
    ) -> StoreResult<Option<OffsetDateTime>>;
    /// Tasks in `project` still waiting for approval (status `requested`).
    async fn count_pending_requests(&self, project: ProjectId) -> StoreResult<u32>;
    // ---- wake-up: turn state, reservations and the queue ----

    /// Records what `session`'s harness reports: `state` and `channel` are each kept when
    /// `None`, and `turn_state_at` moves to `now` only when the state changes. Reporting
    /// `Busy` clears a reservation that has no task (a wake to read the inbox was delivered)
    /// and returns the session; reservations for a task are cleared by the claim. `NotFound`
    /// for an unknown session.
    async fn report_turn(
        &self,
        session: SessionId,
        state: Option<TurnState>,
        channel: Option<WakeChannel>,
        now: OffsetDateTime,
    ) -> StoreResult<Option<SessionId>>;
    /// Reserves the session for one wake with a single guarded write; `false` when the
    /// session is not free (see `Reservation`) or another reserver won the race.
    async fn reserve_session(&self, r: &Reservation) -> StoreResult<bool>;
    /// Clears every reservation pointing at `task` and returns those sessions.
    async fn clear_task_reservations(&self, task: TaskId) -> StoreResult<Vec<SessionId>>;
    /// Clears every reservation with `wake_until <= now` and returns each session with the
    /// task it was reserved for.
    async fn expire_reservations(
        &self,
        now: OffsetDateTime,
    ) -> StoreResult<Vec<(SessionId, Option<TaskId>)>>;
    /// Sessions that are live (heartbeat at or after `live_since`), idle since at or before
    /// `idle_before`, wakeable and not reserved at `now`, with their agents: never-woken
    /// first, then least recently woken, then idle the longest. Leases, eligibility and the
    /// requester are the router's to filter.
    async fn wake_candidates(
        &self,
        live_since: OffsetDateTime,
        idle_before: OffsetDateTime,
        now: OffsetDateTime,
    ) -> StoreResult<Vec<(Agent, Session)>>;
    /// Sanctioned tasks waiting for a session: `open`, or `claimed` / `planning` with a lease
    /// expired at `now`, and no unexpired reservation pointing at them. FIFO by the latest
    /// `queued` event (else the lease expiry, else creation), then id.
    async fn list_queued(&self, now: OffsetDateTime) -> StoreResult<Vec<Task>>;

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
    /// Appends `event` to its task's history and nothing else: the task row (status,
    /// `version`, `updated_at`) is untouched and a child's event is not bubbled to its
    /// parent. For Axle's own notes (`queued`, `routed`, `wake_sent`, `wake_failed`).
    async fn append_task_event(&self, event: &TaskEvent) -> StoreResult<()>;
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

/// Whether `claimant` may claim `task`, whose extra participating projects are
/// `extra_projects`: the task's harness and host filters (when set) match exactly, and the
/// claimant's home project is the task's home project or one of the extra ones. A claimant
/// with no home project is never eligible. The claim (both stores) and the wake router use
/// this one rule.
pub fn eligible(task: &Task, extra_projects: &[ProjectId], claimant: &Claimant) -> bool {
    let matches =
        |filter: &Option<String>, value: &str| filter.as_deref().is_none_or(|f| f == value);
    matches(&task.eligibility.harness, &claimant.harness)
        && matches(&task.eligibility.host, &claimant.host)
        && claimant
            .project
            .is_some_and(|p| p == task.project_id || extra_projects.contains(&p))
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
    /// `blocked` task waiting on it (see `UnblockRule`). `None` leaves them alone.
    pub unblock: Option<UnblockRule>,
    /// Required version: applies only while the task's `version` equals this.
    pub version: Option<i64>,
    /// `None` keeps `sanctioned`.
    pub sanctioned: Option<bool>,
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

/// One guarded reservation of a session for a wake (`Store::reserve_session`). It applies
/// only while the session is wakeable, `idle` since at or before `idle_before`, heartbeating
/// at or after `live_since` and unreserved at `now`; with `new_job` the session must also own
/// no task in `planning`, `claimed`, `blocked` or `review`. It sets `wake_task_id = task`,
/// `wake_until = until` and `last_woken_at = now`.
#[derive(Debug, Clone)]
pub struct Reservation {
    pub session: SessionId,
    /// `None`: a wake with no task (to read the inbox).
    pub task: Option<TaskId>,
    pub now: OffsetDateTime,
    pub until: OffsetDateTime,
    pub live_since: OffsetDateTime,
    pub idle_before: OffsetDateTime,
    /// A wake for work the session does not already hold.
    pub new_job: bool,
}

/// Which wake-log rows `Store::count_wake_attempts` counts: `kind` and `outcome` always, and
/// every set field, from `since` (inclusive) on.
#[derive(Debug, Clone)]
pub struct AttemptQuery {
    pub kind: WakeKind,
    pub outcome: WakeOutcome,
    pub session: Option<SessionId>,
    pub task: Option<TaskId>,
    pub from_agent: Option<AgentId>,
    pub triggers: Option<Vec<WakeTrigger>>,
    pub since: OffsetDateTime,
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

/// A transition that applied.
#[derive(Debug, Clone)]
pub struct Transitioned {
    pub task: Task,
    /// Whether this transition closed its parent's last open child, so the store recorded
    /// `children_complete` on the parent (and moved a `claimed` parent to `review`).
    pub children_complete: bool,
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
        TaskEventKind::Done => TaskEventKind::ChildDone,
        TaskEventKind::Cancelled | TaskEventKind::RequestDeclined => TaskEventKind::ChildCancelled,
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

/// Whether a child event can be the one that finishes the last open child.
pub fn closes_child(source: &TaskEvent) -> bool {
    matches!(
        source.kind,
        TaskEventKind::Done | TaskEventKind::Cancelled | TaskEventKind::RequestDeclined
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

/// Connects to the configured backend and runs its migrations.
pub async fn open_store(backend: &Backend) -> StoreResult<Arc<dyn Store>> {
    Ok(match backend {
        Backend::Sqlite(url) => Arc::new(sqlite::SqliteStore::connect(url).await?),
        Backend::Postgres(url) => Arc::new(postgres::PostgresStore::connect(url).await?),
    })
}
