//! `Store` on SQLite. Ids are stored as hyphenated UUID text and timestamps as fixed-width
//! UTC RFC3339 text (see `ts`), so both compare correctly in SQL.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqliteConnection, SqlitePool};
use time::OffsetDateTime;

use super::{
    ClaimAttempt, ClaimRequest, LeaseUpdate, Store, StoreError, StoreResult, TaskFilter,
    TransitionRequest, claim_event,
};
use crate::domain::{
    Actor, Agent, AgentId, Eligibility, Project, ProjectId, Session, SessionId, Task, TaskEvent,
    TaskId, TokenId,
};

/// Write transactions take the write lock up front, so a read-then-update inside one is
/// serialised against every other writer (and waits on `busy_timeout` instead of failing
/// with a lock-upgrade error).
const BEGIN_WRITE: &str = "BEGIN IMMEDIATE";

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
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
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

fn opt_uuid<T: From<uuid::Uuid>>(row: &SqliteRow, col: &str) -> StoreResult<Option<T>> {
    let v: Option<&str> = row.try_get(col)?;
    v.map(uuid_of).transpose()
}

fn parse_text<T: FromStr<Err = String>>(s: &str) -> StoreResult<T> {
    s.parse().map_err(StoreError::Corrupt)
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

fn task_from(row: &SqliteRow) -> StoreResult<Task> {
    let lease: Option<&str> = row.try_get("lease_expires_at")?;
    Ok(Task {
        id: uuid_of(row.try_get("id")?)?,
        project_id: uuid_of(row.try_get("project_id")?)?,
        parent_id: opt_uuid(row, "parent_id")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        status: parse_text(row.try_get("status")?)?,
        owner_session_id: opt_uuid(row, "owner_session_id")?,
        owner_agent_id: opt_uuid(row, "owner_agent_id")?,
        lease_expires_at: lease.map(ts::dec).transpose()?,
        lease_epoch: row.try_get("lease_epoch")?,
        eligibility: Eligibility {
            harness: row.try_get("eligible_harness")?,
            host: row.try_get("eligible_host")?,
        },
        created_by: Actor::from_ids(
            opt_uuid(row, "created_by_agent_id")?,
            opt_uuid(row, "created_by_session_id")?,
        ),
        created_at: ts::dec(row.try_get("created_at")?)?,
        updated_at: ts::dec(row.try_get("updated_at")?)?,
        version: row.try_get("version")?,
    })
}

fn event_from(row: &SqliteRow) -> StoreResult<TaskEvent> {
    let payload: Option<&str> = row.try_get("payload")?;
    Ok(TaskEvent {
        id: uuid_of(row.try_get("id")?)?,
        task_id: uuid_of(row.try_get("task_id")?)?,
        at: ts::dec(row.try_get("created_at")?)?,
        actor: Actor::from_ids(
            opt_uuid(row, "actor_agent_id")?,
            opt_uuid(row, "actor_session_id")?,
        ),
        kind: parse_text(row.try_get("kind")?)?,
        note: row.try_get("note")?,
        payload: payload
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| StoreError::Corrupt(format!("event payload: {e}")))?,
    })
}

