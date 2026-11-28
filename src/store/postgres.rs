//! `Store` on Postgres (native `uuid` and `timestamptz`).

use async_trait::async_trait;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgConnection, PgPool, Row};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{
    AddProjectsRequest, ClaimAttempt, ClaimRequest, EditRequest, InboxFilter, LeaseUpdate,
    MessageReceipt, NewEnrollment, NewToken, PollOutcome, ProjectsAdded, Recipient, Store,
    StoreError, StoreResult, TaskFilter, TaskOrder, TransitionRequest, Transitioned, UnblockRule,
    Unblocked, block_previous_payload, child_event, children_complete_event, claim_event,
    closes_child, edit_payload, enrollment_token_label, like_pattern, options_from, options_to,
    projects_added_note, projects_added_payload, send_time_recipient, unblocked_records,
};
use crate::domain::{
    Actor, Address, Agent, AgentId, BlockDetail, Eligibility, Enrollment, EnrollmentId,
    EnrollmentStatus, Importance, InboxItem, Message, MessageId, Project, ProjectId, Session,
    SessionId, Task, TaskEvent, TaskEventKind, TaskId, TaskStatus, TokenId, TokenInfo, WaitingOn,
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
        done_at: row.try_get("done_at")?,
        acked_at: row.try_get("acked_at")?,
        version: row.try_get("version")?,
        requires_plan: row.try_get("requires_plan")?,
        plan_state: parse_text(row.try_get("plan_state")?)?,
        block: block_from(row)?,
    })
}

fn block_from(row: &PgRow) -> StoreResult<Option<BlockDetail>> {
    let Some(reason) = row.try_get::<Option<String>, _>("block_reason")? else {
        return Ok(None);
    };
    Ok(Some(BlockDetail {
        reason,
        since: row.try_get("blocked_at")?,
        waiting_on: WaitingOn::from_columns(
            row.try_get("waiting_kind")?,
            opt_id(row, "waiting_task_id")?,
            row.try_get("waiting_text")?,
        ),
        unblock: row.try_get("unblock_text")?,
    }))
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
) -> StoreResult<bool> {
    insert_event(conn, event, token).await?;
    let Some(parent) = task.parent_id else {
        return Ok(false);
    };
    let Some(bubbled) = child_event(task, parent, event) else {
        return Ok(false);
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
        return Ok(false);
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE parent_id = $1 AND status NOT IN ('done', 'cancelled')",
    )
    .bind(parent.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    if open > 0 {
        return Ok(false);
    }
    match status.as_deref() {
        // Never a `done` row, so `done_at` / `acked_at` need no handling here.
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
        Some("open" | "planning" | "blocked" | "review") => {}
        _ => return Ok(false),
    }
    insert_event(conn, &children_complete_event(parent, event), token).await?;
    Ok(true)
}

/// The `WHERE` shared by `list_tasks` and `count_tasks` (everything but the `after` cursor).
/// `$5` is top_level, `$6` the done mode (`any`/`unseen`/`auto`), `$7` its `since`, `$8` the
/// title LIKE pattern (backslash escapes).
macro_rules! task_where {
    () => {
        "($1::uuid IS NULL OR project_id = $1
                    OR EXISTS (SELECT 1 FROM task_projects tp
                               WHERE tp.task_id = tasks.id AND tp.project_id = $1))
               AND ($2::text IS NULL OR status = $2)
               AND ($3::uuid IS NULL OR owner_agent_id = $3)
               AND ($4::uuid IS NULL OR parent_id = $4)
               AND (NOT $5::bool OR parent_id IS NULL)
               AND (status <> 'done' OR $6::text = 'any'
                    OR (acked_at IS NULL
                        AND (($6::text = 'unseen' AND done_at >= $7::timestamptz)
                             OR ($6::text = 'auto' AND done_at < $7::timestamptz))))
               AND ($8::text IS NULL OR title ILIKE $8 ESCAPE '\\')"
    };
}

/// Task columns as a literal, so queries stay static SQL.
macro_rules! task_cols {
    () => {
        "id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
         lease_expires_at, lease_epoch, lease_ttl_secs, eligible_harness, eligible_host,
         created_by_agent_id, created_by_session_id, created_at, updated_at, version,
         requires_plan, plan_state, done_at, acked_at, block_reason, blocked_at, waiting_kind,
         waiting_task_id, waiting_text, unblock_text"
    };
}

/// `SET` assignments that empty a task's block detail (a literal, so queries stay static).
macro_rules! clear_block {
    () => {
        "block_reason = NULL, blocked_at = NULL, waiting_kind = NULL, waiting_task_id = NULL,
         waiting_text = NULL, unblock_text = NULL"
    };
}

/// The transaction-scoped advisory lock that serialises writes of waits on tasks with the
/// `done` transitions that release them: without it, under READ COMMITTED, a task could start
/// waiting on another that finished concurrently and never be unblocked. A transaction takes
/// it before any task row lock (see `transition_task`), so it never deadlocks with them.
const WAIT_LOCK: i64 = 0x6178_6c5f_7761_6974; // "axl_wait"

async fn wait_lock(conn: &mut PgConnection) -> StoreResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(WAIT_LOCK)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Whether `task` may wait on `target` (see `BlockWrite`): the target exists and is not
/// closed, is neither the task nor one of its ancestors, and the chain of tasks the target
/// waits on never reaches `task`.
async fn wait_allowed(conn: &mut PgConnection, task: TaskId, target: TaskId) -> StoreResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = $1
                                              AND status NOT IN ('done', 'cancelled'))
            AND $1 <> $2
            AND $1 NOT IN (WITH RECURSIVE anc(id) AS (
                               SELECT parent_id FROM tasks WHERE id = $2 AND parent_id IS NOT NULL
                               UNION
                               SELECT t.parent_id FROM tasks t JOIN anc ON t.id = anc.id
                                WHERE t.parent_id IS NOT NULL)
                           SELECT id FROM anc)
            AND $2 NOT IN (WITH RECURSIVE chain(id) AS (
                               SELECT $1::uuid
                               UNION
                               SELECT t.waiting_task_id FROM tasks t JOIN chain ON t.id = chain.id
                                WHERE t.status = 'blocked' AND t.waiting_kind = 'task'
                                  AND t.waiting_task_id IS NOT NULL)
                           SELECT id FROM chain)",
    )
    .bind(target.as_uuid())
    .bind(task.as_uuid())
    .fetch_one(&mut *conn)
    .await?)
}

