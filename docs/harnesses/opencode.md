# OpenCode

OpenCode connects to Axle through the stdio bridge and gets all [18 tools](../mcp-tools.md)
plus a short instructions block. It has no shell-hook system, so there is no wake-up yet:
the agent sees new messages only when it calls `fetch_inbox` or `whoami`.

## Set up

```sh
axl install --harness opencode
```

Run it with no `--harness` to configure every harness it finds. It adds the `mcp.axle` local
server to `~/.config/opencode/opencode.jsonc` (or `opencode.json`) and the instructions
block to `~/.config/opencode/AGENTS.md`, keeping your comments and key order. If it cannot
edit the file safely it prints the exact snippet to add by hand instead. A
`<file>.axl-backup-<UTC timestamp>` copy is saved first, and `axl uninstall --harness
opencode` reverses it. See the [CLI reference](../cli.md#axl-install).

Start OpenCode in a repository and ask it to call `whoami`. The agent shows up as
`<project>/<host>/opencode`.

## Works today

- All MCP tools: messages, tasks, claims, leases, the plan gate.
- Presence and lease renewal through the bridge's heartbeat.
- Project detection from `AXL_PROJECT`, a `.axl` file or the git origin.

## Manual setup

Create a token on the server and put it in `~/.axl/token` on this machine (`axl login <url>`
does both steps, or see [getting started](../getting-started.md)). Then add the bridge as a
local MCP server in `opencode.json` (in the project, or in `~/.config/opencode/` for every
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

Add the same short instructions as for Codex to your `AGENTS.md`; see
[Codex: manual setup](codex.md#manual-setup).

## On the roadmap

An OpenCode wake adapter (`axl opencode`), plus `axl adapters install | status | doctor`.
See [roadmap: wake adapters](../../ROADMAP.md#4-wake-adapters).
