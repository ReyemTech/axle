//! `Store` on SQLite. Ids are stored as hyphenated UUID text and timestamps as fixed-width
//! UTC RFC3339 text (see `ts`), so both compare correctly in SQL.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqliteConnection, SqlitePool};
use time::{Duration as TimeDuration, OffsetDateTime};

use super::{
    AddProjectsRequest, AttemptQuery, ClaimAttempt, ClaimRequest, EditRequest, InboxFilter,
    LeaseUpdate, MessageReceipt, MoveRequest, Moved, NewEnrollment, NewToken, PollOutcome, Priced,
    Pricer, ProjectsAdded, Recipient, RelateRequest, Related, RepriceRow, RequestAdmission,
    RequestInsert, Reservation, Store, StoreError, StoreResult, StoredUsage, TaskFilter,
    TaskInsert, TaskOrder, TaskScope, TransitionRequest, Transitioned, UnblockRule, Unblocked,
    UsageBatch, UsageCounts, UsageFilter, UsageGroup, UsageRow, UsageWritten,
    block_previous_payload, child_event, children_complete_event, claim_event, closes_child,
    edit_payload, eligible, enrollment_token_label, like_pattern, options_from, options_to,
    projects_added_note, projects_added_payload, related_payload, send_time_recipient, to_u32,
    unblocked_records,
};
use crate::domain::{
    Actor, Address, Agent, AgentId, BlockDetail, Eligibility, Enrollment, EnrollmentId,
    EnrollmentStatus, Importance, InboxItem, Message, MessageId, Project, ProjectId,
    ProjectWakeSettings, ServerSettings, Session, SessionId, Task, TaskEvent, TaskEventKind,
    TaskId, TaskLink, TaskStatus, TokenId, TokenInfo, TurnState, UsageId, WaitingOn, WakeAttempt,
    WakeChannel,
};

/// The columns `enrollment_from` reads (a macro so queries can `concat!` it statically).
macro_rules! enrollment_cols {
    () => {
        "id, kind, label, host, status, token_id, created_at, expires_at, approved_at, requester_ip"
    };
}

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

fn opt_ts(row: &SqliteRow, col: &str) -> StoreResult<Option<OffsetDateTime>> {
    let text: Option<&str> = row.try_get(col)?;
    text.map(ts::dec).transpose()
}

fn session_from(row: &SqliteRow) -> StoreResult<Session> {
    Ok(Session {
        id: uuid_of(row.try_get("id")?)?,
        agent_id: uuid_of(row.try_get("agent_id")?)?,
        token_id: uuid_of(row.try_get("token_id")?)?,
        started_at: ts::dec(row.try_get("started_at")?)?,
        last_heartbeat_at: ts::dec(row.try_get("last_heartbeat_at")?)?,
        last_tool_call_at: ts::dec(row.try_get("last_tool_call_at")?)?,
        turn_state: parse_text(row.try_get("turn_state")?)?,
        turn_state_at: opt_ts(row, "turn_state_at")?,
        wake_channel: parse_text(row.try_get("wake_channel")?)?,
        wake_task_id: opt_uuid(row, "wake_task_id")?,
        wake_until: opt_ts(row, "wake_until")?,
        last_woken_at: opt_ts(row, "last_woken_at")?,
        run_id: row.try_get("run_id")?,
    })
}

/// A request chain as a JSON array of project id strings.
fn request_chain_json(chain: &[ProjectId]) -> String {
    serde_json::Value::from(chain.iter().map(ToString::to_string).collect::<Vec<_>>()).to_string()
}

fn attempt_from(row: &SqliteRow) -> StoreResult<WakeAttempt> {
    Ok(WakeAttempt {
        id: uuid_of(row.try_get("id")?)?,
        kind: parse_text(row.try_get("kind")?)?,
        trigger: parse_text(row.try_get("trigger")?)?,
        task_id: opt_uuid(row, "task_id")?,
        session_id: opt_uuid(row, "session_id")?,
        from_agent_id: opt_uuid(row, "from_agent_id")?,
        runner_id: opt_uuid(row, "runner_id")?,
        run_id: opt_uuid(row, "run_id")?,
        outcome: parse_text(row.try_get("outcome")?)?,
        detail: row.try_get("detail")?,
        pr_url: row.try_get("pr_url")?,
        at: ts::dec(row.try_get("created_at")?)?,
    })
}

fn task_from(row: &SqliteRow) -> StoreResult<Task> {
    let lease: Option<&str> = row.try_get("lease_expires_at")?;
    let done: Option<&str> = row.try_get("done_at")?;
    let acked: Option<&str> = row.try_get("acked_at")?;
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
        done_at: done.map(ts::dec).transpose()?,
        acked_at: acked.map(ts::dec).transpose()?,
        version: row.try_get("version")?,
        requires_plan: row.try_get("requires_plan")?,
        plan_state: parse_text(row.try_get("plan_state")?)?,
        block: block_from(row)?,
        sanctioned: row.try_get("sanctioned")?,
        request_depth: u32::try_from(row.try_get::<i64, _>("request_depth")?)
            .map_err(|e| StoreError::Corrupt(format!("request_depth: {e}")))?,
        origin_task_id: opt_uuid(row, "origin_task_id")?,
        request_chain: {
            let chain: Vec<String> = serde_json::from_str(row.try_get("request_chain")?)
                .map_err(|e| StoreError::Corrupt(format!("request_chain: {e}")))?;
            chain
                .iter()
                .map(|id| uuid_of(id))
                .collect::<StoreResult<_>>()?
        },
        request_dedupe_key: row.try_get("request_dedupe_key")?,
    })
}

fn block_from(row: &SqliteRow) -> StoreResult<Option<BlockDetail>> {
    let Some(reason) = row.try_get::<Option<String>, _>("block_reason")? else {
        return Ok(None);
    };
    let since: Option<&str> = row.try_get("blocked_at")?;
    Ok(Some(BlockDetail {
        reason,
        since: since.map(ts::dec).transpose()?,
        waiting_on: WaitingOn::from_columns(
            row.try_get("waiting_kind")?,
            opt_uuid(row, "waiting_task_id")?,
            row.try_get("waiting_text")?,
        ),
        unblock: row.try_get("unblock_text")?,
    }))
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
        // Never a `done` row, so `done_at` / `acked_at` need no handling here.
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

/// The `WHERE` shared by `list_tasks` and `count_tasks` (everything but the `after` cursor).
/// `?5` is top_level, `?6` the done mode (`any`/`unseen`/`auto`), `?7` its `since`, `?8` the
/// title LIKE pattern (backslash escapes).
macro_rules! task_where {
    () => {
        "(?1 IS NULL OR project_id = ?1
                    OR EXISTS (SELECT 1 FROM task_projects tp
                               WHERE tp.task_id = tasks.id AND tp.project_id = ?1))
               AND (?2 IS NULL OR status = ?2)
               AND (?3 IS NULL OR owner_agent_id = ?3)
               AND (?4 IS NULL OR parent_id = ?4)
               AND (?5 = 0 OR parent_id IS NULL)
               AND (status <> 'done' OR ?6 = 'any'
                    OR (acked_at IS NULL
                        AND ((?6 = 'unseen' AND done_at >= ?7)
                             OR (?6 = 'auto' AND done_at < ?7))))
               AND (?8 IS NULL OR title LIKE ?8 ESCAPE '\\')"
    };
}

/// Task columns as a literal, so queries stay static SQL.
macro_rules! task_cols {
    () => {
        "id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
         lease_expires_at, lease_epoch, lease_ttl_secs, eligible_harness, eligible_host,
         created_by_agent_id, created_by_session_id, created_at, updated_at, version,
         requires_plan, plan_state, done_at, acked_at, block_reason, blocked_at, waiting_kind,
         waiting_task_id, waiting_text, unblock_text, sanctioned, request_depth,
         origin_task_id, request_chain, request_dedupe_key"
    };
}

/// `SET` assignments that empty a task's block detail (a literal, so queries stay static).
macro_rules! clear_block {
    () => {
        "block_reason = NULL, blocked_at = NULL, waiting_kind = NULL, waiting_task_id = NULL,
         waiting_text = NULL, unblock_text = NULL"
    };
}

/// The statement `list_tasks` runs for `filter`. A literal `status = 'done'` lets the planner
/// use the partial index `tasks_done`; `TaskOrder::DoneNewest` is only meaningful for done
/// rows, so it always carries the literal. Public (hidden) so a test can `EXPLAIN` it.
#[doc(hidden)]
pub fn list_tasks_sql(filter: &TaskFilter) -> &'static str {
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
    macro_rules! created {
        () => {
            " AND (?9 IS NULL
                    OR (created_at, id) > (SELECT created_at, id FROM tasks WHERE id = ?9))
                 ORDER BY created_at, id
                 LIMIT ?10"
        };
    }
    match (filter.order, filter.status == Some(TaskStatus::Done)) {
        (TaskOrder::Created, false) => list_sql!("", created!()),
        (TaskOrder::Created, true) => list_sql!("status = 'done' AND ", created!()),
        (TaskOrder::DoneNewest, _) => list_sql!(
            "status = 'done' AND ",
            " AND (?9 IS NULL
                    OR (done_at, id) < (SELECT done_at, id FROM tasks WHERE id = ?9))
                 ORDER BY done_at DESC, id DESC
                 LIMIT ?10"
        ),
    }
}

