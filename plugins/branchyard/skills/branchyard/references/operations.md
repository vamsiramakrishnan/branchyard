# Operations

All requests use `schema: "branchyard/v1alpha1"` and an `operation_id` UUID.
`action.op` selects `create_task`, `apply_graph`, or `cancel_task`. `branchyard
 describe` exports the Rust-generated JSON shapes. The API is pre-release; unknown
fields and variants fail rather than disappearing silently.

| Operation | Required fields under action | Meaning |
|---|---|---|
| `create_task` | `task_id`, `spec` | Admit a root task; one caller-generated identity |
| `apply_graph` | `root_id`, `expected_revision`, `edits` | Propose 1–64 edits against the ownership root's graph revision |
| `cancel_task` | `task_id`, `expected_revision`, `cascade` | Request cancellation against that task's revision |

A spawn edit has `kind: "spawn"`, `task_id`, `parent_id`, and `spec`.
Dependency edits have `kind: "add_dependency"` or `"remove_dependency"`,
`task_id`, and `depends_on`. Spawned IDs can be referenced in the same proposal;
the server must validate the final graph atomically, independent of list order.

A task spec contains:

- `goal`: 1–16384 UTF-8 bytes.
- `harness_profile`, `environment_profile`, `policy_profile`: registered server
  IDs. The policy binds root budgets, authorization and network access. These
  are not local executable names, image download URLs, or inline credentials.
- `workspace`: either `{"kind":"repository","repository_id":"registered-id",
  "commit":"full-lowercase-git-object-id"}` or
  `{"kind":"checkpoint","checkpoint_id":"UUID"}`. Each creates private task
  writes. It does not resume a native harness conversation.
- `components`: at most 32 `{"component_id":"registered-id",
  "access":"read_only"}` or `"exclusive_write"` bindings. Exclusive writes
  require server enforcement and verified revocation before reassignment.
- `required_capabilities`: at most 32 distinct capability IDs. Missing
  capabilities cause rejection; unknown is not supported.
- `limits`: `max_children`, `max_depth` (unsigned 16-bit), `wall_seconds`,
  `cpu_millis`, `memory_mib` (positive unsigned 32-bit). CPU uses millicores;
  these are requested ceilings, clamped only by explicit server admission rules.
  The server must reject an unsatisfiable request, never silently weaken it.

Before creating a child, inspect the current root's `graph_revision`; before
cancelling, inspect the selected task's `revision`. The client cannot check
existing topology or permission scope locally. The server must do that under
one transaction alongside root reservations and durable command insertion.

## Result discipline

Success is one JSON object on stdout. Errors are one JSON object on stderr.
Exit 0 means that CLI operation succeeded, not that a task finished. `doctor`
returns `execution_ready` as data; false is a reachable but unready server.
Exit 2: invalid input/config. Exit 3: remote/transport error. Exit 4: uncertain
submission. Exit 5: invalid, incompatible or oversized read response.

`reconcile --file` verifies the operation's request fingerprint. `operation ID`
only inspects an identity. On exit 4, keep the command unchanged. Retrying the
same saved command is permissible only with a server that enforces the specified
idempotency contract; this client does not retry automatically.

The HTTP surface is `/v1alpha1/info`, `/commands`, `/operations/{id}`,
`/tasks/{id}`, and `/tasks/{id}/events?after=N&limit=N`, all beneath the same
`/v1alpha1` prefix. Credentials come from the process environment. HTTPS is
required. `--allow-loopback-http` is an explicit fixture/tunnel option limited
to literal loopback IP addresses, not a general insecure mode.
