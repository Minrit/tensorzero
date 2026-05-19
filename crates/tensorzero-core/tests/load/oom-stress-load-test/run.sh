#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOAD_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATES_DIR="$(cd "$LOAD_DIR/../../.." && pwd)"
OUT_DIR="${OUT_DIR:-$SCRIPT_DIR/output}"
RATE="${RATE:-2000}"
DURATION="${DURATION:-600s}"
RSS_DURATION_SECONDS="${RSS_DURATION_SECONDS:-600}"
SAMPLE_SECONDS="${SAMPLE_SECONDS:-30}"
RECOVERY_SECONDS="${RECOVERY_SECONDS:-60}"
GATEWAY_URL="${GATEWAY_URL:-http://127.0.0.1:39100}"
MOCK_PROVIDER_ADDRESS="${MOCK_PROVIDER_ADDRESS:-127.0.0.1:39130}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-tensorzero-oom-stress}"
CLICKHOUSE_DATABASE="${CLICKHOUSE_DATABASE:-tensorzero_oom_stress}"
CLICKHOUSE_HTTP_PORT="${CLICKHOUSE_HTTP_PORT:-39123}"
CLICKHOUSE_NATIVE_PORT="${CLICKHOUSE_NATIVE_PORT:-39000}"
TENSORZERO_CLICKHOUSE_URL="${TENSORZERO_CLICKHOUSE_URL:-http://chuser:chpassword@localhost:$CLICKHOUSE_HTTP_PORT/$CLICKHOUSE_DATABASE}"
CLEANUP_COMPOSE="${CLEANUP_COMPOSE:-true}"
RSS_TOTAL_SECONDS=$((RSS_DURATION_SECONDS + RECOVERY_SECONDS))
COMPOSE=(docker compose -p "$COMPOSE_PROJECT_NAME" -f tensorzero-core/tests/load/docker-compose.yml)
export CLICKHOUSE_HTTP_PORT CLICKHOUSE_NATIVE_PORT TENSORZERO_CLICKHOUSE_URL

