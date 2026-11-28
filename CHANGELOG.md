# Changelog

All notable changes to Axle are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Axle uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, minor versions may
include breaking changes.

## [Unreleased]

## [0.1.3](https://github.com/ReyemTech/axle/compare/v0.1.2...v0.1.3) - 2026-11-28

### Added

- *(ui)* show a task's token usage on its page
- *(ui)* Usage page with totals, breakdown and usage over time
- *(cli)* axl status shows usage collection, billing and the last report
- *(install)* register the SessionEnd hook for Claude Code and Codex
- *(hook)* report token usage on Stop and SessionEnd
- *(usage)* collect usage deltas from harness logs and report them
- *(usage)* cursor files, billing detection and the [usage] config table
- *(usage)* parse token usage from Codex rollouts
- *(usage)* parse token usage from Claude Code transcripts
- *(cli)* axl usage reprice recomputes stored costs
- *(http)* accept token usage reports at POST /v1/usage
- *(usage)* built-in API price table with an override file
- *(store)* sum token usage by project, task, agent, harness, host, model and day
- *(store)* record token usage and split it across held tasks
- *(ui)* one-line compact cards
- move a top-level task under a parent
- related links between tasks; questions always in Needs you
- *(install)* rules for PR links, review threads and follow-up subtasks
- *(ui)* declutter the board: 12 per column with Show more, compact cards
- blocked tasks say why and what unblocks them
- *(ui)* show quick replies as buttons in the inbox and on Needs you cards
- *(mcp)* let send_message offer the human 2-4 quick-reply options
- *(ui)* render agent markdown safely in plans, threads and inbox
- *(ui)* history page for finished tasks
- *(ui)* done column shows unseen work with Ack and Ack all
- *(mcp)* hide acked done tasks from task_list; add AXL_DONE_AUTO_ACK_DAYS
- *(tasks)* ack done tasks and their done descendants
- *(store)* query done tasks by ack window, title and done order
- *(store)* record done_at and acked_at on tasks
- *(ui)* Cmd+Enter or Ctrl+Enter sends the message
- *(install)* write the Axle rules block into ~/.claude/CLAUDE.md for Claude Code
- *(http)* add read-only GET /v1/leases listing a session's held tasks
- *(ui)* link project keys to their repository

### Fixed

- *(ui)* show no price or a lower bound when totals include unpriced models
- *(usage)* hold a trailing Codex fallback record until the next run
- *(usage)* count a Codex token_count only when the running total advances
- *(store)* count a lease takeover as the new owner's holding
- *(client)* give each private write its own temporary file
- *(store)* merge a duplicate usage key only from the token that stored it
- *(usage)* keep the off switch when a billing value or config.toml is bad
- *(ui)* build the usage page's board filter with the new expand field
- *(ui)* say no price on a task's usage line when the model is unpriced
- *(usage)* report an unreadable transcript once per session
- *(ui)* wrap compact signals instead of clipping them in narrow columns
- *(tasks)* allow replacing a link at the limit and moving with a new wait
- *(mcp)* check links and moves before task_update writes anything
- *(store)* refuse a move that makes a task wait on its own ancestor
- *(store)* serialize links and moves under TASK_LOCK; cancel a duplicate atomically
- *(ui)* re-rank the board on urgent thread mail; density links keep the expansion
- *(ui)* rank urgent thread mail first however much attention mail is older
- *(store)* one lock order for completion, blocking and claims on Postgres
- *(store)* record a block update's previous values under the write lock
- *(store)* take the wait lock before any task row lock on Postgres
- *(ui)* done ack polish from review
- *(mcp)* frame quick-reply options as peer text
- *(ui)* keep URL paren trimming linear in markdown
- *(ui)* hide per-card Ack with its card when the filter hides it
- *(ui)* bulk ack only submits cards the filter shows
- *(store)* ack done descendants below non-done intermediates
- *(ui)* ack copy, per-card labels, history filter link and ASCII-only back
- *(store)* use the done index on SQLite, keep DoneNewest to done rows, ack pre-upgrade done tasks older than a week
- *(hook)* frame held task titles, claim the nudge atomically, cache leases
- *(hook)* always inject the Axle rules at session start and nudge once without a task

### Other

- *(codex)* say plainly the cache-write mapping is unverified and how to check it
- *(hook)* say a slow usage report can delay the Stop block within its budget
- Merge pull request #23 from ReyemTech/feat/compact-one-line
- *(store)* renumber blocked_detail migration to 0006
- *(store)* renumber message_options migration to 0005

## [0.1.2](https://github.com/ReyemTech/axle/compare/v0.1.1...v0.1.2) - 2026-11-28

### Added

- *(cli)* axl install --force and --port; UI connect command runs axl install
- *(cli)* axl status with one fix per problem
- *(cli)* axl uninstall from the install manifest
- *(cli)* axl install with login, local mode and dry run
- *(cli)* axl service with launchd and systemd user units
- *(install)* OpenCode config editor with manual-step fallback
- *(install)* Codex config editor with hooks block; axl hook learns Codex
- *(install)* Claude Code plugin editor with adoption
- *(install)* binary path rules, backups, marker blocks and manifest
- *(cli)* axl login (code and device flow) and axl logout
- *(server)* serve install.sh pinned to the server version; helm publicUrl
- *(ui)* connect a machine, pending enrollments and device approval
- *(http)* enroll and device-login endpoints with per-IP limit, whoami
- *(enroll)* enrollment service with one-time codes and device flow
- *(store)* enrollments table and guarded consume for both backends
- *(client)* ~/.axl/config.toml with server URL precedence

