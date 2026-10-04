// Derived from generalaction/emdash at revision
// 873a3e2067f4abc136272ed2b61abea3a2c07bcf:
// packages/plugins/src/issues/impl/linear/queries.ts and
// packages/plugins/src/issues/impl/linear/mapper.ts,
// packages/plugins/src/issues/impl/jira/mapper.ts, and
// packages/plugins/src/issues/impl/gitlab/mapper.ts.
// Copyright 2026 General Action, Inc. Licensed under the Apache License,
// Version 2.0 (vendor/emdash/LICENSE.md).
// Modified for Branchyard: translated from TypeScript to Rust; emdash's
// SDK clients (@linear/sdk, jira.js, @gitbeaker/rest) are replaced by
// plain requests (Linear GraphQL, Jira REST v3, GitLab REST v4) over
// branchyard-client's HTTP client; only the single-issue lookup is kept
// (no listing, searching, comments or history), Linear's summary fields
// gain the issue's labels, Jira's description is rendered with Orca's
// ADF-to-Markdown port (crate::adf) instead of emdash's flattening, and a
// connector gateway tool can stand in for the token.

//! `--issue` for trackers other than GitHub: `linear:ENG-123`,
//! `jira:PROJ-7`, `gitlab:group/project#12`, or the issue's URL. Fetched
//! with a token from the environment, or through the connector gateway
//! (`[trackers.<name>] gateway_tool`); a token is never printed, logged or
//! recorded. See `docs/pull-requests.md#other-trackers`.

use std::io::Read;
use std::time::Duration;

use branchyard_client::http;
use branchyard_setup::config::{TrackerConfig, Trackers};
use branchyard_wire as wire;
use serde_json::{json, Value};

/// The trackers `--issue` takes besides GitHub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracker {
    Linear,
    Jira,
    GitLab,
}

impl Tracker {
    /// `linear`, `jira`, `gitlab`: the prefix, the `[trackers]` key and
    /// the recorded name.
    pub fn id(self) -> &'static str {
        match self {
            Tracker::Linear => "linear",
            Tracker::Jira => "jira",
            Tracker::GitLab => "gitlab",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tracker::Linear => "Linear",
            Tracker::Jira => "Jira",
            Tracker::GitLab => "GitLab",
        }
    }

    pub fn from_id(id: &str) -> Option<Tracker> {
        match id {
            "linear" => Some(Tracker::Linear),
            "jira" => Some(Tracker::Jira),
            "gitlab" => Some(Tracker::GitLab),
            _ => None,
        }
    }

    /// The variables holding its credentials, for messages and docs.
    pub fn variables(self) -> &'static str {
        match self {
            Tracker::Linear => "LINEAR_API_KEY",
            Tracker::Jira => "JIRA_EMAIL and JIRA_API_TOKEN",
            Tracker::GitLab => "GITLAB_TOKEN",
        }
    }
}

/// An issue reference for one of [`Tracker`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueRef {
    pub tracker: Tracker,
    /// `ENG-123`, `PROJ-7`, or GitLab's project path.
    pub key: String,
    /// GitLab's issue IID.
    pub iid: Option<u64>,
    /// The site the reference's URL named (Jira, GitLab).
    pub site: Option<String>,
}

impl IssueRef {
    /// How the tracker itself writes it: `ENG-123`, `group/project#12`.
    pub fn display(&self) -> String {
        match self.iid {
            Some(iid) => format!("{}#{iid}", self.key),
            None => self.key.clone(),
        }
    }
}

/// A fetched issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub tracker: Tracker,
    /// How the tracker writes it (`ENG-123`, `group/project#12`).
    pub key: String,
    /// The number in it (`123` of `ENG-123`, GitLab's IID).
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub labels: Vec<String>,
}