/// The statement `count_tasks` runs for `filter` (same `status = 'done'` literal rule).
#[doc(hidden)]
pub fn count_tasks_sql(filter: &TaskFilter) -> &'static str {
    if filter.status == Some(TaskStatus::Done) {
        concat!(
            "SELECT COUNT(*) FROM tasks WHERE status = 'done' AND ",
            task_where!()
        )
    } else {
        concat!("SELECT COUNT(*) FROM tasks WHERE ", task_where!())
    }
}

/// `store::eligible` as SQL over the `tasks` row in scope, for picking a candidate in
/// `claim_next_task` (the claim itself decides with `store::eligible`). Binds, in order:
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
    let now = ts::enc(req.now);

    let Some(row) = sqlx::query(concat!("SELECT ", task_cols!(), " FROM tasks WHERE id = ?"))
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
    let extra: Vec<ProjectId> =
        sqlx::query_scalar::<_, String>("SELECT project_id FROM task_projects WHERE task_id = ?")
            .bind(&id)
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(|p| uuid_of(p))
            .collect::<StoreResult<_>>()?;
    if !eligible(&previous, &extra, c) {
        return Ok(ClaimAttempt {
            previous: Some(previous),
            eligible: false,
            claimed: None,
        });
    }

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
                lease_epoch = lease_epoch + 1, version = version + 1, updated_at = ?, ",
        clear_block!(),
        "
          WHERE id = ?
            AND (status = 'open'
                 OR (status IN ('claimed', 'planning') AND lease_expires_at < ?)
                 OR (status IN ('claimed', 'planning', 'blocked', 'review') AND owner_agent_id = ?
                     AND (owner_session_id = ?
                          OR NOT EXISTS (SELECT 1 FROM sessions s
                                          WHERE s.id = tasks.owner_session_id
                                            AND s.last_heartbeat_at >= ?))))
          RETURNING ",
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
        eligible: true,
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
        options: options_from(row.try_get("options")?)?,
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
              subject, body, importance, reply_to, created_at, kind, options)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(options_to(&m.options))
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

/// Whether `task` may wait on `target` (see `BlockWrite`): the target exists and is not
/// closed, is neither the task nor one of its ancestors, and the chain of tasks the target
/// waits on never reaches `task`.
async fn wait_allowed(
    conn: &mut SqliteConnection,
    task: TaskId,
    target: TaskId,
) -> StoreResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = ?1
                                              AND status NOT IN ('done', 'cancelled'))
            AND ?1 <> ?2
            AND ?1 NOT IN (WITH RECURSIVE anc(id) AS (
                               SELECT parent_id FROM tasks WHERE id = ?2 AND parent_id IS NOT NULL
                               UNION
                               SELECT t.parent_id FROM tasks t JOIN anc ON t.id = anc.id
                                WHERE t.parent_id IS NOT NULL)
                           SELECT id FROM anc)
            AND ?2 NOT IN (WITH RECURSIVE chain(id) AS (
                               SELECT ?1
                               UNION
                               SELECT t.waiting_task_id FROM tasks t JOIN chain ON t.id = chain.id
                                WHERE t.status = 'blocked' AND t.waiting_kind = 'task'
                                  AND t.waiting_task_id IS NOT NULL)
                           SELECT id FROM chain)",
    )
    .bind(target.to_string())
    .bind(task.to_string())
    .fetch_one(&mut *conn)
    .await?)
}

/// `done` just became done: applies `rule` to every blocked task waiting on it.
async fn unblock_waiting(
    conn: &mut SqliteConnection,
    done: &Task,
    req: &TransitionRequest,
    rule: UnblockRule,
) -> StoreResult<Vec<Unblocked>> {
    let waiting: Vec<Task> = sqlx::query(concat!(
        "SELECT ",
        task_cols!(),
        " FROM tasks
         WHERE status = 'blocked' AND waiting_kind = 'task' AND waiting_task_id = ?
         ORDER BY created_at, id"
    ))
    .bind(done.id.to_string())
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(task_from)
    .collect::<StoreResult<_>>()?;
    let now = ts::enc(req.now);
    let mut out = Vec::with_capacity(waiting.len());
    for dep in waiting {
        let live: bool = match dep.owner_session_id {
            Some(s) => {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM sessions
                                     WHERE id = ? AND last_heartbeat_at >= ?)",
                )
                .bind(s.to_string())
                .bind(ts::enc(rule.live_since))
                .fetch_one(&mut *conn)
                .await?
            }
            None => false,
        };
        let row = if live {
            sqlx::query(concat!(
                "UPDATE tasks SET status = 'claimed', lease_expires_at = ?, ",
                clear_block!(),
                ", version = version + 1, updated_at = ?
                 WHERE id = ? AND status = 'blocked' RETURNING ",
                task_cols!()
            ))
            .bind(ts::enc(req.now + dep.lease_ttl.unwrap_or(rule.default_ttl)))
            .bind(&now)
            .bind(dep.id.to_string())
            .fetch_optional(&mut *conn)
            .await?
        } else {
            sqlx::query(concat!(
                "UPDATE tasks SET status = 'open', owner_session_id = NULL, owner_agent_id = NULL,
                        lease_expires_at = NULL, plan_state = 'none', ",
                clear_block!(),
                ", version = version + 1, updated_at = ?
                 WHERE id = ? AND status = 'blocked' RETURNING ",
                task_cols!()
            ))
            .bind(&now)
            .bind(dep.id.to_string())
            .fetch_optional(&mut *conn)
            .await?
        };
        let Some(row) = row else { continue };
        let task = task_from(&row)?;
        let (event, note) = unblocked_records(&task, done, !live, req.event.actor, req.now);
        record_event(conn, &task, &event, req.token).await?;
        insert_message(conn, &note, req.token).await?;
        out.push(Unblocked { task, note });
    }
    Ok(out)
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
        "s.id, s.agent_id, s.token_id, s.started_at, s.last_heartbeat_at, s.last_tool_call_at,
         s.turn_state, s.turn_state_at, s.wake_channel, s.wake_task_id, s.wake_until,
         s.last_woken_at, s.run_id"
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
/// caller's write transaction. Only when it returns a row is the token inserted and linked
/// (the label comes from that row), so a lost race creates nothing.
async fn consume_row(
    tx: &mut SqliteConnection,
    kind: &str,
    from_status: &str,
    secret_hash: &str,
    host: Option<&str>,
    now: OffsetDateTime,
    token: &NewToken<'_>,
) -> StoreResult<Option<Enrollment>> {
    let now = ts::enc(now);
    let Some(row) = sqlx::query(concat!(
        "UPDATE enrollments SET status = 'consumed', host = COALESCE(?, host)
         WHERE secret_hash = ? AND kind = ? AND status = ? AND expires_at > ?
         RETURNING ",
        enrollment_cols!()
    ))
    .bind(host)
    .bind(secret_hash)
    .bind(kind)
    .bind(from_status)
    .bind(&now)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let mut e = enrollment_from(&row)?;
    sqlx::query("INSERT INTO tokens (id, label, hash, created_at) VALUES (?, ?, ?, ?)")
        .bind(token.id.to_string())
        .bind(enrollment_token_label(&e))
        .bind(token.hash)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE enrollments SET token_id = ? WHERE id = ?")
        .bind(token.id.to_string())
        .bind(e.id.to_string())
        .execute(&mut *tx)
        .await?;
    e.token_id = Some(token.id);
    Ok(Some(e))
}

fn enrollment_from(row: &SqliteRow) -> StoreResult<Enrollment> {
    let approved: Option<&str> = row.try_get("approved_at")?;
    Ok(Enrollment {
        id: uuid_of(row.try_get("id")?)?,
        kind: parse_text(row.try_get("kind")?)?,
        label: row.try_get("label")?,
        host: row.try_get("host")?,
        status: parse_text(row.try_get("status")?)?,
        token_id: opt_uuid(row, "token_id")?,
        created_at: ts::dec(row.try_get("created_at")?)?,
        expires_at: ts::dec(row.try_get("expires_at")?)?,
        approved_at: approved.map(ts::dec).transpose()?,
        requester_ip: row.try_get("requester_ip")?,
    })
}

