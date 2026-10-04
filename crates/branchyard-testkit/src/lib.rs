//! Shared test infrastructure for the Branchyard workspace.
//!
//! Every test crate is built separately, so a helper copied into one test
//! file cannot be reused by the next. This crate is the one place where
//! tests get:
//!
//! - [`wait`]: waiting for a condition, with one default timeout, a
//!   `BY_TEST_TIMEOUT_SCALE` multiplier for slow machines and a failure that
//!   shows the last value observed. Polling lives here and nowhere else.
//! - [`fake_agent()`] (and [`fake_agent!`]): the `fake-acp-agent` binary, built once per target
//!   directory.
//! - [`Repo`]: a throwaway git repository driven through the built `by`
//!   binary, with the command's output shown when it fails.
//! - [`Scratch`]: a temporary directory that is removed on drop.
//! - [`MockHttp`]: a mock HTTP server that fails the owning test on any
//!   I/O error instead of swallowing it.
//!
//! It is a dev-dependency only; nothing in a release build links it. See
//! "Writing tests" in CONTRIBUTING.md.
#![warn(missing_docs)]

mod agent;
mod mock;
mod repo;
mod scratch;
pub mod wait;

pub use agent::{built, fake_agent, fake_agent_here};
pub use mock::{MockHttp, Request, Response};
pub use repo::Repo;
pub use scratch::Scratch;

/// A [`Repo`] driven through the `by` binary of the crate this expands in
/// (only the `branchyard-cli` tests have one): `repo!()` holds `a.txt`;
/// `repo!(&[("a.txt", "one\n"), (".gitignore", "x\n")])` holds those files.
#[macro_export]
macro_rules! repo {
    () => {
        $crate::Repo::init(::std::path::Path::new(env!("CARGO_BIN_EXE_by")))
    };
    ($files:expr) => {
        $crate::Repo::init_with(::std::path::Path::new(env!("CARGO_BIN_EXE_by")), $files)
    };
}