fn jira_key(text: &str) -> bool {
    // emdash's JIRA_KEY_PATTERN, /^[A-Za-z][A-Za-z0-9_]*-\d+$/, which
    // Linear identifiers (`ENG-123`) match too.
    let Some((project, number)) = text.rsplit_once('-') else {
        return false;
    };
    let mut chars = project.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

fn gitlab_path(text: &str) -> Option<(String, u64)> {
    let (project, iid) = text.rsplit_once('#')?;
    let iid: u64 = iid.parse().ok().filter(|n| *n > 0)?;
    let ok = project.contains('/')
        && project
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    ok.then(|| (project.to_owned(), iid))
}

/// `--issue`'s value as one of [`Tracker`]: `linear:KEY`, `jira:KEY`,
/// `gitlab:GROUP/PROJECT#N`, or a Linear, Jira (`…/browse/KEY`) or GitLab
/// (`…/-/issues/N`) URL. `Ok(None)` is a GitHub reference; a prefix with a
/// malformed key is an error.
pub fn parse(text: &str) -> Result<Option<IssueRef>, String> {
    let text = text.trim();
    let bad = |what: &str| Err(format!("--issue {text:?}: {what}"));
    if let Some((prefix, rest)) = text.split_once(':') {
        if let Some(tracker) = Tracker::from_id(&prefix.to_ascii_lowercase()) {
            let rest = rest.trim();
            return match tracker {
                Tracker::Linear | Tracker::Jira if jira_key(rest) => Ok(Some(IssueRef {
                    tracker,
                    key: rest.to_ascii_uppercase(),
                    iid: None,
                    site: None,
                })),
                Tracker::Linear | Tracker::Jira => bad("expected a key such as ENG-123"),
                Tracker::GitLab => match gitlab_path(rest) {
                    Some((project, iid)) => Ok(Some(IssueRef {
                        tracker,
                        key: project,
                        iid: Some(iid),
                        site: None,
                    })),
                    None => bad("expected GROUP/PROJECT#N, such as gitlab:acme/widgets#12"),
                },
            };
        }
    }
    if !(text.starts_with("https://") || text.starts_with("http://")) {
        return Ok(None);
    }
    let Ok(url) = url::Url::parse(text) else {
        return bad("not a URL");
    };
    let host = url.host_str().unwrap_or_default();
    let parts: Vec<&str> = url
        .path_segments()
        .map(|segments| segments.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let site = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    if host == "linear.app" || host.ends_with(".linear.app") {
        // https://linear.app/<workspace>/issue/ENG-123/<slug>
        return match parts.iter().position(|p| *p == "issue") {
            Some(i) if parts.get(i + 1).is_some_and(|k| jira_key(k)) => Ok(Some(IssueRef {
                tracker: Tracker::Linear,
                key: parts[i + 1].to_ascii_uppercase(),
                iid: None,
                site: None,
            })),
            _ => bad("a Linear URL names an issue as …/issue/KEY"),
        };
    }
    if let Some(i) = parts.iter().position(|p| *p == "browse") {
        if let Some(key) = parts.get(i + 1).filter(|k| jira_key(k)) {
            return Ok(Some(IssueRef {
                tracker: Tracker::Jira,
                key: key.to_ascii_uppercase(),
                iid: None,
                site: Some(site),
            }));
        }
    }
    if let Some(i) = parts.windows(2).position(|w| w == ["-", "issues"]) {
        let iid = parts.get(i + 2).and_then(|n| n.parse::<u64>().ok());
        if let (Some(iid), true) = (iid, i > 1) {
            return Ok(Some(IssueRef {
                tracker: Tracker::GitLab,
                key: parts[..i].join("/"),
                iid: Some(iid),
                site: Some(site),
            }));
        }
    }
    if host == "github.com" || host.starts_with("github.") || parts.contains(&"issues") {
        return Ok(None);
    }
    bad("not a GitHub, Linear, Jira or GitLab issue URL")
}

/// A gateway call: a tool and its arguments to the tool's JSON result.
pub type GatewayCall<'a> = &'a dyn Fn(&str, Value) -> Result<Value, String>;

/// An HTTP answer: status, body, headers.
type Answer = (u16, Vec<u8>, Vec<(String, String)>);

/// Where credentials and settings come from.
pub struct Sources<'a> {
    /// Reads a variable.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub config: &'a Trackers,
    /// Fetch through the gateway: `(tool, arguments)` to the tool's result.
    pub gateway: Option<GatewayCall<'a>>,
}

impl Sources<'_> {
    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|v| !v.trim().is_empty())
    }

    fn settings(&self, tracker: Tracker) -> Option<&TrackerConfig> {
        match tracker {
            Tracker::Linear => self.config.linear.as_ref(),
            Tracker::Jira => self.config.jira.as_ref(),
            Tracker::GitLab => self.config.gitlab.as_ref(),
        }
    }
}

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The largest response read.
const MAX_BODY: usize = 8 << 20;

