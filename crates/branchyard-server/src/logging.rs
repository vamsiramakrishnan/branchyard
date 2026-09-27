//! Structured logging setup: one [`tracing`] subscriber for the whole
//! process (`branchyard-server`, `by serve`, and its worker mode), writing
//! to stderr so stdout stays free for the printed status line
//! ([`crate::cli`]) and any scripted output.
//!
//! The level comes from `BRANCHYARD_LOG` or `RUST_LOG` (the same
//! [`EnvFilter`](tracing_subscriber::EnvFilter) syntax, e.g.
//! `branchyard_server=debug,warn`), checked in that order; `--quiet`
//! selects `warn` as the default when neither is set, matching its old
//! meaning of "warn and above". The format is `pretty` unless
//! `BRANCHYARD_LOG_FORMAT=json`.
//!
//! TODO(BRANCHYARD_LOG_FORMAT): once the CLI grows a `--log-format
//! json|pretty` flag (tracked with the clap conversion), prefer it over
//! this env var and keep the env var as a fallback.
use tracing_subscriber::EnvFilter;

/// Sets up the process's global tracing subscriber. Idempotent: a second
/// call (as in a test binary that runs several integration tests linked
/// together) is a harmless no-op.
pub fn init(quiet: bool) {
    let filter = std::env::var("BRANCHYARD_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new(if quiet { "warn" } else { "info" }));
    let json = std::env::var("BRANCHYARD_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);
    // `.json()` changes the builder's type, so the two formats each finish
    // their own call to `try_init`; either way a second call anywhere in
    // the process (a global subscriber is already set) is ignored rather
    // than panicking.
    let _ = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
}
