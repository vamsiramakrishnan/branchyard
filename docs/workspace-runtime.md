# Workspace runtime interactions

The workspace console uses the branch's existing execution provider for terminal
and development-app access. A branch on a remote provider never opens a terminal
on the control server as a fallback.

`Yard::workspace_runtime(branch)` takes the existing branch lease. The branch must
be idle. This keeps a terminal's file writes separate from an agent turn. A local
branch uses the local process provider. A remote branch must already have an
allocation retained with pause enabled. Opening a missing or expired allocation
fails with a reason instead of starting an empty replacement.

The execution handle uses the same placement code as agent turns. Remote
worktree and home transfer, retained-sandbox ownership, eviction and expiry stay
with that code. Closing a helper copies files back and parks the allocation.
Files edited in a terminal become a checked candidate on the next agent turn;
terminal access does not automatically approve or merge those edits.

The latest native-start journal identifies the owning worker host and boot.
Attachment is refused when that identity is missing or belongs to another
worker, even if an identically named worktree exists on the control server.
`workspace_runtime_state` reports the owner and an unavailable reason without
resuming it. A distributed deployment must route the request to that worker.

## Resource lifecycle

`Yard::workspace_runtime_action(branch, action, operation_id)` controls an idle
retained allocation. The caller passes the stable ID of its durable server
operation. The engine journals the allocation before invoking the provider and
records the observed result. Retrying that ID returns the same result. Reusing
it for a different action fails.

An interrupted action whose allocation claim cannot be confirmed fails closed
for reconciliation. It does not steal a cleanup owner's allocation or guess
that a missing row permits replay. The original allocation remains named in
the operation intent for inspection.

The engine checks the current lease immediately before provider effects.
Restoring a claimed allocation row checks the branch incarnation and lease
generation in the same database transaction. A replaced lease cannot publish
an old allocation into the current branch's retained state.

| Action | Provider operation | Retained data |
|---|---|---|
| `Pause` | Pause on the current node | Provider memory and disk remain |
| `Suspend` | Stop with the provider's retained-state semantics | Portability depends on the provider |
| `Resume` | Resume that allocation | No model turn starts |
| `Destroy` | Release the idle allocation | Task history, host worktree, inputs, artifacts and checkpoint records remain |

Pause and resume require the provider's advertised capability. A local branch
has no remote allocation to pause or destroy. Missing resources and unsupported
actions stay explicit; changing workspace metadata is not evidence that a
provider performed an action.

## Terminal service contract

`Terminal::start` launches a fixed operator-approved shell through a real Unix
PTY in the selected execution site. Python's standard `pty`, `termios`,
`selectors`, `json` and `base64` modules provide the bridge. The browser terminal
renderer is separate from this service.

The server must require its shell-execution setting, the caller's `run` scope
and workspace editor access before opening a terminal. It supplies an
authorization callback which rechecks current membership and credential
revocation. The service checks that callback before actions and while idle.
Closing or revoking a terminal stops its PTY process group and releases the
branch lease. The existing local provider cannot stop descendants that
deliberately escape their process group; stronger isolation belongs to a real
sandbox provider.

Local helper and PTY groups register with the existing service registry using
PID and start-time identities. Its reaper can reclaim them after their owning
server stops. Remote processes retain the existing sandbox reclaimer. A lost
branch lease stops interactive work and leaves provider recovery in charge of
the allocation, instead of copying stale files back.

Input frames are limited to 16 KiB. The replay buffer retains 1 MiB and returns
at most 64 KiB per read. Cursors count bytes, and readers are told when older
output was evicted. Resize updates the actual PTY; it is not only a browser
layout change. Output remains bytes so terminal escape sequences and UTF-8
boundaries are preserved.

## Development-app previews

A `PreviewPort` comes only from a leased branch's reserved `BRANCHYARD_PORT`.
It cannot be constructed from a browser-selected host or URL. The helper runs
at the workspace's provider and contacts that port on its own loopback
interface. Remote previews never contact the control server's loopback.

The relay supports GET and HEAD, normalized relative paths and responses up to
1 MiB. It follows no redirects, forwards no browser credentials or cookies, and
returns only selected response headers. Each request needs workspace access.
Serve active applications from a separate preview origin with its own scoped
access, not the console origin. WebSocket upgrades, arbitrary ports and
write-method proxying are not supported by this relay.

The HTTP authorization, routing and origin configuration are integration
responsibilities. The core handle does not grant a browser caller authority.

## HTTP gateway integration

`branchyard_server::console_runtime::RuntimeGateway` supplies the terminal HTTP
adapters without depending on the console's storage implementation. Its resolver
receives the authenticated `Caller`, workspace and session IDs, and the requested
access. It must read current credential validity, tenant policy, workspace role,
session binding and operator shell policy before returning a `Yard` and branch.
The gateway separately enforces `read` or `run` scope. Terminals require an
editor grant and the operator's `allow_client_commands` setting.

Mount `console_runtime::router(gateway)` behind the existing authentication
middleware. The router also refuses a missing authenticated caller. Call
`gateway.close_all()` during graceful shutdown. The resolver runs again during
every terminal request and from the running PTY's authority callback. A changed
session-to-branch binding closes its old terminal. Another editor cannot reuse
the creator's terminal handle; they can open their own after its writer lease
ends.

For example, open a terminal with `POST
/v1/workspaces/WORKSPACE/sessions/SESSION/terminals`, an `Idempotency-Key` header
and `{"rows":24,"columns":80}`. The executable, Python interpreter, environment
and working directory come from operator configuration and the owning runtime.
Unknown body fields are refused. A repeated opening request returns the same
handle while its bounded process-local record remains. It can report `opening`
while another identical request is starting the PTY.

Input uses `POST .../terminals/ID/input` with
`{"sequence":1,"data":"ZWNobyBoZWxsbwo="}`. `data` is standard base64 and
decodes to at most 16 KiB. Sequences start at one. Retrying the last identical
sequence acknowledges it without writing it again. Reusing a sequence with
different bytes or skipping a sequence fails. If a write has an unknown outcome,
the terminal closes; inspect the workspace before explicitly opening another.
The service does not replay shell input after an interrupted write.

Read `GET .../terminals/ID/output?after=0` for base64 output and a byte cursor.
The response identifies its encoding, closed state and any buffer truncation.
`POST .../resize` accepts rows and columns, and `POST .../close` is idempotent.
Successful terminal responses use `Cache-Control: private, no-store`.

Terminal capacity includes pending starts and retained closed records. Idle
terminals expire, and closed records can be evicted when capacity is needed.
Handles, opening-request bindings, sequence acknowledgements and replay buffers
are ephemeral. After eviction or server restart, an old handle returns
`410 terminal_lost`. Opening a new terminal requires an explicit client action;
no shell or input is replayed. The existing process registry handles reclamation
after a crash.

The runtime status response reports provider state, owner-host readiness,
resource capabilities and terminal policy. Readiness is a hint; opening still
checks the actual branch lease and provider. Source originals and accepted
deliverables remain independent of resource lifecycle changes.

Lifecycle HTTP requests require an `Idempotency-Key` and an injected durable
queue callback. They return the admitted operation. Without that callback the
route reports `runtime_control_unavailable`. The worker passes its durable
operation ID to `execute_lifecycle`, which rechecks authority before invoking the
core provider action. The gateway does not create another execution queue or
call a provider from the request handler. Active app HTTP relays still require
the embedding server's separate preview origin.
