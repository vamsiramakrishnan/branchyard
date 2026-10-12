//! Model discovery: what each provider's own API says it serves, for the
//! credentials this machine holds (`by models list`; docs/model-gateway.md,
//! "Listing models").
//!
//! `catalog/harnesses.toml` carries a static list of each harness's models,
//! which goes stale the day a provider ships a model. A [`Source`] is
//! somewhere models can be listed: Gemini's API, OpenAI's, Anthropic's, and
//! each `[models.backends]` entry that has a key. [`resolve`] lists each
//! source whose credential is present, live or from a cache the caller
//! keeps, and says for each one that is not listed why (no credential, or
//! the status it answered), never quoting a key or a response body.
//!
//! - **Gemini**: `GET /v1beta/models?pageSize=100`, `x-goog-api-key`,
//!   paged by `nextPageToken`; only models whose
//!   `supportedGenerationMethods` has `generateContent`, named without
//!   `models/`. The same key serves the OpenAI-compatible endpoint
//!   ([`Source::openai_compatible`]).
//! - **OpenAI**: `GET /v1/models`, `Authorization: Bearer`.
//! - **Anthropic**: `GET /v1/models?limit=1000`, `x-api-key` and
//!   `anthropic-version`, paged by `has_more` and `last_id` (a page holds
//!   20 models unless asked for more).
//! - **A configured backend**: `GET {url}/v1/models`, or `{url}/models`
//!   when the URL already ends in `/v1` or `/openai`, with its key in the
//!   header the gateway sends it in.
//!
//! Requests go through a [`Transport`], so `HTTPS_PROXY` and the bundle
//! `SSL_CERT_FILE` names are honored. What prices a listed model is the
//! caller's to look up (`doctor::price_source`), as the gateway does.

use std::collections::HashSet;
use std::fmt;
use std::io::Read as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::upstream::{Target, Transport};
use super::{Api, Config, KeySource};

/// Gemini's API.
pub const GEMINI_URL: &str = "https://generativelanguage.googleapis.com";
/// OpenAI's API.
pub const OPENAI_URL: &str = "https://api.openai.com";
/// Anthropic's API.
pub const ANTHROPIC_URL: &str = "https://api.anthropic.com";

/// The variables that hold a Gemini API key, in order of preference.
pub const GEMINI_KEYS: [&str; 2] = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];
/// The variables that hold an OpenAI API key, in order of preference.
pub const OPENAI_KEYS: [&str; 2] = ["OPENAI_API_KEY", "CODEX_API_KEY"];
/// The variable that holds an Anthropic API key.
pub const ANTHROPIC_KEYS: [&str; 1] = ["ANTHROPIC_API_KEY"];

/// The most pages followed from one source.
const MAX_PAGES: usize = 50;
/// The largest page read.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Anthropic's API version header.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Everything but `A-Z a-z 0-9 - . _ ~`, escaped in a query value.
const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// How a source lists its models, and how it is authenticated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum Kind {
    /// Gemini's API: `x-goog-api-key`, paged by `nextPageToken`.
    Gemini,
    /// OpenAI's API: a bearer token.
    Openai,
    /// Anthropic's API: `x-api-key`, paged by `last_id`.
    Anthropic,
    /// Anything that lists as OpenAI's API does, with the key in the
    /// header a generic backend names (default a bearer token).
    Generic,
}

/// What came of a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum Status {
    /// Its models were listed.
    Listed,
    /// Not asked: it has no credential here, or repeats another source.
    Skipped,
    /// Asked, and it did not answer with a list.
    Failed,
}

/// A key that was read. It is never printed, serialized or put in an
/// error; [`Credential::from`] says where it came from.
#[derive(Clone)]
pub struct Credential {
    from: String,
    secret: String,
}

impl Credential {
    /// Where the key is (`$GEMINI_API_KEY`, `@/run/key`), not the key.
    pub fn from(&self) -> &str {
        &self.from
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Credential({}, redacted)", self.from)
    }
}

/// Somewhere models can be listed.
#[derive(Clone, Debug)]
pub struct Source {
    /// `gemini`, `openai`, `anthropic`, or `backend:NAME`.
    pub id: String,
    /// How it lists, and how it is authenticated.
    pub kind: Kind,
    /// The API's base URL, without a trailing slash.
    pub base: String,
    /// Where its key may be, in order of preference; the first that is set
    /// is used.
    pub keys: Vec<KeySource>,
    /// For a generic backend, the header that carries the key.
    pub header: Option<String>,
    /// The `[models.backends]` entry this is, if any.
    pub backend: Option<String>,
    /// Another source with the same endpoint and key, listed instead.
    pub duplicate_of: Option<String>,
}

/// The base URLs of the three providers' own sources; the public APIs
/// unless a caller says otherwise.
#[derive(Clone, Debug)]
pub struct Bases {
    /// Gemini's API.
    pub gemini: String,
    /// OpenAI's API.
    pub openai: String,
    /// Anthropic's API.
    pub anthropic: String,
}

impl Default for Bases {
    fn default() -> Bases {
        Bases {
            gemini: GEMINI_URL.to_owned(),
            openai: OPENAI_URL.to_owned(),
            anthropic: ANTHROPIC_URL.to_owned(),
        }
    }
}

