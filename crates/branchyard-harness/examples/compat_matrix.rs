//! Print the driver compatibility matrix as Markdown.
//!
//! ```text
//! cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md
//! ```
//!
//! Rows come from `profiles::PROFILES`, `profiles::NOT_IMPLEMENTED` and the
//! harnesses in `branchyard_controls::harness`; live results come
//! from the reports in `docs/qualification/`. A test in
//! `tests/compatibility.rs` fails when the committed file is stale.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use branchyard_controls::harness::HARNESSES;
use branchyard_harness::profiles::{self, Protocol, NOT_IMPLEMENTED, PROFILES};
use serde_json::Value;

/// A profile's live qualification, from its report.
struct Qualified {
    file: String,
    passed: usize,
    total: usize,
    date: String,
    version: String,
}

fn qualifications() -> BTreeMap<String, Qualified> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/qualification");
    let mut files: Vec<_> = fs::read_dir(&directory)
        .unwrap_or_else(|e| panic!("{}: {e}", directory.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let mut reports = BTreeMap::new();
    for path in files {
        let report: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let profile = report["profile"].as_str().unwrap_or_default().to_owned();
        assert!(
            profiles::by_id(&profile).is_some(),
            "{} reports unknown profile {profile:?}",
            path.display()
        );
        let scenarios = report["scenarios"].as_array().cloned().unwrap_or_default();
        let qualified = Qualified {
            file: path.file_name().unwrap().to_string_lossy().into_owned(),
            passed: scenarios.iter().filter(|s| s["passed"] == true).count(),
            total: scenarios.len(),
            date: report["date"].as_str().unwrap_or("undated").to_owned(),
            version: report["harness_version"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned(),
        };
        let file = qualified.file.clone();
        assert!(
            reports.insert(profile.clone(), qualified).is_none(),
            "two reports for {profile}; keep one per profile ({file})"
        );
    }
    reports
}

fn protocol(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::ClaudeStreamJson => "Claude stream-json",
        Protocol::CodexAppServer => "Codex App Server",
        Protocol::Acp => "ACP v1",
        Protocol::AntigravityStreamJson => "Antigravity stream-json",
        Protocol::PiRpc => "Pi RPC",
        Protocol::AmpStreamJson => "Amp stream-json",
    }
}

fn yes(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// The two-gate interception columns say what Branchyard can intercept
/// mid-turn — a tool permission request it can answer, and input it can
/// steer into a running turn — and how strongly that is evidenced, never
/// overstating it: `live-tested` (a real installed binary, from a
/// qualification report or `docs/harness-integration.md`'s recorded
/// evidence), `recorded-fixture` (a captured real transcript replayed,
/// short of a fresh live run), `contract-only` (only the conformance
/// kit's mocked protocol test, `assert_contract_greeted`/
/// `assert_steer_contract`, run for every profile regardless), or
/// `not verified` for a claimed capability with none of these. A
/// capability the driver does not offer at all is `—`: there is nothing
/// to intercept, so no evidence question applies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Evidence {
    LiveTested,
    ContractOnly,
    NotVerified,
}

impl Evidence {
    fn cell(offered: bool, evidence: Option<Evidence>) -> &'static str {
        if !offered {
            return "—";
        }
        match evidence {
            Some(Evidence::LiveTested) => "live-tested",
            Some(Evidence::ContractOnly) => "contract-only",
            Some(Evidence::NotVerified) | None => "not verified",
        }
    }
}

/// Tool permission interception evidence: qualified means a live run
/// against the real binary, whose required Policy-area cases
/// (`docs/harness-integration.md`'s qualification suite table) include a
/// denied tool the driver must answer through Branchyard. Every offered
/// profile at least passes the conformance kit's own permission-answering
/// contract (`assert_contract_greeted`, `every_profile_follows_the_driver_contract`
/// in `tests/conformance.rs`), so an offered, unqualified profile is
/// `contract-only`, never `not verified`.
fn permission_evidence(
    profile_id: &str,
    offered: bool,
    qualified: &BTreeMap<String, Qualified>,
) -> &'static str {
    let evidence = if qualified.contains_key(profile_id) {
        Some(Evidence::LiveTested)
    } else {
        Some(Evidence::ContractOnly)
    };
    Evidence::cell(offered, evidence)
}

/// Mid-turn steer interception evidence, from
/// `docs/harness-integration.md` "Steering a running turn", which records
/// exactly which profiles were checked against a real installed binary
/// (fixtures under `crates/branchyard-harness/tests/fixtures`) versus
/// negotiated only (an ACP agent other than claude-agent-acp, which
/// advertises the extension but was never run here): that document is the
/// evidence, not a guess from this table alone.
fn steer_evidence(profile_id: &str, offered: bool) -> &'static str {
    let evidence = match profile_id {
        "claude-code-stream-json" | "codex-app-server" | "pi-rpc" | "claude-code-acp" => {
            Some(Evidence::LiveTested)
        }
        // Every offered profile passes `assert_steer_contract`, but an ACP
        // agent other than claude-agent-acp advertising the steering
        // extension has never been run against a real binary here.
        _ if offered => Some(Evidence::NotVerified),
        _ => None,
    };
    Evidence::cell(offered, evidence)
}

