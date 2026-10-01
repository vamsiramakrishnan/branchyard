# Connectors

A harness on a branch often needs GitHub, Slack, Linear, Google or an internal API. Branchyard does not give it MCP server configurations or upstream credentials. It gives it a **skill and an SDK per granted connector**, and one gateway to call them through. The harness writes code (bash, Python, TypeScript) against the SDK; the gateway holds every upstream authorization, enforces what the branch was granted, and records each call.

Connectors are compiled by [Anvil](https://github.com/vamsiramakrishnan/anvil) (Apache-2.0): from an API contract (OpenAPI, GraphQL, gRPC, SOAP, Discovery, OData) or an existing MCP server (adopted as a source, ADR-0016 there), Anvil produces one reviewed model (AIR) and projects it into a skill, a CLI, SDKs and an MCP server that agree on what each operation does.

Status: **built on both sides, tested end to end against Anvil's gateway; not yet used by a real harness.** This page is the contract both repositories build to; each section says which side owns it. [Branchyard side](#branchyard-side) says what Branchyard built and how to use it, [Anvil side](#anvil-side) where Anvil's implementation refines the contract, and [What is not done](#what-is-not-done) what remains.

## Layers

Each layer owns one idea and treats the next as a contract. None reaches past its neighbour.

| Layer | Owns | Exposes | Never knows about |
|---|---|---|---|
| Upstream | APIs and third-party MCP servers | Their own protocols | Anything of ours |
| Anvil compiler | What an operation means: effect, idempotency, confirmation, auth type, approval | A content-hashed AIR bundle and its projections | Harnesses, branches, people |
| Anvil gateway | Acting on upstreams: the credential vault, connect flows, grant enforcement, confirmations, rate and spend limits, audit | `call(principal, operation, input)` over MCP Streamable HTTP | Git, branches, tasks |
| Branchyard access broker | Who may do what: each branch's grant, a signed short-lived token per turn, narrower grants for children | A JWT the gateway verifies against Branchyard's JWKS | Upstream APIs and credentials |
| Branchyard provisioning | What lands in the harness home: the index, the granted skills, the CLIs and SDKs, two variables | Files and environment | Operation semantics |
| Harness | Doing the task, in code | — | Tokens, MCP, upstreams |

Identity is Branchyard's, authority over upstreams is the gateway's, and meaning is Anvil's. The index a harness reads is a convenience; the gateway enforces the grant whatever the harness tries.

## The contract

### Token (Branchyard mints, Anvil verifies)

A compact JWT signed with Ed25519 (`alg: EdDSA`), one per turn, delivered to the harness in a 0600 file and replaced every turn. Upstream tokens never leave the gateway, and the gateway never forwards this token upstream (the MCP specification forbids token passthrough).

| Claim | Value |
|---|---|
| `iss` | The yard's issuer: the server's URL, or `branchyard:local:<yard id>` locally |
| `aud` | The gateway's canonical `/mcp` URL |
| `sub` | The person the branch acts for (`local:<os user>` locally, the principal on a server) |
| `iat`, `exp`, `jti` | Issued now; expires with the turn's deadline, at most one hour |
| `by_tenant`, `by_branch`, `by_turn` | Where the call comes from, for policy and audit |
| `by_grants` | The grant, below |

A grant is a list of entries; an operation is allowed when some entry allows it:

```json
{"connector": "github", "operations": ["issues.*", "pulls.list"], "mode": "read", "account": "work"}
```

- `connector` is the bundle id the gateway serves.
- `operations` are globs over AIR operation ids; `["*"]` is every approved operation.
- `mode` is `read` (only operations AIR classifies as reads) or `write` (reads and mutations). A mutation that AIR says needs confirmation also needs `"confirm": "allow"` on the entry; otherwise the gateway refuses it with `confirmation_required`.
- `account` picks one of the person's connected accounts for that connector; omitted means their default.

Keys: `kid` in the header; the public keys are a JWKS at the server's `/.well-known/jwks.json`, or locally the file `.branchyard/gateway/jwks.json`. Anvil's inbound auth gets a `branchyard` mode that reads `ANVIL_INBOUND_ISSUER`, `ANVIL_INBOUND_AUDIENCE` and `ANVIL_INBOUND_JWKS_URI` (an `https:` or `file:` URL) and accepts `EdDSA`.

### Harness environment (Branchyard sets, Anvil's CLIs and SDKs read)

| Variable | Value |
|---|---|
| `ANVIL_GATEWAY_URL` | The gateway's `/mcp` URL |
| `ANVIL_GATEWAY_TOKEN_FILE` | The 0600 file holding this turn's token |

With `ANVIL_GATEWAY_URL` set, a generated CLI or SDK sends every call to the gateway and never reads an upstream credential variable (`<SERVICE>_TOKEN` and the rest). The wire is MCP Streamable HTTP (the CLI's existing `--mcp <url>` path); that is Anvil's implementation detail, and the harness never sees it.

### Files in the harness home (Anvil packages, Branchyard places)

```
~/.branchyard/connectors/
  INDEX.md                one short entry per granted connector: what it is for, when to use it, where its skill is
  github/                 `anvil package harness` output for the bundle, gateway mode
    SKILL.md              routing and safety, kept small
    reference/            operations, errors, idempotency, workflows (read on demand)
    python/  typescript/  the SDKs
    bin/github            the CLI
```

Progressive disclosure: `INDEX.md` is what the instructions point at; a harness opens a connector's `SKILL.md` only when a task needs it, `reference/*` only for detail, and `--schema` for one operation. Only granted connectors are placed, and `INDEX.md` lists only them. Anvil provides `anvil package harness <bundle> --out <dir>` and `anvil connectors index --grants <file> --out INDEX.md <bundle...>`; Branchyard caches each package by bundle hash.

A harness without a shell may be given the same gateway as one MCP server instead; it is a second door to the same runtime, not a second configuration.

### Connecting accounts (Anvil owns the flow, Branchyard starts it)

`by connect github [--account work]` asks the gateway for an authorization URL for the current person and opens it. The gateway runs the OAuth authorization-code flow with PKCE (or stores an API key for key-based connectors), keeps tokens encrypted at rest per `(sub, connector, account)`, refreshes them, and marks a connection `needs_reconnect` when a refresh is refused. A call for an unconnected account fails naming the connector; the connect link goes to the person, never to the model.

### Audit (Anvil writes, Branchyard shows)

The gateway appends one JSON line per call to its audit log: time, `sub`, `by_tenant`, `by_branch`, `by_turn`, connector, account, operation, decision (`allowed`, `denied`, `confirmation_required`), the grant entry that allowed it, upstream status and latency, and a hash of the redacted input. Branchyard records each line on its branch as a `connector_call` event, so `by log` and `by watch` show it.

## Branchyard side

Built in [`branchyard::connectors`](../crates/branchyard/src/connectors/mod.rs), with the grant type in [`branchyard_provision::connectors`](../crates/branchyard-provision/src/connectors.rs), `by gateway` and `by connect` in [`gateway_cmd.rs`](../crates/branchyard-cli/src/gateway_cmd.rs), and the server's part in [`branchyard-server`'s `connectors.rs`](../crates/branchyard-server/src/connectors.rs).

### Grants

A branch's grant is stored with it, in its provisioning request (`Provisioning::connectors`, a list of the contract's entries), so sends, forks, the HTTP API's `provision` object and `by --remote` carry it like the rest of the request. On the command line it is `--connector`, repeatable, on `run`, `fan`, `send`, `fork` and `spawn`:

```text
--connector CONNECTOR[@ACCOUNT][:MODE[:OPERATION,OPERATION...]]
```

| Flag | Entry |
|---|---|
| `github` or `github:read` | `{"connector": "github", "operations": ["*"], "mode": "read"}` |
| `github:write` | reads and mutations |
| `'github:write:issues.*'` | reads and mutations of the issue operations |
| `github:write+confirm:issues.create` | adds `"confirm": "allow"` |
| `github@work:read:issues.list,pulls.list` | two operations, as the `work` account |

`MODE` defaults to `read` and the operations to `*`. Operation globs are Anvil's: `*` matches any run of characters, dots included, `?` one character, and a glob may leave out the service prefix (`issues.*` matches `github.issues.list`). A connector id is the bundle's fleet id, its path under the bundle root folded to `[A-Za-z0-9_-]` (`shipping/v2` is `shipping_v2`). `confirm` is written only when it is `allow`; Anvil refuses any other value. In the SDK, `Provisioning { connectors: vec![GrantEntry::parse("github:read")?], .. }`; in a rig, a seat's `connectors = ["github:read"]`; in `branchyard.toml`, `[connectors] grants` for new branches that name none.

A grant needs a home private to the branch (`--isolated` or a sandbox provider), as secrets do: a request without one is refused before a branch exists.

**Narrower only.** A delegated child's grant is computed from what it asks for (`by spawn --connector`, `Spawn::connectors`, the MCP `spawn` tool's `connectors`, `branchyard.spawn(..., connectors=[...])`), else its seat's `connectors` (a seat without any gets none), else its parent's, and is always intersected with its parent's grant: each entry is cut to the operations, mode, confirmation and account both allow. An entry the parent allows nothing of is refused by name (`denied`), rather than silently dropped. A send that gives a delegated child a new grant is narrowed the same way, whoever sends it. A rig is checked when it is planned: a seat's grant must be within its parent seat's. Glob intersection is sound but not complete: when Branchyard cannot tell that one glob covers another it keeps neither, so a grant can come out narrower than strictly necessary, never wider.