/// `done` just became done: applies `rule` to every blocked task waiting on it. The caller
/// holds `WAIT_LOCK`, taken before its first row lock.
async fn unblock_waiting(
    conn: &mut PgConnection,
    done: &Task,
    req: &TransitionRequest,
    rule: UnblockRule,
) -> StoreResult<Vec<Unblocked>> {
    let waiting: Vec<Task> = sqlx::query(concat!(
        "SELECT ",
        task_cols!(),
        " FROM tasks
         WHERE status = 'blocked' AND waiting_kind = 'task' AND waiting_task_id = $1
         ORDER BY created_at, id FOR UPDATE"
    ))
    .bind(done.id.as_uuid())
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(task_from)
    .collect::<StoreResult<_>>()?;
    let mut out = Vec::with_capacity(waiting.len());
    for dep in waiting {
        let live: bool = match dep.owner_session_id {
            Some(s) => {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM sessions
                                     WHERE id = $1 AND last_heartbeat_at >= $2)",
                )
                .bind(s.as_uuid())
                .bind(rule.live_since)
                .fetch_one(&mut *conn)
                .await?
            }
            None => false,
        };
        let row = if live {
            sqlx::query(concat!(
                "UPDATE tasks SET status = 'claimed', lease_expires_at = $1, ",
                clear_block!(),
                ", version = version + 1, updated_at = $2
                 WHERE id = $3 AND status = 'blocked' RETURNING ",
                task_cols!()
            ))
            .bind(req.now + dep.lease_ttl.unwrap_or(rule.default_ttl))
            .bind(req.now)
            .bind(dep.id.as_uuid())
            .fetch_optional(&mut *conn)
            .await?
        } else {
            sqlx::query(concat!(
                "UPDATE tasks SET status = 'open', owner_session_id = NULL, owner_agent_id = NULL,
                        lease_expires_at = NULL, plan_state = 'none', ",
                clear_block!(),
                ", version = version + 1, updated_at = $1
                 WHERE id = $2 AND status = 'blocked' RETURNING ",
                task_cols!()
            ))
            .bind(req.now)
            .bind(dep.id.as_uuid())
            .fetch_optional(&mut *conn)
            .await?
        };
        let Some(row) = row else { continue };
        let task = task_from(&row)?;
        let (event, note) = unblocked_records(&task, done, !live, req.event.actor, req.now);
        record_event(&mut *conn, &task, &event, req.token).await?;
        insert_message(&mut *conn, &note, req.token).await?;
        out.push(Unblocked { task, note });
    }
    Ok(out)
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

/// The guarded claim inside an open transaction. A `requires_plan` task is claimed into
/// `planning`; a planning task stays planning on a takeover, and a submitted plan (waiting
/// on the human) keeps having no lease. Locks the row first so `previous` is
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
                status = CASE WHEN status = 'open' AND requires_plan THEN 'planning'
                              WHEN status = 'planning' THEN 'planning'
                              ELSE 'claimed' END,
                plan_state = CASE WHEN status = 'open' AND requires_plan THEN 'drafting'
                                  ELSE plan_state END,
                lease_expires_at = CASE WHEN status = 'planning' AND plan_state = 'submitted'
                                        THEN NULL ELSE $7 END,
                lease_epoch = lease_epoch + 1, version = version + 1, updated_at = $8, ",
        clear_block!(),
        "
          WHERE id = $1
            AND (status = 'open'
                 OR (status IN ('claimed', 'planning') AND lease_expires_at < $8)
                 OR (status IN ('claimed', 'planning', 'blocked', 'review') AND owner_agent_id = $6
                     AND (owner_session_id = $5
                          OR NOT EXISTS (SELECT 1 FROM sessions s
                                          WHERE s.id = tasks.owner_session_id
                                            AND s.last_heartbeat_at >= $9))))
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
    .bind(req.live_since)
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
         m.subject, m.body, m.importance, m.reply_to, m.created_at, m.kind, m.options"
    };
}

/// Recipient rows belonging to `$1` agent / `$2` session: the agent's own rows, or the
/// session's. For the human both are NULL, which matches only the human's rows.
macro_rules! owned_rows {
    () => {
        "((recipient_agent_id IS NOT DISTINCT FROM $1::uuid AND recipient_session_id IS NULL)
          OR (recipient_agent_id IS NULL AND recipient_session_id = $2::uuid))"
    };
}

/// `owned_rows` joined to `messages m`, minus agent rows for mail the agent sent itself.
macro_rules! inbox_rows {
    () => {
        concat!(
            owned_rows!(),
            " AND (recipient_session_id IS NOT NULL
                   OR m.from_agent_id IS DISTINCT FROM $1::uuid)"
        )
    };
}