/// The matrix as Markdown.
pub fn render() -> String {
    let qualified = qualifications();
    let mut out = String::from(
        "# Compatibility\n\
         \n\
         <!-- Generated by `cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md`. Do not edit. -->\n\
         \n\
         Every integration target and driver profile, in the order of the harness registry. Implemented means the driver exists and passes its protocol tests; it does not mean supported. Live qualification ran the profile's driver against the real harness binary as a local process, not inside a Branchyard sandbox, so isolation, credentials and recovery remain unqualified for every profile. See [driver qualification](qualification/README.md) for scope and findings, and [writing a driver](writing-a-driver.md) to add a row.\n\
         \n\
         Capabilities are what the driver offers before negotiation. ACP resume is used only when the agent advertises `session/resume` or `session/load`; a session that cannot resume fails to open rather than starting fresh. \"Checked against\" names the harness version whose transcript or generated schema the driver's frames were compared with. \"Reasons\" names why each `no` capability (and any `yes`/`if advertised` with a real caveat) is that way, quoted from the driver's own refusal message or documentation; `not verified` marks a gap with no such evidence yet, never a guess.\n\
         \n\
         \"Perm. answered\" and \"Steer\" are the two gates Branchyard can intercept mid-turn: a tool permission request it answers, and input it delivers into a running turn without ending it. Each is `—` when the driver does not offer it at all (nothing to intercept); otherwise `live-tested` (checked against a real installed binary — a qualification report's Policy area for permissions, or `docs/harness-integration.md` \"Steering a running turn\"'s recorded evidence for steer), `contract-only` (only the conformance kit's own mocked protocol test, run for every profile regardless of qualification), or `not verified` (offered, but neither) — never overstated.\n\
         \n\
         | Harness | Profile | Protocol | Role | Resume | Fork | Cancel | Approvals | Perm. answered | Steer | Usage | Checked against | Live qualification | Reasons |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    // Integration targets, then any other registered harness with a profile.
    let rows = HARNESSES
        .iter()
        .filter(|h| h.target.is_some() || PROFILES.iter().any(|p| p.harness == h.id));
    for target in rows {
        let name = format!("{} (`{}`)", target.target.unwrap_or(target.id), target.id);
        let mut implemented = PROFILES
            .iter()
            .filter(|p| p.harness == target.id)
            .peekable();
        if implemented.peek().is_none() {
            let reason = NOT_IMPLEMENTED
                .iter()
                .find(|(id, _)| *id == target.id)
                .map(|(_, reason)| *reason)
                .unwrap_or_else(|| panic!("{} is neither implemented nor explained", target.id));
            out += &format!(
                "| {name} | — | — | — | — | — | — | — | — | — | — | — | not implemented: {} | — |\n",
                reason.replace('|', "\\|")
            );
            continue;
        }
        for (index, profile) in implemented.enumerate() {
            let driver = profile.driver();
            let capabilities = driver.capabilities();
            let reasons = driver.capability_reasons();
            let resume = match (capabilities.resume, profile.protocol) {
                (true, Protocol::Acp) => "if advertised",
                (resume, _) => yes(resume),
            };
            let live = match qualified.get(profile.id) {
                Some(q) => format!(
                    "[{} of {} pass](qualification/{}), {}, {}",
                    q.passed, q.total, q.file, q.date, q.version
                ),
                None => "not qualified".into(),
            };
            let reason_notes: Vec<String> = reasons
                .iter()
                .map(|(capability, reason)| format!("{capability}: {reason}"))
                .collect();
            let reason_notes = if reason_notes.is_empty() {
                "—".to_owned()
            } else {
                reason_notes.join("; ").replace('|', "\\|")
            };
            let permission_evidence =
                permission_evidence(profile.id, capabilities.tool_approvals, &qualified);
            let steer_evidence = steer_evidence(profile.id, capabilities.steer);
            out += &format!(
                "| {name} | `{}` | {} | {} | {resume} | {} | {} | {} | {permission_evidence} | \
                 {steer_evidence} | {} | {} | {live} | {reason_notes} |\n",
                profile.id,
                protocol(profile.protocol),
                if index == 0 { "default" } else { "alternate" },
                yes(capabilities.fork),
                yes(capabilities.cancellation),
                yes(capabilities.tool_approvals),
                yes(capabilities.usage),
                profile.checked_against.unwrap_or("—"),
            );
        }
    }
    // The CLIs Branchyard knows about without driving them.
    let catalog = branchyard_controls::catalog::harnesses();
    let known: Vec<String> = catalog
        .iter()
        .filter(|h| !PROFILES.iter().any(|p| p.harness == h.id))
        .map(|h| format!("`{}`", h.id))
        .collect();
    out += &format!(
        "\n## Known, not driven\n\n\
         The harness registry names {} CLIs. Beside the rows above, Branchyard knows these {} from the \
         vendored Herdr, Scion, emdash and Orca registries, with no profile to drive them: {}. `by harnesses \
         --all` lists every one with its install and login commands, API-key variables and \
         models where upstream records them ([`catalog/harnesses.toml`](../catalog/harnesses.toml), \
         generated from the pinned sources); knowing a CLI is not support for it.\n\
         \n## On your machines\n\n\
         This page says what Branchyard can drive. Whether a machine can run a harness is that \
         machine's inventory: `by harnesses` shows which are installed there, at which version, \
         whether each is logged in and how much of its quota is used; `by harnesses install`, \
         `update` and `login` act on one under `[harnesses]` policy; workers advertise theirs, \
         and the router uses it. See [harness lifecycle](harness-lifecycle.md).\n",
        catalog.len(),
        known.len(),
        known.join(", ")
    );
    out
}

fn main() {
    print!("{}", render());
}