/// The sources this machine could list: Gemini, OpenAI and Anthropic (their
/// keys named by `key`, which resolves a variable or secret name to where
/// it is), then each of `config`'s backends that names a key or is
/// reachable without one. A source whose endpoint and key an earlier one
/// shares is marked, so it is not listed twice.
pub fn sources(config: &Config, bases: &Bases, key: impl Fn(&str) -> KeySource) -> Vec<Source> {
    let named = |names: &[&str]| names.iter().map(|name| key(name)).collect::<Vec<_>>();
    let provider = |id: &str, kind: Kind, base: &str, keys: Vec<KeySource>| Source {
        id: id.to_owned(),
        kind,
        base: base.trim_end_matches('/').to_owned(),
        keys,
        header: None,
        backend: None,
        duplicate_of: None,
    };
    let mut found = vec![
        provider("gemini", Kind::Gemini, &bases.gemini, named(&GEMINI_KEYS)),
        provider("openai", Kind::Openai, &bases.openai, named(&OPENAI_KEYS)),
        provider(
            "anthropic",
            Kind::Anthropic,
            &bases.anthropic,
            named(&ANTHROPIC_KEYS),
        ),
    ];
    for (name, backend) in &config.backends {
        let Some(url) = backend.url.as_deref().or(backend.api.default_url()) else {
            continue; // a generic backend without a URL: `by config validate` says so
        };
        let kind = match backend.api {
            Api::Anthropic => Kind::Anthropic,
            Api::Openai => Kind::Openai,
            Api::Generic => Kind::Generic,
        };
        found.push(Source {
            id: format!("backend:{name}"),
            kind,
            base: url.trim_end_matches('/').to_owned(),
            keys: backend.key.iter().map(|k| key(k)).collect(),
            header: backend.header.clone(),
            backend: Some(name.clone()),
            duplicate_of: None,
        });
    }
    for i in 1..found.len() {
        let Ok(mine) = found[i].credential() else {
            continue;
        };
        let twin = found[..i]
            .iter()
            .find(|earlier| {
                earlier.duplicate_of.is_none()
                    && earlier.kind == found[i].kind
                    && earlier.base == found[i].base
                    && earlier
                        .credential()
                        .is_ok_and(|theirs| theirs.secret == mine.secret)
            })
            .map(|earlier| earlier.id.clone());
        found[i].duplicate_of = twin;
    }
    found
}

impl Source {
    /// The pricing catalog's API for this source's models: Gemini's and a
    /// generic backend's are looked up as Claude's, then OpenAI's, which is
    /// how the gateway prices a generic backend's calls.
    pub fn api(&self) -> Api {
        match self.kind {
            Kind::Openai => Api::Openai,
            Kind::Anthropic => Api::Anthropic,
            Kind::Gemini | Kind::Generic => Api::Generic,
        }
    }

    /// The first key that is set, or why none is.
    ///
    /// # Errors
    ///
    /// `no credential: ` and what each place said (a variable not set, a
    /// file unreadable), never a key.
    pub fn credential(&self) -> Result<Credential, String> {
        let mut why = Vec::new();
        for source in &self.keys {
            match source.read() {
                Ok(secret) => {
                    return Ok(Credential {
                        from: source.to_string(),
                        secret,
                    })
                }
                Err(e) => why.push(e),
            }
        }
        match why.is_empty() {
            true => Err("no credential: the backend names no key".to_owned()),
            false => Err(format!("no credential: {}", why.join("; "))),
        }
    }

    /// Gemini's OpenAI-compatible endpoint, which the same key serves
    /// (chat completions; it rejects the OpenAI-only `store` field).
    pub fn openai_compatible(&self) -> Option<String> {
        (self.kind == Kind::Gemini).then(|| format!("{}/v1beta/openai", self.base))
    }

    /// The request headers, with `key` in the one this source reads it
    /// from.
    fn headers(&self, key: &str) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Accept-Encoding".to_owned(), "identity".to_owned()),
            (
                "User-Agent".to_owned(),
                concat!("branchyard/", env!("CARGO_PKG_VERSION")).to_owned(),
            ),
        ];
        match (self.kind, self.header.as_deref()) {
            (Kind::Gemini, _) => headers.push(("x-goog-api-key".to_owned(), key.to_owned())),
            (Kind::Anthropic, _) => {
                headers.push(("x-api-key".to_owned(), key.to_owned()));
                headers.push(("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned()));
            }
            (_, Some(header)) if !header.eq_ignore_ascii_case("authorization") => {
                headers.push((header.to_owned(), key.to_owned()));
            }
            _ => headers.push(("Authorization".to_owned(), format!("Bearer {key}"))),
        }
        headers
    }

    /// The path of one page, after the base URL's own path: `/models` where
    /// that already ends in the API's version (or `/openai`), else with it.
    fn page_path(&self, target: &Target, cursor: Option<&str>) -> String {
        let ends = |suffix: &str| target.prefix.ends_with(suffix);
        match self.kind {
            Kind::Gemini => {
                let root = if ends("/v1beta") { "" } else { "/v1beta" };
                match cursor {
                    Some(token) => format!(
                        "{root}/models?pageSize=100&pageToken={}",
                        percent_encoding::utf8_percent_encode(token, COMPONENT)
                    ),
                    None => format!("{root}/models?pageSize=100"),
                }
            }
            Kind::Anthropic => {
                let root = if ends("/v1") { "" } else { "/v1" };
                match cursor {
                    Some(id) => format!(
                        "{root}/models?limit=1000&after_id={}",
                        percent_encoding::utf8_percent_encode(id, COMPONENT)
                    ),
                    None => format!("{root}/models?limit=1000"),
                }
            }
            Kind::Openai | Kind::Generic => match ends("/v1") || ends("/openai") {
                true => "/models".to_owned(),
                false => "/v1/models".to_owned(),
            },
        }
    }

    /// The model names on one page appended to `models`, and the cursor of
    /// the next page, if the source says there is one.
    fn read_page(&self, page: &Value, models: &mut Vec<String>) -> Result<Option<String>, String> {
        let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        match self.kind {
            Kind::Gemini => {
                for model in page["models"].as_array().into_iter().flatten() {
                    let generates =
                        model["supportedGenerationMethods"]
                            .as_array()
                            .is_some_and(|methods| {
                                methods
                                    .iter()
                                    .any(|m| m.as_str() == Some("generateContent"))
                            });
                    let Some(name) = model["name"].as_str() else {
                        continue;
                    };
                    if generates {
                        models.push(name.strip_prefix("models/").unwrap_or(name).to_owned());
                    }
                }
                Ok(text(&page["nextPageToken"]))
            }
            Kind::Anthropic => {
                let list = page["data"]
                    .as_array()
                    .ok_or("the answer is not a model list")?;
                models.extend(list.iter().filter_map(|m| text(&m["id"])));
                Ok(match page["has_more"].as_bool() {
                    Some(true) => text(&page["last_id"]),
                    _ => None,
                })
            }
            Kind::Openai | Kind::Generic => {
                let list = page["data"]
                    .as_array()
                    .or_else(|| page["models"].as_array())
                    .ok_or("the answer is not a model list")?;
                models.extend(
                    list.iter()
                        .filter_map(|m| text(&m["id"]).or_else(|| text(&m["name"]))),
                );
                Ok(None)
            }
        }
    }
}

