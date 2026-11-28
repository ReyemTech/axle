-- Done column ack (docs/superpowers/specs/2026-11-27-done-ack-design.md), Postgres flavour.
-- Kept in step with migrations/sqlite/0004_done_ack.sql.
--
-- `done_at` is when a task last entered `done`; `acked_at` is when the human acknowledged it.
-- Both are cleared when a task leaves `done`. Existing done tasks are backfilled from
-- `updated_at` (the best record of when they finished) and stay unacked.

ALTER TABLE tasks ADD COLUMN done_at timestamptz;
ALTER TABLE tasks ADD COLUMN acked_at timestamptz;

UPDATE tasks SET done_at = updated_at WHERE status = 'done';

CREATE INDEX tasks_done ON tasks (done_at, id) WHERE status = 'done';
