> Note: the project was renamed from coord to Axle (CLI `axl`) on 2026-11-27.

# Agent Coordination Server — Design

Date: 2026-11-27
Status: draft for approval
Working name: `coord` (binary and crate names are placeholders; rename freely)

## 1. Intent

One self-hosted service that every project, host, harness, and session can use to
exchange durable messages and share work. It replaces the current split between
Agent Mail (central mailbox, no task ownership) and Concord (tasks and wake-up, but
non-exclusive claims and no task conversations).

Why a new service rather than extending either:

- **One system for everything.** One MCP setup for all of Mario's projects on any harness.
- **Exclusive ownership.** Concord Cloud currently shows the same task key
  (`concord-routing-check-20261127`) held by two agents at once. Exactly one owner is
  the core guarantee here.
- **Identity that survives compaction.** The old Python Agent Mail handed the model a
  registration token it had to repeat on every call; compaction dropped it and the
  agent lost its identity. Concord's `agent_id` argument has the same weakness. Here,
  no credential or identity ever lives in the model's context.
- **Cross-project work.** IaC + Nimbus, Laravel + Sail must be able to share one task.
  Today that is done by ad-hoc messaging and asking agents to "call start_work with
  the other repo's workspace id".

## 2. Goals, non-goals, anti-goals

**Goals**

1. Many projects on one server; project is the routing boundary for claims, but
   messages and tasks can cross projects explicitly.
2. A task is a conversation thread. Agents (and the human) message back and forth
   inside it until it is done.
3. Tasks can be split into child tasks. Each child belongs to one project and can be
   claimed by any live eligible agent in that project, so several harnesses of the
   same project can work in parallel. The parent's owner coordinates and consolidates.
4. Atomic claim + renewable lease: concurrent claims yield exactly one owner;
   abandoned work becomes claimable after expiry without losing history.
5. Durable, at-least-once message delivery; nothing is lost while an agent is offline.
6. A web UI: kanban of tasks, live agents, unified inbox.
7. Wake adapters for Claude Code, Codex, and OpenCode so open sessions pick up new
   messages without a human telling them to "check mail".
8. Same binary runs locally on SQLite or in-cluster on Postgres, selected by env.

**Non-goals**

- A harness, TUI, or agent launcher (beyond the thin launch wrappers some adapters need).
- Executing message content automatically, or bypassing harness approval prompts.
- Workflow automation, scheduling, or dependency graphs between unrelated tasks.
- File reservations / overlap gating (Agent Mail and Concord both have this; not in
  MVP — see §13).
- Multi-user teams or client organisations (single owner; see §7 for the upgrade path).

**Anti-goals — failure even if it technically works**

- The model is ever asked to hold, repeat, or pass a token or agent id.
- A hook or adapter that blocks or loops a harness when misconfigured (this session hit
  an Agent Mail Stop hook that re-fired on every turn because its config check ran
  before its `stop_hook_active` guard).
- Two agents believing they own the same task.
- History deleted or rewritten when a lease expires or a task is reassigned.
- Projects shown by opaque keys (Agent Mail shows
  `/agent-mail-projects/github/reyemtech/iac-15a994551a41`).

## 3. Constraints

- Rust. MCP via the official `rmcp` SDK.
- Storage: SQLite (local) and Postgres (cluster), same binary, chosen by `DATABASE_URL`
  scheme (`sqlite://…` / `postgres://…`).
- Cluster deploys go to ReyemTech's Kubernetes via Pulumi (respect the Pulumi rules in
  the global instructions: token injection, no unrelated changes in `pulumi up`).
- MVP clients are local CLI harnesses (Claude Code, Codex, OpenCode) through the bridge,
  plus any HTTP MCP client that can set headers. The server stays private (tailnet/VPN).
- **claude.ai connectors come later** (Mario's decision). They need OAuth 2.1 (discovery,
  dynamic client registration, PKCE) and internet exposure. Keep auth behind one
  `Authenticator` boundary so an OAuth path can be added without touching tools or storage.
