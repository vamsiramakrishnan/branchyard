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
