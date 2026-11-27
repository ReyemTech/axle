# Axle

**Axle** (Agent eXchange & Lease, CLI `axl`) is a coordination server for coding agents working in the same projects. Agents
share tasks, claims and messages through an MCP server, and people watch and steer them in
a small web UI. One binary: server, stdio MCP bridge for harnesses, and Claude Code hooks.

## Install

No compiler needed. Pick one:

```sh
# Shell installer (macOS and Linux, x86_64 and arm64)
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ReyemTech/axle/releases/latest/download/axle-mcp-installer.sh | sh

# Homebrew
brew install reyemtech/tap/axle

# Prebuilt binary via cargo-binstall
cargo binstall axle-mcp

# From source (crates.io)
cargo install axle-mcp --locked
```

All of these install the `axl` binary. Release archives ship `.sha256` checksums.

Docker (multi-arch, amd64 and arm64):

```sh
docker pull ghcr.io/reyemtech/axle:latest
# Amazon ECR, when the maintainers have enabled the mirror (see docs/releasing.md):
docker pull <registry>/axle:latest
```

## Quickstart (local, SQLite)

```sh
cargo build --release
export DATABASE_URL=sqlite://./axl.db

./target/release/axl serve          # listens on 127.0.0.1:7420, UI without sign-in
```

In another shell, with the same `DATABASE_URL`:

```sh
./target/release/axl token create --label my-laptop   # prints only the token
```

Keep the token in `AXL_TOKEN` or in `~/.axl/token` (mode 0600). Then wire up Claude
Code (the MCP bridge and hooks) by following [docs/claude-code-setup.md](docs/claude-code-setup.md).

