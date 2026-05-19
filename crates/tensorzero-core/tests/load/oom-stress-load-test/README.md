# TensorZero OOM Stress Load Test

This is an operator-run harness for validating bounded ClickHouse
`batch_writes` behavior under backpressure. It is intentionally not wired into
default `cargo test` runs.

The harness mirrors the Zenova production mitigation:

- `gateway.observability.async_writes = false`
- `write_queue_capacity = 10000`
- `flush_interval_ms = 100`
- `max_rows = 1000`

## Run

Install `vegeta`, then run from `tensorzero/crates`:

```bash
bash tensorzero-core/tests/load/oom-stress-load-test/run.sh
```

Optional knobs:

```bash
RATE=2000 DURATION=600s RSS_DURATION_SECONDS=600 RECOVERY_SECONDS=60 \
  GATEWAY_URL=http://127.0.0.1:39100 \
  MOCK_PROVIDER_ADDRESS=127.0.0.1:39130 \
  CLICKHOUSE_HTTP_PORT=39123 \
  CLICKHOUSE_NATIVE_PORT=39000 \
  bash tensorzero-core/tests/load/oom-stress-load-test/run.sh
```

The script starts ClickHouse, the mock provider, and the gateway, pauses
ClickHouse for the load window, drives the OpenAI-compatible
`/v1/chat/completions` route, samples gateway RSS, and checks that
`tensorzero_batch_write_dropped_total` is exposed from `/metrics` with the
required `{table,reason}` labels.

By default it uses the compose project `tensorzero-oom-stress` and nonstandard
local ports (`39100`, `39130`, `39123`, `39000`) so it can run beside an existing
LobsterPool/TensorZero development stack. The script tears down that isolated
compose project and volume on exit; set `CLEANUP_COMPOSE=false` to inspect the
container after a failed run.

Healthy signals:

- vegeta success ratio is at least `0.99`
- `max(rss) - baseline_rss < 256 MiB`
- `tensorzero_batch_write_dropped_total` is present and nonzero while ClickHouse
  is paused
- after ClickHouse is unpaused and load has stopped, the drop counter stops
  growing and final RSS returns to within 10% of baseline

If drops are increasing and RSS is still growing, the bounded batch writer is
not the leak vector; capture a gateway profile and escalate to the gateway
owner.
