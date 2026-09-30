# Connectors

A harness on a branch often needs GitHub, Slack, Linear, Google or an internal API. Branchyard does not give it MCP server configurations or upstream credentials. It gives it a **skill and an SDK per granted connector**, and one gateway to call them through. The harness writes code (bash, Python, TypeScript) against the SDK; the gateway holds every upstream authorization, enforces what the branch was granted, and records each call.

Connectors are compiled by [Anvil](https://github.com/vamsiramakrishnan/anvil) (Apache-2.0): from an API contract (OpenAPI, GraphQL, gRPC, SOAP, Discovery, OData) or an existing MCP server (adopted as a source, ADR-0016 there), Anvil produces one reviewed model (AIR) and projects it into a skill, a CLI, SDKs and an MCP server that agree on what each operation does.

Status: **design, being built.** This page is the contract both repositories build to; each section says which side owns it.

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

- `[connectors]` in `branchyard.toml`: where the gateway is and which bundles it serves; grants per task (`--connector github:read`), per seat, and per rig.
- A delegated child's grant is at most its parent's, like the rest of the delegation envelope.
- Provisioning mints the turn's token, writes the token file, places the granted packages and the index, sets the two variables, and adds a line to the harness's instructions pointing at `INDEX.md`.
- A sandboxed branch may reach only the gateway.
- `by gateway` supervises a local Anvil gateway; a server runs one beside `by serve`.

## Anvil side

- The SDK and CLI gateway mode above (Python and TypeScript first).
- The `branchyard` inbound mode (EdDSA, JWKS by URL or file) and principals built from token claims, with grant checks per operation before any upstream call.
- The vault and connect flow.
- `anvil package harness` and `anvil connectors index`.
- The audit log.
