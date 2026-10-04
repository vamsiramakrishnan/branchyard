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
- [`branchyard-bridge`](../crates/branchyard-bridge/src/lib.rs) is the in-actor exec endpoint: a static-friendly binary on `std`, `libc`, `ring` and `rustls` (with `ring`, for TLS), and the host-side client that turns its connection into a `Process`.
- The engine's `Provider::Substrate(SubstrateOptions)` and the CLI's `--provider substrate` use both ([provider](../crates/branchyard/src/providers/substrate.rs), [placement](../crates/branchyard/src/placement.rs)).

## Operation mapping

| Branchyard operation | Substrate call | Notes |
|---|---|---|
| `capabilities()` | `GetActorTemplate` | Derived from `snapshot_config.on_commit`; `exec` only when a container sets `BRANCHYARD_BRIDGE_KEY` |
| `ensure(spec)` | `CreateActor`, then `ResumeActor` | An existing actor is adopted only if it came from the same template. Then a new attempt credential is minted and the bridge's health check is polled through the router. Mounts, an image or limits in the spec are refused: the template fixes the last two and an actor sees no host paths |
| `inspect` | `GetActor` | Fails if the name now refers to a different UID |
| `exec` | none: the bridge, through the router | See [the bridge](#the-bridge) |
| `stop` | bridge `Shutdown` and `EndAttempt`, then `SuspendActor` | A full-scope suspend would otherwise keep the processes, frozen, and resume them later; `stop` must end them. `stop_with` first asks the bridge which execs run ([quiescence](#quiescence)) |
| `checkpoint(name)` | `CreateTag` with `TAG_SCOPE_ATESPACE` | Requires a stopped or paused actor; a running one is never suspended implicitly. A paused actor is suspended first (`SuspendActor` uploads its node-local snapshot, and nothing runs in it). A retry adopts its own earlier tag. `checkpoint_with` stops a running actor first, refusing while an exec runs unless forced |
| `branch(checkpoint, name)` | `CreateActor` with `source_tag` | New name, UID and credentials. The child starts stopped; `ensure` or `resume` starts it. Not a live fork: the source stays as it was |
| `pause(name)` | bridge `EndAttempt`, then `PauseActor` | The actor stays on its node with its snapshot there; pausing a paused actor succeeds |
| `resume(name)` | `ResumeActor`, then a new attempt and the bridge's health check | From a pause or a suspend |
| `release_checkpoint` | `DeleteTag` | Deleting an absent tag succeeds |
| `branch_live` | none | Refused as unsupported: the API has no fork of a running actor |
| `revert` (`Actors` only) | `RevertActor` | Returns to the **latest** suspend only; not restore to a chosen checkpoint |
| `destroy` | bridge `EndAttempt`, then `DeleteActor` with a UID precondition | Idempotent; cannot delete a newer actor that reused the name |

### UID fencing

Names can be reused; UIDs cannot. The vendored API offers a UID precondition only on deletes (`DeleteOptions.uid`, line 1708) and on updates (`UpdateActorRequest` and `UpdateTagRequest` require `metadata.uid` and `metadata.version`); `SuspendActor`, `ResumeActor`, `RevertActor` and `CreateTag` take a name only. So:

- `DeleteActor` carries the handle's UID, as before, and the tag cleanup below carries the tag's.
- `ResumeActor`, `SuspendActor` and `RevertActor` are preceded by `GetActor` (a different UID is `Error::Replaced`, and nothing is called), and the UID of the actor each returns (`ResumeActorResponse.actor` and the others) is compared with the handle's. A mismatch is `Error::ReplacedDuring`.
- `CreateTag` is preceded by `GetActor` and followed by another. If the UID changed, the tag may hold the newer actor's state: it is deleted, fenced by its own UID, and the checkpoint fails with `Error::ReplacedDuring`.

Because a UID is never reused, equal UIDs before and after a call prove the call acted on the handle's actor. What remains is detection, not prevention: when another client replaces the actor between the check and the call, the call has already suspended, resumed or reverted the newer actor, and Branchyard can only report it. `UpdateActor`'s preconditions cannot fence these calls, since an update changes the actor rather than guarding another call.

## Declared capabilities

| Capability | Declared | Reason |
|---|---|---|
| Exec with process pipes | Only for a template that runs the bridge | `ateapi` has no exec or attach call; the bridge provides it |
| Routed ingress | Yes | `atenet-router` activates suspended actors on request (not in the vendored source) |
| Checkpoint | Template scope, crash consistency, portable | Suspend captures without the workload's cooperation; object storage makes it portable |
| Branch | Same as checkpoint | Tag, then create from tag |
| Pause | Yes | `PauseActor` and `ResumeActor` |
| Live branch | No | No fork of a running actor; a branch goes through suspend, a tag and a new actor |
| Restore to a chosen checkpoint | No | Only revert-to-latest exists; use `branch` instead |
| Share | No | No API for state shared between actors |
| Host mounts | No | Refused in `ensure`; code crosses by [transfer](#git-in-and-out) |

`FULL` commit scope maps to a full snapshot. `DATA` maps to a disk snapshot: resuming it cold-boots or restores the template's golden snapshot, so no process state from the source actor survives. An unset scope uses the API's documented default, `FULL`. An unrecognized scope value is an error, not a guess.

## The actor template

`branchyard_substrate::template::bridge_template` builds the `ActorTemplate` to pass to `CreateActorTemplate`:

- One container whose `command` is `branchyard-bridge serve --listen 0.0.0.0:8080 --identity /run/branchyard/identity --state /var/lib/branchyard-bridge/attempts`, and whose image, pinned by digest (`Container.image`, proto line 946 onward), also holds `git` and every harness executable. The bridge execs the harness in the same container. `bridge_template_with` adds `--run-as UID:GID` (a user the image provides) and, for a router that passes TLS through, `--tls-cert FILE --tls-key FILE` (paths in the image).
- `BRANCHYARD_BRIDGE_KEY` in `Container.env` (line 986): the **public** half of the host's signing key. Template environment is shared by every actor made from it, which is why nothing secret is put there.
- A `system_info` volume with an `actor_metadata` projection (`SystemInfoVolumeSource`, `ActorMetadataDataSource` and `ActorMetadataField`, lines 1174-1240) of the actor's atespace, name and UID to `/run/branchyard/identity/{atespace,name,uid}`. The proto says atelet regenerates these files "on every Run/Restore", so a branched actor's bridge sees its new identity even when its memory came from the parent. The bridge re-reads them on every connection.
- A `wakeup_probe` on `GET /healthz`, port 8080 (`ContainerWakeupProbe`, line 1072), so `ResumeActor` returns only once the bridge listens.
- Full-scope pause and commit snapshots.

The template's name goes in `--substrate-template`. `branchyard-bridge keygen --out FILE` writes the host's signing key (PKCS#8, mode 0600) and prints the public key for the template.

The bridge can be the container's process 1; it needs no separate init. See [processes, signals and users](#processes-signals-and-users).

## The bridge

The host reaches the bridge through the router: `--substrate-router` is a URL template with `{atespace}` and `{actor}`, such as `https://router.example/{atespace}/{actor}/`. The vendored source says only that atenet routes requests to an actor's worker (`WorkerAssignment`, proto line 618: "atenet reads worker_pod_ip on the request-routing path"), not how an actor is addressed, so the template is configuration, to be fixed during qualification.

### Transport

An HTTP/1.1 `GET` upgraded to WebSocket (RFC 6455), because routers that forward actor ingress forward WebSocket upgrades. The request names the subprotocol `branchyard-bridge.v2` and carries `Authorization: Bearer <credential>`. The bridge refuses before the upgrade: 401 with the reason for a missing, malformed, forged, expired, foreign, superseded or ended credential; 426 for another WebSocket version or subprotocol; 503 when it cannot read or check its identity or attempt state. A plain `GET …/healthz` answers 200 without a credential. After the upgrade, every binary message is one frame.

### TLS

Both hops can use TLS, with rustls and its `ring` provider (TLS 1.2 and 1.3):

- **`Control`.** `--substrate-endpoint https://…` verifies the server against `--substrate-ca` (PEM), or the Mozilla roots bundled at build time (`webpki-roots`) when none is given; never the host's certificate store. `--substrate-client-cert` and `--substrate-client-key` present a client certificate (mutual TLS).
- **Router.** `https://` or `wss://` (the same thing to the bridge's client) verifies the router against `--substrate-router-ca`, else `--substrate-ca`, else the bundled roots. Where the router passes TLS through rather than terminating it, the bridge serves TLS itself (`--tls-cert`, `--tls-key`), and the same verification applies to the bridge's certificate. Such a bridge still answers the wakeup probe's plain `GET /healthz`, because `HTTPGetAction` (proto line 1088) has no scheme: it reads a connection's first byte, and anything that is not a TLS handshake may fetch the health check and is refused (403) otherwise.
- **In the clear.** `http://` and `ws://` are accepted only to `localhost` or a loopback address (a port-forward) unless `--substrate-insecure` is given. TLS files for a URL in the clear, half a client identity, and an unreadable or malformed PEM file are refused before anything is contacted. A router certificate that does not verify fails `ensure` at once rather than after the ready timeout.

Certificate revocation is not checked. Whoever terminates TLS (a router that does not pass it through) sees credentials and code in the clear.

### Frames (protocol version 2)

Version 2 added `Status` and `Report`; a bridge speaks one version. A frame is a one-byte tag, then its fields: big-endian integers, `bytes` as a `u32` length and the bytes, lists as a `u32` count and the items, optional integers as a `0`/`1` byte and the value. A frame with an unknown tag, a short field or trailing bytes is an error: a peer never guesses at a newer frame. A new frame or field is a new version, negotiated by the subprotocol. The codec is [`protocol.rs`](../crates/branchyard-bridge/src/protocol.rs).

| Tag | Frame | Direction | Meaning |
|---|---|---|---|
| `01` | `Exec {argv, cwd, env}` | request | Start `argv` without a shell, in `cwd`, with `env` added to the bridge's environment (minus its own `BRANCHYARD_BRIDGE*` variables), in a new process group |
| `02` | `PutFile {path, mode}` | request | Write the following `Data` frames, up to `End`, to an absolute path, creating parents; answered by `Done` |
| `03` | `GetFile {path}` | request | Answered by `Data` frames and `End` |
| `04` | `PutTree {path}` | request | Write the following tree under a directory; answered by `Done` |
| `05` | `GetTree {path}` | request | Answered by the tree |
| `06` | `EndAttempt` | request | End the credential's attempt and tear down its processes; answered by `Survivors` |
| `07` | `Shutdown` | request | Tear down every process the bridge started; answered by `Survivors` |
| `08` | `Status` | request | Answered by `Report` |
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
| `29` | `Report {[exec], tampered}` | bridge | Each running exec: `pid` (`u32`), `attempt` (`u64`), `program` (`bytes`), `seconds` running (`u32`) and the command names of its group's live members; then `1` if the state file was ever found changed behind the bridge, else `0` |
| `30` | `Entry {kind, path, mode, target}` | either | A tree entry (directory, regular file or symbolic link); a file's content follows as `Data` frames and `End`; a tree ends with `End` |
| `31`, `32` | `Data(bytes)`, `End` | either | File content |

The client's first frame is its connection's only request. Execs may run concurrently (each has its own connection and process group, and at most 64 run at once), because teardown checks probe the sandbox while a process runs; the engine runs one harness per actor. A closed connection tears its exec down, which is how dropping a `Process` ends it; a connection that ends before `Exited` makes `wait` return an unknown exit status (no code, no signal). Trees are treated as untrusted at both ends: paths must be relative and plain, links are recreated and never followed, and nothing is written through a link.

## Identity: per-attempt credentials

Each attempt gets its own credential. In the engine an attempt is one turn, which also has its own actor; `SubstrateProvider::begin_attempt` rotates the credential of a longer-lived actor.

- **Minting.** The host signs `byb1.<hex payload>.<hex Ed25519 signature>`. The payload names the actor's atespace, name and UID, an attempt label, a sequence number (microseconds since the epoch, strictly increasing per provider) and an expiry (six hours by default). The host's key never enters the actor; the bridge holds only the public half, so nothing inside the sandbox can mint a credential. ([credential.rs](../crates/branchyard-bridge/src/credential.rs))
- **Checking.** The bridge accepts a credential only if the signature verifies, it has not expired, it names the identity projected into this actor (so a recreated or branched actor, which has a new UID, never accepts its predecessor's), and its sequence number is above every ended attempt and not below the newest one seen. A newer attempt ends every older one and tears down their processes.
- **Ending.** `EndAttempt` (sent by `end_attempt`, `stop` and `destroy`) marks the attempt ended. The state is written, and synced, to the state file before the connection that changed it is answered, so an ended attempt stays ended across a bridge restart or a suspend and resume. See [attempt state](#attempt-state) for who can change that file.
- **Why not Substrate's identity APIs.** `MintActorJWT` and `MintActorCertificate` (proto lines 66-77 and 1543-1628) issue credentials that assert the **actor's** identity (`sub` = `atespaces:<atespace>:actors:<name>`) and are "called by the egress gateway" for the actor's outbound requests. They authenticate an actor to others, not a caller to the actor; verifying such a JWT in the bridge would need the issuer's keys (OIDC discovery through egress) and would accept any holder of the `Control` API's mint permission, and neither API can revoke a token when an attempt ends. `CreateActor` takes no per-actor environment or volume (`Actor`, line 361; `CreateActorRequest`, line 1391), so a per-attempt secret cannot be injected at creation either. The design therefore uses a Branchyard-signed credential, bound to the Substrate-issued UID that the `actor_metadata` projection supplies.

## Processes, signals and users

- **Reaping.** On Linux the bridge makes itself a child subreaper (`PR_SET_CHILD_SUBREAPER`), so a process an exec orphans is reparented to it rather than to process 1, and it reaps every zombie child that no exec is waiting for (on `SIGCHLD`, and each second). So nothing accumulates whether or not it is the container's process 1.
- **Signals.** `SIGTERM` or `SIGINT` is forwarded as `SIGTERM` to every exec's process group; ten seconds later what is left is killed; once each exec's last output and exit status have reached its client (up to two seconds more), the bridge exits with status 0. The bridge blocks these signals in its threads and waits for them on one; processes it starts get an empty signal mask. As process 1 it ignores them when the sender is inside the sandbox (a nonzero `si_pid` in its PID namespace), so only the container runtime stops it; the kernel never delivers `SIGKILL` to a namespace's init from inside it.
- **Another user.** With `--run-as UID:GID` (the bridge running as root), execs run as that user and group with no supplementary groups, and `PutFile`, `GetFile`, `PutTree` and `GetTree` run with that user's file-system identity (`setfsuid`, per thread), so files belong to it and a link it planted is followed only with its own rights. The attempt state file is created mode 0600 in a directory the bridge creates with mode 0700.
- **Memory.** The bridge is not dumpable (`PR_SET_DUMPABLE` 0), so a process of its own user without `CAP_SYS_PTRACE` cannot read its memory or environment through `/proc` or `ptrace`.

### Attempt state

The bridge's memory is the authority for attempt state while it runs: it never rereads the file, so resetting the file cannot revive an attempt until the bridge restarts. Before each request that reads the state it compares the file with what it last wrote; a difference is logged, reported in `Report.tampered`, and undone by rewriting the file from memory.

The threat is a process in the actor that wants to reuse a credential the host has ended. It needs the credential and a bridge that no longer remembers the end. Signing or MACing the file does not help: what the attacker wants is an older, validly written state (rollback), and a key the bridge could use after a restart would sit in the same actor, readable by whoever can write the file. So:

| The harness runs as | It can | It cannot |
|---|---|---|
| The bridge's user (the default) | Write the state file; send the bridge signals (except as process 1, where they are ignored); read files the bridge can | Read the bridge's memory, where the live credential is, unless it has `CAP_SYS_PTRACE`; mint a credential; use one for another actor or after it expires |
| Another user (`--run-as`) | Only what that user can | Write or read the state file, signal the bridge, read its memory, or reach a file through the bridge that it could not reach itself |

A reset takes effect only when a bridge starts from the file: after a crash, or on a resume from a disk-scope (`DATA`) snapshot, which cold-boots. Only the host resumes an actor, and every `ensure` starts a newer attempt, which supersedes the ended ones again whatever the file says; so a revived credential is usable only between such a restart and the host's next attempt, and only by a process that already held it. With `--run-as`, the image must not let the harness become the bridge's user (no `sudo`, no setuid helper), and the guest clock still decides expiry.

## Quiescence

`SubstrateProvider::status` asks the bridge (`Status`) which execs run: each one's program, attempt, age and the live members of its process group. More than the leader at work, such as a shell under the harness, is a tool call in progress. `stop_with(name, quiesce)` and `checkpoint_with(name, guarantee, quiesce)` use it: `Quiesce::Refuse` fails with `ResourceBusy`, naming each exec and its members, while any exec runs; `Quiesce::Wait(timeout)` polls until none does, then refuses; `Quiesce::Force` proceeds and kills them. `checkpoint_with` stops a running actor that way, then tags it. The contract's `stop` is `stop_with` forced, because the contract requires `stop` to end processes; the contract's `checkpoint` still requires a stopped actor, whose bridge runs nothing.

The provider cannot see inside a harness: an idle harness waiting for its next prompt is a running exec, so checkpointing an actor during a session needs the harness closed first or `Force`. Between the check and the shutdown, only a holder of the current attempt's credential (this provider) could start another exec.

## Git in and out

An actor cannot mount the worktree, so each turn copies it ([transfer.rs](../crates/branchyard-substrate/src/transfer.rs)). Bundles over the bridge were chosen over a Branchyard-served git remote because they need no listener on the host, no route from the actor back to it, and no credential for one; the bridge connection already exists and is authenticated.

**In**, before the harness starts: the host snapshots the worktree as a commit on top of `HEAD` (every tracked and untracked, non-ignored file, via a temporary index), bundles that commit and `HEAD` with its history, and sends the bundle with `PutFile`. In the actor, git creates a new repository at the workdir, fetches the bundle, checks out the snapshot on the host's branch name and resets the branch and index to `HEAD`. The actor then holds the host's working files with `HEAD` at the host's commit. The branch's private home is sent with `PutTree`.

**Out**, after the harness exits or is killed: git in the actor snapshots its working files the same way on top of its `HEAD` and bundles that commit with the commits below it, against the base. The host fetches them with `transfer.fsckObjects` and reads the actor's `HEAD` as the snapshot's parent from the fetched objects, not from anything the actor reports. It checks that the worktree's `HEAD` is still the base and its files still exactly what was sent (otherwise the result is not applied and the turn warns), and applies the difference between the two snapshots to the worktree's files with a two-tree `read-tree -m -u`: additions, changes, deletions, modes and symbolic links.

If the harness committed, its `HEAD` must descend from the base; a reset below it, an amended base or an unrelated history is refused (`transfer::Error::Rewritten`) and nothing is applied. Otherwise its commits are fetched into the host repository, checked again, and the worktree's branch moves from exactly the base to the actor's `HEAD` (`update-ref` with the old value, hooks disabled), with the index at that commit. The commits keep their messages, authors, dates and order; what the harness left uncommitted, staged or not, is a working-tree change on top. Without commits, the host's index, refs and object store are untouched. Either way the engine's existing snapshot, diff and merge path records the candidate exactly as for a local harness. The home comes back with `GetTree` into a new directory that replaces the private home.

Only commits and files cross. The bundles carry objects reachable from the two snapshots; the actor's repository is new, so no host configuration, hook, remote or credential is in it. On the host both steps run in a staging repository whose object store falls back to the host repository's (git alternates); only the harness's own commits reach the host's object store, and only the worktree's branch is moved. The engine keeps the staging repository at `.branchyard/transfer/<actor>` and deletes it when the turn ends, or recovery does. Ignored files cross in neither direction, a file the harness writes outside the workdir never comes back, and a symbolic link comes back as a link. The actor needs `git`.

## In the engine and the CLI

```sh
branchyard-bridge keygen --out ~/.config/branchyard/bridge.key    # prints the public key
by run "Fix the flaky parser test" --provider substrate \
  --substrate-endpoint https://substrate.example:443 --substrate-ca ca.pem \
  --substrate-client-cert by.crt --substrate-client-key by.key \
  --substrate-router 'wss://router.example/{atespace}/{actor}/' \
  --substrate-template by-claude --substrate-key ~/.config/branchyard/bridge.key \
  --pass-env ANTHROPIC_API_KEY --check "cargo test" --yes
```

`--substrate-atespace` (default `default`), `--substrate-workdir` (default `/workspace`), `--substrate-home` (default `/branchyard/home`), and the [TLS](#tls) flags `--substrate-ca`, `--substrate-client-cert`, `--substrate-client-key`, `--substrate-router-ca` and `--substrate-insecure` are optional; `SubstrateOptions` has a field for each; the flags work on `by run`, `by fan` and `by fork`, and a send keeps the branch's provider. Each turn:

1. Journals the actor's name as the turn's `sandbox` step, then creates and starts it and mints the attempt's credential.
2. Copies the worktree and the private home in, and runs the harness in the workdir with `HOME` and the `--pass-env` variables on top of the image's environment.
3. When the turn ends, copies both back and deletes the actor. A failure to bring the result back is a warning on the turn; the actor is deleted regardless.

With `--keep-sandbox pause` (`SubstrateOptions::keep`), step 3 pauses the actor (`PauseActor`) instead of deleting it and records it; the next turn resumes it (`ResumeActor`, a new attempt), removes its old worktree repository and the files git sees there (ignored files, such as what the branch's `[workspace]` setup installed in the actor, stay) and its home, and sends both again. Each checkpoint of such a branch suspends the actor and tags it, keeping the newest `--sandbox-snapshots` tags; `by fork --at N`, a rewind and a delegated child create their actor from checkpoint N's tag. See [sandbox snapshots](sandbox-snapshots.md). The branch's setup runs in the actor, through the bridge.

Delegation is refused to a sandboxed harness, as for Microsandbox. If the engine dies mid-turn, the bridge tears the harness down when its connection closes. `Yard::recover` then finds the actor named by the `sandbox` step and, if it still exists, brings its work back as the turn's end would have: it begins a new attempt with the host's bridge key (superseding the dead engine's credential), pulls the actor's working files through the turn's staging repository, and applies them only if the worktree still holds exactly what was sent; it pulls the home too. It then deletes the actor (which needs only the `Control` API) and the staging directory. The `recovered` event says whether the work came back or why not, and the branch ends `interrupted`, with the recovered files in its candidate.

## Testing

The fake in `branchyard_substrate::fake` serves `Control` over real gRPC, runs a router that forwards `/actors/<atespace>/<actor>/<rest>` to the actor's bridge as `/<rest>` (WebSocket upgrades included, 503 for an actor that is not running), and "runs" an actor from a bridge template as a real `branchyard-bridge` process on this host with the template's key, identity files written as atelet would, and a per-actor directory standing in for its root filesystem (kept across suspend, copied by a tag). `FakeCluster::start_tls` serves TLS on every hop (the `Control` API optionally requiring a client certificate, the router verifying each bridge's), and `replace_before` swaps an actor under its name just before a given call. Certificates are generated at test time with `rcgen`. Against it, hermetically:

- the bridge's codec, WebSocket framing, TLS, credentials and trees, and the binary over TCP: stdio, exit status, teardown naming survivors, files and trees, and refusal of missing, forged, expired, foreign, superseded and ended credentials, across a restart;
- the bridge serving TLS, refusing requests in the clear except the health check, and refused by a client that trusts another authority or only the public roots;
- the bridge as subreaper (a double-forked orphan reparented to it and reaped, no zombie left), `SIGTERM` forwarded to an exec's trap and a clean exit, the trap's last output and exit status delivered before the bridge exits even behind a client slow to read stderr, children with an empty signal mask, and, as root with `unshare`, the bridge as process 1 of a PID namespace reaping orphans, ignoring `SIGTERM`, `SIGINT` and `SIGKILL` from inside and stopping on `SIGTERM` from outside;
- as root, `--run-as`: execs as the other user with no supplementary groups, unable to read or write the state file, signal the bridge, or read its memory, environment or files through it, and files written as that user;
- a state file reset behind the bridge: the ended attempt stays ended, the file is restored, the tampering reported, and a restart revives nothing; `Status` reporting running execs and their members;
- TLS on both hops through the provider with a private authority and a client certificate, and refusal of a wrong authority, the public roots, a missing or foreign client certificate, a router certificate from another authority (at once), plain HTTP off loopback, and misplaced or malformed TLS files;
- an actor replaced during `SuspendActor`, `ResumeActor` and `CreateTag` reported, and the tag deleted; `stop_with` and `checkpoint_with` refusing and waiting while an exec has a child at work, proceeding once it ends, and killing it when forced;
- every `branchyard_sandbox::conformance` check through `SubstrateProvider`, in the suite's mode for providers without mounts, where the two mount checks require a mount to be refused;
- attempts (rotation, ending, suspend and resume, a branch refusing its parent's live credential) and a reused name never acted on;
- the git round trip (additions, changes, deletions, a binary file, mode changes, links, files written outside the worktree and ignored files never coming back, the host repository's refs, objects and config unchanged, a concurrently changed worktree refused) and the home round trip; a harness that makes two commits by two authors and leaves a changed, a staged and a deleted file: both commits on the branch with their messages, authors, dates and order, the rest as working-tree changes, and the engine's snapshot on top; a reset below the base, an amended base and an unrelated history refused with nothing applied; a host branch that moved meanwhile refused; without commits the host repository not written;
- pause and resume, a paused actor's checkpoint suspending it then tagging it, a branch created stopped from the tag and started while the source stays suspended, live branching refused, a released tag deleted; and through the engine, an actor kept paused between turns, resumed with the worktree sent again, tagged with each checkpoint, a fork at a checkpoint created from its tag, and actors and tags deleted with their branches ([sandbox snapshots](sandbox-snapshots.md));
- the engine and the built `by` running the fake ACP agent in an actor, the candidate merging, commits made in the actor reaching the candidate and the merge, a turn over TLS with a client certificate and one refused without it, the home and session carrying across sends, no process left running, delegation refused, and a killed engine's work brought back by recovery (and not applied over a worktree changed since) before its actor is deleted.

The ignored tests in [`tests/cluster.rs`](../crates/branchyard-substrate/tests/cluster.rs) run the conformance checks, attempt rotation and a worktree round trip against a real cluster; [live testing](testing-live.md#6-agent-substrate-cluster) says how.

## Gaps and required decisions

1. **Router addressing and activation are unverified.** The URL template, whether the router forwards WebSocket upgrades and long-lived connections, whether it activates a suspended actor on a request, whether it terminates TLS or passes it through, and whether it adds its own authentication all come from outside the vendored source.
2. **TLS is implemented but not tried against a cluster.** Both hops support TLS with a private authority and mutual TLS to `Control` ([TLS](#tls)), and plain HTTP is refused off loopback unless `--substrate-insecure`. Unverified: whether Substrate's `Control` endpoint serves TLS itself and negotiates HTTP/2 over ALPN, and how its router handles TLS. Revocation is not checked. A router that terminates TLS sees credentials in the clear, and a credential can be replayed by whoever holds it until its attempt ends. A bridge serving TLS itself has its key in the image.
3. **Secrets inherited by branches.** A full-scope branch copies the source's memory and root filesystem, including passed variables and the harness's home. Use Substrate's egress credential injection so secrets never enter the guest, or checkpoint before any credential is present. Otherwise the design's "no secret inheritance" gate fails.
4. **Quiescence sees processes, not tool calls.** `stop_with` and `checkpoint_with` refuse or wait while any exec runs, and name the processes at work under it ([quiescence](#quiescence)); the contract's `stop` still kills without asking, and the provider cannot tell an idle harness from a busy one. A checkpoint of an open session therefore needs the session closed or `Force`, which is only crash-consistent. No harness-level handshake exists.
5. **Fencing detects replacement during a call but cannot prevent it.** The API has UID preconditions only for deletes and updates. Resume, suspend, revert and tag are checked before and after ([UID fencing](#uid-fencing)); a call that raced a replacement has already acted on the newer actor when it is reported.
6. **Attempt state can be rolled back by the bridge's own user.** Memory is the authority while the bridge runs and tampering is detected and undone, but a process running as the bridge's user can reset the file while the bridge is down, and a bridge that then starts from it (after a crash, or a resume from a disk-scope snapshot) accepts the ended attempt's credential until the host's next attempt, if that process already held it. Running the harness as another user (`--run-as`) closes this within the actor; see [attempt state](#attempt-state). The guest clock decides expiry.
7. **Work on engine death, partly recovered.** Recovery brings back the files the harness had written when the bridge ended it, and the home, then deletes the actor. Still lost: anything the harness had not yet written, and the result when the worktree changed since the turn began, when the bridge key cannot be read, or when the actor does not answer; the `recovered` event says which. Tested against the fake cluster only.
8. **Kubernetes.** Substrate requires a cluster. It stays optional; Kubernetes is not on Branchyard's default per-spawn path.
9. **Fork is upstream work.** Substrate lists actor forking from a state root on its roadmap. Revisit `branch` when it lands; until then a branch is suspend, tag and create, and `live_branch` is not declared.
10. **Pause, suspend-from-pause and tag semantics are the fake's reading of the proto.** Whether `SuspendActor` from `PAUSED` uploads the node-local snapshot as the proto comment says, whether a tag outlives its source actor's later suspends, and whether the workdir and ignored files are in a `DATA`-scope snapshot must be checked on a cluster before a kept actor or a tag is relied on.

## Qualification steps

1. Install Substrate on kind (`hack/create-kind-cluster.sh`, `hack/install-ate-kind.sh`) at the pinned revision.
2. Build an actor image with `branchyard-bridge`, git, `sh` and the harness, create the template, and run the ignored cluster tests, as [live testing](testing-live.md#6-agent-substrate-cluster) describes.
3. Measure create, resume, suspend, tag and branch separately, cold and warm. Record them as the implementation plan describes.
4. Verify that tag readiness, UID preconditions, the actor returned by resume, suspend and revert, identity projection on restore, router forwarding, TLS on both hops, the bridge as process 1 (orphans reaped, `SIGTERM` from the runtime honored and from inside ignored), `--run-as` in the image, and egress policy behave as the fake assumes. Correct the adapter or the declaration where they differ.
5. Update [validation](validation.md) with the exact revision and results before claiming support.