if [[ "$(uname -s)" == "Darwin" ]]; then
  PYTHON_DYLIB_DIR="${PYTHON_DYLIB_DIR:-}"
  if [[ -z "$PYTHON_DYLIB_DIR" ]]; then
    for candidate in \
      /opt/homebrew/opt/python@3.13/Frameworks/Python.framework/Versions/3.13/lib \
      /opt/homebrew/Caskroom/miniforge/base/lib \
      /opt/homebrew/Cellar/python@3.13/*/Frameworks/Python.framework/Versions/3.13/lib; do
      if [[ -f "$candidate/libpython3.13.dylib" ]]; then
        PYTHON_DYLIB_DIR="$candidate"
        break
      fi
    done
  fi
  if [[ -n "$PYTHON_DYLIB_DIR" ]]; then
    DYLD_EXTRA_PATHS="$PYTHON_DYLIB_DIR:/opt/homebrew/Caskroom/miniforge/base/lib:/opt/homebrew/lib"
  fi
fi

mkdir -p "$OUT_DIR"

command -v vegeta >/dev/null || {
  echo "vegeta is required. Install it from https://github.com/tsenart/vegeta" >&2
  exit 1
}

cleanup() {
  set +e
  if [[ -n "${CLICKHOUSE_CONTAINER:-}" ]]; then
    docker unpause "$CLICKHOUSE_CONTAINER" >/dev/null 2>&1
  fi
  if [[ -n "${GATEWAY_PID:-}" ]]; then
    kill "$GATEWAY_PID" >/dev/null 2>&1
  fi
  if [[ -n "${MOCK_PROVIDER_PID:-}" ]]; then
    kill "$MOCK_PROVIDER_PID" >/dev/null 2>&1
  fi
  if [[ -n "${RSS_SAMPLER_PID:-}" ]]; then
    kill "$RSS_SAMPLER_PID" >/dev/null 2>&1
  fi
  if [[ "${CLEANUP_COMPOSE:-true}" == true ]]; then
    "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1
  fi
}
trap cleanup EXIT

cd "$CRATES_DIR"
"${COMPOSE[@]}" up -d --build --force-recreate --remove-orphans

cargo build --profile performance --bin mock-provider-api
cargo build --profile performance --bin gateway
cargo build --package oom-stress-load-test

echo "Waiting for ClickHouse at $TENSORZERO_CLICKHOUSE_URL ..."
CLICKHOUSE_READY=false
for _ in {1..60}; do
  if curl -fsS "${TENSORZERO_CLICKHOUSE_URL%/*}/ping" >/dev/null 2>&1; then
    CLICKHOUSE_READY=true
    break
  fi
  sleep 1
done
if [[ "$CLICKHOUSE_READY" != true ]]; then
  echo "ClickHouse did not become ready within 60 seconds" >&2
  exit 1
fi

target/performance/mock-provider-api "$MOCK_PROVIDER_ADDRESS" >"$OUT_DIR/mock-provider.log" 2>&1 &
MOCK_PROVIDER_PID=$!

if [[ -n "${DYLD_EXTRA_PATHS:-}" ]]; then
  env \
    DYLD_LIBRARY_PATH="$DYLD_EXTRA_PATHS:${DYLD_LIBRARY_PATH:-}" \
    DYLD_FALLBACK_LIBRARY_PATH="$DYLD_EXTRA_PATHS:${DYLD_FALLBACK_LIBRARY_PATH:-}" \
    target/performance/gateway \
      --config-file tensorzero-core/tests/load/oom-stress.tensorzero.toml \
      --log-format pretty \
      >"$OUT_DIR/gateway.log" 2>&1 &
else
  target/performance/gateway \
    --config-file tensorzero-core/tests/load/oom-stress.tensorzero.toml \
    --log-format pretty \
    >"$OUT_DIR/gateway.log" 2>&1 &
fi
GATEWAY_PID=$!

echo "Waiting for gateway at $GATEWAY_URL ..."
GATEWAY_READY=false
for _ in {1..60}; do
  if curl -fsS "$GATEWAY_URL/health" >/dev/null 2>&1 || curl -fsS "$GATEWAY_URL/metrics" >/dev/null 2>&1; then
    GATEWAY_READY=true
    break
  fi
  if ! kill -0 "$GATEWAY_PID" >/dev/null 2>&1; then
    echo "Gateway process exited before becoming ready" >&2
    tail -100 "$OUT_DIR/gateway.log" >&2 || true
    exit 1
  fi
  sleep 1
done
if [[ "$GATEWAY_READY" != true ]]; then
  echo "Gateway did not become ready within 60 seconds" >&2
  tail -100 "$OUT_DIR/gateway.log" >&2 || true
  exit 1
fi

CLICKHOUSE_CONTAINER="$("${COMPOSE[@]}" ps -q clickhouse)"
if [[ -z "$CLICKHOUSE_CONTAINER" ]]; then
  echo "Could not find ClickHouse container" >&2
  exit 1
fi

RSS_READY_FILE="$OUT_DIR/rss.ready"
rm -f "$RSS_READY_FILE"
target/debug/oom-stress-load-test \
  --pid "$GATEWAY_PID" \
  --duration-seconds "$RSS_TOTAL_SECONDS" \
  --sample-seconds "$SAMPLE_SECONDS" \
  --output "$OUT_DIR/rss.csv" \
  --ready-file "$RSS_READY_FILE" &
RSS_SAMPLER_PID=$!

echo "Waiting for RSS baseline sample ..."
for _ in {1..30}; do
  if [[ -f "$RSS_READY_FILE" ]]; then
    break
  fi
  if ! kill -0 "$RSS_SAMPLER_PID" >/dev/null 2>&1; then
    echo "RSS sampler exited before baseline was captured" >&2
    exit 1
  fi
  sleep 1
done
if [[ ! -f "$RSS_READY_FILE" ]]; then
  echo "RSS sampler did not capture baseline within 30 seconds" >&2
  exit 1
fi

docker pause "$CLICKHOUSE_CONTAINER" >/dev/null

cat >"$OUT_DIR/body.json" <<'JSON'
{
  "model": "gpt-4.1-mini",
  "messages": [
    {
      "role": "user",
      "content": "Is Santa real?"
    }
  ],
  "stream": false
}
JSON

echo "POST $GATEWAY_URL/v1/chat/completions" \
  | vegeta attack \
      -header="Content-Type: application/json" \
      -body="$OUT_DIR/body.json" \
      -duration="$DURATION" \
      -rate="$RATE" \
      -timeout=1s \
  | tee "$OUT_DIR/results.bin" \
  | vegeta report -type=json >"$OUT_DIR/report.json"

python3 - "$OUT_DIR/report.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as f:
    report = json.load(f)

success = report.get("success", 0.0)
if success < 0.99:
    raise SystemExit(f"vegeta success ratio {success} is below 0.99")
print(f"vegeta_success_ratio={success}")
PY

curl -fsS "$GATEWAY_URL/metrics" >"$OUT_DIR/metrics-paused.txt"
PAUSED_DROPPED_TOTAL="$(
  python3 - "$OUT_DIR/metrics-paused.txt" <<'PY'
import re
import sys

total = 0.0
metric = "tensorzero_batch_write_dropped_total"
pattern = re.compile(rf"^{metric}(?:\{{([^}}]*)\}})?\s+([0-9.eE+-]+)")

with open(sys.argv[1]) as f:
    for line in f:
        match = pattern.match(line.strip())
        if not match:
            continue
        labels = match.group(1) or ""
        if "table=" not in labels or "reason=" not in labels:
            raise SystemExit(f"{metric} line is missing required labels: {line.strip()}")
        total += float(match.group(2))

print(int(total))
PY
)"
if [[ "$PAUSED_DROPPED_TOTAL" -le 0 ]]; then
  echo "Expected tensorzero_batch_write_dropped_total to be > 0 while ClickHouse is paused" >&2
  exit 1
fi
echo "paused_dropped_total=$PAUSED_DROPPED_TOTAL"
if ! grep -q 'ClickHouse batch channel full' "$OUT_DIR/gateway.log"; then
  echo "Expected gateway log to include ClickHouse batch channel full drop warning" >&2
  exit 1
fi
for expected in 'reason.*queue_full' 'queue_capacity.*10000' 'table.*ChatInference'; do
  if ! grep -E "$expected" "$OUT_DIR/gateway.log" >/dev/null; then
    echo "Expected gateway log to include structured field pattern: $expected" >&2
    exit 1
  fi
done

docker unpause "$CLICKHOUSE_CONTAINER" >/dev/null
sleep "$RECOVERY_SECONDS"
wait "$RSS_SAMPLER_PID"

curl -fsS "$GATEWAY_URL/metrics" >"$OUT_DIR/metrics-recovered.txt"
RECOVERED_DROPPED_TOTAL="$(
  python3 - "$OUT_DIR/metrics-recovered.txt" <<'PY'
import re
import sys

total = 0.0
metric = "tensorzero_batch_write_dropped_total"
pattern = re.compile(rf"^{metric}(?:\{{([^}}]*)\}})?\s+([0-9.eE+-]+)")

with open(sys.argv[1]) as f:
    for line in f:
        match = pattern.match(line.strip())
        if match:
            total += float(match.group(2))

print(int(total))
PY
)"
if [[ "$RECOVERED_DROPPED_TOTAL" -ne "$PAUSED_DROPPED_TOTAL" ]]; then
  echo "Drop counter changed after load stopped and ClickHouse recovered: paused=$PAUSED_DROPPED_TOTAL recovered=$RECOVERED_DROPPED_TOTAL" >&2
  exit 1
fi
grep 'tensorzero_batch_write_dropped_total' "$OUT_DIR/metrics-recovered.txt"

echo "OOM stress output written to $OUT_DIR"
