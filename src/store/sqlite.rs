//! `Store` on SQLite. Ids are stored as hyphenated UUID text and timestamps as fixed-width
//! UTC RFC3339 text (see `ts`), so both compare correctly in SQL.

use std::str::FromStr;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

use super::{Store, StoreError, StoreResult};
use crate::domain::{Agent, AgentId, Project, ProjectId, Session, SessionId, TokenId};

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Opens (creating if missing) the database at `url` and runs migrations.
    /// In-memory URLs get a single, never-recycled connection so the data survives.
    pub async fn connect(url: &str) -> StoreResult<Self> {
        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        let mut opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true);
        let mut pool = SqlitePoolOptions::new();
        if in_memory {
            pool = pool
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None);
        } else {
            opts = opts.journal_mode(SqliteJournalMode::Wal);
        }
        Self::from_pool(pool.connect_with(opts).await?).await
    }

    /// Wraps an existing pool and runs migrations.
    pub async fn from_pool(pool: SqlitePool) -> StoreResult<Self> {
        sqlx::migrate!("migrations/sqlite").run(&pool).await?;
        Ok(Self { pool })
    }
}

/// Fixed-width timestamp text: `2026-11-27T09:00:00.000000Z`.
mod ts {
    use time::format_description::well_known::Rfc3339;
    use time::macros::format_description;
    use time::{OffsetDateTime, UtcOffset};

    use crate::store::StoreError;

    pub fn enc(t: OffsetDateTime) -> String {
        t.to_offset(UtcOffset::UTC)
            .format(format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z"
            ))
            .expect("timestamp formats")
    }

    pub fn dec(s: &str) -> Result<OffsetDateTime, StoreError> {
        OffsetDateTime::parse(s, &Rfc3339).map_err(|e| StoreError::Corrupt(format!("{s}: {e}")))
    }
}

fn uuid_of<T: From<uuid::Uuid>>(s: &str) -> StoreResult<T> {
    uuid::Uuid::parse_str(s)
        .map(T::from)
        .map_err(|e| StoreError::Corrupt(format!("{s}: {e}")))
}

fn project_from(row: &SqliteRow) -> StoreResult<Project> {
    Ok(Project {
        id: uuid_of(row.try_get("id")?)?,
        key: row.try_get("key")?,
        display_name: row.try_get("display_name")?,
        created_at: ts::dec(row.try_get("created_at")?)?,
    })
}

/// Reads agent columns, optionally prefixed (`a_id`, ...) for joined rows.
fn agent_from(row: &SqliteRow, p: &str) -> StoreResult<Agent> {
    let project: Option<&str> = row.try_get(format!("{p}project_id").as_str())?;
    Ok(Agent {
        id: uuid_of(row.try_get(format!("{p}id").as_str())?)?,
        project_id: project.map(uuid_of).transpose()?,
        host: row.try_get(format!("{p}host").as_str())?,
        harness: row.try_get(format!("{p}harness").as_str())?,
        created_at: ts::dec(row.try_get(format!("{p}created_at").as_str())?)?,
    })
}

fn session_from(row: &SqliteRow) -> StoreResult<Session> {
    Ok(Session {
        id: uuid_of(row.try_get("id")?)?,
        agent_id: uuid_of(row.try_get("agent_id")?)?,
        token_id: uuid_of(row.try_get("token_id")?)?,
        started_at: ts::dec(row.try_get("started_at")?)?,
        last_heartbeat_at: ts::dec(row.try_get("last_heartbeat_at")?)?,
        last_tool_call_at: ts::dec(row.try_get("last_tool_call_at")?)?,
    })
}

/// Session columns (alias `s`) as a literal, so queries stay static SQL.
macro_rules! session_cols {
    () => {
        "s.id, s.agent_id, s.token_id, s.started_at, s.last_heartbeat_at, s.last_tool_call_at"
    };
}

#[async_trait]
impl Store for SqliteStore {
    async fn upsert_project(
        &self,
        key: &str,
        display_name: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Project> {
        sqlx::query(
            "INSERT INTO projects (id, key, display_name, created_at) VALUES (?, ?, ?, ?)
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(ProjectId::new().to_string())
        .bind(key)
        .bind(display_name)
        .bind(ts::enc(now))
        .execute(&self.pool)
        .await?;
        self.get_project_by_key(key)
            .await?
            .ok_or(StoreError::NotFound("project"))
    }

    async fn get_project_by_key(&self, key: &str) -> StoreResult<Option<Project>> {
        sqlx::query("SELECT id, key, display_name, created_at FROM projects WHERE key = ?")
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
        let done = sqlx::query("UPDATE projects SET display_name = ? WHERE id = ?")
            .bind(display_name)
            .bind(id.to_string())
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
        let project = project.map(|p| p.to_string());
        sqlx::query(
            "INSERT INTO agents (id, project_id, host, harness, created_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING",
        )
        .bind(AgentId::new().to_string())
        .bind(&project)
        .bind(host)
        .bind(harness)
        .bind(ts::enc(now))
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(
            "SELECT id, project_id, host, harness, created_at FROM agents
             WHERE project_id IS ? AND host = ? AND harness = ?",
        )
        .bind(&project)
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
        let now = ts::enc(now);
        sqlx::query(
            "INSERT INTO sessions
                 (id, agent_id, token_id, started_at, last_heartbeat_at, last_tool_call_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(agent.to_string())
        .bind(token.to_string())
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.get_session(id)
            .await?
            .ok_or(StoreError::NotFound("session"))
    }

    async fn touch_heartbeat(&self, id: SessionId, now: OffsetDateTime) -> StoreResult<()> {
        let done = sqlx::query("UPDATE sessions SET last_heartbeat_at = ? WHERE id = ?")
            .bind(ts::enc(now))
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound("session"));
        }
        Ok(())
    }

    async fn touch_tool_call(&self, id: SessionId, now: OffsetDateTime) -> StoreResult<()> {
        let now = ts::enc(now);
        let done = sqlx::query(
            "UPDATE sessions SET last_tool_call_at = ?, last_heartbeat_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(id.to_string())
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
            " FROM sessions s WHERE s.id = ?"
        ))
        .bind(id.to_string())
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
             WHERE ? IS NULL OR a.project_id = ?
             ORDER BY s.started_at, s.id"
        );
        let project = project.map(|p| p.to_string());
        sqlx::query(sql)
            .bind(&project)
            .bind(&project)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| Ok((agent_from(row, "a_")?, session_from(row)?)))
            .collect()
    }
}
