//! `Store` on SQLite. Ids are stored as hyphenated UUID text and timestamps as fixed-width
//! UTC RFC3339 text (see `ts`), so both compare correctly in SQL.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqliteConnection, SqlitePool};
use time::{Duration as TimeDuration, OffsetDateTime};

use super::{
    AddProjectsRequest, ClaimAttempt, ClaimRequest, EditRequest, InboxFilter, LeaseUpdate,
    MessageReceipt, ProjectsAdded, Recipient, Store, StoreError, StoreResult, TaskFilter,
    TransitionRequest, Transitioned, child_event, children_complete_event, claim_event,
    closes_child, edit_payload, projects_added_note, projects_added_payload, send_time_recipient,
};
use crate::domain::{
    Actor, Address, Agent, AgentId, Eligibility, Importance, InboxItem, Message, MessageId,
    Project, ProjectId, Session, SessionId, Task, TaskEvent, TaskEventKind, TaskId, TokenId,
    TokenInfo,
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
        lease_ttl: row
            .try_get::<Option<i64>, _>("lease_ttl_secs")?
            .map(TimeDuration::seconds),
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
        requires_plan: row.try_get("requires_plan")?,
        plan_state: parse_text(row.try_get("plan_state")?)?,
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

/// Appends `event` to the task's history and, when it belongs to a child, bubbles it into the
/// parent's thread; the last child closing moves a `claimed` parent to `review`. Runs inside
/// the write transaction of the child's state change.
async fn record_event(
    conn: &mut SqliteConnection,
    task: &Task,
    event: &TaskEvent,
    token: Option<TokenId>,
) -> StoreResult<bool> {
    insert_event(conn, event, token).await?;
    let Some(parent) = task.parent_id else {
        return Ok(false);
    };
    let Some(bubbled) = child_event(task, parent, event) else {
        return Ok(false);
    };
    insert_event(conn, &bubbled, token).await?;
    if !closes_child(event) {
        return Ok(false);
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE parent_id = ? AND status NOT IN ('done', 'cancelled')",
    )
    .bind(parent.to_string())
    .fetch_one(&mut *conn)
    .await?;
    if open > 0 {
        return Ok(false);
    }
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
        .bind(parent.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    match status.as_deref() {
        Some("claimed") => {
            sqlx::query(
                "UPDATE tasks SET status = 'review', lease_expires_at = NULL,
                        version = version + 1, updated_at = ?
                  WHERE id = ?",
            )
            .bind(ts::enc(event.at))
            .bind(parent.to_string())
            .execute(&mut *conn)
            .await?;
        }
        Some("open" | "planning" | "blocked" | "review") => {}
        _ => return Ok(false),
    }
    insert_event(conn, &children_complete_event(parent, event), token).await?;
    Ok(true)
}

/// Task columns as a literal, so queries stay static SQL.
macro_rules! task_cols {
    () => {
        "id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
         lease_expires_at, lease_epoch, lease_ttl_secs, eligible_harness, eligible_host,
         created_by_agent_id, created_by_session_id, created_at, updated_at, version,
         requires_plan, plan_state"
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

    // A `requires_plan` task is claimed into `planning`; a planning task stays planning on a
    // takeover, and a submitted plan (waiting on the human) keeps having no lease.
    let claimed = sqlx::query(concat!(
        "UPDATE tasks
            SET owner_session_id = ?, owner_agent_id = ?,
                status = CASE WHEN status = 'open' AND requires_plan THEN 'planning'
                              WHEN status = 'planning' THEN 'planning'
                              ELSE 'claimed' END,
                plan_state = CASE WHEN status = 'open' AND requires_plan THEN 'drafting'
                                  ELSE plan_state END,
                lease_expires_at = CASE WHEN status = 'planning' AND plan_state = 'submitted'
                                        THEN NULL ELSE ? END,
                lease_epoch = lease_epoch + 1, version = version + 1, updated_at = ?
          WHERE id = ?
            AND (status = 'open'
                 OR (status IN ('claimed', 'planning') AND lease_expires_at < ?)
                 OR (status IN ('claimed', 'planning', 'blocked', 'review') AND owner_agent_id = ?
                     AND (owner_session_id = ?
                          OR NOT EXISTS (SELECT 1 FROM sessions s
                                          WHERE s.id = tasks.owner_session_id
                                            AND s.last_heartbeat_at >= ?))))
            AND ",
        eligible!(),
        " RETURNING ",
        task_cols!()
    ))
    .bind(c.session.to_string())
    .bind(&agent)
    .bind(ts::enc(req.lease_until(&previous)))
    .bind(&now)
    .bind(&id)
    .bind(&now)
    .bind(&agent)
    .bind(c.session.to_string())
    .bind(ts::enc(req.live_since))
    .bind(&c.harness)
    .bind(&c.host)
    .bind(&project)
    .bind(&project)
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

/// Message columns (alias `m`) as a literal, so queries stay static SQL.
macro_rules! message_cols {
    () => {
        "m.id, m.project_id, m.from_agent_id, m.from_session_id, m.to_kind, m.to_id, m.task_id,
         m.subject, m.body, m.importance, m.reply_to, m.created_at, m.kind"
    };
}

/// Recipient rows (alias-free, so usable in `UPDATE`) belonging to `?1` agent / `?2` session:
/// the agent's own rows, or the session's. For the human both are NULL, which matches only
/// the human's rows.
macro_rules! owned_rows {
    () => {
        "((recipient_agent_id IS ?1 AND recipient_session_id IS NULL)
          OR (recipient_agent_id IS NULL AND recipient_session_id = ?2))"
    };
}

/// `owned_rows` joined to `messages m`, minus agent rows for mail the agent sent itself.
macro_rules! inbox_rows {
    () => {
        concat!(
            owned_rows!(),
            " AND (recipient_session_id IS NOT NULL OR m.from_agent_id IS NOT ?1)"
        )
    };
}

/// Project / task messages (alias `m`) due to `?1` (agent, or NULL for the human) that it has
/// no row for yet: broadcasts to its home project after its cursor, and posts in threads of
/// tasks it owns or created. Never its own mail.
macro_rules! pending_messages {
    () => {
        "m.from_agent_id IS NOT ?1
         AND ((m.to_kind = 'project'
               AND EXISTS (SELECT 1 FROM agent_cursors c
                           WHERE c.agent_id = ?1 AND c.project_id = m.to_id
                             AND m.seq > c.acked_seq))
              OR (m.to_kind = 'task'
                  AND EXISTS (SELECT 1 FROM tasks t
                              WHERE t.id = m.task_id
                                AND (t.owner_agent_id = ?1 OR t.created_by_agent_id IS ?1))))
         AND NOT EXISTS (SELECT 1 FROM message_recipients r
                         WHERE r.message_id = m.id AND r.recipient_agent_id IS ?1
                           AND r.recipient_session_id IS NULL)"
    };
}

fn message_from(row: &SqliteRow) -> StoreResult<Message> {
    let kind = parse_text(row.try_get("kind")?)?;
    let to_id: Option<uuid::Uuid> = opt_uuid(row, "to_id")?;
    let created_at = ts::dec(row.try_get("created_at")?)?;
    Ok(Message {
        id: uuid_of(row.try_get("id")?)?,
        project_id: opt_uuid(row, "project_id")?,
        from: Actor::from_ids(
            opt_uuid(row, "from_agent_id")?,
            opt_uuid(row, "from_session_id")?,
        ),
        to: Address::from_columns(row.try_get("to_kind")?, to_id).map_err(StoreError::Corrupt)?,
        subject: row.try_get("subject")?,
        body: row.try_get("body")?,
        importance: parse_text(row.try_get("importance")?)?,
        task_id: opt_uuid(row, "task_id")?,
        reply_to: opt_uuid(row, "reply_to")?,
        created_at,
        kind,
    })
}

async fn insert_message(
    conn: &mut SqliteConnection,
    m: &Message,
    token: Option<TokenId>,
) -> StoreResult<()> {
    let (from_agent, from_session) = m.from.ids();
    let (to_kind, to_id) = m.to.columns();
    sqlx::query(
        "INSERT INTO messages
             (id, project_id, from_agent_id, from_session_id, token_id, to_kind, to_id, task_id,
              subject, body, importance, reply_to, created_at, kind)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(m.id.to_string())
    .bind(m.project_id.map(|p| p.to_string()))
    .bind(from_agent.map(|a| a.to_string()))
    .bind(from_session.map(|s| s.to_string()))
    .bind(token.map(|t| t.to_string()))
    .bind(to_kind)
    .bind(to_id.map(|i| i.to_string()))
    .bind(m.task_id.map(|t| t.to_string()))
    .bind(&m.subject)
    .bind(&m.body)
    .bind(m.importance.as_str())
    .bind(m.reply_to.map(|r| r.to_string()))
    .bind(ts::enc(m.created_at))
    .bind(m.kind.as_str())
    .execute(&mut *conn)
    .await?;
    if let Some((agent, session)) = send_time_recipient(&m.to) {
        sqlx::query(
            "INSERT INTO message_recipients (message_id, recipient_agent_id, recipient_session_id)
             VALUES (?, ?, ?)",
        )
        .bind(m.id.to_string())
        .bind(agent.map(|a| a.to_string()))
        .bind(session.map(|s| s.to_string()))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `(agent, session)` bind values of a recipient.
fn recipient_binds(to: Recipient) -> (Option<String>, Option<String>) {
    let (agent, session) = to.ids();
    (agent.map(|a| a.to_string()), session.map(|s| s.to_string()))
}

/// Gives the recipient a row for every project / task message now due to it.
async fn materialize(conn: &mut SqliteConnection, to: Recipient) -> StoreResult<()> {
    let (agent, _) = recipient_binds(to);
    sqlx::query(concat!(
        "INSERT INTO message_recipients (message_id, recipient_agent_id)
         SELECT m.id, ?1 FROM messages m WHERE ",
        pending_messages!(),
        " ON CONFLICT DO NOTHING"
    ))
    .bind(agent)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Runs a per-id `UPDATE` (binds `?1` agent, `?2` session, `?3` now, `?4` message id) on the
/// recipient's rows after materialising them; returns the rows changed.
async fn update_owned(
    pool: &SqlitePool,
    to: Recipient,
    ids: &[MessageId],
    now: OffsetDateTime,
    sql: &'static str,
) -> StoreResult<u64> {
    let (agent, session) = recipient_binds(to);
    let now = ts::enc(now);
    let mut tx = pool.begin_with(BEGIN_WRITE).await?;
    materialize(&mut tx, to).await?;
    let mut changed = 0;
    for id in ids {
        changed += sqlx::query(sql)
            .bind(&agent)
            .bind(&session)
            .bind(&now)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

/// An FTS5 query matching every whitespace-separated word of `query` as a literal phrase, so
/// FTS syntax characters in the input are never parsed as operators.
fn fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Session columns (alias `s`) as a literal, so queries stay static SQL.
macro_rules! session_cols {
    () => {
        "s.id, s.agent_id, s.token_id, s.started_at, s.last_heartbeat_at, s.last_tool_call_at"
    };
}

fn token_from(row: &SqliteRow) -> StoreResult<TokenInfo> {
    let last_used: Option<&str> = row.try_get("last_used_at")?;
    let revoked: Option<&str> = row.try_get("revoked_at")?;
    Ok(TokenInfo {
        id: uuid_of(row.try_get("id")?)?,
        label: row.try_get("label")?,
        created_at: ts::dec(row.try_get("created_at")?)?,
        last_used_at: last_used.map(ts::dec).transpose()?,
        revoked_at: revoked.map(ts::dec).transpose()?,
    })
}

#[async_trait]
impl Store for SqliteStore {
    async fn ping(&self) -> StoreResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    async fn insert_token(
        &self,
        id: TokenId,
        label: &str,
        hash: &str,
        now: OffsetDateTime,
    ) -> StoreResult<()> {
        sqlx::query("INSERT INTO tokens (id, label, hash, created_at) VALUES (?, ?, ?, ?)")
            .bind(id.to_string())
            .bind(label)
            .bind(hash)
            .bind(ts::enc(now))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn use_token(&self, hash: &str, now: OffsetDateTime) -> StoreResult<Option<TokenId>> {
        let id: Option<String> = sqlx::query_scalar(
            "UPDATE tokens SET last_used_at = ? WHERE hash = ? AND revoked_at IS NULL
             RETURNING id",
        )
        .bind(ts::enc(now))
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        id.as_deref().map(uuid_of).transpose()
    }

    async fn revoke_token(&self, id: TokenId, now: OffsetDateTime) -> StoreResult<()> {
        let found: Option<String> = sqlx::query_scalar(
            "UPDATE tokens SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ? RETURNING id",
        )
        .bind(ts::enc(now))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        found.map(|_| ()).ok_or(StoreError::NotFound("token"))
    }

    async fn list_tokens(&self) -> StoreResult<Vec<TokenInfo>> {
        sqlx::query(
            "SELECT id, label, created_at, last_used_at, revoked_at FROM tokens
             ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(token_from)
        .collect()
    }

    async fn insert_outbox(
        &self,
        kind: &str,
        project: Option<ProjectId>,
        payload: &serde_json::Value,
        now: OffsetDateTime,
    ) -> StoreResult<()> {
        let now = ts::enc(now);
        sqlx::query(
            "INSERT INTO outbox (kind, project_id, payload, created_at, published_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(kind)
        .bind(project.map(|p| p.to_string()))
        .bind(payload.to_string())
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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

    async fn get_project(&self, id: ProjectId) -> StoreResult<Option<Project>> {
        sqlx::query("SELECT id, key, display_name, created_at FROM projects WHERE id = ?")
            .bind(id.to_string())
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
        let id = AgentId::new().to_string();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let inserted = sqlx::query(
            "INSERT INTO agents (id, project_id, host, harness, created_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(&project)
        .bind(host)
        .bind(harness)
        .bind(ts::enc(now))
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if inserted && project.is_some() {
            sqlx::query(
                "INSERT INTO agent_cursors (agent_id, project_id, acked_seq, updated_at)
                 VALUES (?, ?, (SELECT COALESCE(MAX(seq), 0) FROM messages), ?)",
            )
            .bind(&id)
            .bind(&project)
            .bind(ts::enc(now))
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query(
            "SELECT id, project_id, host, harness, created_at FROM agents
             WHERE project_id IS ? AND host = ? AND harness = ?",
        )
        .bind(&project)
        .bind(host)
        .bind(harness)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        agent_from(&row, "")
    }

    async fn get_agent(&self, id: AgentId) -> StoreResult<Option<Agent>> {
        sqlx::query("SELECT id, project_id, host, harness, created_at FROM agents WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(|row| agent_from(row, ""))
            .transpose()
    }

    async fn list_agents(&self, limit: u32) -> StoreResult<Vec<Agent>> {
        sqlx::query(
            "SELECT id, project_id, host, harness, created_at FROM agents
             ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| agent_from(row, ""))
        .collect()
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
        first_message: &Message,
    ) -> StoreResult<bool> {
        let (by_agent, by_session) = task.created_by.ids();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        if let Some(parent) = task.parent_id {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
                    .bind(parent.to_string())
                    .fetch_optional(&mut *tx)
                    .await?;
            if status.as_deref() == Some("planning") {
                tx.commit().await?;
                return Ok(false);
            }
        }
        sqlx::query(
            "INSERT INTO tasks
                 (id, project_id, parent_id, title, body, status, owner_session_id,
                  owner_agent_id, lease_expires_at, lease_epoch, lease_ttl_secs,
                  eligible_harness, eligible_host, created_by_agent_id, created_by_session_id,
                  token_id, created_at, updated_at, version, requires_plan, plan_state)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(task.lease_ttl.map(|d| d.whole_seconds()))
        .bind(&task.eligibility.harness)
        .bind(&task.eligibility.host)
        .bind(by_agent.map(|a| a.to_string()))
        .bind(by_session.map(|s| s.to_string()))
        .bind(token.map(|t| t.to_string()))
        .bind(ts::enc(task.created_at))
        .bind(ts::enc(task.updated_at))
        .bind(task.version)
        .bind(task.requires_plan)
        .bind(task.plan_state.as_str())
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
        insert_message(&mut tx, first_message, token).await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn add_task_projects(
        &self,
        req: &AddProjectsRequest<'_>,
    ) -> StoreResult<Option<ProjectsAdded>> {
        let (agent, session) = match req.editor {
            Some((a, s)) => (Some(a.to_string()), Some(s.to_string())),
            None => (None, None),
        };
        let id = req.task.to_string();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        // The guard, as a read under the write lock (a no-op addition writes nothing).
        let guarded = sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks
             WHERE id = ?1 AND parent_id IS NULL AND status NOT IN ('done', 'cancelled')
               AND (?2 IS NULL OR created_by_agent_id = ?2
                    OR (owner_session_id = ?3
                        AND status IN ('planning', 'claimed', 'blocked', 'review')))"
        ))
        .bind(&id)
        .bind(&agent)
        .bind(&session)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()?;
        let Some(mut task) = guarded else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut added = Vec::new();
        let mut names = Vec::new();
        for project in req.projects {
            if *project == task.project_id || added.contains(project) {
                continue;
            }
            let inserted = sqlx::query(
                "INSERT INTO task_projects (task_id, project_id) VALUES (?, ?)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&id)
            .bind(project.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
            if inserted {
                added.push(*project);
                let name: String =
                    sqlx::query_scalar("SELECT display_name FROM projects WHERE id = ?")
                        .bind(project.to_string())
                        .fetch_one(&mut *tx)
                        .await?;
                names.push(name);
            }
        }
        if !added.is_empty() {
            task = sqlx::query(concat!(
                "UPDATE tasks SET version = version + 1, updated_at = ? WHERE id = ? RETURNING ",
                task_cols!()
            ))
            .bind(ts::enc(req.event.at))
            .bind(&id)
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::from)
            .and_then(|row| task_from(&row))?;
            let mut event = req.event.clone();
            event.payload = Some(projects_added_payload(&added));
            insert_event(&mut tx, &event, req.token).await?;
            let mut note = req.note.clone();
            note.body = projects_added_note(&names);
            insert_message(&mut tx, &note, req.token).await?;
        }
        tx.commit().await?;
        Ok(Some(ProjectsAdded { task, added }))
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

    async fn task_projects(&self, id: TaskId) -> StoreResult<Vec<ProjectId>> {
        sqlx::query_scalar::<_, String>(
            "SELECT project_id FROM task_projects WHERE task_id = ? ORDER BY project_id",
        )
        .bind(id.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|p| uuid_of(p))
        .collect()
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
               AND (?5 IS NULL
                    OR (created_at, id) > (SELECT created_at, id FROM tasks WHERE id = ?5))
             ORDER BY created_at, id
             LIMIT ?6"
        ))
        .bind(project)
        .bind(status)
        .bind(owner)
        .bind(parent)
        .bind(filter.after.map(|t| t.to_string()))
        .bind(filter.limit.map_or(-1, i64::from))
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

    async fn task_events_page(
        &self,
        id: TaskId,
        kind: Option<TaskEventKind>,
        before: Option<uuid::Uuid>,
        limit: u32,
    ) -> StoreResult<Vec<TaskEvent>> {
        sqlx::query(
            "SELECT id, task_id, kind, actor_agent_id, actor_session_id, note, payload, created_at
             FROM task_events
             WHERE task_id = ?1 AND (?2 IS NULL OR kind = ?2)
               AND (?3 IS NULL
                    OR (created_at, id) < (SELECT b.created_at, b.id FROM task_events b
                                           WHERE b.id = ?3))
             ORDER BY created_at DESC, id DESC LIMIT ?4",
        )
        .bind(id.to_string())
        .bind(kind.map(|k| k.as_str()))
        .bind(before.map(|b| b.to_string()))
        .bind(i64::from(limit))
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
        // Same reclaimable set as the claim guard minus same-agent takeover: open work and
        // abandoned (expired) claims. `claim_locked` then applies the full guard.
        let picked: Option<String> = sqlx::query_scalar(concat!(
            "SELECT id FROM tasks
              WHERE project_id = ?
                AND (status = 'open'
                     OR (status IN ('claimed', 'planning') AND lease_expires_at < ?))
                AND ",
            eligible!(),
            " ORDER BY created_at, id LIMIT 1"
        ))
        .bind(project.to_string())
        .bind(ts::enc(req.now))
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

    async fn transition_task(&self, req: &TransitionRequest) -> StoreResult<Option<Transitioned>> {
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
        let plan_from = req.plan_from.as_ref().map(|states| {
            format!(
                ",{},",
                states
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        });
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let updated = sqlx::query(concat!(
            "UPDATE tasks
                SET status = COALESCE(?, status),
                    plan_state = COALESCE(?, plan_state),
                    lease_expires_at = CASE WHEN ? THEN lease_expires_at ELSE ? END,
                    owner_session_id = CASE WHEN ? THEN NULL ELSE owner_session_id END,
                    owner_agent_id = CASE WHEN ? THEN NULL ELSE owner_agent_id END,
                    version = version + 1, updated_at = ?
              WHERE id = ?
                AND instr(?, ',' || status || ',') > 0
                AND (? IS NULL OR instr(?, ',' || plan_state || ',') > 0)
                AND (? IS NULL OR owner_session_id = ?)
                AND (? IS NULL
                     OR ? = (SELECT p.id FROM messages p
                              WHERE p.task_id = tasks.id AND p.kind = 'plan'
                              ORDER BY p.seq DESC LIMIT 1))
              RETURNING ",
            task_cols!()
        ))
        .bind(req.to.map(|s| s.as_str()))
        .bind(req.plan_to.map(|s| s.as_str()))
        .bind(keep_lease)
        .bind(lease)
        .bind(req.clear_owner)
        .bind(req.clear_owner)
        .bind(ts::enc(req.now))
        .bind(req.task.to_string())
        .bind(from)
        .bind(&plan_from)
        .bind(&plan_from)
        .bind(&owner)
        .bind(&owner)
        .bind(req.latest_plan.map(|m| m.to_string()))
        .bind(req.latest_plan.map(|m| m.to_string()))
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()?;
        let Some(task) = updated else {
            tx.commit().await?;
            return Ok(None);
        };
        let children_complete = record_event(&mut tx, &task, &req.event, req.token).await?;
        if req.archive_plans {
            sqlx::query(
                "UPDATE message_recipients
                    SET acked_at = ?1, delivered_at = COALESCE(delivered_at, ?1)
                  WHERE recipient_agent_id IS NULL AND recipient_session_id IS NULL
                    AND acked_at IS NULL
                    AND message_id IN (SELECT id FROM messages
                                        WHERE task_id = ?2 AND kind = 'plan')",
            )
            .bind(ts::enc(req.now))
            .bind(req.task.to_string())
            .execute(&mut *tx)
            .await?;
        }
        if let Some(message) = &req.message {
            insert_message(&mut tx, message, req.token).await?;
        }
        tx.commit().await?;
        Ok(Some(Transitioned {
            task,
            children_complete,
        }))
    }

    async fn edit_task(&self, req: &EditRequest<'_>) -> StoreResult<Option<Task>> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let previous: Option<(String, String)> =
            sqlx::query_as("SELECT title, body FROM tasks WHERE id = ?")
                .bind(req.task.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        let Some((title, body)) = previous else {
            return Ok(None);
        };
        let updated = sqlx::query(concat!(
            "UPDATE tasks
                SET title = COALESCE(?1, title), body = COALESCE(?2, body),
                    version = version + 1, updated_at = ?3
              WHERE id = ?4
                AND status NOT IN ('done', 'cancelled')
                AND (created_by_agent_id = ?5
                     OR (owner_session_id = ?6
                         AND status IN ('planning', 'claimed', 'blocked', 'review')))
              RETURNING ",
            task_cols!()
        ))
        .bind(req.title)
        .bind(req.body)
        .bind(ts::enc(req.event.at))
        .bind(req.task.to_string())
        .bind(req.agent.to_string())
        .bind(req.session.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()?;
        if updated.is_some() {
            let mut event = req.event.clone();
            event.payload = Some(edit_payload(req, title, body));
            insert_event(&mut tx, &event, req.token).await?;
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
        default_ttl: TimeDuration,
    ) -> StoreResult<u64> {
        // Timestamps are fixed-width text, so the per-task expiry is computed in Rust; the
        // write transaction keeps the select and the updates consistent.
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let live: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT id, lease_ttl_secs FROM tasks
             WHERE owner_session_id = ? AND status IN ('claimed', 'planning')
               AND lease_expires_at >= ?",
        )
        .bind(session.to_string())
        .bind(ts::enc(now))
        .fetch_all(&mut *tx)
        .await?;
        for (id, ttl) in &live {
            let ttl = ttl.map_or(default_ttl, TimeDuration::seconds);
            sqlx::query("UPDATE tasks SET lease_expires_at = ? WHERE id = ?")
                .bind(ts::enc(now + ttl))
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(live.len() as u64)
    }

    async fn insert_message(&self, message: &Message, token: Option<TokenId>) -> StoreResult<()> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        insert_message(&mut tx, message, token).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert_human_reply(&self, message: &Message, now: OffsetDateTime) -> StoreResult<()> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        insert_message(&mut tx, message, None).await?;
        if let Some(original) = message.reply_to {
            sqlx::query(
                "UPDATE message_recipients
                    SET acked_at = ?1, delivered_at = COALESCE(delivered_at, ?1),
                        read_at = COALESCE(read_at, ?1)
                  WHERE message_id = ?2 AND recipient_agent_id IS NULL
                    AND recipient_session_id IS NULL AND acked_at IS NULL",
            )
            .bind(ts::enc(now))
            .bind(original.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn get_message(&self, id: MessageId) -> StoreResult<Option<Message>> {
        sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            " FROM messages m WHERE m.id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(message_from)
        .transpose()
    }

    async fn inbox(
        &self,
        to: Recipient,
        filter: &InboxFilter,
        now: OffsetDateTime,
    ) -> StoreResult<Vec<InboxItem>> {
        let (agent, session) = recipient_binds(to);
        let now = ts::enc(now);
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        materialize(&mut tx, to).await?;
        let rows = sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            ", r.rowid AS r_rowid, r.delivered_at AS r_delivered_at, r.read_at AS r_read_at
             FROM message_recipients r JOIN messages m ON m.id = r.message_id
             WHERE r.acked_at IS NULL AND ",
            inbox_rows!(),
            " AND (?3 IS NULL OR m.project_id = ?3)
              AND (?4 IS NULL OR m.task_id = ?4)
              AND (?5 IS NULL OR m.importance = ?5)
             ORDER BY m.seq LIMIT ?6"
        ))
        .bind(&agent)
        .bind(&session)
        .bind(filter.project.map(|p| p.to_string()))
        .bind(filter.task.map(|t| t.to_string()))
        .bind(filter.importance.map(|i| i.as_str()))
        .bind(filter.limit.map_or(-1, i64::from))
        .fetch_all(&mut *tx)
        .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let delivered: Option<&str> = row.try_get("r_delivered_at")?;
            let delivered_at = match delivered {
                Some(t) => ts::dec(t)?,
                None => {
                    sqlx::query("UPDATE message_recipients SET delivered_at = ? WHERE rowid = ?")
                        .bind(&now)
                        .bind(row.try_get::<i64, _>("r_rowid")?)
                        .execute(&mut *tx)
                        .await?;
                    ts::dec(&now)?
                }
            };
            let read: Option<&str> = row.try_get("r_read_at")?;
            items.push(InboxItem {
                message: message_from(row)?,
                delivered_at,
                read_at: read.map(ts::dec).transpose()?,
            });
        }
        tx.commit().await?;
        Ok(items)
    }

    async fn ack_messages(
        &self,
        to: Recipient,
        ids: &[MessageId],
        now: OffsetDateTime,
    ) -> StoreResult<u64> {
        update_owned(
            &self.pool,
            to,
            ids,
            now,
            concat!(
                "UPDATE message_recipients
                    SET acked_at = ?3, acked_by_session_id = ?2,
                        delivered_at = COALESCE(delivered_at, ?3)
                  WHERE message_id = ?4 AND acked_at IS NULL AND ",
                owned_rows!()
            ),
        )
        .await
    }

    async fn mark_read(
        &self,
        to: Recipient,
        ids: &[MessageId],
        now: OffsetDateTime,
    ) -> StoreResult<u64> {
        update_owned(
            &self.pool,
            to,
            ids,
            now,
            concat!(
                "UPDATE message_recipients
                    SET read_at = ?3, delivered_at = COALESCE(delivered_at, ?3)
                  WHERE message_id = ?4 AND read_at IS NULL AND ",
                owned_rows!()
            ),
        )
        .await
    }

    async fn unread_count(
        &self,
        to: Recipient,
        importance: Option<Importance>,
    ) -> StoreResult<u64> {
        let (agent, session) = recipient_binds(to);
        let n: i64 = sqlx::query_scalar(concat!(
            "SELECT (SELECT COUNT(*) FROM message_recipients r
                     JOIN messages m ON m.id = r.message_id
                     WHERE r.acked_at IS NULL AND ",
            inbox_rows!(),
            " AND (?3 IS NULL OR m.importance = ?3)) + (SELECT COUNT(*) FROM messages m WHERE ",
            pending_messages!(),
            " AND (?3 IS NULL OR m.importance = ?3))"
        ))
        .bind(&agent)
        .bind(&session)
        .bind(importance.map(|i| i.as_str()))
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }

    async fn search_messages(
        &self,
        query: &str,
        project: Option<ProjectId>,
        limit: u32,
    ) -> StoreResult<Vec<Message>> {
        sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            " FROM messages_fts JOIN messages m ON m.seq = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND (?2 IS NULL OR m.project_id = ?2)
             ORDER BY m.seq DESC LIMIT ?3"
        ))
        .bind(fts_query(query))
        .bind(project.map(|p| p.to_string()))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(message_from)
        .collect()
    }

    async fn task_thread(&self, task: TaskId) -> StoreResult<Vec<Message>> {
        sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            " FROM messages m WHERE m.task_id = ? ORDER BY m.seq"
        ))
        .bind(task.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(message_from)
        .collect()
    }

    async fn task_thread_page(
        &self,
        task: TaskId,
        before: Option<MessageId>,
        limit: u32,
    ) -> StoreResult<Vec<Message>> {
        sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            " FROM messages m
             WHERE m.task_id = ?1
               AND (?2 IS NULL OR m.seq < (SELECT b.seq FROM messages b WHERE b.id = ?2))
             ORDER BY m.seq DESC LIMIT ?3"
        ))
        .bind(task.to_string())
        .bind(before.map(|b| b.to_string()))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(message_from)
        .collect()
    }

    async fn list_messages(
        &self,
        project: Option<ProjectId>,
        before: Option<MessageId>,
        limit: u32,
    ) -> StoreResult<Vec<Message>> {
        sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            " FROM messages m
             WHERE (?1 IS NULL OR m.project_id = ?1)
               AND (?2 IS NULL OR m.seq < (SELECT b.seq FROM messages b WHERE b.id = ?2))
             ORDER BY m.seq DESC LIMIT ?3"
        ))
        .bind(project.map(|p| p.to_string()))
        .bind(before.map(|b| b.to_string()))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(message_from)
        .collect()
    }

    async fn human_attention(&self, limit: u32) -> StoreResult<Vec<Message>> {
        sqlx::query(concat!(
            "SELECT ",
            message_cols!(),
            " FROM messages m
             WHERE m.importance IN ('high', 'urgent') AND m.kind <> 'plan'
               AND (EXISTS (SELECT 1 FROM message_recipients r
                            WHERE r.message_id = m.id AND r.recipient_agent_id IS NULL
                              AND r.recipient_session_id IS NULL AND r.acked_at IS NULL)
                    OR (",
            pending_messages!(),
            "))
             ORDER BY m.seq LIMIT ?2"
        ))
        .bind(Option::<String>::None)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(message_from)
        .collect()
    }

    async fn message_receipts(&self, ids: &[MessageId]) -> StoreResult<Vec<MessageReceipt>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let row = sqlx::query(
                "SELECT COUNT(*) AS n_rows, COUNT(delivered_at) AS n_delivered,
                        COUNT(read_at) AS n_read,
                        EXISTS (SELECT 1 FROM messages r WHERE r.reply_to = ?1) AS replied
                 FROM message_recipients WHERE message_id = ?1",
            )
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;
            let count = |name: &str| -> StoreResult<u32> {
                let n: i64 = row.try_get(name)?;
                Ok(u32::try_from(n).unwrap_or(u32::MAX))
            };
            out.push(MessageReceipt {
                id: *id,
                recipients: count("n_rows")?,
                delivered: count("n_delivered")?,
                read: count("n_read")?,
                replied: row.try_get::<i64, _>("replied")? != 0,
            });
        }
        Ok(out)
    }
}
