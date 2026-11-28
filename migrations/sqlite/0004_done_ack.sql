-- Done column ack (docs/superpowers/specs/2026-11-27-done-ack-design.md), SQLite flavour.
-- Kept in step with migrations/postgres/0004_done_ack.sql.
--
-- `done_at` is when a task last entered `done`; `acked_at` is when the human acknowledged it.
-- Both are cleared when a task leaves `done`. Existing done tasks are backfilled from
-- `updated_at` (the best record of when they finished). Rows that finished more than 7 days
-- ago (the old fixed board window) are acked too, so an upgrade does not present the whole
-- history as "auto-acked, unseen"; newer ones stay unacked. This one-off statement reads the
-- wall clock, which store queries never do.
-- The comparison value is padded to the stored 27-character, microsecond format.

ALTER TABLE tasks ADD COLUMN done_at TEXT;
ALTER TABLE tasks ADD COLUMN acked_at TEXT;

UPDATE tasks SET done_at = updated_at WHERE status = 'done';
UPDATE tasks SET acked_at = done_at WHERE status = 'done' AND done_at < strftime('%Y-%m-%dT%H:%M:%f', 'now', '-7 days') || '000Z';

CREATE INDEX tasks_done ON tasks (done_at, id) WHERE status = 'done';
