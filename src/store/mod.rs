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
    Actor, Address, Agent, AgentId, Importance, InboxItem, Message, MessageId, Project, ProjectId,
    Session, SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus, TokenId, TokenInfo,
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
    /// message of its thread in one transaction. `token` is the writer's token.
    async fn insert_task(
        &self,
        task: &Task,
        extra_projects: &[ProjectId],
        token: Option<TokenId>,
        event: &TaskEvent,
        first_message: &Message,
    ) -> StoreResult<()>;
    async fn get_task(&self, id: TaskId) -> StoreResult<Option<Task>>;
    /// The extra participating projects listed for `id` (not its home project).
    async fn task_projects(&self, id: TaskId) -> StoreResult<Vec<ProjectId>>;
    /// Tasks matching every set filter field, oldest first. `project` also matches tasks
    /// that list it as an extra participating project.
    async fn list_tasks(&self, filter: &TaskFilter) -> StoreResult<Vec<Task>>;
    /// A task's history, oldest first.
    async fn task_events(&self, id: TaskId) -> StoreResult<Vec<TaskEvent>>;

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
    /// A guarded state change: applies only if the task's status is in `from` and (when
    /// `owner` is set) `owner_session_id = owner`. Appends `event` in the same transaction.
    /// `None` when the guard did not match (callers re-read to explain why).
    async fn transition_task(&self, req: &TransitionRequest) -> StoreResult<Option<Task>>;
    /// Whether `session` ever claimed `task` (distinguishes "lost the lease" from "never
    /// owned it").
    async fn session_held_task(&self, task: TaskId, session: SessionId) -> StoreResult<bool>;
    /// Sets `lease_expires_at = now + ttl` (the task's own TTL, else `default_ttl`) on every
    /// `claimed` task owned by `session` whose lease has not yet expired at `now` (heartbeat
    /// renewal never revives an expired lease). Returns the number of leases extended.
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
    /// How many messages `inbox` with no filter would return, without writing anything.
    async fn unread_count(&self, to: Recipient) -> StoreResult<u64>;
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
    /// `None`: no limit.
    pub limit: Option<u32>,
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
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub project: Option<ProjectId>,
    pub status: Option<TaskStatus>,
    pub owner: Option<AgentId>,
    pub parent: Option<TaskId>,
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
                if status == TaskStatus::Claimed {
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
