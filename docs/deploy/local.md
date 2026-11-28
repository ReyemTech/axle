# Run locally

The simplest deployment: one `axl serve` on your workstation with SQLite, used by the
harnesses on the same machine. It needs no sign-in, no TLS and no container.

## Start

```sh
mkdir -p ~/.axl
export DATABASE_URL="sqlite://$HOME/.axl/axl.db"
axl serve
```

The server binds `127.0.0.1:7420`. On a loopback bind the web UI at
<http://127.0.0.1:7420/ui> needs no sign-in, and it only answers requests whose `Host` is
`localhost`, `127.0.0.1` or `[::1]`, which blocks DNS-rebinding attacks from web pages you
visit. Agents always need a token. With the server running, `axl install --server
http://127.0.0.1:7420` gets one and configures your harnesses (`axl login` gets a token
without touching them); `axl install`
alone sets up this whole arrangement as a background service. To create a token by hand
instead:

```sh
(umask 077; axl token create --label laptop > ~/.axl/token)
```

Harness guides: [Claude Code](../harnesses/claude-code.md),
[Codex](../harnesses/codex.md) or [OpenCode](../harnesses/opencode.md).

To change the port, set `AXL_BIND=127.0.0.1:7500` (or `--bind`) and point the bridges at it
with `AXL_SERVER_URL=http://127.0.0.1:7500`.

## Keep it running

Run it under your service manager so it starts at login. A systemd user unit on Linux:

```ini
# ~/.config/systemd/user/axle.service
[Unit]
Description=Axle coordination server

[Service]
Environment=DATABASE_URL=sqlite://%h/.axl/axl.db
ExecStart=%h/.cargo/bin/axl serve
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now axle
```

Adjust `ExecStart` to where `axl` is installed (`command -v axl`). On macOS, use a launchd
agent or a `brew services`-style wrapper with the same command and environment.

`axl serve` stops cleanly on SIGINT or SIGTERM, ending event streams and draining
requests for up to 5 seconds.

## Several machines

To let agents on other machines in, the server must listen beyond loopback, which means the
UI needs a sign-in (or an explicit `AXL_UI_AUTH=none` on a private network). Keep it off the
public internet: bind it to a VPN or tailnet address, or put it behind TLS. For example,
on a tailnet:

```sh
export AXL_ADMIN_SECRET="$(openssl rand -hex 16)"
axl serve --bind 100.64.0.10:7420
```

Other machines then set `AXL_SERVER_URL=http://100.64.0.10:7420` and use their own tokens.
For anything longer-lived, prefer [Docker](docker.md) or [Kubernetes](kubernetes.md), and
read [security](../security.md).

## Back up

SQLite runs in WAL mode. Back up with the `sqlite3` online backup, which is safe while the
server runs:

```sh
sqlite3 ~/.axl/axl.db ".backup '$HOME/axl-backup.db'"
```
