//! The one strict `http(s)` URL.

use crate::error::WireError;

/// An `http://` or `https://` URL, split into what a request needs.
///
/// Strict where the old copies disagreed: only the lower-case schemes, no
/// user information, no fragment, a non-empty host with no stray `:`, a
/// numeric port, and no whitespace, control bytes or non-ASCII anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    /// `https` (true) or `http` (false).
    pub tls: bool,
    /// The host, without the brackets of an IPv6 literal.
    pub host: String,
    /// The port, the scheme's default when none is written.
    pub port: u16,
    /// The path as written: empty, or starting with `/`.
    pub path: String,
    /// The query without its `?`, as written; `None` when there is none.
    pub query: Option<String>,
}

impl HttpUrl {
    /// Parse `url`, refusing anything outside the strict form above.
    pub fn parse(url: &str) -> Result<HttpUrl, WireError> {
        let bad = |why: &str| WireError::BadUrl(format!("{url:?} {why}"));
        if url.bytes().any(|b| b <= b' ' || b >= 0x7f) {
            return Err(bad("has whitespace, control or non-ASCII characters"));
        }
        let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(bad("is not an http:// or https:// URL"));
        };
        if rest.contains('#') {
            return Err(bad("must not have a fragment"));
        }
        let (rest, query) = match rest.split_once('?') {
            Some((rest, query)) => (rest, Some(query.to_owned())),
            None => (rest, None),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() || authority.contains('@') || authority.contains('\\') {
            return Err(bad("has no usable host"));
        }
        let default = if tls { 443 } else { 80 };
        let port = |text: &str| -> Result<u16, WireError> {
            match text.bytes().all(|b| b.is_ascii_digit()) {
                true => text.parse().map_err(|_| bad("has an invalid port")),
                false => Err(bad("has an invalid port")),
            }
        };
        let (host, port) = match authority.strip_prefix('[') {
            Some(v6) => {
                let (host, after) = v6
                    .split_once(']')
                    .ok_or_else(|| bad("has an unterminated IPv6 address"))?;
                if host.is_empty()
                    || !host
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() || b":.%".contains(&b))
                {
                    return Err(bad("has a malformed IPv6 address"));
                }
                let port = match after.strip_prefix(':') {
                    Some(text) => port(text)?,
                    None if after.is_empty() => default,
                    None => return Err(bad("has a malformed host")),
                };
                (host, port)
            }
            None => {
                let (host, port) = match authority.split_once(':') {
                    Some((host, text)) => (host, port(text)?),
                    None => (authority, default),
                };
                if host.is_empty() || host.contains([':', '[', ']']) {
                    return Err(bad("has no usable host"));
                }
                (host, port)
            }
        };
        Ok(HttpUrl {
            tls,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
            query,
        })
    }

    /// The `Host` header value: the port only when it is not the scheme's
    /// default.
    pub fn host_header(&self) -> String {
        host_header(self.tls, &self.host, self.port)
    }

    /// `scheme://host[:port]`.
    pub fn origin(&self) -> String {
        format!(
            "{}://{}",
            if self.tls { "https" } else { "http" },
            self.host_header()
        )
    }

    /// The request target: the path (`/` for none) and the query.
    pub fn target(&self) -> String {
        let path = match self.path.is_empty() {
            true => "/",
            false => &self.path,
        };
        match &self.query {
            Some(query) => format!("{path}?{query}"),
            None => path.to_owned(),
        }
    }

    /// The path with trailing slashes removed, for a base URL that
    /// requests are made under.
    pub fn prefix(&self) -> &str {
        self.path.trim_end_matches('/')
    }

    /// For a base URL: refuse a query (a base has none).
    pub fn without_query(self) -> Result<HttpUrl, WireError> {
        match self.query {
            None => Ok(self),
            Some(_) => Err(WireError::BadUrl(format!(
                "{} must be a base URL: a host and a path, no query",
                self.origin()
            ))),
        }
    }
}

/// The `Host` header value for `host` (an IPv6 literal without brackets)
/// and `port`: the port only when it is not the scheme's default.
pub fn host_header(tls: bool, host: &str, port: u16) -> String {
    let host = match host.contains(':') {
        true => format!("[{host}]"),
        false => host.to_owned(),
    };
    match (tls, port) {
        (true, 443) | (false, 80) => host,
        _ => format!("{host}:{port}"),
    }
}
