//! `server`: a server configuration with hashed credentials, 0600 token
//! files, tenants and quotas.

use serde_json::{json, Map, Value};

use super::{
    back_to_root, flag, float, join, list, parent, secret_choices, secret_ref_question, text,
};
use crate::interview::{split_list, Answers, Choice, Condition, Kind, Question, Rule};
use crate::plan::{ArtifactKind, Plan, PlannedFile};
use crate::probe::{Entropy, Facts, Probe};
use crate::{sha256_hex, Topic};

const SECRET_PREFIX: &str = "secret.";
pub const SCOPES: &[&str] = &["read", "run", "merge", "admin"];
pub const WEBHOOK_EVENTS: &[&str] = &["status", "stall", "permission_wait", "merge", "failure"];

fn loopback(listen: &str) -> bool {
    listen
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|addr| addr.ip().is_loopback())
}

pub fn questions(facts: &Facts, answers: &Answers) -> Vec<Question> {
    let listen = text(answers, "listen").unwrap_or_else(|| "127.0.0.1:8421".into());
    let postgres_hint = match (facts.tools.contains_key("psql") || facts.tools.contains_key("pg_isready"), facts.tools.contains_key("docker")) {
        (true, _) => "PostgreSQL client tools found; several servers and by worker can share it",
        (false, true) => "docker found (the deploy topic runs PostgreSQL in compose); several servers can share it",
        (false, false) => "several servers and by worker can share it; needs the postgres build feature",
    };
    let mut qs = vec![
        Question::new(
            "path",
            Kind::Path,
            "Config file",
            "Where should the server configuration go?",
            "by serve --config reads it. It holds token hashes only; tokens go into 0600 files beside it.",
        )
        .default(".branchyard/server.json")
        .choices(vec![
            Choice::new(".branchyard/server.json", "Beside by's state", ".branchyard/ is excluded from git"),
            Choice::new("deploy/branchyard-server.json", "In the repository", "reviewable; its tokens/ directory is git-ignored"),
        ]),
        Question::new(
            "listen",
            Kind::Select,
            "Listen",
            "Which address should the server listen on?",
            "Plain HTTP is refused off loopback; any other address needs TLS.",
        )
        .default("127.0.0.1:8421")
        .choices(vec![
            Choice::new("127.0.0.1:8421", "Loopback", "this machine only; no TLS needed"),
            Choice::new("0.0.0.0:8421", "All interfaces", "reachable from the network; needs a certificate"),
            Choice::new("[::1]:8421", "IPv6 loopback", "this machine only"),
        ])
        .allow_other(true)
        .rule(Rule::ListenAddress),
        Question::new(
            "database",
            Kind::Select,
            "Database",
            "Where should branch state and operations be kept?",
            "SQLite needs nothing; PostgreSQL lets several servers and workers share the queue.",
        )
        .default("sqlite")
        .choices(vec![
            Choice::new("sqlite", "SQLite", "in the data directory; one server"),
            Choice::new("postgres", "PostgreSQL", postgres_hint),
        ]),
        Question::new(
            "tenancy",
            Kind::Select,
            "Tenants",
            "Who will use this server?",
            "Each tenant gets its own credential, repository reach and quotas; requests never choose their tenant.",
        )
        .default("single")
        .choices(vec![
            Choice::new("single", "Just me", "one admin credential in the default tenant"),
            Choice::new("multi", "Several teams", "one credential and quota per tenant"),
        ]),
        Question::new(
            "tls",
            Kind::Confirm,
            "TLS",
            "Should the server serve HTTPS with its own certificate?",
            "Required off loopback, unless a proxy terminates TLS and you pass --insecure-bind.",
        )
        .default(!loopback(&listen))
        .choices(vec![
            Choice::new(true, "Yes", "give a PEM certificate chain and key"),
            Choice::new(false, "No", "plain HTTP (loopback only)"),
        ])
        .when(Condition::truthy("listen")),
        Question::new("tls.cert", Kind::Path, "Certificate", "Which PEM certificate chain?", "--tls-cert.")
            .default("tls/cert.pem")
            .choices(vec![Choice::new("tls/cert.pem", "tls/cert.pem", "relative to the configuration file")])
            .when(Condition::truthy("tls")),
        Question::new("tls.key", Kind::Path, "Key", "Which PEM private key?", "--tls-key; keep it 0600.")
            .default("tls/key.pem")
            .choices(vec![Choice::new("tls/key.pem", "tls/key.pem", "relative to the configuration file")])
            .when(Condition::truthy("tls")),
        Question::new(
            "database.url",
            Kind::Text,
            "Postgres URL",
            "Which PostgreSQL database?",
            "Stored in the configuration, so it must not hold a password: use a .pgpass file.",
        )
        .default("postgres://branchyard@localhost/branchyard")
        .choices(vec![Choice::new(
            "postgres://branchyard@localhost/branchyard",
            "Local database",
            "user branchyard on localhost",
        )])
        .rule(Rule::PostgresUrl)
        .when(Condition::equals("database", "postgres")),
        Question::new(
            "tenants",
            Kind::Text,
            "Tenant names",
            "Which tenants, comma-separated?",
            "Each gets a credential named TENANT-admin and a token file.",
        )
        .default("team-a, team-b")
        .choices(vec![Choice::new("team-a, team-b", "team-a and team-b", "two example teams")])
        .rule(Rule::NameList { max: 64 })
        .when(Condition::equals("tenancy", "multi")),
        Question::new(
            "scopes",
            Kind::Multiselect,
            "Scopes",
            "What may the generated credentials do?",
            "read covers every GET; run starts and steers turns; merge merges; admin removes branches.",
        )
        .default(json!(SCOPES))
        .choices(vec![
            Choice::new("read", "read", "list and watch branches"),
            Choice::new("run", "run", "start, send, fork, spawn, cancel, steer"),
            Choice::new("merge", "merge", "merge and integrate"),
            Choice::new("admin", "admin", "remove branches"),
        ]),
        Question::new(
            "quota.max_running",
            Kind::Number,
            "Running",
            "How many turns may one tenant run at once?",
            "A tenant quota, counted in the admission's transaction.",
        )
        .optional()
        .default(2)
        .choices(vec![
            Choice::new(2, "2", "a small team"),
            Choice::new(4, "4", "a larger team"),
            Choice::new(Value::Null, "No limit", "only the server's max_running"),
        ])
        .rule(Rule::Min { value: 1.0 })
        .rule(Rule::Integer)
        .when(Condition::equals("tenancy", "multi")),
        Question::new(
            "quota.max_branches",
            Kind::Number,
            "Branches",
            "How many open branches may one tenant have?",
            "A tenant quota across its repositories.",
        )
        .optional()
        .default(20)
        .choices(vec![
            Choice::new(20, "20", "most teams"),
            Choice::new(100, "100", "heavy fan-out"),
            Choice::new(Value::Null, "No limit", "unlimited"),
        ])
        .rule(Rule::Min { value: 1.0 })
        .rule(Rule::Integer)
        .when(Condition::equals("tenancy", "multi")),
        Question::new(
            "quota.max_cost_usd",
            Kind::Number,
            "Spend",
            "How many dollars may one tenant's open branches reserve?",
            "A tenant quota over the recorded cost of its open branches.",
        )
        .optional()
        .default(50)
        .choices(vec![
            Choice::new(50, "$50", "a modest ceiling"),
            Choice::new(500, "$500", "a generous ceiling"),
            Choice::new(Value::Null, "No limit", "unlimited"),
        ])
        .rule(Rule::Min { value: 0.01 })
        .when(Condition::equals("tenancy", "multi")),
        Question::new(
            "max_running",
            Kind::Number,
            "Concurrency",
            "How many operations may run at once on this server?",
            "More wait queued.",
        )
        .default(8)
        .choices(vec![
            Choice::new(4, "4", "a laptop"),
            Choice::new(8, "8", "the default"),
            Choice::new(16, "16", "a larger machine"),
        ])
        .rule(Rule::Min { value: 1.0 })
        .rule(Rule::Integer),
        Question::new(
            "allow_providers",
            Kind::Multiselect,
            "Providers",
            "Which sandbox providers may requests name besides local?",
            "Refused unless allowed. Microsandbox needs KVM and the microsandbox build feature; Substrate a cluster.",
        )
        .optional()
        .default(json!([]))
        .choices(vec![
            Choice::new(
                "microsandbox",
                "Microsandbox",
                match facts.platform.kvm {
                    true => "microVMs; KVM is available here",
                    false => "microVMs; no KVM here",
                },
            ),
            Choice::new("substrate", "Agent Substrate", "actors on Kubernetes"),
        ]),
        Question::new(
            "allow_delegation",
            Kind::Confirm,
            "Delegation",
            "May harnesses delegate to child branches on this server?",
            "Allows delegation envelopes, by spawn and rigs; off by default.",
        )
        .default(false)
        .choices(vec![
            Choice::new(false, "No", "refuse delegation requests"),
            Choice::new(true, "Yes", "harnesses get the delegation tools with this server's by"),
        ]),
    ];
    let (choices, set) = secret_choices(facts, &["claude-code".to_owned(), "codex".to_owned()]);
    qs.push(
        Question::new(
            "secrets",
            Kind::Multiselect,
            "Secrets",
            "Which secrets may requests name?",
            "The server reads each from its own variable or a file; requests name secrets, never values.",
        )
        .optional()
        .default(Value::Array(set.into_iter().map(Value::from).collect()))
        .choices(choices)
        .allow_other(true)
        .rule(Rule::SecretRef),
    );
    for name in list(answers, "secrets") {
        qs.push(
            secret_ref_question(
                SECRET_PREFIX,
                &name,
                facts,
                "The server reads the secret from there at each turn; only this reference is stored.",
            )
            .when(Condition::includes("secrets", name.as_str())),
        );
    }
    qs.push(
        Question::new(
            "webhook.url",
            Kind::Text,
            "Webhook",
            "Should a webhook be notified of branch activity?",
            "Each delivery is signed with a generated secret (X-Branchyard-Signature).",
        )
        .optional()
        .choices(vec![
            Choice::skip("no webhook"),
            Choice::new(
                "https://hooks.example.com/branchyard",
                "An HTTPS URL",
                "replace with your receiver",
            ),
        ])
        .rule(Rule::Url {
            schemes: vec!["https".into(), "http".into()],
        }),
    );
    qs.push(
        Question::new(
            "webhook.events",
            Kind::Multiselect,
            "Events",
            "Which events should the webhook receive?",
            "An empty choice means every kind.",
        )
        .optional()
        .default(json!([]))
        .choices(
            WEBHOOK_EVENTS
                .iter()
                .map(|e| Choice::new(*e, *e, ""))
                .collect(),
        )
        .when(Condition::truthy("webhook.url")),
    );
    qs
}

