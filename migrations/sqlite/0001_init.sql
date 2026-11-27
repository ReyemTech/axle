-- coord schema, SQLite flavour. Kept in step with migrations/postgres/0001_init.sql.
-- Ids are hyphenated UUID text; timestamps are UTC RFC3339 text with exactly six
-- fractional digits (`2026-11-27T09:00:00.000000Z`) so they compare correctly as text.

-- ---------------------------------------------------------------- identity

CREATE TABLE projects (
    id            TEXT PRIMARY KEY,
    key           TEXT NOT NULL UNIQUE,
    display_name  TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE TABLE tokens (
    id            TEXT PRIMARY KEY,
    label         TEXT NOT NULL,
    hash          TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL,
    last_used_at  TEXT,
    revoked_at    TEXT
);

CREATE TABLE agents (
    id          TEXT PRIMARY KEY,
    project_id  TEXT REFERENCES projects(id),
    host        TEXT NOT NULL,
    harness     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
-- NULL project_id is a real identity component: treat NULLs as equal.
CREATE UNIQUE INDEX agents_identity ON agents (IFNULL(project_id, ''), host, harness);

CREATE TABLE sessions (
    id                 TEXT PRIMARY KEY,
    agent_id           TEXT NOT NULL REFERENCES agents(id),
    token_id           TEXT NOT NULL,
    started_at         TEXT NOT NULL,
    last_heartbeat_at  TEXT NOT NULL,
    last_tool_call_at  TEXT NOT NULL
);
CREATE INDEX sessions_agent ON sessions (agent_id);

-- ---------------------------------------------------------------- tasks

CREATE TABLE tasks (
    id                     TEXT PRIMARY KEY,
    project_id             TEXT NOT NULL REFERENCES projects(id),
    parent_id              TEXT REFERENCES tasks(id),
    title                  TEXT NOT NULL,
    body                   TEXT NOT NULL DEFAULT '',
    status                 TEXT NOT NULL DEFAULT 'open'
                           CHECK (status IN ('open', 'claimed', 'blocked', 'review', 'done', 'cancelled')),
    owner_session_id       TEXT REFERENCES sessions(id),
    owner_agent_id         TEXT REFERENCES agents(id),
    lease_expires_at       TEXT,
    lease_ttl_secs         INTEGER,           -- per-task override of the server TTL
    lease_epoch            INTEGER NOT NULL DEFAULT 0,
    eligible_harness       TEXT,
    eligible_host          TEXT,
    created_by_agent_id    TEXT REFERENCES agents(id),     -- both NULL = human
    created_by_session_id  TEXT REFERENCES sessions(id),
    token_id               TEXT,
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL,
    version                INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX tasks_project_status ON tasks (project_id, status, created_at);
CREATE INDEX tasks_parent ON tasks (parent_id);
CREATE INDEX tasks_owner_session ON tasks (owner_session_id);

-- Extra participating projects of a parent task.
CREATE TABLE task_projects (
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    project_id  TEXT NOT NULL REFERENCES projects(id),
    PRIMARY KEY (task_id, project_id)
);

CREATE TABLE task_events (
    id                TEXT PRIMARY KEY,
    task_id           TEXT NOT NULL REFERENCES tasks(id),
    kind              TEXT NOT NULL,
    actor_agent_id    TEXT REFERENCES agents(id),          -- both NULL = human / system
    actor_session_id  TEXT REFERENCES sessions(id),
    token_id          TEXT,
    note              TEXT,
    payload           TEXT,                                -- JSON
    created_at        TEXT NOT NULL
);
CREATE INDEX task_events_task ON task_events (task_id, created_at);

-- ---------------------------------------------------------------- messages

CREATE TABLE messages (
    id               TEXT PRIMARY KEY,
    project_id       TEXT REFERENCES projects(id),
    from_agent_id    TEXT REFERENCES agents(id),           -- both NULL = human
    from_session_id  TEXT REFERENCES sessions(id),
    token_id         TEXT,
    to_kind          TEXT NOT NULL
                     CHECK (to_kind IN ('project', 'agent', 'session', 'task', 'human')),
    to_id            TEXT,                                 -- NULL only for 'human'
    task_id          TEXT REFERENCES tasks(id),
    subject          TEXT,
    body             TEXT NOT NULL,
    importance       TEXT NOT NULL DEFAULT 'normal'
                     CHECK (importance IN ('normal', 'high', 'urgent')),
    reply_to         TEXT REFERENCES messages(id),
    archived_at      TEXT,
    created_at       TEXT NOT NULL
);
CREATE INDEX messages_to ON messages (to_kind, to_id, created_at);
CREATE INDEX messages_task ON messages (task_id, created_at);
CREATE INDEX messages_project ON messages (project_id, created_at);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject, body, content = 'messages', content_rowid = 'rowid'
);
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, subject, body) VALUES (new.rowid, new.subject, new.body);
END;
CREATE TRIGGER messages_fts_update AFTER UPDATE OF subject, body ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, subject, body)
        VALUES ('delete', old.rowid, old.subject, old.body);
    INSERT INTO messages_fts (rowid, subject, body) VALUES (new.rowid, new.subject, new.body);
END;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, subject, body)
        VALUES ('delete', old.rowid, old.subject, old.body);
END;

-- Per-recipient delivery for agent-, session- and human-addressed messages.
-- Agent rows are acked once per agent; session rows per session; both NULL = human.
CREATE TABLE message_recipients (
    message_id            TEXT NOT NULL REFERENCES messages(id),
    recipient_agent_id    TEXT REFERENCES agents(id),
    recipient_session_id  TEXT REFERENCES sessions(id),
    delivered_at          TEXT,
    read_at               TEXT,
    acked_at              TEXT,
    acked_by_session_id   TEXT REFERENCES sessions(id)
);
CREATE UNIQUE INDEX message_recipients_unique ON message_recipients
    (message_id, IFNULL(recipient_agent_id, ''), IFNULL(recipient_session_id, ''));
CREATE INDEX message_recipients_agent ON message_recipients (recipient_agent_id, acked_at);
CREATE INDEX message_recipients_session ON message_recipients (recipient_session_id, acked_at);

-- Project-wide messages are resolved at read time from this per-agent cursor.
CREATE TABLE agent_cursors (
    agent_id         TEXT NOT NULL REFERENCES agents(id),
    project_id       TEXT NOT NULL REFERENCES projects(id),
    acked_until      TEXT NOT NULL,       -- messages created at or before this are acked
    last_message_id  TEXT REFERENCES messages(id),
    updated_at       TEXT NOT NULL,
    PRIMARY KEY (agent_id, project_id)
);

-- ---------------------------------------------------------------- events

CREATE TABLE outbox (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT NOT NULL,
    project_id    TEXT REFERENCES projects(id),
    payload       TEXT NOT NULL,          -- JSON
    created_at    TEXT NOT NULL,
    published_at  TEXT
);
CREATE INDEX outbox_unpublished ON outbox (published_at, seq);
