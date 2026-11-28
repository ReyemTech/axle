# Board declutter: 12 per column, Show more, compact cards

Owner-approved, 2026-11-28. Stacked on blocked-explained (#17).

## Problem

A busy board (37 claimed tasks) is a wall of tall cards; the few that need the owner sink.

## Design

- **Cap.** Open, Claimed, Blocked and Review render at most `COLUMN_CAP` (12) cards. Done keeps
  its 50 / "N unseen" rules. The header counts every task in the column.
- **Order.** Cards are ranked (stable sort, ties keep the column's order): urgent unarchived
  human mail in the task's thread, plan awaiting you, blocked (task or a subtask), review (task
  or a subtask), lease expired or under a fifth left, the rest. Ranking happens before the cap.
- **Show more.** A capped column ends in "Show N more": a plain `href` to
  `/ui/board?expand=<status>` (no JS: full page, column uncapped) plus `hx-get` of
  `/ui/board/columns?expand=…` into `#columns`, with `hx-push-url`. An expanded column with more
  than 12 offers "Show top 12". `expand` is a comma list of statuses, or `all`.
- **Expanded state lives in the viewer's URL and DOM**, not the server. Expanded columns carry
  `data-expanded`; app.js adds `expand=` from them to every `#columns` re-fetch (SSE `board`,
  60s poll), so live refreshes keep the expansion. Reload keeps it via the pushed URL.
- **Filter.** Typing in the filter box while any column is `data-capped` triggers
  `axl:refetch` with `expand=…,all`; `all` uncaps without marking columns expanded. Clearing
  the filter re-fetches capped columns.
- **Compact.** `GET /ui/board/density?set=compact|detailed` sets the `axl_density` cookie
  (HttpOnly, SameSite=Strict, 1 year) and redirects to the board; it changes nothing else, so
  it is a GET like the other view switches. Compact cards drop the subtask stops, per-project
  holder chips and subtask disclosure, and show "x/y subtasks"; everything with state (status
  word, holder, presence, lease, blocked line, "N blocked") stays.
- **Live cards.** The SSE `task-<id>` frame is rendered once for everyone (detailed), so a
  compact card does not `sse-swap` it; it `hx-get`s `/ui/tasks/{id}/card` on that event, which
  renders in the viewer's density.

## Unchanged

CSP, CSRF, Ack forms and `data-ack-for`, `[data-nav]` keyboard selection, the Done column.