### Keys and tokens

Each yard has an Ed25519 key ring: a private JSON Web Key Set, signing key first. Locally it is `.branchyard/gateway/key` (0600, in a 0700 directory), made on first use, and its public half is kept in step in `.branchyard/gateway/jwks.json`. `by gateway rotate-key [--keep N]` puts a new key first and keeps publishing N older ones (default 1), so tokens the previous key signed stay valid until they expire; `by gateway jwks` prints the public set. On a server the key file is `connectors.signing_key` (default `<data_dir>/gateway/key`) and the public set is served, without a token, at `GET /.well-known/jwks.json`.

Each turn of a branch with a grant gets one token, a compact JWS signed with `ring`'s Ed25519 (`alg: EdDSA`, `typ: JWT`, `kid`): `iss` is `branchyard:local:<yard id>` (the id is made once, in `.branchyard/gateway/yard-id`) or the server's `connectors.issuer` (default its URL); `aud` the gateway's `/mcp` URL; `sub` `local:<user>` locally, or on a server the principal that created the branch (recorded with it and inherited by its forks and children); `exp` at most an hour after `iat` and never after the turn's deadline (`--max-minutes`); a random `jti`; `by_tenant` (`local`, or the principal's tenant); `by_branch` (the branch, or on a server `<repo>/<branch>`, so one gateway can serve several repositories); `by_turn` (the turn's number, as a string); `by_grants` (always present). A turn's token has no `by_purpose`. A branch without a grant gets no token.

