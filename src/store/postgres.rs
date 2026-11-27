//! `Store` on Postgres (native `uuid` and `timestamptz`).

use async_trait::async_trait;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{Store, StoreError, StoreResult};
use crate::domain::{Agent, AgentId, Project, ProjectId, Session, SessionId, TokenId};

pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    /// Connects to `url` and runs migrations.
    pub async fn connect(url: &str) -> StoreResult<Self> {
        Self::from_pool(PgPoolOptions::new().connect(url).await?).await
    }

    /// Wraps an existing pool (e.g. one with a test `search_path`) and runs migrations.
    pub async fn from_pool(pool: PgPool) -> StoreResult<Self> {
        sqlx::migrate!("migrations/postgres").run(&pool).await?;
        Ok(Self { pool })
    }
}

fn project_from(row: &PgRow) -> StoreResult<Project> {
    Ok(Project {
        id: row.try_get::<Uuid, _>("id")?.into(),
        key: row.try_get("key")?,
        display_name: row.try_get("display_name")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Reads agent columns, optionally prefixed (`a_id`, ...) for joined rows.
fn agent_from(row: &PgRow, p: &str) -> StoreResult<Agent> {
    Ok(Agent {
        id: row.try_get::<Uuid, _>(format!("{p}id").as_str())?.into(),
        project_id: row
            .try_get::<Option<Uuid>, _>(format!("{p}project_id").as_str())?
            .map(Into::into),
        host: row.try_get(format!("{p}host").as_str())?,
        harness: row.try_get(format!("{p}harness").as_str())?,
        created_at: row.try_get(format!("{p}created_at").as_str())?,
    })
}

fn session_from(row: &PgRow) -> StoreResult<Session> {
    Ok(Session {
        id: row.try_get::<Uuid, _>("id")?.into(),
        agent_id: row.try_get::<Uuid, _>("agent_id")?.into(),
        token_id: row.try_get::<Uuid, _>("token_id")?.into(),
        started_at: row.try_get("started_at")?,
        last_heartbeat_at: row.try_get("last_heartbeat_at")?,
        last_tool_call_at: row.try_get("last_tool_call_at")?,
    })
}

/// Session columns (alias `s`) as a literal, so queries stay static SQL.
macro_rules! session_cols {
    () => {
        "s.id, s.agent_id, s.token_id, s.started_at, s.last_heartbeat_at, s.last_tool_call_at"
    };
}

#[async_trait]
impl Store for PostgresStore {
    async fn upsert_project(
        &self,
        key: &str,
        display_name: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Project> {
        sqlx::query(
            "INSERT INTO projects (id, key, display_name, created_at) VALUES ($1, $2, $3, $4)
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(ProjectId::new().as_uuid())
        .bind(key)
        .bind(display_name)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_project_by_key(key)
            .await?
            .ok_or(StoreError::NotFound("project"))
    }

    async fn get_project_by_key(&self, key: &str) -> StoreResult<Option<Project>> {
        sqlx::query("SELECT id, key, display_name, created_at FROM projects WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(project_from)
            .transpose()
    }

    async fn list_projects(&self) -> StoreResult<Vec<Project>> {
        sqlx::query("SELECT id, key, display_name, created_at FROM projects ORDER BY key")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(project_from)
            .collect()
    }

    async fn rename_project(&self, id: ProjectId, display_name: &str) -> StoreResult<()> {
        let done = sqlx::query("UPDATE projects SET display_name = $1 WHERE id = $2")
            .bind(display_name)
            .bind(id.as_uuid())
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("project"));
        }
        Ok(())
    }

    async fn upsert_agent(
        &self,
        project: Option<ProjectId>,
        host: &str,
        harness: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Agent> {
        let project = project.map(|p| p.as_uuid());
        sqlx::query(
            "INSERT INTO agents (id, project_id, host, harness, created_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT ON CONSTRAINT agents_identity DO NOTHING",
        )
        .bind(AgentId::new().as_uuid())
        .bind(project)
        .bind(host)
        .bind(harness)
        .bind(now)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(
            "SELECT id, project_id, host, harness, created_at FROM agents
             WHERE project_id IS NOT DISTINCT FROM $1 AND host = $2 AND harness = $3",
        )
        .bind(project)
        .bind(host)
        .bind(harness)
        .fetch_one(&self.pool)
        .await?;
        agent_from(&row, "")
    }

    async fn create_session(
        &self,
        id: SessionId,
        agent: AgentId,
        token: TokenId,
        now: OffsetDateTime,
    ) -> StoreResult<Session> {
        sqlx::query(
            "INSERT INTO sessions
                 (id, agent_id, token_id, started_at, last_heartbeat_at, last_tool_call_at)
             VALUES ($1, $2, $3, $4, $4, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id.as_uuid())
        .bind(agent.as_uuid())
        .bind(token.as_uuid())
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_session(id)
            .await?
            .ok_or(StoreError::NotFound("session"))
    }

    async fn touch_heartbeat(&self, id: SessionId, now: OffsetDateTime) -> StoreResult<()> {
        let done = sqlx::query("UPDATE sessions SET last_heartbeat_at = $1 WHERE id = $2")
            .bind(now)
            .bind(id.as_uuid())
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("session"));
        }
        Ok(())
    }

    async fn touch_tool_call(&self, id: SessionId, now: OffsetDateTime) -> StoreResult<()> {
        let done = sqlx::query(
            "UPDATE sessions SET last_tool_call_at = $1, last_heartbeat_at = $1 WHERE id = $2",
        )
        .bind(now)
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("session"));
        }
        Ok(())
    }

    async fn get_session(&self, id: SessionId) -> StoreResult<Option<Session>> {
        sqlx::query(concat!(
            "SELECT ",
            session_cols!(),
            " FROM sessions s WHERE s.id = $1"
        ))
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(session_from)
        .transpose()
    }

    async fn list_sessions(
        &self,
        project: Option<ProjectId>,
    ) -> StoreResult<Vec<(Agent, Session)>> {
        let sql = concat!(
            "SELECT ",
            session_cols!(),
            ", a.id AS a_id, a.project_id AS a_project_id, a.host AS a_host,
                    a.harness AS a_harness, a.created_at AS a_created_at
             FROM sessions s JOIN agents a ON a.id = s.agent_id
             WHERE $1::uuid IS NULL OR a.project_id = $1
             ORDER BY s.started_at, s.id"
        );
        sqlx::query(sql)
            .bind(project.map(|p| p.as_uuid()))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| Ok((agent_from(row, "a_")?, session_from(row)?)))
            .collect()
    }
}
