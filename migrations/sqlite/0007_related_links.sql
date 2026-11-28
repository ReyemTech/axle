-- Related links between tasks (docs/superpowers/specs/2026-11-28-related-links-design.md),
-- SQLite flavour. Kept in step with migrations/postgres/0007_related_links.sql.
--
-- A link is written from `task_id` (the task whose creator or owner added it) to `target_id`.
-- `relates` reads the same from both sides; `follow-up` and `duplicate` say what `task_id` is
-- to `target_id` ("follow-up of", "duplicate of"). One link per pair of tasks, whichever way
-- it points.
CREATE TABLE task_links (
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    target_id   TEXT NOT NULL REFERENCES tasks(id),
    kind        TEXT NOT NULL CHECK (kind IN ('relates', 'follow-up', 'duplicate')),
    created_at  TEXT NOT NULL,
    PRIMARY KEY (task_id, target_id),
    CHECK (task_id <> target_id)
);
CREATE INDEX task_links_target ON task_links (target_id);
CREATE UNIQUE INDEX task_links_pair ON task_links (min(task_id, target_id), max(task_id, target_id));
