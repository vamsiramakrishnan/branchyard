//! `by models list` end to end with the built `by`: every source whose key
//! is set is asked for its models (mocks of Gemini's, OpenAI's and
//! Anthropic's APIs and of a `[models.backends]` entry, all on this
//! machine's loopback), the models are marked when nothing prices them, the
//! answer is cached with its age, and `--harness` puts a harness's static
//! list beside the live one. No real provider is called and no key of the
//! environment is used: each test sets the keys it means.
//!
//! Requires `git`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::process::Output;

use branchyard_testkit::{MockHttp, Response};
use serde_json::{json, Value};

const GEMINI_KEY: &str = "g-key-111-not-real";
const OPENAI_KEY: &str = "o-key-222-not-real";
const ANTHROPIC_KEY: &str = "a-key-333-not-real";
const LOCAL_KEY: &str = "l-key-444-not-real";

/// The variables a model source reads its key from, none of which a test
/// inherits.
const KEY_VARIABLES: [&str; 6] = [
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "ANTHROPIC_API_KEY",
    "LOCAL_KEY",
];

/// Gemini's API with two pages: a model that generates, one that only
/// embeds, and (on the second page) another that generates.
fn gemini() -> MockHttp {
    MockHttp::start(|request| {
        let page = |models: Value, next: Option<&str>| {
            let mut body = json!({ "models": models });
            if let Some(next) = next {
                body["nextPageToken"] = json!(next);
            }
            Response::json(200, &body)
        };
        if request.path.contains("pageToken=page2") {
            return page(
                json!([{"name": "models/gemini-2.5-flash",
                    "supportedGenerationMethods": ["generateContent"]}]),
                None,
            );
        }
        page(
            json!([
                {"name": "models/gemini-2.5-pro",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]},
                {"name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]}
            ]),
            Some("page2"),
        )
    })
}

/// OpenAI's API: models the catalog prices (`gpt-5.5`, `gpt-5.4-mini`), one it
/// does not, and one it does not list for a harness (`gpt-7-new`).
fn openai() -> MockHttp {
    MockHttp::start(|_| {
        Response::json(
            200,
            &json!({"object": "list", "data": [
                {"id": "gpt-5.5", "object": "model"},
                {"id": "gpt-5.4-mini", "object": "model"},
                {"id": "gpt-7-new", "object": "model"},
                {"id": "whisper-1", "object": "model"}]}),
        )
    })
}

/// Anthropic's API refusing the key.
fn anthropic_refusing() -> MockHttp {
    MockHttp::start(|_| Response::new(401, "invalid x-api-key a-key-333-not-real"))
}

/// A backend of `[models.backends]`, listing at `/v1/models`.
fn local() -> MockHttp {
    MockHttp::start(|_| {
        Response::json(
            200,
            &json!({"data": [{"id": "llama-3.3"}, {"id": "qwen3-coder"}]}),
        )
    })
}

/// A mock that must never be asked.
fn silent() -> MockHttp {
    MockHttp::start(|request| {
        Response::new(
            500,
            format!("asked, but nothing here has a key: {}", request.path),
        )
    })
}

struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Repo {
    /// A repository with `branchyard.toml` holding `toml`, no key in the
    /// environment and the live catalog's cache in its own directory.
    fn new(toml: &str) -> Repo {
        let mut repo = branchyard_testkit::repo!();
        for variable in KEY_VARIABLES {
            repo.remove_env(variable);
        }
        let cache = repo.dir.join("catalog").display().to_string();
        repo.set_env("BRANCHYARD_CATALOG_DIR", &cache);
        std::fs::write(repo.root.join("branchyard.toml"), toml).unwrap();
        Repo(repo)
    }

    /// `by models list` against the mocks, with `extra` arguments.
    fn list(&self, urls: &Urls<'_>, extra: &[&str]) -> Output {
        let mut args = vec!["models", "list"];
        args.extend(extra);
        let flags = [
            ("--gemini-url", urls.gemini),
            ("--openai-url", urls.openai),
            ("--anthropic-url", urls.anthropic),
        ];
        for (flag, url) in &flags {
            args.extend([*flag, *url]);
        }
        self.by(&args)
    }

    fn list_json(&self, urls: &Urls<'_>, extra: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(extra);
        let out = self.list(urls, &all);
        assert!(out.status.success(), "{}", shown(&out));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", shown(&out)))
    }
}

struct Urls<'a> {
    gemini: &'a str,
    openai: &'a str,
    anthropic: &'a str,
}