/// One page, as JSON. A failure says the status or what went wrong, never
/// the body.
fn get(
    transport: &Transport,
    target: &Target,
    path: &str,
    headers: &[(String, String)],
) -> Result<Value, String> {
    let response = transport
        .send(target, "GET", path, headers, &[])
        .map_err(|e| e.to_string())?;
    if !(200..300).contains(&response.status) {
        return Err(format!("answered {}", response.status));
    }
    let mut bytes = Vec::new();
    let mut body = response.body;
    (&mut body)
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading the answer: {e}"))?;
    if bytes.len() > MAX_BODY {
        return Err(format!("the answer is over {} MiB", MAX_BODY >> 20));
    }
    serde_json::from_slice(&bytes).map_err(|_| "the answer is not JSON".to_owned())
}

/// Every model `source` lists with `credential`, sorted, once each.
fn fetch(
    source: &Source,
    credential: &Credential,
    transport: &Transport,
) -> Result<Vec<String>, String> {
    let target = Target::parse(&source.base).map_err(|e| format!("the URL: {e}"))?;
    let headers = source.headers(&credential.secret);
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_PAGES {
        let path = source.page_path(&target, cursor.as_deref());
        let page = get(transport, &target, &path, &headers)?;
        match source.read_page(&page, &mut models)? {
            None => {
                models.sort();
                models.dedup();
                return Ok(models);
            }
            Some(next) if seen.insert(next.clone()) => cursor = Some(next),
            Some(_) => return Err("the source repeats a page cursor".to_owned()),
        }
    }
    Err(format!("more than {MAX_PAGES} pages"))
}

/// What one source answered, at `fetched_at_ms`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// The source's `id`.
    pub source: String,
    /// How the source lists.
    pub kind: Kind,
    /// The source's base URL when it was listed.
    pub url: String,
    /// Whether it was listed, skipped or failed.
    pub status: Status,
    /// Why a source was skipped or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where the key was (`$GEMINI_API_KEY`), never the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Milliseconds since the Unix epoch, when it was asked.
    pub fetched_at_ms: u64,
    /// The models, sorted.
    #[serde(default)]
    pub models: Vec<String>,
}

impl Listing {
    fn without_models(source: &Source, status: Status, reason: String, at: u64) -> Listing {
        Listing {
            source: source.id.clone(),
            kind: source.kind,
            url: source.base.clone(),
            status,
            reason: Some(reason),
            key: None,
            fetched_at_ms: at,
            models: Vec::new(),
        }
    }
}

/// A source and what it came to.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// What was asked.
    pub source: Source,
    /// What it came to.
    pub listing: Listing,
    /// The listing is a cached one, not asked for now.
    pub from_cache: bool,
}

/// List `source` now.
pub fn list(source: &Source, transport: &Transport, now_ms: u64) -> Listing {
    if let Some(twin) = &source.duplicate_of {
        return Listing::without_models(
            source,
            Status::Skipped,
            format!("the same endpoint and key as {twin}"),
            now_ms,
        );
    }
    let credential = match source.credential() {
        Ok(credential) => credential,
        Err(why) => return Listing::without_models(source, Status::Skipped, why, now_ms),
    };
    match fetch(source, &credential, transport) {
        Ok(models) => Listing {
            source: source.id.clone(),
            kind: source.kind,
            url: source.base.clone(),
            status: Status::Listed,
            reason: None,
            key: Some(credential.from),
            fetched_at_ms: now_ms,
            models,
        },
        Err(why) => Listing::without_models(source, Status::Failed, why, now_ms),
    }
}

/// Each of `sources`: skipped when it has no credential, else listed from
/// `cache` (an earlier listing of the same source and URL) unless
/// `refresh`, else asked now.
pub fn resolve(
    sources: &[Source],
    cache: &[Listing],
    refresh: bool,
    transport: &Transport,
    now_ms: u64,
) -> Vec<Resolved> {
    sources
        .iter()
        .map(|source| {
            let cached = cache.iter().find(|c| {
                c.source == source.id && c.url == source.base && c.status == Status::Listed
            });
            let usable = source.duplicate_of.is_none() && source.credential().is_ok();
            match (cached, refresh, usable) {
                (Some(cached), false, true) => Resolved {
                    source: source.clone(),
                    listing: cached.clone(),
                    from_cache: true,
                },
                _ => Resolved {
                    source: source.clone(),
                    listing: list(source, transport, now_ms),
                    from_cache: false,
                },
            }
        })
        .collect()
}

