-- coord schema, Postgres flavour. Kept in step with migrations/sqlite/0001_init.sql.

-- ---------------------------------------------------------------- identity

CREATE TABLE projects (
    id            uuid PRIMARY KEY,
    key           text NOT NULL UNIQUE,
    display_name  text NOT NULL,
    created_at    timestamptz NOT NULL
);

CREATE TABLE tokens (
    id            uuid PRIMARY KEY,
    label         text NOT NULL,
    hash          text NOT NULL UNIQUE,
    created_at    timestamptz NOT NULL,
    last_used_at  timestamptz,
    revoked_at    timestamptz
);

CREATE TABLE agents (
    id          uuid PRIMARY KEY,
    project_id  uuid REFERENCES projects(id),
    host        text NOT NULL,
    harness     text NOT NULL,
    created_at  timestamptz NOT NULL,
    -- NULL project_id is a real identity component: treat NULLs as equal.
    CONSTRAINT agents_identity UNIQUE NULLS NOT DISTINCT (project_id, host, harness)
);

CREATE TABLE sessions (
    id                 uuid PRIMARY KEY,
    agent_id           uuid NOT NULL REFERENCES agents(id),
    token_id           uuid NOT NULL REFERENCES tokens(id),
    started_at         timestamptz NOT NULL,
    last_heartbeat_at  timestamptz NOT NULL,
    last_tool_call_at  timestamptz NOT NULL
);
CREATE INDEX sessions_agent ON sessions (agent_id);

-- ---------------------------------------------------------------- tasks

CREATE TABLE tasks (
    id                     uuid PRIMARY KEY,
    project_id             uuid NOT NULL REFERENCES projects(id),
    parent_id              uuid REFERENCES tasks(id),
    title                  text NOT NULL,
    body                   text NOT NULL DEFAULT '',
    status                 text NOT NULL DEFAULT 'open'
                           CHECK (status IN ('open', 'claimed', 'blocked', 'review', 'done', 'cancelled')),
    owner_session_id       uuid REFERENCES sessions(id),
    owner_agent_id         uuid REFERENCES agents(id),
    lease_expires_at       timestamptz,
    lease_ttl_secs         bigint,            -- per-task override of the server TTL
    lease_epoch            bigint NOT NULL DEFAULT 0,
    eligible_harness       text,
    eligible_host          text,
    created_by_agent_id    uuid REFERENCES agents(id),     -- both NULL = human
    created_by_session_id  uuid REFERENCES sessions(id),
    token_id               uuid REFERENCES tokens(id),
    created_at             timestamptz NOT NULL,
    updated_at             timestamptz NOT NULL,
    version                bigint NOT NULL DEFAULT 0
);
CREATE INDEX tasks_project_status ON tasks (project_id, status, created_at);
CREATE INDEX tasks_parent ON tasks (parent_id);
CREATE INDEX tasks_owner_session ON tasks (owner_session_id);

-- Extra participating projects of a parent task.
CREATE TABLE task_projects (
    task_id     uuid NOT NULL REFERENCES tasks(id),
    project_id  uuid NOT NULL REFERENCES projects(id),
    PRIMARY KEY (task_id, project_id)
);

CREATE TABLE task_events (
    id                uuid PRIMARY KEY,
    task_id           uuid NOT NULL REFERENCES tasks(id),
    kind              text NOT NULL,
    actor_agent_id    uuid REFERENCES agents(id),          -- both NULL = human / system
    actor_session_id  uuid REFERENCES sessions(id),
    token_id          uuid REFERENCES tokens(id),
    note              text,
    payload           jsonb,
    created_at        timestamptz NOT NULL
);
CREATE INDEX task_events_task ON task_events (task_id, created_at);

-- ---------------------------------------------------------------- messages

-- `seq` orders messages and positions agent cursors (kept in step with SQLite's rowid).
CREATE TABLE messages (
    id               uuid PRIMARY KEY,
    seq              bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    project_id       uuid REFERENCES projects(id),
    from_agent_id    uuid REFERENCES agents(id),           -- both NULL = human
    from_session_id  uuid REFERENCES sessions(id),
    token_id         uuid REFERENCES tokens(id),
    to_kind          text NOT NULL
                     CHECK (to_kind IN ('project', 'agent', 'session', 'task', 'human')),
    to_id            uuid,                                 -- NULL only for 'human'
    task_id          uuid REFERENCES tasks(id),
    subject          text,
    body             text NOT NULL,
    importance       text NOT NULL DEFAULT 'normal'
                     CHECK (importance IN ('normal', 'high', 'urgent')),
    reply_to         uuid REFERENCES messages(id),
    archived_at      timestamptz,
    created_at       timestamptz NOT NULL,
    search           tsvector GENERATED ALWAYS AS
                     (to_tsvector('simple', coalesce(subject, '') || ' ' || body)) STORED
);
CREATE INDEX messages_to ON messages (to_kind, to_id, created_at);
CREATE INDEX messages_task ON messages (task_id, created_at);
CREATE INDEX messages_project ON messages (project_id, created_at);
CREATE INDEX messages_search ON messages USING gin (search);

-- Per-recipient delivery. Agent-, session- and human-addressed messages get their row at
-- send time; project- and task-addressed ones get an agent (or human) row when a recipient
-- first reads or acks its inbox. Agent rows (session NULL) are acked once per agent; session
-- rows (agent NULL) per session; both NULL = the human.
CREATE TABLE message_recipients (
    message_id            uuid NOT NULL REFERENCES messages(id),
    recipient_agent_id    uuid REFERENCES agents(id),
    recipient_session_id  uuid REFERENCES sessions(id),
    delivered_at          timestamptz,
    read_at               timestamptz,
    acked_at              timestamptz,
    acked_by_session_id   uuid REFERENCES sessions(id),
    CONSTRAINT message_recipients_unique
        UNIQUE NULLS NOT DISTINCT (message_id, recipient_agent_id, recipient_session_id)
);
CREATE INDEX message_recipients_agent ON message_recipients (recipient_agent_id, acked_at);
CREATE INDEX message_recipients_session ON message_recipients (recipient_session_id, acked_at);

-- Project-wide messages are resolved at read time from this per-agent cursor: an agent sees
-- messages to its home project with `seq > acked_seq`. Written with the agent, at the newest
-- message then, so an agent that first appears later starts at its creation.
CREATE TABLE agent_cursors (
    agent_id    uuid NOT NULL REFERENCES agents(id),
    project_id  uuid NOT NULL REFERENCES projects(id),
    acked_seq   bigint NOT NULL,
    updated_at  timestamptz NOT NULL,
    PRIMARY KEY (agent_id, project_id)
);

-- ---------------------------------------------------------------- events

CREATE TABLE outbox (
    seq           bigserial PRIMARY KEY,
    kind          text NOT NULL,
    project_id    uuid REFERENCES projects(id),
    payload       jsonb NOT NULL,
    created_at    timestamptz NOT NULL,
    published_at  timestamptz
);
CREATE INDEX outbox_unpublished ON outbox (published_at, seq);
