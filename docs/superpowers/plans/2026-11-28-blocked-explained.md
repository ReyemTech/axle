# Blocked Tasks Explained Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A blocked task says why, what it waits on (the human, another task, something external) and what would unblock it; the owner can update that while blocked; a task waiting on another is unblocked when that one is done.

**Architecture:** Block detail is a set of nullable columns on `tasks`, written by the guarded `transition_task` and cleared by every write that leaves `blocked`. Wait validation and the automatic unblock run inside the same store transaction as the write that needs them; Postgres serialises them with an advisory lock taken before any row lock. The UI reads the detail off `Task.block`.

**Tech Stack:** Rust, axum 0.8, askama 0.16, htmx, sqlx 0.9 (SQLite + Postgres), rmcp, tokio.

**Spec:** `docs/superpowers/specs/2026-11-28-blocked-explained-design.md`

## Global Constraints

- Migration `0006_blocked_detail.sql` in **both** stores; never edit 0001–0005.
- Columns: `block_reason`, `blocked_at`, `waiting_kind` (`human` | `task` | `external`), `waiting_task_id`, `waiting_text`, `unblock_text`. Cleared whenever the status is not `blocked`.
- `unblock` and `waiting_on.external` are at most 500 characters; a first block needs a reason.
- Refused waits: self, a done/cancelled/unknown task, an ancestor, a cycle. A parent may wait on its own subtask.
- Unblock rule: owner session live (heartbeat within `presence_stale`) → `claimed` with a fresh lease; else → `open`, no owner, plan state `none`. A cancelled dependency unblocks nothing.
- History is append-only: `block_updated` carries the replaced values, read under the write's lock.
- Timestamps come from the `Clock`; no `now()` in SQL. Peer text is escaped (or rendered by `ui::markdown`) and framed.

## Review Focus

1. **Postgres lock order:** `WAIT_LOCK` is taken before any task row lock, in the completion path and the block-on-task path. Test: `completing_a_child_while_its_parent_blocks_on_a_task`.
2. **Races on the wait rule:** the store re-checks the rule inside the write, so the service's check cannot go stale. Test: `waits_that_could_never_end_are_refused` (raw store write).
3. **`block_updated` previous values under concurrency.** Test: `concurrent_block_updates_record_what_they_replaced`.
4. **Hostile reason text** on the card, panel and Needs you. Test: `hostile_block_text_is_escaped`.

---

### Task 1: Store — block detail columns, guarded writes, automatic unblock

**Files:** `migrations/{sqlite,postgres}/0006_blocked_detail.sql`; `src/domain.rs`; `src/store/mod.rs`; `src/store/sqlite.rs`; `src/store/postgres.rs`; `tests/store_done_ack.rs` (new request fields); `tests/store_blocked.rs` (new).

**Interfaces:** `Task.block: Option<BlockDetail>`, `WaitingOn { Human, Task(TaskId), External(String) }`, `TaskEventKind::{BlockUpdated, Unblocked}`; `TransitionRequest.block: Option<BlockWrite>` (with `record_previous`), `TransitionRequest.unblock: Option<UnblockRule>`, `Transitioned.unblocked: Vec<Unblocked>`; helpers `unblocked_records`, `block_previous_payload`.

**Tests:** `block_detail_is_stored_and_cleared`, `done_unblocks_its_waiters`, `done_reopens_a_waiter_whose_owner_left`, `a_cancelled_dependency_does_not_unblock`, `completing_a_child_while_its_parent_blocks_on_a_task`, `concurrent_block_updates_record_what_they_replaced`.

### Task 2: Service — `block_with`, wait checks, publishing

**Files:** `src/service/tasks.rs`; `src/ui/actions.rs` (error mapping).

**Interfaces:** `BlockInput { reason, waiting_on, unblock }`, `TaskService::block_with`, `UNBLOCK_MAX`; `block` stays as a reason-only wrapper. New `TaskError`s: `NothingToUpdate`, `WaitOnSelf`, `WaitTargetNotFound`, `WaitOnClosed`, `WaitOnAncestor`, `WaitCycle`. `complete` passes an `UnblockRule`; `after_transition` publishes `TaskChanged(Unblocked)` and `MessageCreated` for each unblocked task.

**Tests:** `owner_updates_a_blocked_task`, `waits_that_could_never_end_are_refused`, `unblocking_publishes_events`.

### Task 3: MCP — `task_update` `waiting_on` / `unblock`, `blocked` in task objects

**Files:** `src/mcp/tools.rs`, `src/mcp/mod.rs`, `tests/mcp_tools.rs`.

**Interfaces:** `WaitingOnParam { human, task, external }` (exactly one); `task_summary` adds `blocked { reason, since, waiting_on, unblock }` with framed agent text.

**Tests:** `task_update_blocks_with_waiting_on_and_unblock`.

### Task 4: UI — Blocked panel, card line, "Blocked on you"

**Files:** `src/ui/task_page.rs`, `src/ui/views.rs`, `src/ui/needs.rs`, `src/ui/actions.rs` (`back` on thread posts), `templates/task.html`, `templates/_card.html`, `templates/_needs_you.html`, `templates/_pulse.html`, `static/app.css`, `tests/ui.rs`.

**Interfaces:** `BlockedView`, `WaitingTask`; `CardView.blocked_line` / `blocked_on_you`; `Need::BlockedOnYou` with `NeedCard.reply_task`.

**Tests:** `blocked_panel_says_why_and_what_unblocks_it`, `blocked_on_you_is_a_needs_you_card_with_a_reply_box`, `hostile_block_text_is_escaped`.

### Task 5: Agent rule and docs

**Files:** `src/install/instructions.md` and the Claude Code golden fixtures under `tests/fixtures/harness/claude-code/`; `docs/concepts.md`, `docs/mcp-tools.md`, `docs/web-ui.md`; the spec.

**Tests:** the existing install golden tests (`tests/install_claude_code.rs`).
