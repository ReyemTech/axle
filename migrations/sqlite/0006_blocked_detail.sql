-- Blocked tasks say why and what unblocks them
-- (docs/superpowers/specs/2026-11-28-blocked-explained-design.md), SQLite flavour.
-- Kept in step with migrations/postgres/0006_blocked_detail.sql.
--
-- While a task is `blocked` these hold the reason, when it was blocked, what it waits on
-- (`human`, another `task`, or `external` free text) and what would unblock it. They are all
-- cleared whenever the task leaves `blocked`. History stays in task_events.
-- Tasks blocked before this migration get their reason and time from their latest
-- `blocked` event.

ALTER TABLE tasks ADD COLUMN block_reason TEXT;
ALTER TABLE tasks ADD COLUMN blocked_at TEXT;
ALTER TABLE tasks ADD COLUMN waiting_kind TEXT
    CHECK (waiting_kind IS NULL OR waiting_kind IN ('human', 'task', 'external'));
ALTER TABLE tasks ADD COLUMN waiting_task_id TEXT REFERENCES tasks(id);
ALTER TABLE tasks ADD COLUMN waiting_text TEXT;
ALTER TABLE tasks ADD COLUMN unblock_text TEXT;

UPDATE tasks
   SET block_reason = (SELECT e.note FROM task_events e
                        WHERE e.task_id = tasks.id AND e.kind = 'blocked'
                        ORDER BY e.created_at DESC, e.id DESC LIMIT 1),
       blocked_at = (SELECT e.created_at FROM task_events e
                      WHERE e.task_id = tasks.id AND e.kind = 'blocked'
                      ORDER BY e.created_at DESC, e.id DESC LIMIT 1)
 WHERE status = 'blocked';

-- Finding the tasks a finished task unblocks.
CREATE INDEX tasks_waiting_task ON tasks (waiting_task_id) WHERE waiting_task_id IS NOT NULL;
