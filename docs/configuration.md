# Configuration

Axle is configured with environment variables. The server reads them at start; the bridge
and hooks read theirs each time they start. `axl serve` flags override the matching
variables.

## Server (`axl serve`)

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | required | Database to use: `sqlite://…` or `postgres://…` (see [below](#database_url)). Also read by `axl migrate` and `axl token`. |
| `AXL_BIND` | `127.0.0.1:7420` | Listen address: a socket address such as `0.0.0.0:7420` or `[::1]:7420`, or `localhost:<port>`. `--bind` overrides it. |
| `AXL_UI_AUTH` | `auto` | How the human signs in to the web UI: `auto`, `none`, `secret` or `trusted-header`. See [UI sign-in](#ui-sign-in). |
| `AXL_ADMIN_SECRET` | unset | The UI sign-in secret, at least 16 characters (surrounding spaces do not count). Ignored when `AXL_UI_AUTH=none`. |
| `AXL_TRUSTED_HEADER` | unset | A request header set by an authenticating proxy (for example `X-Auth-Request-Email`); requests that carry it are signed in. `--trusted-header` overrides it. |
| `AXL_ALLOWED_HOSTS` | none | Extra comma-separated `Host` names the UI answers to when it has no sign-in, ports ignored. `localhost`, `127.0.0.1`, `[::1]` and the bind address are always allowed. |
| `AXL_COOKIE_SECURE` | `false` | Mark the UI session cookie `Secure`. Set it to `true` whenever the UI is served over https. Accepts `true`, `false`, `1`, `0`. |
| `AXL_PUBLIC_URL` | unset | The address clients reach this server at, such as `https://axle.example.com` (scheme and host, no path). Required for `/install.sh`, the **Connect a machine** commands and enrollment when the server is bound beyond loopback; without it they answer 503. On loopback the bind address is used. |
| `AXL_TRUSTED_PROXY_HEADER` | unset | A header your reverse proxy sets to the client IP, such as `X-Forwarded-For`. Its right-most entry (which must parse as an IP) keys the 10-per-minute limit on `/v1/enroll` and `/v1/login/*`. Unset: the socket address is used. |
| `AXL_RATE_LIMIT_PER_SEC` | `20` | Sustained requests per second, per agent token. Must be a positive whole number. |
| `AXL_RATE_LIMIT_BURST` | `100` | Burst size, per agent token. Must be a positive whole number. |
| `AXL_DONE_AUTO_ACK_DAYS` | `7` | Days a finished task stays on the board's Done column before it counts as auto-acked. A positive whole number. |
| `AXL_PRICES_FILE` | unset | A TOML file of API prices merged over the built-in table (see [Prices](#prices)). A file that cannot be read or parsed stops the server at start. Blank means unset. |

Invalid values stop the server at start with a message naming the variable, for example
`AXL_RATE_LIMIT_BURST must be a positive whole number, got "0"`.

### Fixed in this release

These are not configurable yet:

| Setting | Value |
|---|---|
| Task lease | 15 minutes |
| Heartbeat renewal stops after no tool call for | 2 hours |
| A session is stale after no heartbeat for | 90 seconds |
| Bridge heartbeat interval | 30 seconds |
| Message or task body | 64 KB |
| Subject or task title | 200 characters |
| UI session lifetime | 12 hours |
| UI login attempts | 10 per minute per client IP |
| Enrollment and device-login requests | 10 per minute per client IP |
| One-time code lifetime | 15 minutes, single use |
| Device login lifetime | 10 minutes, polled every 7 seconds |

### Prices

The server prices each reported usage record at API rates, in USD per million tokens, from a
built-in table (version printed at start: `axl: prices: 2026-11-28, 24 models`). A model the
table does not know gets no price: it shows its tokens and "no price". Point
`AXL_PRICES_FILE` at a file to add models or change prices; the start-up line then ends with
`(+ <file>)`.

```toml
version = "2026-12-01"          # required; becomes part of each record's price_ref

[[model]]
id = "claude-opus-5-5"          # required; matches this exact model id
match = ["claude-opus-5-5-*"]   # optional globs; * matches any run of characters
input = 4.00                    # required, USD per million tokens
output = 20.00                  # required
cache_write_5m = 5.00           # optional; defaults to input
cache_write_1h = 8.00           # optional; defaults to cache_write_5m
cache_read = 0.20               # optional; defaults to input
fast = { input = 8.00, output = 40.00 }   # optional; prices for records with speed "fast"
long_context = { above = 200000, input = 8.00, output = 30.00 }   # optional
```

- **Merge by `id`.** An entry whose `id` is already in the built-in table replaces it
  entirely; a new `id` is added. Entries you leave out keep the built-in price.
- **Matching.** A record's model is matched against exact `id`s first, then against the
  `match` globs, the longest glob winning. An override glob of the same length as a built-in
  one loses the tie, because built-ins are listed first: give the entry an exact `id` or a
  longer glob.
- **`fast` and `long_context`** overlay only the prices they name. `long_context.above`
  compares the record's input plus cache tokens (`input`, both cache writes and `cache_read`).
- **`reasoning` tokens are never priced** on their own: they are already part of `output`.
- **`cost_micros`** is `round(sum(tokens × price))` in micro-USD, stored with the record.
- **`price_ref`** records where the price came from, for example
  `builtin:2026-11-28#claude-opus-5-5` or `file:2026-12-01#claude-opus-5-5`.
- **A bad file stops the server at start**, naming the file and line (an unknown key, a
  negative price, a duplicate `id`, a missing `version`). Nothing is half-loaded.

Changing prices does not change the cost of records already stored.

## Bridge and hooks (`axl bridge`, `axl hook`)

| Variable | Default | Meaning |
|---|---|---|
| `AXL_TOKEN` | read from `~/.axl/token` | The agent's bearer token. Prefer the file (mode 0600), so the token is not in the harness's environment or config. |
| `AXL_SERVER_URL` | `http://127.0.0.1:7420` | The server's base URL. Used by the bridge; hooks use the URL the bridge recorded in its session file. |
| `AXL_PROJECT` | from `.axl` or git `origin` | Pin the session's project key. See [projects and identity](concepts.md#projects-and-identity). |

`axl login` and `axl install` write `~/.axl/config.toml` (`server_url`, `port`) and
`~/.axl/token`; see the [CLI reference](cli.md#files-on-the-client). `AXL_SERVER_URL` and
`AXL_TOKEN` win over both files.

The bridge reads `~/.axl/token` again on each retry, so a token written after the harness
started is picked up within 30 seconds.

## `DATABASE_URL`

| Backend | Example | Notes |
|---|---|---|
| SQLite, relative path | `sqlite://./axl.db` | Relative to the server's working directory. |
| SQLite, absolute path | `sqlite:///var/lib/axle/axl.db` | Three slashes before an absolute path. The file is created if missing; its directory must exist. Runs in WAL mode with one writer. |
| Postgres | `postgres://axle:password@db.example.com:5432/axle` | `postgresql://` works too. Standard libpq-style query parameters such as `?sslmode=require` are accepted. |

The schema is migrated on every start, so `axl migrate` is only needed to migrate without
starting the server. Any other scheme is refused: `unsupported database scheme: mysql`.

SQLite suits one machine or a single pod with a volume. Use Postgres when you already run
one, or want managed backups.

## UI sign-in

`AXL_UI_AUTH` decides how the human signs in to the web UI. It never affects agents: `/mcp`
and `/v1` always need a bearer token, and `axl token create` needs only `DATABASE_URL`.

| Mode | Behavior |
|---|---|
| `auto` (default) | `trusted-header` if `AXL_TRUSTED_HEADER` is set, else `secret` if `AXL_ADMIN_SECRET` is set, else `none` when bound to a loopback address (`127.0.0.0/8`, `::1`, `localhost`). Bound anywhere else with neither set, the server refuses to start and names the three ways out. |
| `none` | No login page and no log-out: every UI request acts as the human. Allowed off loopback (for a deploy reachable only over a VPN or through an authenticating proxy), with a loud warning at start. Only allowlisted `Host` names are served. |
| `secret` | A login form that checks `AXL_ADMIN_SECRET` (required). |
| `trusted-header` | Requests carrying `AXL_TRUSTED_HEADER` are signed in. The secret login also works when `AXL_ADMIN_SECRET` is set. |

The server prints the mode it chose at start: `axl: UI sign-in: secret`.

In every mode, each POST needs the session's CSRF token and the strict Content Security
Policy applies. See [security](security.md) for what each mode protects against.

### Examples

```sh
# Local: loopback bind, no sign-in (auto)
axl serve

# A shared host behind TLS: secret sign-in
export AXL_ADMIN_SECRET="$(openssl rand -hex 16)"
export AXL_COOKIE_SECURE=true
axl serve --bind 0.0.0.0:7420

# Behind oauth2-proxy, which sets and strips X-Auth-Request-Email
axl serve --bind 0.0.0.0:7420 --trusted-header X-Auth-Request-Email

# A container reachable only over a VPN, browsed as axle.internal.example.com
AXL_UI_AUTH=none AXL_ALLOWED_HOSTS=axle.internal.example.com axl serve --bind 0.0.0.0:7420
```

## Command-line reference

| Command | Purpose |
|---|---|
| `axl serve` | Run the server (needs `DATABASE_URL`). `--bind` and `--trusted-header` override the variables above. |
| `axl token create\|list\|revoke` | Manage tokens by hand (needs `DATABASE_URL`). |
| `axl install`, `login`, `logout`, `uninstall`, `status`, `service` | Set up a machine; see the [CLI reference](cli.md). |
| `axl bridge`, `axl hook` | Spawned by harnesses. |
| `axl migrate`, `axl project` | Migrate the database; manage projects. |

Run `axl --help` for the full list.
