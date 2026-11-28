# Related links, moving under a parent, questions in Needs you

Status: approved by the owner, 2026-11-28.

## Problem

Agents create follow-ups and notice duplicates, but nothing ties those tasks together: the
human finds them by memory. A task created on its own cannot later become a subtask. And a
question with quick-reply options only reaches Needs you when the agent marked it high or
urgent, so a plain-importance question waits unseen in the inbox.

## Design

### Links

- Table `task_links (task_id, target_id, kind, created_at)`, written from the task whose
  creator or owner (or the human) added it. `kind` is `relates`, `follow-up` (this task
  follows up the target) or `duplicate` (this task duplicates the target).
- One link per pair, whichever way it points (unique index on the ordered pair); adding a
  new kind replaces it, re-adding the same link is a no-op. No self-links. At most 20 links
  per task, counting both directions, checked inside the write transaction for the task and
  every target, so a refused change writes nothing. On Postgres every link write takes
  `TASK_LOCK` first (the lock order from #17), so two writers can never both see a free slot.
- Both tasks show the link. Labels, from the adding task / the other task:
  `related to` / `related to`, `follow-up of` / `follow-up`, `duplicate of` / `duplicated by`.
- `duplicate` cancels the adding task through the existing cancel transition, with a system
  note in its thread ("Duplicate of "<title>" (<id>).") and the same note on the `cancelled`
  event. The status check, the link, the cancellation and the note are one store transaction,
  so a duplicate racing a completion is all or nothing. Refused on a done task; on an already cancelled task it only records the link. Only
  one duplicate target per call.
- Removal (`unrelate`, or **Unlink** in the UI) drops the pair's link from either side; it
  does not reopen a duplicate.
- Every change appends one `related` event on the adding task (payload: added `{id, kind}`
  and removed ids) and bumps its version. History stays append-only: the event is the record,
  the link row is current state.
- Permission is the edit rule: the human, the creating agent, or the session holding the task
  (`planning`, `claimed`, `blocked`, `review`), checked in the store's guarded read. The target
  needs only to exist.
- Surfaces: `task_update` `related: [{id, kind}]` and `unrelate: [ids]`; `task_get` returns
  `related` (id, kind, direction, label, framed title, status); the task page lists
  **Related** and offers **Link a task** / **Unlink** (CSRF POSTs).

### Moving under a parent

- `task_update` `parent: "<id>"` and **Move under a parent** on the task page; creator, owner
  or human. Only a top-level task without participating projects; re-parenting a subtask is
  out of scope and refused with a message saying so.
- The parent must exist, not be `planning`, `done` or `cancelled`, and be in the task's
  project or span it (the `task_create` rule). Self and cycles (the parent is a descendant of
  the task) are refused. On Postgres a move takes `TASK_LOCK` first, then the moved row and
  the parent, and the whole guard, including an ancestor walk, is read after that, so crossed
  concurrent moves (A under b while B under a) cannot form a cycle. The service's explanation
  walk is bounded as a safety net.
- A `moved_under` event on the task (note `moved under "<parent title>"`, payload the parent
  id). Progress is computed from children, so it counts at once; the usual child rules apply
  from then on (coordinator notes, parent to `review` when the last open child closes).

### Questions always in Needs you

`human_attention` also returns unarchived human messages carrying quick-reply options,
whatever their importance. They leave when the human answers (the reply archives the
question) or dismisses them.

## Migration

`0007_related_links` in both stores (main has 0001-0006).
