-- Local projects and adopting a git remote
-- (docs/superpowers/specs/2026-11-28-local-projects-design.md), SQLite flavour.
-- Kept in step with migrations/postgres/0008_local_projects.sql.
--
-- A local project (`local/<host>/<folder>`) records the host and the bridge's hash of the
-- folder path, so the same folder always finds it and a same-named folder elsewhere never
-- does. `adopt_key` is the git key last seen for that folder once it gained a remote; the
-- human's "Move tasks and history" action then renames the project (or merges it into an
-- existing one, setting `merged_into`). `project_aliases` keeps every old key resolving.

ALTER TABLE projects ADD COLUMN local_host TEXT;
ALTER TABLE projects ADD COLUMN local_folder TEXT;
ALTER TABLE projects ADD COLUMN adopt_key TEXT;
ALTER TABLE projects ADD COLUMN merged_into TEXT REFERENCES projects(id);

-- An agent folded into the target's agent of the same host and harness during a merge:
-- its id keeps resolving to that agent. Its events, messages and receipts are never
-- rewritten; reads resolve them through this column.
ALTER TABLE agents ADD COLUMN merged_into TEXT REFERENCES agents(id);
CREATE INDEX agents_merged_into ON agents (merged_into) WHERE merged_into IS NOT NULL;

CREATE UNIQUE INDEX projects_local_folder ON projects (local_host, local_folder)
    WHERE local_folder IS NOT NULL;

CREATE TABLE project_aliases (
    id            TEXT PRIMARY KEY,
    key           TEXT NOT NULL,
    project_id    TEXT NOT NULL REFERENCES projects(id),
    local_host    TEXT,
    local_folder  TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX project_aliases_key ON project_aliases (key, created_at);
CREATE INDEX project_aliases_folder ON project_aliases (local_host, local_folder, created_at);
