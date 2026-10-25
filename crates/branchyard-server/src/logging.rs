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
//! `BRANCHYARD_LOG_FORMAT`'s (`json` or `pretty`), else `pretty`. Colors
//! only when stderr is a terminal and `NO_COLOR` is unset: a log read from
//! a pipe or a file carries no escape codes.
use std::fmt;
use std::io::{self, IsTerminal as _};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

/// Whether log lines on stderr may carry ANSI colors: only on a terminal,
/// and never with `NO_COLOR` set to anything but the empty string.
pub fn ansi(stderr_is_terminal: bool, no_color: Option<&std::ffi::OsStr>) -> bool {
    stderr_is_terminal && no_color.is_none_or(|value| value.is_empty())
}

fn ansi_here() -> bool {
    ansi(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    )
}

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
        .with_ansi(ansi_here())
        .with_writer(QuietStderr);
    // `.json()` changes the builder's type, so the two formats each finish
    // their own call to `try_init`; either way a second call anywhere in
    // the process (a global subscriber is already set) is ignored rather
    // than panicking.
    branchyard_support::best_effort(
        "install the log subscriber",
        if json {
            builder.json().try_init()
        } else {
            builder.try_init()
        },
    );
}

/// Standard error for log lines, where a write that fails is dropped.
/// `tracing-subscriber` reports a failed write with `eprintln!`, which panics
/// when standard error is a closed pipe (`by run 2>&1 | head -1`): the panic
/// ended the command, and with it the turn it was running. A log line that
/// cannot be written has nowhere else to go, and a reader that has gone away
/// has made its choice.
#[derive(Clone, Copy, Debug, Default)]
struct QuietStderr;

impl<'a> MakeWriter<'a> for QuietStderr {
    type Writer = QuietWriter;

    fn make_writer(&'a self) -> QuietWriter {
        QuietWriter
    }
}

/// See [`QuietStderr`].
struct QuietWriter;

impl io::Write for QuietWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match io::stderr().write(buf) {
            Ok(written) => Ok(written),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match io::stderr().flush() {
            Ok(()) | Err(_) => Ok(()),
        }
    }
}

/// The fields of one event: its message, and for a best-effort failure its
/// `context` and `error`.
#[derive(Default)]
struct Fields {
    message: String,
    context: Option<String>,
    error: Option<String>,
    rest: Vec<(String, String)>,
}

impl Fields {
    fn set(&mut self, name: &str, text: String) {
        match name {
            "message" => self.message = text,
            "context" => self.context = Some(text),
            "error" => self.error = Some(text),
            name => self.rest.push((name.to_owned(), text)),
        }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.set(field.name(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.set(field.name(), value.to_owned());
    }
}

/// One line for a person at a terminal: `by: warning: <step>: <error>` for a
/// best-effort failure (`branchyard_support::best_effort`), `by: <level>:
/// <message>` for anything else. `debug` and `trace` lines name their source,
/// which is what someone setting `RUST_LOG=trace` looks for.
struct CommandFormat {
    ansi: bool,
}

impl CommandFormat {
    /// The line for an event of `level` from `target`.
    fn line(&self, level: Level, target: &str, fields: &Fields) -> String {
        let (word, color) = match level {
            Level::ERROR => ("error", "31"),
            Level::WARN => ("warning", "33"),
            Level::INFO => ("info", "32"),
            Level::DEBUG => ("debug", "34"),
            Level::TRACE => ("trace", "35"),
        };
        let word = match self.ansi {
            true => format!("\x1b[{color}m{word}\x1b[0m"),
            false => word.to_owned(),
        };
        let mut line = format!("by: {word}: ");
        if level == Level::DEBUG || level == Level::TRACE {
            line.push_str(target);
            line.push_str(": ");
        }
        match (&fields.context, &fields.error) {
            (Some(context), Some(error)) => line.push_str(&format!("{context}: {error}")),
            _ => line.push_str(&fields.message),
        }
        for (name, value) in &fields.rest {
            line.push_str(&format!(" {name}={value}"));
        }
        line
    }
}

impl<S, N> FormatEvent<S, N> for CommandFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        writeln!(
            writer,
            "{}",
            self.line(*meta.level(), meta.target(), &fields)
        )
    }
}

/// The subscriber for a short-lived command (`by ls`, `by merge`, ...):
/// warnings and errors only unless `BRANCHYARD_LOG` or `RUST_LOG` says
/// otherwise, on stderr as `by: warning: <message>`, without timestamps.
/// Best-effort failures anywhere in the workspace (`branchyard-support`) log
/// at `warn`, so this is what makes a swallowed cleanup error visible to the
/// person running the command: `by: warning: destroy the sandbox: ...`.
/// Idempotent, like [`init`].
pub fn init_for_commands() {
    let filter = std::env::var("BRANCHYARD_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new("warn"));
    branchyard_support::best_effort(
        "install the log subscriber",
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .event_format(CommandFormat { ansi: ansi_here() })
            .with_writer(QuietStderr)
            .try_init(),
    );
}

#[cfg(test)]
mod tests {
    use super::{CommandFormat, Fields, LogFormat};
    use tracing::Level;

    fn best_effort_fields() -> Fields {
        Fields {
            message: "best-effort step failed: destroy the sandbox: gone".into(),
            context: Some("destroy the sandbox".into()),
            error: Some("gone".into()),
            rest: Vec::new(),
        }
    }

    /// The line a person sees names the step and the error, once.
    #[test]
    fn a_best_effort_failure_is_one_by_warning_line() {
        let line = CommandFormat { ansi: false }.line(Level::WARN, "x", &best_effort_fields());
        assert_eq!(line, "by: warning: destroy the sandbox: gone");
    }

    #[test]
    fn other_events_print_their_message_and_fields() {
        let fields = Fields {
            message: "thread panicked".into(),
            rest: vec![("thread".into(), "by-x".into())],
            ..Fields::default()
        };
        let line = CommandFormat { ansi: false }.line(Level::ERROR, "x", &fields);
        assert_eq!(line, "by: error: thread panicked thread=by-x");
        let trace = CommandFormat { ansi: false }.line(Level::TRACE, "hyper::proto", &fields);
        assert!(
            trace.starts_with("by: trace: hyper::proto: thread panicked"),
            "{trace}"
        );
    }

    #[test]
    fn colors_only_when_asked() {
        let line = CommandFormat { ansi: true }.line(Level::WARN, "x", &best_effort_fields());
        assert_eq!(
            line,
            "by: \x1b[33mwarning\x1b[0m: destroy the sandbox: gone"
        );
    }

    /// The best-effort warning `by rm` once printed into a pipe carried
    /// `\x1b[33m`: colors are for a terminal only.
    #[test]
    fn colors_only_on_a_terminal_without_no_color() {
        use std::ffi::OsStr;
        assert!(super::ansi(true, None));
        assert!(super::ansi(true, Some(OsStr::new(""))));
        assert!(!super::ansi(false, None));
        assert!(!super::ansi(true, Some(OsStr::new("1"))));
    }

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
