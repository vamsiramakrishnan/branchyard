# Manage workspaces from an external agent

The workspace manager lets an operator's agent use the same authenticated HTTP
actions as the browser console. It starts with a server credential, not a
delegation token from a running branch. The manager command refuses to start
inside a managed turn before reading its operator credential. The native in-task MCP still acts only on
its own branch and permitted descendants.

Start the standalone MCP binary with a token file:

```sh
branchyard-mcp manager --url https://branchyard.example --token-file /run/secrets/branchyard-manager
```

The token file's first line supplies the scoped bearer credential. An operator
may instead set `BRANCHYARD_MANAGER_TOKEN` in the child process environment. The
command has no plaintext token argument. Use `--ca-file` for a private CA.

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