/// Linear's GraphQL endpoint.
pub const LINEAR_API: &str = "https://api.linear.app/graphql";

/// emdash's `ISSUE_SUMMARY_FIELDS`, with the labels.
const LINEAR_ISSUE_QUERY: &str = "query Issue($id: String!) { issue(id: $id) { id identifier \
     title description url branchName state { name type color } team { name key } project { name \
     } assignee { displayName name } updatedAt labels { nodes { name } } } }";

/// One HTTP request: `(status, body)`. Nothing of `headers` is ever put in
/// an error.
fn request(
    method: &str,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&[u8]>,
) -> Result<Answer, String> {
    let parsed = wire::HttpUrl::parse(url).map_err(|e| e.to_string())?;
    let (origin, target) = (parsed.origin(), parsed.target());
    let endpoint = http::Endpoint {
        tls: parsed.tls,
        host: parsed.host,
        port: parsed.port,
        prefix: String::new(),
        unix: None,
    };
    let tls = match endpoint.tls {
        true => Some(http::tls_config(None)?),
        false => None,
    };
    let stream = http::connect(&endpoint, tls.as_ref(), TIMEOUT)
        .map_err(|e| format!("could not reach {origin}: {e}"))?;
    let response = http::send(
        stream,
        &endpoint,
        &http::Request {
            method,
            target: &target,
            headers,
            body,
        },
    )
    .map_err(|e| format!("{origin}: {e}"))?;
    let status = response.status;
    let headers = response.headers.clone();
    let mut out = Vec::new();
    let mut body = response.body;
    (&mut body)
        .take(MAX_BODY as u64)
        .read_to_end(&mut out)
        .map_err(|e| format!("{origin}: {e}"))?;
    Ok((status, out, headers))
}

fn json_of(bytes: &[u8], what: &str) -> Result<Value, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("{what} answered something not JSON: {e}"))
}

fn refused(tracker: Tracker, status: u16, reference: &str) -> String {
    match status {
        401 | 403 => format!(
            "{} refused the credentials in {} ({status}); check them, or that they may read \
             {reference}",
            tracker.name(),
            tracker.variables()
        ),
        404 => format!("{} has no issue {reference} (404)", tracker.name()),
        _ => format!("{} answered {status} for {reference}", tracker.name()),
    }
}