async fn insert_event(
    conn: &mut SqliteConnection,
    event: &TaskEvent,
    token: Option<TokenId>,
) -> StoreResult<()> {
    let (agent, session) = event.actor.ids();
    sqlx::query(
        "INSERT INTO task_events
             (id, task_id, kind, actor_agent_id, actor_session_id, token_id, note, payload,
              created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.id.to_string())
    .bind(event.task_id.to_string())
    .bind(event.kind.as_str())
    .bind(agent.map(|a| a.to_string()))
    .bind(session.map(|s| s.to_string()))
    .bind(token.map(|t| t.to_string()))
    .bind(&event.note)
    .bind(event.payload.as_ref().map(|p| p.to_string()))
    .bind(ts::enc(event.at))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Task columns as a literal, so queries stay static SQL.
macro_rules! task_cols {
    () => {
        "id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
         lease_expires_at, lease_epoch, eligible_harness, eligible_host, created_by_agent_id,
         created_by_session_id, created_at, updated_at, version"
    };
}

/// Whether the claimant may claim the `tasks` row in scope. Binds, in order:
/// harness, host, home project, home project.
macro_rules! eligible {
    () => {
        "COALESCE((eligible_harness IS NULL OR eligible_harness = ?)
                  AND (eligible_host IS NULL OR eligible_host = ?)
                  AND (project_id = ? OR EXISTS (SELECT 1 FROM task_projects tp
                                                 WHERE tp.task_id = tasks.id
                                                   AND tp.project_id = ?)), 0)"
    };
}

/// The guarded claim on an already-open write transaction.
async fn claim_locked(
    conn: &mut SqliteConnection,
    task: TaskId,
    req: &ClaimRequest,
) -> StoreResult<ClaimAttempt> {
    let c = &req.claimant;
    let id = task.to_string();
    let agent = c.agent.to_string();
    let project = c.project.map(|p| p.to_string());
    let now = ts::enc(req.now);

    let Some(row) = sqlx::query(concat!(
        "SELECT ",
        task_cols!(),
        ", ",
        eligible!(),
        " AS eligible FROM tasks WHERE id = ?"
    ))
    .bind(&c.harness)
    .bind(&c.host)
    .bind(&project)
    .bind(&project)
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(ClaimAttempt {
            previous: None,
            eligible: false,
            claimed: None,
        });
    };
    let previous = task_from(&row)?;
    let eligible: bool = row.try_get("eligible")?;

    let claimed = sqlx::query(concat!(
        "UPDATE tasks
            SET owner_session_id = ?, owner_agent_id = ?,
                status = 'claimed', lease_expires_at = ?,
                lease_epoch = lease_epoch + 1, version = version + 1, updated_at = ?
          WHERE id = ?
            AND (status = 'open'
                 OR (status = 'claimed' AND lease_expires_at < ?)
                 OR (status IN ('claimed', 'blocked', 'review') AND owner_agent_id = ?))
            AND ",
        eligible!(),
        " RETURNING ",
        task_cols!()
    ))
    .bind(c.session.to_string())
    .bind(&agent)
    .bind(ts::enc(req.lease_until))
    .bind(&now)
    .bind(&id)
    .bind(&now)
    .bind(&agent)
    .bind(&c.harness)
    .bind(&c.host)
    .bind(&project)
    .bind(&project)
    .fetch_optional(&mut *conn)
    .await?
    .as_ref()
    .map(task_from)
    .transpose()?;

    if claimed.is_some() {
        insert_event(conn, &claim_event(&previous, req), Some(c.token)).await?;
    }
    Ok(ClaimAttempt {
        previous: Some(previous),
        eligible,
        claimed,
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
    async fn insert_task(
        &self,
        task: &Task,
        extra_projects: &[ProjectId],
        token: Option<TokenId>,
        event: &TaskEvent,
    ) -> StoreResult<()> {
        let (by_agent, by_session) = task.created_by.ids();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        sqlx::query(
            "INSERT INTO tasks
                 (id, project_id, parent_id, title, body, status, owner_session_id,
                  owner_agent_id, lease_expires_at, lease_epoch, eligible_harness, eligible_host,
                  created_by_agent_id, created_by_session_id, token_id, created_at, updated_at,
                  version)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task.id.to_string())
        .bind(task.project_id.to_string())
        .bind(task.parent_id.map(|p| p.to_string()))
        .bind(&task.title)
        .bind(&task.body)
        .bind(task.status.as_str())
        .bind(task.owner_session_id.map(|s| s.to_string()))
        .bind(task.owner_agent_id.map(|a| a.to_string()))
        .bind(task.lease_expires_at.map(ts::enc))
        .bind(task.lease_epoch)
        .bind(&task.eligibility.harness)
        .bind(&task.eligibility.host)
        .bind(by_agent.map(|a| a.to_string()))
        .bind(by_session.map(|s| s.to_string()))
        .bind(token.map(|t| t.to_string()))
        .bind(ts::enc(task.created_at))
        .bind(ts::enc(task.updated_at))
        .bind(task.version)
        .execute(&mut *tx)
        .await?;
        for project in extra_projects {
            sqlx::query(
                "INSERT INTO task_projects (task_id, project_id) VALUES (?, ?)
                 ON CONFLICT DO NOTHING",
            )
            .bind(task.id.to_string())
            .bind(project.to_string())
            .execute(&mut *tx)
            .await?;
        }
        insert_event(&mut tx, event, token).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_task(&self, id: TaskId) -> StoreResult<Option<Task>> {
        sqlx::query(concat!("SELECT ", task_cols!(), " FROM tasks WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(task_from)
            .transpose()
    }

    async fn list_tasks(&self, filter: &TaskFilter) -> StoreResult<Vec<Task>> {
        let project = filter.project.map(|p| p.to_string());
        let status = filter.status.map(|s| s.as_str());
        let owner = filter.owner.map(|a| a.to_string());
        let parent = filter.parent.map(|p| p.to_string());
        sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks
             WHERE (?1 IS NULL OR project_id = ?1
                    OR EXISTS (SELECT 1 FROM task_projects tp
                               WHERE tp.task_id = tasks.id AND tp.project_id = ?1))
               AND (?2 IS NULL OR status = ?2)
               AND (?3 IS NULL OR owner_agent_id = ?3)
               AND (?4 IS NULL OR parent_id = ?4)
             ORDER BY created_at, id"
        ))
        .bind(project)
        .bind(status)
        .bind(owner)
        .bind(parent)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(task_from)
        .collect()
    }

    async fn task_events(&self, id: TaskId) -> StoreResult<Vec<TaskEvent>> {
        sqlx::query(
            "SELECT id, task_id, kind, actor_agent_id, actor_session_id, note, payload, created_at
             FROM task_events WHERE task_id = ? ORDER BY created_at, id",
        )
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(event_from)
        .collect()
    }

    async fn claim_task(&self, task: TaskId, req: &ClaimRequest) -> StoreResult<ClaimAttempt> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let attempt = claim_locked(&mut tx, task, req).await?;
        tx.commit().await?;
        Ok(attempt)
    }

    async fn claim_next_task(
        &self,
        project: ProjectId,
        req: &ClaimRequest,
    ) -> StoreResult<Option<Task>> {
        let c = &req.claimant;
        let home = c.project.map(|p| p.to_string());
        // The write lock taken by BEGIN IMMEDIATE is SQLite's equivalent of SKIP LOCKED:
        // no other writer can pick the same row between this SELECT and the UPDATE.
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let picked: Option<String> = sqlx::query_scalar(concat!(
            "SELECT id FROM tasks WHERE project_id = ? AND status = 'open' AND ",
            eligible!(),
            " ORDER BY created_at, id LIMIT 1"
        ))
        .bind(project.to_string())
        .bind(&c.harness)
        .bind(&c.host)
        .bind(&home)
        .bind(&home)
        .fetch_optional(&mut *tx)
        .await?;
        let claimed = match picked {
            Some(id) => claim_locked(&mut tx, uuid_of(&id)?, req).await?.claimed,
            None => None,
        };
        tx.commit().await?;
        Ok(claimed)
    }

    async fn transition_task(&self, req: &TransitionRequest) -> StoreResult<Option<Task>> {
        let from = format!(
            ",{},",
            req.from
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        let (keep_lease, lease) = match req.lease {
            LeaseUpdate::Keep => (true, None),
            LeaseUpdate::Clear => (false, None),
            LeaseUpdate::Set(t) => (false, Some(ts::enc(t))),
        };
        let owner = req.owner.map(|s| s.to_string());
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let updated = sqlx::query(concat!(
            "UPDATE tasks
                SET status = COALESCE(?, status),
                    lease_expires_at = CASE WHEN ? THEN lease_expires_at ELSE ? END,
                    owner_session_id = CASE WHEN ? THEN NULL ELSE owner_session_id END,
                    owner_agent_id = CASE WHEN ? THEN NULL ELSE owner_agent_id END,
                    version = version + 1, updated_at = ?
              WHERE id = ?
                AND instr(?, ',' || status || ',') > 0
                AND (? IS NULL OR owner_session_id = ?)
              RETURNING ",
            task_cols!()
        ))
        .bind(req.to.map(|s| s.as_str()))
        .bind(keep_lease)
        .bind(lease)
        .bind(req.clear_owner)
        .bind(req.clear_owner)
        .bind(ts::enc(req.now))
        .bind(req.task.to_string())
        .bind(from)
        .bind(&owner)
        .bind(&owner)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()?;
        if updated.is_some() {
            insert_event(&mut tx, &req.event, req.token).await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    async fn session_held_task(&self, task: TaskId, session: SessionId) -> StoreResult<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM task_events
                            WHERE task_id = ? AND actor_session_id = ?
                              AND kind IN ('claimed', 'lease_expired_takeover'))",
        )
        .bind(task.to_string())
        .bind(session.to_string())
        .fetch_one(&self.pool)
        .await?)
    }

    async fn extend_session_leases(
        &self,
        session: SessionId,
        now: OffsetDateTime,
        until: OffsetDateTime,
    ) -> StoreResult<u64> {
        let done = sqlx::query(
            "UPDATE tasks SET lease_expires_at = ?
             WHERE owner_session_id = ? AND status = 'claimed' AND lease_expires_at >= ?",
        )
        .bind(ts::enc(until))
        .bind(session.to_string())
        .bind(ts::enc(now))
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}
