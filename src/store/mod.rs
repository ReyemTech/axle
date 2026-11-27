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
use crate::domain::{Agent, AgentId, Project, ProjectId, Session, SessionId, TokenId};

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
}

/// Connects to the configured backend and runs its migrations.
pub async fn open_store(backend: &Backend) -> StoreResult<Arc<dyn Store>> {
    Ok(match backend {
        Backend::Sqlite(url) => Arc::new(sqlite::SqliteStore::connect(url).await?),
        Backend::Postgres(url) => Arc::new(postgres::PostgresStore::connect(url).await?),
    })
}
