# Axle Helm chart

Installs the Axle coordination server as a single replica, with SQLite on a PVC or an
external Postgres. The [Kubernetes guide](../../../docs/deploy/kubernetes.md) covers install,
database modes and ingress; this page covers the `ui` block. Every value is commented in
[values.yaml](values.yaml) and checked by [values.schema.json](values.schema.json).

## Connecting machines (`publicUrl`)

`publicUrl` sets `AXL_PUBLIC_URL`, the address **Tokens → Connect a machine**, `/install.sh`
and enrollment hand to clients. Leave it empty to derive it from the first ingress host
(`https` when `ingress.tls` is set, else `http`); set it explicitly when the public address
differs from the ingress, for example `publicUrl: https://axle.example.com`. With neither,
the variable is unset and a server bound beyond loopback answers 503 on those endpoints.
Behind an ingress, also pass `AXL_TRUSTED_PROXY_HEADER` through `extraEnv`; see
[security](../../../docs/security.md#enrollment).

## UI sign-in (`ui`)

`ui` controls how a human signs in to the web UI. Agents are not affected: MCP and `/v1`
always need a bearer token, from **Connect a machine** or `kubectl exec … axl token create`.

```yaml
ui:
  auth:
    mode: secret            # secret | none | trustedHeader  (AXL_UI_AUTH)
    secret:                 # mode: secret only
      existingSecret: ""    # use an existing Secret instead of creating one
      key: admin-secret     # key inside that Secret
      value: ""             # optional fixed value (16+ chars); empty = generated once, kept on upgrade
    trustedHeader:          # mode: trustedHeader only
      name: ""              # header your auth proxy sets (AXL_TRUSTED_HEADER); required
  allowedHosts: []          # extra Host names (AXL_ALLOWED_HOSTS)
  cookieSecure: null        # AXL_COOKIE_SECURE; null = on when the ingress has TLS
```

- The chart creates a Secret only for `mode: secret` without `existingSecret`. Each mode
  sets only its own variables: `AXL_ADMIN_SECRET` for `secret` and `AXL_TRUSTED_HEADER` for
  `trustedHeader`.
- `AXL_ALLOWED_HOSTS` is always the in-cluster service names (`<svc>`, `<svc>.<ns>`,
  `<svc>.<ns>.svc`, `<svc>.<ns>.svc.cluster.local`), plus the `ingress.hosts[*].host` values
  and `ui.allowedHosts`, with duplicates removed. `localhost` is always allowed, so
  `kubectl port-forward` works. The list is enforced only when the UI has no sign-in, where
  it blocks DNS-rebinding attacks.
- The render fails with a clear message when values conflict: `secret.value` together
  with `existingSecret`, a `secret.value` shorter than 16 characters, `trustedHeader` mode
  without a `name`, or a `trustedHeader.name` set in another mode. The old top-level
  `adminSecret`, `trustedHeader` and `cookieSecure` keys are rejected.

### GitOps or Pulumi: `secret` with `existingSecret`

`helm template` cannot look up a Secret the chart generated, so a GitOps or Pulumi render
would get a new secret every time. Create the Secret yourself and point the chart at it:

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

### Internal only: `none`

Use this only when the UI can be reached solely over a VPN, a tailnet or an authenticating
proxy. Anyone who reaches it acts as the human, and the install notes print a warning.
Every POST still needs the CSRF token of an anonymous session, and only allowlisted `Host`
names are served.

```yaml
ui:
  auth:
    mode: none
  allowedHosts:
    - axle.tailnet-1234.ts.net   # the name you browse to, if it is not an ingress host
```

### oauth2-proxy: `trustedHeader`

Put [oauth2-proxy](https://oauth2-proxy.github.io/oauth2-proxy/) in front of the ingress and
have it pass the signed-in user. The proxy must also strip that header from client
requests. Route all UI traffic through it and block direct access to the pod or service
(for example with a NetworkPolicy). Without an ingress the install notes print a warning,
because a client that reaches the pod directly can forge the header.

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

`bash tests/helm.sh` renders every mode and checks the variables, the Secret, the schema
rejections and the merged allowed hosts.
