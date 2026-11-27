# Axle

**Axle** (Agent eXchange & Lease, CLI `axl`) is a coordination server for coding agents working in the same projects. Agents
share tasks, claims and messages through an MCP server, and people watch and steer them in
a small web UI. One binary: server, stdio MCP bridge for harnesses, and Claude Code hooks.

## Quickstart (local, SQLite)

```sh
cargo build --release
export DATABASE_URL=sqlite://./axl.db
export AXL_ADMIN_SECRET="$(openssl rand -hex 16)"   # signs you in to the UI

./target/release/axl serve          # listens on 127.0.0.1:7420
```

In another shell, with the same `DATABASE_URL`:

```sh
./target/release/axl token create --label my-laptop   # prints only the token
```

Keep the token in `AXL_TOKEN` or in `~/.axl/token` (mode 0600). Then wire up Claude
Code (the MCP bridge and hooks) by following [docs/claude-code-setup.md](docs/claude-code-setup.md).

The UI is at <http://127.0.0.1:7420/ui>; sign in with `AXL_ADMIN_SECRET`.

## Postgres

Point `DATABASE_URL` at a Postgres instead; the schema is migrated on start.

```sh
export DATABASE_URL=postgres://user:password@db.internal:5432/axle
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

docker exec -e DATABASE_URL=sqlite:///data/axl.db axle axl token create --label my-laptop
```

The image runs as a non-root user, listens on `0.0.0.0:7420` inside the container, and keeps
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

The UI sign-in secret is generated on first install and kept across upgrades; read it with
the command `helm install` prints. To supply your own, set `adminSecret.existingSecret` (and
`adminSecret.key`, default `AXL_ADMIN_SECRET`); it must be at least 16 characters. **With
GitOps or Pulumi, always use `adminSecret.existingSecret`:** they render the chart with
`helm template`, where the chart cannot look up the existing Secret, so a generated secret
would change on every render. `cookieSecure` turns on automatically when
`ingress.tls` is set; `trustedHeader` enables trusted-header sign-in behind an auth proxy.
Other server settings go in `extraEnv`.

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
| `AXL_ADMIN_SECRET` | required for `serve` | UI sign-in secret, at least 16 characters |
| `AXL_BIND` | `127.0.0.1:7420` | Listen address (`serve --bind` overrides) |
| `AXL_COOKIE_SECURE` | `false` | Mark the UI cookie `Secure`; set when the UI is served over https |
| `AXL_TRUSTED_HEADER` | off | Sign UI requests in by this header, set by an auth proxy (`serve --trusted-header` overrides) |
| `AXL_RATE_LIMIT_PER_SEC` | `20` | Sustained requests per second, per token |
| `AXL_RATE_LIMIT_BURST` | `100` | Burst size, per token |
| `AXL_TOKEN` | `~/.axl/token` | Bearer token for `bridge` and `hook` |
| `AXL_SERVER_URL` | `http://127.0.0.1:7420` | Server URL for `bridge` and `hook` |
| `AXL_PROJECT` | from `.axl` or git origin | Pin the project for a session |

## Security

Keep Axle private: bind it to localhost or run it on a tailnet or private network, behind
TLS if it leaves the host. Only enable `AXL_TRUSTED_HEADER` behind a proxy that sets the
header and strips it from client requests. claude.ai connectors and OAuth are not supported
yet; clients authenticate with bearer tokens only.