fn shown(out: &Output) -> String {
    format!(
        "status {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn text(out: &Output) -> String {
    assert!(out.status.success(), "{}", shown(out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The words of the table row that starts with `model`.
fn row<'a>(text: &'a str, model: &str) -> Vec<&'a str> {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|words| words.first() == Some(&model))
        .unwrap_or_else(|| panic!("no row for {model} in:\n{text}"))
}

fn source<'a>(listing: &'a Value, id: &str) -> &'a Value {
    listing["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == id)
        .unwrap_or_else(|| panic!("no source {id} in {listing}"))
}

fn names(source: &Value) -> Vec<&str> {
    source["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect()
}

#[test]
fn every_source_with_a_key_is_listed_with_its_unpriced_models_marked() {
    let (gemini, openai, anthropic, local) = (gemini(), openai(), anthropic_refusing(), local());
    let mut repo = Repo::new(&format!(
        "version = 1\n\n[models.backends.local]\napi = \"generic\"\nurl = \"{}\"\n\
         key = \"LOCAL_KEY\"\nheader = \"x-api-key\"\n\n[models.prices.\"llama-*\"]\n\
         input = 0.2\noutput = 0.6\n",
        local.url()
    ));
    for (variable, key) in [
        ("GEMINI_API_KEY", GEMINI_KEY),
        ("OPENAI_API_KEY", OPENAI_KEY),
        ("ANTHROPIC_API_KEY", ANTHROPIC_KEY),
        ("LOCAL_KEY", LOCAL_KEY),
    ] {
        repo.0.set_env(variable, key);
    }
    let urls = Urls {
        gemini: &gemini.url(),
        openai: &openai.url(),
        anthropic: &anthropic.url(),
    };

    let out = repo.list(&urls, &[]);
    let table = text(&out);
    // Gemini: the model that generates, on both pages; the embedder is not
    // a chat model. The catalog prices no Gemini model.
    assert_eq!(
        row(&table, "gemini-2.5-pro"),
        ["gemini-2.5-pro", "unpriced"]
    );
    assert_eq!(row(&table, "gemini-2.5-flash")[1], "unpriced");
    assert!(!table.contains("text-embedding-004"), "{table}");
    assert!(
        table.contains("/v1beta/openai") && table.contains("\"store\""),
        "Gemini's OpenAI-compatible endpoint is named:\n{table}"
    );
    // OpenAI: the catalog prices gpt-5.5 and gpt-5.4-mini, not whisper.
    assert_eq!(row(&table, "gpt-5.5"), ["gpt-5.5", "catalog"]);
    assert_eq!(row(&table, "gpt-5.4-mini")[1], "catalog");
    assert_eq!(row(&table, "whisper-1")[1], "unpriced");
    // The backend's models, one priced by [models.prices].
    assert_eq!(
        row(&table, "llama-3.3"),
        ["llama-3.3", "[models.prices]", "llama-*"]
    );
    assert_eq!(row(&table, "qwen3-coder")[1], "unpriced");
    // Anthropic refused the key: its status, not its body.
    let line = table
        .lines()
        .find(|l| l.starts_with("anthropic"))
        .unwrap_or_else(|| panic!("{table}"));
    assert!(line.contains("failed: answered 401"), "{line}");
    assert!(table.contains("backend:local"), "{table}");
    assert!(table.contains("cannot be priced"), "{table}");
    assert!(table.contains("key $GEMINI_API_KEY"), "{table}");
    // No key anywhere in what was printed.
    for key in [GEMINI_KEY, OPENAI_KEY, ANTHROPIC_KEY, LOCAL_KEY] {
        assert!(!shown(&out).contains(key), "{key} was printed");
    }

    // The same, as JSON.
    let listing = repo.list_json(&urls, &[]);
    let g = source(&listing, "gemini");
    assert_eq!(g["status"], "listed");
    assert_eq!(g["kind"], "gemini");
    assert_eq!(g["key"], "$GEMINI_API_KEY");
    assert_eq!(names(g), ["gemini-2.5-flash", "gemini-2.5-pro"]);
    assert_eq!(g["unpriced"], 2);
    assert_eq!(
        g["openai_compatible"],
        json!({
            "url": format!("{}/v1beta/openai", gemini.url()),
            "api": "chat_completions",
            "rejects": ["store"]
        })
    );
    let o = source(&listing, "openai");
    assert_eq!(o["status"], "listed");
    let priced: Vec<(&str, bool)> = o["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["priced"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        priced,
        [
            ("gpt-5.4-mini", true),
            ("gpt-5.5", true),
            ("gpt-7-new", false),
            ("whisper-1", false)
        ]
    );
    assert_eq!(o["models"][0]["price"], "catalog");
    let a = source(&listing, "anthropic");
    assert_eq!(a["status"], "failed");
    assert_eq!(a["reason"], "answered 401");
    assert_eq!(a["count"], 0);
    let b = source(&listing, "backend:local");
    assert_eq!(b["status"], "listed");
    assert_eq!(b["backend"], "local");
    assert_eq!(b["models"][0]["price"], "[models.prices] llama-*");
    assert_eq!(listing["models"], 2 + 4 + 2);
    assert_eq!(listing["unpriced"], 2 + 2 + 1);
    // The keys went in the headers each API reads, never the query.
    for request in gemini.requests() {
        assert_eq!(request.header("x-goog-api-key"), Some(GEMINI_KEY));
        assert!(!request.path.contains("key"), "{}", request.path);
    }
    assert_eq!(
        openai.requests()[0].header("authorization"),
        Some(format!("Bearer {OPENAI_KEY}").as_str())
    );
    assert_eq!(
        anthropic.requests()[0].header("x-api-key"),
        Some(ANTHROPIC_KEY)
    );
    assert_eq!(
        anthropic.requests()[0].header("anthropic-version"),
        Some("2023-06-01")
    );
    assert_eq!(local.requests()[0].header("x-api-key"), Some(LOCAL_KEY));
    assert_eq!(local.requests()[0].header("authorization"), None);
    gemini.finish();
    openai.finish();
    anthropic.finish();
    local.finish();
}

#[test]
fn the_cache_is_shown_with_its_age_until_a_refresh_asks_again() {
    let (gemini, openai) = (gemini(), openai());
    let silent = silent();
    let mut repo = Repo::new("version = 1\n");
    repo.0.set_env("GEMINI_API_KEY", GEMINI_KEY);
    repo.0.set_env("CODEX_API_KEY", OPENAI_KEY);
    let urls = Urls {
        gemini: &gemini.url(),
        openai: &openai.url(),
        anthropic: &silent.url(),
    };
    let first = repo.list_json(&urls, &[]);
    assert_eq!(first["cache"]["refreshed"], true);
    assert_eq!(source(&first, "gemini")["cached"], false);
    // CODEX_API_KEY stands in for OPENAI_API_KEY.
    assert_eq!(source(&first, "openai")["key"], "$CODEX_API_KEY");
    assert_eq!(source(&first, "anthropic")["status"], "skipped");
    let asked = (gemini.requests().len(), openai.requests().len());
    assert_eq!(asked, (2, 1));

    // Again: from the cache, with its age, and nothing is asked.
    let second = repo.list_json(&urls, &[]);
    assert_eq!(second["cache"]["refreshed"], false);
    let g = source(&second, "gemini");
    assert_eq!(g["cached"], true);
    assert_eq!(
        g["fetched_at_ms"],
        source(&first, "gemini")["fetched_at_ms"]
    );
    assert!(g["age_s"].as_u64().is_some());
    assert_eq!(names(g), names(source(&first, "gemini")));
    let table = text(&repo.list(&urls, &[]));
    assert!(
        table.contains("fetched ") && table.contains(", cached"),
        "{table}"
    );
    assert_eq!(
        (gemini.requests().len(), openai.requests().len()),
        asked,
        "the cache answered"
    );

    // The cache is the live catalog's: named in its manifest, checksummed.
    let status = repo.json(&["catalog", "status", "--json"]);
    let models = &status["manifest"]["models"];
    assert_eq!(models["file"], "models.json");
    assert_eq!(models["count"], 6);
    let manifest_sha = models["sha256"].as_str().unwrap().to_owned();
    let cached = repo.dir.join("catalog/models.json");
    let mut file = std::fs::read(&cached).unwrap();
    assert!(!String::from_utf8_lossy(&file).contains(GEMINI_KEY));

    // --refresh asks again.
    let third = repo.list_json(&urls, &["--refresh"]);
    assert_eq!(third["cache"]["refreshed"], true);
    assert_eq!(source(&third, "gemini")["cached"], false);
    assert_eq!(gemini.requests().len(), 4);
    assert_eq!(openai.requests().len(), 2);

    // A cache that does not match its checksum is refused, not believed.
    file.extend_from_slice(b" ");
    std::fs::write(&cached, &file).unwrap();
    assert_ne!(manifest_sha, "");
    let out = repo.list(&urls, &[]);
    assert!(out.status.success(), "{}", shown(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refused"),
        "{}",
        shown(&out)
    );
    // ... and listed again from the sources.
    assert_eq!(gemini.requests().len(), 6);
    assert!(silent.requests().is_empty());
    gemini.finish();
    openai.finish();
    silent.finish();
}

#[test]
fn a_source_without_a_key_is_one_line_and_is_not_asked() {
    let (gemini, openai, anthropic) = (silent(), silent(), silent());
    let repo = Repo::new("version = 1\n");
    let urls = Urls {
        gemini: &gemini.url(),
        openai: &openai.url(),
        anthropic: &anthropic.url(),
    };
    let table = text(&repo.list(&urls, &[]));
    for id in ["gemini", "openai", "anthropic"] {
        let line = table
            .lines()
            .find(|l| l.starts_with(id))
            .unwrap_or_else(|| panic!("{id} in:\n{table}"));
        assert!(line.contains("skipped: no credential"), "{line}");
    }
    assert!(table.contains("No source listed models"), "{table}");
    let listing = repo.list_json(&urls, &[]);
    for id in ["gemini", "openai", "anthropic"] {
        let s = source(&listing, id);
        assert_eq!(s["status"], "skipped", "{id}");
        assert!(
            s["reason"].as_str().unwrap().starts_with("no credential: "),
            "{s}"
        );
    }
    assert_eq!(listing["models"], 0);
    // Nothing was cached, so nothing is claimed to be.
    assert!(!repo.dir.join("catalog/models.json").exists());
    for server in [gemini, openai, anthropic] {
        assert!(server.requests().is_empty());
        server.finish();
    }
}

#[test]
fn a_harness_shows_its_static_list_beside_what_its_keys_provider_lists() {
    let (gemini, openai, anthropic) = (silent(), openai(), silent());
    let mut repo = Repo::new("version = 1\n");
    repo.0.set_env("OPENAI_API_KEY", OPENAI_KEY);
    let urls = Urls {
        gemini: &gemini.url(),
        openai: &openai.url(),
        anthropic: &anthropic.url(),
    };
    // Codex: catalog/harnesses.toml lists gpt-5.5 (live) and gpt-6-astra (not).
    let out = repo.list(&urls, &["--harness", "codex"]);
    let view = text(&out);
    assert_eq!(row(&view, "gpt-5.5"), ["gpt-5.5", "listed"], "{view}");
    assert_eq!(row(&view, "gpt-6-astra"), ["gpt-6-astra", "not", "listed"]);
    assert!(
        view.contains("live models the catalog list does not name")
            && view.contains("gpt-7-new")
            && view.contains("whisper-1"),
        "{view}"
    );
    assert!(view.contains("Branchyard drives it"), "{view}");
    let listing = repo.list_json(&urls, &["--harness", "codex"]);
    assert_eq!(listing["harness"]["id"], "codex");
    assert_eq!(listing["harness"]["drives"], true);
    let comparison = &listing["harness"]["sources"][0]["comparison"];
    let standing = |name: &str| {
        comparison["static_models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {comparison}"))["standing"]
            .clone()
    };
    assert_eq!(standing("gpt-5.5"), "listed");
    assert_eq!(standing("gpt-6-sol"), "not_listed");
    assert_eq!(
        comparison["only_live"],
        json!(["gpt-5.4-mini", "gpt-7-new", "whisper-1"])
    );
    // Only the provider of its key was asked.
    let sources = listing["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["source"], "openai");

    // Gemini CLI's catalog entry lists no models; Gemini's source is the one
    // its key is for, and it has no key here.
    let view = text(&repo.list(&urls, &["--harness", "gemini-cli"]));
    assert!(
        !view.contains("the catalog lists no models for it"),
        "{view}"
    );
    assert!(view.contains("skipped: no credential"), "{view}");
    // A harness no listing covers says so.
    let view = text(&repo.list(&urls, &["--harness", "aider"]));
    assert!(view.contains("no live source"), "{view}");
    // An unknown one is an error that names the way to the list.
    let out = repo.list(&urls, &["--harness", "no-such-harness"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("by harnesses --all"),
        "{}",
        shown(&out)
    );
    assert!(gemini.requests().is_empty() && anthropic.requests().is_empty());
    gemini.finish();
    openai.finish();
    anthropic.finish();
}

#[test]
fn listing_is_refused_inside_a_branch() {
    let mut repo = Repo::new("version = 1\n");
    repo.0.set_env("BRANCHYARD_BRANCH", "a-branch");
    repo.0.set_env("OPENAI_API_KEY", OPENAI_KEY);
    let out = repo.by(&["models", "list"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("turn's token") && !err.contains(OPENAI_KEY),
        "{err}"
    );
}

#[test]
fn the_usage_view_is_still_what_by_models_prints_without_a_subcommand() {
    let repo = Repo::new("version = 1\n");
    let out = repo.by(&["models", "--json"]);
    assert!(out.status.success(), "{}", shown(&out));
    let view: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(view["configured"], false);
    assert!(view["usage"].is_object());
}