/// A generated secret file: kept when it exists (its hash read through
/// the probe), else a fresh token. Returns the file and the token's hash.
pub(crate) fn token_file(
    path: &str,
    probe: &dyn Probe,
    entropy: &mut dyn Entropy,
) -> (PlannedFile, String) {
    match probe.token_sha256(path) {
        Some(hash) => (PlannedFile::secret(path, String::new(), true), hash),
        None => {
            let token = entropy.token();
            let hash = sha256_hex(token.as_bytes());
            (PlannedFile::secret(path, format!("{token}\n"), false), hash)
        }
    }
}

pub(crate) fn credential(hash: &str, name: &str, tenant: &str, scopes: &[String]) -> Value {
    json!({ "token_sha256": hash, "tenant": tenant, "name": name, "scopes": scopes })
}

#[allow(clippy::expect_used)] // ratchet: branchyard-setup
pub fn plan(
    facts: &Facts,
    answers: &Answers,
    probe: &dyn Probe,
    entropy: &mut dyn Entropy,
) -> Plan {
    let path = text(answers, "path").unwrap_or_else(|| ".branchyard/server.json".into());
    let dir = parent(&path);
    let tokens = join(&dir, "tokens");
    let mut config: Map<String, Value> = probe
        .read(&path)
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let repo = facts.repo_name();
    let root = match path.starts_with('/') {
        true => facts.root.clone(),
        false => back_to_root(&path),
    };
    config.insert("listen".into(), json!(text(answers, "listen")));
    config
        .entry("data_dir")
        .or_insert_with(|| match dir.as_str() {
            ".branchyard" => json!("server"),
            _ => json!(join(&root, ".branchyard/server")),
        });
    let repos = config.entry("repos").or_insert_with(|| json!({}));
    if let Value::Object(repos) = repos {
        repos.entry(repo.clone()).or_insert_with(|| json!(root));
    }

    let mut plan = Plan::new(Topic::Server);
    let scopes = list(answers, "scopes");
    let multi = text(answers, "tenancy").as_deref() == Some("multi");
    let tenants: Vec<String> = match multi {
        true => split_list(&text(answers, "tenants").unwrap_or_default()),
        false => vec!["default".into()],
    };
    let mut credentials: Vec<Value> = config
        .get("credentials")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut token_paths = Vec::new();
    for tenant in &tenants {
        let name = match multi {
            true => format!("{tenant}-admin"),
            false => "admin".to_owned(),
        };
        let file = join(&tokens, &format!("{name}.token"));
        let (planned, hash) = token_file(&file, probe, entropy);
        plan.files.push(planned);
        token_paths.push(file);
        credentials.retain(|c| c.get("name").and_then(Value::as_str) != Some(name.as_str()));
        credentials.push(credential(&hash, &name, tenant, &scopes));
    }
    credentials.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    config.insert("credentials".into(), Value::Array(credentials));
    if multi {
        let policies = config.entry("tenants").or_insert_with(|| json!({}));
        if let Value::Object(policies) = policies {
            for tenant in &tenants {
                let mut policy = Map::new();
                policy.insert("repos".into(), json!([repo]));
                for (key, id) in [
                    ("max_running", "quota.max_running"),
                    ("max_branches", "quota.max_branches"),
                    ("max_cost_usd", "quota.max_cost_usd"),
                ] {
                    if let Some(value) = answers.get(id).filter(|v| !v.is_null()) {
                        policy.insert(key.into(), value.clone());
                    }
                }
                policies.insert(tenant.clone(), Value::Object(policy));
            }
        }
    }
    match flag(answers, "tls") {
        true => {
            config.insert(
                "tls".into(),
                json!({ "cert": text(answers, "tls.cert"), "key": text(answers, "tls.key") }),
            );
        }
        false => {
            config.remove("tls");
        }
    }
    match text(answers, "database").as_deref() {
        Some("postgres") => {
            config.insert("database".into(), json!(text(answers, "database.url")));
        }
        _ => {
            config.remove("database");
        }
    }
    if let Some(n) = float(answers, "max_running") {
        config.insert("max_running".into(), crate::interview::number(n));
    }
    config.insert(
        "allow_providers".into(),
        json!(list(answers, "allow_providers")),
    );
    config.insert(
        "allow_delegation".into(),
        json!(flag(answers, "allow_delegation")),
    );
    let secrets = config.entry("secrets").or_insert_with(|| json!({}));
    if let Value::Object(secrets) = secrets {
        for name in list(answers, "secrets") {
            let source =
                text(answers, &format!("{SECRET_PREFIX}{name}")).unwrap_or_else(|| name.clone());
            secrets.insert(name, json!(source));
        }
    }
    if let Some(url) = text(answers, "webhook.url") {
        let file = join(&tokens, "webhook.secret");
        let (planned, _) = token_file(&file, probe, entropy);
        plan.files.push(planned);
        let hooks = config.entry("webhooks").or_insert_with(|| json!([]));
        if let Value::Array(hooks) = hooks {
            hooks.retain(|h| h.get("url").and_then(Value::as_str) != Some(url.as_str()));
            hooks.push(json!({
                "url": url,
                "secret_file": "tokens/webhook.secret",
                "events": list(answers, "webhook.events"),
            }));
        }
    }
    plan.files.push(PlannedFile::new(
        &join(&tokens, ".gitignore"),
        ArtifactKind::Text,
        0o644,
        "# Bearer tokens and webhook secrets written by `by init server`: never commit them.\n*\n"
            .into(),
        probe.read(&join(&tokens, ".gitignore")),
    ));
    let mut body = serde_json::to_string_pretty(&Value::Object(config)).expect("JSON serializes");
    body.push('\n');
    let existed = probe.exists(&path);
    plan.files.push(PlannedFile::new(
        &path,
        ArtifactKind::ServerConfig,
        0o644,
        body,
        probe.read(&path),
    ));

    plan.summary.push(format!(
        "{} {path}: serve {repo} on {}{}, {} with {} credential{}",
        match existed {
            true => "Update",
            false => "Create",
        },
        text(answers, "listen").unwrap_or_default(),
        match flag(answers, "tls") {
            true => " over TLS",
            false => "",
        },
        match text(answers, "database").as_deref() {
            Some("postgres") => "PostgreSQL",
            _ => "SQLite",
        },
        tenants.len(),
        match tenants.len() {
            1 => "",
            _ => "s",
        }
    ));
    plan.summary.push(format!(
        "Token files (0600, printed nowhere): {}",
        token_paths.join(", ")
    ));
    plan.notes.push(
        "Only each token's SHA-256 is in the configuration, as `branchyard-server token new` prints it; \
         an existing token file is kept, so re-running does not lock out a client."
            .into(),
    );
    if !flag(answers, "tls") && !loopback(&text(answers, "listen").unwrap_or_default()) {
        plan.notes.push("Off loopback without TLS the server refuses to start unless given --insecure-bind behind a TLS proxy.".into());
    }
    plan.command(
        format!("by serve --config {path} --check"),
        "Load and check the configuration exactly as the server will, without serving.",
    );
    plan.command(format!("by serve --config {path}"), "Start the server.");
    let scheme = match flag(answers, "tls") {
        true => "https",
        false => "http",
    };
    let url = format!(
        "{scheme}://{}",
        text(answers, "listen")
            .unwrap_or_default()
            .replace("0.0.0.0", "127.0.0.1")
    );
    if let Some(first) = token_paths.first() {
        plan.command(
            format!("by --remote {url} --token-file {first} ls"),
            "Reach it as a client; by init project can store these in [remote].",
        );
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview::resolve;
    use crate::probe::{CountingEntropy, FakeProbe};
    use std::collections::BTreeMap;

    #[test]
    fn multi_tenant_plans_hash_every_token_and_never_show_one() {
        let probe = FakeProbe::typical();
        let facts = Facts::gather(&probe);
        let raw: BTreeMap<String, Value> = serde_json::from_value(json!({
            "tenancy": "multi", "tenants": "acme, globex", "quota.max_cost_usd": "$75",
            "webhook.url": "https://hooks.example.com/x"
        }))
        .unwrap();
        let state = resolve(&|a| questions(&facts, a), &raw, true);
        assert!(state.done(), "{:?}", state.errors);
        let plan = plan(&facts, &state.answers, &probe, &mut CountingEntropy(0));
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("test-token-"), "a token leaked: {json}");
        let config = plan
            .files
            .iter()
            .find(|f| f.kind == ArtifactKind::ServerConfig)
            .unwrap();
        let value: Value = serde_json::from_str(config.body()).unwrap();
        assert_eq!(value["credentials"].as_array().unwrap().len(), 2);
        assert_eq!(value["tenants"]["acme"]["max_cost_usd"], json!(75));
        assert_eq!(value["repos"]["app"], json!(".."));
        assert_eq!(value["data_dir"], json!("server"));
        let secrets: Vec<&str> = plan
            .files
            .iter()
            .filter(|f| f.sensitive)
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(
            secrets,
            [
                ".branchyard/tokens/acme-admin.token",
                ".branchyard/tokens/globex-admin.token",
                ".branchyard/tokens/webhook.secret"
            ]
        );
        let token = plan.files[0].body().trim();
        assert_eq!(
            value["credentials"][0]["token_sha256"],
            json!(sha256_hex(token.as_bytes()))
        );
    }

    #[test]
    fn an_existing_token_file_is_kept_by_its_hash() {
        let mut probe = FakeProbe::typical();
        probe.files.insert(
            ".branchyard/tokens/admin.token".into(),
            "kept-token-0123456789\n".into(),
        );
        let facts = Facts::gather(&probe);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let plan = plan(&facts, &state.answers, &probe, &mut CountingEntropy(0));
        assert_eq!(plan.files[0].action, crate::FileAction::Keep);
        let config = plan
            .files
            .iter()
            .find(|f| f.kind == ArtifactKind::ServerConfig)
            .unwrap();
        assert!(config
            .body()
            .contains(&sha256_hex(b"kept-token-0123456789")));
    }
}
