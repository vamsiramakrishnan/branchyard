//! Prometheus metrics: a small in-process registry and an encoder for the
//! text exposition format (version 0.0.4), served at `GET /metrics`.
//!
//! No metrics crate is a dependency (none was in `Cargo.lock` to adopt), so
//! this is the whole of it: counters and histograms recorded as things
//! happen in this process, and gauges read from the operation store when
//! scraped. Every family is declared once in [`FAMILIES`], so a name, its
//! type and its help text cannot drift apart; `docs/observability.md` lists
//! them.
//!
//! Counters count what this process did: on several servers sharing a
//! database, sum them across servers (each operation is claimed, run and
//! finished by one of them). Gauges read from the store (queue depth and
//! age, live workers) describe the shared database, so every server
//! reports the same values: take one, or `max`, not the sum.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;

/// A metric family's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// A declared family: name, type, help, and a histogram's bucket bounds.
pub struct Family {
    pub name: &'static str,
    pub kind: Kind,
    pub help: &'static str,
    pub buckets: &'static [f64],
}

const SECONDS: &[f64] = &[
    0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

const fn family(name: &'static str, kind: Kind, help: &'static str) -> Family {
    Family {
        name,
        kind,
        help,
        buckets: &[],
    }
}

pub const OPERATIONS: &str = "branchyard_operations";
pub const ADMITTED: &str = "branchyard_operations_admitted_total";
pub const FINISHED: &str = "branchyard_operations_finished_total";
pub const QUEUE_DEPTH: &str = "branchyard_queue_depth";
pub const QUEUE_OLDEST: &str = "branchyard_queue_oldest_age_seconds";
pub const CLAIMS: &str = "branchyard_claims_total";
pub const CLAIM_WAIT: &str = "branchyard_claim_wait_seconds";
pub const RENEWALS: &str = "branchyard_lease_renewals_total";
pub const EXPIRIES: &str = "branchyard_lease_expiries_total";
pub const TURNS_STARTED: &str = "branchyard_turns_started_total";
pub const TURNS_ENDED: &str = "branchyard_turns_ended_total";
pub const TURN_DURATION: &str = "branchyard_turn_duration_seconds";
pub const TOOL_CALLS: &str = "branchyard_tool_calls_total";
pub const COST: &str = "branchyard_cost_usd_total";
pub const CONNECTOR_CALLS: &str = "branchyard_connector_calls_total";
pub const WEBHOOKS: &str = "branchyard_webhook_deliveries_total";
pub const WORKERS: &str = "branchyard_workers_live";
pub const WORKER_SEEN: &str = "branchyard_worker_last_seen_seconds";
pub const BUILD: &str = "branchyard_build_info";

/// Every family this server exposes, in the order it is written.
pub const FAMILIES: &[Family] = &[
    family(
        BUILD,
        Kind::Gauge,
        "Always 1; the version label is this server's.",
    ),
    family(
        OPERATIONS,
        Kind::Gauge,
        "Unfinished operations in the shared queue, by state (queued: unclaimed; running: claimed).",
    ),
    family(
        ADMITTED,
        Kind::Counter,
        "Operations this process admitted, by kind and tenant.",
    ),
    family(
        FINISHED,
        Kind::Counter,
        "Operations this process recorded finished, by kind and state (succeeded, failed, interrupted).",
    ),
    family(
        QUEUE_DEPTH,
        Kind::Gauge,
        "Unclaimed queued operations, by tenant, priority and required worker labels.",
    ),
    family(
        QUEUE_OLDEST,
        Kind::Gauge,
        "Age of the oldest unclaimed queued operation, by tenant, priority and required worker labels.",
    ),
    family(
        CLAIMS,
        Kind::Counter,
        "Queued operations this process's dispatcher claimed, by tenant and priority.",
    ),
    Family {
        name: CLAIM_WAIT,
        kind: Kind::Histogram,
        help: "Time from admission to a claim, by priority.",
        buckets: SECONDS,
    },
    family(
        RENEWALS,
        Kind::Counter,
        "Claim lease renewals by this process, by result (renewed, lost, error).",
    ),
    family(
        EXPIRIES,
        Kind::Counter,
        "Claims this process took over from a worker whose lease had lapsed.",
    ),
    family(
        TURNS_STARTED,
        Kind::Counter,
        "Turns started by operations this process ran, by harness.",
    ),
    family(
        TURNS_ENDED,
        Kind::Counter,
        "Turns ended by operations this process ran, by harness and outcome.",
    ),
    Family {
        name: TURN_DURATION,
        kind: Kind::Histogram,
        help: "Turn duration, from the prompt to the turn's end, by harness.",
        buckets: SECONDS,
    },
    family(
        TOOL_CALLS,
        Kind::Counter,
        "Tool calls harnesses reported in operations this process ran, by harness.",
    ),
    family(
        COST,
        Kind::Counter,
        "Harness-reported cost, in USD, of operations this process ran, by tenant and harness.",
    ),
    family(
        CONNECTOR_CALLS,
        Kind::Counter,
        "Connector gateway calls in operations this process ran, by connector and decision.",
    ),
    family(
        WEBHOOKS,
        Kind::Counter,
        "Webhook deliveries by this process, by result (delivered, retried, dead_lettered).",
    ),
    family(
        WORKERS,
        Kind::Gauge,
        "Workers that recorded themselves alive in the last 15 seconds.",
    ),
    family(
        WORKER_SEEN,
        Kind::Gauge,
        "Seconds since each live worker last recorded itself alive.",
    ),
];

fn declared(name: &str) -> &'static Family {
    FAMILIES
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("metric {name} is not declared in FAMILIES"))
}

