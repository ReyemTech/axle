-- no-transaction
-- Delivery stages (docs/superpowers/specs/2026-11-28-delivery-status-design.md), SQLite
-- flavour. Kept in step with migrations/postgres/0008_delivery.sql. It follows
-- 0006_blocked_detail and 0007_related_links.
--
-- SQLite cannot change a CHECK constraint in place, so `tasks` is rebuilt (the documented
-- create-copy-drop-rename procedure, as 0002 did). Foreign keys must be off while the old
-- table is dropped, and `PRAGMA foreign_keys` is a no-op inside a transaction, so this
-- migration runs outside sqlx's transaction and opens its own around the rebuild. The new
-- table carries every column of 0001-0007 (0006's block detail included) and every index
-- on `tasks`; `task_links`, `waiting_task_id` and the other references to `tasks(id)` name
-- the table, so they follow the rename.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

CREATE TABLE tasks_new (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id),
    parent_id              TEXT REFERENCES tasks(id),
    title                  TEXT NOT NULL,
    body                   TEXT NOT NULL DEFAULT '',
    status                 TEXT NOT NULL DEFAULT 'open'
                           CHECK (status IN ('open', 'planning', 'claimed', 'blocked', 'review',
                                             'delivering', 'done', 'cancelled')),
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
    requires_plan          INTEGER NOT NULL DEFAULT 0 CHECK (requires_plan IN (0, 1)),
    plan_state             TEXT NOT NULL DEFAULT 'none'
                           CHECK (plan_state IN ('none', 'drafting', 'submitted',
                                                 'changes_requested', 'approved')),
    done_at                TEXT,
    acked_at               TEXT,
    block_reason           TEXT,
    blocked_at             TEXT,
    waiting_kind           TEXT
                           CHECK (waiting_kind IS NULL OR waiting_kind IN ('human', 'task', 'external')),
    waiting_task_id        TEXT REFERENCES tasks(id),
    waiting_text           TEXT,
    unblock_text           TEXT,
    -- The furthest delivery stage reached; NULL until `in_pr` is reached.
    delivery_stage         TEXT
                           CHECK (delivery_stage IN ('in_pr', 'merged', 'released', 'deployed'))
);

INSERT INTO tasks_new
    (id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
     lease_expires_at, lease_ttl_secs, lease_epoch, eligible_harness, eligible_host,
     created_by_agent_id, created_by_session_id, token_id, created_at, updated_at, version,
     requires_plan, plan_state, done_at, acked_at, block_reason, blocked_at, waiting_kind,
     waiting_task_id, waiting_text, unblock_text)
SELECT id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
       lease_expires_at, lease_ttl_secs, lease_epoch, eligible_harness, eligible_host,
       created_by_agent_id, created_by_session_id, token_id, created_at, updated_at, version,
       requires_plan, plan_state, done_at, acked_at, block_reason, blocked_at, waiting_kind,
       waiting_task_id, waiting_text, unblock_text
  FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_status ON tasks (project_id, status, created_at);
CREATE INDEX tasks_parent ON tasks (parent_id);
CREATE INDEX tasks_owner_session ON tasks (owner_session_id);
CREATE INDEX tasks_done ON tasks (done_at, id) WHERE status = 'done';
CREATE INDEX tasks_waiting_task ON tasks (waiting_task_id) WHERE waiting_task_id IS NOT NULL;
CREATE INDEX tasks_delivery ON tasks (project_id, status, delivery_stage);

