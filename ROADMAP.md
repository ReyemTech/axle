# Axle roadmap

Axle (`axl`) ships today as an MVP: the MCP server, SQLite or Postgres, the stdio bridge, tasks with leases, subtasks and the plan gate, durable messages, the live web UI, Claude Code hooks, Docker, Helm and binary releases. This file lists what comes next, in rough order. Each phase gets its own design doc under `docs/superpowers/specs/` before it is built.

## 1. Release 0.1

- Merge PR #1, tag `v0.1.0`, and check every channel: GitHub Release binaries, the Homebrew formula, crates.io, GHCR, and optionally ECR.
- Switch crates.io publishing to Trusted Publishing (GitHub OIDC) and retire the `CARGO_REGISTRY_TOKEN` secret.
- Verify the parts not yet exercised for real: a live Kubernetes install and the hooks inside a real Claude Code session.

## 2. Public HTTP API

Today `/v1` only serves what the bridge and hooks need (`register`, `heartbeat`, `events`, `inbox/summary`). The goal is a documented JSON API, with every MCP capability and every UI action, for scripts, CI, dashboards and the TUI below.

- **Resources:**
  - `projects`, `agents` (presence)
  - `tasks`: list/filter/get, create, claim, renew, release, complete, post, block/review, add projects
  - plan: propose, approve, request changes
  - `messages`: send, inbox, ack, search
  - `needs-you`
  - `tokens` (admin)
- **Events:** the existing SSE stream `/v1/events`, documented, with resumable `Last-Event-ID`.
- **One service layer:** the API calls the same `service::*` code as MCP and the UI, so behaviour, guards and the peer-data framing stay identical everywhere.
- **Token scopes:**
  - `agent`: today's host tokens.
  - `viewer`: read-only, for dashboards and TUI monitoring.
  - `operator`: human actions over the API, such as approving plans, force-release, cancel and replying as the human.
  Tokens without a scope keep acting as `agent`.
- **Conventions:**
  - cursor pagination
  - JSON error bodies with stable codes (also fixes today's plain-text 4xx on malformed JSON)
  - idempotency keys on create/send
  - the per-token rate limit
- **Spec:** OpenAPI 3.1 generated from the handlers (e.g. `utoipa`), served at `/v1/openapi.json`, and checked in CI so the docs can't drift.
- **Later:** outbound webhooks (Slack, n8n) for "needs you" and task state changes.

## 3. Terminal UI — `axl tui`

**Yes, it can live in the same binary.** It would be a new subcommand, `axl tui`, built with `ratatui` and `crossterm` behind a default-on cargo feature `tui`. That adds roughly 1–2 MB to the binary, and `--no-default-features` builds a server-only image. It needs no extra install and no second artifact to release.

- **Remote first:** `axl tui --server https://axle.example.com` with a `viewer` or `operator` token. It reads through the public API (phase 2) and stays live over `/v1/events` SSE, so it monitors a remote Kubernetes deploy exactly as it monitors local. With no `--server`, it defaults to `AXL_SERVER_URL` and the token in `~/.axl/token`.
- **Views:**
  - Needs you: plans to approve, urgent mail, reviews
  - Board: columns, lease bars, owners and presence
  - Task: thread, plan and actions
  - Agents: who's live or stale, per host and harness
  - Inbox
- **Actions** (operator token): approve or request changes on a plan, reply, dismiss, force-release, cancel. Each is one keystroke with a confirm step for destructive ones.
- **Look:** the same Block Signalling language as the web UI, with signal glyphs and text labels and lease bars as gauges. It follows the terminal's colours and never relies on colour alone.
- **Dependency:** needs phase 2. Its read-only views could start against the existing SSE stream plus a few read endpoints, so a first cut can come early.

## 4. Wake adapters

From the base design, phase 2:

- A Claude Code idle monitor.
- Codex and OpenCode adapters (`axl codex`, `axl opencode`).
- `axl adapters install | status | doctor`, so harnesses are woken by new mail without being told to check.

## 5. Cluster deploy and migration

- A Pulumi stack that installs the Helm chart with Postgres in the ReyemTech cluster, using `ui.auth.secret.existingSecret`.
- Retire Agent Mail hooks and Concord per project. No history is imported.

## 6. Later

- OAuth so claude.ai connectors can use Axle remotely, behind the existing `Authenticator` boundary.
- Several server replicas (Postgres LISTEN/NOTIFY fan-out instead of the in-process event bus).
- Team users and roles beyond a single owner.
- File reservations, if they turn out to be needed.
- Copilot and Gemini adapters.

## Ongoing

- The deferred minor findings in `docs/superpowers/plans/2026-11-27-coord-mvp-followups.md`.
- New items found by review, which are triaged into the phases above.
