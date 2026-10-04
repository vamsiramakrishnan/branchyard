//! Live catalogs: the connector and harness catalogs refreshed from live
//! registries, only when asked (`by catalog refresh`), never on the hot
//! path. `catalog/connectors.toml` and `catalog/harnesses.toml` stay the
//! pinned baseline; a refresh adds to them:
//!
//! - **connectors** from the official MCP registry
//!   (`GET {registry}/v0/servers?limit=100&version=latest&cursor=...`,
//!   each server's latest version), mapped to catalog entries pinned at
//!   that version, and matched to a baseline entry with the same URL or
//!   package;
//! - **harness versions** from the npm registry (`GET {npm}/{package}/
//!   latest`) for every harness the catalog installs with `npm install -g`,
//!   pinned with the release's integrity hash, for `by harnesses update` to
//!   suggest.
//!
//! Every response is cached with its ETag and sent back as
//! `If-None-Match`, so an unchanged page costs a `304`. The cache
//! (`$BRANCHYARD_CATALOG_DIR`, else `~/.cache/branchyard/catalog`) keeps a
//! manifest with the SHA-256 of every file it names; a reader verifies
//! each file against it and refuses one that does not match.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use branchyard_client::http;
use branchyard_controls::catalog;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The variable naming the cache directory.
pub const ENV_DIR: &str = "BRANCHYARD_CATALOG_DIR";
/// The largest response kept.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// One cached response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cached {
    /// The file under the cache directory.
    pub file: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// One derived catalog file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Derived {
    pub file: String,
    pub sha256: String,
    /// Where it came from.
    pub registry: String,
    pub fetched_at_ms: u64,
    pub count: usize,
}

/// `manifest.json`: every file the cache holds, with its checksum.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Raw responses by URL.
    #[serde(default)]
    pub responses: BTreeMap<String, Cached>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connectors: Option<Derived>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harnesses: Option<Derived>,
}

/// A connector from the MCP registry, pinned at the version read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveConnector {
    /// The registry's name, such as `io.github.owner/server`.
    pub id: String,
    pub name: String,
    pub description: String,
    /// The pinned version.
    pub version: String,
    /// `remote-mcp` or `stdio-package`, as in `catalog/connectors.toml`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `identifier@version`, and the package registry it is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_type: Option<String>,
    /// `server`, `header`, `env` or `none`, as in the baseline.
    pub auth: String,
    /// Credential names (headers or variables), required ones first.
    pub credentials: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// The baseline entry with the same URL or package, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
    /// `mcp-registry:<name>@<version>`.
    pub source: String,
}

/// A harness's latest release on npm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveHarness {
    /// The catalog's harness ID.
    pub id: String,
    pub package: String,
    pub latest: String,
    /// npm's `dist.integrity` of that release, its pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
}

