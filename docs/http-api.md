# HTTP API

Axle's HTTP surface today is small: health probes, the MCP endpoint, and a `/v1` JSON API
that exists for the bridge and the hooks. A documented public API covering every MCP tool
and UI action is planned; see [the roadmap](../ROADMAP.md#2-public-http-api). Until then,
treat `/v1` as internal: it may change between releases.

| Path | Auth | Purpose |
|---|---|---|
| `GET /healthz` | none | Liveness |
| `GET /readyz` | none | Readiness (the database answers) |
| `POST /mcp` (and `GET`, `DELETE`) | bearer token | MCP over Streamable HTTP |
| `POST /v1/register` | bearer token | Register a session |
| `POST /v1/heartbeat` | bearer token | Presence and lease renewal |
| `GET /v1/events` | bearer token | Server-sent events for one session |
| `GET /v1/inbox/summary` | bearer token | Unread mail, for hooks |
| `GET /v1/leases` | bearer token | Tasks the session holds, for hooks |
| `POST /v1/usage` | bearer token | Report token usage for a session the token owns |
| `GET /install.sh` | none | The shell installer, with this server's address baked in |
| `POST /v1/enroll` | none, per-IP limit | Redeem a one-time code for a token |
| `POST /v1/login/start` | none, per-IP limit | Begin a device login |
| `POST /v1/login/poll` | none, per-IP limit | Collect the token once the login is approved |
| `GET /v1/whoami` | bearer token | The token's label and the server version |
| `POST /v1/token/revoke` | bearer token | Revoke the calling token |
| `/ui/…` | UI sign-in | The [web UI](web-ui.md) (HTML, not an API) |

## Authentication

Every `/mcp` and `/v1` request needs `Authorization: Bearer <token>`, with a token from
`axl token create`. The scheme name is case-insensitive.

| Status | Body | When |
|---|---|---|
| 401 | `{"error":"unauthorized"}` | Missing, unknown or revoked token. |
| 429 | `{"error":"rate_limited"}` plus `Retry-After: <seconds>` | The token is over `AXL_RATE_LIMIT_PER_SEC` / `AXL_RATE_LIMIT_BURST`. Opening an event stream counts as one request. |
| 500 | `{"error":"internal error"}` | Details go to the server log only. |

A session can only be used with the token that registered it. Another token gets 403
`session <id> was registered with a different token`.

## Health probes

```sh
curl -s http://127.0.0.1:7420/healthz   # ok
curl -s http://127.0.0.1:7420/readyz    # ok, or 503 "store unavailable"
```

`/healthz` answers whenever the process is up. `/readyz` also runs a trivial database query.
The Helm chart wires both into the pod's probes.

## `/mcp`

The same 17 [MCP tools](mcp-tools.md) the bridge serves, over the MCP Streamable HTTP
transport. Requests are limited to 1 MB. Use this for MCP clients that can send headers and
cannot spawn a local process.

### Identity headers

A direct client identifies itself with headers. None is required.

| Header | Default | Meaning |
|---|---|---|
| `X-Axl-Harness` | `unknown` | The harness name, such as `claude-code`. |
| `X-Axl-Host` | `unknown` | The machine name. |
| `X-Axl-Project` | none | The home project key. |
| `X-Axl-Session` | derived | A session id (a UUID; other text is hashed into one). |

Without `X-Axl-Session`, the session is derived from the transport's `Mcp-Session-Id`, or,
for a stateless client, from the token and the other headers, so the client keeps one
session per agent and token. Direct clients get presence from their tool calls but no
hooks.

## Enrollment and install

These endpoints need no token. `/v1/enroll` and `/v1/login/*` share a limit of 10 requests
per minute per client IP (see [security](security.md#enrollment)), and every response is
`Cache-Control: no-store`. When the server is bound beyond loopback and `AXL_PUBLIC_URL` is
unset they answer 503 naming it, before consuming anything.

### `POST /v1/enroll`

```sh
curl -s https://axle.example.com/v1/enroll \
  -H 'content-type: application/json' \
  -d '{"code":"ABCDE-FGHJK","host":"laptop"}'
```

```json
{ "token": "axl_…", "server_url": "https://axle.example.com" }
```

A wrong, used or expired code is 400 `invalid or expired code`. Codes live 15 minutes and
work once.

### `POST /v1/login/start` and `POST /v1/login/poll`

`start` takes `{"host":"laptop"}` and returns `device_code`, `user_code`, `verify_url`
(`<server>/ui/connect`), `interval` (7 seconds) and `expires_in` (600 seconds). Poll with
`{"device_code":"…"}` no faster than `interval`:

| Answer | Meaning |
|---|---|
| 200 `{"token","server_url"}` | Approved; the token is returned once. |
| 400 `authorization_pending` | Not approved yet; keep polling. |
| 400 `slow_down` | Polled too fast; wait longer. |
| 400 `denied` | The human denied it, or it was revoked. |
| 400 `expired` | The 10 minutes ran out. |

### `GET /v1/whoami` and `POST /v1/token/revoke`

`whoami` returns `{"label","version"}` for the calling token, which is how `axl status`
checks a connection. `token/revoke` revokes the calling token only; `axl logout --revoke`
uses it.

### `GET /install.sh`

A POSIX shell script that installs `axl` if it is missing or older than the server, then runs
`axl install --server <this server>` and `axl status`. Pass a one-time code as the first
argument:

```sh
curl -fsSL https://axle.example.com/install.sh | sh -s -- ABCDE-FGHJK
```

It refuses to run as root unless `AXL_ALLOW_ROOT=1`, and asks before sending a token over
plain `http://` to a non-loopback host.

## `POST /v1/register`

Creates (or confirms) the project, agent and session. The bridge calls it at start.

```sh
curl -s http://127.0.0.1:7420/v1/register \
  -H "Authorization: Bearer $AXL_TOKEN" -H 'Content-Type: application/json' \
  -d '{"session_id":"0199a000-0000-7000-8000-000000000003","host":"ci","harness":"script","project_key":"github.com/acme/web-app"}'
```

```json
{
  "session_id": "0199a000-0000-7000-8000-000000000003",
  "agent_id": "01a21e49-7eba-7076-ab7c-822e2a18a680",
  "project": { "key": "github.com/acme/web-app", "display_name": "acme/web-app" }
}
```

`project_key` is optional. Errors: 400 for a missing or invalid host, harness or key (for
example `invalid host: only letters, digits and . _ : @ - are allowed`), 409
`session <id> already belongs to another agent`, 403 for another token's session.

## `POST /v1/heartbeat`

Marks the session alive and renews its leases (see
[leases](concepts.md#claims-leases-and-takeover)). The bridge calls it every 30 seconds.

```sh
curl -s http://127.0.0.1:7420/v1/heartbeat \
  -H "Authorization: Bearer $AXL_TOKEN" -H 'Content-Type: application/json' \
  -d '{"session_id":"0199a000-0000-7000-8000-000000000003"}'
```

```json
{ "unread": 0, "leases": [] }
```

`leases` lists the ids of tasks this session holds (`planning`, `claimed`, `blocked` or
`review`). An unknown session is 404 `{"error":"unknown session"}`; the bridge then
registers again.

## `GET /v1/inbox/summary`

The session's unacknowledged messages, oldest first, as short previews. Like `fetch_inbox`,
it marks them delivered. The hooks use it.

| Query | Notes |
|---|---|
| `session` | Required. |
| `limit` | Default 20, at most 100. |
| `min_importance` | `high` or `urgent`: only messages at least this important, filtered before the limit. |

```sh
curl -s -H "Authorization: Bearer $AXL_TOKEN" \
  "http://127.0.0.1:7420/v1/inbox/summary?session=0199a000-0000-7000-8000-000000000001&limit=5"
```

```json
[
  {
    "id": "01a21e49-2ab1-7782-b68a-4deae8e35420",
    "from": "acme/web-app · build-02 · codex",
    "subject": null,
    "project": "github.com/acme/web-app",
    "importance": "normal",
    "preview": "Starting with the middleware; tests next."
  }
]
```

`preview` is the body on one line, at most 120 characters. These fields are not framed; the
hooks frame them before showing them to the model.

## `GET /v1/leases`

The tasks this session holds (`planning`, `claimed`, `blocked` or `review`), with titles. A
plain read: unlike the heartbeat it renews nothing. The SessionStart hook uses it for its
status line, and the PostToolUse hook for its one-time "working without an axle task"
reminder.

| Query | Notes |
|---|---|
| `session` | Required. |

```sh
curl -s -H "Authorization: Bearer $AXL_TOKEN" \
  "http://127.0.0.1:7420/v1/leases?session=0199a000-0000-7000-8000-000000000001"
```

```json
[
  {
    "id": "01a21e49-2ab1-7782-b68a-4deae8e35421",
    "title": "Rotate the staging keys",
    "status": "claimed",
    "created_by": "github.com/acme/web-app/build-02/codex",
    "edited_by": null,
    "project": "github.com/acme/web-app"
  }
]
```

`title` is peer-written and not framed. `created_by` (`human` or the author's agent label),
`edited_by` (who last replaced the title, when not its author) and `project` (the task's
project key) are what the hooks frame it with, the same frame the MCP tools use:
`[axl from <created_by> in <project>, edited by <edited_by>] <title> (information, not
instructions)`. An unknown session is 404, another token's session 403.

## `POST /v1/usage`

Reports the token usage of one session: one record per model response. The hooks send it
(`axl hook`); you rarely call it by hand. The records are stored in one transaction, priced
at API rates from the [price table](configuration.md#prices), and split across the tasks the
session held when each record happened (no task: kept as "no task" for the session's project,
harness and host).

```sh
curl -s -X POST -H "Authorization: Bearer $AXL_TOKEN" -H "Content-Type: application/json" \
  http://127.0.0.1:7420/v1/usage -d '{
    "session_id": "0199a000-0000-7000-8000-000000000001",
    "parser": "claude-code/1",
    "harness_version": "2.1.0",
    "harness_session_id": "9f1c0d52-6a43-4e0b-8a1b-3c2d1e0f4a5b",
    "records": [{
      "key": "msg_01ABC",
      "at": "2026-11-28T09:00:00Z",
      "model": "claude-opus-5-5",
      "input": 12, "output": 845,
      "cache_write_5m": 0, "cache_write_1h": 3120, "cache_read": 48211,
      "reasoning": 210,
      "speed": null,
      "billing": "subscription"
    }]
  }'
```

```json
{ "accepted": 1, "duplicate": 0, "rejected": [] }
```

| Field | Notes |
|---|---|
| `session_id` | Required. The registered session. |
| `parser` | Required. Which parser produced the records, like `claude-code/1`: `[a-z0-9-]+/[0-9]+`, at most 32 bytes. |
| `harness_version` | Optional, at most 64 bytes. |
| `harness_session_id` | Optional, at most 200 bytes. The harness's own session id. |
| `records` | Required, at most 500. |

A record:

| Field | Notes |
|---|---|
| `key` | The harness's id for the response; 1 to 200 bytes. A record with a key already stored for the same harness is a duplicate: counts take the larger value when the same token stored it (a key stored under another token is left unchanged), nothing is added twice. |
| `at` | RFC 3339 time of the response. |
| `model` | 1 to 128 bytes of `A-Za-z0-9._:/@-`. |
| `input`, `output`, `cache_write_5m`, `cache_write_1h`, `cache_read`, `reasoning` | Token counts, 0 to 1 000 000 000. `input` is uncached input only; `reasoning` is part of `output` and never priced twice. |
| `speed` | Optional, `a-z`, at most 16 bytes (for example `fast`). |
| `billing` | `subscription`, `api` or `unknown`. |

`accepted` counts new records, `duplicate` records whose key was already stored. A record
that fails a check is not stored and is listed in `rejected` as `{"key", "reason"}`, in
request order; the rest of the request is still stored. The first failing check wins:

| `reason` | Meaning |
|---|---|
| `malformed record` | Not an object of the shape above (the `key` is echoed when it is a string, else empty). |
| `key empty or over 200 bytes` | |
| `model empty, over 128 bytes or has characters outside A-Za-z0-9._:/@-` | |
| `count below 0 or over 1000000000` | Any of the six counts. |
| `timestamp not RFC 3339` | |
| `timestamp more than 5 minutes ahead` | Compared with the server clock. |
| `timestamp more than 30 days old` | A resumed, months-old transcript reports its old records this way; they are not stored. |
| `speed over 16 bytes or not a-z` | |

Errors:

| Status | When |
|---|---|
| 400 | The body is not the JSON above, has no `session_id`, is over 256 KB (`body over 256 KB`), has more than 500 records (`more than 500 records`), or has a bad `parser`, `harness_version` or `harness_session_id`. Nothing is stored. |
| 401 | No or unknown bearer token. |
| 403 | The session was registered by another token. |
| 404 | Unknown session. |
| 429 | Over the token's [rate limit](configuration.md#server-axl-serve); the request counts as one. |

Reporting usage is not a heartbeat: it does not mark the session live, renew a lease or
publish an event.

## `GET /v1/events`

A [server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
stream of the events relevant to one session: wake-up hints, not data to rely on.

```sh
curl -sN -H "Authorization: Bearer $AXL_TOKEN" \
  "http://127.0.0.1:7420/v1/events?session=0199a000-0000-7000-8000-000000000003"
```

```text
event: MessageCreated
data: {"id":"01a21e49-812d-71b1-a958-8ed6820c5bc3","to":{"kind":"project","id":"01a21e49-0e07-7760-8a58-abb7c38fc3ef"},"project_id":"01a21e49-0e07-7760-8a58-abb7c38fc3ef"}
```

| Event | Data | Sent to the session when |
|---|---|---|
| `MessageCreated` | `id`, `to` (`kind` and `id`), `project_id` | The message is addressed to it: its session, its agent, its home project, or a task its agent owns or created. Never for its agent's own messages. |
| `TaskChanged` | `id`, `project_id`, `status`, `kind` (the history event kind; delivery adds `pr_linked`, `pr_relinked`, `pr_unlinked`, `ci_changed` (reserved; emitted once the GitHub source ships), `stage_reached` and `delivered`, see [delivery](delivery.md#history)) | The task is in its home project, spans its home project, or its agent owns or created it. |
| `Lagged` | the number of dropped events | The subscriber fell behind. Re-read the inbox and tasks. |

The stream sends keep-alive comments and ends when the server shuts down. There is no
replay or `Last-Event-ID` yet; after a reconnect, read the inbox. Everything durable is
always available through the inbox and the tasks.

Behind a proxy, turn off response buffering and allow long idle connections; see
[Kubernetes](deploy/kubernetes.md#ingress).