/// Label names and values, in a fixed order.
pub type Labels = Vec<(String, String)>;

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// One series' value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    /// Cumulative counts per bucket bound (same order as the family's
    /// buckets), then the sum and count of every observation.
    Histogram {
        counts: Vec<u64>,
        sum: f64,
        count: u64,
    },
}

/// Families by name, each with its series by labels.
pub type Snapshot = BTreeMap<&'static str, BTreeMap<Labels, Value>>;

/// The process's counters and histograms.
#[derive(Default)]
pub struct Metrics {
    series: Mutex<Snapshot>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Metrics")
    }
}

impl Metrics {
    fn with<T>(&self, f: impl FnOnce(&mut Snapshot) -> T) -> T {
        f(&mut self.series.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Add `by` to a counter.
    pub fn add(&self, name: &str, pairs: &[(&str, &str)], by: f64) {
        let family = declared(name);
        debug_assert_eq!(family.kind, Kind::Counter, "{name}");
        if !(by.is_finite() && by >= 0.0) {
            return;
        }
        self.with(|series| {
            let entry = series
                .entry(family.name)
                .or_default()
                .entry(labels(pairs))
                .or_insert(Value::Number(0.0));
            if let Value::Number(n) = entry {
                *n += by;
            }
        });
    }

    /// Add one to a counter.
    pub fn inc(&self, name: &str, pairs: &[(&str, &str)]) {
        self.add(name, pairs, 1.0);
    }

    /// Record an observation in a histogram.
    pub fn observe(&self, name: &str, pairs: &[(&str, &str)], value: f64) {
        let family = declared(name);
        debug_assert_eq!(family.kind, Kind::Histogram, "{name}");
        if !value.is_finite() {
            return;
        }
        self.with(|series| {
            let entry = series
                .entry(family.name)
                .or_default()
                .entry(labels(pairs))
                .or_insert_with(|| Value::Histogram {
                    counts: vec![0; family.buckets.len()],
                    sum: 0.0,
                    count: 0,
                });
            if let Value::Histogram { counts, sum, count } = entry {
                for (bound, n) in family.buckets.iter().zip(counts.iter_mut()) {
                    if value <= *bound {
                        *n += 1;
                    }
                }
                *sum += value;
                *count += 1;
            }
        });
    }

    /// The counters and histograms as they are now.
    pub fn snapshot(&self) -> Snapshot {
        self.with(|series| series.clone())
    }
}

/// Gauges read at scrape time, added to a [`Snapshot`].
pub fn set(snapshot: &mut Snapshot, name: &str, pairs: &[(&str, &str)], value: f64) {
    let family = declared(name);
    debug_assert_eq!(family.kind, Kind::Gauge, "{name}");
    snapshot
        .entry(family.name)
        .or_default()
        .insert(labels(pairs), Value::Number(value));
}

/// A label value escaped for the exposition format: backslash, double
/// quote and line feed.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Help text escaped: backslash and line feed.
fn escape_help(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n")
}

/// A sample value: integers without a fraction, `+Inf`, `-Inf`, `NaN`.
fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn label_set(labels: &[(String, String)], extra: Option<(&str, &str)>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{}\"", escape_label(v)));
    }
    match parts.is_empty() {
        true => String::new(),
        false => format!("{{{}}}", parts.join(",")),
    }
}