/// Base64 for HTTP Basic authentication.
fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn number_in(key: &str) -> u64 {
    key.rsplit(['-', '#'])
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// emdash's Linear `toIssueData`, from `issue` (the GraphQL `issue`
/// object), with the labels.
fn linear_issue(issue: &Value, reference: &IssueRef) -> Result<Fetched, String> {
    if !issue.is_object() {
        return Err(format!("Linear has no issue {}", reference.key));
    }
    let key = issue["identifier"]
        .as_str()
        .unwrap_or(&reference.key)
        .to_owned();
    Ok(Fetched {
        tracker: Tracker::Linear,
        number: number_in(&key),
        title: text(&issue["title"]),
        body: text(&issue["description"]),
        url: text(&issue["url"]),
        labels: issue["labels"]["nodes"]
            .as_array()
            .map(|nodes| nodes.iter().map(|n| text(&n["name"])).collect())
            .unwrap_or_default(),
        key,
    })
}

/// emdash's Jira `toIssueData`: the URL is `<site>/browse/<key>`; the
/// description (ADF in REST v3) is rendered as Markdown.
fn jira_issue(issue: &Value, site: &str, reference: &IssueRef) -> Fetched {
    let key = issue["key"].as_str().unwrap_or(&reference.key).to_owned();
    let description = &issue["fields"]["description"];
    Fetched {
        tracker: Tracker::Jira,
        number: number_in(&key),
        title: text(&issue["fields"]["summary"]),
        body: match description {
            Value::String(s) => s.clone(),
            other => crate::adf::to_markdown(other),
        },
        url: format!("{}/browse/{key}", site.trim_end_matches('/')),
        labels: issue["fields"]["labels"]
            .as_array()
            .map(|labels| labels.iter().map(text).collect())
            .unwrap_or_default(),
        key,
    }
}

/// emdash's GitLab `toIssueData`, keyed `group/project#iid`.
fn gitlab_issue(issue: &Value, reference: &IssueRef) -> Fetched {
    let iid = issue["iid"].as_u64().or(reference.iid).unwrap_or(0);
    Fetched {
        tracker: Tracker::GitLab,
        key: format!("{}#{iid}", reference.key),
        number: iid,
        title: text(&issue["title"]),
        body: text(&issue["description"]),
        url: text(&issue["web_url"]),
        labels: issue["labels"]
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .map(|l| match l {
                        Value::String(s) => s.clone(),
                        other => text(&other["name"]),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Percent-encode a GitLab project path for `/projects/:id`.
fn encode_path(path: &str) -> String {
    branchyard_client::http::encode(path)
}

/// The object a gateway tool returned for the issue: the API's own answer,
/// whether whole (`{"data": {"issue": …}}`), partly unwrapped
/// (`{"issue": …}`) or the issue itself.
fn unwrap_issue(value: Value) -> Value {
    let value = match value.get("data") {
        Some(data) if data.is_object() => data.clone(),
        _ => value,
    };
    match value.get("issue") {
        Some(issue) if issue.is_object() => issue.clone(),
        _ => value,
    }
}

/// Fetch the issue `reference` names.
pub fn fetch(reference: &IssueRef, sources: &Sources<'_>) -> Result<Fetched, String> {
    let tracker = reference.tracker;
    let settings = sources.settings(tracker);
    let gateway_tool = settings.and_then(|s| s.gateway_tool.as_deref());
    let configured_url = settings.and_then(|s| s.url.clone());
    let display = reference.display();
    let have_token = match tracker {
        Tracker::Linear => {
            sources.var("LINEAR_API_KEY").is_some() || sources.var("LINEAR_ACCESS_TOKEN").is_some()
        }
        Tracker::Jira => {
            sources.var("JIRA_EMAIL").is_some() && sources.var("JIRA_API_TOKEN").is_some()
        }
        Tracker::GitLab => sources.var("GITLAB_TOKEN").is_some(),
    };
    let site = |var: &str, default: Option<&str>| -> Result<String, String> {
        reference
            .site
            .clone()
            .or(configured_url.clone())
            .or_else(|| sources.var(var))
            .or(default.map(str::to_owned))
            .map(|s| s.trim_end_matches('/').to_owned())
            .ok_or_else(|| {
                format!(
                    "which {} site? Give the issue's URL, set [trackers.{}] url in \
                     branchyard.toml, or {var}",
                    tracker.name(),
                    tracker.id()
                )
            })
    };
    if !have_token {
        let (Some(tool), Some(call)) = (gateway_tool, sources.gateway) else {
            return Err(format!(
                "--issue {display} needs {} in the environment, or [trackers.{}] gateway_tool \
                 with a connector gateway (docs/pull-requests.md#other-trackers)",
                tracker.variables(),
                tracker.id()
            ));
        };
        let arguments = match tracker {
            Tracker::Linear => json!({ "id": reference.key }),
            Tracker::Jira => json!({ "issueIdOrKey": reference.key }),
            Tracker::GitLab => json!({ "id": reference.key, "issue_iid": reference.iid }),
        };
        let issue = unwrap_issue(call(tool, arguments)?);
        return match tracker {
            Tracker::Linear => linear_issue(&issue, reference),
            Tracker::Jira => {
                let site = site("JIRA_URL", None).unwrap_or_default();
                Ok(jira_issue(&issue, &site, reference))
            }
            Tracker::GitLab => Ok(gitlab_issue(&issue, reference)),
        };
    }
    match tracker {
        Tracker::Linear => {
            let url = configured_url
                .or_else(|| sources.var("LINEAR_API_URL"))
                .unwrap_or_else(|| LINEAR_API.to_owned());
            // A personal API key goes as it is; an OAuth token as Bearer.
            let auth = match (
                sources.var("LINEAR_API_KEY"),
                sources.var("LINEAR_ACCESS_TOKEN"),
            ) {
                (Some(key), _) => key,
                (None, Some(token)) => format!("Bearer {token}"),
                (None, None) => unreachable!("checked above"),
            };
            let body = json!({ "query": LINEAR_ISSUE_QUERY, "variables": { "id": reference.key } });
            let body = serde_json::to_vec(&body).unwrap_or_default();
            let (status, bytes, _) = request(
                "POST",
                &url,
                &[
                    ("Authorization", auth),
                    ("Content-Type", "application/json".into()),
                    ("Accept", "application/json".into()),
                ],
                Some(&body),
            )?;
            let answer = json_of(&bytes, "Linear");
            if status != 200 {
                // GraphQL errors come with 400 and a message worth showing.
                if let Ok(Some(message)) = answer
                    .as_ref()
                    .map(|v| v["errors"][0]["message"].as_str().map(str::to_owned))
                {
                    if message.to_ascii_lowercase().contains("not found") {
                        return Err(format!("Linear has no issue {display}"));
                    }
                    if status == 400 {
                        return Err(format!("Linear refused the query for {display}: {message}"));
                    }
                }
                return Err(refused(tracker, status, &display));
            }
            let answer = answer?;
            if let Some(message) = answer["errors"][0]["message"].as_str() {
                if message.to_ascii_lowercase().contains("not found") {
                    return Err(format!("Linear has no issue {display}"));
                }
                return Err(format!("Linear: {message}"));
            }
            linear_issue(&answer["data"]["issue"], reference)
        }
        Tracker::Jira => {
            let site = site("JIRA_URL", None)?;
            let email = sources.var("JIRA_EMAIL").unwrap_or_default();
            let token = sources.var("JIRA_API_TOKEN").unwrap_or_default();
            let url = format!(
                "{site}/rest/api/3/issue/{}?fields=summary,description,labels",
                reference.key
            );
            let (status, bytes, _) = request(
                "GET",
                &url,
                &[
                    (
                        "Authorization",
                        format!("Basic {}", base64(format!("{email}:{token}").as_bytes())),
                    ),
                    ("Accept", "application/json".into()),
                ],
                None,
            )?;
            if status != 200 {
                return Err(refused(tracker, status, &display));
            }
            Ok(jira_issue(&json_of(&bytes, "Jira")?, &site, reference))
        }
        Tracker::GitLab => {
            let site = site("GITLAB_URL", Some("https://gitlab.com"))?;
            let url = format!(
                "{site}/api/v4/projects/{}/issues/{}",
                encode_path(&reference.key),
                reference.iid.unwrap_or(0)
            );
            let token = sources.var("GITLAB_TOKEN").unwrap_or_default();
            let (status, bytes, _) = request(
                "GET",
                &url,
                &[
                    ("PRIVATE-TOKEN", token),
                    ("Accept", "application/json".into()),
                ],
                None,
            )?;
            if status != 200 {
                return Err(refused(tracker, status, &display));
            }
            Ok(gitlab_issue(&json_of(&bytes, "GitLab")?, reference))
        }
    }
}

/// Call `tool` on the MCP gateway at `url` with `token` (Streamable HTTP:
/// initialize, then `tools/call`), returning the tool's JSON result: its
/// `structuredContent`, else its first text content parsed as JSON.
pub fn gateway_call(url: &str, token: &str, tool: &str, arguments: Value) -> Result<Value, String> {
    let mut session: Option<String> = None;
    let mut call =
        |id: Option<u64>, method: &str, params: Value| -> Result<Option<Value>, String> {
            let mut message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
            if let Some(id) = id {
                message["id"] = json!(id);
            }
            let body = serde_json::to_vec(&message).unwrap_or_default();
            let mut headers = vec![
                ("Authorization", format!("Bearer {token}")),
                ("Content-Type", "application/json".to_owned()),
                ("Accept", "application/json, text/event-stream".to_owned()),
            ];
            if let Some(session) = &session {
                headers.push(("Mcp-Session-Id", session.clone()));
            }
            let (status, bytes, response_headers) = request("POST", url, &headers, Some(&body))?;
            if let Some((_, id)) = response_headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("mcp-session-id"))
            {
                session = Some(id.clone());
            }
            if !(200..300).contains(&status) {
                return Err(format!(
                    "the connector gateway answered {status} to {method}{}",
                    match status {
                        401 | 403 =>
                            " (is the gateway running with this yard's keys? by gateway status)",
                        _ => "",
                    }
                ));
            }
            if id.is_none() {
                return Ok(None);
            }
            let text = String::from_utf8_lossy(&bytes);
            // A JSON answer, or an event stream whose data lines carry it.
            let answer = match serde_json::from_str::<Value>(text.trim()) {
                Ok(value) => value,
                Err(_) => text
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
                    .find(|v| v["id"] == json!(id))
                    .ok_or_else(|| {
                        format!("the connector gateway's answer to {method} was not JSON")
                    })?,
            };
            if let Some(error) = answer.get("error") {
                return Err(format!(
                    "the connector gateway refused {method}: {}",
                    error["message"].as_str().unwrap_or("an error")
                ));
            }
            Ok(Some(answer["result"].clone()))
        };
    call(
        Some(1),
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "by", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;
    call(None, "notifications/initialized", json!({}))?;
    let result = call(
        Some(2),
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )?
    .unwrap_or_default();
    let content_text = result["content"]
        .as_array()
        .and_then(|items| items.iter().find_map(|c| c["text"].as_str()))
        .unwrap_or("");
    if result["isError"].as_bool() == Some(true) {
        return Err(format!(
            "the connector gateway's {tool} failed: {content_text}"
        ));
    }
    match result.get("structuredContent") {
        Some(value) if !value.is_null() => Ok(value.clone()),
        _ => serde_json::from_str(content_text)
            .map_err(|_| format!("the connector gateway's {tool} gave no JSON result")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse_by_prefix_and_by_url() {
        let r = |t: &str| parse(t).unwrap();
        let linear = r("linear:eng-123").unwrap();
        assert_eq!(
            (linear.tracker, linear.key.as_str()),
            (Tracker::Linear, "ENG-123")
        );
        assert_eq!(r("jira:PROJ-7").unwrap().tracker, Tracker::Jira);
        let gitlab = r("gitlab:acme/sub/widgets#12").unwrap();
        assert_eq!(gitlab.display(), "acme/sub/widgets#12");
        let url = r("https://linear.app/acme/issue/ENG-9/fix-the-parser").unwrap();
        assert_eq!((url.tracker, url.key.as_str()), (Tracker::Linear, "ENG-9"));
        let jira = r("https://acme.atlassian.net/browse/PROJ-7?x=1").unwrap();
        assert_eq!(jira.site.as_deref(), Some("https://acme.atlassian.net"));
        let gl = r("https://gitlab.example.com/acme/widgets/-/issues/12").unwrap();
        assert_eq!(
            (gl.key.as_str(), gl.iid, gl.site.as_deref()),
            ("acme/widgets", Some(12), Some("https://gitlab.example.com"))
        );
        for github in ["42", "#42", "https://github.com/acme/widgets/issues/42"] {
            assert_eq!(r(github), None, "{github}");
        }
        for bad in [
            "linear:123",
            "jira:",
            "gitlab:widgets#1",
            "gitlab:a/../b#1",
            "https://example.com/x",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    /// The vendored sources still have the shape this port follows.
    #[test]
    fn emdashs_sources_are_the_ones_ported() {
        let queries = include_str!(
            "../../../vendor/emdash/packages/plugins/src/issues/impl/linear/queries.ts"
        );
        for field in [
            "identifier",
            "branchName",
            "state { name type color }",
            "updatedAt",
        ] {
            assert!(
                queries.contains(field),
                "Linear's summary fields lost {field}"
            );
            assert!(LINEAR_ISSUE_QUERY.contains(field), "the query lost {field}");
        }
        let jira =
            include_str!("../../../vendor/emdash/packages/plugins/src/issues/impl/jira/mapper.ts");
        assert!(jira.contains("url: `${base}/browse/${issue.key}`"));
        let gitlab = include_str!(
            "../../../vendor/emdash/packages/plugins/src/issues/impl/gitlab/mapper.ts"
        );
        assert!(gitlab.contains("url: issue.web_url"));
    }

    #[test]
    fn basic_auth_is_base64() {
        assert_eq!(
            base64(b"ada@example.com:t0k"),
            "YWRhQGV4YW1wbGUuY29tOnQwaw=="
        );
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(encode_path("acme/sub widgets"), "acme%2Fsub%20widgets");
    }

    /// What a mock tracker saw of one request.
    struct Seen {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// A loopback tracker that answers the next connections with `replies`
    /// in order (one request each, read through the wire codec), and the
    /// requests it saw.
    fn tracker(replies: Vec<Vec<u8>>) -> (String, std::sync::mpsc::Receiver<Seen>) {
        use std::io::{BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, seen) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut out = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let head = wire::read_request_head(&mut reader, 64 * 1024)
                    .unwrap()
                    .unwrap();
                let framing = wire::request_framing(&head.headers).unwrap();
                let body = wire::read_body(&mut reader, framing, 1 << 20).unwrap();
                let _ = sender.send(Seen {
                    method: head.method,
                    target: head.target,
                    headers: head.headers,
                    body,
                });
                let _ = out.write_all(&reply);
            }
        });
        (url, seen)
    }

    /// A response with `body` sent chunked, in two pieces.
    fn chunked(status: &str, headers: &str, body: &str) -> Vec<u8> {
        let (a, b) = body.split_at(body.len() / 2);
        format!(
            "HTTP/1.1 {status}\r\n{headers}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
             {:x}\r\n{a}\r\n{:x};part=2\r\n{b}\r\n0\r\n\r\n",
            a.len(),
            b.len()
        )
        .into_bytes()
    }

    fn plain(status: &str, headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn linear(key: &str) -> IssueRef {
        parse(&format!("linear:{key}")).unwrap().unwrap()
    }

    fn with_env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        move |name| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn a_linear_issue_comes_back_through_a_chunked_answer() {
        let answer = json!({"data": {"issue": {
            "identifier": "ENG-5", "title": "Fix the parser", "description": "It breaks.",
            "url": "https://linear.app/acme/issue/ENG-5", "labels": {"nodes": [{"name": "bug"}]}
        }}})
        .to_string();
        let (url, seen) = tracker(vec![chunked(
            "200 OK",
            "Content-Type: application/json\r\n",
            &answer,
        )]);
        let env = with_env(&[("LINEAR_API_KEY", "lin_key"), ("LINEAR_API_URL", &url)]);
        let config = Trackers::default();
        let sources = Sources {
            env: &env,
            config: &config,
            gateway: None,
        };
        let issue = fetch(&linear("ENG-5"), &sources).unwrap();
        assert_eq!(
            (
                issue.key.as_str(),
                issue.title.as_str(),
                issue.labels.as_slice()
            ),
            ("ENG-5", "Fix the parser", &["bug".to_owned()][..])
        );
        let seen = seen.recv().unwrap();
        assert_eq!((seen.method.as_str(), seen.target.as_str()), ("POST", "/"));
        assert_eq!(
            wire::header(&seen.headers, "authorization"),
            Some("lin_key")
        );
        let query: Value = serde_json::from_slice(&seen.body).unwrap();
        assert_eq!(query["variables"]["id"], "ENG-5");
    }

    #[test]
    fn an_http_error_body_is_shown_and_a_bare_status_is_explained() {
        let (url, _) = tracker(vec![
            plain(
                "400 Bad Request",
                "Content-Type: application/json\r\n",
                r#"{"errors":[{"message":"Variable id is invalid"}]}"#,
            ),
            plain(
                "200 OK",
                "",
                r#"{"errors":[{"message":"Entity not found: Issue"}]}"#,
            ),
            plain("403 Forbidden", "", "nope"),
            plain("502 Bad Gateway", "", "<html>bad gateway</html>"),
        ]);
        let env = with_env(&[("LINEAR_API_KEY", "k"), ("LINEAR_API_URL", &url)]);
        let config = Trackers::default();
        let sources = Sources {
            env: &env,
            config: &config,
            gateway: None,
        };
        let r = linear("ENG-9");
        assert_eq!(
            fetch(&r, &sources).unwrap_err(),
            "Linear refused the query for ENG-9: Variable id is invalid"
        );
        assert_eq!(
            fetch(&r, &sources).unwrap_err(),
            "Linear has no issue ENG-9"
        );
        let denied = fetch(&r, &sources).unwrap_err();
        assert!(
            denied.contains("refused the credentials") && denied.contains("(403)"),
            "{denied}"
        );
        assert_eq!(
            fetch(&r, &sources).unwrap_err(),
            "Linear answered 502 for ENG-9"
        );
    }

    #[test]
    fn a_tracker_answer_that_is_not_framed_is_an_error_not_a_guess() {
        let (url, _) = tracker(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Length: 9\r\n\r\n{\"a\":1}".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n{}\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n{\"data\":".to_vec(),
        ]);
        let env = with_env(&[("LINEAR_API_KEY", "k"), ("LINEAR_API_URL", &url)]);
        let config = Trackers::default();
        let sources = Sources {
            env: &env,
            config: &config,
            gateway: None,
        };
        for _ in 0..3 {
            let error = fetch(&linear("ENG-1"), &sources).unwrap_err();
            assert!(error.contains(&url[7..]), "{error}");
        }
    }

    #[test]
    fn with_no_token_the_issue_comes_through_the_connector_gateway() {
        let result = json!({"structuredContent": {"issue": {
            "identifier": "ENG-7", "title": "Via the gateway", "description": "",
            "url": "https://linear.app/acme/issue/ENG-7", "labels": {"nodes": []}
        }}});
        let rpc = |id: u64, result: &Value| {
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
        };
        let (url, seen) = tracker(vec![
            plain(
                "200 OK",
                "Content-Type: application/json\r\nMcp-Session-Id: sess-1\r\n",
                &rpc(1, &json!({"protocolVersion": "2025-06-18"})),
            ),
            plain("202 Accepted", "", ""),
            chunked(
                "200 OK",
                "Content-Type: application/json\r\n",
                &rpc(2, &result),
            ),
        ]);
        let env = with_env(&[]);
        let mut config = Trackers::default();
        config.linear = Some(branchyard_setup::config::TrackerConfig {
            url: None,
            gateway_tool: Some("linear__get_issue".into()),
        });
        let call = |tool: &str, arguments: Value| gateway_call(&url, "gw-token", tool, arguments);
        let sources = Sources {
            env: &env,
            config: &config,
            gateway: Some(&call),
        };
        let issue = fetch(&linear("ENG-7"), &sources).unwrap();
        assert_eq!(
            (issue.key.as_str(), issue.title.as_str()),
            ("ENG-7", "Via the gateway")
        );
        let calls: Vec<Seen> = seen.try_iter().collect();
        assert_eq!(calls.len(), 3);
        for seen in &calls {
            assert_eq!(
                wire::header(&seen.headers, "authorization"),
                Some("Bearer gw-token")
            );
        }
        assert_eq!(wire::header(&calls[0].headers, "mcp-session-id"), None);
        assert_eq!(
            wire::header(&calls[2].headers, "mcp-session-id"),
            Some("sess-1")
        );
        let tools_call: Value = serde_json::from_slice(&calls[2].body).unwrap();
        assert_eq!(tools_call["params"]["name"], "linear__get_issue");
    }

    #[test]
    fn a_refusing_connector_gateway_says_what_to_check() {
        let (url, _) = tracker(vec![plain(
            "401 Unauthorized",
            "",
            r#"{"error":"bad token"}"#,
        )]);
        let error = gateway_call(&url, "t", "linear__get_issue", json!({})).unwrap_err();
        assert!(
            error.contains("answered 401 to initialize") && error.contains("by gateway status"),
            "{error}"
        );
    }
}
