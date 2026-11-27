# Command-line reference

`axl` is one binary: the server, the bridge and hooks harnesses spawn, and the commands that
set up a machine. This page covers the setup commands. For `serve`, `bridge` and `hook` see
[configuration](configuration.md) and the [harness guides](harnesses/claude-code.md); for
`token`, `project` and `migrate` run `axl <command> --help`.

| Command | What it does |
|---|---|
| [`axl install`](#axl-install) | Connect this machine and configure every detected harness |
| [`axl login`](#axl-login) | Connect to a server, without touching any harness |
| [`axl logout`](#axl-logout) | Forget this machine's token |
| [`axl uninstall`](#axl-uninstall) | Undo `axl install` |
| [`axl status`](#axl-status) | Show what is connected and what is not |
| [`axl service`](#axl-service) | Run the server in the background on this machine |

## `axl install`

```sh
axl install [--server URL] [--code CODE] [--harness NAME]... [--local] [--dry-run] [--yes]
```

Sets up this machine in one step: gets a token (logging in first when there is none), then
adds the bridge, hooks and a short instructions block to each harness it finds.

| Flag | Meaning |
|---|---|
| `--server URL` | Connect to this server, logging in first if the machine has no token for it. |
| `--code CODE` | A one-time code from the web UI (`XXXXX-XXXXX`), used with `--server`. Skips the browser. |
| `--harness NAME` | Only configure this harness: `claude-code`, `codex` or `opencode`. Repeat for several. |
| `--local` | Run the server on this machine as a [background service](#axl-service). |
| `--dry-run` | Print the planned changes as a diff and write nothing. |
| `--yes` | Ask nothing. With no configured server and no `--server`, use local mode. |

With no server configured and no `--server`, an interactive terminal is asked "Run Axle on
this machine, or connect to a server?". A non-interactive run starts the local service
unless you pass `--server`.

What it changes, per harness:

| Harness | Bridge | Hooks | Where |
|---|---|---|---|
| Claude Code | yes | yes | An Axle-owned plugin under `~/.axl/integrations/claude-code/`, linked into `~/.claude/skills/axle` (user-scope entries in `~/.claude.json` and `~/.claude/settings.json` as the fallback) |
| Codex | yes | yes: SessionStart, PostToolUse, Stop | `~/.codex/config.toml` |
| OpenCode | yes | none: OpenCode has no shell hooks | `~/.config/opencode/` |

Each harness also gets a short block of instructions (`CLAUDE.md` or `AGENTS.md`) telling
the agent how to use the Axle tools. Codex asks you to trust new hooks the next time it starts; review them with `/hooks`.
Existing hand-made `axle` entries are adopted rather than duplicated.

Before it edits a file, `axl install` saves a copy as `<file>.axl-backup-<UTC timestamp>`
and records every change in `~/.axl/install-manifest.json`, which `axl uninstall` reads.
Running it again is safe: it changes only what differs. Use `--dry-run` to see the diff
first.

## `axl login`

```sh
axl login <url> [--code CODE] [--no-browser] [--force]
```

Connects this machine to a server and saves the token. Use it when you want the connection
without configuring harnesses; `axl install` calls it for you.

| Flag | Meaning |
|---|---|
| `<url>` | The server, for example `https://axle.example.com`. Default: `AXL_SERVER_URL`, then `~/.axl/config.toml`, then `http://127.0.0.1:7420`. |
| `--code CODE` | Redeem a one-time code from **Tokens → Connect a machine**. |
| `--no-browser` | Print the approval URL instead of opening a browser. |
| `--force` | Switch to a different server than the one this machine is already connected to. |

Without `--code`, login uses the device flow: the CLI prints a short code and opens the
server's approval page; you approve it in the web UI and the CLI picks the token up. See
[enrollment](security.md#enrollment) for how both paths work.

## `axl logout`

```sh
axl logout [--revoke]
```

Deletes `~/.axl/token`. With `--revoke` it also revokes that token on the server first, so
it stops working everywhere.

## `axl uninstall`

```sh
axl uninstall [--harness NAME]... [--service [--purge]] [--revoke]
```

Removes what `axl install` added to each harness, using the manifest it kept. Edits you made
to those files are left alone.

| Flag | Meaning |
|---|---|
| `--harness NAME` | Only undo this harness (repeatable). |
| `--service` | Also stop and remove the local background service. The database is kept. |
| `--purge` | With `--service`: also delete the local database. |
| `--revoke` | Revoke this machine's token on the server and forget it. |

## `axl status`

```sh
axl status
```

Prints one line each for the server, the token, the local service (when there is one) and
every harness, with a `fix:` line under anything that needs attention. It is the first
thing to run when an agent cannot reach Axle.

## `axl service`

```sh
axl service install [--port PORT]
axl service uninstall | start | stop | restart | status | logs
```

Runs the server in the background on this machine: a launchd agent
(`com.reyemtech.axle`) on macOS, a systemd user unit (`axle.service`) on Linux. It starts at
login and listens on loopback, port 7420 unless `--port` says otherwise (the port is
remembered for later installs).

| Subcommand | Meaning |
|---|---|
| `install` | Install the service and start it. |
| `uninstall` | Stop and remove the service. The database is kept. |
| `start`, `stop`, `restart` | Control it. |
| `status` | Show whether it is running. |
| `logs` | Print the end of `~/.axl/logs/axl.log`. |

## Files on the client

| Path | Contents |
|---|---|
| `~/.axl/config.toml` | `server_url` and `port`, written by `login`, `install` and `service`. |
| `~/.axl/token` | This machine's token, mode 0600. |
| `~/.axl/install-manifest.json` | What `axl install` changed, for `axl uninstall`. |
| `<file>.axl-backup-<UTC timestamp>` | A copy of a harness file from before it was edited. |
| `~/.axl/logs/axl.log` | The local service's log. |

`AXL_SERVER_URL` and `AXL_TOKEN`, when set, win over the files.
