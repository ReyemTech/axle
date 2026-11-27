#!/usr/bin/env bash
# Lints and renders the Helm chart and asserts the rendered manifests. Needs helm and yq.
set -euo pipefail

cd "$(dirname "$0")/.."
CHART=deploy/helm/coord

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
  "sqlite:///data/coord.db" "sqlite DATABASE_URL"
expect "$(doc Deployment -f "$f" | yq '.spec.template.spec.securityContext.fsGroup')" 65532 "sqlite fsGroup"
expect "$(doc Deployment -f "$f" | yq '.spec.template.spec.containers[0].securityContext.readOnlyRootFilesystem')" true "readOnlyRootFilesystem"

# postgres: no PVC, RollingUpdate maxSurge 0, URL from the named secret
f="$CHART/ci/postgres-values.yaml"
expect "$(count PersistentVolumeClaim -f "$f")" 0 "postgres PVC count"
expect "$(doc Deployment -f "$f" | yq '.spec.strategy.type')" RollingUpdate "postgres strategy"
expect "$(doc Deployment -f "$f" | yq '.spec.strategy.rollingUpdate.maxSurge')" 0 "postgres maxSurge"
url="$(doc Deployment -f "$f" | yq '.spec.template.spec.containers[0].env[] | select(.name == "DATABASE_URL") | .valueFrom.secretKeyRef')"
expect "$(echo "$url" | yq '.name')" coord-db "postgres secret name"
expect "$(echo "$url" | yq '.key')" DATABASE_URL "postgres secret key"

# admin secret: generated when none is given, not rendered when an existing one is named
expect "$(count Secret -f "$CHART/ci/sqlite-values.yaml")" 1 "generated admin secret"
expect "$(count Secret -f "$CHART/ci/sqlite-values.yaml" --set adminSecret.existingSecret=mine)" 0 "no secret with existingSecret"
expect "$(doc Deployment -f "$CHART/ci/sqlite-values.yaml" --set adminSecret.existingSecret=mine --set adminSecret.key=K \
  | yq '.spec.template.spec.containers[0].env[] | select(.name == "COORD_ADMIN_SECRET") | .valueFrom.secretKeyRef | .name + "/" + .key')" \
  mine/K "existing admin secret ref"

# ingress is off by default and renders when enabled
expect "$(count Ingress -f "$CHART/ci/sqlite-values.yaml")" 0 "ingress off by default"
expect "$(count Ingress -f "$CHART/ci/sqlite-values.yaml" --set ingress.enabled=true --set 'ingress.hosts[0].host=coord.example.com')" 1 "ingress on"

# schema: one replica only, database mode must be known, postgres needs a secret
if helm template t "$CHART" --set replicaCount=2 >/dev/null 2>&1; then fail "replicaCount=2 was accepted"; fi
if helm template t "$CHART" --set database.mode=mysql >/dev/null 2>&1; then fail "database.mode=mysql was accepted"; fi
if helm template t "$CHART" --set database.mode=postgres >/dev/null 2>&1; then fail "postgres without existingSecret was accepted"; fi

echo "helm: ok"