/// The guarded state change behind `Store::transition_task`, inside the caller's
/// transaction (which commits it, also when the guard did not match).
async fn transition_in(
    conn: &mut SqliteConnection,
    req: &TransitionRequest,
) -> StoreResult<Option<Transitioned>> {
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
    let block = req.block.clone().unwrap_or_default();
    let set_waiting = block.waiting_on.is_some();
    let waiting = block.waiting_on.as_ref();
    if let Some(target) = waiting.and_then(|w| w.task())
        && !wait_allowed(conn, req.task, target).await?
    {
        return Ok(None);
    }
    // The values an update replaces, read under the write lock.
    let mut event = req.event.clone();
    if block.record_previous {
        let previous = sqlx::query(concat!("SELECT ", task_cols!(), " FROM tasks WHERE id = ?"))
            .bind(req.task.to_string())
            .fetch_optional(&mut *conn)
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
    let to = req.to.map(|s| s.as_str());
    let updated = sqlx::query(concat!(
        "UPDATE tasks
            SET status = COALESCE(?, status),
                plan_state = COALESCE(?, plan_state),
                sanctioned = COALESCE(?, sanctioned),
                lease_expires_at = CASE WHEN ? THEN lease_expires_at ELSE ? END,
                owner_session_id = CASE WHEN ? THEN NULL ELSE owner_session_id END,
                owner_agent_id = CASE WHEN ? THEN NULL ELSE owner_agent_id END,
                done_at = CASE WHEN COALESCE(?, status) = 'done' AND status <> 'done' THEN ?
                               WHEN COALESCE(?, status) = 'done' THEN done_at
                               ELSE NULL END,
                acked_at = CASE WHEN COALESCE(?, status) = 'done' THEN acked_at
                                ELSE NULL END,
                block_reason = CASE WHEN COALESCE(?, status) <> 'blocked' THEN NULL
                                    ELSE COALESCE(?, block_reason) END,
                blocked_at = CASE WHEN COALESCE(?, status) <> 'blocked' THEN NULL
                                  WHEN status <> 'blocked' THEN ?
                                  ELSE blocked_at END,
                waiting_kind = CASE WHEN COALESCE(?, status) <> 'blocked' THEN NULL
                                    WHEN ? THEN ? ELSE waiting_kind END,
                waiting_task_id = CASE WHEN COALESCE(?, status) <> 'blocked' THEN NULL
                                       WHEN ? THEN ? ELSE waiting_task_id END,
                waiting_text = CASE WHEN COALESCE(?, status) <> 'blocked' THEN NULL
                                    WHEN ? THEN ? ELSE waiting_text END,
                unblock_text = CASE WHEN COALESCE(?, status) <> 'blocked' THEN NULL
                                    ELSE COALESCE(?, unblock_text) END,
                version = version + 1, updated_at = ?
          WHERE id = ?
            AND instr(?, ',' || status || ',') > 0
            AND (? IS NULL OR instr(?, ',' || plan_state || ',') > 0)
            AND (? IS NULL OR owner_session_id = ?)
            AND (? IS NULL
                 OR ? = (SELECT p.id FROM messages p
                          WHERE p.task_id = tasks.id AND p.kind = 'plan'
                          ORDER BY p.seq DESC LIMIT 1))
            AND (? IS NULL OR version = ?)
          RETURNING ",
        task_cols!()
    ))
    .bind(req.to.map(|s| s.as_str()))
    .bind(req.plan_to.map(|s| s.as_str()))
    .bind(req.sanctioned)
    .bind(keep_lease)
    .bind(lease)
    .bind(req.clear_owner)
    .bind(req.clear_owner)
    .bind(req.to.map(|s| s.as_str()))
    .bind(ts::enc(req.now))
    .bind(req.to.map(|s| s.as_str()))
    .bind(req.to.map(|s| s.as_str()))
    .bind(to)
    .bind(&block.reason)
    .bind(to)
    .bind(ts::enc(req.now))
    .bind(to)
    .bind(set_waiting)
    .bind(waiting.map(|w| w.kind()))
    .bind(to)
    .bind(set_waiting)
    .bind(waiting.and_then(|w| w.task()).map(|t| t.to_string()))
    .bind(to)
    .bind(set_waiting)
    .bind(waiting.and_then(|w| w.text()))
    .bind(to)
    .bind(&block.unblock)
    .bind(ts::enc(req.now))
    .bind(req.task.to_string())
    .bind(from)
    .bind(&plan_from)
    .bind(&plan_from)
    .bind(&owner)
    .bind(&owner)
    .bind(req.latest_plan.map(|m| m.to_string()))
    .bind(req.latest_plan.map(|m| m.to_string()))
    .bind(req.version)
    .bind(req.version)
    .fetch_optional(&mut *conn)
    .await?
    .as_ref()
    .map(task_from)
    .transpose()?;
    let Some(task) = updated else {
        return Ok(None);
    };
    let children_complete = record_event(conn, &task, &event, req.token).await?;
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
        .execute(&mut *conn)
        .await?;
    }
    if let Some(message) = &req.message {
        insert_message(conn, message, req.token).await?;
    }
    let unblocked = match req.unblock {
        Some(rule) if req.to == Some(TaskStatus::Done) && task.status == TaskStatus::Done => {
            unblock_waiting(conn, &task, req, rule).await?
        }
        _ => Vec::new(),
    };
    Ok(Some(Transitioned {
        task,
        children_complete,
        unblocked,
    }))
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

    async fn insert_enrollment(&self, e: &NewEnrollment<'_>) -> StoreResult<()> {
        let now = ts::enc(e.now);
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        sqlx::query(
            "UPDATE enrollments SET status = 'expired'
             WHERE status IN ('pending', 'approved') AND expires_at <= ?",
        )
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM enrollments WHERE expires_at < ?")
            .bind(ts::enc(e.now - TimeDuration::days(1)))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO enrollments (id, kind, secret_hash, user_code_hash, label, host, status,
                                      created_at, expires_at, requester_ip)
             VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)",
        )
        .bind(e.id.to_string())
        .bind(e.kind.as_str())
        .bind(e.secret_hash)
        .bind(e.user_code_hash)
        .bind(e.label)
        .bind(e.host)
        .bind(&now)
        .bind(ts::enc(e.expires_at))
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
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
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
             WHERE user_code_hash = ? AND kind = 'device' AND status = 'pending'
               AND expires_at > ?"
        ))
        .bind(user_code_hash)
        .bind(ts::enc(now))
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
        let now = ts::enc(now);
        let (status, approved_at) = if approve {
            ("approved", Some(now.clone()))
        } else {
            ("denied", None)
        };
        sqlx::query(concat!(
            "UPDATE enrollments SET status = ?, approved_at = ?
             WHERE id = ? AND kind = 'device' AND status = 'pending' AND expires_at > ?
             RETURNING ",
            enrollment_cols!()
        ))
        .bind(status)
        .bind(approved_at)
        .bind(id.to_string())
        .bind(&now)
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
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        if let Some(e) =
            consume_row(&mut tx, "device", "approved", secret_hash, None, now, token).await?
        {
            tx.commit().await?;
            return Ok(PollOutcome::Consumed(e));
        }
        let row = sqlx::query(concat!(
            "SELECT ",
            enrollment_cols!(),
            " FROM enrollments WHERE secret_hash = ? AND kind = 'device'"
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
        let found: Option<String> = sqlx::query_scalar(
            "UPDATE enrollments SET status = 'denied'
             WHERE id = ? AND status = 'pending' AND expires_at > ?
             RETURNING id",
        )
        .bind(id.to_string())
        .bind(ts::enc(now))
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn list_pending_enrollments(&self, now: OffsetDateTime) -> StoreResult<Vec<Enrollment>> {
        sqlx::query(concat!(
            "SELECT ",
            enrollment_cols!(),
            " FROM enrollments
             WHERE status = 'pending' AND expires_at > ?
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(ts::enc(now))
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
        cover: Option<SessionId>,
        token: Option<TokenId>,
        event: &TaskEvent,
        first_message: &Message,
    ) -> StoreResult<TaskInsert> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let mut sanctioned = task.sanctioned;
        if let Some(parent) = task.parent_id {
            // The parent's status and the covering rule, read under the write lock.
            let row: Option<(String, Option<bool>)> = sqlx::query_as(
                "SELECT status,
                        sanctioned = 1 AND (requires_plan = 0 OR plan_state = 'approved')
                          AND owner_session_id = ?2
                          AND status IN ('planning', 'claimed', 'blocked', 'review')
                   FROM tasks WHERE id = ?1",
            )
            .bind(parent.to_string())
            .bind(cover.map(|s| s.to_string()))
            .fetch_optional(&mut *tx)
            .await?;
            if row.as_ref().is_some_and(|(status, _)| status == "planning") {
                tx.commit().await?;
                return Ok(TaskInsert::ParentPlanning);
            }
            if cover.is_some() {
                sanctioned &= row.and_then(|(_, covered)| covered).unwrap_or(false);
            }
        } else if cover.is_some() {
            sanctioned = false;
        }
        let task = Task {
            sanctioned,
            ..task.clone()
        };
        write_task_rows(&mut tx, &task, extra_projects, token, event, first_message).await?;
        tx.commit().await?;
        Ok(TaskInsert::Inserted { sanctioned })
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

    async fn relate_tasks(&self, req: &RelateRequest<'_>) -> StoreResult<Related> {
        let (agent, session) = match req.editor {
            Some((a, s)) => (Some(a.to_string()), Some(s.to_string())),
            None => (None, None),
        };
        let id = req.task.to_string();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        // The guard, as a read under the write lock (a no-op change writes nothing).
        let guarded = sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks
             WHERE id = ?1
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
            return Ok(Related::Denied);
        };
        if req.cancel.is_some() && task.status == TaskStatus::Done {
            return Ok(Related::DuplicateOfDone);
        }
        const UNLINK: &str = "DELETE FROM task_links
             WHERE (task_id = ?1 AND target_id = ?2) OR (task_id = ?2 AND target_id = ?1)";
        let mut removed = Vec::new();
        for other in req.remove {
            let gone = sqlx::query(UNLINK)
                .bind(&id)
                .bind(other.to_string())
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if gone > 0 {
                removed.push(*other);
            }
        }
        let mut added = Vec::new();
        for (target, kind) in req.add {
            let target_s = target.to_string();
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM tasks WHERE id = ?")
                .bind(&target_s)
                .fetch_optional(&mut *tx)
                .await?;
            if exists.is_none() {
                return Ok(Related::UnknownTarget(*target));
            }
            let same: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM task_links WHERE task_id = ? AND target_id = ? AND kind = ?",
            )
            .bind(&id)
            .bind(&target_s)
            .bind(kind.as_str())
            .fetch_optional(&mut *tx)
            .await?;
            if same.is_some() {
                continue;
            }
            sqlx::query(UNLINK)
                .bind(&id)
                .bind(&target_s)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO task_links (task_id, target_id, kind, created_at) VALUES (?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&target_s)
            .bind(kind.as_str())
            .bind(ts::enc(req.event.at))
            .execute(&mut *tx)
            .await?;
            added.push(TaskLink {
                task_id: req.task,
                target_id: *target,
                kind: *kind,
                created_at: req.event.at,
            });
        }
        if added.is_empty() && removed.is_empty() && req.cancel.is_none() {
            tx.commit().await?;
            return Ok(Related::Applied {
                task,
                added,
                removed,
                cancelled: None,
            });
        }
        let touched = std::iter::once(req.task).chain(added.iter().map(|l| l.target_id));
        for t in touched {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM task_links WHERE task_id = ?1 OR target_id = ?1",
            )
            .bind(t.to_string())
            .fetch_one(&mut *tx)
            .await?;
            if n > i64::from(req.max_links) {
                return Ok(Related::TooMany(t));
            }
        }
        // A re-added duplicate link changes no link, only the cancellation below.
        if !added.is_empty() || !removed.is_empty() {
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
            event.payload = Some(related_payload(&added, &removed));
            insert_event(&mut tx, &event, req.token).await?;
        }
        let cancelled = match &req.cancel {
            Some(cancel) if task.status != TaskStatus::Cancelled => {
                let done = transition_in(&mut tx, cancel).await?.ok_or_else(|| {
                    StoreError::Corrupt(format!("task {} did not cancel under its lock", req.task))
                })?;
                task = done.task.clone();
                Some(done)
            }
            _ => None,
        };
        tx.commit().await?;
        Ok(Related::Applied {
            task,
            added,
            removed,
            cancelled,
        })
    }

    async fn move_task(&self, req: &MoveRequest) -> StoreResult<Moved> {
        let (agent, session) = match req.editor {
            Some((a, s)) => (Some(a.to_string()), Some(s.to_string())),
            None => (None, None),
        };
        let id = req.task.to_string();
        let parent = req.parent.to_string();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        // The whole guard as one read under the write lock; the ancestor walk rules out
        // cycles (the task is top-level, so it can only be the parent or an ancestor of it).
        let allowed: Option<i64> = sqlx::query(
            "WITH RECURSIVE ancestors(id, parent_id) AS (
                 SELECT id, parent_id FROM tasks WHERE id = ?4
                 UNION
                 SELECT t.id, t.parent_id FROM tasks t JOIN ancestors a ON t.id = a.parent_id
             )
             SELECT 1 AS ok FROM tasks t, tasks p
             WHERE t.id = ?1 AND p.id = ?4 AND t.parent_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM task_projects x WHERE x.task_id = t.id)
               AND (?2 IS NULL OR t.created_by_agent_id = ?2
                    OR (t.owner_session_id = ?3
                        AND t.status IN ('planning', 'claimed', 'blocked', 'review')))
               AND p.status NOT IN ('planning', 'done', 'cancelled')
               AND (p.project_id = t.project_id
                    OR EXISTS (SELECT 1 FROM task_projects x
                               WHERE x.task_id = p.id AND x.project_id = t.project_id))
               AND NOT EXISTS (SELECT 1 FROM ancestors a WHERE a.id = t.id)",
        )
        .bind(&id)
        .bind(&agent)
        .bind(&session)
        .bind(&parent)
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.try_get("ok"))
        .transpose()?;
        if allowed.is_none() {
            tx.commit().await?;
            return Ok(Moved::Refused);
        }
        // The new ancestors of the moved subtree are the parent and its ancestors: no task in
        // the subtree may be waiting on one of them.
        let waiting: Option<String> = sqlx::query_scalar(
            "WITH RECURSIVE
                 up(id, parent_id) AS (
                     SELECT id, parent_id FROM tasks WHERE id = ?2
                     UNION
                     SELECT t.id, t.parent_id FROM tasks t JOIN up ON t.id = up.parent_id),
                 down(id) AS (
                     SELECT id FROM tasks WHERE id = ?1
                     UNION
                     SELECT t.id FROM tasks t JOIN down ON t.parent_id = down.id)
             SELECT CAST(w.id AS TEXT) FROM down d JOIN tasks w ON w.id = d.id
              WHERE w.status = 'blocked' AND w.waiting_kind = 'task'
                AND w.waiting_task_id IN (SELECT id FROM up)
              ORDER BY w.created_at, w.id LIMIT 1",
        )
        .bind(req.task.to_string())
        .bind(&parent)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(w) = waiting {
            tx.commit().await?;
            let w: uuid::Uuid = w
                .parse()
                .map_err(|e| StoreError::Corrupt(format!("task id {w:?}: {e}")))?;
            return Ok(Moved::WaitOnAncestor(w.into()));
        }
        let task = sqlx::query(concat!(
            "UPDATE tasks SET parent_id = ?, version = version + 1, updated_at = ? WHERE id = ?
             RETURNING ",
            task_cols!()
        ))
        .bind(&parent)
        .bind(ts::enc(req.event.at))
        .bind(&id)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::from)
        .and_then(|row| task_from(&row))?;
        insert_event(&mut tx, &req.event, req.token).await?;
        tx.commit().await?;
        Ok(Moved::Moved(task))
    }

    async fn task_links(&self, task: TaskId) -> StoreResult<Vec<TaskLink>> {
        sqlx::query(
            "SELECT task_id, target_id, kind, created_at FROM task_links
             WHERE task_id = ?1 OR target_id = ?1 ORDER BY created_at, task_id, target_id",
        )
        .bind(task.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| {
            Ok(TaskLink {
                task_id: uuid_of(&row.try_get::<String, _>("task_id")?)?,
                target_id: uuid_of(&row.try_get::<String, _>("target_id")?)?,
                kind: row
                    .try_get::<String, _>("kind")?
                    .parse()
                    .map_err(StoreError::Corrupt)?,
                created_at: ts::dec(&row.try_get::<String, _>("created_at")?)?,
            })
        })
        .collect()
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

    async fn wake_settings(&self, project: ProjectId) -> StoreResult<ProjectWakeSettings> {
        let row = sqlx::query(
            "SELECT auto_start, headless_enabled, headless_harness, max_concurrent_runs,
                    daily_run_cap, max_run_minutes, max_budget_usd
               FROM project_wake_settings WHERE project_id = ?",
        )
        .bind(project.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(ProjectWakeSettings::defaults(project));
        };
        Ok(ProjectWakeSettings {
            project_id: project,
            auto_start: row.try_get("auto_start")?,
            headless_enabled: row.try_get("headless_enabled")?,
            headless_harness: row.try_get("headless_harness")?,
            max_concurrent_runs: to_u32(row.try_get("max_concurrent_runs")?)?,
            daily_run_cap: to_u32(row.try_get("daily_run_cap")?)?,
            max_run_minutes: to_u32(row.try_get("max_run_minutes")?)?,
            max_budget_usd: row.try_get("max_budget_usd")?,
        })
    }

    async fn put_wake_settings(
        &self,
        s: &ProjectWakeSettings,
        now: OffsetDateTime,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO project_wake_settings
                 (project_id, auto_start, headless_enabled, headless_harness,
                  max_concurrent_runs, daily_run_cap, max_run_minutes, max_budget_usd,
                  updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (project_id) DO UPDATE SET
                 auto_start = excluded.auto_start,
                 headless_enabled = excluded.headless_enabled,
                 headless_harness = excluded.headless_harness,
                 max_concurrent_runs = excluded.max_concurrent_runs,
                 daily_run_cap = excluded.daily_run_cap,
                 max_run_minutes = excluded.max_run_minutes,
                 max_budget_usd = excluded.max_budget_usd,
                 updated_at = excluded.updated_at",
        )
        .bind(s.project_id.to_string())
        .bind(s.auto_start)
        .bind(s.headless_enabled)
        .bind(&s.headless_harness)
        .bind(i64::from(s.max_concurrent_runs))
        .bind(i64::from(s.daily_run_cap))
        .bind(i64::from(s.max_run_minutes))
        .bind(s.max_budget_usd)
        .bind(ts::enc(now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn server_settings(&self) -> StoreResult<ServerSettings> {
        let row =
            sqlx::query("SELECT wake_paused, headless_paused FROM server_settings WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(ServerSettings::default());
        };
        Ok(ServerSettings {
            wake_paused: row.try_get("wake_paused")?,
            headless_paused: row.try_get("headless_paused")?,
        })
    }

    async fn put_server_settings(
        &self,
        s: &ServerSettings,
        now: OffsetDateTime,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO server_settings (id, wake_paused, headless_paused, updated_at)
             VALUES (1, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 wake_paused = excluded.wake_paused,
                 headless_paused = excluded.headless_paused,
                 updated_at = excluded.updated_at",
        )
        .bind(s.wake_paused)
        .bind(s.headless_paused)
        .bind(ts::enc(now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn insert_wake_attempt(&self, a: &WakeAttempt) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO wake_attempts
                 (id, kind, trigger, task_id, session_id, from_agent_id, runner_id, run_id,
                  outcome, detail, pr_url, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(a.id.to_string())
        .bind(a.kind.as_str())
        .bind(a.trigger.as_str())
        .bind(a.task_id.map(|t| t.to_string()))
        .bind(a.session_id.map(|s| s.to_string()))
        .bind(a.from_agent_id.map(|g| g.to_string()))
        .bind(a.runner_id.map(|r| r.to_string()))
        .bind(a.run_id.map(|r| r.to_string()))
        .bind(a.outcome.as_str())
        .bind(&a.detail)
        .bind(&a.pr_url)
        .bind(ts::enc(a.at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn last_wake_sent(&self, session: SessionId) -> StoreResult<Option<WakeAttempt>> {
        sqlx::query(
            "SELECT id, kind, trigger, task_id, session_id, from_agent_id, runner_id, run_id,
                    outcome, detail, pr_url, created_at
               FROM wake_attempts
              WHERE session_id = ? AND kind = 'wake' AND outcome = 'sent'
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(session.to_string())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(attempt_from)
        .transpose()
    }

    async fn count_wake_attempts(&self, q: &AttemptQuery) -> StoreResult<u32> {
        let triggers = q.triggers.as_ref().map(|list| {
            format!(
                ",{},",
                list.iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        });
        let session = q.session.map(|s| s.to_string());
        let task = q.task.map(|t| t.to_string());
        let from_agent = q.from_agent.map(|a| a.to_string());
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM wake_attempts
              WHERE kind = ? AND outcome = ? AND created_at >= ?
                AND (? IS NULL OR session_id = ?)
                AND (? IS NULL OR task_id = ?)
                AND (? IS NULL OR from_agent_id = ?)
                AND (? IS NULL OR instr(?, ',' || trigger || ',') > 0)",
        )
        .bind(q.kind.as_str())
        .bind(q.outcome.as_str())
        .bind(ts::enc(q.since))
        .bind(&session)
        .bind(&session)
        .bind(&task)
        .bind(&task)
        .bind(&from_agent)
        .bind(&from_agent)
        .bind(&triggers)
        .bind(&triggers)
        .fetch_one(&self.pool)
        .await?;
        to_u32(n)
    }

    async fn find_request(
        &self,
        from: AgentId,
        project: ProjectId,
        dedupe_key: &str,
        since: OffsetDateTime,
    ) -> StoreResult<Option<Task>> {
        find_request_in(&self.pool, from, project, dedupe_key, since).await
    }

    async fn count_requests_by(&self, from: AgentId, since: OffsetDateTime) -> StoreResult<u32> {
        count_requests_in(&self.pool, from, since).await
    }

    async fn oldest_request_by(
        &self,
        from: AgentId,
        since: OffsetDateTime,
    ) -> StoreResult<Option<OffsetDateTime>> {
        oldest_request_in(&self.pool, from, since).await
    }

    async fn count_pending_requests(&self, project: ProjectId) -> StoreResult<u32> {
        count_pending_in(&self.pool, project).await
    }

    async fn insert_request(
        &self,
        task: &Task,
        token: Option<TokenId>,
        event: &TaskEvent,
        first_message: &Message,
        admit: &RequestAdmission,
    ) -> StoreResult<RequestInsert> {
        let from = task.created_by.ids().0.ok_or_else(|| {
            StoreError::Corrupt(format!("request {} has no creating agent", task.id))
        })?;
        let key = task
            .request_dedupe_key
            .as_deref()
            .ok_or_else(|| StoreError::Corrupt(format!("request {} has no dedupe key", task.id)))?;
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        // BEGIN IMMEDIATE takes the write lock first, so concurrent requests see one another.
        if let Some(existing) =
            find_request_in(&mut *tx, from, task.project_id, key, admit.dedupe_since).await?
        {
            tx.commit().await?;
            return Ok(RequestInsert::Existing(Box::new(existing)));
        }
        if count_requests_in(&mut *tx, from, admit.rate_since).await? >= admit.per_hour {
            let oldest = oldest_request_in(&mut *tx, from, admit.rate_since)
                .await?
                .unwrap_or(task.created_at);
            tx.commit().await?;
            return Ok(RequestInsert::TooMany { oldest });
        }
        if count_pending_in(&mut *tx, task.project_id).await? >= admit.pending_cap {
            tx.commit().await?;
            return Ok(RequestInsert::ProjectFull);
        }
        if let Some(parent) = task.parent_id {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
                    .bind(parent.to_string())
                    .fetch_optional(&mut *tx)
                    .await?;
            if status.as_deref() == Some("planning") {
                tx.commit().await?;
                return Ok(RequestInsert::ParentPlanning);
            }
        }
        write_task_rows(&mut tx, task, &[], token, event, first_message).await?;
        tx.commit().await?;
        Ok(RequestInsert::Inserted)
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

    async fn report_turn_at(
        &self,
        session: SessionId,
        state: Option<TurnState>,
        channel: Option<WakeChannel>,
        reported_at: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> StoreResult<Option<SessionId>> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let row = sqlx::query("SELECT wake_task_id, wake_until FROM sessions WHERE id = ?")
            .bind(session.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound("session"))?;
        let task: Option<&str> = row.try_get("wake_task_id")?;
        let until: Option<&str> = row.try_get("wake_until")?;
        if let Some(channel) = channel {
            sqlx::query("UPDATE sessions SET wake_channel = ? WHERE id = ?")
                .bind(channel.as_str())
                .bind(session.to_string())
                .execute(&mut *tx)
                .await?;
        }
        let mut cleared = false;
        if let Some(state) = state {
            let busy = state == TurnState::Busy && until.is_some() && task.is_none();
            let reported_at = reported_at.map(ts::enc);
            // The guard is the rule: a report older than the stored one changes nothing.
            let applied = sqlx::query(
                "UPDATE sessions
                    SET turn_state_at = CASE WHEN ? <> turn_state THEN ? ELSE turn_state_at END,
                        turn_state = ?,
                        turn_reported_at = COALESCE(?, turn_reported_at),
                        wake_until = CASE WHEN ? THEN NULL ELSE wake_until END
                  WHERE id = ?
                    AND (? IS NULL OR turn_reported_at IS NULL OR turn_reported_at <= ?)",
            )
            .bind(state.as_str())
            .bind(ts::enc(now))
            .bind(state.as_str())
            .bind(&reported_at)
            .bind(busy)
            .bind(session.to_string())
            .bind(&reported_at)
            .bind(&reported_at)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
            cleared = busy && applied;
        }
        tx.commit().await?;
        Ok(cleared.then_some(session))
    }

    async fn reserve_session(&self, r: &Reservation) -> StoreResult<bool> {
        // One statement: SQLite takes the write lock before it evaluates the guard, so
        // concurrent reservers are serialised and exactly one sees the session free. The
        // same write lock orders it against claims and cancels: a new job's task must still
        // be routable by `list_queued`'s rule (sanctioned; open, or a lapsed claim; no other
        // live reservation), so a stale snapshot cannot reserve for a taken task.
        let task = r.task.map(|t| t.to_string());
        let done = sqlx::query(
            "UPDATE sessions
                SET wake_task_id = ?, wake_until = ?, last_woken_at = ?
              WHERE id = ?
                AND wake_channel <> 'none'
                AND turn_state = 'idle' AND turn_state_at <= ?
                AND last_heartbeat_at >= ?
                AND (wake_until IS NULL OR wake_until <= ?)
                AND (NOT ? OR NOT EXISTS (
                        SELECT 1 FROM tasks t
                         WHERE t.owner_session_id = sessions.id
                           AND t.status IN ('planning', 'claimed', 'blocked', 'review')))
                AND (NOT ? OR ? IS NULL OR EXISTS (
                        SELECT 1 FROM tasks t
                         WHERE t.id = ?
                           AND t.sanctioned = 1
                           AND (t.status = 'open'
                                OR (t.status IN ('claimed', 'planning')
                                    AND t.lease_expires_at <= ?))
                           AND NOT EXISTS (SELECT 1 FROM sessions o
                                            WHERE o.wake_task_id = t.id AND o.wake_until > ?
                                              AND o.id <> sessions.id)))",
        )
        .bind(&task)
        .bind(ts::enc(r.until))
        .bind(ts::enc(r.now))
        .bind(r.session.to_string())
        .bind(ts::enc(r.idle_before))
        .bind(ts::enc(r.live_since))
        .bind(ts::enc(r.now))
        .bind(r.new_job)
        .bind(r.new_job)
        .bind(&task)
        .bind(&task)
        .bind(ts::enc(r.now))
        .bind(ts::enc(r.now))
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn clear_session_reservation(
        &self,
        session: SessionId,
        task: Option<TaskId>,
    ) -> StoreResult<bool> {
        let done = sqlx::query(
            "UPDATE sessions SET wake_task_id = NULL, wake_until = NULL
              WHERE id = ? AND wake_until IS NOT NULL
                AND (wake_task_id IS NULL OR wake_task_id = ?)",
        )
        .bind(session.to_string())
        .bind(task.map(|t| t.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    async fn clear_task_reservations(&self, task: TaskId) -> StoreResult<Vec<SessionId>> {
        let mut ids = sqlx::query_scalar::<_, String>(
            "UPDATE sessions SET wake_task_id = NULL, wake_until = NULL
              WHERE wake_task_id = ? RETURNING id",
        )
        .bind(task.to_string())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|s| uuid_of(s))
        .collect::<StoreResult<Vec<SessionId>>>()?;
        ids.sort_by_key(|s| s.as_uuid());
        Ok(ids)
    }

    async fn expire_reservations(
        &self,
        now: OffsetDateTime,
    ) -> StoreResult<Vec<(SessionId, Option<TaskId>)>> {
        let now = ts::enc(now);
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let expired =
            sqlx::query("SELECT id, wake_task_id FROM sessions WHERE wake_until <= ? ORDER BY id")
                .bind(&now)
                .fetch_all(&mut *tx)
                .await?
                .iter()
                .map(|row| Ok((uuid_of(row.try_get("id")?)?, opt_uuid(row, "wake_task_id")?)))
                .collect::<StoreResult<Vec<_>>>()?;
        sqlx::query(
            "UPDATE sessions SET wake_task_id = NULL, wake_until = NULL WHERE wake_until <= ?",
        )
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(expired)
    }

    async fn wake_candidates(
        &self,
        live_since: OffsetDateTime,
        idle_before: OffsetDateTime,
        now: OffsetDateTime,
    ) -> StoreResult<Vec<(Agent, Session)>> {
        let sql = concat!(
            "SELECT ",
            session_cols!(),
            ", a.id AS a_id, a.project_id AS a_project_id, a.host AS a_host,
                    a.harness AS a_harness, a.created_at AS a_created_at
             FROM sessions s JOIN agents a ON a.id = s.agent_id
             WHERE s.last_heartbeat_at >= ?
               AND s.turn_state = 'idle' AND s.turn_state_at <= ?
               AND s.wake_channel <> 'none'
               AND (s.wake_until IS NULL OR s.wake_until <= ?)
             ORDER BY s.last_woken_at NULLS FIRST, s.turn_state_at, s.id"
        );
        sqlx::query(sql)
            .bind(ts::enc(live_since))
            .bind(ts::enc(idle_before))
            .bind(ts::enc(now))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|row| Ok((agent_from(row, "a_")?, session_from(row)?)))
            .collect()
    }

    async fn list_queued(&self, now: OffsetDateTime) -> StoreResult<Vec<Task>> {
        let now = ts::enc(now);
        sqlx::query(concat!(
            "SELECT ",
            task_cols!(),
            " FROM tasks
               WHERE sanctioned = 1
                 AND (status = 'open'
                      OR (status IN ('claimed', 'planning') AND lease_expires_at <= ?))
                 AND NOT EXISTS (SELECT 1 FROM sessions s
                                  WHERE s.wake_task_id = tasks.id AND s.wake_until > ?)
               ORDER BY COALESCE((SELECT MAX(e.created_at) FROM task_events e
                                   WHERE e.task_id = tasks.id AND e.kind = 'queued'),
                                 tasks.lease_expires_at, tasks.created_at),
                        tasks.id"
        ))
        .bind(&now)
        .bind(&now)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(task_from)
        .collect()
    }

    async fn list_tasks(&self, filter: &TaskFilter) -> StoreResult<Vec<Task>> {
        let (mode, since) = filter.done.parts();
        let sql = list_tasks_sql(filter);
        sqlx::query(sql)
            .bind(filter.project.map(|p| p.to_string()))
            .bind(filter.status.map(|s| s.as_str()))
            .bind(filter.owner.map(|a| a.to_string()))
            .bind(filter.parent.map(|p| p.to_string()))
            .bind(filter.top_level)
            .bind(mode)
            .bind(since.map(ts::enc))
            .bind(filter.title.as_deref().map(like_pattern))
            .bind(filter.after.map(|t| t.to_string()))
            .bind(filter.limit.map_or(-1, i64::from))
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
        let listed = serde_json::to_string(&ids.iter().map(|i| i.to_string()).collect::<Vec<_>>())
            .expect("strings serialize");
        let rows: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE sub(id) AS (
                 SELECT id FROM tasks
                  WHERE id IN (SELECT value FROM json_each(?1)) AND status = 'done'
                 UNION
                 SELECT t.id FROM tasks t JOIN sub ON t.parent_id = sub.id)
             UPDATE tasks SET acked_at = ?2
              WHERE id IN (SELECT id FROM sub) AND status = 'done' AND acked_at IS NULL
              RETURNING id",
        )
        .bind(listed)
        .bind(ts::enc(now))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|s| uuid_of(s)).collect()
    }

    async fn count_tasks(&self, filter: &TaskFilter) -> StoreResult<u64> {
        let (mode, since) = filter.done.parts();
        let sql = count_tasks_sql(filter);
        let n: i64 = sqlx::query_scalar(sql)
            .bind(filter.project.map(|p| p.to_string()))
            .bind(filter.status.map(|s| s.as_str()))
            .bind(filter.owner.map(|a| a.to_string()))
            .bind(filter.parent.map(|p| p.to_string()))
            .bind(filter.top_level)
            .bind(mode)
            .bind(since.map(ts::enc))
            .bind(filter.title.as_deref().map(like_pattern))
            .fetch_one(&self.pool)
            .await?;
        Ok(n as u64)
    }

    async fn append_task_event(&self, event: &TaskEvent) -> StoreResult<()> {
        let mut conn = self.pool.acquire().await?;
        insert_event(&mut conn, event, None).await
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
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        let done = transition_in(&mut tx, req).await?;
        tx.commit().await?;
        Ok(done)
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
              AND (?7 IS NULL
                   OR (CASE m.importance WHEN 'urgent' THEN 2 WHEN 'high' THEN 1 ELSE 0 END)
                      >= ?7)
             ORDER BY m.seq LIMIT ?6"
        ))
        .bind(&agent)
        .bind(&session)
        .bind(filter.project.map(|p| p.to_string()))
        .bind(filter.task.map(|t| t.to_string()))
        .bind(filter.importance.map(|i| i.as_str()))
        .bind(filter.limit.map_or(-1, i64::from))
        .bind(filter.min_importance.map(|i| i.rank()))
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
             WHERE (m.importance IN ('high', 'urgent') OR m.options IS NOT NULL)
               AND m.kind <> 'plan'
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

    async fn urgent_attention_tasks(&self) -> StoreResult<Vec<TaskId>> {
        sqlx::query(concat!(
            "SELECT DISTINCT m.task_id FROM messages m
             WHERE m.importance = 'urgent' AND m.kind <> 'plan' AND m.task_id IS NOT NULL
               AND (EXISTS (SELECT 1 FROM message_recipients r
                            WHERE r.message_id = m.id AND r.recipient_agent_id IS NULL
                              AND r.recipient_session_id IS NULL AND r.acked_at IS NULL)
                    OR (",
            pending_messages!(),
            "))
             ORDER BY m.task_id"
        ))
        .bind(Option::<String>::None)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| uuid_of(row.try_get("task_id")?))
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

    async fn insert_usage(
        &self,
        batch: &UsageBatch<'_>,
        pricer: &dyn Pricer,
    ) -> StoreResult<UsageWritten> {
        let mut written = UsageWritten::default();
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        for r in batch.records {
            let id = UsageId::new();
            let priced = pricer.price(&r.model, r.speed.as_deref(), &r.counts);
            let row = sqlx::query(
                "INSERT INTO usage_records (id, harness, dedupe_key, session_id, agent_id,
                     project_id, token_id, model, input_tokens, output_tokens,
                     cache_write_5m_tokens, cache_write_1h_tokens, cache_read_tokens,
                     reasoning_tokens, speed, billing, cost_micros, price_ref, parser,
                     harness_session_id, occurred_at, received_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (harness, dedupe_key) DO UPDATE SET
                     input_tokens = MAX(usage_records.input_tokens, excluded.input_tokens),
                     output_tokens = MAX(usage_records.output_tokens, excluded.output_tokens),
                     cache_write_5m_tokens = MAX(usage_records.cache_write_5m_tokens,
                                                 excluded.cache_write_5m_tokens),
                     cache_write_1h_tokens = MAX(usage_records.cache_write_1h_tokens,
                                                 excluded.cache_write_1h_tokens),
                     cache_read_tokens = MAX(usage_records.cache_read_tokens,
                                             excluded.cache_read_tokens),
                     reasoning_tokens = MAX(usage_records.reasoning_tokens,
                                            excluded.reasoning_tokens)
                 WHERE usage_records.token_id = excluded.token_id
                 RETURNING id, model, speed, input_tokens, output_tokens, cache_write_5m_tokens,
                           cache_write_1h_tokens, cache_read_tokens, reasoning_tokens",
            )
            .bind(id.to_string())
            .bind(&batch.agent.harness)
            .bind(&r.key)
            .bind(batch.session.to_string())
            .bind(batch.agent.id.to_string())
            .bind(batch.agent.project_id.map(|p| p.to_string()))
            .bind(batch.token.to_string())
            .bind(&r.model)
            .bind(r.counts.input)
            .bind(r.counts.output)
            .bind(r.counts.cache_write_5m)
            .bind(r.counts.cache_write_1h)
            .bind(r.counts.cache_read)
            .bind(r.counts.reasoning)
            .bind(&r.speed)
            .bind(r.billing.as_str())
            .bind(priced.as_ref().map(|p| p.cost_micros))
            .bind(priced.as_ref().map(|p| p.price_ref.as_str()))
            .bind(batch.parser)
            .bind(batch.harness_session_id)
            .bind(ts::enc(r.occurred_at))
            .bind(ts::enc(batch.now))
            .fetch_optional(&mut *tx)
            .await?;
            // No row: another token already holds this key. Its counts are not ours to merge.
            let Some(row) = row else {
                written.duplicate += 1;
                continue;
            };
            let stored: UsageId = uuid_of(row.try_get("id")?)?;
            if stored == id {
                written.inserted += 1;
                let tasks = held_tasks(&mut tx, batch.session, r.occurred_at).await?;
                let parts = tasks.len().max(1) as i64;
                let tasks: Vec<Option<String>> = if tasks.is_empty() {
                    vec![None]
                } else {
                    tasks.into_iter().map(Some).collect()
                };
                for task in tasks {
                    sqlx::query(
                        "INSERT INTO usage_allocations (usage_id, task_id, parts) VALUES (?, ?, ?)",
                    )
                    .bind(id.to_string())
                    .bind(task)
                    .bind(parts)
                    .execute(&mut *tx)
                    .await?;
                }
            } else {
                written.duplicate += 1;
                let model: String = row.try_get("model")?;
                let speed: Option<String> = row.try_get("speed")?;
                let priced = pricer.price(&model, speed.as_deref(), &counts_from(&row)?);
                sqlx::query("UPDATE usage_records SET cost_micros = ?, price_ref = ? WHERE id = ?")
                    .bind(priced.as_ref().map(|p| p.cost_micros))
                    .bind(priced.as_ref().map(|p| p.price_ref.as_str()))
                    .bind(stored.to_string())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(written)
    }

    async fn get_usage(&self, harness: &str, key: &str) -> StoreResult<Option<StoredUsage>> {
        let Some(row) = sqlx::query(
            "SELECT id, harness, session_id, agent_id, project_id, model, input_tokens,
                    output_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
                    cache_read_tokens, reasoning_tokens, speed, billing, cost_micros, price_ref,
                    parser, harness_session_id, occurred_at, received_at
             FROM usage_records WHERE harness = ? AND dedupe_key = ?",
        )
        .bind(harness)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let id: &str = row.try_get("id")?;
        let allocations = sqlx::query(
            "SELECT task_id, parts FROM usage_allocations
             WHERE usage_id = ? ORDER BY COALESCE(task_id, '')",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|a| Ok((opt_uuid(a, "task_id")?, a.try_get("parts")?)))
        .collect::<StoreResult<Vec<_>>>()?;
        Ok(Some(StoredUsage {
            id: uuid_of(id)?,
            harness: row.try_get("harness")?,
            session: uuid_of(row.try_get("session_id")?)?,
            agent: uuid_of(row.try_get("agent_id")?)?,
            project: opt_uuid(&row, "project_id")?,
            model: row.try_get("model")?,
            counts: counts_from(&row)?,
            speed: row.try_get("speed")?,
            billing: parse_text(row.try_get("billing")?)?,
            cost_micros: row.try_get("cost_micros")?,
            price_ref: row.try_get("price_ref")?,
            parser: row.try_get("parser")?,
            harness_session_id: row.try_get("harness_session_id")?,
            occurred_at: ts::dec(row.try_get("occurred_at")?)?,
            received_at: ts::dec(row.try_get("received_at")?)?,
            allocations,
        }))
    }

    async fn usage_summary(
        &self,
        filter: &UsageFilter,
        group: UsageGroup,
    ) -> StoreResult<Vec<UsageRow>> {
        let (mode, task) = match filter.task {
            None => (0, None),
            Some(TaskScope::NoTask) => (1, None),
            Some(TaskScope::Task { id, subtasks }) => (if subtasks { 3 } else { 2 }, Some(id)),
        };
        sqlx::query(usage_summary_sql(group))
            .bind(filter.from.map(ts::enc))
            .bind(filter.to.map(ts::enc))
            .bind(filter.project.map(|p| p.to_string()))
            .bind(filter.agent.map(|a| a.to_string()))
            .bind(filter.harness.as_deref())
            .bind(filter.host.as_deref())
            .bind(filter.model.as_deref())
            .bind(filter.billing.map(|b| b.as_str()))
            .bind(mode)
            .bind(task.map(|t| t.to_string()))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| {
                Ok(UsageRow {
                    key: r.try_get("key")?,
                    requests: r.try_get("requests")?,
                    sessions: r.try_get("sessions")?,
                    input: r.try_get("input")?,
                    output: r.try_get("output")?,
                    cache_write: r.try_get("cache_write")?,
                    cache_read: r.try_get("cache_read")?,
                    cost_micros: r.try_get("cost_micros")?,
                    unpriced: r.try_get("unpriced")?,
                    estimated: r.try_get("estimated")?,
                    estimated_cost_micros: r.try_get("estimated_cost_micros")?,
                })
            })
            .collect()
    }

    async fn usage_to_reprice(
        &self,
        since: Option<OffsetDateTime>,
        unpriced_only: bool,
        after: Option<UsageId>,
        limit: u32,
    ) -> StoreResult<Vec<RepriceRow>> {
        sqlx::query(
            "SELECT id, model, speed, input_tokens, output_tokens, cache_write_5m_tokens,
                    cache_write_1h_tokens, cache_read_tokens, reasoning_tokens
             FROM usage_records
             WHERE (?1 IS NULL OR occurred_at >= ?1)
               AND (?2 = 0 OR cost_micros IS NULL)
               AND (?3 IS NULL OR id > ?3)
             ORDER BY id LIMIT ?4",
        )
        .bind(since.map(ts::enc))
        .bind(unpriced_only)
        .bind(after.map(|a| a.to_string()))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(RepriceRow {
                id: uuid_of(r.try_get("id")?)?,
                model: r.try_get("model")?,
                speed: r.try_get("speed")?,
                counts: counts_from(r)?,
            })
        })
        .collect()
    }

    async fn set_usage_costs(&self, costs: &[(UsageId, Option<Priced>)]) -> StoreResult<()> {
        let mut tx = self.pool.begin_with(BEGIN_WRITE).await?;
        for (id, priced) in costs {
            sqlx::query("UPDATE usage_records SET cost_micros = ?, price_ref = ? WHERE id = ?")
                .bind(priced.as_ref().map(|p| p.cost_micros))
                .bind(priced.as_ref().map(|p| p.price_ref.as_str()))
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// The aggregate statement for `group` (binds: from, to, project, agent, harness, host,
/// model, billing, task mode 0 none / 1 no task / 2 task / 3 task and subtasks, task id).
macro_rules! usage_sql {
    ($key:literal, $group_by:literal) => {
        concat!(
            "WITH RECURSIVE tree(id) AS (
                 SELECT ?10 UNION ALL SELECT t.id FROM tasks t JOIN tree ON t.parent_id = tree.id)
             SELECT ", $key, " AS key,
                    COUNT(DISTINCT r.id) AS requests,
                    COUNT(DISTINCT r.session_id) AS sessions,
                    COALESCE(SUM(r.input_tokens * 1.0 / a.parts), 0.0) AS input,
                    COALESCE(SUM(r.output_tokens * 1.0 / a.parts), 0.0) AS output,
                    COALESCE(SUM((r.cache_write_5m_tokens + r.cache_write_1h_tokens) * 1.0
                                 / a.parts), 0.0) AS cache_write,
                    COALESCE(SUM(r.cache_read_tokens * 1.0 / a.parts), 0.0) AS cache_read,
                    COALESCE(SUM(r.cost_micros * 1.0 / a.parts), 0.0) AS cost_micros,
                    COUNT(DISTINCT CASE WHEN r.cost_micros IS NULL THEN r.id END) AS unpriced,
                    COUNT(DISTINCT CASE WHEN r.billing <> 'api' THEN r.id END) AS estimated,
                    COALESCE(SUM(CASE WHEN r.billing <> 'api'
                                      THEN r.cost_micros * 1.0 / a.parts END), 0.0)
                        AS estimated_cost_micros
             FROM usage_records r
             JOIN usage_allocations a ON a.usage_id = r.id
             JOIN agents g ON g.id = r.agent_id
             WHERE (?1 IS NULL OR r.occurred_at >= ?1)
               AND (?2 IS NULL OR r.occurred_at < ?2)
               AND (?3 IS NULL OR r.project_id = ?3)
               AND (?4 IS NULL OR r.agent_id = ?4)
               AND (?5 IS NULL OR r.harness = ?5)
               AND (?6 IS NULL OR g.host = ?6)
               AND (?7 IS NULL OR r.model = ?7)
               AND (?8 IS NULL OR r.billing = ?8)
               AND (?9 = 0
                    OR (?9 = 1 AND a.task_id IS NULL)
                    OR (?9 = 2 AND a.task_id = ?10)
                    OR (?9 = 3 AND a.task_id IN (SELECT id FROM tree)))",
            $group_by
        )
    };
}

fn usage_summary_sql(group: UsageGroup) -> &'static str {
    match group {
        UsageGroup::Total => usage_sql!("NULL", ""),
        UsageGroup::Project => usage_sql!("r.project_id", " GROUP BY 1"),
        UsageGroup::Task => usage_sql!("a.task_id", " GROUP BY 1"),
        UsageGroup::Agent => usage_sql!("r.agent_id", " GROUP BY 1"),
        UsageGroup::Harness => usage_sql!("r.harness", " GROUP BY 1"),
        UsageGroup::Host => usage_sql!("g.host", " GROUP BY 1"),
        UsageGroup::Model => usage_sql!("r.model", " GROUP BY 1"),
        UsageGroup::Day => usage_sql!("substr(r.occurred_at, 1, 10)", " GROUP BY 1"),
        UsageGroup::Hour => usage_sql!("substr(r.occurred_at, 1, 13)", " GROUP BY 1"),
    }
}

fn counts_from(row: &SqliteRow) -> StoreResult<UsageCounts> {
    Ok(UsageCounts {
        input: row.try_get("input_tokens")?,
        output: row.try_get("output_tokens")?,
        cache_write_5m: row.try_get("cache_write_5m_tokens")?,
        cache_write_1h: row.try_get("cache_write_1h_tokens")?,
        cache_read: row.try_get("cache_read_tokens")?,
        reasoning: row.try_get("reasoning_tokens")?,
    })
}

/// The tasks (id text) `session` held at `at`: claimed by it (a `claimed`, which covers a
/// resume and a same-agent takeover, or a `lease_expired_takeover`) at or before `at`, with
/// no release, finish or claim by another session after that claim and up to `at`. A
/// session that re-claimed a task it still held has two qualifying claims, hence `DISTINCT`.
async fn held_tasks(
    conn: &mut SqliteConnection,
    session: SessionId,
    at: OffsetDateTime,
) -> StoreResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT e.task_id FROM task_events e
         WHERE e.kind IN ('claimed', 'lease_expired_takeover')
           AND e.actor_session_id = ?1 AND e.created_at <= ?2
           AND NOT EXISTS (SELECT 1 FROM task_events x
             WHERE x.task_id = e.task_id AND x.created_at > e.created_at
               AND x.created_at <= ?2
               AND (x.kind IN ('released', 'done', 'cancelled', 'force_released')
                    OR (x.kind IN ('claimed', 'lease_expired_takeover')
                        AND x.actor_session_id IS NOT ?1)))",
    )
    .bind(session.to_string())
    .bind(ts::enc(at))
    .fetch_all(conn)
    .await?)
}

/// Writes a new task's rows (the task, its extra projects, its first event and message)
/// inside the caller's transaction.
async fn write_task_rows(
    tx: &mut SqliteConnection,
    task: &Task,
    extra_projects: &[ProjectId],
    token: Option<TokenId>,
    event: &TaskEvent,
    first_message: &Message,
) -> StoreResult<()> {
    let (by_agent, by_session) = task.created_by.ids();
    sqlx::query(
        "INSERT INTO tasks
             (id, project_id, parent_id, title, body, status, owner_session_id,
              owner_agent_id, lease_expires_at, lease_epoch, lease_ttl_secs,
              eligible_harness, eligible_host, created_by_agent_id, created_by_session_id,
              token_id, created_at, updated_at, version, requires_plan, plan_state,
              sanctioned, request_depth, origin_task_id, request_chain,
              request_dedupe_key)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                 ?)",
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
    .bind(task.sanctioned)
    .bind(i64::from(task.request_depth))
    .bind(task.origin_task_id.map(|t| t.to_string()))
    .bind(request_chain_json(&task.request_chain))
    .bind(&task.request_dedupe_key)
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
    insert_event(&mut *tx, event, token).await?;
    insert_message(&mut *tx, first_message, token).await?;
    Ok(())
}

async fn find_request_in<'e>(
    ex: impl sqlx::SqliteExecutor<'e>,
    from: AgentId,
    project: ProjectId,
    dedupe_key: &str,
    since: OffsetDateTime,
) -> StoreResult<Option<Task>> {
    sqlx::query(concat!(
        "SELECT ",
        task_cols!(),
        " FROM tasks
           WHERE created_by_agent_id = ? AND project_id = ? AND request_dedupe_key = ?
             AND created_at >= ?
           ORDER BY created_at DESC, id DESC LIMIT 1"
    ))
    .bind(from.to_string())
    .bind(project.to_string())
    .bind(dedupe_key)
    .bind(ts::enc(since))
    .fetch_optional(ex)
    .await?
    .as_ref()
    .map(task_from)
    .transpose()
}

async fn count_requests_in<'e>(
    ex: impl sqlx::SqliteExecutor<'e>,
    from: AgentId,
    since: OffsetDateTime,
) -> StoreResult<u32> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks
          WHERE created_by_agent_id = ? AND request_dedupe_key IS NOT NULL
            AND created_at >= ?",
    )
    .bind(from.to_string())
    .bind(ts::enc(since))
    .fetch_one(ex)
    .await?;
    to_u32(n)
}

async fn oldest_request_in<'e>(
    ex: impl sqlx::SqliteExecutor<'e>,
    from: AgentId,
    since: OffsetDateTime,
) -> StoreResult<Option<OffsetDateTime>> {
    let oldest: Option<String> = sqlx::query_scalar(
        "SELECT MIN(created_at) FROM tasks
          WHERE created_by_agent_id = ? AND request_dedupe_key IS NOT NULL
            AND created_at >= ?",
    )
    .bind(from.to_string())
    .bind(ts::enc(since))
    .fetch_one(ex)
    .await?;
    oldest.as_deref().map(ts::dec).transpose()
}

async fn count_pending_in<'e>(
    ex: impl sqlx::SqliteExecutor<'e>,
    project: ProjectId,
) -> StoreResult<u32> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE project_id = ? AND status = 'requested'",
    )
    .bind(project.to_string())
    .fetch_one(ex)
    .await?;
    to_u32(n)
}
