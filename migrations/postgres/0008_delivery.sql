-- Delivery stages (docs/superpowers/specs/2026-11-28-delivery-status-design.md), Postgres
-- flavour. Kept in step with migrations/sqlite/0008_delivery.sql. It follows
-- 0006_blocked_detail and 0007_related_links.

ALTER TABLE tasks DROP CONSTRAINT tasks_status_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_status_check
    CHECK (status IN ('open', 'planning', 'claimed', 'blocked', 'review', 'delivering', 'done',
                      'cancelled'));

-- The furthest delivery stage reached; NULL until `in_pr` is reached.
ALTER TABLE tasks ADD COLUMN delivery_stage text
    CHECK (delivery_stage IN ('in_pr', 'merged', 'released', 'deployed'));
CREATE INDEX tasks_delivery ON tasks (project_id, status, delivery_stage);

-- One row per project; no row means no stages (today's behaviour).
CREATE TABLE project_delivery (
    project_id           uuid PRIMARY KEY REFERENCES projects(id),
    stages               jsonb NOT NULL DEFAULT '[]',   -- a subset in stage order
    repo                 text,                          -- derived `owner/name`
    github_enabled       boolean NOT NULL DEFAULT true,
    release_source       text NOT NULL DEFAULT 'github_releases'
                         CHECK (release_source IN ('github_releases', 'github_tags', 'generic',
                                                   'none')),
    tag_pattern          text,
    deploy_source        text NOT NULL DEFAULT 'github_deployments'
                         CHECK (deploy_source IN ('github_deployments', 'generic', 'none')),
    required_environment text NOT NULL DEFAULT 'production',
    updated_at           timestamptz NOT NULL
);

-- The PR a task ships in (one per task). A non-GitHub PR is stored as a link only
-- (`number` NULL); unique indexes ignore NULLs.
CREATE TABLE task_prs (
    task_id          uuid PRIMARY KEY REFERENCES tasks(id),
    project_id       uuid NOT NULL REFERENCES projects(id),
    repo             text NOT NULL,
    number           bigint,
    url              text NOT NULL,
    linked_by        text NOT NULL
                     CHECK (linked_by IN ('agent', 'human', 'branch_match', 'title_match',
                                          'complete_links')),
    head_ref         text,
    base_ref         text,
    head_sha         text,
    title            text,
    state            text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed', 'merged')),
    merged_at        timestamptz,
    merge_commit_sha text,
    ci_state         text NOT NULL DEFAULT 'none'
                     CHECK (ci_state IN ('none', 'pending', 'passing', 'failing')),
    ci_failing       jsonb NOT NULL DEFAULT '[]',       -- up to 5 check names
    checked_at       timestamptz
);
CREATE UNIQUE INDEX task_prs_repo_number ON task_prs (repo, number);
CREATE UNIQUE INDEX task_prs_url ON task_prs (url);
CREATE INDEX task_prs_project ON task_prs (project_id);

-- Append-only per stage; the furthest is on `tasks`.
CREATE TABLE task_stages (
    task_id    uuid NOT NULL REFERENCES tasks(id),
    stage      text NOT NULL CHECK (stage IN ('in_pr', 'merged', 'released', 'deployed')),
    reached_at timestamptz NOT NULL,
    implied    boolean NOT NULL DEFAULT false,
    signal_id  uuid,
    detail     jsonb,                                   -- version, environment, url
    PRIMARY KEY (task_id, stage)
);

-- Append-only log of everything that tried to move a task.
CREATE TABLE delivery_signals (
    id          uuid PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects(id),
    kind        text NOT NULL CHECK (kind IN ('pr', 'ci', 'merged', 'released', 'deployed')),
    source      text NOT NULL CHECK (source IN ('github', 'cli', 'webhook', 'human')),
    version     text,
    sha         text,
    environment text,
    url         text,
    delivery_id text,
    outcome     text NOT NULL CHECK (outcome IN ('applied', 'unmatched', 'duplicate', 'refused')),
    matched     integer NOT NULL DEFAULT 0,
    received_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX delivery_signals_delivery
    ON delivery_signals (project_id, delivery_id) WHERE delivery_id IS NOT NULL;
CREATE INDEX delivery_signals_received ON delivery_signals (project_id, received_at);

-- Resolves a deploy that names a version; `task_ids` is the task list of a generic release.
CREATE TABLE delivery_versions (
    project_id  uuid NOT NULL REFERENCES projects(id),
    version     text NOT NULL,
    commit_sha  text,
    url         text,
    released_at timestamptz NOT NULL,
    task_ids    jsonb NOT NULL DEFAULT '[]',
    PRIMARY KEY (project_id, version)
);

CREATE TABLE delivery_hooks (
    project_id         uuid PRIMARY KEY REFERENCES projects(id),
    hook_id            text NOT NULL UNIQUE,
    secret_sealed      bytea NOT NULL,
    secret_fingerprint text NOT NULL,
    created_at         timestamptz NOT NULL,
    rotated_at         timestamptz
);

CREATE TABLE github_cache (
    key        text PRIMARY KEY,                        -- method + path + query
    etag       text NOT NULL,
    fetched_at timestamptz NOT NULL
);

CREATE TABLE github_health (
    scope      text PRIMARY KEY,                        -- `token` or a project id
    state      text NOT NULL
               CHECK (state IN ('ok', 'missing', 'rejected', 'rate_limited', 'no_access',
                                'unreachable')),
    since      timestamptz NOT NULL,
    until      timestamptz,
    last_ok_at timestamptz
);
