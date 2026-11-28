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
    Actor, Address, Agent, AgentId, Enrollment, EnrollmentId, EnrollmentKind, Importance,
    InboxItem, Message, MessageId, PlanState, Project, ProjectId, Session, SessionId, Task,
    TaskEvent, TaskEventKind, TaskId, TaskStatus, TokenId, TokenInfo,
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
    /// human-due messages (their own rows, and thread posts not yet materialised) of high or
    /// urgent importance that are not plans. Read-only: nothing is marked delivered.
    async fn human_attention(&self, limit: u32) -> StoreResult<Vec<Message>>;
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

/// Whether a child event can be the one that finishes the last open child.
pub fn closes_child(source: &TaskEvent) -> bool {
    matches!(source.kind, TaskEventKind::Done | TaskEventKind::Cancelled)
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
