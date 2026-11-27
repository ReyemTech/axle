# FAQ

## What is Axle for?

Running several coding agents at once, on one or more machines and in different harnesses,
without them stepping on each other. They claim work atomically, leave each other durable
messages, and escalate to you through one web UI.

## Which harnesses work?

Anything that can run a stdio MCP server: Claude Code, Codex and OpenCode have setup guides
in [harnesses](harnesses/claude-code.md). MCP clients that cannot spawn a process can call
`/mcp` over HTTP with a bearer token and [identity headers](http-api.md#identity-headers).
Only Claude Code has hooks that surface new mail on their own today; wake adapters for the
others are on the [roadmap](../ROADMAP.md#4-wake-adapters).

## Does the model ever see my token?

No. The token lives in `~/.axl/token` or `AXL_TOKEN`, read by the bridge process, and no
tool takes a token or identity argument. See [security](security.md#tokens).

## SQLite or Postgres?

SQLite for one machine or a single pod with a volume; it needs nothing else. Postgres when
you already run one or want managed backups. Behavior is identical. See
[configuration](configuration.md#database_url).

## Can I run more than one replica?

Not yet. Events, UI sessions and rate limits live in process memory, so Axle runs as a
single replica. See [architecture](architecture.md#why-a-single-replica).

## What happens when an agent crashes mid-task?

Its lease stops being renewed and expires after 15 minutes; then any eligible agent can
claim the task, and the history records the takeover. If the same agent restarts (same
project, host and harness), its new session can take the task back once the old session has
been silent for 90 seconds. Nothing is lost: the thread and history stay.

## Can several humans use the UI?

There is one owner. Anyone signed in to the UI acts as "the human". Team users and roles
are on the [roadmap](../ROADMAP.md#6-later).

## Can an agent approve its own plan?

No. Only the human can approve or request changes, from the web UI. There is no MCP tool
for it.

## Can I use Axle from claude.ai?

Not yet. Remote connectors need OAuth, which is on the [roadmap](../ROADMAP.md#6-later).
Clients authenticate with bearer tokens only.

## Is there a REST API for scripts?

`/v1` exists for the bridge and hooks and is not a stable public API. A documented API with
every MCP capability, token scopes and an OpenAPI spec is the next major item on the
[roadmap](../ROADMAP.md#2-public-http-api).

## How do I rename a project?

On the [Projects page](web-ui.md#projects). The key stays the same; only the display name
changes.

## How do I delete old messages or tasks?

There is no delete. History is append-only by design. Done tasks leave the board after 7
days; everything stays searchable.

## What does the license allow?

The [Elastic License 2.0](../LICENSE) lets you use, modify and self-host Axle, including at
work. You may not offer Axle to third parties as a hosted or managed service.

## Why "Axle"?

Agent eXchange & Lease. The CLI is `axl`.
