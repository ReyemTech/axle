# Local projects and adopting a git remote

Status: approved by the owner, 2026-11-28.

## Problem

A session in a folder with no git remote got no project at all and showed as
`<host>/<harness>`. Most projects start as brainstorming before `git init`, so the early
tasks and messages had no home, and nothing connected them to the repository once it
existed.

## Design

1. **Fallback key.** No `AXL_PROJECT`, no `.axl`, and no usable `origin` (no repository, or a
   repository without a remote) gives `local/<host>/<folder>`. `folder` is the git top
   level's basename inside a repository, else the working directory's. Both segments are
   lowercased; each run of characters outside `[a-z0-9._-]` becomes one `-`; leading and
   trailing `-`/`.` are trimmed; at most 64 bytes; empty becomes `folder` (`host`). The
   default display name is the folder; the UI shows "<folder> · local to <host>", and
   `repo_url()` returns `None` for `local/` keys.
2. **Folder identity.** The bridge sends `X-Axl-Folder` (and `folder` in `/v1/register`): the
   first 16 hex digits of SHA-256 of the canonical absolute folder path. Never the path. Pins
   send no folder. The server stores `(local_host, local_folder)` on the local project
   (unique while set) and finds it by that pair, not by name.
3. **Same-name collisions.** The first folder keeps `local/<host>/<folder>`; a different
   folder whose key is already taken gets `local/<host>/<folder>-<first 6 hex of its hash>`.
   Decided on the server, inside the insert, so two racing sessions of one folder still end
   on one project.
4. **Pin.** `.axl` / `AXL_PROJECT` still win over everything. `axl project pin <key>` writes or
   updates `project` in the `.axl` of the git top level (else the cwd), keeping the rest of
   the file (`toml_edit`); `axl project pin --show` prints the effective key and its source.
5. **Noticing a remote.** A session that sends a git key plus a folder hash whose un-adopted
   local project exists on that host **stays on the local project** and records the git key
   as `adopt_key`. Keys never change silently. `GET /v1/project` exposes it; the SessionStart
   hook adds one line telling the agent; the Projects page shows an amber box on that
   project only: "<name> now has a git remote: <key> — Move tasks and history to it".
6. **Adopting (human only, CSRF POST `/ui/projects/{id}/adopt`).** In one transaction:
   - no project has the git key: rename the key in place (display name follows only if it
     was still the default); everything stays attached;
   - the git key's project exists: merge. Tasks, participating-project rows, messages
     (`project_id` and project-addressed `to_id`) move; each local agent either moves
     (with its cursor) or, when the target has the same host and harness, is folded into
     that agent. **A fold never rewrites history or mail:** task events, messages and
     receipts keep the folded id exactly as stored. Only live state follows the survivor
     (its running sessions, and who holds or created a task). The folded agent gets
     `merged_into`, and reads resolve through it: `get_agent(old id)` returns the
     survivor (so labels show it), new messages to the old id are stored to it, listings
     hide it, and the survivor's inbox covers its own receipts and those of agents folded
     into it. "Not its own mail" compares the stored sender with each receipt's own
     recipient, so mail between the folded pair stays deliverable; when two of its
     receipts hold one message it is listed once and stays unread while either is. Before
     anything moves, every local agent's due project and task mail is materialised as
     recipient rows, so nothing unread is lost to a cursor that moves or folds; a moved
     agent's cursor then starts at the newest message, like a new agent's. Tasks and
     messages addressed to the old project id land on the target. The local row stays as
     a redirect: `merged_into = target`, key
     `merged/<id>`; `get_project(old id)` returns the target and listings skip it.
   - Both record a `project_aliases` row (old key, host, folder → project), so the old local
     key, with or without the folder hash, keeps resolving. A running bridge that still sends
     the local key lands on the moved project; new bridges send the git key.

## Not done

- The bridge resolves its project once at start; a remote added mid-session is noticed by
  the next session (the alias keeps the running one working after adoption).
- `merge_project` covers every project and agent reference in migrations 0001–0008. 0006's
  `tasks.waiting_task_id` and 0007's `task_links` (task to task, no creator column) point
  only at tasks, which keep their ids when they move, so they need nothing. Later
  migrations that reference projects or agents must be added to `merge_project`.
- No undo of an adoption.