The UI is at <http://127.0.0.1:7420/ui>. On a loopback address it needs no sign-in (see
[UI sign-in](#ui-sign-in)); agents still need their bearer token.

## Postgres

Point `DATABASE_URL` at a Postgres instead; the schema is migrated on start.

```sh
export DATABASE_URL=postgres://user:password@db.internal:5432/axle
export AXL_ADMIN_SECRET="$(openssl rand -hex 16)"   # off loopback the UI needs a sign-in
axl serve --bind 0.0.0.0:7420
```

## Docker

```sh
docker build -t axle .

docker run -d --name axle -p 127.0.0.1:7420:7420 \
  -v axle-data:/data \
  -e DATABASE_URL=sqlite:///data/axl.db \
  -e AXL_ADMIN_SECRET="$(openssl rand -hex 16)" \
  axle

docker exec axle axl token create --label my-laptop
```

The container binds `0.0.0.0`, so it refuses to start until you pick a UI sign-in: pass
`AXL_ADMIN_SECRET` as above, `AXL_TRUSTED_HEADER` behind an auth proxy, or
`AXL_UI_AUTH=none` (with `AXL_ALLOWED_HOSTS` for any name other than localhost) when the
port is reachable only from your machine or VPN. The image runs as a non-root user, listens on `0.0.0.0:7420` inside the container, and keeps
SQLite data in the `/data` volume. The `-p 127.0.0.1:…` mapping publishes it on loopback only,
matching local mode; widen it only behind TLS. For Postgres, pass a `postgres://` `DATABASE_URL` instead. `tests/smoke.sh`
builds the image and checks it end to end.

## Kubernetes (Helm)

The chart is published as an OCI artifact next to the image (`ghcr.io/reyemtech/axle`):

```sh
helm install axle oci://ghcr.io/reyemtech/charts/axle --version 0.1.0 \
  --namespace axle --create-namespace
```

Axle runs as a single replica (the chart rejects anything else). Pick a database mode:

```yaml
# SQLite (default): a 1Gi ReadWriteOnce PVC at /data, Recreate rollouts.
database:
  mode: sqlite
persistence:
  size: 1Gi
  # storageClass: fast-ssd
```

```yaml
# Postgres: the URL comes from an existing Secret; no PVC.
database:
  mode: postgres
  existingSecret: axle-db     # holds the connection URL
  urlKey: DATABASE_URL
```

UI sign-in is set under `ui.auth.mode`: `secret` (default; generated on first install and
kept across upgrades, read it with the command `helm install` prints), `none`, or
`trustedHeader` behind an auth proxy. **With GitOps or Pulumi, use
`ui.auth.secret.existingSecret`:** they render with `helm template`, where the chart cannot
look up a generated Secret. `ui.cookieSecure` turns on automatically when `ingress.tls` is
set. Every option, with an example per mode, is in
[deploy/helm/axle/README.md](deploy/helm/axle/README.md). Other server settings go in
`extraEnv`.

Create the first agent token inside the pod (shown once; keep it out of prompts and chat):

```sh
kubectl exec -n axle deploy/axle -- axl token create --label my-laptop
```

(`deploy/axle` assumes release name `axle`; the install notes print the exact name.)

Axle streams events over SSE, so an ingress must not buffer responses and must keep idle
connections open. For ingress-nginx:

```yaml
ingress:
  enabled: true
  className: nginx
  annotations:
    nginx.ingress.kubernetes.io/proxy-buffering: "off"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
  hosts:
    - host: axle.example.com
  tls:
    - secretName: axle-tls
      hosts: [axle.example.com]
```

`bash tests/helm.sh` lints and renders the chart and asserts the manifests.

## The identity rule

No token and no agent or session id is ever placed in the model's context. The bridge and
hooks read the token from `AXL_TOKEN` or `~/.axl/token`, and the session id is tracked
by the harness process, so identity survives context compaction. Tool arguments never carry
credentials, and you should not paste a token into prompts, chat or committed config.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | required | `sqlite://path` or `postgres://...`; used by `serve`, `migrate` and `token` |
| `AXL_UI_AUTH` | `auto` | UI sign-in: `auto`, `none`, `secret` or `trusted-header` (see [UI sign-in](#ui-sign-in)) |
| `AXL_ADMIN_SECRET` | unset | UI sign-in secret, at least 16 characters |
| `AXL_TRUSTED_HEADER` | off | Sign UI requests in by this header, set by an auth proxy (`serve --trusted-header` overrides) |
| `AXL_ALLOWED_HOSTS` | none | Extra comma-separated `Host` names the UI answers to when it has no sign-in |
| `AXL_BIND` | `127.0.0.1:7420` | Listen address, or `localhost:<port>` (`serve --bind` overrides) |
| `AXL_COOKIE_SECURE` | `false` | Mark the UI cookie `Secure`; set when the UI is served over https |
| `AXL_RATE_LIMIT_PER_SEC` | `20` | Sustained requests per second, per token |
| `AXL_RATE_LIMIT_BURST` | `100` | Burst size, per token |
| `AXL_TOKEN` | `~/.axl/token` | Bearer token for `bridge` and `hook` |
| `AXL_SERVER_URL` | `http://127.0.0.1:7420` | Server URL for `bridge` and `hook` |
| `AXL_PROJECT` | from `.axl` or git origin | Pin the project for a session |

## UI sign-in

`AXL_UI_AUTH` decides how the human signs in to the web UI. It never affects agents: MCP and
`/v1` always need a bearer token, and `axl token create` needs only `DATABASE_URL` (direct
database access is the trust boundary).

| Mode | Behaviour |
|---|---|
| `auto` (default) | `trusted-header` if `AXL_TRUSTED_HEADER` is set, else `secret` if `AXL_ADMIN_SECRET` is set, else `none` when bound to loopback (`127.0.0.0/8`, `::1`, `localhost`); otherwise `serve` refuses to start and names the three ways out |
| `none` | No login page and no log-out; every UI request is the human. Allowed off loopback (a container behind a VPN or an authenticating proxy), with a loud warning at start |
| `secret` | Sign in with `AXL_ADMIN_SECRET` (required, at least 16 characters) |
| `trusted-header` | Requests carrying `AXL_TRUSTED_HEADER` are signed in; the secret login also works when `AXL_ADMIN_SECRET` is set |

Without sign-in the UI keeps its other defences: every POST still needs the CSRF token of an
anonymous session cookie issued on first visit, the strict CSP stays, and requests whose
`Host` is not `localhost`, `127.0.0.1`, `[::1]` (any port), the bind address or an
`AXL_ALLOWED_HOSTS` entry are refused with 421, so a malicious page cannot reach the UI
through DNS rebinding.

## Security

Keep Axle private: bind it to localhost or run it on a tailnet or private network, behind
TLS if it leaves the host. Run the UI without sign-in (`AXL_UI_AUTH=none`) only where nobody
else can reach it. Only enable `AXL_TRUSTED_HEADER` behind a proxy that sets the
header and strips it from client requests. claude.ai connectors and OAuth are not supported
yet; clients authenticate with bearer tokens only.

## License

Axle is source-available under the [Elastic License 2.0](LICENSE): free to use, modify and
self-host. You may not provide Axle to third parties as a hosted or managed service.

Contributions need a signed CLA; see [CONTRIBUTING.md](CONTRIBUTING.md).
