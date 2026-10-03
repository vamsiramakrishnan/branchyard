//! Requests to the cloud APIs, through the blocking client the remote SDK
//! already has (`branchyard_client::http`): one connection per request,
//! TLS through rustls with the Mozilla roots, plus the certificates in
//! `BRANCHYARD_SYNC_CA_FILE` for a private endpoint. Responses are read
//! whole. Status codes map to the sync error kinds here, once.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use branchyard_client::http as wire;

use crate::error::{Error, Kind, Result};

/// Extra CA certificates for `https` endpoints (MinIO or Azurite with a
/// private certificate).
pub const ENV_CA_FILE: &str = "BRANCHYARD_SYNC_CA_FILE";

/// The largest response body read.
const MAX_BODY: usize = 8 << 30;

/// A URL split into what a request needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// The path, already percent-encoded, starting with `/`.
    pub path: String,
    /// The query without `?`, already encoded; empty for none.
    pub query: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url> {
        let (base, query) = match url.split_once('?') {
            Some((b, q)) => (b, q.to_owned()),
            None => (url, String::new()),
        };
        let (tls, rest) = if let Some(rest) = base.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = base.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(Error::config(format!("{url:?} is not an http(s) URL")));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let endpoint = wire::Endpoint::parse(&format!(
            "{}://{authority}",
            if tls { "https" } else { "http" }
        ))
        .map_err(Error::config)?;
        Ok(Url {
            tls,
            host: endpoint.host,
            port: endpoint.port,
            path: path.to_owned(),
            query,
        })
    }

    /// The `Host` header the client sends, which signatures cover.
    pub fn host_header(&self) -> String {
        let host = match self.host.contains(':') {
            true => format!("[{}]", self.host),
            false => self.host.clone(),
        };
        match (self.tls, self.port) {
            (true, 443) | (false, 80) => host,
            _ => format!("{host}:{}", self.port),
        }
    }

    /// `scheme://host[:port]`.
    pub fn origin(&self) -> String {
        format!(
            "{}://{}",
            if self.tls { "https" } else { "http" },
            self.host_header()
        )
    }

    pub fn target(&self) -> String {
        match self.query.is_empty() {
            true => self.path.clone(),
            false => format!("{}?{}", self.path, self.query),
        }
    }

    pub fn with_path(&self, path: &str, query: &str) -> Url {
        Url {
            path: path.to_owned(),
            query: query.to_owned(),
            ..self.clone()
        }
    }
}

/// One request.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl Request {
    pub fn new(method: &str, url: Url) -> Request {
        Request {
            method: method.to_owned(),
            url,
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    pub fn body(mut self, body: Vec<u8>) -> Request {
        self.body = Some(body);
        self
    }

    pub fn find(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One response, read whole.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The error a non-2xx status means, with `what` for context: `404`
    /// not found, `409`/`412` a failed precondition, `408`/`429`/`5xx`
    /// transient, the rest refused.
    pub fn error(&self, what: &str) -> Error {
        let detail: String = self.text().chars().take(400).collect();
        let message = format!("{what}: HTTP {} {}", self.status, detail.trim());
        let kind = match self.status {
            404 => Kind::NotFound,
            409 | 412 => Kind::Precondition,
            408 | 429 | 500..=599 => Kind::Transient,
            _ => Kind::Refused,
        };
        Error::new(kind, message)
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(Into::into)
    }
}

fn tls() -> Result<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<std::result::Result<Arc<rustls::ClientConfig>, String>> =
        OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let ca = std::env::var_os(ENV_CA_FILE).filter(|v| !v.is_empty());
            wire::tls_config(ca.as_deref().map(std::path::Path::new))
        })
        .clone()
        .map_err(Error::config)
}

/// Send `request` and read the whole response.
pub fn send(request: &Request) -> Result<Response> {
    let endpoint = wire::Endpoint {
        tls: request.url.tls,
        host: request.url.host.clone(),
        port: request.url.port,
        prefix: String::new(),
        unix: None,
    };
    let config = match request.url.tls {
        true => Some(tls()?),
        false => None,
    };
    let stream = wire::connect(&endpoint, config.as_ref(), Duration::from_secs(120))
        .map_err(|e| Error::transient(format!("{}: {e}", request.url.origin())))?;
    let headers: Vec<(&str, String)> = request
        .headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.clone()))
        .collect();
    let target = request.url.target();
    let response = wire::send(
        stream,
        &endpoint,
        &wire::Request {
            method: &request.method,
            target: &target,
            headers: &headers,
            body: request.body.as_deref(),
        },
    )
    .map_err(|e| Error::transient(format!("{} {}: {e}", request.method, request.url.origin())))?;
    let status = response.status;
    let headers = response.headers.clone();
    let body = response
        .read_body(MAX_BODY)
        .map_err(|e| Error::transient(format!("reading {}: {e}", request.url.origin())))?;
    Ok(Response {
        status,
        headers,
        body,
    })
}
