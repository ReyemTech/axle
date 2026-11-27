-- no-transaction
-- Plan gate (docs/superpowers/specs/2026-11-27-plan-gate-and-projects-design.md), SQLite
-- flavour. Kept in step with migrations/postgres/0002_plan_gate.sql.
--
-- SQLite cannot change a CHECK constraint in place, so `tasks` is rebuilt (the documented
-- create-copy-drop-rename procedure). Foreign keys must be off while the old table is
-- dropped, and `PRAGMA foreign_keys` is a no-op inside a transaction, so this migration runs
-- outside sqlx's transaction and opens its own around the rebuild.

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
                                             'done', 'cancelled')),
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
                                                 'changes_requested', 'approved'))
);

INSERT INTO tasks_new
    (id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
     lease_expires_at, lease_ttl_secs, lease_epoch, eligible_harness, eligible_host,
     created_by_agent_id, created_by_session_id, token_id, created_at, updated_at, version)
SELECT id, project_id, parent_id, title, body, status, owner_session_id, owner_agent_id,
       lease_expires_at, lease_ttl_secs, lease_epoch, eligible_harness, eligible_host,
       created_by_agent_id, created_by_session_id, token_id, created_at, updated_at, version
  FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX tasks_project_status ON tasks (project_id, status, created_at);
CREATE INDEX tasks_parent ON tasks (parent_id);
CREATE INDEX tasks_owner_session ON tasks (owner_session_id);

-- What a message is: an ordinary post, a plan submitted for approval, or a note the server
-- wrote (approvals, added projects).
ALTER TABLE messages ADD COLUMN kind TEXT NOT NULL DEFAULT 'message'
    CHECK (kind IN ('message', 'plan', 'system'));
CREATE INDEX messages_task_kind ON messages (task_id, kind);

COMMIT;

PRAGMA foreign_keys = ON;
