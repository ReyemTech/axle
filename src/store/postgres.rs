//! `Store` on Postgres (native `uuid` and `timestamptz`).

use async_trait::async_trait;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgConnection, PgPool, Row};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{
    ClaimAttempt, ClaimRequest, LeaseUpdate, Store, StoreError, StoreResult, TaskFilter,
    TransitionRequest, child_event, children_complete_event, claim_event, closes_child,
};
use crate::domain::{
    Actor, Agent, AgentId, Eligibility, Project, ProjectId, Session, SessionId, Task, TaskEvent,
    TaskId, TokenId,
};

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

fn opt_id<T: From<Uuid>>(row: &PgRow, col: &str) -> StoreResult<Option<T>> {
    Ok(row.try_get::<Option<Uuid>, _>(col)?.map(Into::into))
}

fn parse_text<T: std::str::FromStr<Err = String>>(s: &str) -> StoreResult<T> {
    s.parse().map_err(StoreError::Corrupt)
}

fn task_from(row: &PgRow) -> StoreResult<Task> {
    Ok(Task {
        id: row.try_get::<Uuid, _>("id")?.into(),
        project_id: row.try_get::<Uuid, _>("project_id")?.into(),
        parent_id: opt_id(row, "parent_id")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        status: parse_text(row.try_get("status")?)?,
        owner_session_id: opt_id(row, "owner_session_id")?,
        owner_agent_id: opt_id(row, "owner_agent_id")?,
        lease_expires_at: row.try_get("lease_expires_at")?,
        lease_epoch: row.try_get("lease_epoch")?,
        lease_ttl: row
            .try_get::<Option<i64>, _>("lease_ttl_secs")?
            .map(Duration::seconds),
        eligibility: Eligibility {
            harness: row.try_get("eligible_harness")?,
            host: row.try_get("eligible_host")?,
        },
        created_by: Actor::from_ids(
            opt_id(row, "created_by_agent_id")?,
            opt_id(row, "created_by_session_id")?,
        ),
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        version: row.try_get("version")?,
    })
}

