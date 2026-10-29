# Manage workspaces from an external agent

The workspace manager lets an operator's agent use the same authenticated HTTP
actions as the browser console. It starts with a server credential, not a
delegation token from a running branch. The manager command refuses to start
inside a managed turn before reading its operator credential. The native in-task MCP still acts only on
its own branch and permitted descendants.

Start the installed CLI's MCP server with a token file:

```sh
by mcp manager --url https://branchyard.example --token-file /run/secrets/branchyard-manager
```

The token file's first line supplies the scoped bearer credential. An operator
may instead set `BRANCHYARD_MANAGER_TOKEN` in the child process environment. The
command has no plaintext token argument. Use `--ca-file` for a private CA.
The installed `by` command reuses its global token-file and CA-file options,
including `BRANCHYARD_TOKEN_FILE` and `BRANCHYARD_CA_FILE`. `--json` formats startup
errors as JSON. Normal MCP messages always use the stdio protocol.
Manager startup does not open a yard or load the current directory's task defaults.
The standalone `branchyard-mcp manager` accepts the same options. Existing
engine launches with `by mcp --root DIR --branch NAME` keep their restricted
delegation tools. `by mcp --list-tools` still lists those delegation tools.

`BRANCHYARD_MANAGER_TOKEN` is reserved for the external operator. Harness
environments remove it even in inherited mode and with explicit keep or set
rules. Sandbox `--pass-env` refuses it. Provisioned environment entries cannot
add it to a harness or alias it into another secret. Keep its token file outside
managed workspaces.

Knowledge projects use `source: {"kind": "knowledge"}` and
`result_mode: "artifacts"`. They require no user Git repository. Code projects
reference a repository already granted by the server operator. Mixed work keeps
code integration and deliverable acceptance separate.

Use `list_harnesses` and `worker_inventory` to check reported readiness before
selecting an agent. Catalog presence does not establish live qualification.
Use the listed manager tools to discover projects, create workspaces, start and
continue native sessions, inspect immutable sources and artifacts, request
bounded document previews, accept deliverables, and observe operations. Each
mutation requires an `idempotency_key`: use one key for one intended action and
reuse that key after a lost connection. A new key is a new intended action.

Work continues on the server if this MCP client disconnects. Inspect the returned
operation ID; a long-poll response that is still nonterminal means work continues.
Observing an operation does not cancel it. Cancellation is an explicit request
and does not undo existing side effects.

Every call still needs the ordinary server scope, tenant/repository access and
workspace membership. The manager accepts no arbitrary request URL, shell
command, host path or download destination. Artifact inspection returns metadata;
original downloads use the ordinary authorized content endpoint. Previewing a
document does not establish visual approval. Upstream connector consent and
worker IAM remain separate grants.

Each manager tool's text and structured JSON payload has a 32 KiB budget. A
larger success or API error becomes `manager_output_too_large`, with bounded
identifiers, operation state and continuation cursors where present. Oversized
metadata is explicitly marked as omitted. JSON is never cut mid-value. Preview
defaults to 25 rows. Request fewer rows or a different page, download the original
through its authorized endpoint, or ask a session to compute a focused result.
An oversized mutation response can follow a successful action. Inspect its
operation and reuse the original idempotency key when reconciling.

## Pull-request service integration

The server service in `console_pr` reuses the installed `by pr --json` command for
candidate checks, pushing and PR creation/update. It refreshes the existing
engine observations with `by show --refresh --json` and obtains comments, reviews
and check detail through the operator-selected GitHub CLI. Request bodies cannot
select an executable, working directory, repository credential, force push or
check bypass.

The queued worker creates an `AuthorizedBranch` from its resolved yard and branch,
with a callback that rechecks current workspace/repository/scope authority. A read
context cannot publish. The worker must hold the existing durable branch lock and
retain its claim while executing the service. The service checks the admitted
candidate before publication and rechecks authority before returning the result.

Subprocess output is bounded and never included verbatim in errors. Deadlines,
capture failures and malformed output after a possible publication report
`unknown_effect`; inspect forge state before retrying. They are not permission
to replay a push automatically. Native checked merging remains the existing
engine operation, separate from creating a PR.

## Verification

Hermetic verification on 29 October 2026 passed the client route/idempotency
checks, manager argument and real stdio protocol checks, existing delegation and
protocol suites, and PR execution with real engine candidates plus fake forge
executables. The PR tests cover revocation before and during publication,
stale candidates, malformed output, output limits and process deadlines.
Clippy for the changed crates and rustdoc with warnings denied passed. See the [verification record](evidence/console-management-2026-10-29.json) for
the exact suite scopes and log digests.
Not done: integrated workspace HTTP routes, the UI, or a live GitHub publication
through this service.

Hermetic followup verification on 29 October 2026 passed the installed manager
startup guard, credential scrubbing with a real fake-harness process, reserved
secret-source and sandbox pass-through guards, bounded manager outputs over real
MCP stdio, and the shipped skill, projection and command-surface checks. The CLI
argument, help and reference checks passed, along with production Clippy,
rustdoc and the fast Python gate. The verification record states the exact
source scopes. Not done: live model, cloud or forge calls in this followup.