/// The listed sources' listings merged into `cache`: a source listed now
/// replaces its earlier entry, and the rest (other repositories' backends,
/// sources not asked this time) are kept.
pub fn merged(cache: &[Listing], resolved: &[Resolved]) -> Vec<Listing> {
    let mut merged: Vec<Listing> = cache
        .iter()
        .filter(|old| {
            !resolved
                .iter()
                .any(|r| r.listing.status == Status::Listed && r.listing.source == old.source)
        })
        .cloned()
        .collect();
    merged.extend(
        resolved
            .iter()
            .filter(|r| r.listing.status == Status::Listed)
            .map(|r| r.listing.clone()),
    );
    merged.sort_by(|a, b| a.source.cmp(&b.source));
    merged
}

/// The built-in sources whose keys `harness` reads: those of the
/// catalog's `auth_env` for it, and the ones its provisioning reads beyond
/// that (Gemini CLI and Antigravity take `GEMINI_API_KEY` or
/// `GOOGLE_API_KEY`; Hermes, `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`).
pub fn harness_sources(harness: &str, auth_env: &[String]) -> Vec<&'static str> {
    let extra: &[&str] = match harness {
        "gemini-cli" | "antigravity" => &GEMINI_KEYS,
        "hermes" => &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
        _ => &[],
    };
    let reads = |keys: &[&str]| {
        keys.iter()
            .any(|k| auth_env.iter().any(|v| v == k) || extra.contains(k))
    };
    let mut ids = Vec::new();
    for (id, keys) in [
        ("gemini", &GEMINI_KEYS[..]),
        ("openai", &OPENAI_KEYS[..]),
        ("anthropic", &ANTHROPIC_KEYS[..]),
    ] {
        if reads(keys) {
            ids.push(id);
        }
    }
    ids
}

/// How one name of a static list stands against a live listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// The live list has it.
    Listed,
    /// A family word (`sonnet`, `opus`), which the live list has models of.
    Alias,
    /// The live list does not have it.
    NotListed,
}

/// One name of a harness's static list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StaticModel {
    /// The name as the catalog writes it.
    pub name: String,
    /// Where it stands against the live list.
    pub standing: Standing,
    /// The live models it matched (one for a listed name, the family for
    /// an alias).
    pub live: Vec<String>,
}

/// A static list against a live one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Comparison {
    /// Each name of the static list, in its order.
    pub static_models: Vec<StaticModel>,
    /// Live models no name of the static list matched.
    pub only_live: Vec<String>,
}

/// A model name for comparison: lower case, without a `models/` prefix, a
/// trailing `[1m]` or `(High)`, spaces as hyphens.
fn comparable(name: &str) -> String {
    let mut text = name.trim().to_ascii_lowercase();
    text = text.strip_prefix("models/").unwrap_or(&text).to_owned();
    for (open, close) in [('[', ']'), ('(', ')')] {
        if text.ends_with(close) {
            if let Some(at) = text.rfind(open) {
                text.truncate(at);
            }
        }
    }
    text.trim().replace(' ', "-")
}

