//! Persistence: the `Store` trait and its SQLite / Postgres implementations.
//!
//! Every timestamp written is supplied by the caller (computed from a `Clock`); queries
//! never call `now()` / `CURRENT_TIMESTAMP`.

pub mod postgres;
pub mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::config::Backend;
use crate::domain::{
    Actor, Agent, AgentId, Project, ProjectId, Session, SessionId, Task, TaskEvent, TaskEventKind,
    TaskId, TaskStatus, TokenId,
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
    /// All projects ordered by key.
    async fn list_projects(&self) -> StoreResult<Vec<Project>>;
    /// Changes only the display name; `NotFound` if the project does not exist.
    async fn rename_project(&self, id: ProjectId, display_name: &str) -> StoreResult<()>;

    // ---- agents ----

    /// Returns the agent for `(project, host, harness)`, creating it if needed. A `None`
    /// project is a real identity component (agents with no home project dedupe too).
    async fn upsert_agent(
        &self,
        project: Option<ProjectId>,
        host: &str,
        harness: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Agent>;

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

    /// Inserts a new task, its extra participating projects and its `created` event in one
    /// transaction. The event's token is `task`'s writer token (`token`).
    async fn insert_task(
        &self,
        task: &Task,
        extra_projects: &[ProjectId],
        token: Option<TokenId>,
        event: &TaskEvent,
    ) -> StoreResult<()>;
    async fn get_task(&self, id: TaskId) -> StoreResult<Option<Task>>;
    /// Tasks matching every set filter field, oldest first. `project` also matches tasks
    /// that list it as an extra participating project.
    async fn list_tasks(&self, filter: &TaskFilter) -> StoreResult<Vec<Task>>;
    /// A task's history, oldest first.
    async fn task_events(&self, id: TaskId) -> StoreResult<Vec<TaskEvent>>;

    /// The spec §8 guarded claim. In one transaction: locks and reads the task, runs the
    /// guarded `UPDATE … RETURNING`, and on success appends the event from `claim_event`.
    async fn claim_task(&self, task: TaskId, req: &ClaimRequest) -> StoreResult<ClaimAttempt>;
    /// Claims the oldest open task in `project` the claimant is eligible for (Postgres:
    /// `FOR UPDATE SKIP LOCKED`), appending a `claimed` event. `None` if there is none.
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
    /// Sets `lease_expires_at = until` on every `claimed` task owned by `session` whose lease
    /// has not yet expired at `now` (heartbeat renewal never revives an expired lease).
    /// Returns the number of leases extended.
    async fn extend_session_leases(
        &self,
        session: SessionId,
        now: OffsetDateTime,
        until: OffsetDateTime,
    ) -> StoreResult<u64>;
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
    pub lease_until: OffsetDateTime,
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

/// Connects to the configured backend and runs its migrations.
pub async fn open_store(backend: &Backend) -> StoreResult<Arc<dyn Store>> {
    Ok(match backend {
        Backend::Sqlite(url) => Arc::new(sqlite::SqliteStore::connect(url).await?),
        Backend::Postgres(url) => Arc::new(postgres::PostgresStore::connect(url).await?),
    })
}
