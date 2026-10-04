//! Structured logging setup: one [`tracing`] subscriber for the whole
//! process (`branchyard-server`, `by serve`, and its worker mode), writing
//! to stderr so stdout stays free for the printed status line
//! ([`crate::cli`]) and any scripted output.
//!
//! The level comes from `BRANCHYARD_LOG` or `RUST_LOG` (the same
//! [`EnvFilter`] syntax, e.g.
//! `branchyard_server=debug,warn`), checked in that order; `--quiet`
//! selects `warn` as the default when neither is set, matching its old
//! meaning of "warn and above". The format is `--log-format`'s, else
//! `BRANCHYARD_LOG_FORMAT`'s (`json` or `pretty`), else `pretty`.
use tracing_subscriber::EnvFilter;

/// How log lines are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Pretty,
    /// One JSON object per line.
    Json,
}

impl LogFormat {
    /// The flag's format if given, else `BRANCHYARD_LOG_FORMAT`'s (`json`
    /// or `pretty`, any case; anything else is `pretty`), else `pretty`.
    pub fn resolve(flag: Option<LogFormat>, env: Option<&str>) -> LogFormat {
        flag.unwrap_or(match env {
            Some(value) if value.trim().eq_ignore_ascii_case("json") => LogFormat::Json,
            _ => LogFormat::Pretty,
        })
    }
}

/// Sets up the process's global tracing subscriber, in `format` if given
/// (see [`LogFormat::resolve`]). Idempotent: a second call (as in a test
/// binary that runs several integration tests linked together) is a
/// harmless no-op.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-server
pub fn init(quiet: bool, format: Option<LogFormat>) {
    let filter = std::env::var("BRANCHYARD_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new(if quiet { "warn" } else { "info" }));
    let env = std::env::var("BRANCHYARD_LOG_FORMAT").ok();
    let json = LogFormat::resolve(format, env.as_deref()) == LogFormat::Json;
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

/// The subscriber for a short-lived command (`by ls`, `by merge`, ...):
/// warnings and errors only unless `BRANCHYARD_LOG` or `RUST_LOG` says
/// otherwise, on stderr, without timestamps or targets. Best-effort failures
/// anywhere in the workspace (`branchyard-support`) log at `warn`, so this
/// is what makes a swallowed cleanup error visible to the person running the
/// command. Idempotent, like [`init`].
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-server
pub fn init_for_commands() {
    let filter = std::env::var("BRANCHYARD_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::LogFormat;

    #[test]
    fn the_flag_wins_over_the_variable() {
        assert_eq!(LogFormat::resolve(None, None), LogFormat::Pretty);
        assert_eq!(LogFormat::resolve(None, Some("JSON")), LogFormat::Json);
        assert_eq!(LogFormat::resolve(None, Some("yaml")), LogFormat::Pretty);
        assert_eq!(
            LogFormat::resolve(Some(LogFormat::Pretty), Some("json")),
            LogFormat::Pretty
        );
        assert_eq!(
            LogFormat::resolve(Some(LogFormat::Json), None),
            LogFormat::Json
        );
    }
}
