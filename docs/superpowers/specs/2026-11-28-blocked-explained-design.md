# Blocked tasks say why and what unblocks them

**Problem.** A block's reason lived only in a history line. The task page and the card did
not show it, nobody could tell what the task was waiting on, and the owner could not change
the reason while blocked ("cannot move a blocked task to blocked").

**Model.** While a task is `blocked`, columns on `tasks` (migration `0006_blocked_detail`,
both stores) hold `block_reason`, `blocked_at`, `waiting_kind` (`human` | `task` |
`external`), `waiting_task_id`, `waiting_text` and `unblock_text`. Every write that leaves
`blocked` clears them (transitions and claims); history keeps every earlier block. Tasks
blocked before the migration get their reason and time from their latest `blocked` event.

**MCP.** `task_update` with `status: blocked` takes `waiting_on` (`{"human": true}` |
`{"task": id}` | `{"external": text}`) and `unblock` (at most 500 characters). On a task the
caller already holds blocked it is an update: given fields replace, others are kept, and a
`block_updated` event records the previous values.

**Waits that could never end are refused**: on itself, on a done or cancelled task, on the
task's own parent or ancestor (a parent finishes after its subtasks), or closing a cycle
(a waits on b waits on a). A parent may wait on its own subtask. The store enforces the same
rule inside the write (a guard miss); Postgres serialises waits on tasks and the `done`
transitions that release them with a transaction-scoped advisory lock, so a task can never
start waiting on one that finished concurrently.

**Auto-unblock.** In the same transaction that makes a task `done`, every blocked task with
`waiting_kind = task` pointing at it leaves `blocked`. The rule mirrors how an agent
unblocks today (re-claiming its own task): if the owner's session is live (heartbeat within
`presence_stale`), back to `claimed` with a fresh lease for that session; otherwise back to
`open` with no owner, as a release would. Each gets an `unblocked` event and a system note in
its thread, `unblocked: "<title>" (<id>) is done`, written without a sending agent so the
owner sees it even when it finished the awaited task itself. `TaskChanged` and
`MessageCreated` are published for each; wake-up will use them to wake the owner.

**Cancelled dependencies do not unblock.** Cancelled is not done: the waiter stays blocked
and the Blocked panel says "Waiting on a cancelled task". The owner re-points or releases.

**UI.** The task page opens with a Blocked panel: the reason (agent markdown, rendered
safely), who blocked it and when, Waiting on (you / the task with its live status /
external text) and What unblocks it. A board card shows the reason's first line under the
title. A task waiting on the human is a Needs you card, "Blocked on you", with the unblock
text and a reply box posting into the task thread (CSRF like every POST).

**Agent rule.** The installed Axle instructions gain one line: "When you block a task, say
what it waits on and what would unblock it."