/// Project / task messages (alias `m`) due to `$1` (agent, or NULL for the human) that it has
/// no row for yet: broadcasts to its home project after its cursor, and posts in threads of
/// tasks it owns or created. Never its own mail.
macro_rules! pending_messages {
    () => {
        "m.from_agent_id IS DISTINCT FROM $1::uuid
         AND ((m.to_kind = 'project'
               AND EXISTS (SELECT 1 FROM agent_cursors c
                           WHERE c.agent_id = $1::uuid AND c.project_id = m.to_id
                             AND m.seq > c.acked_seq))
              OR (m.to_kind = 'task'
                  AND EXISTS (SELECT 1 FROM tasks t
                              WHERE t.id = m.task_id
                                AND (t.owner_agent_id = $1::uuid
                                     OR t.created_by_agent_id IS NOT DISTINCT FROM $1::uuid))))
         AND NOT EXISTS (SELECT 1 FROM message_recipients r
                         WHERE r.message_id = m.id
                           AND r.recipient_agent_id IS NOT DISTINCT FROM $1::uuid
                           AND r.recipient_session_id IS NULL)"
    };
}

fn message_from(row: &PgRow) -> StoreResult<Message> {
    Ok(Message {
        id: row.try_get::<Uuid, _>("id")?.into(),
        project_id: opt_id(row, "project_id")?,
        from: Actor::from_ids(
            opt_id(row, "from_agent_id")?,
            opt_id(row, "from_session_id")?,
        ),
        to: Address::from_columns(row.try_get("to_kind")?, row.try_get("to_id")?)
            .map_err(StoreError::Corrupt)?,
        subject: row.try_get("subject")?,
        body: row.try_get("body")?,
        importance: parse_text(row.try_get("importance")?)?,
        task_id: opt_id(row, "task_id")?,
        reply_to: opt_id(row, "reply_to")?,
        created_at: row.try_get("created_at")?,
        kind: parse_text(row.try_get("kind")?)?,
        options: options_from(row.try_get("options")?)?,
    })
}