fn event_from(row: &PgRow) -> StoreResult<TaskEvent> {
    let payload: Option<&str> = row.try_get("payload")?;
    Ok(TaskEvent {
        id: row.try_get("id")?,
        task_id: row.try_get::<Uuid, _>("task_id")?.into(),
        at: row.try_get("created_at")?,
        actor: Actor::from_ids(
            opt_id(row, "actor_agent_id")?,
            opt_id(row, "actor_session_id")?,
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
    conn: &mut PgConnection,
    event: &TaskEvent,
    token: Option<TokenId>,
) -> StoreResult<()> {
    let (agent, session) = event.actor.ids();
    sqlx::query(
        "INSERT INTO task_events
             (id, task_id, kind, actor_agent_id, actor_session_id, token_id, note, payload,
              created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9)",
    )
    .bind(event.id)
    .bind(event.task_id.as_uuid())
    .bind(event.kind.as_str())
    .bind(agent.map(|a| a.as_uuid()))
    .bind(session.map(|s| s.as_uuid()))
    .bind(token.map(|t| t.as_uuid()))
    .bind(&event.note)
    .bind(event.payload.as_ref().map(|p| p.to_string()))
    .bind(event.at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Appends `event` to the task's history and, when it belongs to a child, bubbles it into the
/// parent's thread; the last child closing moves a `claimed` parent to `review`. Runs inside
/// the transaction of the child's state change. The parent row is locked before the siblings
/// are counted, so two children closing concurrently cannot both miss the other.
async fn record_event(
    conn: &mut PgConnection,
    task: &Task,
    event: &TaskEvent,
    token: Option<TokenId>,
) -> StoreResult<()> {
    insert_event(conn, event, token).await?;
    let Some(parent) = task.parent_id else {
        return Ok(());
    };
    let Some(bubbled) = child_event(task, parent, event) else {
        return Ok(());
    };
    // Lock the parent before anything references it: the event insert below takes a shared
    // key lock on the parent row, and two children each holding one while waiting for the
    // exclusive lock would deadlock.
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1 FOR UPDATE")
            .bind(parent.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    insert_event(conn, &bubbled, token).await?;
    if !closes_child(event) {
        return Ok(());
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE parent_id = $1 AND status NOT IN ('done', 'cancelled')",
    )
    .bind(parent.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    if open > 0 {
        return Ok(());
    }
    match status.as_deref() {
        Some("claimed") => {
            sqlx::query(
                "UPDATE tasks SET status = 'review', lease_expires_at = NULL,
                        version = version + 1, updated_at = $2
                  WHERE id = $1",
            )
            .bind(parent.as_uuid())
            .bind(event.at)
            .execute(&mut *conn)
            .await?;
        }
        Some("open" | "blocked" | "review") => {}
        _ => return Ok(()),
    }
    insert_event(conn, &children_complete_event(parent, event), token).await
}

/// Task columns as a literal, so queries stay static SQL.
macro_rules! task_cols {
    () => {
        "id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
         lease_expires_at, lease_epoch, lease_ttl_secs, eligible_harness, eligible_host,
         created_by_agent_id, created_by_session_id, created_at, updated_at, version"
    };
}

/// Whether the claimant may claim the `tasks` row in scope. Uses `$2` harness, `$3` host,
/// `$4` home project.
macro_rules! eligible {
    () => {
        "COALESCE((eligible_harness IS NULL OR eligible_harness = $2::text)
                  AND (eligible_host IS NULL OR eligible_host = $3::text)
                  AND (project_id = $4::uuid OR EXISTS (SELECT 1 FROM task_projects tp
                                                        WHERE tp.task_id = tasks.id
                                                          AND tp.project_id = $4::uuid)),
                  false)"
    };
}

/// The guarded claim inside an open transaction. Locks the row first so `previous` is
/// exactly what the guarded update replaces (and the per-task TTL is read under the lock).
async fn claim_locked(
    conn: &mut PgConnection,
    task: TaskId,
    req: &ClaimRequest,
) -> StoreResult<ClaimAttempt> {
    let c = &req.claimant;
    let Some(row) = sqlx::query(concat!(
        "SELECT ",
        task_cols!(),
        ", ",
        eligible!(),
        " AS eligible FROM tasks WHERE id = $1 FOR UPDATE OF tasks"
    ))
    .bind(task.as_uuid())
    .bind(&c.harness)
    .bind(&c.host)
    .bind(c.project.map(|p| p.as_uuid()))
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
            SET owner_session_id = $5, owner_agent_id = $6,
                status = 'claimed', lease_expires_at = $7,
                lease_epoch = lease_epoch + 1, version = version + 1, updated_at = $8
          WHERE id = $1
            AND (status = 'open'
                 OR (status = 'claimed' AND lease_expires_at < $8)
                 OR (status IN ('claimed', 'blocked', 'review') AND owner_agent_id = $6))
            AND ",
        eligible!(),
        " RETURNING ",
        task_cols!()
    ))
    .bind(task.as_uuid())
    .bind(&c.harness)
    .bind(&c.host)
    .bind(c.project.map(|p| p.as_uuid()))
    .bind(c.session.as_uuid())
    .bind(c.agent.as_uuid())
    .bind(req.lease_until(&previous))
    .bind(req.now)
    .fetch_optional(&mut *conn)
    .await?
    .as_ref()
    .map(task_from)
    .transpose()?;

    if let Some(task) = &claimed {
        record_event(conn, task, &claim_event(&previous, req), Some(c.token)).await?;
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

    async fn insert_task(
        &self,
        task: &Task,
        extra_projects: &[ProjectId],
        token: Option<TokenId>,
        event: &TaskEvent,
    ) -> StoreResult<()> {
        let (by_agent, by_session) = task.created_by.ids();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO tasks
                 (id, project_id, parent_id, title, body, status, owner_session_id,
                  owner_agent_id, lease_expires_at, lease_epoch, lease_ttl_secs,
                  eligible_harness, eligible_host, created_by_agent_id, created_by_session_id,
                  token_id, created_at, updated_at, version)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     $17, $18, $19)",
        )
        .bind(task.id.as_uuid())
        .bind(task.project_id.as_uuid())
        .bind(task.parent_id.map(|p| p.as_uuid()))
        .bind(&task.title)
        .bind(&task.body)
        .bind(task.status.as_str())
        .bind(task.owner_session_id.map(|s| s.as_uuid()))
        .bind(task.owner_agent_id.map(|a| a.as_uuid()))
        .bind(task.lease_expires_at)
        .bind(task.lease_epoch)
        .bind(task.lease_ttl.map(|d| d.whole_seconds()))
        .bind(&task.eligibility.harness)
        .bind(&task.eligibility.host)
        .bind(by_agent.map(|a| a.as_uuid()))
        .bind(by_session.map(|s| s.as_uuid()))
        .bind(token.map(|t| t.as_uuid()))
        .bind(task.created_at)
        .bind(task.updated_at)
        .bind(task.version)
        .execute(&mut *tx)
        .await?;
        for project in extra_projects {
            sqlx::query(
                "INSERT INTO task_projects (task_id, project_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(task.id.as_uuid())
            .bind(project.as_uuid())
            .execute(&mut *tx)
            .await?;
        }
        insert_event(&mut tx, event, token).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn get_task(&self, id: TaskId) -> StoreResult<Option<Task>> {
        sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks WHERE id = $1"
        ))
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()
    }

    async fn task_projects(&self, id: TaskId) -> StoreResult<Vec<ProjectId>> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT project_id FROM task_projects WHERE task_id = $1 ORDER BY project_id",
        )
        .bind(id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
    }

    async fn list_tasks(&self, filter: &TaskFilter) -> StoreResult<Vec<Task>> {
        sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks
             WHERE ($1::uuid IS NULL OR project_id = $1
                    OR EXISTS (SELECT 1 FROM task_projects tp
                               WHERE tp.task_id = tasks.id AND tp.project_id = $1))
               AND ($2::text IS NULL OR status = $2)
               AND ($3::uuid IS NULL OR owner_agent_id = $3)
               AND ($4::uuid IS NULL OR parent_id = $4)
             ORDER BY created_at, id"
        ))
        .bind(filter.project.map(|p| p.as_uuid()))
        .bind(filter.status.map(|s| s.as_str()))
        .bind(filter.owner.map(|a| a.as_uuid()))
        .bind(filter.parent.map(|p| p.as_uuid()))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(task_from)
        .collect()
    }

    async fn task_events(&self, id: TaskId) -> StoreResult<Vec<TaskEvent>> {
        sqlx::query(
            "SELECT id, task_id, kind, actor_agent_id, actor_session_id, note,
                    payload::text AS payload, created_at
             FROM task_events WHERE task_id = $1 ORDER BY created_at, id",
        )
        .bind(id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(event_from)
        .collect()
    }

    async fn claim_task(&self, task: TaskId, req: &ClaimRequest) -> StoreResult<ClaimAttempt> {
        let mut tx = self.pool.begin().await?;
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
        let mut tx = self.pool.begin().await?;
        // Same reclaimable set as the claim guard minus same-agent takeover: open work and
        // abandoned (expired) claims. Rows another claimer has locked are skipped, and
        // `claim_locked` then applies the full guard to the row we locked.
        let picked: Option<Uuid> = sqlx::query_scalar(concat!(
            "SELECT id FROM tasks
              WHERE project_id = $1
                AND (status = 'open' OR (status = 'claimed' AND lease_expires_at < $5))
                AND ",
            eligible!(),
            " ORDER BY created_at, id
              LIMIT 1
              FOR UPDATE SKIP LOCKED"
        ))
        .bind(project.as_uuid())
        .bind(&c.harness)
        .bind(&c.host)
        .bind(c.project.map(|p| p.as_uuid()))
        .bind(req.now)
        .fetch_optional(&mut *tx)
        .await?;
        let claimed = match picked {
            Some(id) => claim_locked(&mut tx, id.into(), req).await?.claimed,
            None => None,
        };
        tx.commit().await?;
        Ok(claimed)
    }

    async fn transition_task(&self, req: &TransitionRequest) -> StoreResult<Option<Task>> {
        let from: Vec<&str> = req.from.iter().map(|s| s.as_str()).collect();
        let (keep_lease, lease) = match req.lease {
            LeaseUpdate::Keep => (true, None),
            LeaseUpdate::Clear => (false, None),
            LeaseUpdate::Set(t) => (false, Some(t)),
        };
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(concat!(
            "UPDATE tasks
                SET status = COALESCE($1::text, status),
                    lease_expires_at = CASE WHEN $2 THEN lease_expires_at
                                            ELSE $3::timestamptz END,
                    owner_session_id = CASE WHEN $4 THEN NULL ELSE owner_session_id END,
                    owner_agent_id = CASE WHEN $4 THEN NULL ELSE owner_agent_id END,
                    version = version + 1, updated_at = $5
              WHERE id = $6
                AND status = ANY($7::text[])
                AND ($8::uuid IS NULL OR owner_session_id = $8)
              RETURNING ",
            task_cols!()
        ))
        .bind(req.to.map(|s| s.as_str()))
        .bind(keep_lease)
        .bind(lease)
        .bind(req.clear_owner)
        .bind(req.now)
        .bind(req.task.as_uuid())
        .bind(&from)
        .bind(req.owner.map(|s| s.as_uuid()))
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()?;
        if let Some(task) = &updated {
            record_event(&mut tx, task, &req.event, req.token).await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    async fn session_held_task(&self, task: TaskId, session: SessionId) -> StoreResult<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM task_events
                            WHERE task_id = $1 AND actor_session_id = $2
                              AND kind IN ('claimed', 'lease_expired_takeover'))",
        )
        .bind(task.as_uuid())
        .bind(session.as_uuid())
        .fetch_one(&self.pool)
        .await?)
    }

    async fn extend_session_leases(
        &self,
        session: SessionId,
        now: OffsetDateTime,
        default_ttl: Duration,
    ) -> StoreResult<u64> {
        let done = sqlx::query(
            "UPDATE tasks
                SET lease_expires_at = $1
                    + make_interval(secs => COALESCE(lease_ttl_secs, $3)::double precision)
              WHERE owner_session_id = $2 AND status = 'claimed' AND lease_expires_at >= $1",
        )
        .bind(now)
        .bind(session.as_uuid())
        .bind(default_ttl.whole_seconds())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}
