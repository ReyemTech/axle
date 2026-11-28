# Done column: ack and auto-ack — design

Status: approach approved by the owner on 2026-11-27 (approach A). Size: small change.

## Why

The board's Done column shows top-level tasks finished in the last 7 days. With several agents
splitting work into subtasks, that is easily 100+ cards a week, and the server loads every done task
ever and filters in memory on each render (and on each SSE update). The owner uses Done for two
things: a glance at what just finished, and checking each finished task off.

## Goals

- Done holds what the owner has not seen yet; acking a task takes it off the board.
- A safety net: tasks done longer than N days count as auto-acked and leave the board, but the
  board says how many of those the owner never saw, until the owner clears them.
- Every finished task stays reachable in a History page.
- The Done query is bounded and indexed.

## Non-goals / anti-goals

- Nothing is deleted. Ack is a view flag, like the inbox.
- No background job. Ack never blocks or changes task state for agents.
- No per-user ack: Axle has one human owner.

## Approaches

- **A. Derive auto-ack at read time (chosen).** Store only the owner's own ack (`acked_at`).
  "Auto-acked, unseen" = done before now − N days and `acked_at` is null: a query, nothing written.
  The count is always correct and changing N takes effect at once in both directions.
- B. Background sweeper writing `acked_at` + `ack_kind = auto`. Rejected: a timer, config and
  tests for it, and changing N leaves stale rows; the only gain (a record of when each auto-ack
  happened) no screen needs.

## Design

**Data** (migration `0004_done_ack.sql`, both stores; never edit earlier migrations):
- `tasks.done_at` (nullable timestamp): set when a task moves to `done`, cleared if it leaves
  `done`. Backfill existing done tasks from `updated_at`. *My call:* `updated_at` moves when
  anything touches the task, so the window and the count would drift without it.
- `tasks.acked_at` (nullable timestamp): set by the owner's ack.
- Index on `(status, done_at)` (or partial index where status = done where the store supports it).

**Rules:**
- *Unseen* = `status = done`, `acked_at` null, `done_at >= now − N`. The Done column shows these,
  top-level only, newest first.
- *Auto-acked, unseen* = `status = done`, `acked_at` null, `done_at < now − N`.
- N = `AXL_DONE_AUTO_ACK_DAYS`, default **7**, integer ≥ 1; invalid values fail startup with a
  clear message, like other config.
- Acking a parent sets `acked_at` on the parent and its done descendants. Acking an already acked
  task is a no-op.
- A task that leaves `done` (re-opened by a future feature) has `done_at` and `acked_at` cleared.

**Board (Done column):**
- Header: `DONE · <n> unseen`, an **Ack all** button (acks exactly the cards shown, by id — not
  "everything done", so a task finishing mid-click is not acked unseen), and a **History →** link.
- Each card gets an **Ack** button (labelled text, per DESIGN.md; no glyph alone).
- When the auto-acked unseen count > 0, a line under the header: `<k> auto-acked, unseen →`
  linking to History filtered to those tasks.
- Ack actions are CSRF-protected POSTs, update live over SSE (a task event), and work without JS
  via redirect back.

**History page** (`/ui/history`):
- Every done task, newest `done_at` first, paged (50 per page, cursor), with a project filter and a
  title search. Shows done time and acked state (unseen / acked / auto-acked).
- Filter `?unseen=auto` shows the auto-acked unseen set, with an **Ack these** button that acks the
  listed ids and clears the count.
- Linked from the board and the nav.

**MCP / HTTP:**
- `task_list` leaves out acked and auto-acked done tasks by default; new boolean parameter
  `include_acked` (default false) returns them. Other statuses are unaffected.
- `task_get` still returns any task by id. Agents cannot ack (no MCP tool); ack is the human's.

**Store:** `TaskFilter` gains the done-window/ack fields it needs so the Done column and History
query in SQL with `LIMIT`, not in memory. Both stores, same behaviour.

## Tests

- Store, both backends: `done_at` set/backfilled; ack, ack parent covers children, idempotent;
  unseen vs auto-acked boundaries at exactly N days; changing N moves tasks between sets without
  writes.
- UI: column header counts, Ack / Ack all (by id; a task finishing between render and click stays
  unseen), auto-acked line hidden at 0, History paging/filter/search, Ack these clears the count,
  CSRF rejected without token.
- MCP: `task_list` default hides acked/auto-acked done tasks; `include_acked` returns them.
- Config: `AXL_DONE_AUTO_ACK_DAYS` default, valid, invalid.

## Left to the builder

Exact markup and copy within DESIGN.md, index form per store, History page layout, how the nav
link is labelled.
