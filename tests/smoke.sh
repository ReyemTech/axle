#!/usr/bin/env bash
# Builds the image, runs it, and checks the basics end to end. Exits non-zero on any failure.
# Set AXL_SMOKE_IMAGE to test an image that is already built (CI builds it with a layer cache).
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${AXL_SMOKE_IMAGE:-axle-smoke:$$}"
NAME="axle-smoke-$$"
PORT="${AXL_SMOKE_PORT:-$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')}"
SECRET="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  [ -n "${AXL_SMOKE_IMAGE:-}" ] || docker rmi -f "$IMAGE" >/dev/null 2>&1 || true
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; docker logs "$NAME" >&2 2>&1 || true; exit 1; }

if [ -z "${AXL_SMOKE_IMAGE:-}" ]; then
  echo "building image"
  docker build -q -t "$IMAGE" . >/dev/null
fi

docker run -d --name "$NAME" -p "127.0.0.1:$PORT:7420" \
  -e DATABASE_URL=sqlite:///data/axl.db \
  -e AXL_ADMIN_SECRET="$SECRET" \
  "$IMAGE" >/dev/null

base="http://127.0.0.1:$PORT"
code() { curl -s -o /dev/null -w '%{http_code}' "$@" || true; }

ready=""
for _ in $(seq 1 60); do
  if [ "$(code "$base/healthz")" = "200" ]; then ready=1; break; fi
  sleep 0.5
done
[ -n "$ready" ] || fail "/healthz never returned 200"
echo "ok /healthz 200"

token="$(docker exec "$NAME" axl token create --label smoke)" \
  || fail "token creation failed"
[ -n "$token" ] || fail "empty token"
echo "ok token created"

session="$(python3 -c 'import uuid; print(uuid.uuid4())')"
status="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$base/v1/register" \
  -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
  -d "{\"session_id\":\"$session\",\"host\":\"smoke\",\"harness\":\"claude-code\"}" || true)"
[ "$status" = "200" ] || fail "/v1/register returned $status"
echo "ok /v1/register 200"

status="$(code "$base/ui/login")"
[ "$status" = "200" ] || fail "/ui/login returned $status"
echo "ok /ui/login 200"

# The image binds 0.0.0.0, so with no UI sign-in configured it must refuse to start.
if out="$(docker run --rm -e DATABASE_URL=sqlite:///tmp/axl.db "$IMAGE" 2>&1)"; then
  fail "the image started without any UI sign-in configured"
fi
case "$out" in
  *AXL_ADMIN_SECRET*AXL_TRUSTED_HEADER*AXL_UI_AUTH=none*) ;;
  *) fail "unclear refusal: $out" ;;
esac
echo "ok refuses to start without UI sign-in"

echo "smoke test passed"
