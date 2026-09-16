# SDK and harness surface

The implemented surface is a remote Rust client, a JSON CLI and a portable skill.
They share one versioned contract. The admission server is implemented; execution nodes and
sandbox drivers are not. Client-only tests use an HTTP fixture, not a local
execution mode. Nothing here launches a model or runs a workspace command.

## Start without a server

```sh
cargo build --locked -p branchyard-cli
./target/debug/branchyard describe
./target/debug/branchyard validate --file examples/commands/create-task.json
./target/debug/branchyard new-id
```

`validate` checks syntax, identity shape and local bounds. It cannot establish
server authorization, a profile's existence, graph validity or available budget.
The example profile names and IDs are templates, not deployed resources. Replace
all IDs, registered profiles, source and revisions before a real submission.

Install the binary with `cargo install --locked --path crates/branchyard-cli`.
Packages are currently unpublished; use this checkout or pin this repository's
revision. Python 3.12 is required for distribution scripts, not the Rust SDK.

## Rust

Add `branchyard-sdk` as a path dependency or pin its Git revision. Construct a
`Command` using `branchyard_sdk::protocol` and persist it before sending it. The
[compiled example](../crates/branchyard-sdk/examples/submit.rs) reads a saved
command and demonstrates submission and reconciliation.

```rust,no_run
use branchyard_sdk::{Client, Command};

async fn submit_saved(command: &Command, endpoint: &str, token: &str)
    -> Result<(), branchyard_sdk::Error>
{
    let client = Client::new(endpoint, token)?;
    let receipt = client.submit(command).await?;
    // Admission is not task completion. Retain the operation identity.
    let operation = client.reconcile(command).await?;
    println!("{} {:?}", receipt.operation_id, operation.state);
    Ok(())
}
```

`Client` is cloneable and shares Reqwest's connection pool. It has no coordinator
lock, per-request process or blocking network thread. Independent calls may run
concurrently; caller admission and server quotas must bound their number. The
32-call fixture test establishes concurrency behavior, not a throughput claim.

The client has a 30-second request deadline by default, adjustable up to 300
seconds, an eight-idle-connection per-host pool limit and a 1 MiB response bound.
The deadline covers body reading. No automatic retry or redirect is enabled.
Ambient proxy configuration is deliberately disabled to keep endpoint routing
explicit; a future proxy option must preserve that property. Credentials are
sensitive headers and are excluded from error messages. Errors do not return
server bodies. HTTPS uses the existing Rustls stack and platform trust store.

Plain HTTP requires an explicit option and a literal loopback IP, for fixtures
or a trusted tunnel. Hostnames resolving to loopback do not qualify. There is no
insecure-TLS switch. Dropping a future stops waiting; it does not send cancellation.

## Any harness with a terminal

Use the same installed binary through the harness's normal command tool:

```sh
branchyard doctor
branchyard validate --file request.json
branchyard call --file request.json
branchyard reconcile --file request.json
branchyard task TASK_UUID
branchyard events TASK_UUID --after 0 --limit 50
```

Remote commands use `BRANCHYARD_ENDPOINT` and `BRANCHYARD_TOKEN`. Supply a scoped
token through the host's credential mechanism; it is never a CLI flag. The
endpoint may have a deployment prefix such as `https://service.example/api/`.
Requests append `v1alpha1/...` beneath that prefix.

A harness need not support ACP to **call** Branchyard. It needs terminal access,
a custom tool that calls the Rust SDK, or an HTTP client speaking this contract.
A harness **controlled by** Branchyard needs a qualified ACP/native driver inside
the server execution boundary. These are independent integration directions.
An MCP-only host currently needs an adapter; there is no shipped MCP facade.

Exit codes: 0 command success; 2 invalid arguments/input/configuration; 3 remote
or read transport error; 4 uncertain submission; 5 invalid/oversized read reply.
Results are JSON on stdout, errors JSON on stderr; `--help` and `--version` are
human-readable. `doctor` reports readiness as data, so exit 0 alone does not
establish `execution_ready: true`.

## Plugin and standalone skill

[The plugin](../plugins/branchyard/README.md) contains one canonical skill under
`plugins/branchyard/skills/branchyard`. Both Codex and Claude manifests refer to
that directory. Other skill-capable hosts can install an exact standalone copy.
Hosts without skills use the CLI directly. No host-specific hook is required.

```sh
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/project/.agents/skills
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/project/.agents/skills --apply
python3 tools/package.py --output dist
```

The installer targets an explicit directory, previews by default and refuses
replacement. It does not discover or modify global host configuration. Install
one form per host to avoid duplicated instructions. The launcher uses `execv`,
not shell interpolation. No binary is downloaded on skill activation.

The builder produces reproducible plugin and standalone ZIPs containing license
text and file-hash manifests. They do not bundle a platform binary; install the
CLI separately. Tests extract the plugin into a temporary empty directory, run
its installer, compare the copied skill, and invoke the actual CLI through its
launcher. This establishes artifact completeness, not live host compatibility.
Claude's [documented plugin loader](https://code.claude.com/docs/en/plugins)
accepts `--plugin-dir`; Codex team distribution should register the packaged
directory through its supported plugin mechanism. Live receipts remain pending.

## Extension rules

1. Add a domain operation and its invariants to `branchyard-protocol`, then make
   the server enforce them. Shapes derive from Serde/Schemars; regenerate
   `schema/contract.json` with `branchyard describe`. Do not add an untyped
   catch-all action that bypasses policy or idempotency.
2. Keep HTTP and uncertainty handling in `branchyard-sdk`. CLI, future MCP tools,
   plugins and language bindings must call the same operation boundary. Do not
   put retries, launch logic or a second state journal in a skill script.
3. Use registered profile IDs and capability names to extend harness/runtime
   support without changing the task shape. A capability is a server claim until
   qualified with a version-pinned runtime receipt. Unknown required capability
   means rejection. Different semantics require a new versioned contract.
4. Add host-specific behavior only when it needs a capability unavailable through
   the generic CLI/skill. Test the actual host callback boundary. Installing a
   hook is not evidence that it controls a model turn.
5. Keep artifacts addressable and responses bounded. Next steps for artifact
   retrieval must preserve tenant-scoped access; an artifact ID is not a public URL.

The contract is `v1alpha1`, deliberately strict about unknown request and response
fields. An incompatible extension needs a new negotiated version; adding fields
is not automatically backward-compatible for this client. This is not a stable
1.0 API or a promise of compatibility with future server revisions.