/// The cache directory.
pub fn dir() -> PathBuf {
    match std::env::var_os(ENV_DIR).filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from(".cache"))
            .join("branchyard")
            .join("catalog"),
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn read_manifest(dir: &Path) -> Result<Option<Manifest>, String> {
    let path = dir.join("manifest.json");
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("{} is not a catalog manifest: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// `file` under `dir`, if it matches `sha256`.
fn verified(dir: &Path, file: &str, sha: &str) -> Result<Vec<u8>, String> {
    if file.contains("..") || file.starts_with('/') {
        return Err(format!(
            "the catalog manifest names {file:?}, outside its cache"
        ));
    }
    let path = dir.join(file);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let actual = sha256(&bytes);
    match actual == sha {
        true => Ok(bytes),
        false => Err(format!(
            "{} does not match its checksum in the catalog manifest (sha256 {actual}, recorded \
             {sha}); refusing it. Run `by catalog refresh` to fetch it again",
            path.display()
        )),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes).map_err(|e| format!("{}: {e}", temp.display()))?;
    fs::rename(&temp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// What the cache holds, each derived file verified: `None` for a part
/// never refreshed. An error when a file does not match its checksum.
pub struct Loaded {
    pub manifest: Manifest,
    pub connectors: Option<Vec<LiveConnector>>,
    pub harnesses: Option<Vec<LiveHarness>>,
}

pub fn load(dir: &Path) -> Result<Option<Loaded>, String> {
    let Some(manifest) = read_manifest(dir)? else {
        return Ok(None);
    };
    let connectors = match &manifest.connectors {
        Some(d) => Some(
            serde_json::from_slice(&verified(dir, &d.file, &d.sha256)?)
                .map_err(|e| format!("{}: {e}", d.file))?,
        ),
        None => None,
    };
    let harnesses = match &manifest.harnesses {
        Some(d) => Some(
            serde_json::from_slice(&verified(dir, &d.file, &d.sha256)?)
                .map_err(|e| format!("{}: {e}", d.file))?,
        ),
        None => None,
    };
    Ok(Some(Loaded {
        manifest,
        connectors,
        harnesses,
    }))
}

/// A GET of `url` with `If-None-Match` from `cached` when its file still
/// verifies: the body, and the response as the manifest records it.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-cli
fn fetch(dir: &Path, url: &str, cached: Option<&Cached>) -> Result<(Vec<u8>, Cached), String> {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let endpoint = http::Endpoint::parse(base)?;
    let tls = match endpoint.tls {
        true => Some(http::tls_config(None)?),
        false => None,
    };
    let reusable = cached.and_then(|c| {
        verified(dir, &c.file, &c.sha256)
            .ok()
            .map(|bytes| (c.clone(), bytes))
    });
    let mut headers = vec![("Accept", "application/json".to_owned())];
    if let Some((
        Cached {
            etag: Some(etag), ..
        },
        _,
    )) = &reusable
    {
        headers.push(("If-None-Match", etag.clone()));
    }
    let target = match query {
        Some(query) => format!("{}?{query}", endpoint.prefix),
        None => match endpoint.prefix.is_empty() {
            true => "/".to_owned(),
            false => endpoint.prefix.clone(),
        },
    };
    let stream = http::connect(&endpoint, tls.as_ref(), Duration::from_secs(30))
        .map_err(|e| format!("{url}: {e}"))?;
    let response = http::send(
        stream,
        &endpoint,
        &http::Request {
            method: "GET",
            target: &target,
            headers: &headers,
            body: None,
        },
    )
    .map_err(|e| format!("{url}: {e}"))?;
    match response.status {
        304 => match reusable {
            Some((cached, bytes)) => Ok((bytes, cached)),
            None => Err(format!("{url} answered 304 to a request without an ETag")),
        },
        200 => {
            let etag = response.header("etag").map(str::to_owned);
            let mut body = Vec::new();
            let limit = MAX_BODY;
            let read = response
                .read_body(limit)
                .map_err(|e| format!("{url}: {e}"))?;
            body.extend_from_slice(&read);
            serde_json::from_slice::<Value>(&body)
                .map_err(|e| format!("{url} did not answer JSON: {e}"))?;
            let sha = sha256(&body);
            let file = format!("responses/{}.json", &sha256(url.as_bytes())[..32]);
            write_private(&dir.join(&file), &body)?;
            Ok((
                body,
                Cached {
                    file,
                    sha256: sha,
                    etag,
                },
            ))
        }
        status => {
            let mut text = String::new();
            let _ = response.body.take(500).read_to_string(&mut text);
            Err(format!("{url} answered {status}: {}", text.trim()))
        }
    }
}

/// What a refresh did.
#[derive(Debug, Default, Serialize)]
pub struct Refreshed {
    /// Requests made, and how many were answered `304`.
    pub requests: usize,
    pub not_modified: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectors: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harnesses: Option<usize>,
    pub dir: PathBuf,
}

pub struct Sources<'a> {
    pub mcp_registry: &'a str,
    pub npm_registry: &'a str,
    pub max_pages: usize,
    pub connectors: bool,
    pub harnesses: bool,
}

/// Fetch, verify and cache what `sources` name.
pub fn refresh(dir: &Path, sources: &Sources<'_>) -> Result<Refreshed, String> {
    let mut manifest = read_manifest(dir).unwrap_or_default().unwrap_or_default();
    manifest.version = 1;
    let mut done = Refreshed {
        dir: dir.to_path_buf(),
        ..Refreshed::default()
    };
    let mut get = |manifest: &mut Manifest, url: String| -> Result<Vec<u8>, String> {
        let before = manifest.responses.get(&url).cloned();
        let (body, cached) = fetch(dir, &url, before.as_ref())?;
        done.requests += 1;
        if before.as_ref() == Some(&cached) {
            done.not_modified += 1;
        }
        manifest.responses.insert(url, cached);
        Ok(body)
    };
    let now = branchyard_support::time::now_ms();
    let mut connectors_count = None;
    if sources.connectors {
        let base = sources.mcp_registry.trim_end_matches('/');
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..sources.max_pages.max(1) {
            let url = match &cursor {
                Some(c) => format!(
                    "{base}/v0/servers?limit=100&version=latest&cursor={}",
                    http::encode(c)
                ),
                None => format!("{base}/v0/servers?limit=100&version=latest"),
            };
            let page: Value = serde_json::from_slice(&get(&mut manifest, url)?)
                .map_err(|e| format!("the MCP registry's page: {e}"))?;
            for item in page["servers"].as_array().into_iter().flatten() {
                if let Some(entry) = connector(item) {
                    found.push(entry);
                }
            }
            cursor = page["metadata"]["nextCursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        found.sort_by(|a, b| a.id.cmp(&b.id));
        found.dedup_by(|a, b| a.id == b.id);
        let bytes = serde_json::to_vec_pretty(&found).map_err(|e| e.to_string())?;
        write_private(&dir.join("connectors.json"), &bytes)?;
        manifest.connectors = Some(Derived {
            file: "connectors.json".into(),
            sha256: sha256(&bytes),
            registry: base.to_owned(),
            fetched_at_ms: now,
            count: found.len(),
        });
        connectors_count = Some(found.len());
    }
    let mut harness_count = None;
    if sources.harnesses {
        let base = sources.npm_registry.trim_end_matches('/');
        let mut found = Vec::new();
        for (id, package) in npm_packages() {
            let url = format!("{base}/{}/latest", package.replace('/', "%2f"));
            let release: Value = serde_json::from_slice(&get(&mut manifest, url)?)
                .map_err(|e| format!("npm's {package}: {e}"))?;
            if let Some(latest) = release["version"].as_str() {
                found.push(LiveHarness {
                    id,
                    package,
                    latest: latest.to_owned(),
                    integrity: release["dist"]["integrity"].as_str().map(str::to_owned),
                });
            }
        }
        let bytes = serde_json::to_vec_pretty(&found).map_err(|e| e.to_string())?;
        write_private(&dir.join("harnesses.json"), &bytes)?;
        manifest.harnesses = Some(Derived {
            file: "harnesses.json".into(),
            sha256: sha256(&bytes),
            registry: base.to_owned(),
            fetched_at_ms: now,
            count: found.len(),
        });
        harness_count = Some(found.len());
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    write_private(&dir.join("manifest.json"), &bytes)?;
    done.connectors = connectors_count;
    done.harnesses = harness_count;
    Ok(done)
}

/// The npm package each catalog harness installs with `npm install -g`,
/// by harness ID.
pub fn npm_packages() -> Vec<(String, String)> {
    catalog::harnesses()
        .iter()
        .filter_map(|h| {
            h.install.iter().find_map(|command| {
                let words: Vec<&str> = command.split_whitespace().collect();
                let at = words
                    .windows(3)
                    .position(|w| w[0] == "npm" && w[1] == "install" && w[2] == "-g")?;
                words[at + 3..]
                    .iter()
                    .find(|w| !w.starts_with('-'))
                    .map(|p| (h.id.clone(), (*p).to_owned()))
            })
        })
        .collect()
}

/// One MCP registry server as a catalog entry; `None` for one with
/// neither a remote nor a package.
fn connector(item: &Value) -> Option<LiveConnector> {
    let server = &item["server"];
    let name = server["name"].as_str()?.to_owned();
    let version = server["version"].as_str().unwrap_or("").to_owned();
    let text = |v: &Value| v.as_str().map(str::to_owned);
    let names = |list: &Value| -> Vec<String> {
        let mut required = Vec::new();
        let mut optional = Vec::new();
        for entry in list.as_array().into_iter().flatten() {
            if let Some(n) = entry["name"].as_str() {
                match entry["isRequired"].as_bool() == Some(true) {
                    true => required.push(n.to_owned()),
                    false => optional.push(n.to_owned()),
                }
            }
        }
        required.extend(optional);
        required
    };
    let remote = server["remotes"].as_array().and_then(|r| r.first());
    let package = server["packages"].as_array().and_then(|p| p.first());
    let (kind, url, pkg, registry_type, credentials, auth) = match (remote, package) {
        (Some(remote), _) => {
            let credentials = names(&remote["headers"]);
            let auth = match credentials.is_empty() {
                true => "server",
                false => "header",
            };
            (
                "remote-mcp",
                text(&remote["url"]),
                None,
                None,
                credentials,
                auth,
            )
        }
        (None, Some(package)) => {
            let identifier = package["identifier"].as_str()?;
            let pinned = match package["version"].as_str() {
                Some(v) if !v.is_empty() => format!("{identifier}@{v}"),
                _ => identifier.to_owned(),
            };
            let credentials = names(&package["environmentVariables"]);
            let auth = match credentials.is_empty() {
                true => "none",
                false => "env",
            };
            (
                "stdio-package",
                None,
                Some(pinned),
                text(&package["registryType"]),
                credentials,
                auth,
            )
        }
        (None, None) => return None,
    };
    let same_as = catalog::connectors()
        .iter()
        .find(|c| {
            (url.is_some() && c.url == url)
                || pkg.as_deref().is_some_and(|p| {
                    c.package
                        .as_deref()
                        .is_some_and(|q| unversioned(q) == unversioned(p))
                })
        })
        .map(|c| c.id.clone());
    Some(LiveConnector {
        source: format!("mcp-registry:{name}@{version}"),
        name: server["title"].as_str().unwrap_or(&name).to_owned(),
        description: server["description"].as_str().unwrap_or("").to_owned(),
        homepage: text(&server["repository"]["url"]).or_else(|| text(&server["websiteUrl"])),
        id: name,
        version,
        kind: kind.to_owned(),
        url,
        package: pkg,
        registry_type,
        auth: auth.to_owned(),
        credentials,
        same_as,
    })
}

/// A package name without its `@version` (a scope's leading `@` kept).
fn unversioned(package: &str) -> &str {
    match package.strip_prefix('@') {
        Some(rest) => match rest.find('@') {
            Some(i) => &package[..i + 1],
            None => package,
        },
        None => package.split('@').next().unwrap_or(package),
    }
}

/// The cached latest release of harness `id`, when a refresh found one
/// and the cache verifies.
pub fn latest(id: &str) -> Option<LiveHarness> {
    load(&dir())
        .ok()
        .flatten()?
        .harnesses?
        .into_iter()
        .find(|h| h.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_installed_harnesses_name_their_packages() {
        let packages: BTreeMap<String, String> = npm_packages().into_iter().collect();
        assert_eq!(
            packages.get("codex").map(String::as_str),
            Some("@openai/codex")
        );
        assert_eq!(
            packages.get("opencode").map(String::as_str),
            Some("opencode-ai")
        );
        assert!(!packages.contains_key("claude-code"), "installed with curl");
    }

    #[test]
    fn registry_servers_become_pinned_entries() {
        let remote: Value = serde_json::json!({"server": {
            "name": "io.example/notes", "title": "Notes", "description": "d", "version": "1.2.0",
            "remotes": [{"type": "streamable-http", "url": "https://notes.example/mcp",
                "headers": [{"name": "Authorization", "isRequired": true}]}]}});
        let entry = connector(&remote).unwrap();
        assert_eq!(entry.kind, "remote-mcp");
        assert_eq!(entry.auth, "header");
        assert_eq!(entry.credentials, ["Authorization"]);
        assert_eq!(entry.source, "mcp-registry:io.example/notes@1.2.0");
        let package: Value = serde_json::json!({"server": {
            "name": "io.example/fs", "version": "0.1.5",
            "packages": [{"registryType": "npm", "identifier": "@example/fs", "version": "0.1.5",
                "environmentVariables": [{"name": "OPT"}, {"name": "KEY", "isRequired": true}]}]}});
        let entry = connector(&package).unwrap();
        assert_eq!(entry.package.as_deref(), Some("@example/fs@0.1.5"));
        assert_eq!(entry.credentials, ["KEY", "OPT"]);
        assert_eq!(entry.auth, "env");
        assert!(connector(&serde_json::json!({"server": {"name": "x"}})).is_none());
        assert_eq!(unversioned("@example/fs@0.1.5"), "@example/fs");
        assert_eq!(unversioned("pkg@2"), "pkg");
    }
}