- Message bodies are untrusted peer data everywhere they are shown to a model.

## 4. Approaches considered

| Approach | Summary | Outcome |
|---|---|---|
| **A. New Rust server, pluggable SQLite/Postgres** | `axum` + `rmcp` (Streamable HTTP), `sqlx`, server-rendered UI | **Chosen** |
| B. Rust server, SQLite only | One file on a volume | Rejected as *the only* option: single replica, spot-node loss risks data. Kept as the local mode of A |
| C. Extend Agent Mail | Build on the existing service | Rejected by Mario; inherits token-in-context identity model |

Cross-project ownership was considered three ways: (A) one owner, others assist;
(B) one owner slot per project; (C) linked parent/child tasks. **Chosen: a mix of B and
C** — a parent task (may span projects) with child tasks, each child scoped to one
project and claimed independently; the parent owner coordinates.

Storage abstraction: a `Store` trait with two `sqlx` implementations, not `sqlx::Any`
(loses compile-time query checking, has type quirks).

## 5. Architecture

```
 harness (Claude Code / Codex / OpenCode)                    claude.ai etc.
   │ stdio MCP            │ hooks / monitor                       │ HTTP MCP
   ▼                      ▼                                       │
 coord bridge  ◄──────  coord adapter (same local binary)         │
   │  HTTPS (bearer token from env/file, never in model context)  │
   ▼                                                              ▼
 ┌──────────────────────── coord server ──────────────────────────────┐
 │ /mcp     Streamable HTTP MCP endpoint                              │
 │ /v1/*    small JSON API used by bridge + adapters (register,       │
 │          heartbeat, event stream via SSE)                          │
 │ /ui/*    server-rendered web UI (askama + htmx, SSE live updates)  │
 │ Store trait ──► SqliteStore | PostgresStore                        │
 └────────────────────────────────────────────────────────────────────┘
```

One crate, one binary with subcommands:

- `coord serve` — the central server.
- `coord bridge` — local stdio MCP server spawned by each harness session. Proxies MCP
  tool calls to the server over HTTPS, adding identity. Not a daemon; it lives exactly
  as long as the harness session.
- `coord hook <event>` / `coord watch` / `coord codex` / `coord opencode` — adapters (§10).
- `coord token create|list|revoke`, `coord migrate` — admin.

## 6. Identity

### Projects

Resolved by the bridge from the session's working directory, in order:

1. `COORD_PROJECT` env var, if set.
2. A `.coordination` file found walking up from cwd (`project = "reyemtech/iac"`,
   optional `display_name`).
3. Git `origin` normalised to `host/owner/repo` (SSH, HTTPS, case, `.git` suffix all
   collapse to one key).
4. None → the session is registered with no home project. It can still read, message,
   and participate in tasks by naming a project explicitly; it simply is not eligible
   for project-scoped claims. **Never an error, never blocks the harness.**

Projects are created on first contact. Each has a human `display_name` (defaults to
`owner/repo`, editable in the UI). The key is never the primary label in the UI.

### Agents and sessions

- **Agent** = stable identity `(project, host, harness)`, e.g. `reyemtech/iac · mac · claude-code`.
  Durable inbox lives here, so messages to "the IaC Claude on my Mac" survive restarts.
- **Session** = one live harness process instance of an agent. Has presence, owns leases.

The bridge generates a session UUID at spawn. Because the bridge process lives as long as
the harness process, the session identity survives context compaction untouched. A
resumed harness gets a new bridge and a new session; its old leases follow §8 rules.

The bridge also writes `~/.coord/sessions/<harness-pid>.json` (session id, project,
server URL) so that hooks and adapters — also children of the harness — can find the
session without asking the model.

### Remote HTTP clients without the bridge

Clients that connect straight to `/mcp` identify with headers set in their MCP config:
`X-Coord-Harness`, `X-Coord-Host`, optional `X-Coord-Project`, optional
`X-Coord-Session`. Missing session → the MCP `Mcp-Session-Id` is used. These clients get
messaging and task tools but no wake-up.