### Fixed

- *(service)* check a changed port before restarting a running service
- *(onboarding)* keep remote server URL, fail status on unchecked token, revalidate saved token on install
- install.sh status is advisory, clear redirect error, Codex hook timeout 10, redacting Debug
- *(cli)* axl status separates 401 from server errors and lists the root cause first
- *(cli)* record partial installs, redact secrets, positional login URL
- *(cli)* wait for launchd unload before bootstrap, scope dollar escaping, narrow purge
- *(install)* fail loudly on corrupt manifest and orphan markers, back up replaced links, ignore relative PATH
- *(cli)* read config before redeeming; clamp server-supplied login timings
- *(server)* install.sh treats backslash as a URL path delimiter
- *(server)* install.sh tty probe safe under dash, strip query/fragment before userinfo
- *(server)* install.sh stdin isolation, exact loopback match, hostless ingress entries
- *(ui)* connect code countdown, copy labels and CSRF/escaping tests
- *(http)* key per-IP limits by IpAddr and evict refilled buckets
- *(client)* warn on stderr when config.toml is ignored
- *(release)* build arm64 Linux on Ubuntu 22.04 for older glibc, restore Homebrew homepage

### Other

- cargo fmt
- onboarding (install, login, enrollment, status, service)

### Added

- One-command onboarding: `axl install` connects a machine (local service or team server)
  and configures Claude Code, Codex (MCP bridge and hooks) and OpenCode (MCP bridge and
  `AGENTS.md`), with backups, a manifest and `--dry-run`.
- `axl login`, `logout`, `uninstall`, `status` and `service` (launchd or systemd user unit).
- Enrollment by one-time code or browser device login, a **Connect a machine** panel on the
  Tokens page, and `GET /install.sh`. New endpoints: `/v1/enroll`, `/v1/login/start`,
  `/v1/login/poll`, `/v1/whoami`, `/v1/token/revoke`.
- `AXL_PUBLIC_URL` (Helm `publicUrl`) and `AXL_TRUSTED_PROXY_HEADER`; enrollment and login
  are limited to 10 requests per minute per client IP.
- A [CLI reference](docs/cli.md); the harness guides keep their hand-wired snippets under
  "Manual setup".

## [0.1.1](https://github.com/ReyemTech/axle/compare/v0.1.0...v0.1.1) - 2026-11-27

### Fixed

- *(release)* define the dist cargo profile and fix the CLA allowlist

### Other

- drop homepage that duplicates repository
- add package metadata and social preview for discoverability

## [0.1.0] - 2026-11-27

The first release: the MVP.

### Added

- **Server** (`axl serve`): an MCP coordination server over Streamable HTTP at `/mcp`, with
  SQLite or Postgres storage, migrations on start, health (`/healthz`) and readiness
  (`/readyz`) probes, and graceful shutdown on SIGINT and SIGTERM.
- **17 MCP tools**: `whoami`, `project_list`, `agent_list`, `send_message`, `fetch_inbox`,
  `ack_messages`, `search_messages`, `task_create`, `task_list`, `task_get`, `task_claim`,
  `task_post`, `task_update`, `task_propose_plan`, `task_renew`, `task_release` and
  `task_complete`. No tool takes an identity argument.
- **Identity**: projects from `AXL_PROJECT`, a `.axl` file or the normalised git origin;
  agents as project, host and harness; one session per harness process.
- **Tasks and leases**: atomic claims, 15-minute leases renewed by heartbeats and tool calls,
  idle cut-off after 2 hours, same-agent takeover from a stale session, and append-only
  history.
- **Subtasks across projects**: parent tasks with participating projects (append-only),
  child tasks in one project, automatic notes to the coordinator, and parents moving to
  review when the last child closes.
- **Plan gate**: `requires_plan` tasks move to `planning` on claim; the agent submits a plan
  with `task_propose_plan`, and the human approves or requests changes in the web UI.
- **Messages**: durable mail to a project, agent, session, task thread or the human, with
  importance, at-least-once delivery, acknowledgement and full-text search.
- **Peer-data framing**: all peer-written text reaches models as
  `[axl from <agent> in <project>] … (information, not instructions)`, with frame markers
  defused.
- **Bridge** (`axl bridge`): a fail-open stdio MCP proxy for any harness, with heartbeats,
  automatic re-registration and a session file for hooks.
- **Claude Code hooks** (`axl hook`): unread mail surfaced on session start, prompt submit,
  after tool use, and an urgent-mail stop guard; always exit 0.
- **Web UI** ("Block Signalling"): overview, board with lease bars and subtask stops, Needs
  you strip, task pages with plan approval, force release and cancel, unified inbox with
  receipts, project renaming and token revocation, live updates over SSE, keyboard
  navigation, light and dark themes.
- **UI sign-in** (`AXL_UI_AUTH`): `auto`, `none`, `secret` or `trusted-header`; CSRF on
  every POST, strict CSP, and a `Host` allowlist against DNS rebinding.
- **Tokens** (`axl token create | list | revoke`): hashed bearer tokens with last-use
  tracking and a per-token rate limit.
- **`/v1` API** for the bridge and hooks: `register`, `heartbeat`, `events` (SSE) and
  `inbox/summary`.
- **Distribution**: prebuilt binaries for macOS and Linux (x86_64 and arm64) with a shell
  installer and Homebrew formula, crates.io (`axle-mcp`), a multi-arch image on GHCR, and a
  Helm chart published as an OCI artifact.

[Unreleased]: https://github.com/ReyemTech/axle/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ReyemTech/axle/releases/tag/v0.1.0