async fn insert_message(
    conn: &mut PgConnection,
    m: &Message,
    token: Option<TokenId>,
) -> StoreResult<()> {
    let (from_agent, from_session) = m.from.ids();
    let (to_kind, to_id) = m.to.columns();
    sqlx::query(
        "INSERT INTO messages
             (id, project_id, from_agent_id, from_session_id, token_id, to_kind, to_id, task_id,
              subject, body, importance, reply_to, created_at, kind, options)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(m.id.as_uuid())
    .bind(m.project_id.map(|p| p.as_uuid()))
    .bind(from_agent.map(|a| a.as_uuid()))
    .bind(from_session.map(|s| s.as_uuid()))
    .bind(token.map(|t| t.as_uuid()))
    .bind(to_kind)
    .bind(to_id)
    .bind(m.task_id.map(|t| t.as_uuid()))
    .bind(&m.subject)
    .bind(&m.body)
    .bind(m.importance.as_str())
    .bind(m.reply_to.map(|r| r.as_uuid()))
    .bind(m.created_at)
    .bind(m.kind.as_str())
    .bind(options_to(&m.options))
    .execute(&mut *conn)
    .await?;
    if let Some((agent, session)) = send_time_recipient(&m.to) {
        sqlx::query(
            "INSERT INTO message_recipients (message_id, recipient_agent_id, recipient_session_id)
             VALUES ($1, $2, $3)",
        )
        .bind(m.id.as_uuid())
        .bind(agent.map(|a| a.as_uuid()))
        .bind(session.map(|s| s.as_uuid()))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `(agent, session)` bind values of a recipient.
fn recipient_binds(to: Recipient) -> (Option<Uuid>, Option<Uuid>) {
    let (agent, session) = to.ids();
    (agent.map(|a| a.as_uuid()), session.map(|s| s.as_uuid()))
}

/// Gives the recipient a row for every project / task message now due to it.
async fn materialize(conn: &mut PgConnection, to: Recipient) -> StoreResult<()> {
    let (agent, _) = recipient_binds(to);
    sqlx::query(concat!(
        "INSERT INTO message_recipients (message_id, recipient_agent_id)
         SELECT m.id, $1::uuid FROM messages m WHERE ",
        pending_messages!(),
        " ON CONFLICT ON CONSTRAINT message_recipients_unique DO NOTHING"
    ))
    .bind(agent)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Runs an `UPDATE` (binds `$1` agent, `$2` session, `$3` now, `$4` message ids) on the
/// recipient's rows after materialising them; returns the rows changed.
async fn update_owned(
    pool: &PgPool,
    to: Recipient,
    ids: &[MessageId],
    now: OffsetDateTime,
    sql: &'static str,
) -> StoreResult<u64> {
    let (agent, session) = recipient_binds(to);
    let ids: Vec<Uuid> = ids.iter().map(|i| i.as_uuid()).collect();
    let mut tx = pool.begin().await?;
    materialize(&mut tx, to).await?;
    let changed = sqlx::query(sql)
        .bind(agent)
        .bind(session)
        .bind(now)
        .bind(&ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(changed)
}

/// Session columns (alias `s`) as a literal, so queries stay static SQL.
macro_rules! session_cols {
    () => {
        "s.id, s.agent_id, s.token_id, s.started_at, s.last_heartbeat_at, s.last_tool_call_at"
    };
}

fn token_from(row: &PgRow) -> StoreResult<TokenInfo> {
    Ok(TokenInfo {
        id: row.try_get::<Uuid, _>("id")?.into(),
        label: row.try_get("label")?,
        created_at: row.try_get("created_at")?,
        last_used_at: row.try_get("last_used_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// The columns `enrollment_from` reads (a macro so queries can `concat!` it statically).
macro_rules! enrollment_cols {
    () => {
        "id, kind, label, host, status, token_id, created_at, expires_at, approved_at, requester_ip"
    };
}

fn enrollment_from(row: &PgRow) -> StoreResult<Enrollment> {
    Ok(Enrollment {
        id: row.try_get::<Uuid, _>("id")?.into(),
        kind: parse_text(row.try_get("kind")?)?,
        label: row.try_get("label")?,
        host: row.try_get("host")?,
        status: parse_text(row.try_get("status")?)?,
        token_id: opt_id(row, "token_id")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        approved_at: row.try_get("approved_at")?,
        requester_ip: row.try_get("requester_ip")?,
    })
}

/// Reports the state of a device enrollment that could not be consumed.
fn poll_state(e: &Enrollment, now: OffsetDateTime) -> PollOutcome {
    match e.status {
        EnrollmentStatus::Consumed | EnrollmentStatus::Expired => PollOutcome::Expired,
        _ if e.expires_at <= now => PollOutcome::Expired,
        EnrollmentStatus::Denied => PollOutcome::Denied,
        EnrollmentStatus::Pending | EnrollmentStatus::Approved => PollOutcome::Pending,
    }
}

/// The guarded `from_status -> consumed` move shared by codes and device polls, inside the
/// caller's transaction (the guarded UPDATE takes the row lock, so a racing request re-checks
/// the guard after the winner commits and finds nothing). Only when it returns a row is the
/// token inserted and linked (the label comes from that row), so a lost race creates nothing.
async fn consume_row(
    tx: &mut PgConnection,
    kind: &str,
    from_status: &str,
    secret_hash: &str,
    host: Option<&str>,
    now: OffsetDateTime,
    token: &NewToken<'_>,
) -> StoreResult<Option<Enrollment>> {
    let Some(row) = sqlx::query(concat!(
        "UPDATE enrollments SET status = 'consumed', host = COALESCE($1, host)
         WHERE secret_hash = $2 AND kind = $3 AND status = $4 AND expires_at > $5
         RETURNING ",
        enrollment_cols!()
    ))
    .bind(host)
    .bind(secret_hash)
    .bind(kind)
    .bind(from_status)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let mut e = enrollment_from(&row)?;
    sqlx::query("INSERT INTO tokens (id, label, hash, created_at) VALUES ($1, $2, $3, $4)")
        .bind(token.id.as_uuid())
        .bind(enrollment_token_label(&e))
        .bind(token.hash)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE enrollments SET token_id = $1 WHERE id = $2")
        .bind(token.id.as_uuid())
        .bind(e.id.as_uuid())
        .execute(&mut *tx)
        .await?;
    e.token_id = Some(token.id);
    Ok(Some(e))
}

#[async_trait]
impl Store for PostgresStore {
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
        sqlx::query("INSERT INTO tokens (id, label, hash, created_at) VALUES ($1, $2, $3, $4)")
            .bind(id.as_uuid())
            .bind(label)
            .bind(hash)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn use_token(&self, hash: &str, now: OffsetDateTime) -> StoreResult<Option<TokenId>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "UPDATE tokens SET last_used_at = $1 WHERE hash = $2 AND revoked_at IS NULL
             RETURNING id",
        )
        .bind(now)
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id.map(Into::into))
    }

    async fn revoke_token(&self, id: TokenId, now: OffsetDateTime) -> StoreResult<()> {
        let found: Option<Uuid> = sqlx::query_scalar(
            "UPDATE tokens SET revoked_at = COALESCE(revoked_at, $1) WHERE id = $2 RETURNING id",
        )
        .bind(now)
        .bind(id.as_uuid())
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

    async fn insert_enrollment(&self, e: &NewEnrollment<'_>) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE enrollments SET status = 'expired'
             WHERE status IN ('pending', 'approved') AND expires_at <= $1",
        )
        .bind(e.now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM enrollments WHERE expires_at < $1")
            .bind(e.now - Duration::days(1))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO enrollments (id, kind, secret_hash, user_code_hash, label, host, status,
                                      created_at, expires_at, requester_ip)
             VALUES ($1, $2, $3, $4, $5, $6, 'pending', $7, $8, $9)",
        )
        .bind(e.id.as_uuid())
        .bind(e.kind.as_str())
        .bind(e.secret_hash)
        .bind(e.user_code_hash)
        .bind(e.label)
        .bind(e.host)
        .bind(e.now)
        .bind(e.expires_at)
        .bind(e.requester_ip)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn consume_code(
        &self,
        secret_hash: &str,
        host: &str,
        now: OffsetDateTime,
        token: &NewToken<'_>,
    ) -> StoreResult<Option<Enrollment>> {
        let mut tx = self.pool.begin().await?;
        let enrollment = consume_row(
            &mut tx,
            "code",
            "pending",
            secret_hash,
            Some(host),
            now,
            token,
        )
        .await?;
        tx.commit().await?;
        Ok(enrollment)
    }

    async fn device_by_user_code(
        &self,
        user_code_hash: &str,
        now: OffsetDateTime,
    ) -> StoreResult<Option<Enrollment>> {
        sqlx::query(concat!(
            "SELECT ",
            enrollment_cols!(),
            " FROM enrollments
             WHERE user_code_hash = $1 AND kind = 'device' AND status = 'pending'
               AND expires_at > $2"
        ))
        .bind(user_code_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(enrollment_from)
        .transpose()
    }

    async fn decide_device(
        &self,
        id: EnrollmentId,
        approve: bool,
        now: OffsetDateTime,
    ) -> StoreResult<Option<Enrollment>> {
        let (status, approved_at) = if approve {
            ("approved", Some(now))
        } else {
            ("denied", None)
        };
        sqlx::query(concat!(
            "UPDATE enrollments SET status = $1, approved_at = $2
             WHERE id = $3 AND kind = 'device' AND status = 'pending' AND expires_at > $4
             RETURNING ",
            enrollment_cols!()
        ))
        .bind(status)
        .bind(approved_at)
        .bind(id.as_uuid())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(enrollment_from)
        .transpose()
    }

    async fn poll_device(
        &self,
        secret_hash: &str,
        now: OffsetDateTime,
        token: &NewToken<'_>,
    ) -> StoreResult<PollOutcome> {
        let mut tx = self.pool.begin().await?;
        if let Some(e) =
            consume_row(&mut tx, "device", "approved", secret_hash, None, now, token).await?
        {
            tx.commit().await?;
            return Ok(PollOutcome::Consumed(e));
        }
        let row = sqlx::query(concat!(
            "SELECT ",
            enrollment_cols!(),
            " FROM enrollments WHERE secret_hash = $1 AND kind = 'device'"
        ))
        .bind(secret_hash)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(PollOutcome::Expired);
        };
        let e = enrollment_from(&row)?;
        Ok(poll_state(&e, now))
    }

    async fn cancel_enrollment(&self, id: EnrollmentId, now: OffsetDateTime) -> StoreResult<bool> {
        let found: Option<Uuid> = sqlx::query_scalar(
            "UPDATE enrollments SET status = 'denied'
             WHERE id = $1 AND status = 'pending' AND expires_at > $2
             RETURNING id",
        )
        .bind(id.as_uuid())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn list_pending_enrollments(&self, now: OffsetDateTime) -> StoreResult<Vec<Enrollment>> {
        sqlx::query(concat!(
            "SELECT ",
            enrollment_cols!(),
            " FROM enrollments
             WHERE status = 'pending' AND expires_at > $1
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(now)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(enrollment_from)
        .collect()
    }

    async fn insert_outbox(
        &self,
        kind: &str,
        project: Option<ProjectId>,
        payload: &serde_json::Value,
        now: OffsetDateTime,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO outbox (kind, project_id, payload, created_at, published_at)
             VALUES ($1, $2, $3::jsonb, $4, $4)",
        )
        .bind(kind)
        .bind(project.map(|p| p.as_uuid()))
        .bind(payload.to_string())
        .bind(now)
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

    async fn get_project(&self, id: ProjectId) -> StoreResult<Option<Project>> {
        sqlx::query("SELECT id, key, display_name, created_at FROM projects WHERE id = $1")
            .bind(id.as_uuid())
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
        let id = AgentId::new().as_uuid();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO agents (id, project_id, host, harness, created_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT ON CONSTRAINT agents_identity DO NOTHING",
        )
        .bind(id)
        .bind(project)
        .bind(host)
        .bind(harness)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if inserted && project.is_some() {
            sqlx::query(
                "INSERT INTO agent_cursors (agent_id, project_id, acked_seq, updated_at)
                 VALUES ($1, $2, (SELECT COALESCE(MAX(seq), 0) FROM messages), $3)",
            )
            .bind(id)
            .bind(project)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query(
            "SELECT id, project_id, host, harness, created_at FROM agents
             WHERE project_id IS NOT DISTINCT FROM $1 AND host = $2 AND harness = $3",
        )
        .bind(project)
        .bind(host)
        .bind(harness)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        agent_from(&row, "")
    }

    async fn get_agent(&self, id: AgentId) -> StoreResult<Option<Agent>> {
        sqlx::query("SELECT id, project_id, host, harness, created_at FROM agents WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(|row| agent_from(row, ""))
            .transpose()
    }

    async fn list_agents(&self, limit: u32) -> StoreResult<Vec<Agent>> {
        sqlx::query(
            "SELECT id, project_id, host, harness, created_at FROM agents
             ORDER BY created_at DESC, id DESC LIMIT $1",
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
        first_message: &Message,
    ) -> StoreResult<bool> {
        let (by_agent, by_session) = task.created_by.ids();
        let mut tx = self.pool.begin().await?;
        if let Some(parent) = task.parent_id {
            // FOR SHARE holds off a concurrent claim moving the parent into `planning`.
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1 FOR SHARE")
                    .bind(parent.as_uuid())
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     $17, $18, $19, $20, $21)",
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
        .bind(task.requires_plan)
        .bind(task.plan_state.as_str())
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
        insert_message(&mut tx, first_message, token).await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn add_task_projects(
        &self,
        req: &AddProjectsRequest<'_>,
    ) -> StoreResult<Option<ProjectsAdded>> {
        let (agent, session) = match req.editor {
            Some((a, s)) => (Some(a.as_uuid()), Some(s.as_uuid())),
            None => (None, None),
        };
        let mut tx = self.pool.begin().await?;
        // The guard, as a locking read (a no-op addition writes nothing).
        let guarded = sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks
             WHERE id = $1 AND parent_id IS NULL AND status NOT IN ('done', 'cancelled')
               AND ($2::uuid IS NULL OR created_by_agent_id = $2
                    OR (owner_session_id = $3::uuid
                        AND status IN ('planning', 'claimed', 'blocked', 'review')))
             FOR UPDATE"
        ))
        .bind(req.task.as_uuid())
        .bind(agent)
        .bind(session)
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
                "INSERT INTO task_projects (task_id, project_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(req.task.as_uuid())
            .bind(project.as_uuid())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
            if inserted {
                added.push(*project);
                let name: String =
                    sqlx::query_scalar("SELECT display_name FROM projects WHERE id = $1")
                        .bind(project.as_uuid())
                        .fetch_one(&mut *tx)
                        .await?;
                names.push(name);
            }
        }
        if !added.is_empty() {
            let row = sqlx::query(concat!(
                "UPDATE tasks SET version = version + 1, updated_at = $1 WHERE id = $2
                 RETURNING ",
                task_cols!()
            ))
            .bind(req.event.at)
            .bind(req.task.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
            task = task_from(&row)?;
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
        let (mode, since) = filter.done.parts();
        // A literal `status = 'done'` lets the planner use the partial index `tasks_done`;
        // `DoneNewest` is only meaningful for done rows, so it always carries it.
        macro_rules! list_sql {
            ($done_only:literal, $tail:expr) => {
                concat!(
                    "SELECT ",
                    task_cols!(),
                    " FROM tasks WHERE ",
                    $done_only,
                    task_where!(),
                    $tail
                )
            };
        }
        let done_only = filter.status == Some(TaskStatus::Done);
        let sql = match (filter.order, done_only) {
            (TaskOrder::Created, false) => list_sql!(
                "",
                " AND ($9::uuid IS NULL
                    OR (created_at, id) > (SELECT created_at, id FROM tasks WHERE id = $9))
                 ORDER BY created_at, id
                 LIMIT $10"
            ),
            (TaskOrder::Created, true) => list_sql!(
                "status = 'done' AND ",
                " AND ($9::uuid IS NULL
                    OR (created_at, id) > (SELECT created_at, id FROM tasks WHERE id = $9))
                 ORDER BY created_at, id
                 LIMIT $10"
            ),
            (TaskOrder::DoneNewest, _) => list_sql!(
                "status = 'done' AND ",
                " AND ($9::uuid IS NULL
                    OR (done_at, id) < (SELECT done_at, id FROM tasks WHERE id = $9))
                 ORDER BY done_at DESC, id DESC
                 LIMIT $10"
            ),
        };
        sqlx::query(sql)
            .bind(filter.project.map(|p| p.as_uuid()))
            .bind(filter.status.map(|s| s.as_str()))
            .bind(filter.owner.map(|a| a.as_uuid()))
            .bind(filter.parent.map(|p| p.as_uuid()))
            .bind(filter.top_level)
            .bind(mode)
            .bind(since)
            .bind(filter.title.as_deref().map(like_pattern))
            .bind(filter.after.map(|t| t.as_uuid()))
            .bind(filter.limit.map(i64::from))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(task_from)
            .collect()
    }

    async fn ack_done_tasks(
        &self,
        ids: &[TaskId],
        now: OffsetDateTime,
    ) -> StoreResult<Vec<TaskId>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let listed: Vec<uuid::Uuid> = ids.iter().map(|i| i.as_uuid()).collect();
        let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM tasks WHERE id = ANY($1) AND status = 'done'
                 UNION
                 SELECT t.id FROM tasks t JOIN sub ON t.parent_id = sub.id)
             UPDATE tasks SET acked_at = $2
              WHERE id IN (SELECT id FROM sub) AND status = 'done' AND acked_at IS NULL
              RETURNING id",
        )
        .bind(listed)
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(TaskId::from).collect())
    }

    async fn count_tasks(&self, filter: &TaskFilter) -> StoreResult<u64> {
        let (mode, since) = filter.done.parts();
        let sql = if filter.status == Some(TaskStatus::Done) {
            concat!(
                "SELECT COUNT(*) FROM tasks WHERE status = 'done' AND ",
                task_where!()
            )
        } else {
            concat!("SELECT COUNT(*) FROM tasks WHERE ", task_where!())
        };
        let n: i64 = sqlx::query_scalar(sql)
            .bind(filter.project.map(|p| p.as_uuid()))
            .bind(filter.status.map(|s| s.as_str()))
            .bind(filter.owner.map(|a| a.as_uuid()))
            .bind(filter.parent.map(|p| p.as_uuid()))
            .bind(filter.top_level)
            .bind(mode)
            .bind(since)
            .bind(filter.title.as_deref().map(like_pattern))
            .fetch_one(&self.pool)
            .await?;
        Ok(n as u64)
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

    async fn task_events_page(
        &self,
        id: TaskId,
        kind: Option<TaskEventKind>,
        before: Option<Uuid>,
        limit: u32,
    ) -> StoreResult<Vec<TaskEvent>> {
        sqlx::query(
            "SELECT id, task_id, kind, actor_agent_id, actor_session_id, note,
                    payload::text AS payload, created_at
             FROM task_events
             WHERE task_id = $1 AND ($2::text IS NULL OR kind = $2)
               AND ($3::uuid IS NULL
                    OR (created_at, id) < (SELECT b.created_at, b.id FROM task_events b
                                           WHERE b.id = $3))
             ORDER BY created_at DESC, id DESC LIMIT $4",
        )
        .bind(id.as_uuid())
        .bind(kind.map(|k| k.as_str()))
        .bind(before)
        .bind(i64::from(limit))
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
                AND (status = 'open'
                     OR (status IN ('claimed', 'planning') AND lease_expires_at < $5))
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

    async fn transition_task(&self, req: &TransitionRequest) -> StoreResult<Option<Transitioned>> {
        let from: Vec<&str> = req.from.iter().map(|s| s.as_str()).collect();
        let (keep_lease, lease) = match req.lease {
            LeaseUpdate::Keep => (true, None),
            LeaseUpdate::Clear => (false, None),
            LeaseUpdate::Set(t) => (false, Some(t)),
        };
        let plan_from: Option<Vec<&str>> = req
            .plan_from
            .as_ref()
            .map(|states| states.iter().map(|s| s.as_str()).collect());
        let mut tx = self.pool.begin().await?;
        let block = req.block.clone().unwrap_or_default();
        let waiting = block.waiting_on.as_ref();
        let finishing = req.unblock.is_some() && req.to == Some(TaskStatus::Done);
        // Writes of waits on tasks and the `done` transitions that release them are serialised
        // by `WAIT_LOCK`. It is always taken first, before any task row lock: a completion
        // locks its row and its parent's, and blocking that parent on a task locks the parent
        // after the wait lock, so the other order deadlocks.
        if finishing || waiting.and_then(|w| w.task()).is_some() {
            wait_lock(&mut tx).await?;
        }
        // A decision on a named plan: lock the row first, then read the newest plan in a
        // statement of its own. Under READ COMMITTED a guard subquery inside the UPDATE would
        // keep the snapshot taken before it queued on the row lock, so a resubmission that
        // held the lock (and wrote a newer plan) would go unseen. The UPDATE then also
        // requires the version read under the lock, a guard on the row itself.
        let mut locked_version: Option<i64> = None;
        if let Some(plan) = req.latest_plan {
            let version: Option<i64> =
                sqlx::query_scalar("SELECT version FROM tasks WHERE id = $1 FOR UPDATE")
                    .bind(req.task.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await?;
            let latest: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM messages WHERE task_id = $1 AND kind = 'plan'
                  ORDER BY seq DESC LIMIT 1",
            )
            .bind(req.task.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            if version.is_none() || latest != Some(plan.as_uuid()) {
                tx.commit().await?;
                return Ok(None);
            }
            locked_version = version;
        }
        // Under the wait lock, this check sees every committed wait and every committed `done`.
        if let Some(target) = waiting.and_then(|w| w.task())
            && !wait_allowed(&mut tx, req.task, target).await?
        {
            tx.commit().await?;
            return Ok(None);
        }
        // The values an update replaces, read under the row lock the UPDATE then holds.
        let mut event = req.event.clone();
        if block.record_previous {
            let previous = sqlx::query(concat!(
                "SELECT ",
                task_cols!(),
                " FROM tasks WHERE id = $1 FOR UPDATE"
            ))
            .bind(req.task.as_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .as_ref()
            .map(task_from)
            .transpose()?
            .and_then(|t| t.block);
            event.payload = Some(block_previous_payload(
                event.payload.as_ref(),
                previous.as_ref(),
            ));
        }
        let updated = sqlx::query(concat!(
            "UPDATE tasks
                SET status = COALESCE($1::text, status),
                    plan_state = COALESCE($9::text, plan_state),
                    lease_expires_at = CASE WHEN $2 THEN lease_expires_at
                                            ELSE $3::timestamptz END,
                    owner_session_id = CASE WHEN $4 THEN NULL ELSE owner_session_id END,
                    owner_agent_id = CASE WHEN $4 THEN NULL ELSE owner_agent_id END,
                    done_at = CASE WHEN COALESCE($1::text, status) = 'done' AND status <> 'done'
                                   THEN $5
                                   WHEN COALESCE($1::text, status) = 'done' THEN done_at
                                   ELSE NULL END,
                    acked_at = CASE WHEN COALESCE($1::text, status) = 'done' THEN acked_at
                                    ELSE NULL END,
                    block_reason = CASE WHEN COALESCE($1::text, status) <> 'blocked' THEN NULL
                                        ELSE COALESCE($12::text, block_reason) END,
                    blocked_at = CASE WHEN COALESCE($1::text, status) <> 'blocked' THEN NULL
                                      WHEN status <> 'blocked' THEN $5
                                      ELSE blocked_at END,
                    waiting_kind = CASE WHEN COALESCE($1::text, status) <> 'blocked' THEN NULL
                                        WHEN $13 THEN $14::text ELSE waiting_kind END,
                    waiting_task_id = CASE WHEN COALESCE($1::text, status) <> 'blocked' THEN NULL
                                           WHEN $13 THEN $15::uuid ELSE waiting_task_id END,
                    waiting_text = CASE WHEN COALESCE($1::text, status) <> 'blocked' THEN NULL
                                        WHEN $13 THEN $16::text ELSE waiting_text END,
                    unblock_text = CASE WHEN COALESCE($1::text, status) <> 'blocked' THEN NULL
                                        ELSE COALESCE($17::text, unblock_text) END,
                    version = version + 1, updated_at = $5
              WHERE id = $6
                AND status = ANY($7::text[])
                AND ($8::uuid IS NULL OR owner_session_id = $8)
                AND ($10::text[] IS NULL OR plan_state = ANY($10::text[]))
                AND ($11::bigint IS NULL OR version = $11)
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
        .bind(req.plan_to.map(|s| s.as_str()))
        .bind(&plan_from)
        .bind(locked_version)
        .bind(&block.reason)
        .bind(waiting.is_some())
        .bind(waiting.map(|w| w.kind()))
        .bind(waiting.and_then(|w| w.task()).map(|t| t.as_uuid()))
        .bind(waiting.and_then(|w| w.text()))
        .bind(&block.unblock)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(task_from)
        .transpose()?;
        let Some(task) = updated else {
            tx.commit().await?;
            return Ok(None);
        };
        let children_complete = record_event(&mut tx, &task, &event, req.token).await?;
        if req.archive_plans {
            sqlx::query(
                "UPDATE message_recipients
                    SET acked_at = $1, delivered_at = COALESCE(delivered_at, $1)
                  WHERE recipient_agent_id IS NULL AND recipient_session_id IS NULL
                    AND acked_at IS NULL
                    AND message_id IN (SELECT id FROM messages
                                        WHERE task_id = $2 AND kind = 'plan')",
            )
            .bind(req.now)
            .bind(req.task.as_uuid())
            .execute(&mut *tx)
            .await?;
        }
        if let Some(message) = &req.message {
            insert_message(&mut tx, message, req.token).await?;
        }
        let unblocked = match req.unblock {
            Some(rule) if req.to == Some(TaskStatus::Done) && task.status == TaskStatus::Done => {
                unblock_waiting(&mut tx, &task, req, rule).await?
            }
            _ => Vec::new(),
        };
        tx.commit().await?;
        Ok(Some(Transitioned {
            task,
            children_complete,
            unblocked,
        }))
    }

    async fn edit_task(&self, req: &EditRequest<'_>) -> StoreResult<Option<Task>> {
        let mut tx = self.pool.begin().await?;
        let previous: Option<(String, String)> =
            sqlx::query_as("SELECT title, body FROM tasks WHERE id = $1 FOR UPDATE")
                .bind(req.task.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        let Some((title, body)) = previous else {
            return Ok(None);
        };
        let updated = sqlx::query(concat!(
            "UPDATE tasks
                SET title = COALESCE($1::text, title), body = COALESCE($2::text, body),
                    version = version + 1, updated_at = $3
              WHERE id = $4
                AND status NOT IN ('done', 'cancelled')
                AND (created_by_agent_id = $5
                     OR (owner_session_id = $6
                         AND status IN ('planning', 'claimed', 'blocked', 'review')))
              RETURNING ",
            task_cols!()
        ))
        .bind(req.title)
        .bind(req.body)
        .bind(req.event.at)
        .bind(req.task.as_uuid())
        .bind(req.agent.as_uuid())
        .bind(req.session.as_uuid())
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
              WHERE owner_session_id = $2 AND status IN ('claimed', 'planning')
                AND lease_expires_at >= $1",
        )
        .bind(now)
        .bind(session.as_uuid())
        .bind(default_ttl.whole_seconds())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    async fn insert_message(&self, message: &Message, token: Option<TokenId>) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        insert_message(&mut tx, message, token).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert_human_reply(&self, message: &Message, now: OffsetDateTime) -> StoreResult<()> {
        let mut tx = self.pool.begin().await?;
        insert_message(&mut tx, message, None).await?;
        if let Some(original) = message.reply_to {
            sqlx::query(
                "UPDATE message_recipients
                    SET acked_at = $1, delivered_at = COALESCE(delivered_at, $1),
                        read_at = COALESCE(read_at, $1)
                  WHERE message_id = $2 AND recipient_agent_id IS NULL
                    AND recipient_session_id IS NULL AND acked_at IS NULL",
            )
            .bind(now)
            .bind(original.as_uuid())
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
            " FROM messages m WHERE m.id = $1"
        ))
        .bind(id.as_uuid())
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
        let mut tx = self.pool.begin().await?;
        materialize(&mut tx, to).await?;
        // Stamp first delivery and read the rows back in one statement: the CTE picks the
        // rows, the UPDATE stamps the unstamped ones, and the join returns every picked row.
        let rows = sqlx::query(concat!(
            "WITH picked AS (
                 SELECT r.ctid AS rid, m.seq FROM message_recipients r
                   JOIN messages m ON m.id = r.message_id
                  WHERE r.acked_at IS NULL AND ",
            inbox_rows!(),
            "     AND ($3::uuid IS NULL OR m.project_id = $3)
                  AND ($4::uuid IS NULL OR m.task_id = $4)
                  AND ($5::text IS NULL OR m.importance = $5)
                  AND ($8::bigint IS NULL
                       OR (CASE m.importance WHEN 'urgent' THEN 2 WHEN 'high' THEN 1 ELSE 0 END)
                          >= $8)
                  ORDER BY m.seq LIMIT $6
             ),
             stamped AS (
                 UPDATE message_recipients r SET delivered_at = $7
                   FROM picked p WHERE r.ctid = p.rid AND r.delivered_at IS NULL
                 RETURNING r.message_id
             )
             SELECT ",
            message_cols!(),
            ", COALESCE(r.delivered_at, $7) AS r_delivered_at, r.read_at AS r_read_at
               FROM picked p
               JOIN message_recipients r ON r.ctid = p.rid
               JOIN messages m ON m.id = r.message_id
              ORDER BY p.seq"
        ))
        .bind(agent)
        .bind(session)
        .bind(filter.project.map(|p| p.as_uuid()))
        .bind(filter.task.map(|t| t.as_uuid()))
        .bind(filter.importance.map(|i| i.as_str()))
        .bind(filter.limit.map(i64::from))
        .bind(now)
        .bind(filter.min_importance.map(|i| i.rank()))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter()
            .map(|row| {
                Ok(InboxItem {
                    message: message_from(row)?,
                    delivered_at: row.try_get("r_delivered_at")?,
                    read_at: row.try_get("r_read_at")?,
                })
            })
            .collect()
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
                    SET acked_at = $3, acked_by_session_id = $2::uuid,
                        delivered_at = COALESCE(delivered_at, $3)
                  WHERE message_id = ANY($4) AND acked_at IS NULL AND ",
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
                    SET read_at = $3, delivered_at = COALESCE(delivered_at, $3)
                  WHERE message_id = ANY($4) AND read_at IS NULL AND ",
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
            " AND ($3::text IS NULL OR m.importance = $3)) + (SELECT COUNT(*) FROM messages m WHERE ",
            pending_messages!(),
            " AND ($3::text IS NULL OR m.importance = $3))"
        ))
        .bind(agent)
        .bind(session)
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
            " FROM messages m
             WHERE m.search @@ plainto_tsquery('simple', $1)
               AND ($2::uuid IS NULL OR m.project_id = $2)
             ORDER BY m.seq DESC LIMIT $3"
        ))
        .bind(query)
        .bind(project.map(|p| p.as_uuid()))
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
            " FROM messages m WHERE m.task_id = $1 ORDER BY m.seq"
        ))
        .bind(task.as_uuid())
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
             WHERE m.task_id = $1
               AND ($2::uuid IS NULL OR m.seq < (SELECT b.seq FROM messages b WHERE b.id = $2))
             ORDER BY m.seq DESC LIMIT $3"
        ))
        .bind(task.as_uuid())
        .bind(before.map(|b| b.as_uuid()))
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
             WHERE ($1::uuid IS NULL OR m.project_id = $1)
               AND ($2::uuid IS NULL OR m.seq < (SELECT b.seq FROM messages b WHERE b.id = $2))
             ORDER BY m.seq DESC LIMIT $3"
        ))
        .bind(project.map(|p| p.as_uuid()))
        .bind(before.map(|b| b.as_uuid()))
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
             ORDER BY m.seq LIMIT $2"
        ))
        .bind(Option::<Uuid>::None)
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
                        EXISTS (SELECT 1 FROM messages r WHERE r.reply_to = $1) AS replied
                 FROM message_recipients WHERE message_id = $1",
            )
            .bind(id.as_uuid())
            .fetch_one(&self.pool)
            .await?;
            out.push(MessageReceipt {
                id: *id,
                recipients: count(row.try_get("n_rows")?),
                delivered: count(row.try_get("n_delivered")?),
                read: count(row.try_get("n_read")?),
                replied: row.try_get("replied")?,
            });
        }
        Ok(out)
    }
}

fn count(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