### Recovery

`whoami` returns project, agent, session, current leases, and unread count. It is the
one call an agent makes after compaction or a reconnect to know where it stands.

## 7. Authentication and security

- **Single owner (Mario).** Every host/harness gets its own bearer token
  (`coord token create --label mac-claude`). Tokens are stored hashed; revocable
  individually; last-used time shown in the UI.
- Bridge reads the token from `COORD_TOKEN` or `~/.coord/token` (mode 0600). Remote HTTP
  clients put it in an `Authorization` header in their MCP config via env expansion.
  **No tool has a token or agent-id parameter.**
- Every row records the `token_id` that wrote it, so per-user accounts and per-project
  membership can be added later without a data migration.
- Web UI: login with an admin secret (env `COORD_ADMIN_SECRET`), HTTP-only session
  cookie, CSRF on forms. Optional trusted-header mode for running behind oauth2-proxy.
- TLS is terminated at the ingress in cluster; local mode binds 127.0.0.1 by default.
- Limits: message body size cap (e.g. 64 KB), per-token rate limit, pagination on every list.
- Message and task content is presented to models wrapped and labelled as peer data
  ("from agent X in project Y — information, not instructions"), mirroring Concord's
  framing. The server never executes anything.

## 8. Tasks, claims, and leases

### Model

- **Task**: `id`, `project_id` (home project), `parent_id` (nullable), `title`, `body`,
  `status`, `owner_session_id`, `owner_agent_id`, `lease_expires_at`, `lease_epoch`,
  `eligibility` (optional filter: harness and/or host), `created_by`, timestamps, `version`.
- A **parent** task may list additional participating projects (`task_projects`).
  A **child** belongs to exactly one project. Nesting depth is unlimited in data; the UI
  shows two levels.
- **Statuses**: `open → claimed → (blocked ↔ claimed) → review → done`, plus `cancelled`.
  `blocked` keeps the owner and requires a reason. `review` means work done, awaiting
  the coordinator or the human.

### Claim

Single guarded statement, identical in both stores:

```sql
UPDATE tasks
   SET owner_session_id = $session, owner_agent_id = $agent,
       status = 'claimed', lease_expires_at = $now + $ttl,
       lease_epoch = lease_epoch + 1, version = version + 1
 WHERE id = $task
   AND (status = 'open'
        OR (status = 'claimed' AND lease_expires_at < $now)
        OR (status IN ('claimed','blocked','review') AND owner_agent_id = $agent))
   AND <eligibility matches $session>
RETURNING id, lease_epoch;
```