/// A harness's `static_models` against the `live` listing of its key's
/// source, so that a stale static list shows: a name the live list lacks
/// is `not_listed`, a live model no name matches is in `only_live`.
pub fn compare(static_models: &[String], live: &[String]) -> Comparison {
    let live_names: Vec<(String, &String)> = live.iter().map(|l| (comparable(l), l)).collect();
    let mut matched: HashSet<&str> = HashSet::new();
    let mut compared = Vec::new();
    for name in static_models {
        let wanted = comparable(name);
        let exact: Vec<&String> = live_names
            .iter()
            .filter(|(c, _)| *c == wanted)
            .map(|(_, l)| *l)
            .collect();
        let family = wanted.bytes().all(|b| b.is_ascii_lowercase());
        let alias: Vec<&String> = match exact.is_empty() && family && !wanted.is_empty() {
            true => live_names
                .iter()
                .filter(|(c, _)| c.split('-').any(|part| part == wanted))
                .map(|(_, l)| *l)
                .collect(),
            false => Vec::new(),
        };
        let (standing, found) = match (exact.is_empty(), alias.is_empty()) {
            (false, _) => (Standing::Listed, exact),
            (true, false) => (Standing::Alias, alias),
            (true, true) => (Standing::NotListed, Vec::new()),
        };
        matched.extend(found.iter().map(|l| l.as_str()));
        compared.push(StaticModel {
            name: name.clone(),
            standing,
            live: found.into_iter().cloned().collect(),
        });
    }
    Comparison {
        static_models: compared,
        only_live: live
            .iter()
            .filter(|l| !matched.contains(l.as_str()))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use branchyard_testkit::{MockHttp, Response};
    use serde_json::json;

    use super::*;

    /// A key in a file, so a test reads one without touching the process's
    /// environment.
    fn key_file(dir: &tempfile::TempDir, name: &str, key: &str) -> KeySource {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("{key}\n")).unwrap();
        KeySource::File(path)
    }

    fn unset() -> KeySource {
        KeySource::Env("BRANCHYARD_TEST_THIS_VARIABLE_IS_NEVER_SET".to_owned())
    }

    fn source(id: &str, kind: Kind, base: &str, keys: Vec<KeySource>) -> Source {
        Source {
            id: id.to_owned(),
            kind,
            base: base.trim_end_matches('/').to_owned(),
            keys,
            header: None,
            backend: None,
            duplicate_of: None,
        }
    }

    fn transport() -> Transport {
        Transport::direct(Duration::from_secs(10))
    }

    fn models_of(listing: &Listing) -> Vec<&str> {
        listing.models.iter().map(String::as_str).collect()
    }

    #[test]
    fn gemini_pages_are_followed_and_only_generating_models_kept() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockHttp::start(|request| {
            let page = |names: &[(&str, &[&str])], next: Option<&str>| {
                let models: Vec<Value> = names
                    .iter()
                    .map(|(name, methods)| {
                        json!({"name": name, "supportedGenerationMethods": methods})
                    })
                    .collect();
                let mut body = json!({"models": models});
                if let Some(next) = next {
                    body["nextPageToken"] = json!(next);
                }
                Response::json(200, &body)
            };
            match request.path.as_str() {
                "/v1beta/models?pageSize=100" => page(
                    &[
                        ("models/gemini-2.5-pro", &["generateContent", "countTokens"]),
                        ("models/embedding-001", &["embedContent"]),
                    ],
                    Some("tok/2=="),
                ),
                "/v1beta/models?pageSize=100&pageToken=tok%2F2%3D%3D" => {
                    page(&[("models/gemini-2.5-flash", &["generateContent"])], None)
                }
                other => Response::new(404, other.to_owned()),
            }
        });
        let gemini = source(
            "gemini",
            Kind::Gemini,
            &server.url(),
            vec![unset(), key_file(&dir, "k", "g-secret-key")],
        );
        let listing = list(&gemini, &transport(), 42);
        assert_eq!(listing.status, Status::Listed, "{listing:?}");
        assert_eq!(models_of(&listing), ["gemini-2.5-flash", "gemini-2.5-pro"]);
        assert_eq!(listing.fetched_at_ms, 42);
        assert_eq!(
            listing.key.as_deref(),
            Some(format!("@{}", dir.path().join("k").display()).as_str())
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.method, "GET");
            assert_eq!(request.header("x-goog-api-key"), Some("g-secret-key"));
            assert_eq!(request.header("authorization"), None);
        }
        assert!(!serde_json::to_string(&listing)
            .unwrap()
            .contains("g-secret"));
        assert_eq!(
            gemini.openai_compatible().as_deref(),
            Some(format!("{}/v1beta/openai", server.url()).as_str())
        );
        server.finish();
    }

    #[test]
    fn a_repeated_gemini_page_token_ends_the_walk_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockHttp::start(|_| {
            Response::json(200, &json!({"models": [], "nextPageToken": "same"}))
        });
        let gemini = source(
            "gemini",
            Kind::Gemini,
            &server.url(),
            vec![key_file(&dir, "k", "k")],
        );
        let listing = list(&gemini, &transport(), 1);
        assert_eq!(listing.status, Status::Failed);
        assert_eq!(
            listing.reason.as_deref(),
            Some("the source repeats a page cursor")
        );
        assert_eq!(server.requests().len(), 2);
        server.finish();
    }

    #[test]
    fn openai_lists_with_a_bearer_token_and_the_second_variable_is_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockHttp::start(|request| {
            assert_eq!(request.path, "/v1/models");
            Response::json(
                200,
                &json!({"object": "list", "data": [
                    {"id": "gpt-5", "object": "model"}, {"id": "gpt-5-mini"}, {"id": "gpt-5"}]}),
            )
        });
        let openai = source(
            "openai",
            Kind::Openai,
            &server.url(),
            vec![unset(), key_file(&dir, "codex", "sk-codex")],
        );
        let listing = list(&openai, &transport(), 1);
        assert_eq!(models_of(&listing), ["gpt-5", "gpt-5-mini"], "{listing:?}");
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer sk-codex")
        );
        server.finish();
    }

    #[test]
    fn anthropic_asks_for_a_thousand_a_page_and_follows_has_more() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockHttp::start(|request| match request.path.as_str() {
            "/v1/models?limit=1000" => Response::json(
                200,
                &json!({"data": [{"id": "claude-opus-4-8", "type": "model"}],
                    "has_more": true, "last_id": "claude-opus-4-8"}),
            ),
            "/v1/models?limit=1000&after_id=claude-opus-4-8" => Response::json(
                200,
                &json!({"data": [{"id": "claude-haiku-4-5"}], "has_more": false,
                    "last_id": "claude-haiku-4-5"}),
            ),
            other => Response::new(404, other.to_owned()),
        });
        let anthropic = source(
            "anthropic",
            Kind::Anthropic,
            &server.url(),
            vec![key_file(&dir, "k", "sk-ant-secret")],
        );
        let listing = list(&anthropic, &transport(), 1);
        assert_eq!(
            models_of(&listing),
            ["claude-haiku-4-5", "claude-opus-4-8"],
            "{listing:?}"
        );
        for request in server.requests() {
            assert_eq!(request.header("x-api-key"), Some("sk-ant-secret"));
            assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
            assert_eq!(request.header("authorization"), None);
        }
        server.finish();
    }

    #[test]
    fn a_backend_lists_at_v1_models_or_models_when_its_url_ends_in_v1_or_openai() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockHttp::start(|request| {
            Response::json(
                200,
                &json!({"data": [{"id": format!("served-at{}", request.path)}]}),
            )
        });
        let at = |suffix: &str, header: Option<&str>| {
            let mut backend = source(
                "backend:local",
                Kind::Generic,
                &format!("{}{suffix}", server.url()),
                vec![key_file(&dir, "k", "local-key")],
            );
            backend.header = header.map(str::to_owned);
            list(&backend, &transport(), 1)
        };
        assert_eq!(models_of(&at("", None)), ["served-at/v1/models"]);
        assert_eq!(models_of(&at("/", None)), ["served-at/v1/models"]);
        assert_eq!(models_of(&at("/v1", None)), ["served-at/v1/models"]);
        assert_eq!(models_of(&at("/openai", None)), ["served-at/openai/models"]);
        assert_eq!(
            models_of(&at("/proxy", None)),
            ["served-at/proxy/v1/models"]
        );
        at("", Some("x-api-key"));
        let requests = server.requests();
        assert_eq!(
            requests[0].header("authorization"),
            Some("Bearer local-key")
        );
        let custom = requests.last().unwrap();
        assert_eq!(custom.header("x-api-key"), Some("local-key"));
        assert_eq!(custom.header("authorization"), None);
        server.finish();
    }

    #[test]
    fn a_source_without_a_credential_is_skipped_and_nothing_is_sent() {
        let server = MockHttp::start(|_| Response::new(500, "must not be asked"));
        let gemini = source(
            "gemini",
            Kind::Gemini,
            &server.url(),
            vec![
                KeySource::Env("BRANCHYARD_TEST_NOT_SET_A".to_owned()),
                KeySource::Env("BRANCHYARD_TEST_NOT_SET_B".to_owned()),
            ],
        );
        let listing = list(&gemini, &transport(), 1);
        assert_eq!(listing.status, Status::Skipped);
        let reason = listing.reason.unwrap();
        assert!(reason.starts_with("no credential: "), "{reason}");
        assert!(reason.contains("BRANCHYARD_TEST_NOT_SET_A"), "{reason}");
        assert!(reason.contains("BRANCHYARD_TEST_NOT_SET_B"), "{reason}");
        let mut keyless = source("backend:x", Kind::Generic, &server.url(), Vec::new());
        keyless.backend = Some("x".to_owned());
        assert_eq!(
            list(&keyless, &transport(), 1).reason.as_deref(),
            Some("no credential: the backend names no key")
        );
        assert!(server.requests().is_empty());
        server.finish();
    }

    #[test]
    fn a_failing_source_says_the_status_and_not_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockHttp::start(|request| match request.path.as_str() {
            "/v1/models" => Response::new(401, "invalid x-api-key sk-ant-leaky"),
            _ => Response::new(200, "<html>not json</html>"),
        });
        let failing = |kind, base: &str| {
            list(
                &source("s", kind, base, vec![key_file(&dir, "k", "sk-ant-leaky")]),
                &transport(),
                1,
            )
        };
        let denied = failing(Kind::Openai, &server.url());
        assert_eq!(denied.status, Status::Failed);
        assert_eq!(denied.reason.as_deref(), Some("answered 401"));
        assert!(denied.models.is_empty());
        let html = failing(Kind::Gemini, &server.url());
        assert_eq!(html.reason.as_deref(), Some("the answer is not JSON"));
        // Nothing listens there.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let refused = failing(Kind::Anthropic, &url);
        assert_eq!(refused.status, Status::Failed);
        assert!(
            refused.reason.unwrap().starts_with("could not connect"),
            "connection refused"
        );
        // An OpenAI-shaped answer that is not a list.
        let odd = MockHttp::start(|_| Response::json(200, &json!({"error": "no"})));
        let odd_listing = failing(Kind::Generic, &odd.url());
        assert_eq!(
            odd_listing.reason.as_deref(),
            Some("the answer is not a model list")
        );
        odd.finish();
        for listing in [denied, html] {
            assert!(!serde_json::to_string(&listing).unwrap().contains("leaky"));
        }
        server.finish();
    }

    #[test]
    fn a_cached_listing_is_shown_until_a_refresh_and_a_lost_credential_wins() {
        let dir = tempfile::tempdir().unwrap();
        let server =
            MockHttp::start(|_| Response::json(200, &json!({"data": [{"id": "gpt-live"}]})));
        let openai = source(
            "openai",
            Kind::Openai,
            &server.url(),
            vec![key_file(&dir, "k", "sk-1")],
        );
        let cached = Listing {
            source: "openai".to_owned(),
            kind: Kind::Openai,
            url: server.url(),
            status: Status::Listed,
            reason: None,
            key: Some("$OPENAI_API_KEY".to_owned()),
            fetched_at_ms: 7,
            models: vec!["gpt-cached".to_owned()],
        };
        let sources = [openai.clone()];
        let shown = resolve(
            &sources,
            std::slice::from_ref(&cached),
            false,
            &transport(),
            100,
        );
        assert!(shown[0].from_cache);
        assert_eq!(models_of(&shown[0].listing), ["gpt-cached"]);
        assert_eq!(shown[0].listing.fetched_at_ms, 7);
        assert!(server.requests().is_empty());
        let fresh = resolve(
            &sources,
            std::slice::from_ref(&cached),
            true,
            &transport(),
            100,
        );
        assert!(!fresh[0].from_cache);
        assert_eq!(models_of(&fresh[0].listing), ["gpt-live"]);
        assert_eq!(fresh[0].listing.fetched_at_ms, 100);
        // The cache of another URL is not this source's.
        let mut elsewhere = cached.clone();
        elsewhere.url = "http://127.0.0.1:1".to_owned();
        let other = resolve(&sources, &[elsewhere], false, &transport(), 100);
        assert!(!other[0].from_cache);
        // No credential: skipped, whatever the cache holds.
        let mut keyless = openai.clone();
        keyless.keys = vec![unset()];
        let lost = resolve(
            &[keyless],
            std::slice::from_ref(&cached),
            false,
            &transport(),
            100,
        );
        assert_eq!(lost[0].listing.status, Status::Skipped);
        // Merging keeps the cache's other sources and replaces a refreshed one.
        let other_cache = Listing {
            source: "backend:z".to_owned(),
            ..cached.clone()
        };
        let merged = merged(&[cached, other_cache], &fresh);
        let names: Vec<&str> = merged.iter().map(|l| l.source.as_str()).collect();
        assert_eq!(names, ["backend:z", "openai"]);
        assert_eq!(merged[1].models, ["gpt-live"]);
        assert_eq!(server.requests().len(), 2);
        server.finish();
    }

    #[test]
    fn sources_are_the_three_providers_and_the_backends_with_the_duplicates_marked() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::write(&shared, "sk-shared").unwrap();
        let other = dir.path().join("other");
        std::fs::write(&other, "sk-other").unwrap();
        let config: Config = serde_json::from_value(json!({
            "backends": {
                "anthropic": {"api": "anthropic", "key": "shared"},
                "mirror": {"api": "openai", "key": "shared"},
                "team": {"api": "openai", "key": "other"},
                "local": {"api": "generic", "url": "http://127.0.0.1:11434/", "key": "other",
                    "header": "x-api-key"},
                "bare": {"api": "generic"}
            }
        }))
        .unwrap();
        let key = |name: &str| match name {
            "shared" | "ANTHROPIC_API_KEY" | "OPENAI_API_KEY" => KeySource::File(shared.clone()),
            "other" => KeySource::File(other.clone()),
            _ => unset(),
        };
        let found = sources(&config, &Bases::default(), key);
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        // `bare` is a generic backend without a URL: left to `by config validate`.
        assert_eq!(
            ids,
            [
                "gemini",
                "openai",
                "anthropic",
                "backend:anthropic",
                "backend:local",
                "backend:mirror",
                "backend:team"
            ]
        );
        let by_id = |id: &str| found.iter().find(|s| s.id == id).unwrap();
        assert_eq!(by_id("gemini").keys.len(), 2);
        assert_eq!(by_id("openai").keys.len(), 2);
        assert_eq!(by_id("backend:anthropic").base, ANTHROPIC_URL);
        assert_eq!(
            by_id("backend:anthropic").duplicate_of.as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            by_id("backend:mirror").duplicate_of.as_deref(),
            Some("openai")
        );
        // The same URL with another key lists on its own.
        assert_eq!(by_id("backend:team").duplicate_of, None);
        assert_eq!(by_id("backend:local").base, "http://127.0.0.1:11434");
        assert_eq!(by_id("backend:local").header.as_deref(), Some("x-api-key"));
        assert_eq!(by_id("backend:local").kind, Kind::Generic);
        assert_eq!(by_id("gemini").api(), Api::Generic);
        assert_eq!(by_id("openai").api(), Api::Openai);
        assert_eq!(by_id("anthropic").api(), Api::Anthropic);
        // A duplicate is skipped with the source that stands for it.
        let server = MockHttp::start(|_| Response::json(200, &json!({"data": []})));
        let listing = list(by_id("backend:mirror"), &transport(), 1);
        assert_eq!(listing.status, Status::Skipped);
        assert_eq!(
            listing.reason.as_deref(),
            Some("the same endpoint and key as openai")
        );
        assert!(server.requests().is_empty());
        server.finish();
    }

    #[test]
    fn a_harness_reads_the_sources_of_its_keys() {
        let env = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            harness_sources("claude-code", &env(&["ANTHROPIC_API_KEY"])),
            ["anthropic"]
        );
        assert_eq!(
            harness_sources("codex", &env(&["OPENAI_API_KEY"])),
            ["openai"]
        );
        // Gemini CLI's catalog entry names no key; its provisioning reads these.
        assert_eq!(harness_sources("gemini-cli", &[]), ["gemini"]);
        assert_eq!(harness_sources("antigravity", &[]), ["gemini"]);
        assert_eq!(
            harness_sources(
                "opencode",
                &env(&[
                    "ANTHROPIC_API_KEY",
                    "OPENAI_API_KEY",
                    "GEMINI_API_KEY",
                    "ORCAROUTER_API_KEY"
                ])
            ),
            ["gemini", "openai", "anthropic"]
        );
        assert!(harness_sources("aider", &[]).is_empty());
    }

    #[test]
    fn a_static_list_shows_what_the_live_list_lacks_and_what_it_adds() {
        let live: Vec<String> = [
            "claude-haiku-4-5",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "gpt-5.5",
            "gemini-3.5-flash",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let static_models: Vec<String> = [
            "claude-opus-4-8[1m]",
            "sonnet",
            "claude-fable-5-1[1m]",
            "gpt-5.5",
            "gpt-6-astra",
            "Gemini 3.5 Flash (High)",
            "Gemini 3.1 Pro (Low)",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let compared = compare(&static_models, &live);
        let standing: Vec<(&str, Standing)> = compared
            .static_models
            .iter()
            .map(|m| (m.name.as_str(), m.standing))
            .collect();
        assert_eq!(
            standing,
            [
                ("claude-opus-4-8[1m]", Standing::Listed),
                ("sonnet", Standing::Alias),
                ("claude-fable-5-1[1m]", Standing::NotListed),
                ("gpt-5.5", Standing::Listed),
                ("gpt-6-astra", Standing::NotListed),
                ("Gemini 3.5 Flash (High)", Standing::Listed),
                ("Gemini 3.1 Pro (Low)", Standing::NotListed),
            ]
        );
        assert_eq!(compared.static_models[1].live, ["claude-sonnet-4-6"]);
        // Only the haiku is matched by no name.
        assert_eq!(compared.only_live, ["claude-haiku-4-5"]);
        // An empty static list: everything live is news.
        assert_eq!(compare(&[], &live).only_live.len(), live.len());
    }

    /// An HTTPS origin behind a CONNECT proxy: the proxy answers the
    /// tunnel, then the same socket speaks TLS with a certificate for
    /// `provider.test` and answers the listing. Returns the proxy's
    /// address, the certificate to trust, and the lines the proxy saw
    /// before the handshake.
    fn proxied_origin(
        status: &'static str,
    ) -> (
        std::net::SocketAddr,
        String,
        std::sync::mpsc::Receiver<Vec<String>>,
    ) {
        use std::io::{self, BufRead as _, BufReader, Write as _};
        use std::sync::Arc;

        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["provider.test".to_owned()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
                )
                .unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                lines.push(line.trim_end().to_owned());
            }
            sender.send(lines).unwrap();
            let mut stream = stream;
            if !status.starts_with("200") {
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").unwrap();
                return;
            }
            write!(stream, "HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
            // The client may refuse the certificate: its end of the
            // handshake is then the test's finding, not this thread's.
            branchyard_support::best_effort(
                "serve the tunnelled request",
                (|| -> io::Result<()> {
                    let connection = rustls::ServerConnection::new(config).unwrap();
                    let mut tls = rustls::StreamOwned::new(connection, stream);
                    let mut reader = BufReader::new(&mut tls);
                    let mut request = Vec::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                            break;
                        }
                        request.push(line);
                    }
                    assert!(request[0].starts_with("GET /v1/models "), "{request:?}");
                    let body = r#"{"data":[{"id":"through-the-tunnel"}]}"#;
                    write!(
                        tls,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )?;
                    tls.flush()
                })(),
            );
        });
        (address, pem, seen)
    }

    #[test]
    fn https_goes_through_the_proxy_and_trusts_the_bundle_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let (proxy, pem, seen) = proxied_origin("200");
        let through = Transport::new(
            Some(&format!("http://user:pa%20ss@{proxy}")),
            "",
            Some(&pem),
            Duration::from_secs(10),
        );
        let openai = source(
            "openai",
            Kind::Openai,
            "https://provider.test",
            vec![key_file(&dir, "k", "sk-tunnel")],
        );
        let listing = list(&openai, &through, 1);
        assert_eq!(listing.status, Status::Listed, "{listing:?}");
        assert_eq!(models_of(&listing), ["through-the-tunnel"]);
        let lines = seen.recv().unwrap();
        assert_eq!(lines[0], "CONNECT provider.test:443 HTTP/1.1");
        assert!(
            lines.contains(&"Host: provider.test:443".to_owned()),
            "{lines:?}"
        );
        // "user:pa ss" in base64.
        assert!(
            lines.contains(&"Proxy-Authorization: Basic dXNlcjpwYSBzcw==".to_owned()),
            "{lines:?}"
        );
        // The key is the origin's to read, not the proxy's.
        assert!(!lines.iter().any(|l| l.contains("sk-tunnel")), "{lines:?}");
    }

    #[test]
    fn a_proxy_that_refuses_the_tunnel_is_named_by_its_status_and_a_bundle_is_required_to_vouch() {
        let dir = tempfile::tempdir().unwrap();
        let openai = |base: &str| {
            source(
                "openai",
                Kind::Openai,
                base,
                vec![key_file(&dir, "k", "sk-tunnel")],
            )
        };
        let (proxy, pem, seen) = proxied_origin("403 Forbidden");
        let refused = list(
            &openai("https://provider.test"),
            &Transport::new(
                Some(&proxy.to_string()),
                "",
                Some(&pem),
                Duration::from_secs(10),
            ),
            1,
        );
        seen.recv().unwrap();
        assert_eq!(refused.status, Status::Failed);
        let reason = refused.reason.unwrap();
        assert!(
            reason.contains(&format!("the proxy {}:{}", proxy.ip(), proxy.port()))
                && reason.contains("answered 403 to CONNECT provider.test:443"),
            "{reason}"
        );
        // Without the certificate in the bundle, the origin is not trusted.
        let (proxy, _pem, seen) = proxied_origin("200");
        let untrusted = list(
            &openai("https://provider.test"),
            &Transport::new(Some(&proxy.to_string()), "", None, Duration::from_secs(10)),
            1,
        );
        seen.recv().unwrap();
        assert_eq!(untrusted.status, Status::Failed);
        assert!(untrusted.reason.unwrap().contains("TLS"), "TLS error");
        // A bundle that is not one fails https requests and no others.
        let broken = Transport::new(None, "", Some("not a certificate"), Duration::from_secs(10));
        let https = list(&openai("https://provider.test"), &broken, 1);
        assert!(https.reason.unwrap().contains("holds no certificate"));
        let server = MockHttp::start(|_| Response::json(200, &json!({"data": []})));
        assert_eq!(
            list(&openai(&server.url()), &broken, 1).status,
            Status::Listed
        );
        server.finish();
    }
}