/// `snapshot` in the text exposition format, every declared family in
/// [`FAMILIES`] order with its `HELP` and `TYPE` lines, series sorted by
/// labels. A family with no series is written with no samples.
pub fn encode(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    for family in FAMILIES {
        let _ = writeln!(out, "# HELP {} {}", family.name, escape_help(family.help));
        let _ = writeln!(out, "# TYPE {} {}", family.name, family.kind.name());
        let Some(series) = snapshot.get(family.name) else {
            continue;
        };
        for (labels, value) in series {
            match value {
                Value::Number(n) => {
                    let _ = writeln!(
                        out,
                        "{}{} {}",
                        family.name,
                        label_set(labels, None),
                        number(*n)
                    );
                }
                Value::Histogram { counts, sum, count } => {
                    for (bound, n) in family.buckets.iter().zip(counts) {
                        let _ = writeln!(
                            out,
                            "{}_bucket{} {n}",
                            family.name,
                            label_set(labels, Some(("le", &number(*bound))))
                        );
                    }
                    let _ = writeln!(
                        out,
                        "{}_bucket{} {count}",
                        family.name,
                        label_set(labels, Some(("le", "+Inf")))
                    );
                    let _ = writeln!(
                        out,
                        "{}_sum{} {}",
                        family.name,
                        label_set(labels, None),
                        number(*sum)
                    );
                    let _ = writeln!(
                        out,
                        "{}_count{} {count}",
                        family.name,
                        label_set(labels, None)
                    );
                }
            }
        }
    }
    out
}

