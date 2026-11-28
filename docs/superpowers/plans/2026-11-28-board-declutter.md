# Plan: board declutter

Design: `docs/superpowers/specs/2026-11-28-board-declutter-design.md`.

1. Tests first (`tests/ui.rs`): cap at 12 with the true count and Show more links (href,
   hx-get, hx-push-url, project kept); priority order (urgent mail, plan, blocked subtask,
   expiring lease); `?expand=` page and fragment uncapped; compact cookie markup and the
   `/ui/tasks/{id}/card` fragment; `expand=all` for the filter; CSP unchanged.
2. `src/ui/views.rs`: `COLUMN_CAP`, `Expand` (parse `?expand=`), `attention` ranking,
   `ColumnView { total, expanded, more, fewer }`, `compact()` cookie read, `set_density`,
   `card_fragment`. Routes in `src/ui/mod.rs`.
3. Templates: `_columns.html` (count, `data-capped`/`data-expanded`, links), `_card.html`
   (compact branch, re-fetch instead of `sse-swap`), `board.html` (Detailed/Compact, the
   `axl:refetch` trigger).
4. `static/app.js`: add `expand` to `#columns` requests; filter re-fetches all. `app.css`:
   footer links, compact spacing, density control.
5. Docs: `docs/web-ui.md`. Then fmt, clippy (`--features pg-tests -D warnings`), full tests.
