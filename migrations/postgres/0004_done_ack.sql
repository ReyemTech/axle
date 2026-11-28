-- Done column ack (docs/superpowers/specs/2026-11-27-done-ack-design.md), Postgres flavour.
-- Kept in step with migrations/sqlite/0004_done_ack.sql.
--
-- `done_at` is when a task last entered `done`; `acked_at` is when the human acknowledged it.
-- Both are cleared when a task leaves `done`. Existing done tasks are backfilled from
-- `updated_at` (the best record of when they finished). Rows that finished more than 7 days
-- ago (the old fixed board window) are acked too, so an upgrade does not present the whole
-- history as "auto-acked, unseen"; newer ones stay unacked. This one-off statement reads the
-- wall clock, which store queries never do.

ALTER TABLE tasks ADD COLUMN done_at timestamptz;
ALTER TABLE tasks ADD COLUMN acked_at timestamptz;

UPDATE tasks SET done_at = updated_at WHERE status = 'done';
UPDATE tasks SET acked_at = done_at WHERE status = 'done' AND done_at < now() - interval '7 days';

CREATE INDEX tasks_done ON tasks (done_at, id) WHERE status = 'done';
