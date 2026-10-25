# branchyard

The Branchyard engine as a Rust library: run coding-agent harnesses (Claude Code, Codex, Gemini CLI and others) as
local processes, each on its own git worktree and branch, keep every event durable, and merge a branch's result only
after a check passes on the exact merge. [`by`](https://github.com/vamsiramakrishnan/branchyard), the command-line
tool, and `by serve`, its HTTP server, are programs over this crate.

**Not published yet.** The workspace sets `publish = false`, there is no crates.io release, and the crate depends on
the other crates of the workspace by path. To use it, depend on it by path from a checkout:

```toml
[dependencies]
branchyard = { path = "../branchyard/crates/branchyard" }   # needs Rust 1.94 or newer
```

A git dependency on the repository has not been tried. The crate root documents the API (`cargo doc -p branchyard
--open`); some public items still have no doc comment of their own, and `tools/check_missing_docs.py` counts them so
the number only falls.

## Local mode

The engine runs in your process. Harnesses run as your operating-system user, with no isolation beyond that (a branch
can ask for a sandbox provider; see [`docs/providers.md`](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/providers.md)).
State is in `.branchyard/` at the repository root.

```rust,no_run
use branchyard::{Budget, Policy, Yard};

let yard = Yard::open(".")?;
let branches = yard
    .task("Make the flaky parser test deterministic")
    .budget(Budget::usd(2.0))
    .policy(Policy::allow_all())
    .check(["cargo", "test"])
    .run_on(&["claude-code", "codex"])?;
for branch in &branches {
    println!("{}: {:?}", branch.info().name, branch.info().status);
}
yard.merge(&branches[0].info().name, "main")?;
# Ok::<(), branchyard::Error>(())
```

This is [`examples/local_run.rs`](examples/local_run.rs) with arguments, built by `cargo build -p branchyard --examples`.
It calls real harnesses, so it needs their CLIs installed and logged in. `Policy::allow_all()` lets a harness do
anything your user can; use a narrower `Policy` (or `Policy::deny_all()` and rules) where that is too much.
`Yard::open_configured` reads `branchyard.toml` as `by` does, and `Yard::up` brings the configured backends up.

## Features

| Feature | Adds | Needs |
|---|---|---|
| (none) | the engine over SQLite, local processes as harnesses | a C compiler for the bundled SQLite |
| `postgres` | `Yard::open_postgres`: a repository's state in PostgreSQL instead of SQLite ([`docs/durability.md`](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/durability.md)) | a PostgreSQL server to use it |
| `microsandbox` | running a branch's harness in a Microsandbox microVM (`TaskOptions::provider`) | `libcap-ng-dev` to link; KVM to run |
| `schema` | `schemars::JsonSchema` on the branch and delegation types, for [`schema/contract.json`](https://github.com/vamsiramakrishnan/branchyard/blob/main/schema/contract.json) | nothing |

The docs.rs build, once there is one, enables `postgres` and `schema` and leaves `microsandbox` out because it links a
system library.

## What it guarantees, and does not

The crate-level documentation lists both; in short, state is durable and written in transactions, two engines never
drive one branch at once, a turn over budget is interrupted and waited for, and a merge moves the target only by
compare-and-swap after the check passed. It does not isolate a harness from your user, recover a turn whose engine
ran on another host before its lease expires, or resume sessions across working directories for harnesses that key them
by path.

## Other ways in

- Over HTTP from Rust: the `branchyard-client` crate (the wire types and a client for `by serve`).
- From Python: [`sdk/python`](https://github.com/vamsiramakrishnan/branchyard/tree/main/sdk/python); see
  [`docs/sdk.md`](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/sdk.md) for which surface to use.
- The design, and what each surface does: [`docs/design.md`](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/design.md),
  [`docs/surfaces.md`](https://github.com/vamsiramakrishnan/branchyard/blob/main/docs/surfaces.md).
