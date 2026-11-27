# Changelog

All notable changes to Axle are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Axle uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, minor versions may
include breaking changes.

## [Unreleased]

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

[Unreleased]: https://github.com/ReyemTech/axle/commits/main
