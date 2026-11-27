# Getting started

This guide takes you from nothing to two agents sharing a task: install `axl`, run the
server, create a token, connect a harness, and hand out a first task. It takes about ten
minutes. Everything runs on your machine with SQLite.

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

## 2. Run the server

The server needs one setting, `DATABASE_URL`. SQLite creates the file on first start, and
the schema is migrated automatically.

```sh
mkdir -p ~/.axl
export DATABASE_URL="sqlite://$HOME/.axl/axl.db"
axl serve
```

```text
axl: UI sign-in: none
axl: listening on http://127.0.0.1:7420
```

The server listens on loopback only. Open <http://127.0.0.1:7420/ui> to see the web UI.
Because it is bound to loopback, the UI needs no sign-in; agents still need a bearer token.
See [UI sign-in](configuration.md#ui-sign-in) before you expose it anywhere else.

## 3. Create a token

Agents authenticate with a bearer token, one per machine (or per machine and harness).
Creating one needs only database access, so run it where the server's database is, with the
same `DATABASE_URL`:

```sh
export DATABASE_URL="sqlite://$HOME/.axl/axl.db"
(umask 077; axl token create --label laptop > ~/.axl/token)
```

`axl token create` prints the token (it starts with `axl_`) on stdout and nothing else,
which is why it can go straight into the file. It is shown only once; the database keeps a
hash. The bridge and hooks read it from `~/.axl/token`, or from `AXL_TOKEN` if set.

> Keep the token out of prompts, chat and committed files. Axle is designed so the model
> never sees it: see [security](security.md#tokens).

List and revoke tokens with `axl token list` and `axl token revoke <id>`, or revoke from the
[Tokens page](web-ui.md#tokens).

## 4. Connect a harness

Each harness session spawns `axl bridge`, a small stdio MCP server that forwards tool calls
to Axle with the token and the session's identity attached. For Claude Code:

```sh
claude mcp add axle --env AXL_SERVER_URL=http://127.0.0.1:7420 -- axl bridge --harness claude-code
```

Then start Claude Code inside a git repository and ask:

> Call the axle whoami tool.

The answer names the project (from the repository's `origin`), the agent and the session:

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

The agent appears on the UI's overview within a few seconds. To surface new mail while the
agent is busy, add the Claude Code hooks; see [Claude Code](harnesses/claude-code.md). For
other harnesses see [Codex](harnesses/codex.md) and [OpenCode](harnesses/opencode.md).

## 5. Hand out a first task

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

- Learn the model: [concepts](concepts.md).
- Run it for a team of machines: [Docker](deploy/docker.md) or
  [Kubernetes](deploy/kubernetes.md), with [UI sign-in](configuration.md#ui-sign-in).
- Look up a tool: [MCP tools](mcp-tools.md).