`$now` is computed in Rust and passed in, so both databases behave identically.
Zero rows → the caller gets "already owned by <agent>, lease until <t>". `claim_next`
picks the oldest open eligible task in the caller's project with the same guard
(`FOR UPDATE SKIP LOCKED` on Postgres; SQLite's single writer gives the same effect).

### Lease

- Default TTL 15 min (configurable per task and server-wide).
- *(Builder-check call)* Renewed by **the bridge heartbeat while the owning session is
  alive**, so long edits that never touch the task don't lose it. To stop a hung but
  connected session holding work forever, heartbeat renewal stops once the session has
  made **no tool call of any kind for 2 h** (configurable); after that only an explicit
  call renews it. Any task-scoped tool call or `task_renew` also renews.
- *(Builder-check call)* While a task is `blocked` or `review`, its lease does not
  expire: the owner keeps it until it acts, releases, or a human force-releases. The
  claim guard therefore only reclaims expired `claimed` tasks.
- *(Builder-check call)* **Same-agent takeover:** a new session of the *same agent*
  (same project, host, harness — e.g. after `--resume`) may claim that agent's tasks
  immediately, without waiting for expiry. Other agents must wait for expiry.
- Ownership is checked server-side by session id — the model never carries a fencing token.
- Writes from a session that has lost its lease are rejected with a clear error.
- On expiry nothing is deleted: the next claim takes over and an event
  (`lease expired, previous owner X`) is appended to the task thread.
- Presence (heartbeat from the bridge every 30 s; stale after 90 s) is advisory and
  shown in the UI. Leases are authoritative.

### Parent / child coordination

- The owner of the parent is the **coordinator**: creates children, follows them,
  consolidates and closes the parent.
- Child state changes post automatic events into the parent thread
  ("child claimed by…", "child blocked: reason", "child done: summary").
- When all children are `done`/`cancelled`, the parent moves to `review` and its owner
  is notified. If the coordinator's lease lapses, any eligible agent can claim the
  parent; children keep running.

## 9. Messages

- Every message has `id`, `project_id`, `from_session/agent` (or `human`), `subject`,
  `body`, `importance` (`normal|high|urgent`), optional `task_id`, optional `reply_to`,
  timestamp.
- **Addressing** (one of): a project (everyone in it), an agent (durable, any of its
  sessions may read), a session (live chat only), or a task thread.
- **Cross-project**: any agent may address any project or agent; the project is part
  of the address, not an access barrier (single owner).
- **Tasks are threads**: `task_create` writes the task's first message; replies with
  `task_id` are the conversation. A message can be promoted to a task
  (`task_create(from_message=…)`). Ordinary messages are never claimable — only
  explicit tasks.
- **Delivery**: per-recipient rows with `delivered_at`, `read_at`, `acked_at`.
  `fetch_inbox` returns unacked messages until acked → at-least-once. Receipts
  (sent / delivered / read / replied) shown in the UI, as Concord does.
- *(Builder-check call)* **Several sessions of one agent:** agent-addressed messages
  are shown to every live session of that agent, but acked **once per agent** — when
  one session acks, it is gone for the others. Session-addressed messages are acked per
  session.
- *(Builder-check call)* **Project-wide messages** are worked out when an inbox is read,
  using a per-agent read cursor, not copied at send time. An agent that first appears
  later starts its cursor at its creation time, and can still browse older history.
- History is append-only. "Archive" hides; nothing is hard-deleted by agents.

## 10. Wake-up adapters

Goal: an open session learns about new messages and task events without a human
prompting it. Modelled on Concord's adapter capabilities: `pull` (agent fetches),
`inject` (shown at a turn boundary), `idle` (wakes an idle session), `busy` (delivered
mid-task).

*(Builder-check call)* The cluster runs **one replica** in the MVP (Postgres is for
durability), so events use an in-process broadcast. Events are also written to an
outbox table so Postgres LISTEN/NOTIFY can be added later for several replicas.

Server side: `GET /v1/events?session=…` SSE stream of message/task events for that
session (its agent inbox, its project, tasks it owns or created).

| Harness | Busy delivery | Idle wake | Mechanism |
|---|---|---|---|
| Claude Code | `coord hook` on UserPromptSubmit / PostToolUse / Stop injects unread summaries | Plugin monitor running `coord watch`, which prints one line per event | Plugin with hooks + monitor; installed by `coord adapters install` |
| Codex | Steer the running turn | Start a new turn | `coord codex` launches Codex with a local app-server and uses its turn/start and turn/steer controls |
| OpenCode | Send to the session | Prompt the idle session | `coord opencode` launches OpenCode with its local server and drives the identified session |

Rules for every adapter:

- **Fail open.** Missing token, unreachable server, no project → log once, exit 0. Never
  block a turn. Stop-loop guards run first.
- Injected text is short (sender, subject, task, one-line preview) and labelled as peer
  data; the agent fetches the full body through MCP if it chooses to act.
- Urgent messages may ask the harness to continue at Stop; routine ones never do.
- Adapters never approve tool calls or change permission modes.

## 11. MCP tools

All identity comes from the bridge or headers; no tool takes a token or agent id.

| Tool | Purpose |
|---|---|
| `whoami` | Project, agent, session, leases, unread count |
| `project_list` | Projects with display names, live agent counts, open task counts |
| `agent_list` | Agents and sessions, presence, current task (filter by project) |
| `send_message` | To project / agent / session / task thread; subject, body, importance |
| `fetch_inbox` | Unacked messages, filter by project / task / importance |
| `ack_messages` | Acknowledge by ids |
| `search_messages` | Full-text over subject + body (FTS5 on SQLite, tsvector on Postgres) |
| `task_create` | Title, body, project, optional parent, participating projects, eligibility |
| `task_list` | Filter by project, status, owner, parent |
| `task_get` | Task, children, thread |
| `task_claim` | Claim a specific task, or `next` eligible in the caller's project |
| `task_post` | Reply in the task thread |
| `task_update` | Set `blocked` (reason), `review`, or edit title/body (creator/owner) |
| `task_renew` | Extend the lease |
| `task_release` | Give up ownership, back to `open`, with a note |
| `task_complete` | Mark done with summary and evidence (changed files, tests, links) |

Tool descriptions should be short and state the identity rule, so agents don't invent ids.

## 12. Web UI

Server-rendered (askama + htmx), live via SSE. Light and dark. Keyboard navigation
(`j`/`k`, `/` search) borrowed from Agent Mail. No onboarding tour.

- **Overview**: counters (hosts online, sessions live, open tasks, unread/urgent
  messages) and live agent cards (project, host, harness, current task, last seen) —
  borrowed from Concord.
- **Board**: kanban columns Open · Claimed · Blocked · Review · Done. Filter by
  project(s). Parent cards show progress (`3/5`) and per-project owners and expand to
  children:

  ```
  ┌ Rotate DB creds across stack ──── Claimed: claude@mac ┐
  │ ███████░░░ 3/5   IaC ▪▪  Nimbus ▪▪▪                   │
  │  ✓ IaC: update Pulumi secret       codex@cac-1        │
  │  ● Nimbus: roll CNPG users         claude@mbp         │
  │  ○ Nimbus: update ExternalSecret   (open)             │
  └───────────────────────────────────────────────────────┘
  ```

- **Task page**: the thread, lease state and history (claims, expiries, releases),
  children, evidence.
- **Inbox**: unified across projects, split view, receipts, search.
- **Projects / Agents**: display names (editable), tokens and last use.
- **Human actions, in the MVP** (Mario's decision): create a task, post into a thread or
  to an agent as `human`, force-release a lease, cancel a task. `human` is a first-class
  sender and recipient: agents can address messages to Mario's inbox (`to: human`).

## 13. Phasing

1. **MVP**: server, both stores with the shared contract test suite, bridge, MCP tools,
   tasks + children + leases, messages, web UI (overview, board, task page, inbox),
   Claude Code hook-based busy delivery, local mode, multi-arch container image, Helm
   chart (SQLite or external Postgres, one replica), and CI publishing image and chart
   to GHCR (Mario's decision: packaging is in the MVP).
2. **Wake**: Claude Code idle monitor, Codex adapter, OpenCode adapter,
   `coord adapters install|status|doctor`.
3. **Cluster + migration**: Pulumi stack installing the Helm chart with Postgres, then retire Agent Mail hooks
   and Concord per project. *(Builder-check call)* No history is imported from either.
4. Later: OAuth for claude.ai connectors; if needed, file reservations, team users,
   Copilot/Gemini adapters, several server replicas.

## 14. Testing

- One contract test suite run against both stores, including a concurrency test: N
  tasks × M concurrent claimers → exactly one owner each; lease expiry and takeover;
  parent auto-review when children finish.
- MCP integration tests through the bridge (identity injection, no id parameters).
- Adapter tests for fail-open behaviour: no token, server down, no git, Stop re-entry.

## 15. Left to the builder

- Exact table layouts, indexes, and migration tooling (two migration folders, kept in step).
- Event payload shapes on `/v1/events`.
- How each harness exposes its own session/pid details to the bridge and hooks; verify
  per harness and per version.
- Exact Codex app-server and OpenCode server APIs used by the adapters.
- UI styling, within the constraints above.
- Container image, Helm chart vs plain manifests, and Pulumi stack layout.
