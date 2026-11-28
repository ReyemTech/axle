-- Token usage (docs/superpowers/specs/2026-11-28-token-usage-design.md), SQLite flavour.
-- Kept in step with migrations/postgres/0008_token_usage.sql.
--
-- One row per model response, deduplicated by `(harness, dedupe_key)`. Usage is split across
-- the tasks its session held when it happened by `usage_allocations` rows (task NULL = "no
-- task"), written once at ingest.

CREATE TABLE usage_records (
    id TEXT PRIMARY KEY,
    harness TEXT NOT NULL,                -- from the session's agent, not the payload
    dedupe_key TEXT NOT NULL,             -- requestId / response_id / message id
    session_id TEXT NOT NULL REFERENCES sessions(id),
    agent_id TEXT NOT NULL REFERENCES agents(id),
    project_id TEXT REFERENCES projects(id),       -- the agent's home project
    token_id TEXT NOT NULL REFERENCES tokens(id),
    model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,        -- uncached input only
    output_tokens INTEGER NOT NULL,       -- includes reasoning
    cache_write_5m_tokens INTEGER NOT NULL,
    cache_write_1h_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    reasoning_tokens INTEGER NOT NULL,    -- informational, never priced twice
    speed TEXT,                           -- 'fast' when the log says so
    billing TEXT NOT NULL CHECK (billing IN ('subscription', 'api', 'unknown')),
    cost_micros INTEGER,                  -- API-equivalent micro-USD; NULL = no price
    price_ref TEXT,                       -- 'builtin:2026-11-28#claude-opus-5-5'
    parser TEXT NOT NULL,                 -- 'claude-code/1'
    harness_session_id TEXT,              -- opaque; lets approach B map later
    occurred_at TEXT NOT NULL,            -- from the log
    received_at TEXT NOT NULL
);
CREATE UNIQUE INDEX usage_dedupe ON usage_records (harness, dedupe_key);
CREATE INDEX usage_time ON usage_records (occurred_at);
CREATE INDEX usage_session ON usage_records (session_id, occurred_at);
CREATE INDEX usage_project_time ON usage_records (project_id, occurred_at);

CREATE TABLE usage_allocations (
    usage_id TEXT NOT NULL REFERENCES usage_records(id),
    task_id TEXT REFERENCES tasks(id),    -- NULL = "no task"
    parts INTEGER NOT NULL                -- the record splits into this many shares
);
CREATE UNIQUE INDEX usage_alloc_key ON usage_allocations (usage_id, COALESCE(task_id, ''));
CREATE INDEX usage_alloc_task ON usage_allocations (task_id);
