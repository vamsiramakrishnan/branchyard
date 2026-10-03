# Observability

What a Branchyard server tells you about itself: Prometheus metrics at `GET /metrics`, OpenTelemetry traces of each operation from admission to its turns, a `traceparent` carried to harnesses, the connector gateway and webhook receivers, and `by stats` for a quick summary from the command line. The scheduling these describe (priority, fair share, aging) is in [the server reference](server.md#scheduling).

> **Status, 1 October 2026.** Tested hermetically: the exposition format against an independent parser, the counters over real HTTP with the fake ACP agent, spans in memory and over OTLP/HTTP to a local stand-in collector. Not yet scraped by a real Prometheus or exported to a real collector, and no dashboard is provided.

## Metrics

Off by default. Turn them on with `--metrics` (on the server's own listener), `--metrics-addr ADDR` (also a plain-HTTP listener of its own that serves only `/metrics`), `--metrics-token-file FILE`, or in the configuration file:

```json
"metrics": { "listen": "127.0.0.1:9464", "token_file": "/etc/branchyard/metrics.token" }
```

| Where | Who may read it |
|---|---|
| `GET /metrics` on the server's listener | a principal with the `admin` scope, or the metrics token. Another principal gets `403 scope_required` (`detail.scope = "admin"`), no token `401` |
| `GET /metrics` on `--metrics-addr` | the metrics token when one is configured, else anyone who can reach it. A non-loopback metrics address needs a token and `--insecure-bind` (it is plain HTTP) |

The metrics token (at least 16 characters) reads `/metrics` and nothing else: it is not a credential for the API. A `by worker`, which has no listener, can serve metrics with `--metrics-addr`.

The format is Prometheus text exposition 0.0.4 (`text/plain; version=0.0.4`), written by a small encoder in [`metrics.rs`](../crates/branchyard-server/src/metrics.rs): no metrics crate was in `Cargo.lock` to adopt. Every family is declared once, with its `HELP` and `TYPE`, and written even before it has samples.

| Metric | Type | Labels | What |
|---|---|---|---|
| `branchyard_build_info` | gauge | `version` | Always 1 |
| `branchyard_operations` | gauge | `state` (`queued`, `running`) | Unfinished operations in the shared queue: unclaimed, and claimed under a live lease |
| `branchyard_operations_admitted_total` | counter | `kind`, `tenant` | Operations this process admitted |
| `branchyard_operations_finished_total` | counter | `kind`, `state` (`succeeded`, `failed`, `interrupted`) | Operations this process recorded finished |
| `branchyard_queue_depth` | gauge | `tenant`, `priority`, `labels` | Unclaimed queued operations; `labels` is the required worker labels, comma-joined, empty for none |
| `branchyard_queue_oldest_age_seconds` | gauge | `tenant`, `priority`, `labels` | How long the oldest unclaimed operation of the group has waited |
| `branchyard_claims_total` | counter | `tenant`, `priority` | Claims this process's dispatcher made |
| `branchyard_claim_wait_seconds` | histogram | `priority` | Time from admission to claim |
| `branchyard_lease_renewals_total` | counter | `result` (`renewed`, `lost`, `error`) | Claim lease renewals |
| `branchyard_lease_expiries_total` | counter | | Claims that took over another worker's lapsed claim (its lease expired, or its process is gone from this host) |
| `branchyard_turns_started_total` | counter | `harness` | Turns started by operations this process ran |
| `branchyard_turns_ended_total` | counter | `harness`, `outcome` | Turns ended: the harness's outcome (`completed`, `interrupted`, `failed`, `limit_reached`, `refused`), or the engine's (`budget_exceeded`, `failed`, `interrupted`) when it ended the turn itself, or `unknown` |
| `branchyard_turn_duration_seconds` | histogram | `harness` | From the prompt to the turn's end |
| `branchyard_tool_calls_total` | counter | `harness` | Tool calls the harnesses reported |
| `branchyard_cost_usd_total` | counter | `tenant`, `harness` | Harness-reported cost: what each of an operation's branches added to its recorded `cost_usd` |
| `branchyard_connector_calls_total` | counter | `connector`, `decision` (`allowed`, `denied`, `confirmation_required`) | Connector gateway calls, from its audit log |
| `branchyard_webhook_deliveries_total` | counter | `result` (`delivered`, `retried`, `dead_lettered`) | Webhook delivery attempts |
| `branchyard_workers_live` | gauge | | Workers that recorded themselves alive in the last 15 seconds |
| `branchyard_worker_last_seen_seconds` | gauge | `worker`, `host` | Seconds since each live worker's last beat |
| `branchyard_pool_slots` | gauge | `repo`, `state` (`ready`, `filling`, `claimed`) | [Warm pool](pools.md) slots of each served repository whose workspace has a pool, on this host, read at scrape time |
| `branchyard_pool_claims_total` | counter | `repo`, `result` (`hit`, `miss`) | New branches of tasks this process ran whose workspace has a pool: took a ready slot, or found none |
| `branchyard_pool_slots_made_total` | counter | `repo`, `result` (`made`, `failed`) | Slots this process's keepers made, and fills that stopped on an error |
| `branchyard_pool_fill_seconds` | histogram | `repo` | Time a keeper took to make one slot (worktree and environment) |
| `branchyard_pool_slots_discarded_total` | counter | `repo` | Slots a keeper removed: stale, or left by a stopped process |
| `branchyard_start_seconds` | histogram | `pool` (`hit`, `miss`, `none`) | Start latency of a task's new branches: from the operation's admission to the harness's first prompt, by whether the worktree came from a warm pool |
| `branchyard_sync_bytes_total` | counter | `repo`, `direction` (`up`, `down`) | Bytes this process's [sync](sync.md) sent to and read from the remote |
| `branchyard_sync_objects_total` | counter | `repo`, `direction` | Objects written to and read from the remote |
| `branchyard_sync_swaps_total` | counter | `repo` | Task manifests swapped into the remote |
| `branchyard_sync_swap_conflicts_total` | counter | `repo` | Swaps that lost to another writer, then merged and tried again |
| `branchyard_sync_retries_total` | counter | `repo` | Remote requests tried again after a transient failure |
| `branchyard_sync_divergences_total` | counter | `repo` | Divergences recorded as conflict branches |
| `branchyard_sync_corrupt_total` | counter | `repo` | Objects refused because they did not match their name |
| `branchyard_sync_errors_total` | counter | `repo` | Task syncs that failed and were queued to try again |
| `branchyard_sync_pending` | gauge | `repo` | Tasks queued in the sync outbox |
| `branchyard_sync_lag_seconds` | gauge | `repo` | How long the oldest queued change has waited to reach the remote |

Histogram buckets are 0.1, 0.5, 1, 5, 15, 30, 60, 120, 300, 600, 1800 and 3600 seconds, except `branchyard_start_seconds`: 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 300 and 1800 seconds.

**Counters and gauges on several servers.** Counters count what the process that serves them did. Each operation is admitted by one server and claimed, run and finished by one worker, so on several servers and workers sharing a database, sum counters across them. Gauges read at scrape time from the store (`branchyard_operations`, queue depth and age, live workers) describe the shared database, so every server reports the same values: take one, or `max`, not the sum.

**What turns are counted.** Turns, tool calls, connector calls and cost are read, once an operation finishes, from the events its branches (and the branches they delegated to) recorded between its admission and its end. Turns no operation ran are not counted: a local `by run` on a served repository, and a graph dependent started by a recovery tick.

## Traces

Set `OTEL_EXPORTER_OTLP_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) in the server's or worker's environment:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4318 OTEL_SERVICE_NAME=by-prod by serve --config server.json
```

| Variable | Meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The collector's base URL; spans go to `<it>/v1/traces` |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | The full traces URL, used as is; wins over the above |
| `OTEL_EXPORTER_OTLP_PROTOCOL` (or `..._TRACES_PROTOCOL`) | `http/protobuf` (the default) or `http/json`. `grpc` is not built in: the server warns and exports nothing; point it at the collector's HTTP port, 4318 |
| `OTEL_EXPORTER_OTLP_HEADERS` (or `..._TRACES_HEADERS`) | `key=value,key2=value2`, percent-decoded, sent with each export (an API key, a tenant header) |
| `OTEL_SERVICE_NAME` | The resource's `service.name`; default `branchyard-server` |
| `OTEL_SDK_DISABLED=true`, `OTEL_TRACES_EXPORTER=none` | No export |

Spans are batched (256 at most, every 2 seconds) on a thread of their own; an export that fails is logged and dropped, never retried, and never delays an operation. The admission and operation spans also enter a [`tracing`](https://docs.rs/tracing) span carrying `trace_id` and `span_id` on the thread that runs them, so a `--log-format json` line written while an operation runs there can be joined to its trace.

An operation's trace:

| Span | Parent | From, to | Attributes (`by.*`) |
|---|---|---|---|
| `admission` (server kind) | the request's `traceparent` header, when it sends a valid one; else a new trace | the admission's transaction | `operation`, `repo`, `kind`, `tenant`, `priority`, `admitted` (false when a key replayed, a branch was busy or a quota refused) |
| `claim` | `admission` | admission to the claim: the time it waited queued | `operation`, `worker`, `fence`, `took_over`, `waited_ms`, `priority` |
| `operation <kind>` (`operation task`, `operation merge`, ...) | `admission` | the worker's run, start to outcome | `operation`, `repo`, `kind`, `tenant`, `branches`, `fence`; `error` and an error status when it failed |
| `turn` | `operation <kind>` | the prompt to the turn's end | `branch`, `harness`, `turn`, `outcome`; an error status unless it completed |
| `tool <name>` | `turn` | the harness's `tool_started` to the branch's next event (harnesses report when a tool starts, not when it ends) | `branch`, `tool`, `call_id` |
| `connector <operation>` (client kind) | `turn` | the gateway's recorded latency, ending when the call was recorded | `branch`, `connector`, `operation`, `decision`; an error status unless allowed |

The admission's context is stored with the operation (`StoredOperation::trace`), so the worker that claims it, on any server sharing the database, continues the same trace. Turn, tool and connector spans are made after the operation finishes, from its branches' recorded events and their timestamps (as the metrics above are), so a trace is complete when its operation is.

**Propagation.**

- **Into the harness.** Each turn of a task, send, fork, reincarnate or spawn the server runs gets `TRACEPARENT` (the operation span's context) in its environment ([`TaskOptions::trace_parent`](../crates/branchyard/src/lib.rs)). What the harness runs inherits it: a connector SDK that copies `TRACEPARENT` into the gateway call's `_meta.traceparent`, where Anvil reads it, makes the gateway's spans children of the operation; a tool that exports its own spans joins the trace the same way. A local `by run` passes on whatever `TRACEPARENT` its own environment has.
- **Into webhooks.** A delivery of a branch's activity carries a `traceparent` header with the context of the last operation this server ran on that branch, when there is one (not for a branch another server worked on).
- **From clients.** Send `traceparent` on any operation's `POST`; a malformed one starts a new trace.

**Why not the OpenTelemetry crates.** `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` and `tracing-opentelemetry` are not in `Cargo.lock` or the local registry, and this was built offline, so they could not be added. [`telemetry.rs`](../crates/branchyard-server/src/telemetry.rs) implements the part the server needs instead: the span model, W3C `traceparent` parsing and generation, a batching tracer behind a pluggable `SpanExporter` trait (`MemoryExporter` for tests and embedding), and `OtlpExporter`, which encodes OTLP's `ExportTraceServiceRequest` with `prost` (already a workspace dependency, through the Substrate provider) or as OTLP's JSON mapping, and posts it with `reqwest`. Replacing it with `tracing-opentelemetry` later changes this module only. Not implemented: OTLP over gRPC, sampling (every operation is traced when an exporter is set), span events and links, and metrics or logs over OTLP (metrics are Prometheus only).

## `by stats`

```text
$ by stats
branches  12 (failed 1, merged 4, ready 5, running 2)
turns     37 (claude-code 20, codex 17)
outcomes  budget_exceeded 2, completed 30, failed 2, interrupted 3
turn time median 42.0s, p90 3m10s
tools     120 calls
connectors 8 calls (allowed 7, denied 1)
cost      $4.12 (claude-code $3.00, codex $1.12)
```

Locally it reads the repository's store: branches by status, turns and cost by harness from their records, and outcomes, turn durations, tool calls and connector calls from the event store. When branches used a [warm pool](pools.md), it adds `pool` (hits, misses, and locally the ready slots of the pool's size and how long they took to make) and `start` lines (from a branch being asked for to its first prompt, of hits and of misses). With `--remote` it reads the server's branches (turns and cost, not the event store) and adds the queue: the caller's tenant's queued operations of the repository by priority, those running, and how long the oldest has waited. `--json` prints the same as an object (`branches`, `turns`, `cost_usd`, `outcomes`, `turn_seconds`, `tool_calls`, `connector_calls`, `queue`, `pool`).

## Tests

- `crates/branchyard-server/src/metrics.rs` (3): the warm pool's gauges, counters and fill histogram; every family's `HELP` and `TYPE`, counters, histograms (cumulative buckets, `+Inf`, sum and count), queue gauges and label escaping, checked with a parser of the exposition format written independently of the encoder; number formatting.
- `crates/branchyard-server/src/telemetry.rs` (6): `traceparent` round trip and malformed headers; the protobuf encoding's field numbers checked byte by byte; the JSON mapping; the standard variables; the batching tracer; and a real export over HTTP, in both encodings, to a stand-in collector on loopback.
- `crates/branchyard-server/src/observe.rs` (3): a task's new branches' start latency by pool use, and hits and misses; turns, tools and connector calls from recorded events into metrics and spans (a tool lasting until the next event, an engine-ended turn, another operation's branch ignored); cost as each branch's growth.
- `crates/branchyard-server/tests/observability.rs` (3, over real HTTP with the fake ACP agent): priority checked, capped and inherited; `/metrics` off by default, `401`/`403`, the metrics token reading nothing else, the counters after a task, and the separate listener; an operation traced from an incoming `traceparent` through admission, claim and run to its turn, with the harness seeing the operation's span as `TRACEPARENT`.
- `crates/branchyard-server/tests/webhook.rs`: a delivery carries the operation's `traceparent`, and deliveries are counted.
- `crates/branchyard-server/tests/pools.rs` (1): the pool metrics over real HTTP after a miss and a hit.
- `crates/branchyard-server/tests/sync.rs` (1): the sync series over real HTTP after two pushes.
- `crates/branchyard-cli/src/stats_cmd.rs` (2, one for pool hits, misses and start latency) and `crates/branchyard-cli/tests/remote.rs` (1): the summary's arithmetic and rendering; `--priority` reaching the server's queue, `by --remote stats` showing it, and `by stats` locally.
