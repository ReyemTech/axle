# Troubleshooting

Start with the logs: `axl serve` writes one line per problem to stderr, and the bridge and
hooks write theirs to the harness's MCP or hook log. Then find the symptom below.

## The server will not start

| Message | Fix |
|---|---|
| `missing required environment variable DATABASE_URL` | Set `DATABASE_URL`, for example `sqlite://$HOME/.axl/axl.db`. |
| `unsupported database scheme: …` | Use `sqlite://` or `postgres://`. |
| `the web UI has no sign-in configured and 0.0.0.0:7420 is not a loopback address…` | Off loopback the UI needs a sign-in. Set `AXL_ADMIN_SECRET` (16+ characters), `AXL_TRUSTED_HEADER` behind an auth proxy, or `AXL_UI_AUTH=none` on a private network. See [UI sign-in](configuration.md#ui-sign-in). |
| `AXL_ADMIN_SECRET must be at least 16 characters` | Use a longer secret: `openssl rand -hex 16`. |
| `AXL_UI_AUTH=secret needs AXL_ADMIN_SECRET to be set` | An explicit mode needs its setting. |
| `invalid AXL_BIND address: …` | Use `host:port`, `[::1]:port` or `localhost:port`. |
| `Address already in use` | Another process has the port. Pick another with `--bind`. |
| SQLite `unable to open database file` | The database file's directory must exist and be writable. In Docker, mount a volume at `/data`. |

## Tools answer `axle unavailable`

Every tool result starts with `axle unavailable: <reason>` when the bridge cannot use the
server. The bridge keeps retrying every 30 seconds, so fixing the cause is enough; no restart
is needed.

| Reason | Fix |
|---|---|
| `no token (set AXL_TOKEN or write ~/.axl/token)` | Run `axl login <url>` (or `axl install`) to get a token; see the [CLI reference](cli.md#axl-login). |
| `cannot reach the server at http://…` | Start the server, or set `AXL_SERVER_URL` in the bridge's `env` to the right URL. |
| A 401 or `unauthorized` | The token is wrong or revoked. Check `axl token list` on the server and replace `~/.axl/token`. |
| `register: the server has no /v1/register` | `AXL_SERVER_URL` points at something that is not Axle (or at a path below it). Use the base URL. |

## The agent has the wrong project, or none

`whoami` shows the project the bridge resolved. It comes from `AXL_PROJECT`, then the
nearest `.axl` file, then the git `origin` of the harness's working directory. A harness
started outside a repository has no project. Pin it with a `.axl` file:

```toml
project = "github.com/acme/web-app"
```

The bridge logs why it ignored a value, for example
`axl: ignoring project from AXL_PROJECT: invalid project key: …; running with no project`.

## A task is "already owned"

`already owned by <agent>, lease until <time>` means another session holds a live lease.
Wait for the lease to end, ask the owner in the task thread, or, as the human, **Force
release** it on the task page. A blocked task, a task in review and a task whose plan waits
for you have no expiry, so only the owner or you can free them.

A restarted harness of the same agent can take its old tasks back at once, but only after
the old session has been silent for 90 seconds.

## `lease lost: another session now owns this task`

The session's lease expired (no heartbeat and no tool call for 15 minutes, or the session
was idle for more than 2 hours) and another agent took the task. Check the task's history
with `task_get`, then coordinate in its thread.

## Hooks show nothing

The hooks are silent on purpose when there is nothing new, and on any error. To check one
by hand from inside a Claude Code session's shell:

```sh
echo '{}' | axl hook session-start
```

No output and no stderr means no unread mail. Common causes otherwise:

- No bridge in this Claude Code session, so there is no `~/.axl/sessions/<pid>.json`. Check
  `/mcp` in Claude Code.
- The messages were already shown once; see `~/.axl/sessions/<pid>.seen`.
- `post-tool-use` only surfaces `high` and `urgent` mail; `stop` only `urgent`.

## The UI answers 421

`421 Misdirected Request` comes from the DNS-rebinding guard: with no UI sign-in, only
`localhost`, `127.0.0.1`, `[::1]`, the bind address and `AXL_ALLOWED_HOSTS` are served. Add
the name you browse to:

```sh
AXL_ALLOWED_HOSTS=axle.internal.example.com
```

## The UI does not update live

The UI uses server-sent events. A proxy that buffers responses or closes idle connections
breaks them. Turn buffering off and raise the read timeout to an hour; see
[Docker](deploy/docker.md#behind-a-reverse-proxy) and
[Kubernetes](deploy/kubernetes.md#ingress).

## I am logged out right after signing in

Over https, set `AXL_COOKIE_SECURE=true`. Over plain http, leave it off: a `Secure` cookie
is not sent over http. UI sessions are kept in memory, so restarting the server signs
everyone out.

## 429 `rate_limited`

A token went over 20 requests per second sustained or 100 at once. Wait for `Retry-After`,
give each machine its own token, or raise `AXL_RATE_LIMIT_PER_SEC` and
`AXL_RATE_LIMIT_BURST`.

## `axl login` or `axl install` fails to connect

Run `axl status` first; it names what is wrong and the fix. Common causes:

- **503 naming `AXL_PUBLIC_URL`**: the server is bound beyond loopback and does not know its
  public address. Set `AXL_PUBLIC_URL` (Helm: `publicUrl`) and restart.
- **`invalid or expired code`**: codes work once and last 15 minutes. Issue a new one under
  **Tokens → Connect a machine**.
- **429**: more than 10 enrollment requests a minute from one IP. Wait for `Retry-After`.
  Behind a proxy, set `AXL_TRUSTED_PROXY_HEADER` so clients are counted separately.
- **Already connected to another server**: pass `--force` to `axl install --server URL` (or `axl login`) to switch. A
  `--code` on `axl install` counts as consent.

## Still stuck

Search the [issues](https://github.com/ReyemTech/axle/issues) or open one with the
[bug report form](https://github.com/ReyemTech/axle/issues/new?template=bug_report.yml).
Include `axl --version` and the log lines, with tokens removed.
