# Agent Substrate

[Agent Substrate](https://github.com/agent-substrate/substrate) runs many mostly idle sandboxes ("actors") on fewer warm Kubernetes pods ("workers"). It suspends an idle actor to object storage and resumes it on any free worker. Branchyard uses it as an optional sandbox provider that runs harnesses: `by run --provider substrate` gives each turn its own actor. The provider is **unqualified**: it has run only against the in-process fake described below, never against a cluster, and stays unqualified until it passes the runtime gates in the [implementation plan](implementation-plan.md) and the steps in [live testing](testing-live.md#6-agent-substrate-cluster).

Reviewed at Substrate revision `1d7ca8ced056192a1801d6565251adcaab3eb0c9`. Only the API contract is vendored: [`ateapi.proto`](../vendor/substrate/pkg/proto/ateapipb/ateapi.proto), unmodified. The router (atenet), the node agent (atelet) and the egress gateway are not, so everything this page says about them beyond what the proto states is an assumption for qualification to confirm.

## Division of responsibility

| Concern | Agent Substrate | Branchyard |
|---|---|---|
| Unit of work | Actor: an instance of a template, suspended and resumed | Task, run, attempt, session, workspace and candidate, each with its own identity |
| Topology | None; actors are independent | Parent/child graph proposed at runtime, with budgets and delegation envelopes |
| Execution | gVisor or microVM sandboxes on Kubernetes worker pods; an actor runs its template's entry point | The Branchyard bridge, as that entry point, starts the harness and relays its pipes |
| State | Memory and filesystem snapshots in object storage; tags name them | Durable commands and domain state; the turn's actor is journaled so recovery can delete it |
| Harness awareness | None; a harness is an OCI image | Drivers with explicit fresh/resume/fork and permission handling |
| Code integration | Out of scope | Git bundles in and out of the actor, validated candidates and guarded promotion |

Substrate describes itself as "not an SDK for building agents." Branchyard is the harness-aware, merge-aware layer above such a runtime. It does not reimplement Substrate.

## How it is built

- [`branchyard-substrate`](../crates/branchyard-substrate/src/lib.rs) generates the `Control` client at build time with `tonic-prost-build` and a pinned `protoc` from `protoc-bin-vendored` (host `protoc` installations are ignored). `Actors` maps the actor lifecycle asynchronously; `SubstrateProvider` implements the synchronous `SandboxProvider` on top of it and of the bridge; `transfer` moves code; `template` builds the actor template; `fake` (cargo feature `fake`) is the test cluster. No Substrate Go code is translated. See [vendoring](vendoring.md#agent-substrate-generate-from-the-contract-do-not-translate-the-runtime).
- [`branchyard-bridge`](../crates/branchyard-bridge/src/lib.rs) is the in-actor exec endpoint: a static-friendly binary on `std`, `libc` and `ring`, and the host-side client that turns its connection into a `Process`.
- The engine's `Provider::Substrate(SubstrateOptions)` and the CLI's `--provider substrate` use both ([placement](../crates/branchyard/src/placement.rs)).

## Operation mapping

| Branchyard operation | Substrate call | Notes |
|---|---|---|
| `capabilities()` | `GetActorTemplate` | Derived from `snapshot_config.on_commit`; `exec` only when a container sets `BRANCHYARD_BRIDGE_KEY` |
| `ensure(spec)` | `CreateActor`, then `ResumeActor` | An existing actor is adopted only if it came from the same template. Then a new attempt credential is minted and the bridge's health check is polled through the router. Mounts, an image or limits in the spec are refused: the template fixes the last two and an actor sees no host paths |
| `inspect` | `GetActor` | Fails if the name now refers to a different UID |
| `exec` | none: the bridge, through the router | See [the bridge](#the-bridge) |
| `stop` | bridge `Shutdown` and `EndAttempt`, then `SuspendActor` | A full-scope suspend would otherwise keep the processes, frozen, and resume them later; `stop` must end them |
| `checkpoint(name)` | `CreateTag` with `TAG_SCOPE_ATESPACE` | Requires a stopped actor; never suspends implicitly. A retry adopts its own earlier tag |
| `branch(checkpoint, name)` | `CreateActor` with `source_tag` | New name, UID and credentials. The child starts stopped; `ensure` starts it |
| `revert` (`Actors` only) | `RevertActor` | Returns to the **latest** suspend only; not restore to a chosen checkpoint |
| `destroy` | bridge `EndAttempt`, then `DeleteActor` with a UID precondition | Idempotent; cannot delete a newer actor that reused the name |

## Declared capabilities

| Capability | Declared | Reason |
|---|---|---|
| Exec with process pipes | Only for a template that runs the bridge | `ateapi` has no exec or attach call; the bridge provides it |
| Routed ingress | Yes | `atenet-router` activates suspended actors on request (not in the vendored source) |
| Checkpoint | Template scope, crash consistency, portable | Suspend captures without the workload's cooperation; object storage makes it portable |
| Branch | Same as checkpoint | Tag, then create from tag |
| Restore to a chosen checkpoint | No | Only revert-to-latest exists; use `branch` instead |
| Share | No | No API for state shared between actors |
| Host mounts | No | Refused in `ensure`; code crosses by [transfer](#git-in-and-out) |

`FULL` commit scope maps to a full snapshot. `DATA` maps to a disk snapshot: resuming it cold-boots or restores the template's golden snapshot, so no process state from the source actor survives. An unset scope uses the API's documented default, `FULL`. An unrecognized scope value is an error, not a guess.

## The actor template

`branchyard_substrate::template::bridge_template` builds the `ActorTemplate` to pass to `CreateActorTemplate`:

- One container whose `command` is `branchyard-bridge serve --listen 0.0.0.0:8080 --identity /run/branchyard/identity --state /var/lib/branchyard-bridge/attempts`, and whose image, pinned by digest (`Container.image`, proto line 946 onward), also holds `git` and every harness executable. The bridge execs the harness in the same container.
- `BRANCHYARD_BRIDGE_KEY` in `Container.env` (line 986): the **public** half of the host's signing key. Template environment is shared by every actor made from it, which is why nothing secret is put there.
- A `system_info` volume with an `actor_metadata` projection (`SystemInfoVolumeSource`, `ActorMetadataDataSource` and `ActorMetadataField`, lines 1174-1240) of the actor's atespace, name and UID to `/run/branchyard/identity/{atespace,name,uid}`. The proto says atelet regenerates these files "on every Run/Restore", so a branched actor's bridge sees its new identity even when its memory came from the parent. The bridge re-reads them on every connection.
- A `wakeup_probe` on `GET /healthz`, port 8080 (`ContainerWakeupProbe`, line 1072), so `ResumeActor` returns only once the bridge listens.
- Full-scope pause and commit snapshots.

The template's name goes in `--substrate-template`. `branchyard-bridge keygen --out FILE` writes the host's signing key (PKCS#8, mode 0600) and prints the public key for the template.

Run the bridge under an init that reaps orphans (such as `tini`) when it is the container's process 1; it does not reap processes it did not start.

## The bridge

The host reaches the bridge through the router: `--substrate-router` is a URL template with `{atespace}` and `{actor}`, such as `http://router.example/{atespace}/{actor}/`. The vendored source says only that atenet routes requests to an actor's worker (`WorkerAssignment`, proto line 618: "atenet reads worker_pod_ip on the request-routing path"), not how an actor is addressed, so the template is configuration, to be fixed during qualification.

### Transport

An HTTP/1.1 `GET` upgraded to WebSocket (RFC 6455), because routers that forward actor ingress forward WebSocket upgrades. The request names the subprotocol `branchyard-bridge.v1` and carries `Authorization: Bearer <credential>`. The bridge refuses before the upgrade: 401 with the reason for a missing, malformed, forged, expired, foreign, superseded or ended credential; 426 for another WebSocket version or subprotocol; 503 when it cannot read its identity or write its attempt state. A plain `GET …/healthz` answers 200 without a credential. After the upgrade, every binary message is one frame. Plain `http` only: neither the router hop nor the `Control` endpoint uses TLS yet.

### Frames (protocol version 1)

A frame is a one-byte tag, then its fields: big-endian integers, `bytes` as a `u32` length and the bytes, lists as a `u32` count and the items, optional integers as a `0`/`1` byte and the value. A frame with an unknown tag, a short field or trailing bytes is an error: a peer never guesses at a newer frame. A new frame or field is a new version, negotiated by the subprotocol. The codec is [`protocol.rs`](../crates/branchyard-bridge/src/protocol.rs).

| Tag | Frame | Direction | Meaning |
|---|---|---|---|
| `01` | `Exec {argv, cwd, env}` | request | Start `argv` without a shell, in `cwd`, with `env` added to the bridge's environment (minus its own `BRANCHYARD_BRIDGE*` variables), in a new process group |
| `02` | `PutFile {path, mode}` | request | Write the following `Data` frames, up to `End`, to an absolute path, creating parents; answered by `Done` |
| `03` | `GetFile {path}` | request | Answered by `Data` frames and `End` |
| `04` | `PutTree {path}` | request | Write the following tree under a directory; answered by `Done` |
| `05` | `GetTree {path}` | request | Answered by the tree |
| `06` | `EndAttempt` | request | End the credential's attempt and tear down its processes; answered by `Survivors` |
| `07` | `Shutdown` | request | Tear down every process the bridge started; answered by `Survivors` |
| `10` | `Stdin(bytes)` | client, during exec | Bytes for stdin |
| `11` | `CloseStdin` | client, during exec | End of file on stdin |
| `12` | `Kill` | client, during exec | SIGKILL to the launched process only |
| `13` | `Teardown` | client, during exec | Name the group's live members, then SIGKILL the group; answered by `Survivors` |
| `20` | `Started {pid}` | bridge | The exec started |
| `21`, `22` | `Stdout(bytes)`, `Stderr(bytes)` | bridge | Output, at most 64 KiB per frame |
| `23`, `24` | `StdoutClosed`, `StderrClosed` | bridge | End of an output pipe |
| `25` | `Exited {code?, signal?}` | bridge | The launched process was reaped; sent independently of the pipes, which descendants may hold open |
| `26` | `Survivors [name]` | bridge | Command names of the processes found and killed |
| `27` | `Failed {kind, message}` | bridge | A request failed; `kind` maps to `NotFound`, `PermissionDenied`, `InvalidInput`, `AlreadyExists`, `Unsupported` or other |
| `28` | `Done` | bridge | A put finished |
| `30` | `Entry {kind, path, mode, target}` | either | A tree entry (directory, regular file or symbolic link); a file's content follows as `Data` frames and `End`; a tree ends with `End` |
| `31`, `32` | `Data(bytes)`, `End` | either | File content |

The client's first frame is its connection's only request. Execs may run concurrently (each has its own connection and process group, and at most 64 run at once), because teardown checks probe the sandbox while a process runs; the engine runs one harness per actor. A closed connection tears its exec down, which is how dropping a `Process` ends it; a connection that ends before `Exited` makes `wait` return an unknown exit status (no code, no signal). Trees are treated as untrusted at both ends: paths must be relative and plain, links are recreated and never followed, and nothing is written through a link.

## Identity: per-attempt credentials

Each attempt gets its own credential. In the engine an attempt is one turn, which also has its own actor; `SubstrateProvider::begin_attempt` rotates the credential of a longer-lived actor.

- **Minting.** The host signs `byb1.<hex payload>.<hex Ed25519 signature>`. The payload names the actor's atespace, name and UID, an attempt label, a sequence number (microseconds since the epoch, strictly increasing per provider) and an expiry (six hours by default). The host's key never enters the actor; the bridge holds only the public half, so nothing inside the sandbox can mint a credential. ([credential.rs](../crates/branchyard-bridge/src/credential.rs))
- **Checking.** The bridge accepts a credential only if the signature verifies, it has not expired, it names the identity projected into this actor (so a recreated or branched actor, which has a new UID, never accepts its predecessor's), and its sequence number is above every ended attempt and not below the newest one seen. A newer attempt ends every older one and tears down their processes.
- **Ending.** `EndAttempt` (sent by `end_attempt`, `stop` and `destroy`) marks the attempt ended. The state is written, and synced, to the state file before the connection that changed it is answered, so an ended attempt stays ended across a bridge restart or a suspend and resume.
- **Why not Substrate's identity APIs.** `MintActorJWT` and `MintActorCertificate` (proto lines 66-77 and 1543-1628) issue credentials that assert the **actor's** identity (`sub` = `atespaces:<atespace>:actors:<name>`) and are "called by the egress gateway" for the actor's outbound requests. They authenticate an actor to others, not a caller to the actor; verifying such a JWT in the bridge would need the issuer's keys (OIDC discovery through egress) and would accept any holder of the `Control` API's mint permission, and neither API can revoke a token when an attempt ends. `CreateActor` takes no per-actor environment or volume (`Actor`, line 361; `CreateActorRequest`, line 1391), so a per-attempt secret cannot be injected at creation either. The design therefore uses a Branchyard-signed credential, bound to the Substrate-issued UID that the `actor_metadata` projection supplies.

## Git in and out

An actor cannot mount the worktree, so each turn copies it ([transfer.rs](../crates/branchyard-substrate/src/transfer.rs)). Bundles over the bridge were chosen over a Branchyard-served git remote because they need no listener on the host, no route from the actor back to it, and no credential for one; the bridge connection already exists and is authenticated.

**In**, before the harness starts: the host snapshots the worktree as a commit on top of `HEAD` (every tracked and untracked, non-ignored file, via a temporary index), bundles that commit and `HEAD` with its history, and sends the bundle with `PutFile`. In the actor, git creates a new repository at the workdir, fetches the bundle, checks out the snapshot on the host's branch name and resets the branch and index to `HEAD`. The actor then holds the host's working files with `HEAD` at the host's commit. The branch's private home is sent with `PutTree`.

**Out**, after the harness exits or is killed: git in the actor snapshots its working files the same way on top of its `HEAD` and bundles that one commit against the base. The host fetches it with `transfer.fsckObjects`, checks that the worktree still holds exactly what was sent (otherwise the result is not applied and the turn warns), and applies the difference between the two snapshots to the worktree's files with a two-tree `read-tree -m -u`: additions, changes, deletions, modes and symbolic links. The host's index, refs and branch are untouched, so the engine's existing snapshot, diff and merge path records the candidate exactly as for a local harness. The home comes back with `GetTree` into a new directory that replaces the private home.

Only commits and files cross. The bundles carry objects reachable from the two snapshots; the actor's repository is new, so no host configuration, hook, remote or credential is in it. On the host both steps run in a staging repository whose object store falls back to the host repository's (git alternates), so the host's refs and object store are never written. The engine keeps it at `.branchyard/transfer/<actor>` and deletes it when the turn ends, or recovery does. Ignored files cross in neither direction, a file the harness writes outside the workdir never comes back, and a symbolic link comes back as a link. Commits the harness made in the actor are flattened into the working-tree difference; their messages are lost. The actor needs `git`.

## In the engine and the CLI

```sh
branchyard-bridge keygen --out ~/.config/branchyard/bridge.key    # prints the public key
by run "Fix the flaky parser test" --provider substrate \
  --substrate-endpoint http://127.0.0.1:8080 \
  --substrate-router 'http://127.0.0.1:8081/{atespace}/{actor}/' \
  --substrate-template by-claude --substrate-key ~/.config/branchyard/bridge.key \
  --pass-env ANTHROPIC_API_KEY --check "cargo test" --yes
```

`--substrate-atespace` (default `default`), `--substrate-workdir` (default `/workspace`) and `--substrate-home` (default `/branchyard/home`) are optional; the flags work on `by run`, `by fan` and `by fork`, and a send keeps the branch's provider. Each turn:

1. Journals the actor's name as the turn's `sandbox` step, then creates and starts it and mints the attempt's credential.
2. Copies the worktree and the private home in, and runs the harness in the workdir with `HOME` and the `--pass-env` variables on top of the image's environment.
3. When the turn ends, copies both back and deletes the actor. A failure to bring the result back is a warning on the turn; the actor is deleted regardless.

Delegation is refused to a sandboxed harness, as for Microsandbox. If the engine dies mid-turn, the bridge tears the harness down when its connection closes, and `Yard::recover` deletes the actor named by the `sandbox` step (with only the `Control` API; the key is not needed) and the turn's staging directory, and says so in the `recovered` event. What the harness changed in that actor is lost: recovery does not bring it back.

## Testing

The fake in `branchyard_substrate::fake` serves `Control` over real gRPC, runs a router that forwards `/actors/<atespace>/<actor>/<rest>` to the actor's bridge as `/<rest>` (WebSocket upgrades included, 503 for an actor that is not running), and "runs" an actor from a bridge template as a real `branchyard-bridge` process on this host with the template's key, identity files written as atelet would, and a per-actor directory standing in for its root filesystem (kept across suspend, copied by a tag). Against it, hermetically:

- the bridge's codec, WebSocket framing, credentials and trees, and the binary over TCP: stdio, exit status, teardown naming survivors, files and trees, and refusal of missing, forged, expired, foreign, superseded and ended credentials, across a restart;
- every `branchyard_sandbox::conformance` check through `SubstrateProvider`, in the suite's mode for providers without mounts, where the two mount checks require a mount to be refused;
- attempts (rotation, ending, suspend and resume, a branch refusing its parent's live credential) and a reused name never acted on;
- the git round trip (additions, changes, deletions, a binary file, mode changes, links, a harness commit, files written outside the worktree and ignored files never coming back, the host repository's refs, objects and config unchanged, a concurrently changed worktree refused) and the home round trip;
- the engine and the built `by` running the fake ACP agent in an actor, the candidate merging, the home and session carrying across sends, no process left running, delegation refused, and a killed engine's actor deleted by recovery.

The ignored tests in [`tests/cluster.rs`](../crates/branchyard-substrate/tests/cluster.rs) run the conformance checks, attempt rotation and a worktree round trip against a real cluster; [live testing](testing-live.md#6-agent-substrate-cluster) says how.

## Gaps and required decisions

1. **Router addressing and activation are unverified.** The URL template, whether the router forwards WebSocket upgrades and long-lived connections, whether it activates a suspended actor on a request, and whether it adds its own authentication all come from outside the vendored source.
2. **No TLS.** The `Control` endpoint and the router URL are plain `http`, so credentials and code cross the network in the clear; use a port-forward or a trusted network until TLS is added. A credential is short-lived and bound to one actor and attempt, but can be replayed by whoever observes it until its attempt ends.
3. **Secrets inherited by branches.** A full-scope branch copies the source's memory and root filesystem, including passed variables and the harness's home. Use Substrate's egress credential injection so secrets never enter the guest, or checkpoint before any credential is present. Otherwise the design's "no secret inheritance" gate fails.
4. **No quiescence handshake.** Checkpoints are crash-consistent. A harness must reach a quiet point (no in-flight tool call) before `stop`. The adapter enforces only that the actor is stopped.
5. **Fencing covers delete only.** Suspend, resume and revert take a name without a UID precondition. `inspect` detects replacement, but a check followed by an action can still race.
6. **Attempt state lives in the actor.** A process in the actor with write access to the state file could reset it and revive its own ended attempts; it still cannot mint a credential or accept one issued for another actor. The guest clock decides expiry.
7. **Work lost on engine death.** Recovery deletes the actor and the turn's staging directory without bringing the actor's files back.
8. **Kubernetes.** Substrate requires a cluster. It stays optional; Kubernetes is not on Branchyard's default per-spawn path.
9. **Fork is upstream work.** Substrate lists actor forking from a state root on its roadmap. Revisit `branch` when it lands.

## Qualification steps

1. Install Substrate on kind (`hack/create-kind-cluster.sh`, `hack/install-ate-kind.sh`) at the pinned revision.
2. Build an actor image with `branchyard-bridge`, git, `sh` and the harness, create the template, and run the ignored cluster tests, as [live testing](testing-live.md#6-agent-substrate-cluster) describes.
3. Measure create, resume, suspend, tag and branch separately, cold and warm. Record them as the implementation plan describes.
4. Verify that tag readiness, UID preconditions, identity projection on restore, router forwarding and egress policy behave as the fake assumes. Correct the adapter or the declaration where they differ.
5. Update [validation](validation.md) with the exact revision and results before claiming support.
