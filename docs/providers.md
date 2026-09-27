# Sandbox providers

A provider is where a harness process runs. Branchyard talks to every provider through one vendor-independent contract, `SandboxProvider` in [`branchyard-sandbox`](../crates/branchyard-sandbox/src/provider.rs), from [design §10](design.md#10-protocol-and-extensibility). Three providers implement it: the **local provider**, which is today's local mode behind the contract; the **Microsandbox provider**, which runs each turn's harness in a microVM; and the **Agent Substrate provider**, which runs it in a Kubernetes-hosted actor reached through a router. None is qualified against the M2 runtime gates in the [implementation plan](implementation-plan.md) yet.

## The contract

| Operation | What it does |
|---|---|
| `capabilities()` | The guarantees the provider claims, checked by `admit` against a task's requirements. A claim is not qualification evidence |
| `ensure(spec)` | Create and start the sandbox `spec.name` if the provider holds none by that name; otherwise return it. It never weakens a spec: an image, limit or mount it cannot honor is an error |
| `inspect(name)` | The sandbox's state, or none |
| `exec(name, spec)` | Start an argument vector, without a shell, in a working directory (a sandbox path), with named variables added to the provider's base environment. Returns a `Process` |
| `stop(name)` | End every process in the sandbox; keep its state where the provider keeps any |
| `destroy(name)` | Stop and release everything the provider holds. Mounted host directories are kept. Destroying a missing sandbox is not an error |
| `checkpoint`, `restore`, `branch`, `share` | Optional. A provider that lacks one returns `Unsupported` and does not declare it |

A `SandboxSpec` names an OCI image, CPU and memory limits, and **mounts**: each makes a host directory (such as a branch's git worktree) visible at an absolute sandbox path, writable or read-only. `SandboxSpec::guest_path` and `host_path` map paths through the longest matching mount. The mapping is lexical: it does not resolve symlinks, so code acting on a harness-supplied path must still validate it inside the sandbox ([harness integration](harness-integration.md#transport-and-callback-placement)).

A `Process` has a stdin writer, stdout and stderr readers (each taken once; dropping stdin delivers end of file), `try_wait` and `wait` returning an exit status, `kill`, and `teardown`. `teardown` names and kills every process the exec started that is still in the process group the provider made for it, including descendants of a launched process that already exited. Dropping a `Process` that was never waited for kills and tears it down.

The contract is synchronous and uses `std::io` traits, because the runtime around it is. A provider whose SDK is asynchronous bridges it privately.

[`branchyard-runtime`](../crates/branchyard-runtime/src/lib.rs) runs a harness driver over any provider: `Session::start_in(driver, open, provider, sandbox, env, transcript)`. `Session::start` is the same session over the local provider.

### Conformance

`branchyard_sandbox::conformance` holds the checks every provider must pass: lifecycle (`ensure` is idempotent, `inspect` follows `stop` and `destroy`), exit status, a missing program failing `exec`, a stdio round trip with separate stdout and stderr, environment and working directory, the workspace mount in both directions, a read-only mount being refused or enforced, `kill` and `teardown` reaching background children, `teardown` naming survivors after the launched process exits, drop tearing down, and `stop` ending processes. They need `sh`, `sleep`, `cat` and `printf` and a Linux `/proc` in the sandbox. They run against the local provider and, through the fake cluster, the Substrate provider in `cargo test`, and against Microsandbox and a real Substrate cluster in ignored tests.

A provider whose sandboxes cannot see host directories at all runs them with `Setup::without_mounts`: sandboxes get no mount and processes work in a directory that exists in every sandbox, and the workspace and read-only mount checks instead require that a spec with a mount is refused, never created with the mount missing. Code then crosses by an explicit transfer the provider documents.

## Local provider

`LocalProvider` in `branchyard-runtime`. A sandbox is only a name; processes run on the host as your user.

It guarantees:

- Each exec runs in its own process group, and `teardown`, `stop` and drop signal the group, so descendants that stay in it are reached. `teardown` names them first with `ps`.
- The process gets exactly the variables in the exec spec. `branchyard::TaskOptions` builds them as before: your environment minus the parent Claude Code session's variables, or a scrubbed one with a private `HOME` under `--isolated`.

It guarantees nothing beyond your operating-system user. The process sees the host filesystem and network and can read whatever you can. Mounts must be identity mappings (host path equals sandbox path) and writable, and images and resource limits are refused, because the provider could not honor them. A descendant that leaves its process group (a daemon calling `setsid`) escapes teardown. It declares `exec` only.

## Microsandbox provider

`MicrosandboxProvider` in [`branchyard-microsandbox`](../crates/branchyard-microsandbox/src/lib.rs), behind the cargo feature `microsandbox`, which is **off by default**.

### Why it is feature-gated

It pins the public Rust SDK `microsandbox = "=0.7.3"` with `default-features = false, features = ["local", "net"]`: the local runtime only, no cloud backend, no build-time runtime download, no keyring. That SDK needs **Rust 1.94 or newer**: its local backend depends on `sea-orm` 2 and `sqlx` 0.9, which declare `rust-version = "1.94.0"`, and every `sea-orm` 2.0 release does. The workspace pins Rust 1.90, so the default build leaves the SDK out and still builds offline. The newest SDK without that dependency (0.6.8) needs Rust 1.91 for its network stack (`smoltcp` 0.13), so no release builds on 1.90 with networking.

The SDK's packages are in `Cargo.lock` (about 360 more), so `cargo fetch` downloads them even when the feature is off. With the feature on, the build compiles several hundred more crates, takes a few minutes, and links against `libcap-ng`, which needs its development files (`libcap-ng-dev` on Debian and Ubuntu) and a C toolchain. It compiled and its unit tests passed here with Rust 1.94.1; nothing ran against a runtime.

### What it maps

| Contract | SDK 0.7.3 |
|---|---|
| `ensure` | `Sandbox::builder(name).image(..).cpus(..).memory(..).volume(guest, \|m\| m.bind(host))`, `.readonly()` for a read-only mount, then `create()`. The sandbox is attached: the microVM ends if this process dies |
| `inspect` | `Sandbox::status()`, or `Sandbox::get(name)` and `status_snapshot()`. `Created` and `Stopped` map to stopped, `Draining` to stopping, `Paused` to unknown |
| `exec` | `Sandbox::exec_stream_with(program, \|e\| e.args(..).cwd(..).envs(..).stdin_pipe())`. The provider waits for the agent's `Started` or `Failed` event, so a missing program is an `exec` error |
| `Process` pipes | A thread per exec copies `ExecEvent::Stdout` and `Stderr` into OS pipes; stdin writes are `ExecSink::write`, and dropping stdin is `ExecSink::close` |
| `Process::kill` | `ExecControl::kill()` |
| `Process::teardown` | `exec_with("sh", ..)` of a script that lists `/proc/*/stat` for the exec's process group, prints the names, and sends the group SIGKILL |
| `stop` | `stop_with_timeout(30s)`, then `kill()` if that fails |
| `destroy` | `Sandbox::destroy()` (stop and remove) |
| `checkpoint` | `Snapshot::builder(label).from_sandbox(name).create()`: a live disk snapshot |
| `branch` | `Sandbox::restore(label).name(..).cpus(..).memory(..).volume(..).restore()`: a new sandbox from the checkpoint's disk |

Evidence from the pinned source: the guest agent (`microsandbox-agentd` 0.7.3, `lib/session.rs`) makes every exec a session leader in both pipe and PTY mode and delivers `ExecSignal` to the negative PID, so `kill` reaches the whole group. After the launched process exits the agent drops the exec's registration and ignores further signals, which is why `teardown` runs its own in-guest script against the group ID (the start PID). That script is tested here against a local process group.

Declared capabilities are `exec`, `checkpoint` and `branch` with one guarantee: disk scope, crash consistency, same host. The SDK also has full-memory snapshots (`SnapshotBuilder::full`), copy-on-write restores (`RestoreBuilder::forked`) and live branching (`Sandbox::branch`); they are not declared, because [design §6](design.md#6-fast-sandbox-creation) enables them only after qualification. `restore` is not declared because the SDK restores into a new sandbox, not in place. Checkpoints do **not** include bind-mounted host directories, so a branch's workspace is not in its checkpoint.

It does not guarantee: qualification (see below); egress restriction (the guest gets the runtime's default network); bounded buffering (the SDK's exec event channel is unbounded while a reader is slow); or who owns files the guest writes into a bound directory. Record the last during qualification.

### Using it from the engine

`TaskOptions::provider` selects it; the CLI flags are `--provider microsandbox --image REF [--cpus N] [--memory MIB] [--pass-env NAME,...]` on `by run`, `by fan` and `by fork`. A send keeps its branch's provider; a fork inherits its parent's unless the flags name one.

```sh
cargo +1.94 install --locked --path crates/branchyard-cli --features microsandbox
by run "Fix the flaky parser test" --provider microsandbox \
  --image ghcr.io/you/claude-code:2.1 --cpus 2 --memory 4096 \
  --pass-env ANTHROPIC_API_KEY --check "cargo test" --yes
```

Each turn gets a fresh microVM, destroyed when the turn ends:

- The branch's worktree is mounted read-write at `/workspace`, and the harness runs there; the driver's `cwd` is `/workspace`.
- The branch's private home, `.branchyard/homes/<name>`, is mounted at `/branchyard/home` and is `HOME`. Harness sessions persist there across sends; a forked session shares its parent's home, as under `--isolated`.
- The repository's git directory is mounted **read-only** at its host path, so `git log` and `git diff` work in the sandbox but the harness cannot move refs. Candidates are still snapshotted on the host.
- The harness gets `HOME`, the variables named by `--pass-env`, copied from your environment at each turn, and the variables its [provisioning](provisioning.md) sets. Nothing else from your environment crosses: not your login, not `PATH`. The names are stored with the branch; the values are not. A named variable that is unset fails the turn.
- Before the sandbox is created, [provisioning](provisioning.md) writes the harness's native files into the private home on the host (credentials from `--secret` at mode 0600, settings, MCP configuration where the driver cannot pass it), so the mount carries them in.
- Every [scratch area](storage.md) the branch may reach is mounted read-write at `/branchyard/scratch/<name>`, with `BRANCHYARD_SCRATCH_<NAME>` set to that guest path; the mount does not itself enforce the area's single-writer lock (`by scratch lock`), which is a policy over callers that use it, not a filesystem fence (design §7).

The image must contain the harness executable on its `PATH` (or pass its guest path with `--command`), `sh` for teardown, and whatever the harness needs to authenticate from the passed variables. The engine does not look for the harness on the host.

### Prerequisites and the ignored tests

The tests in [`crates/branchyard-microsandbox/tests/microsandbox.rs`](../crates/branchyard-microsandbox/tests/microsandbox.rs) are `#[ignore]`: the conformance checks, CPU and memory limits reaching the guest, root-disk writes staying private to each sandbox, and a disk checkpoint branching into a new sandbox. Run them on a host with:

1. Linux on x86_64 or aarch64 with KVM: `/dev/kvm` exists and your user can open it read-write (usually membership in the `kvm` group). Containers must pass `/dev/kvm` through.
2. Rust 1.94 or newer (`rustup toolchain install 1.94.0`), a C toolchain, and `libcap-ng-dev`.
3. The `msb` runtime and `libkrunfw` at exactly the SDK's version, 0.7.3, installed where the SDK looks (`$MSB_HOME`, default `~/.microsandbox`). Without the `download-binaries` feature the SDK never installs it. The release archive holds two files:

   ```sh
   curl -fsSLO https://github.com/superradcompany/microsandbox/releases/download/v0.7.3/microsandbox-linux-x86_64.tar.gz
   sha256sum microsandbox-linux-x86_64.tar.gz        # record it in the qualification record
   mkdir -p stage ~/.microsandbox/bin ~/.microsandbox/lib
   tar -xzf microsandbox-linux-x86_64.tar.gz -C stage
   install -m 0755 stage/msb ~/.microsandbox/bin/msb
   install -m 0644 stage/libkrunfw.so.5.6.1 ~/.microsandbox/lib/
   ~/.microsandbox/bin/msb --version                 # must print 0.7.3
   ```

   Use `aarch64` in the archive name on ARM hosts.
4. The test image in the local image cache or reachable from its registry. `BY_MSB_IMAGE` selects it (default `alpine:3.20`); it needs `sh`, `sleep`, `cat`, `printf`, `awk` and `nproc`. For the upstream-independence gate, preload it from operator-controlled storage and deny the vendor's control endpoints.
5. No Microsandbox cloud credentials or profile. The provider is built without the SDK's `cloud` feature, so a cloud profile cannot be selected.

Then, from the repository root:

```sh
cargo +1.94 test -p branchyard-microsandbox --features microsandbox          # unit tests; no KVM needed
BY_MSB_IMAGE=alpine:3.20 cargo +1.94 test -p branchyard-microsandbox \
  --features microsandbox -- --ignored --test-threads 1                      # needs KVM and msb
```

Record the results with the [runtime qualification record](implementation-plan.md#runtime-qualification-record): host CPU and kernel, `msb --version`, the archive digest, the image digest, and which tests passed.

## Agent Substrate provider

`SubstrateProvider` in [`branchyard-substrate`](../crates/branchyard-substrate/src/lib.rs), in the default build. [Agent Substrate](substrate.md)'s API has no exec: an actor runs its template's entry point and is reached through routed network ingress. The template's entry point is therefore the Branchyard bridge ([`branchyard-bridge`](../crates/branchyard-bridge/src/lib.rs)), which accepts WebSocket connections through the router, each carrying a per-attempt credential the host signs, and starts the harness with piped stdio in its own process group. `exec` returns a `Process` whose pipes are backed by that connection; `kill`, `teardown` (naming survivors), `wait` and drop behave as the contract says.

It guarantees, beyond the contract:

- Every attempt's credential is refused once the attempt ends or a newer one starts, even across a bridge restart; the bridge's memory is the authority while it runs, and a state file changed behind it is detected and restored. With `--run-as` in the template, the harness runs as another user and cannot touch the state, signal the bridge or read its memory.
- Operations on a known actor are bound to its UID: checked before and after each resume, suspend, revert and tag, and in the request itself for delete. A replacement during a call is reported, not prevented.
- TLS on both hops (`https://` to `Control`, with an optional client certificate; `https://` or `wss://` to the router, or to a bridge that serves TLS itself), verified against a given authority or the bundled public roots. Plain HTTP is refused off loopback unless explicitly allowed.
- `stop_with` and `checkpoint_with` refuse, or wait, while the bridge reports a running exec, unless forced.
- The bridge reaps every orphan and stops cleanly on the runtime's `SIGTERM`, so it can be the container's process 1.
- Commits the harness makes in the actor come back as commits on the branch (a history rewritten below the commit sent is refused), with its uncommitted changes as working-tree changes on top.

It does not mount: a spec with a mount, an image or limits is refused, because the actor template fixes the image and limits and an actor sees no host paths. The engine instead copies the worktree in and out as git bundles and the private home as a directory tree, with file modes. [Scratch areas](storage.md) are not transferred either: a turn on this provider gets no `BRANCHYARD_SCRATCH_*` variable and no mount, whether or not it is authorized for one; implementing the same copy-in/copy-out treatment for them is future work. [Provisioning](provisioning.md) runs before the copy, so the harness's credential and configuration files go in with the home and come back with it. It cannot tell a harness's tool call from an idle harness, and it is tested only against an in-process fake cluster. It declares `exec` for a template that runs the bridge, `ingress`, and checkpoint and branch with the template's commit scope, crash consistency and portability.

`TaskOptions::provider` selects it with `Provider::Substrate(SubstrateOptions)`; the CLI flags are `--provider substrate --substrate-endpoint URL --substrate-router URL --substrate-template NAME --substrate-key FILE [--substrate-atespace NAME] [--substrate-workdir PATH] [--substrate-home PATH] [--substrate-ca FILE] [--substrate-client-cert FILE --substrate-client-key FILE] [--substrate-router-ca FILE] [--substrate-insecure] [--pass-env NAME,...]` on `by run`, `by fan` and `by fork`. [Agent Substrate](substrate.md) documents the bridge protocol, the credentials, the transfer, the template and what remains unqualified; [live testing](testing-live.md#6-agent-substrate-cluster) says how to run it on a kind cluster.

## Provisioning a harness's home

Every provider runs the same [provisioning](provisioning.md) before the harness starts: the harness's provisioner plans its native files, variables and session items from the task's secrets, MCP servers, instructions, model, reasoning effort and telemetry, and the engine applies the files to the branch's private home on the host. Where the home is, and so whether files may be written at all, depends on the provider:

| Provider | `HOME` as the harness sees it | Files written | How they reach the harness |
|---|---|---|---|
| Local | your `HOME` | none: a plan that needs a file, or any secret, is refused | variables only; MCP servers and instructions through the driver |
| Local, `--isolated` | `.branchyard/homes/<name>` | yes | it is the harness's `HOME` |
| Microsandbox | `/branchyard/home` | yes, in `.branchyard/homes/<name>` on the host | the home is mounted read-write |
| Substrate | `SubstrateOptions::home` (default `/branchyard/home`) | yes, on the host | the home transfer into the actor, and back when the turn ends |

`--secret` is the way to give a sandboxed harness credentials: unlike `--pass-env`, it also writes the files a harness authenticates from (Codex's `auth.json`, Claude Code's `.credentials.json`) and records which method it chose. Both remain available.

## Through a server

A server runs a request's provider only when its operator allowed it: `by serve --allow-provider microsandbox,substrate`, or `"allow_providers"` in the configuration. `local` is always allowed; anything else is refused with `403 provider_not_allowed`. `by --remote … --provider …` sends the same flags as a `provider` object, [`Provider`](../crates/branchyard/src/lib.rs)'s serde form, and the server stores it with the branch as local mode does.

Everything a provider names is the server's:

- `--pass-env` names are read from **the server's environment** at each turn, not the caller's. Set the credentials in the server's environment (its service unit, for example) and let callers name them.
- `--substrate-key`, and the TLS files `--substrate-ca`, `--substrate-client-cert`, `--substrate-client-key` and `--substrate-router-ca`, are paths on the server and must be absolute; `by --remote` refuses a relative key rather than resolve it against the caller's directory.
- The Microsandbox provider needs a server built with the `microsandbox` feature, on a host with KVM.
- The Substrate endpoint and router must be reachable from the server.
- `--secret` names are resolved from the server's own table (`by serve --secret NAME[=VAR|=@FILE]`), never from a source the caller names; see [provisioning](provisioning.md#through-a-server).

`by --remote run --provider substrate` is tested against the fake cluster with a spawned `by serve`; the refusal without `--allow-provider` is tested too.