The person's **connect token** is the only other kind. `by connect` mints it (`Gateway::connect_token`) with the same `iss`, `aud`, `sub` and `by_tenant`, an empty `by_grants`, empty `by_branch` and `by_turn`, `by_purpose: "connect"`, and an `exp` at most ten minutes after `iat` (`CONNECT_TTL`). The rule both sides hold to:

- The gateway's connect routes (`/connect/start`, `/connect/api-key`, `/connect/status`; the callback is bound to the person by the `state` a start made) take **only** a connect token. A turn's token is refused `403 connect_token_required` before the vault is read or written, so a harness, or a prompt injected into one, can never start a connection or replace the person's stored `(sub, connector, account)` credential.
- `/mcp` takes **only** a turn's token (or another token without `by_purpose`, such as `person_token_granted`'s): a connect token is refused `403 turn_token_required` for `tools/list` and every tool call.
- A connect token living more than ten minutes, or any other `by_purpose`, is invalid (`401`).
- A connect token goes to `anvil connect` in a 0600 file and is removed afterwards; it is never placed in a harness's home.

### Each turn

Before a granted turn starts, and before its sandbox exists ([provisioning](provisioning.md#connectors)):

1. Every granted connector must be one the gateway serves, or the turn fails naming it (`connector slack is not served by the gateway (it serves: github)`). What is served is the bundle root's bundles, found as Anvil's fleet finds them (a directory with `air.yaml` or `air.json`).
2. Each granted bundle's package comes from `.branchyard/connectors/cache/<hash>/`, made with `anvil package harness <bundle> --out <dir> --workspace <root> --connector <id>` when missing; the hash is BLAKE3 over the bundle's files, so a recompiled bundle is packaged again.
3. The packages and `anvil connectors index --grants <file> --out INDEX.md --workspace <root> <bundle...>`'s index (`--workspace` makes Anvil name each bundle by its fleet id, so a nested `team/github` is indexed as `team_github`, as it is granted) replace `~/.branchyard/connectors/` in the private home. Only granted connectors are there.
4. The token is written to `~/.branchyard/gateway-token` (0600) and removed when the turn ends.
5. The harness gets `ANVIL_GATEWAY_URL` and `ANVIL_GATEWAY_TOKEN_FILE` (its home's path as it sees it, `/branchyard/home/...` in a Microsandbox guest), and one line in its instructions: *Connectors (github) are available through Branchyard's gateway: before using one, read …/INDEX.md and follow it; never ask for or use upstream credentials.*
6. The `provisioned` event lists the connectors, the two variable names and the files; never the token.

Packaging is behind the `Packager` trait ([`packager.rs`](../crates/branchyard/src/connectors/packager.rs)); `AnvilPackager` runs Anvil's command, and the tests use fakes.

### Configuration

`branchyard.toml` (or the user file):

```toml
[connectors]
gateway = "http://127.0.0.1:8931/mcp"     # the gateway's /mcp URL: tokens' audience; without it, connectors are off
bundles = "../connectors"                  # the bundle root (default: connectors/ at the repository root)
anvil = "node /opt/anvil/packages/cli/dist/bin-anvil.js"   # default: anvil
grants = ["github:read"]                   # for new isolated or sandboxed branches that name none
# sandbox_gateway = "http://192.168.127.1:8931/mcp"   # the gateway as a sandbox reaches it
# listen = "0.0.0.0"                       # what by gateway start binds, when not the URL's loopback host
# vault_key = "~/.config/branchyard/vault.key"        # default .branchyard/gateway/vault.key, made 0600
```

A server's configuration file takes `"connectors": {"gateway", "bundles", "anvil": [...], "issuer", "signing_key", "audit_file", "sandbox_gateway", "run_gateway", "listen", "vault_key"}`; see [server](server.md#connectors). A request that names connectors on a server without them is refused `403 connectors_not_configured`.

### Running the gateway

`by gateway start` runs `anvil serve mcp <bundles> --fleet --http <port>` with `ANVIL_INBOUND_AUTH_MODE=branchyard`, the yard's issuer, the gateway URL as audience, `ANVIL_INBOUND_JWKS_URI=file://…/.branchyard/gateway/jwks.json`, `ANVIL_AUDIT_FILE=.branchyard/gateway/audit.jsonl`, `ANVIL_VAULT_KEY_FILE` (made if missing: 32 random bytes, 64 hex characters, 0600) and `ANVIL_VAULT_DIR=.branchyard/gateway/vault`. It starts a supervisor in the background, in its own process group, which restarts the gateway when it exits (backing off up to 30 seconds), reads its audit log every half second, and records itself in `.branchyard/gateway/gateway.json` (pid and start time, so a reused pid is never mistaken for it); output goes to `.branchyard/gateway/gateway.log`. `--foreground` runs the supervisor in the terminal. `by gateway status [--json]` says whether it runs and listens, what it serves and which keys sign; `by gateway stop` ends the process group. Other variables, such as `ANVIL_ALLOWED_HOSTS` and `ANVIL_CONNECT_<CONNECTOR>_CLIENT_ID`, pass through from `by`'s environment.

A server with `"run_gateway": true` supervises one the same way beside `by serve`, stopped with it; without it, the server only signs tokens and reads the audit log of a gateway someone else runs.

`by connect <connector> [--account NAME] [--api-key-stdin] [--open]` mints your connect token (ten minutes, an empty grant, `by_purpose: "connect"`), and runs `anvil connect <bundles> <connector>` with it in a 0600 file (removed afterwards): Anvil asks the gateway for an authorization URL and prints or opens it, or submits a key read from stdin. The link goes to you; a harness never sees it.

### Audit

The gateway's audit log is read from where the last read stopped (`.branchyard/gateway/audit.cursor`, keyed by the file's identity, so a replaced or truncated log is read from its start; one reader at a time under a lock). Each complete line whose `by_branch` names a branch of this yard is recorded on it as a `connector_call` event, with the line's own time: connector, operation, decision, account, `by_turn`, `sub`, Anvil's `error_code` (as `reason`) and `rule`, the grant entry, upstream status, latency and `input_sha256`. Lines for other branches and yards are skipped. The log is read while a granted turn runs (every 250 ms, and once more when it ends), by the gateway supervisor, by `by log` (and `--follow`) and `by watch` before they read, and by a server's poller. `by log` prints `connector: github github.issues.create denied (policy_denied, 0 ms)`; `by log --json` gives `{"activity": "connector_call", "connector_call": {...}}`; `by watch` shows the latest call as what the branch is doing.

A crash between recording a line and moving the cursor records that one line again on the next read.

### Sandboxes

A sandboxed harness cannot reach the host's loopback, so a sandboxed branch with a grant gets `[connectors] sandbox_gateway` (server: `connectors.sandbox_gateway`) as `ANVIL_GATEWAY_URL`, and fails its turn naming that setting when there is none. The token's audience stays the gateway's canonical URL, which is what Anvil checks.

- **Microsandbox**: point `sandbox_gateway` at an address of the host the guest routes to, and have the gateway listen there (`listen = "0.0.0.0"` or that address). Anvil requires HTTPS for a non-loopback audience but not for the address a client dials, so the audience can stay `http://127.0.0.1:...`. Not checked on a KVM host.
- **Substrate**: point `sandbox_gateway` at a URL the actor's egress reaches, such as the gateway exposed through the cluster's routed ingress. Not checked against a cluster.

The target rule is that **a sandboxed branch may reach only the gateway**. It is **not enforced**: Microsandbox guests get the runtime's default network (egress restriction is an unimplemented capability, [providers](providers.md)), and Substrate's egress policy is not vendored. Until it is, a sandboxed harness can reach whatever its network allows; it still holds no upstream credential, and the gateway still enforces its grant.

### Delegation, rigs and the server

See [delegation](delegation.md#connectors), [rigs](rigs.md) and [server](server.md#connectors).

## Anvil side

Built in Anvil (ADR-0029 there, `docs/branchyard.md`): the `branchyard` inbound mode, grant checks in `execute()` before any upstream call, the vault and connect flow, the audit log, gateway mode in the generated CLI and the Python and TypeScript SDKs, `anvil package harness` and `anvil connectors index`, and the `examples/github-mini` fixture both repositories test against. Where Anvil's implementation refines the contract, Branchyard matches it:

- Every tool is on the wire as `<connector>__<tool>`, even when the gateway serves one bundle; the packaged SDKs and CLI handle that, the harness never sees it.
- Grant globs match the AIR operation id with or without its service prefix (`issues.*` and `github.issues.*` both match `github.issues.list`), `?` matches one character, and the first entry that allows a call picks its account.
- `by_grants` is required in every token (an empty list is allowed); a token whose `exp` is more than an hour (plus leeway) ahead is refused, and a connect token whose `exp` is more than ten minutes after its `iat`. `by_tenant`, `by_branch` and `by_turn` are strings of at most 256 characters.
- Connector ids are `[A-Za-z0-9_-]{1,64}` (fleet prefixes), and `confirm` may only be `"allow"`.
- The connect routes are `/connect/start`, `/connect/callback`, `/connect/api-key` and `/connect/status`; `anvil connect <workspace> <connector>` takes `--gateway`/`ANVIL_GATEWAY_URL`, `--token-file`/`ANVIL_GATEWAY_TOKEN_FILE` and `--api-key-stdin`, and every connect route but the callback needs the person's connect token (`by_purpose: "connect"`), never a turn's token.
- An SDK built with `anvil compile` rather than packaged may need `ANVIL_GATEWAY_CONNECTOR`; packaged ones (what Branchyard places) do not.
- The vault needs `ANVIL_VAULT_KEY_FILE` (0600) and takes `ANVIL_VAULT_DIR`.
- A package holds `SKILL.md`, `reference/`, `schemas/`, `examples/`, `python/`, `typescript/`, `bin/<connector>` (a Node.js 18+ script) and `harness.json` with the bundle hash. Branchyard keys its cache by its own hash of the bundle instead, which changes whenever the bundle does.
- Audit lines carry `input_sha256` (not `input_hash`), `error_code`, `rule`, `dry_run` and `trace_id` besides the contract's fields.

## Tests

- Unit: grant parsing, wire form, globs, intersection and narrowing chains (`branchyard-provision`, 6); key rings, rotation, signing and verification, bad key files, bundle discovery and hashing, audit line and time parsing, the gateway command, vault key and supervisor restarts (`branchyard`, 10); `[connectors]` parsing and rendering (`branchyard-setup`); `--connector`, `by gateway`, `by connect`, default grants and rig seats (`by`); the server's configuration file.
- Engine (`crates/branchyard/tests/connectors.rs`, 10, fake packager and the fake ACP agent): a granted turn sees its packages, index, URL and a 0600 token that verifies against the yard's JWKS with the contract's claims, no `by_purpose`, and an `exp` within the turn's deadline; a connect token carries `by_purpose: "connect"`, no grant, branch or turn, and lives at most ten minutes; a second turn reuses the cached package and gets a new token; the token appears in neither the event log nor the stored record, and the index is in the instructions; no grant, no token; an unserved connector, a missing gateway and a missing private home each refused by name; children narrowed by spawn, by seat and on a send; audit lines recorded once, a partial line kept for later, a rotated log read again; a sandboxed turn given the sandbox address or failed naming it.
- CLI (`crates/branchyard-cli/tests/connectors.rs`, 2): `by gateway start|status|rotate-key|jwks|stop` supervising a stand-in Anvil written in Python, a granted turn calling it through its token, `by log` and `by log --json` showing the two calls, `by connect` (with a connect token), and a refusal without a private home.
- Server (`crates/branchyard-server/tests/connectors.rs`, 2): the JWKS route, refusal without a gateway, the principal and `<repo>/<branch>` in a verified token, and audit lines recorded by the poller for that repository only.
- **Against Anvil** (`crates/branchyard-cli/tests/anvil_e2e.rs`, ignored by default; it skips with a message without `node`, `python3` and a built Anvil at `ANVIL_BIN` or `/home/user/anvil/packages/cli/dist/bin-anvil.js`): compiles `examples/github-mini` against its mock upstream, `by gateway start`, `by connect github --api-key-stdin`, then a branch granted `github:read` runs the fake ACP agent, whose turn runs the packaged Python SDK in gateway mode: listing issues returns the mock's two, creating one is refused `policy_denied` (`policy/grant_denied`), and `by log --json` shows two `connector_call` events; the turn's own token is refused `403` at the gateway's `/connect/api-key`. Run it with `cargo test -p branchyard-cli --test anvil_e2e -- --ignored`.

## What is not done

- Egress enforcement for sandboxes (above), and any check of the sandbox addresses on a KVM host or a Substrate cluster.
- A real harness using a connector: the end-to-end test drives the packaged SDK from the fake agent's shell.
- One gateway per host for several local repositories: each local yard runs its own (`by gateway` per repository) with its own issuer and keys.
- Removing a branch removes its home, and with it the placed packages; nothing else is tracked in `provisioned.json` for connectors, since the token file is removed when each turn ends.

## Catalog

[`catalog/connectors.toml`](../catalog/connectors.toml) is the starting list of connectors Anvil adopts from: 55 servers, generated from emdash's MCP catalog (`apps/emdash-desktop/src/core/primitives/mcp/api/catalog.ts`, Apache-2.0, pinned under `vendor/emdash/`). `by connectors catalog [--json]` prints it. Listing a connector grants nothing: a branch reaches one only through a grant and the gateway above.

Each entry has:

| Field | Meaning |
|---|---|
| `id`, `name`, `description` | emdash's key (with `_` as `-`), name and description |
| `kind` | `remote-mcp`: a Streamable HTTP MCP server at `url` (46 entries); `stdio-package`: a package run over stdio, with its `command`, `args` and `package` (such as `npx -y resend-mcp`; 7); `stdio-command`: a CLI's own MCP mode (`gt mcp`, `executor mcp`; 2) |
| `auth` | `server`: the remote server runs its own authorization, as the MCP authorization specification describes (usually OAuth), which the gateway's connect flow completes; `header`: a credential in an HTTP header; `env`: a credential in a variable; `none` |
| `credentials` | The header or variable names a credential goes in, each `required` or optional; never a value (emdash's placeholders are dropped) |
| `homepage` | The server's documentation |
| `source` | `emdash:<key>`, the entry it came from |

Nothing in emdash's catalog is a plain API yet; an OpenAPI or GraphQL contract Anvil compiles directly needs no catalog entry. To adopt one, Anvil reads the entry's URL or package as an MCP source (its ADR-0016) and produces the reviewed model; the catalog is not consulted at run time. The file is regenerated from the vendored source by `BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog`, and that test fails when the checked-in file differs from what the pinned source gives, so a re-pin of emdash shows every added, removed or changed server as a diff to review.
