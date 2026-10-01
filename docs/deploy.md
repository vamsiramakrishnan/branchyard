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

Multi-stage: `rust:1.94.0-bookworm` builds `by` (`cargo build --release --locked --features postgres -p branchyard-cli`; the `postgres` feature is needed for `--database`, used in `deploy/compose.yaml`), and the runtime stage is `debian:bookworm-slim` with only `ca-certificates`, `curl` (for the healthcheck) and `git` (every served repository is a git work tree) installed. The image runs as a dedicated non-root `branchyard` user, never root. `ENTRYPOINT` is `by serve`, so a bare `docker run branchyard-server --help` (no `serve`) will not do what it looks like; pass `serve`'s own flags directly, as above.

`HEALTHCHECK` polls `GET /healthz` (no token needed; the only unauthenticated route, `docs/server.md#authentication`) every 10 seconds.

By default the local process provider runs harnesses as the container's own user with **no isolation** beyond it (`docs/server.md`, "What it does not guarantee"). This image installs no harness executables; use the [image with harnesses](#image-with-harnesses), or add a layer for the ones a served repository's requests will name, or the server refuses them with `harness_not_found`.

**Not built here.** This environment has no container runtime, so the image was validated statically (this file's build/run flags, the binary and feature it builds, the base images and packages) and the compose recipe only for YAML/schema validity (`docker compose -f deploy/compose.yaml config`, with placeholder values for its required variables), not by actually building or running either.

`by init deploy` writes a compose file, its server configuration and generated secret files for one repository, checked with the server's loader and `docker compose config` ([setup](setup.md)). The image copies `plugins/branchyard/skills` as well as `crates`: `by init plugin` embeds the shipped skills.

## Image with harnesses

```sh
docker build -f deploy/Dockerfile.harnesses -t branchyard-harnesses .
docker run --rm -p 127.0.0.1:8421:8421 -v /path/to/a/repo:/repos/app -v branchyard-data:/data \
  branchyard-harnesses --repo app=/repos/app --data-dir /data --listen 0.0.0.0:8421 --insecure-bind \
  --secret ANTHROPIC_API_KEY=@/run/secrets/anthropic
```

`deploy/Dockerfile.harnesses` is `deploy/Dockerfile` with the harnesses the qualification workflow pins: three stages, the same `by` build stage (`tests/test_distribution.py` checks the two are identical), an `npm ci` of `deploy/harnesses/package-lock.json`, and a runtime on `node:22.22.2-bookworm-slim` pinned by digest, with `git`, `openssh-client` (for [recipes](recipes.md) and [ssh remotes](remote-ssh.md)), `ca-certificates` and `curl`, running as the non-root `branchyard` user.

| Harness | Package | Command |
|---|---|---|
| Claude Code | `@anthropic-ai/claude-code@2.1.283` | `claude` |
| Codex | `@openai/codex@0.157.1` | `codex` |
| Claude Code over ACP | `@agentclientprotocol/claude-agent-acp@0.81.2` | `claude-agent-acp` |

The lock records the SHA-512 `integrity` of every one of its 128 packages (each harness's platform binaries included), all from `registry.npmjs.org`, and `npm ci` refuses a tarball that differs; the test checks every entry has one and that the pins are exactly those in `.github/workflows/qualify.yml`. Claude Code's install script (which links its platform binary) runs, after that check. `DISABLE_AUTOUPDATER=1` keeps the pinned versions. No credential is in the image: pass them at run time, as `--secret` names ([provisioning](provisioning.md)). To change a version, edit `deploy/harnesses/package.json` and `qualify.yml` together and regenerate the lock with `npm install --package-lock-only --ignore-scripts` in `deploy/harnesses`.

The release workflow builds it on every `v*` tag and smoke-tests `by`, `claude`, `codex`, `claude-agent-acp`, `ssh`, `git` and a non-root user in it; it pushes to `ghcr.io/<owner>/<repo>-harnesses:<version>` only when the repository variable `PUBLISH_IMAGES` is `true`. **Not built here** (no container daemon, and no `hadolint`): checked statically by `tests/test_distribution.py` and by review.

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

**Stopping.** `docker stop` and `docker compose down` send SIGTERM, which `by serve` (the container's PID 1, through `exec`) handles like Ctrl-C: it stops taking work, drains connections and gives running operations `shutdown_grace_seconds` (60 by default), all within that period, then records what is still running as `interrupted` and exits 0. Docker kills the container 10 seconds after SIGTERM unless told otherwise, which would cut the grace period short and leave the records to the next start's recovery, so the compose files set `stop_grace_period: 75s`; with `docker run`, pass `--stop-timeout 75`. Keep it above the configured grace. A SIGTERM that arrives while the server is still starting is kept and stops it once it serves. See [the server reference](server.md#running-it).

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
