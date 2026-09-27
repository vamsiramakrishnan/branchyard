# Deploying `by serve`

`deploy/` holds a container image, a self-hosted compose recipe, an example configuration, and a preflight report script. See [the server reference](server.md) for the API, authentication and what is durable; this page is only about packaging and running it.

## Container

```sh
docker build -f deploy/Dockerfile -t branchyard-server .
docker run --rm -p 127.0.0.1:8421:8421 \
  -v "$PWD/deploy/config.example.json:/config/server.json:ro" \
  -v /path/to/a/repo:/repos/app \
  -v branchyard-data:/data \
  branchyard-server serve --config /config/server.json --insecure-bind
```

Multi-stage: `rust:1.90.0-bookworm` builds `by` (`cargo build --release --locked --features postgres -p branchyard-cli`; the `postgres` feature is needed for `--database`, used in `deploy/compose.yaml`), and the runtime stage is `debian:bookworm-slim` with only `ca-certificates`, `curl` (for the healthcheck) and `git` (every served repository is a git work tree) installed. The image runs as a dedicated non-root `branchyard` user, never root. `ENTRYPOINT` is `by serve`, so a bare `docker run branchyard-server --help` (no `serve`) will not do what it looks like; pass `serve`'s own flags directly, as above.

`HEALTHCHECK` polls `GET /healthz` (no token needed; the only unauthenticated route, `docs/server.md#authentication`) every 10 seconds.

By default the local process provider runs harnesses as the container's own user with **no isolation** beyond it (`docs/server.md`, "What it does not guarantee"). This image installs no harness executables; add a layer for the ones a served repository's requests will name, or the server refuses them with `harness_not_found`.

**Not built here.** This environment has no container runtime, so the image was validated statically (this file's build/run flags, the binary and feature it builds, the base images and packages) and the compose recipe only for YAML/schema validity (`docker compose -f deploy/compose.yaml config`, with placeholder values for its required variables), not by actually building or running either.

## Compose: server behind PostgreSQL

`deploy/compose.yaml` runs `database` (`postgres:16`, a named volume) and `server` (built from `deploy/Dockerfile`), wired together with a healthcheck-gated `depends_on` so the server never starts against a database that is not yet accepting connections.

**Secrets are files, never environment variables or literals in the compose file.** Set `BRANCHYARD_SECRETS_DIR` to a directory holding:

- `token.txt`: a bearer token, 16 or more characters, mounted read-only into the server container and referenced as `token_file` in the configuration (below).
- `db-password.txt`: the PostgreSQL password, consumed by Postgres's own `POSTGRES_PASSWORD_FILE` and read again by the server's entrypoint to build its `--database postgres://...` URL — the configuration schema (`crates/branchyard-server/src/config.rs`) has no `database_file` field, only an inline `database` string, so the URL is assembled at container start rather than baked into the checked-in example.

Then set `BRANCHYARD_CONFIG` (an absolute path to an edited copy of `config.example.json`) and `BRANCHYARD_REPO` (an absolute path to the repository to serve), and:

```sh
docker compose -f deploy/compose.yaml up --build
```

The server listens on `0.0.0.0:8421` **inside the compose network only** (nothing is published except `127.0.0.1:8421` on the host itself); `--insecure-bind` is needed because that address is not loopback (`docs/server.md`'s non-loopback rule). Put a real TLS-terminating reverse proxy in front before exposing this beyond the host, or configure `--tls-cert`/`--tls-key` on the server instead and drop `--insecure-bind`.

## Configuration

`deploy/config.example.json` is a minimal, valid configuration for the schema in `crates/branchyard-server/src/config.rs` (also documented in `docs/server.md#configuration`): one repository, one token read from a file, and every other field at an explicit, conservative default (no providers besides `local`, no delegation, no client-supplied commands, no webhooks). Copy it, point `repos` at real repositories, and change `data_dir`, `max_running` and so on for your deployment; unknown keys are rejected (`serde(deny_unknown_fields)`), so a typo fails loudly rather than being silently ignored.

**Checked against the real loader, not just this document.** `crates/branchyard-server/tests/deploy_config.rs` loads `deploy/config.example.json` with `branchyard_server::config::load_file` — the exact function `by serve` calls — as part of `cargo test --workspace`, so the example and the schema cannot drift apart unnoticed. (It patches the example's `token_file` onto a temporary file first, since `/run/secrets/branchyard_token` does not exist outside a container; that is the only change it makes.)

## Preflight

```sh
python3 tools/runtime_preflight.py
python3 tools/runtime_preflight.py --require-sandbox         # also require KVM and cgroup v2
python3 tools/runtime_preflight.py --postgres-url "$URL"     # also require it to be reachable
```

Prints a JSON report (`git` found and its version; `/dev/kvm`'s presence and accessibility; whether cgroup v2's unified hierarchy is mounted; a TCP reachability probe of a given PostgreSQL host and port; which harness executables in `crates/branchyard-harness/src/profiles.rs`'s profile table are on `PATH`) and exits non-zero only on a **required** failure: `git` missing is always required; KVM/cgroup v2 are required only with `--require-sandbox` (what `--allow-provider microsandbox` needs, `docs/providers.md`); PostgreSQL reachability is required only when `--postgres-url` is given. Harness executables are always informational: a served repository only needs the harnesses its own requests actually name. It never starts a sandbox, a server, or a database migration.
