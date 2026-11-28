-- Wake idle agents (docs/superpowers/specs/2026-11-28-wake-up-design.md), Postgres flavour.
-- Kept in step with migrations/sqlite/0006_wake_up.sql.

ALTER TABLE tasks DROP CONSTRAINT tasks_status_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_status_check
    CHECK (status IN ('requested', 'open', 'planning', 'claimed', 'blocked', 'review', 'done',
                      'cancelled'));

-- The human (or a task they sanctioned) approved waking agents for this task.
ALTER TABLE tasks ADD COLUMN sanctioned boolean NOT NULL DEFAULT false;
-- Cross-project requests: how deep the chain is, where it started, the projects it has passed
-- through and the key that collapses duplicates.
ALTER TABLE tasks ADD COLUMN request_depth bigint NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN origin_task_id uuid REFERENCES tasks(id);
ALTER TABLE tasks ADD COLUMN request_chain jsonb NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE tasks ADD COLUMN request_dedupe_key text;
CREATE INDEX tasks_request_dedupe ON tasks (request_dedupe_key, created_at);

-- What a session reports about its turn, and how Axle can reach it.
ALTER TABLE sessions ADD COLUMN turn_state text NOT NULL DEFAULT 'unknown'
    CHECK (turn_state IN ('unknown', 'busy', 'idle'));
ALTER TABLE sessions ADD COLUMN turn_state_at timestamptz;
ALTER TABLE sessions ADD COLUMN wake_channel text NOT NULL DEFAULT 'none'
    CHECK (wake_channel IN ('none', 'claude-socket', 'claude-monitor', 'codex-app-server',
                            'opencode-api'));
ALTER TABLE sessions ADD COLUMN wake_task_id uuid;
ALTER TABLE sessions ADD COLUMN wake_until timestamptz;
ALTER TABLE sessions ADD COLUMN last_woken_at timestamptz;
ALTER TABLE sessions ADD COLUMN run_id text;

-- Per-project wake and headless-run settings; no row means the defaults.
CREATE TABLE project_wake_settings (
    project_id          uuid PRIMARY KEY REFERENCES projects(id),
    auto_start          boolean NOT NULL DEFAULT false,
    headless_enabled    boolean NOT NULL DEFAULT false,
    headless_harness    text,
    max_concurrent_runs bigint NOT NULL DEFAULT 1,
    daily_run_cap       bigint NOT NULL DEFAULT 5,
    max_run_minutes     bigint NOT NULL DEFAULT 30,
    max_budget_usd      double precision,
    updated_at          timestamptz NOT NULL
);

-- Machines that can start headless runs (used by the headless plan).
CREATE TABLE runners (
    id            uuid PRIMARY KEY,
    host          text NOT NULL,
    harnesses     jsonb NOT NULL DEFAULT '[]'::jsonb,
    projects      jsonb NOT NULL DEFAULT '[]'::jsonb,   -- project ids
    paused        boolean NOT NULL DEFAULT false,
    can_deliver   boolean NOT NULL DEFAULT true,
    last_seen_at  timestamptz,
    created_at    timestamptz NOT NULL
);

-- Append-only log of every wake and headless attempt; each change of outcome is a new row,
-- and the caps count `sent` rows. `outcome` is unchecked text so later plans can add values.
CREATE TABLE wake_attempts (
    id             uuid PRIMARY KEY,
    kind           text NOT NULL CHECK (kind IN ('wake', 'headless')),
    trigger        text NOT NULL,
    task_id        uuid REFERENCES tasks(id),
    session_id     uuid REFERENCES sessions(id),
    from_agent_id  uuid REFERENCES agents(id),
    runner_id      uuid REFERENCES runners(id),
    run_id         uuid,
    outcome        text NOT NULL,
    detail         text,
    pr_url         text,
    created_at     timestamptz NOT NULL
);
CREATE INDEX wake_attempts_session ON wake_attempts (session_id, created_at);
CREATE INDEX wake_attempts_task ON wake_attempts (task_id, created_at);
CREATE INDEX wake_attempts_from_agent ON wake_attempts (from_agent_id, created_at);
CREATE INDEX wake_attempts_run ON wake_attempts (run_id, created_at);

-- Server-wide switches (one row).
CREATE TABLE server_settings (
    id               integer PRIMARY KEY CHECK (id = 1),
    wake_paused      boolean NOT NULL DEFAULT false,
    headless_paused  boolean NOT NULL DEFAULT false,
    updated_at       timestamptz NOT NULL
);
