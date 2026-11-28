# Plan: related links, moving under a parent, questions in Needs you

Spec: `docs/superpowers/specs/2026-11-28-related-links-design.md`.

1. Migration `0007_related_links` (SQLite and Postgres): `task_links` with the pair index.
2. Domain: `LinkKind` (with `label`), `TaskLink`, event kinds `related` and `moved_under`.
3. Store: `relate_tasks`, `task_links`, `move_task` on both backends; `human_attention`
   includes messages with options. `store_matrix!` tests: symmetric read, replace, removal,
   no-op re-add, self link, unknown target, the 20 limit, permissions, duplicate cancel with
   note, duplicate refused on done; move, progress, consolidation, every refusal; a
   normal-importance question in and out of Needs you.
4. Service: `TaskService::relate`, `related`, `move_under`; errors for each refusal.
5. MCP: `task_update` `related`, `unrelate`, `parent`; `task_get` `related`. Tests for
   permission, labels, duplicate, removal and move.
6. UI: Related panel, **Link a task**, **Unlink**, **Move under a parent** (CSRF); tests.
7. Docs: `mcp-tools.md`, `concepts.md`, `web-ui.md`; Axle rules line in
   `src/install/instructions.md` and the golden fixtures.
8. `cargo fmt --check`, clippy with `pg-tests`, `cargo test`, Postgres run.