/// The content type of [`encode`]'s output.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The gauges read from the store at scrape time: the shared queue by
/// state, its depth and oldest age by tenant, priority and labels, and live
/// workers. `now_ms` is the scraping process's clock.
pub fn queue_gauges(
    snapshot: &mut Snapshot,
    queue: &[crate::store::Queued],
    workers: &[crate::store::LiveWorker],
    now_ms: i64,
) {
    let running = queue.iter().filter(|q| q.claimed).count();
    set(
        snapshot,
        OPERATIONS,
        &[("state", "queued")],
        (queue.len() - running) as f64,
    );
    set(
        snapshot,
        OPERATIONS,
        &[("state", "running")],
        running as f64,
    );
    let mut groups: BTreeMap<(String, i32, String), (usize, i64)> = BTreeMap::new();
    for q in queue.iter().filter(|q| !q.claimed) {
        let key = (q.tenant.clone(), q.priority, q.requires.join(","));
        let entry = groups.entry(key).or_insert((0, i64::MAX));
        entry.0 += 1;
        entry.1 = entry.1.min(q.enqueued_ms);
    }
    for ((tenant, priority, required), (depth, oldest)) in groups {
        let priority = priority.to_string();
        let pairs = [
            ("tenant", tenant.as_str()),
            ("priority", priority.as_str()),
            ("labels", required.as_str()),
        ];
        set(snapshot, QUEUE_DEPTH, &pairs, depth as f64);
        let age = (now_ms - oldest).max(0) as f64 / 1000.0;
        set(snapshot, QUEUE_OLDEST, &pairs, age);
    }
    set(snapshot, WORKERS, &[], workers.len() as f64);
    for worker in workers {
        set(
            snapshot,
            WORKER_SEEN,
            &[("worker", &worker.id), ("host", &worker.host)],
            worker.seen_ms_ago as f64 / 1000.0,
        );
    }
    set(
        snapshot,
        BUILD,
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{LiveWorker, Queued};

    /// A minimal parser of the exposition format, independent of the
    /// encoder: every line is a comment (`# HELP name text` or `# TYPE
    /// name type`) or a sample `name{labels} value`, label values quoted
    /// with `\\`, `\"` and `\n` escapes. Returns the samples.
    /// A sample: its name, labels and value.
    type Sample = (String, Vec<(String, String)>, String);

    fn parse(text: &str) -> Vec<Sample> {
        let mut samples = Vec::new();
        let mut typed = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# ") {
                let mut words = rest.splitn(3, ' ');
                let (kind, name, tail) = (words.next(), words.next(), words.next());
                match kind {
                    Some("HELP") => assert!(tail.is_some(), "{line}"),
                    Some("TYPE") => {
                        let tail = tail.unwrap();
                        assert!(
                            ["counter", "gauge", "histogram", "summary", "untyped"].contains(&tail),
                            "{line}"
                        );
                        assert!(
                            typed.insert(name.unwrap().to_owned(), tail).is_none(),
                            "a family typed twice: {line}"
                        );
                    }
                    _ => panic!("unexpected comment {line}"),
                }
                continue;
            }
            let name_end = line.find(['{', ' ']).unwrap();
            let name = &line[..name_end];
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                "{line}"
            );
            let mut rest = &line[name_end..];
            let mut labels = Vec::new();
            if let Some(body) = rest.strip_prefix('{') {
                let mut chars = body.char_indices().peekable();
                loop {
                    let start = chars.peek().unwrap().0;
                    let mut end = start;
                    for (i, c) in chars.by_ref() {
                        if c == '=' {
                            end = i;
                            break;
                        }
                    }
                    let key = body[start..end].to_owned();
                    assert_eq!(chars.next().map(|c| c.1), Some('"'), "{line}");
                    let mut value = String::new();
                    loop {
                        match chars.next().unwrap().1 {
                            '\\' => match chars.next().unwrap().1 {
                                'n' => value.push('\n'),
                                c @ ('\\' | '"') => value.push(c),
                                c => panic!("bad escape \\{c} in {line}"),
                            },
                            '"' => break,
                            c => value.push(c),
                        }
                    }
                    labels.push((key, value));
                    match chars.next().unwrap() {
                        (_, ',') => continue,
                        (i, '}') => {
                            rest = &body[i + 1..];
                            break;
                        }
                        (_, c) => panic!("unexpected {c} in {line}"),
                    }
                }
            }
            let value = rest.strip_prefix(' ').unwrap();
            assert!(
                value.parse::<f64>().is_ok() || ["+Inf", "-Inf", "NaN"].contains(&value),
                "{line}"
            );
            let family = name
                .strip_suffix("_bucket")
                .or_else(|| name.strip_suffix("_sum"))
                .or_else(|| name.strip_suffix("_count"))
                .filter(|f| typed.get(*f) == Some(&"histogram"))
                .unwrap_or(name);
            assert!(
                typed.contains_key(family),
                "a sample before its TYPE: {line}"
            );
            samples.push((name.to_owned(), labels, value.to_owned()));
        }
        samples
    }

    fn sample<'a>(samples: &'a [Sample], name: &str, labels: &[(&str, &str)]) -> Option<&'a str> {
        samples
            .iter()
            .find(|(n, l, _)| {
                n == name
                    && l.len() == labels.len()
                    && labels
                        .iter()
                        .all(|(k, v)| l.iter().any(|(lk, lv)| lk == k && lv == v))
            })
            .map(|(_, _, v)| v.as_str())
    }

    #[test]
    fn counters_histograms_and_gauges_encode_in_the_exposition_format() {
        let metrics = Metrics::default();
        metrics.inc(CLAIMS, &[("tenant", "acme"), ("priority", "5")]);
        metrics.inc(CLAIMS, &[("tenant", "acme"), ("priority", "5")]);
        metrics.add(COST, &[("tenant", "acme"), ("harness", "codex")], 0.25);
        metrics.add(COST, &[("tenant", "acme"), ("harness", "codex")], -1.0);
        metrics.add(COST, &[("tenant", "acme"), ("harness", "codex")], f64::NAN);
        // A label value that needs every escape.
        metrics.inc(
            CONNECTOR_CALLS,
            &[("connector", "a\"b\\c\nd"), ("decision", "allowed")],
        );
        for seconds in [0.05, 3.0, 4000.0] {
            metrics.observe(TURN_DURATION, &[("harness", "claude-code")], seconds);
        }
        let mut snapshot = metrics.snapshot();
        let queue = [
            Queued {
                id: "a".into(),
                repo: "r".into(),
                tenant: "acme".into(),
                priority: 0,
                requires: vec!["gpu".into(), "linux".into()],
                enqueued_ms: 1_000,
                claimed: false,
            },
            Queued {
                id: "b".into(),
                repo: "r".into(),
                tenant: "acme".into(),
                priority: 0,
                requires: vec!["gpu".into(), "linux".into()],
                enqueued_ms: 4_000,
                claimed: false,
            },
            Queued {
                id: "c".into(),
                repo: "r".into(),
                tenant: "other".into(),
                priority: -3,
                requires: vec![],
                enqueued_ms: 9_000,
                claimed: true,
            },
        ];
        let workers = [LiveWorker {
            id: "w_1".into(),
            host: "h".into(),
            labels: vec![],
            repos: vec!["r".into()],
            seen_ms_ago: 1_500,
            inventory: None,
        }];
        queue_gauges(&mut snapshot, &queue, &workers, 11_000);
        let text = encode(&snapshot);
        let samples = parse(&text);
        // Every declared family has its HELP and TYPE, once, in order.
        for family in FAMILIES {
            assert!(
                text.contains(&format!("# TYPE {} {}\n", family.name, family.kind.name())),
                "{}",
                family.name
            );
            if family.kind == Kind::Counter {
                assert!(family.name.ends_with("_total"), "{}", family.name);
            }
        }
        assert_eq!(
            sample(&samples, CLAIMS, &[("tenant", "acme"), ("priority", "5")]),
            Some("2")
        );
        assert_eq!(
            sample(&samples, COST, &[("tenant", "acme"), ("harness", "codex")]),
            Some("0.25"),
            "negative and NaN additions are ignored"
        );
        assert_eq!(
            sample(
                &samples,
                CONNECTOR_CALLS,
                &[("connector", "a\"b\\c\nd"), ("decision", "allowed")]
            ),
            Some("1")
        );
        assert!(text.contains(r#"connector="a\"b\\c\nd""#), "{text}");
        // Histogram buckets are cumulative, with +Inf, sum and count.
        let bucket = |le: &str| {
            sample(
                &samples,
                &format!("{TURN_DURATION}_bucket"),
                &[("harness", "claude-code"), ("le", le)],
            )
        };
        assert_eq!(bucket("0.1"), Some("1"));
        assert_eq!(bucket("1"), Some("1"));
        assert_eq!(bucket("5"), Some("2"));
        assert_eq!(bucket("3600"), Some("2"));
        assert_eq!(bucket("+Inf"), Some("3"));
        assert_eq!(
            sample(
                &samples,
                &format!("{TURN_DURATION}_count"),
                &[("harness", "claude-code")]
            ),
            Some("3")
        );
        assert_eq!(
            sample(
                &samples,
                &format!("{TURN_DURATION}_sum"),
                &[("harness", "claude-code")]
            ),
            Some("4003.05")
        );
        // Gauges from the queue: unclaimed depth and oldest age by group.
        assert_eq!(
            sample(&samples, OPERATIONS, &[("state", "queued")]),
            Some("2")
        );
        assert_eq!(
            sample(&samples, OPERATIONS, &[("state", "running")]),
            Some("1")
        );
        let group = [
            ("tenant", "acme"),
            ("priority", "0"),
            ("labels", "gpu,linux"),
        ];
        assert_eq!(sample(&samples, QUEUE_DEPTH, &group), Some("2"));
        assert_eq!(sample(&samples, QUEUE_OLDEST, &group), Some("10"));
        assert_eq!(
            sample(
                &samples,
                QUEUE_DEPTH,
                &[("tenant", "other"), ("priority", "-3"), ("labels", "")]
            ),
            None,
            "a claimed operation is not queue depth"
        );
        assert_eq!(sample(&samples, WORKERS, &[]), Some("1"));
        assert_eq!(
            sample(&samples, WORKER_SEEN, &[("worker", "w_1"), ("host", "h")]),
            Some("1.5")
        );
    }

    #[test]
    fn numbers_and_help_are_written_as_the_format_requires() {
        assert_eq!(number(3.0), "3");
        assert_eq!(number(-2.0), "-2");
        assert_eq!(number(0.5), "0.5");
        assert_eq!(number(f64::INFINITY), "+Inf");
        assert_eq!(number(f64::NEG_INFINITY), "-Inf");
        assert_eq!(number(f64::NAN), "NaN");
        assert_eq!(escape_help("a\\b\nc"), "a\\\\b\\nc");
        assert_eq!(label_set(&[], None), "");
    }
}
