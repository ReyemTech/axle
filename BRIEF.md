# Project Brief: Shared Agent Coordination

## Goal

Build a small, self-hostable Rust MCP server that lets agents working on the same project exchange durable messages and claim work across machines and harnesses.

The desired outcome is project-level coordination: the same project may have agents on several hosts and using different harnesses. Any eligible live agent can pick up a task; if several are available, the system must ensure only one owns it at a time.

## Product shape

- Rust MCP server as the primary interface for agents.
- Minimal web UI for people to inspect projects, agents, messages, task ownership, and status.
- No local TUI.
- Central, durable service; no requirement for a per-machine coordination server.
- Project membership is the routing boundary. Agents are identified by project, host, harness, and live session.

## MVP capabilities

1. Create or join a project mailbox.
2. Register an agent session with project, host, harness, and session identity; maintain a heartbeat/presence record.
3. Send and fetch durable project messages.
4. Mark work as claimable and let an eligible agent atomically claim it.
5. Represent ownership with a renewable lease so abandoned work becomes claimable after expiry.
6. Complete, release, or report a blocked task; preserve message and task history.
7. Expose the above through MCP tools and a small read-oriented web UI.

## Coordination model to evaluate

Use an atomic server-side claim plus a time-limited lease and heartbeat. Presence is advisory; the claim is authoritative. If a claimant disconnects, the lease expires and another agent can claim the work. Keep the inbox durable so agents that are offline do not lose messages.

Do not make the first version depend on harness-specific wake/steer APIs. Agents can poll or fetch messages through MCP. A later adapter can wake an idle Codex or Claude session, while this service remains the source of truth for messages and claims.

## Important open product decision

Should ordinary project messages remain conversational, with only explicitly marked tasks eligible for claiming, or should every unaddressed message be claimable work? Prefer explicit task marking for the MVP unless product discussion decides otherwise; this reduces accidental work pickup and coordination noise.

## Initial non-goals

- Reimplementing a full agent harness or terminal UI.
- Automatically launching agents or executing arbitrary messages without harness approval.
- Complex workflow automation, scheduling, or cross-project routing.
- Replacing project-specific coding instructions or access controls.

## Suggested implementation starting point

Before implementation, inspect the current Rust MCP ecosystem and existing Agent Mail / Concord workflows for protocol and UX lessons. Then propose a compact design covering authentication, project membership, persistence, message/task schema, lease recovery, MCP tools, web UI, deployment, and security boundaries. Keep harness adapters as a separate future layer.

## Success criteria

- Agents on different hosts and harnesses can discover the same project mailbox.
- Messages survive restarts and are delivered at least once without losing history.
- Concurrent claim attempts result in exactly one current owner.
- Expired leases permit recovery without deleting task history.
- A person can see who is present, what work is open, and who owns it in the web UI.