-- One row per project; no row means no stages (today's behaviour).
CREATE TABLE project_delivery (
    project_id           TEXT PRIMARY KEY REFERENCES projects(id),
    stages               TEXT NOT NULL DEFAULT '[]',   -- JSON array, a subset in stage order
    repo                 TEXT,                          -- derived `owner/name`
    github_enabled       INTEGER NOT NULL DEFAULT 1 CHECK (github_enabled IN (0, 1)),
    release_source       TEXT NOT NULL DEFAULT 'github_releases'
                         CHECK (release_source IN ('github_releases', 'github_tags', 'generic',
                                                   'none')),
    tag_pattern          TEXT,
    deploy_source        TEXT NOT NULL DEFAULT 'github_deployments'
                         CHECK (deploy_source IN ('github_deployments', 'generic', 'none')),
    required_environment TEXT NOT NULL DEFAULT 'production',
    updated_at           TEXT NOT NULL
);

-- The PR a task ships in (one per task). A non-GitHub PR is stored as a link only
-- (`number` NULL); unique indexes ignore NULLs.
CREATE TABLE task_prs (
    task_id          TEXT PRIMARY KEY REFERENCES tasks(id),
    project_id       TEXT NOT NULL REFERENCES projects(id),
    repo             TEXT NOT NULL,
    number           INTEGER,
    url              TEXT NOT NULL,
    linked_by        TEXT NOT NULL
                     CHECK (linked_by IN ('agent', 'human', 'branch_match', 'title_match',
                                          'complete_links')),
    head_ref         TEXT,
    base_ref         TEXT,
    head_sha         TEXT,
    title            TEXT,
    state            TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed', 'merged')),
    merged_at        TEXT,
    merge_commit_sha TEXT,
    ci_state         TEXT NOT NULL DEFAULT 'none'
                     CHECK (ci_state IN ('none', 'pending', 'passing', 'failing')),
    ci_failing       TEXT NOT NULL DEFAULT '[]',        -- JSON, up to 5 check names
    checked_at       TEXT
);
CREATE UNIQUE INDEX task_prs_repo_number ON task_prs (repo, number);
CREATE UNIQUE INDEX task_prs_url ON task_prs (url);
CREATE INDEX task_prs_project ON task_prs (project_id);

-- Append-only per stage; the furthest is on `tasks`.
CREATE TABLE task_stages (
    task_id    TEXT NOT NULL REFERENCES tasks(id),
    stage      TEXT NOT NULL CHECK (stage IN ('in_pr', 'merged', 'released', 'deployed')),
    reached_at TEXT NOT NULL,
    implied    INTEGER NOT NULL DEFAULT 0 CHECK (implied IN (0, 1)),
    signal_id  TEXT,
    detail     TEXT,                                    -- JSON: version, environment, url
    PRIMARY KEY (task_id, stage)
);

-- Append-only log of everything that tried to move a task.
CREATE TABLE delivery_signals (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    kind        TEXT NOT NULL CHECK (kind IN ('pr', 'ci', 'merged', 'released', 'deployed')),
    source      TEXT NOT NULL CHECK (source IN ('github', 'cli', 'webhook', 'human')),
    version     TEXT,
    sha         TEXT,
    environment TEXT,
    url         TEXT,
    delivery_id TEXT,
    outcome     TEXT NOT NULL CHECK (outcome IN ('applied', 'unmatched', 'duplicate', 'refused')),
    matched     INTEGER NOT NULL DEFAULT 0,
    received_at TEXT NOT NULL
);
CREATE UNIQUE INDEX delivery_signals_delivery
    ON delivery_signals (project_id, delivery_id) WHERE delivery_id IS NOT NULL;
CREATE INDEX delivery_signals_received ON delivery_signals (project_id, received_at);

-- Resolves a deploy that names a version; `task_ids` is the task list of a generic release.
CREATE TABLE delivery_versions (
    project_id  TEXT NOT NULL REFERENCES projects(id),
    version     TEXT NOT NULL,
    commit_sha  TEXT,
    url         TEXT,
    released_at TEXT NOT NULL,
    task_ids    TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (project_id, version)
);

CREATE TABLE delivery_hooks (
    project_id         TEXT PRIMARY KEY REFERENCES projects(id),
    hook_id            TEXT NOT NULL UNIQUE,
    secret_sealed      BLOB NOT NULL,
    secret_fingerprint TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    rotated_at         TEXT
);

CREATE TABLE github_cache (
    key        TEXT PRIMARY KEY,                        -- method + path + query
    etag       TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);

CREATE TABLE github_health (
    scope      TEXT PRIMARY KEY,                        -- `token` or a project id
    state      TEXT NOT NULL
               CHECK (state IN ('ok', 'missing', 'rejected', 'rate_limited', 'no_access',
                                'unreachable')),
    since      TEXT NOT NULL,
    until      TEXT,
    last_ok_at TEXT
);

COMMIT;

PRAGMA foreign_keys = ON;
