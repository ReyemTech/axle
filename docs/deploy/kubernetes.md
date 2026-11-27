# Run on Kubernetes

The Helm chart is published as an OCI artifact next to the image. It runs Axle as a single
replica with SQLite on a PVC or an external Postgres. Every value is documented in the
[chart README](../../deploy/helm/axle/README.md), [values.yaml](../../deploy/helm/axle/values.yaml)
and [values.schema.json](../../deploy/helm/axle/values.schema.json).

## Install

```sh
helm install axle oci://ghcr.io/reyemtech/charts/axle --version 0.1.0 \
  --namespace axle --create-namespace
```

With the defaults you get SQLite on a 1 Gi PVC and secret sign-in with a generated secret,
kept across upgrades. The install notes print the command that reads it.

Create the first agent token inside the pod (it is shown once; keep it out of prompts and
chat):

```sh
kubectl exec -n axle deploy/axle -- axl token create --label laptop
```

`deploy/axle` assumes the release name `axle`; the install notes print the exact name.

## Values summary

| Value | Default | Meaning |
|---|---|---|
| `replicaCount` | `1` | Must be 1; the schema rejects anything else (see [architecture](../architecture.md#why-a-single-replica)). |
| `image.repository` / `image.tag` | `ghcr.io/reyemtech/axle` / chart appVersion | The image. |
| `database.mode` | `sqlite` | `sqlite` (PVC at `/data`, `Recreate` rollouts) or `postgres`. |
| `database.existingSecret` / `database.urlKey` | `""` / `DATABASE_URL` | Postgres only: the Secret and key holding the connection URL. |
| `persistence.size` / `persistence.storageClass` | `1Gi` / cluster default | SQLite only. |
| `ui.auth.mode` | `secret` | `secret`, `none` or `trustedHeader`. |
| `ui.auth.secret.existingSecret` / `.key` / `.value` | `""` / `admin-secret` / `""` | Secret mode. Empty value: generated once. |
| `ui.auth.trustedHeader.name` | `""` | Trusted-header mode: the header your proxy sets. |
| `ui.allowedHosts` | `[]` | Extra `Host` names. Ingress hosts and service names are added automatically. |
| `ui.cookieSecure` | `null` | `null` means on when the ingress has TLS. |
| `service.type` / `service.port` | `ClusterIP` / `7420` | The service. |
| `ingress.*` | disabled | Class, annotations, hosts, TLS. |
| `extraEnv` | `[]` | Other server settings, such as `AXL_RATE_LIMIT_PER_SEC`. |
| `resources`, `nodeSelector`, `tolerations`, `affinity`, `podAnnotations` | empty | Scheduling. |
| `terminationGracePeriodSeconds` | `15` | The server drains for up to 5 s on SIGTERM. |

The pod runs as non-root with a read-only root filesystem and all capabilities dropped, and
has liveness (`/healthz`) and readiness (`/readyz`) probes.

## Database

```yaml
# SQLite (default)
database:
  mode: sqlite
persistence:
  size: 1Gi
  # storageClass: fast-ssd
```

```yaml
# Postgres: the URL comes from an existing Secret; no PVC
database:
  mode: postgres
  existingSecret: axle-db
  urlKey: DATABASE_URL
```

```sh
kubectl create secret generic axle-db -n axle \
  --from-literal=DATABASE_URL='postgres://axle:password@db.example.com:5432/axle'
```

## UI sign-in, one example per mode

### `secret` with your own Secret (GitOps, Pulumi, Argo CD)

`helm template` cannot look up a Secret the chart generated, so a GitOps render would get a
new secret every time. Create it yourself:

```sh
kubectl create secret generic axle-admin -n axle \
  --from-literal=admin-secret="$(openssl rand -hex 16)"
```

```yaml
ui:
  auth:
    mode: secret
    secret:
      existingSecret: axle-admin
      key: admin-secret
```

### `none` (reachable only over a VPN or tailnet)

Anyone who reaches the UI acts as the human, and the install notes print a warning. Only
allowlisted `Host` names are served.

```yaml
ui:
  auth:
    mode: none
  allowedHosts:
    - axle.tailnet-example.ts.net
```

### `trustedHeader` (behind oauth2-proxy)

The proxy must set the header and strip it from client requests. Route all UI traffic
through it and block direct access to the pod, for example with a NetworkPolicy.

```yaml
ingress:
  enabled: true
  className: nginx
  annotations:
    nginx.ingress.kubernetes.io/auth-url: "https://$host/oauth2/auth"
    nginx.ingress.kubernetes.io/auth-signin: "https://$host/oauth2/start?rd=$escaped_request_uri"
    nginx.ingress.kubernetes.io/auth-response-headers: X-Auth-Request-Email
    nginx.ingress.kubernetes.io/proxy-buffering: "off"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
  hosts:
    - host: axle.example.com
  tls:
    - secretName: axle-tls
      hosts: [axle.example.com]
ui:
  auth:
    mode: trustedHeader
    trustedHeader:
      name: X-Auth-Request-Email
```

Agents are not affected by any of this: `/mcp` and `/v1` always need a bearer token.

## Ingress

Axle streams events over SSE, so the ingress must not buffer responses and must keep idle
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

With `ingress.tls` set, `ui.cookieSecure` turns on automatically. Harnesses then use
`AXL_SERVER_URL=https://axle.example.com`.

## Check the chart

```sh
bash tests/helm.sh
```

It lints and renders every mode and asserts the variables, the Secret, the schema
rejections and the merged allowed hosts.
