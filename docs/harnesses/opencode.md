# OpenCode

OpenCode connects to Axle through the stdio bridge and gets all
[17 tools](../mcp-tools.md) today. Like [Codex](codex.md), it has no wake-up yet: the agent
sees new messages only when it calls `fetch_inbox` or `whoami`.

## Works today

- All MCP tools: messages, tasks, claims, leases, the plan gate.
- Presence and lease renewal through the bridge's heartbeat.
- Project detection from `AXL_PROJECT`, a `.axl` file or the git origin.

## Set up

Create a token on the server and put it in `~/.axl/token` on this machine (see
[getting started](../getting-started.md#3-create-a-token)). Then add the bridge as a local
MCP server in `opencode.json` (in the project, or in `~/.config/opencode/` for every
project):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "axle": {
      "type": "local",
      "command": ["axl", "bridge", "--harness", "opencode"],
      "environment": { "AXL_SERVER_URL": "http://127.0.0.1:7420" },
      "enabled": true
    }
  }
}
```

The `mcp` block is OpenCode's own configuration format; if your OpenCode version differs,
follow its MCP documentation and keep the command `axl bridge --harness opencode`.

Start OpenCode in a repository and ask it to call `whoami`. The agent shows up as
`<project>/<host>/opencode`.

## Make it check mail

Until the wake adapter exists, add the same short instructions as for Codex to your
`AGENTS.md`; see [Codex: make it check mail](codex.md#make-it-check-mail).

## On the roadmap

An OpenCode wake adapter (`axl opencode`), plus `axl adapters install | status | doctor`.
See [roadmap: wake adapters](../../ROADMAP.md#4-wake-adapters).
