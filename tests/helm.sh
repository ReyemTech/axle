#!/usr/bin/env bash
# Lints and renders the Helm chart and asserts the rendered manifests. Needs helm and yq.
set -euo pipefail

cd "$(dirname "$0")/.."
CHART=deploy/helm/axle

fail() { echo "FAIL: $*" >&2; exit 1; }
expect() { [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1'"; }

helm lint "$CHART"

# kind of every document, and one document selected by kind
render() { helm template t "$CHART" "$@"; }
doc() { local kind="$1"; shift; render "$@" | yq "select(.kind == \"$kind\")"; }
count() { local kind="$1"; shift; render "$@" | yq "select(.kind == \"$kind\") | .kind" | grep -c . || true; }

probes() {
  local f="$1"; shift
  local d; d="$(doc Deployment -f "$f" "$@")"
  expect "$(echo "$d" | yq '.spec.template.spec.containers[0].livenessProbe.httpGet.path')" /healthz "$f liveness path"
  expect "$(echo "$d" | yq '.spec.template.spec.containers[0].readinessProbe.httpGet.path')" /readyz "$f readiness path"
  expect "$(echo "$d" | yq '.spec.template.spec.containers[0].livenessProbe.httpGet.port')" 7420 "$f liveness port"
  expect "$(echo "$d" | yq '.spec.replicas')" 1 "$f replicas"
}

for f in "$CHART"/ci/*.yaml; do
  probes "$f"
done

# sqlite: PVC, Recreate, URL on /data
f="$CHART/ci/sqlite-values.yaml"
expect "$(count PersistentVolumeClaim -f "$f")" 1 "sqlite PVC count"
expect "$(doc Deployment -f "$f" | yq '.spec.strategy.type')" Recreate "sqlite strategy"
expect "$(doc Deployment -f "$f" | yq '.spec.template.spec.containers[0].env[] | select(.name == "DATABASE_URL") | .value')" \
  "sqlite:///data/axl.db" "sqlite DATABASE_URL"
expect "$(doc Deployment -f "$f" | yq '.spec.template.spec.securityContext.fsGroup')" 65532 "sqlite fsGroup"
expect "$(doc Deployment -f "$f" | yq '.spec.template.spec.containers[0].securityContext.readOnlyRootFilesystem')" true "readOnlyRootFilesystem"

# postgres: no PVC, RollingUpdate maxSurge 0, URL from the named secret
f="$CHART/ci/postgres-values.yaml"
expect "$(count PersistentVolumeClaim -f "$f")" 0 "postgres PVC count"
expect "$(doc Deployment -f "$f" | yq '.spec.strategy.type')" RollingUpdate "postgres strategy"
expect "$(doc Deployment -f "$f" | yq '.spec.strategy.rollingUpdate.maxSurge')" 0 "postgres maxSurge"
url="$(doc Deployment -f "$f" | yq '.spec.template.spec.containers[0].env[] | select(.name == "DATABASE_URL") | .valueFrom.secretKeyRef')"
expect "$(echo "$url" | yq '.name')" axle-db "postgres secret name"
expect "$(echo "$url" | yq '.key')" DATABASE_URL "postgres secret key"

# UI sign-in modes: env vars only for the active mode, the Secret only for a generated one
S="$CHART/ci/sqlite-values.yaml"
env_of() { doc Deployment -f "$S" "$@" | yq '.spec.template.spec.containers[0].env'; }
env_val() { local name="$1"; shift; env_of "$@" | yq ".[] | select(.name == \"$name\") | .value"; }
has_env() { local name="$1"; shift; env_of "$@" | yq "[.[] | select(.name == \"$name\")] | length"; }

# secret (default): generated Secret, referenced by key
expect "$(env_val AXL_UI_AUTH)" secret "default mode"
expect "$(count Secret -f "$S")" 1 "generated admin secret"
expect "$(env_of | yq '.[] | select(.name == "AXL_ADMIN_SECRET") | .valueFrom.secretKeyRef | .name + "/" + .key')" \
  t-axle/admin-secret "generated admin secret ref"
expect "$(has_env AXL_TRUSTED_HEADER)" 0 "no trusted header in secret mode"
# secret with an explicit value: rendered into the Secret
expect "$(doc Secret -f "$S" --set ui.auth.secret.value=sixteen-chars-xx | yq '.data.admin-secret' | base64 -d)" \
  sixteen-chars-xx "explicit secret value"
# secret with existingSecret: no Secret, ref to it
expect "$(count Secret -f "$S" --set ui.auth.secret.existingSecret=mine)" 0 "no secret with existingSecret"
expect "$(env_of --set ui.auth.secret.existingSecret=mine --set ui.auth.secret.key=K \
  | yq '.[] | select(.name == "AXL_ADMIN_SECRET") | .valueFrom.secretKeyRef | .name + "/" + .key')" \
  mine/K "existing admin secret ref"

# none: no secret, no Secret, a loud warning in the notes
expect "$(env_val AXL_UI_AUTH --set ui.auth.mode=none)" none "none mode"
expect "$(has_env AXL_ADMIN_SECRET --set ui.auth.mode=none)" 0 "no admin secret in none mode"
expect "$(has_env AXL_TRUSTED_HEADER --set ui.auth.mode=none)" 0 "no trusted header in none mode"
expect "$(count Secret -f "$S" --set ui.auth.mode=none)" 0 "no Secret in none mode"
# (output captured first: `grep -q` closing the pipe early would trip pipefail)
notes() { helm install t "$CHART" --dry-run=client -f "$S" "$@" 2>/dev/null; }
has() { [[ "$1" == *"$2"* ]]; }
has "$(notes --set ui.auth.mode=none)" "WARNING: ui.auth.mode=none" || fail "none mode warning missing from NOTES"
if has "$(notes)" WARNING; then fail "secret mode printed a warning"; fi

# trustedHeader: the header, no secret, no Secret; warns without an ingress
TH=(--set ui.auth.mode=trustedHeader --set ui.auth.trustedHeader.name=X-Auth-Request-Email)
expect "$(env_val AXL_UI_AUTH "${TH[@]}")" trusted-header "trustedHeader mode"
expect "$(env_val AXL_TRUSTED_HEADER "${TH[@]}")" X-Auth-Request-Email "trusted header name"
expect "$(has_env AXL_ADMIN_SECRET "${TH[@]}")" 0 "no admin secret in trustedHeader mode"
expect "$(count Secret -f "$S" "${TH[@]}")" 0 "no Secret in trustedHeader mode"
has "$(notes "${TH[@]}")" "WARNING: ui.auth.mode=trustedHeader without an ingress" || fail "trustedHeader warning missing"
if has "$(notes "${TH[@]}" --set ingress.enabled=true --set 'ingress.hosts[0].host=a.example.com')" WARNING; then
  fail "trustedHeader behind an ingress printed a warning"
fi

# allowed hosts: service names + ingress hosts + ui.allowedHosts, deduplicated
expect "$(env_val AXL_ALLOWED_HOSTS --set ingress.enabled=true \
  --set 'ingress.hosts[0].host=axle.example.com' --set 'ingress.hosts[1].host=axle.internal' \
  --set 'ui.allowedHosts[0]=axle.internal' --set 'ui.allowedHosts[1]=10.0.0.7')" \
  "t-axle,t-axle.default,t-axle.default.svc,t-axle.default.svc.cluster.local,axle.example.com,axle.internal,10.0.0.7" \
  "allowed hosts merge"
expect "$(env_val AXL_ALLOWED_HOSTS --namespace ops)" \
  "t-axle,t-axle.ops,t-axle.ops.svc,t-axle.ops.svc.cluster.local" "allowed hosts namespace"

# cookieSecure: auto from ingress TLS, or forced
expect "$(env_val AXL_COOKIE_SECURE)" false "cookie not secure without TLS"
expect "$(env_val AXL_COOKIE_SECURE --set ingress.enabled=true --set 'ingress.hosts[0].host=a' \
  --set 'ingress.tls[0].secretName=t' --set 'ingress.tls[0].hosts[0]=a')" true "cookie secure with TLS"
expect "$(env_val AXL_COOKIE_SECURE --set ui.cookieSecure=true)" true "cookie secure forced"

# publicUrl: AXL_PUBLIC_URL is explicit, else derived from the first ingress host, else absent
expect "$(has_env AXL_PUBLIC_URL)" 0 "no AXL_PUBLIC_URL by default"
expect "$(env_val AXL_PUBLIC_URL --set ingress.enabled=true --set 'ingress.hosts[0].host=axle.example.com' \
  --set 'ingress.tls[0].hosts[0]=axle.example.com')" https://axle.example.com "AXL_PUBLIC_URL from TLS ingress"
expect "$(env_val AXL_PUBLIC_URL --set ingress.enabled=true --set 'ingress.hosts[0].host=axle.example.com')" \
  http://axle.example.com "AXL_PUBLIC_URL from plain ingress"
expect "$(env_val AXL_PUBLIC_URL --set publicUrl=https://x.test)" https://x.test "explicit publicUrl"
expect "$(env_val AXL_PUBLIC_URL --set publicUrl=https://x.test --set ingress.enabled=true \
  --set 'ingress.hosts[0].host=axle.example.com')" https://x.test "explicit publicUrl wins over ingress"
expect "$(has_env AXL_PUBLIC_URL --set ingress.enabled=true)" 0 "no AXL_PUBLIC_URL for an ingress without hosts"
expect "$(env_val AXL_PUBLIC_URL --set ingress.enabled=true --set 'ingress.hosts[0].paths[0].path=/' \
  --set 'ingress.hosts[1].host=second.example.com')" http://second.example.com "AXL_PUBLIC_URL skips a hostless ingress entry"
expect "$(has_env AXL_PUBLIC_URL --set ingress.enabled=true --set 'ingress.hosts[0].paths[0].path=/')" 0 \
  "no AXL_PUBLIC_URL when no ingress entry has a host"

# invalid UI settings are rejected (schema or template)
rejects() { local why="$1"; shift; if helm template t "$CHART" -f "$S" "$@" >/dev/null 2>&1; then fail "$why was accepted"; fi; }
rejects "ui.auth.mode=open" --set ui.auth.mode=open
rejects "trustedHeader without a name" --set ui.auth.mode=trustedHeader
rejects "a bad trusted header name" "${TH[@]}" --set "ui.auth.trustedHeader.name=x user"
rejects "secret value and existingSecret" --set ui.auth.secret.value=sixteen-chars-xx --set ui.auth.secret.existingSecret=mine
rejects "a short secret value" --set ui.auth.secret.value=short
rejects "a trusted header name in secret mode" --set ui.auth.trustedHeader.name=X-User
rejects "an unknown ui key" --set ui.auth.secrets.value=x
rejects "the old adminSecret block" --set adminSecret.existingSecret=mine
has "$(helm template t "$CHART" -f "$S" --set ui.auth.secret.value=sixteen-chars-xx \
  --set ui.auth.secret.existingSecret=mine 2>&1 || true)" "set either value or existingSecret, not both" \
  || fail "combo error message"

# ingress is off by default and renders when enabled
expect "$(count Ingress -f "$CHART/ci/sqlite-values.yaml")" 0 "ingress off by default"
expect "$(count Ingress -f "$CHART/ci/sqlite-values.yaml" --set ingress.enabled=true --set 'ingress.hosts[0].host=axle.example.com')" 1 "ingress on"

# schema: one replica only, database mode must be known, postgres needs a secret
if helm template t "$CHART" --set replicaCount=2 >/dev/null 2>&1; then fail "replicaCount=2 was accepted"; fi
if helm template t "$CHART" --set database.mode=mysql >/dev/null 2>&1; then fail "database.mode=mysql was accepted"; fi
if helm template t "$CHART" --set database.mode=postgres >/dev/null 2>&1; then fail "postgres without existingSecret was accepted"; fi

echo "helm: ok"
