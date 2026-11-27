-- Plan gate (docs/superpowers/specs/2026-11-27-plan-gate-and-projects-design.md), Postgres
-- flavour. Kept in step with migrations/sqlite/0002_plan_gate.sql.

ALTER TABLE tasks DROP CONSTRAINT tasks_status_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_status_check
    CHECK (status IN ('open', 'planning', 'claimed', 'blocked', 'review', 'done', 'cancelled'));

-- The human approves a plan before the work starts (opt-in per task).
ALTER TABLE tasks ADD COLUMN requires_plan boolean NOT NULL DEFAULT false;
ALTER TABLE tasks ADD COLUMN plan_state text NOT NULL DEFAULT 'none'
    CHECK (plan_state IN ('none', 'drafting', 'submitted', 'changes_requested', 'approved'));

-- What a message is: an ordinary post, a plan submitted for approval, or a note the server
-- wrote (approvals, added projects).
ALTER TABLE messages ADD COLUMN kind text NOT NULL DEFAULT 'message'
    CHECK (kind IN ('message', 'plan', 'system'));
CREATE INDEX messages_task_kind ON messages (task_id, kind);
