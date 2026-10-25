//! A task run in local mode: one prompt, one branch per harness, a merge.
//!
//! ```sh
//! cargo run -p branchyard --example local_run -- "Make the flaky parser test deterministic" claude-code codex
//! ```
//!
//! Run it in a git repository with at least one commit. The first argument
//! is the prompt, the rest are the harnesses to run it on (default:
//! `claude-code` and `codex`); each needs its CLI installed and logged in.
//! This calls real harnesses, with `Policy::allow_all()`: a harness may do
//! anything your user can, so run it where that is acceptable. It prints each
//! branch's status and merges the first branch that is ready into `main`.
//! The same program is the `no_run` example in the crate's documentation
//! (`src/lib.rs`), kept here so `cargo build --examples` compiles it.
//! `docs/design.md` §4 says what local mode does not isolate.

use branchyard::{BranchStatus, Budget, Policy, Yard};

fn main() -> Result<(), branchyard::Error> {
    let mut args = std::env::args().skip(1);
    let prompt = args
        .next()
        .unwrap_or_else(|| "Make the flaky parser test deterministic".to_owned());
    let mut harnesses: Vec<String> = args.collect();
    if harnesses.is_empty() {
        harnesses = vec!["claude-code".to_owned(), "codex".to_owned()];
    }
    let harnesses: Vec<&str> = harnesses.iter().map(String::as_str).collect();

    let yard = Yard::open(".")?;
    let branches = yard
        .task(prompt)
        .budget(Budget::usd(2.0))
        .policy(Policy::allow_all())
        .check(["cargo", "test"])
        .run_on(&harnesses)?;
    for branch in &branches {
        let info = branch.info();
        println!("{}: {:?}", info.name, info.status);
    }
    match branches
        .iter()
        .map(|branch| branch.info())
        .find(|info| info.status == BranchStatus::Ready)
    {
        Some(ready) => {
            let merged = yard.merge(&ready.name, "main")?;
            println!("merged {}: {merged:?}", ready.name);
        }
        None => println!("no branch is ready; `by ls` shows why"),
    }
    Ok(())
}
