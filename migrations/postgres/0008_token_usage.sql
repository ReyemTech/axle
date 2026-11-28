-- Token usage (docs/superpowers/specs/2026-11-28-token-usage-design.md), Postgres flavour.
-- Kept in step with migrations/sqlite/0008_token_usage.sql.
--
-- One row per model response, deduplicated by `(harness, dedupe_key)`. Usage is split across
-- the tasks its session held when it happened by `usage_allocations` rows (task NULL = "no
-- task"), written once at ingest.

CREATE TABLE usage_records (
    id uuid PRIMARY KEY,
    harness text NOT NULL,                -- from the session's agent, not the payload
    dedupe_key text NOT NULL,             -- requestId / response_id / message id
    session_id uuid NOT NULL REFERENCES sessions(id),
    agent_id uuid NOT NULL REFERENCES agents(id),
    project_id uuid REFERENCES projects(id),       -- the agent's home project
    token_id uuid NOT NULL REFERENCES tokens(id),
    model text NOT NULL,
    input_tokens bigint NOT NULL,         -- uncached input only
    output_tokens bigint NOT NULL,        -- includes reasoning
    cache_write_5m_tokens bigint NOT NULL,
    cache_write_1h_tokens bigint NOT NULL,
    cache_read_tokens bigint NOT NULL,
    reasoning_tokens bigint NOT NULL,     -- informational, never priced twice
    speed text,                           -- 'fast' when the log says so
    billing text NOT NULL CHECK (billing IN ('subscription', 'api', 'unknown')),
    cost_micros bigint,                   -- API-equivalent micro-USD; NULL = no price
    price_ref text,                       -- 'builtin:2026-11-28#claude-opus-5-5'
    parser text NOT NULL,                 -- 'claude-code/1'
    harness_session_id text,              -- opaque; lets approach B map later
    occurred_at timestamptz NOT NULL,     -- from the log
    received_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX usage_dedupe ON usage_records (harness, dedupe_key);
CREATE INDEX usage_time ON usage_records (occurred_at);
CREATE INDEX usage_session ON usage_records (session_id, occurred_at);
CREATE INDEX usage_project_time ON usage_records (project_id, occurred_at);

CREATE TABLE usage_allocations (
    usage_id uuid NOT NULL REFERENCES usage_records(id),
    task_id uuid REFERENCES tasks(id),    -- NULL = "no task"
    parts bigint NOT NULL                 -- the record splits into this many shares
);
CREATE UNIQUE INDEX usage_alloc_key ON usage_allocations (usage_id, COALESCE(task_id::text, ''));
CREATE INDEX usage_alloc_task ON usage_allocations (task_id);
