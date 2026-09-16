# Admission server

`branchyard-server` implements the SDK's authenticated HTTP contract using Axum,
SQLx, PostgreSQL and PGMQ. It does not start sandboxes or consume dispatch messages.
`info.execution_ready` is always false in this release.

## Start on a server

Build with the pinned Rust toolchain and a C compiler/CMake:

```sh
cargo build --locked -p branchyard-server -p branchyard-cli
cp deploy/config.example.json /secure/path/server.json
./target/debug/branchyard-server token
```

The token command prints a new secret and its SHA-256 hash once. Protect the output.
Put only `token_sha256` in the configuration, set a future `expires_at_unix`, and
supply the token to clients through `BRANCHYARD_TOKEN`. The example credential is
expired and unusable. Its harness registry is empty, so no task can be admitted.
Do not mark a real harness qualified without the corresponding execution evidence.

Use PostgreSQL 17 with the PGMQ 1.13.0 extension available. Set
`BRANCHYARD_DATABASE_URL` through the server's secret mechanism, then:

```sh
./target/debug/branchyard-server migrate
./target/debug/branchyard-server serve --config /secure/path/server.json
```

Migrations need a role permitted to install PGMQ and create tables. Use a separate
runtime role in production with the required table/sequence permissions and PGMQ
send/list access. The API does not run migrations at startup. It checks that its
queue exists and initializes configured tenant records. Tenant isolation is
application-enforced; there is no database row-level-security policy in this slice.

The listener defaults to `127.0.0.1:8787`. Place authenticated server traffic behind
TLS. A reverse proxy must never rewrite an uncertain committed response into a
submission status reserved for definitive rejection, such as 429.

For an isolated development server, `deploy/compose.yaml` supplies a persistent
PGMQ database, one-shot migration and API container. Set `BRANCHYARD_DB_PASSWORD`
to a URL-safe random password and `BRANCHYARD_SERVER_CONFIG` to the absolute edited
config path, then run `docker compose -f deploy/compose.yaml up --build`. This
recipe uses the database administrator for simplicity; it is not a production role
configuration. The image runs the API as UID 65532. Compose was not run in the
implementation workspace, which has no container runtime.

## Authentication and policy

The bootstrap registry maps high-entropy bearer-token hashes to tenant, subject,
expiry, allowed actions and optional subtree. Configuration is read at startup;
rotation or revocation requires restarting every API replica. This is not an OIDC
issuer, Cedar integration, or runtime token-minting service.

A controller credential has `subtree: null`. A delegated credential names an
existing task UUID and cannot create roots, read ancestors or unrelated tasks,
modify dependencies outside its subtree, or cancel outside that subtree. The
operator provisions these credentials explicitly. Do not give a guest a controller
token. Automatic credential attenuation and injection belong to the next worker slice.

Tenant policy registers harness/environment/policy/repository IDs and resource
ceilings. An admitted child must retain its parent's policy profile and cannot
increase its per-task CPU, memory, wall time or fan-out limits. `max_depth` is the
remaining delegation depth, so every ownership edge strictly decreases it.
`cpu_millis` means millicores. Aggregate reservations cover every nonterminal task,
subject to separate root and tenant ceilings. Required capabilities must be in
the registered qualified profile; an operator-set flag is not runtime evidence.

Checkpoint sources and component bindings are rejected until their lifecycle and
ownership registries exist. Repository commits are immutable input references;
admission does not fetch or verify repository content.

## Transaction and concurrency boundary

A tenant row lock serializes that tenant's admissions across API replicas. Other
tenants can proceed independently. The transaction validates the complete proposed
graph and revisions, updates root state and aggregate reservations, appends task
events, saves the operation and sends its PGMQ command. Queue-write failure rolls
all of them back. No network or model call happens under this lock.

Same operation ID, principal subject and fingerprint replays without another
queue message. Changed input conflicts. A database commit error is an uncertain
submission; the client reconciles the saved operation ID. `operation.state=succeeded`
means the control mutation committed, not that a queued harness ran.

Ownership and dependency edges are separate. A proposal may introduce parents and
descendants in either order. Graph edits are bounded to 64; roots to 256 tasks at
most, with lower configured limits. A graph edit increments the root graph revision
and existing task revisions; cancellation checks a task revision. Read fresh state
before another edit. Dependency cycles, stale revisions and out-of-scope edits
leave no partial graph or reservation.

The API permits 128 active requests, with a 20-connection database pool, 10-second
statement timeout and 5-second lock timeout. Excess ingress requests receive 429
before admission. These are initial bounds, not throughput benchmarks. Tenant-row
contention and fairness must be measured before scaling this design further.

## Retention and unfinished execution

Operations, task identities and events have no TTL or deletion API. Backups and
restores must preserve them together. There is no identity reuse after retention.
Events use task revisions as monotonically increasing sequence IDs; pages are
bounded to 100 and future cursors fail explicitly.

Cancellation records intent only. Reservations remain charged until a future
worker verifies termination and releases them. With this admission-only server,
queued and cancellation-requested tasks therefore continue consuming quota. Do
not remove rows manually to free quota; that would break identity and accounting.

PGMQ carries handles rather than credentials or model prompts. Queue visibility
will not be sufficient execution authority: the next slice needs durable claims,
attempt generations, reconciliation and fenced completion. There are no allocation,
model-turn or Git-promotion side effects in this release.

## Database acceptance tests

Use a disposable PostgreSQL/PGMQ database and set `BRANCHYARD_TEST_DATABASE_URL`:

```sh
cargo test --locked -p branchyard-server --test postgres -- --ignored --test-threads=1
```

The six tests execute real SQL and PGMQ writes, including a trigger-injected failure
at the final queue insertion, concurrent duplicate requests, stale/cyclic graph
proposals, scoped access, quotas, restart reconciliation and a lost HTTP response
after admission. They are explicitly ignored by database-free workspace tests;
the separate `postgres` CI job runs all six and must pass. They do not kill or
fail over the PostgreSQL process, qualify a worker, or establish isolation.
