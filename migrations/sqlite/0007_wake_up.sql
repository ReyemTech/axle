-- no-transaction
-- Wake idle agents (docs/superpowers/specs/2026-11-28-wake-up-design.md), SQLite flavour.
-- Kept in step with migrations/postgres/0007_wake_up.sql.
--
-- SQLite cannot change a CHECK constraint in place, so `tasks` is rebuilt (the same
-- create-copy-drop-rename procedure as 0002) to admit the `requested` status and gain the
-- request columns. Foreign keys must be off while the old table is dropped, and
-- `PRAGMA foreign_keys` is a no-op inside a transaction, so this migration runs outside
-- sqlx's transaction and opens its own around the rebuild. `done_at` / `acked_at` (0004) are
-- copied, and the `tasks_done` partial index is recreated.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

CREATE TABLE tasks_new (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id),
    parent_id              TEXT REFERENCES tasks(id),
    title                  TEXT NOT NULL,
    body                   TEXT NOT NULL DEFAULT '',
    status                 TEXT NOT NULL DEFAULT 'open'
                           CHECK (status IN ('requested', 'open', 'planning', 'claimed', 'blocked',
                                             'review', 'done', 'cancelled')),
    owner_session_id       TEXT REFERENCES sessions(id),
    owner_agent_id         TEXT REFERENCES agents(id),
    lease_expires_at       TEXT,
    lease_ttl_secs         INTEGER,           -- per-task override of the server TTL
    lease_epoch            INTEGER NOT NULL DEFAULT 0,
    eligible_harness       TEXT,
    eligible_host          TEXT,
    created_by_agent_id    TEXT REFERENCES agents(id),     -- both NULL = human
    created_by_session_id  TEXT REFERENCES sessions(id),
    token_id               TEXT REFERENCES tokens(id),
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL,
    version                INTEGER NOT NULL DEFAULT 0,
    -- The human approves a plan before the work starts (opt-in per task).
    requires_plan          INTEGER NOT NULL DEFAULT 0 CHECK (requires_plan IN (0, 1)),
    plan_state             TEXT NOT NULL DEFAULT 'none'
                           CHECK (plan_state IN ('none', 'drafting', 'submitted',
                                                 'changes_requested', 'approved')),
    done_at                TEXT,
    acked_at               TEXT,
    -- The human (or a task they sanctioned) approved waking agents for this task.
    sanctioned             INTEGER NOT NULL DEFAULT 0 CHECK (sanctioned IN (0, 1)),
    -- Cross-project requests: how deep the chain is, where it started, the projects it has
    -- passed through (JSON array of project ids) and the key that collapses duplicates.
    request_depth          INTEGER NOT NULL DEFAULT 0,
    origin_task_id         TEXT REFERENCES tasks(id),
    request_chain          TEXT NOT NULL DEFAULT '[]',
    request_dedupe_key     TEXT
);

INSERT INTO tasks_new
    (id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
     lease_expires_at, lease_ttl_secs, lease_epoch, eligible_harness, eligible_host,
     created_by_agent_id, created_by_session_id, token_id, created_at, updated_at, version,
     requires_plan, plan_state, done_at, acked_at)
SELECT id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
       lease_expires_at, lease_ttl_secs, lease_epoch, eligible_harness, eligible_host,
       created_by_agent_id, created_by_session_id, token_id, created_at, updated_at, version,
       requires_plan, plan_state, done_at, acked_at
  FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_status ON tasks (project_id, status, created_at);
CREATE INDEX tasks_parent ON tasks (parent_id);
CREATE INDEX tasks_owner_session ON tasks (owner_session_id);
CREATE INDEX tasks_done ON tasks (done_at, id) WHERE status = 'done';
CREATE INDEX tasks_request_dedupe ON tasks (request_dedupe_key, created_at);

-- What a session reports about its turn, and how Axle can reach it.
ALTER TABLE sessions ADD COLUMN turn_state TEXT NOT NULL DEFAULT 'unknown'
    CHECK (turn_state IN ('unknown', 'busy', 'idle'));
ALTER TABLE sessions ADD COLUMN turn_state_at TEXT;
ALTER TABLE sessions ADD COLUMN wake_channel TEXT NOT NULL DEFAULT 'none'
    CHECK (wake_channel IN ('none', 'claude-socket', 'claude-monitor', 'codex-app-server',
                            'opencode-api'));
ALTER TABLE sessions ADD COLUMN wake_task_id TEXT;
ALTER TABLE sessions ADD COLUMN wake_until TEXT;
ALTER TABLE sessions ADD COLUMN last_woken_at TEXT;
ALTER TABLE sessions ADD COLUMN run_id TEXT;

-- Per-project wake and headless-run settings; no row means the defaults.
CREATE TABLE project_wake_settings (
    project_id          TEXT PRIMARY KEY REFERENCES projects(id),
    auto_start          INTEGER NOT NULL DEFAULT 0 CHECK (auto_start IN (0, 1)),
    headless_enabled    INTEGER NOT NULL DEFAULT 0 CHECK (headless_enabled IN (0, 1)),
    headless_harness    TEXT,
    max_concurrent_runs INTEGER NOT NULL DEFAULT 1,
    daily_run_cap       INTEGER NOT NULL DEFAULT 5,
    max_run_minutes     INTEGER NOT NULL DEFAULT 30,
    max_budget_usd      REAL,
    updated_at          TEXT NOT NULL
);

-- Machines that can start headless runs (used by the headless plan).
CREATE TABLE runners (
    id            TEXT PRIMARY KEY,
    host          TEXT NOT NULL,
    harnesses     TEXT NOT NULL DEFAULT '[]',   -- JSON array
    projects      TEXT NOT NULL DEFAULT '[]',   -- JSON array of project ids
    paused        INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    can_deliver   INTEGER NOT NULL DEFAULT 1 CHECK (can_deliver IN (0, 1)),
    last_seen_at  TEXT,
    created_at    TEXT NOT NULL
);

-- Append-only log of every wake and headless attempt; each change of outcome is a new row,
-- and the caps count `sent` rows. `outcome` is unchecked text so later plans can add values.
CREATE TABLE wake_attempts (
    id             TEXT PRIMARY KEY,
    kind           TEXT NOT NULL CHECK (kind IN ('wake', 'headless')),
    trigger        TEXT NOT NULL,
    task_id        TEXT REFERENCES tasks(id),
    session_id     TEXT REFERENCES sessions(id),
    from_agent_id  TEXT REFERENCES agents(id),
    runner_id      TEXT REFERENCES runners(id),
    run_id         TEXT,
    outcome        TEXT NOT NULL,
    detail         TEXT,
    pr_url         TEXT,
    created_at     TEXT NOT NULL
);
CREATE INDEX wake_attempts_session ON wake_attempts (session_id, created_at);
CREATE INDEX wake_attempts_task ON wake_attempts (task_id, created_at);
CREATE INDEX wake_attempts_from_agent ON wake_attempts (from_agent_id, created_at);
CREATE INDEX wake_attempts_run ON wake_attempts (run_id, created_at);

-- Server-wide switches (one row).
CREATE TABLE server_settings (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    wake_paused      INTEGER NOT NULL DEFAULT 0 CHECK (wake_paused IN (0, 1)),
    headless_paused  INTEGER NOT NULL DEFAULT 0 CHECK (headless_paused IN (0, 1)),
    updated_at       TEXT NOT NULL
);

COMMIT;

PRAGMA foreign_keys = ON;
