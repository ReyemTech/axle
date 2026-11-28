# Plan: local projects and adopting a git remote

Spec: `docs/superpowers/specs/2026-11-28-local-projects-design.md`.

1. **Bridge detection** (`src/bridge/project.rs`): `detect()` returns key + folder hash;
   `local_key`, `local_segment`, `folder_hash`, `project_root` (git top level), `pin`,
   `describe`. Bridge sends `X-Axl-Folder` and `folder` on register. Tests in
   `tests/bridge.rs`: no git, git without remote, git with remote, pin precedence, name
   normalisation, same-named folders.
2. **Migration 0008** (both backends; after main's 0006 and #20's 0007): `projects.local_host,
   local_folder, adopt_key, merged_into`, partial unique index on the folder pair,
   `project_aliases`.
3. **Store** (`Store` trait, SQLite and Postgres): alias-aware `upsert_project`, redirecting
   `get_project`, `list_projects` skipping merged rows, `local_project` (collision suffix),
   `find_local_project`, `note_adoption`, `adopt_project` (rename or `merge_project`).
   `store_matrix!` tests in `tests/store_local_projects.rs`.
4. **Identity service**: route local keys by folder; git key + matching local folder stays
   local and notes the key; `IdentityService::adopt` picks the display name.
5. **Surfaces**: `GET /v1/project`; SessionStart hook notice; Projects page box and
   `POST /ui/projects/{id}/adopt`; `repo_url` ignores `local/`; `axl project pin`.
   Tests: `tests/http_v1.rs`, hook unit test, `tests/ui.rs` (banner only on the matching
   folder, CSRF required, board shows the task under the git key afterwards).
6. **Docs**: concepts (local projects, pin, adopt), cli (`axl project`), web-ui (Projects),
   http-api (`/v1/project`, `folder`).
7. fmt, clippy (`--all-targets --features pg-tests -D warnings`), `cargo test`, and the
   Postgres matrix against a throwaway `postgres:16`.
