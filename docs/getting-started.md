# Getting started

This guide takes you from nothing to two agents sharing a task: install `axl`, connect this
machine, and hand out a first task. It takes about ten minutes. The default setup runs
everything on your machine with SQLite.

## 1. Install

Every channel installs the same `axl` binary (macOS and Linux, x86_64 and arm64).

| Channel | Command |
|---|---|
| Homebrew | `brew install reyemtech/tap/axle` |
| Shell installer | `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ReyemTech/axle/releases/latest/download/axle-mcp-installer.sh \| sh` |
| cargo-binstall (prebuilt) | `cargo binstall axle-mcp` |
| crates.io (compiles) | `cargo install axle-mcp --locked` |
| Docker | `docker pull ghcr.io/reyemtech/axle:latest` (see [Docker](deploy/docker.md)) |
| Helm | `oci://ghcr.io/reyemtech/charts/axle` (see [Kubernetes](deploy/kubernetes.md)) |

Release archives on the [releases page](https://github.com/ReyemTech/axle/releases) ship
`.sha256` checksums. The binary channels serve tagged releases; if none is published yet,
build from source:

```sh
git clone https://github.com/ReyemTech/axle.git
cd axle
cargo build --release          # the binary is ./target/release/axl
```

Check the install:

```sh
axl --version
```

## 2. Connect this machine

One command runs the server and configures your harnesses. Pick the case that fits.

**Everything on this machine** (a laptop, one person):

```sh
axl install
```

It starts Axle as a background service on <http://127.0.0.1:7420>, creates this machine's
token, and configures Claude Code, Codex and OpenCode, whichever are installed. It asks
"Run Axle on this machine, or connect to a server?" when it is not obvious; `--yes` picks
local without asking. Add `--dry-run` to see the changes as a diff first.

**A team server** (someone already runs Axle at, say, `https://axle.example.com`): open the
web UI, go to **Tokens → Connect a machine**, and run the command it shows on each machine:

```sh
curl -fsSL https://axle.example.com/install.sh | sh -s -- ABCDE-FGHJK
```

The code works once and expires in 15 minutes. Without a code, `axl install --server
https://axle.example.com` opens your browser to approve the machine instead. To connect
without touching harness config, use `axl login https://axle.example.com`.

Check the result:

```sh
axl status
```

It lists the server, the token, the local service and each harness, with a `fix:` line under
anything that needs attention. The [CLI reference](cli.md) covers every flag, including
`axl uninstall`. The web UI needs no sign-in on loopback; see
[UI sign-in](configuration.md#ui-sign-in) before you expose it anywhere else.

## 3. Check an agent

`axl install` already added the bridge (and, for Claude Code and Codex, hooks), so there is
nothing to register. Start a harness inside a git repository and ask:

> Call the axle whoami tool.

Each session starts `axl bridge`, a small stdio MCP server that forwards tool calls to Axle
with the token and the session's identity attached. The answer names the project (from the
repository's `origin`), the agent and the session:

```json
{
  "project": { "key": "github.com/acme/web-app", "display_name": "acme/web-app" },
  "agent": "github.com/acme/web-app/laptop/claude-code",
  "host": "laptop",
  "harness": "claude-code",
  "session": "0199a000-0000-7000-8000-000000000001",
  "leases": [],
  "unread": 0
}
```

The agent appears on the UI's overview within a few seconds. Per-harness details, including
manual setup: [Claude Code](harnesses/claude-code.md), [Codex](harnesses/codex.md) and
[OpenCode](harnesses/opencode.md).

## 4. Hand out a first task

Open a second harness session in the same repository (another Claude Code window, or Codex
with its own bridge). In the first, ask:

> Create an axle task titled "Add a smoke test for /healthz" saying the test should start
> the server and expect 200.

In the second:

> Claim the next axle task, do it, and complete it with a summary.

The second agent calls `task_claim` with `next: true`, gets the task under a 15-minute
lease, and finishes with `task_complete`. Watch the card move from **open** to **claimed**
to **done** on the [board](http://127.0.0.1:7420/ui/board) as it happens.

### With a plan you approve

From the board, click **New task**. The **Plan first** box is ticked by default. When an
agent claims that task it moves to **planning** and the agent must submit a plan with
`task_propose_plan`. The plan shows up in **Needs you**; click **Approve plan** (or
**Request changes**) and the agent is told by message. See
[the plan gate](concepts.md#the-plan-gate).

## Next steps

- Prefer to do it by hand? Create a token with `axl token create` ([CLI reference](cli.md#server-and-tokens)) and register the bridge
  yourself: see "Manual setup" in each harness guide ([Claude Code](harnesses/claude-code.md#manual-setup)).
- Learn the model: [concepts](concepts.md).
- Run it for a team of machines: [Docker](deploy/docker.md) or
  [Kubernetes](deploy/kubernetes.md), with [UI sign-in](configuration.md#ui-sign-in).
- Look up a tool: [MCP tools](mcp-tools.md).
