//! Bounded HTTP reads of a branch-reserved service at its own provider.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use super::WorkspaceRuntime;
use crate::Error;

const MAX_BODY: usize = 1024 * 1024;
const MAX_ENVELOPE: usize = MAX_BODY * 2;

const RELAY: &str = r#"
import base64,http.client,json,sys
port,path,method=int(sys.argv[1]),sys.argv[2],sys.argv[3]
connection=http.client.HTTPConnection('127.0.0.1',port,timeout=5)
try:
    connection.request(method,path,headers={'Host':'127.0.0.1:'+str(port),'Connection':'close'})
    response=connection.getresponse()
    if 300<=response.status<400:
        raise ValueError('development-app redirects are not followed')
    body=response.read(1048577)
    if len(body)>1048576:
        raise ValueError('development-app response exceeds 1 MiB')
    allowed={'content-type','cache-control','etag','last-modified','content-encoding'}
    headers={key.lower():value for key,value in response.getheaders() if key.lower() in allowed}
    print(json.dumps({'status':response.status,'headers':headers,'body':base64.b64encode(body).decode('ascii')}))
finally:
    connection.close()
"#;

/// A branch-reserved port, obtained from a leased runtime rather than user input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PreviewPort(u16);

/// A response from a workspace service. Redirects and cookie/auth headers are absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewResponse {
    /// Upstream status code, excluding redirects.
    pub status: u16,
    /// Explicitly allowed response headers with lower-case names.
    pub headers: BTreeMap<String, String>,
    /// Bounded original response bytes.
    pub body: Vec<u8>,
}

#[derive(Deserialize)]
struct Response {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

fn path(value: &str) -> Result<(), Error> {
    let decoded = percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| Error::Unsupported("preview path is not valid UTF-8".into()))?;
    if value.len() > 8192
        || !decoded.starts_with('/')
        || decoded.starts_with("//")
        || decoded.contains(['\\', '\r', '\n', '\0', '#'])
        || decoded
            .split('?')
            .next()
            .is_some_and(|path| path.split('/').any(|part| matches!(part, "." | "..")))
    {
        return Err(Error::Unsupported(
            "preview needs a normalized relative app path".into(),
        ));
    }
    Ok(())
}

impl WorkspaceRuntime {
    /// The development port reserved for this branch by the engine.
    pub fn preview_port(&self) -> Result<PreviewPort, Error> {
        self.info.port.map(PreviewPort).ok_or_else(|| Error::Unsupported(
            "this branch has no reserved development port; run it with a workspace configuration first".into(),
        ))
    }

    /// Read a service on this runtime's reserved port through its actual provider.
    ///
    /// Only GET and HEAD are supported. The server must reauthorize each call
    /// and serve active HTML from a separate preview origin, never its control
    /// origin. There is no arbitrary destination URL, auth-header forwarding,
    /// redirect following, or control-host fallback for remote workspaces.
    pub fn preview(
        self,
        port: PreviewPort,
        method: &str,
        target: &str,
    ) -> Result<PreviewResponse, Error> {
        if self.info.port != Some(port.0) {
            return Err(Error::Denied(
                "the development port belongs to another workspace".into(),
            ));
        }
        if !matches!(method, "GET" | "HEAD") {
            return Err(Error::Unsupported(
                "development-app previews support only GET and HEAD".into(),
            ));
        }
        path(target)?;
        let output = self
            .exec(vec![
                "python3".into(),
                "-u".into(),
                "-c".into(),
                RELAY.into(),
                port.0.to_string(),
                target.into(),
                method.into(),
            ])?
            .output(MAX_ENVELOPE, Duration::from_secs(10))?;
        let response: Response = serde_json::from_slice(&output).map_err(|error| {
            Error::State(format!("invalid development-app relay response: {error}"))
        })?;
        if !(100..600).contains(&response.status) || (300..400).contains(&response.status) {
            return Err(Error::Unsupported(
                "development-app redirect or invalid status was refused".into(),
            ));
        }
        let allowed = [
            "content-type",
            "cache-control",
            "etag",
            "last-modified",
            "content-encoding",
        ];
        if response.headers.iter().any(|(name, value)| {
            !allowed.contains(&name.as_str()) || value.contains(['\r', '\n', '\0'])
        }) {
            return Err(Error::State(
                "development-app relay returned an unsafe header".into(),
            ));
        }
        let body = STANDARD.decode(response.body).map_err(|_| {
            Error::State("development-app relay returned invalid encoded bytes".into())
        })?;
        if body.len() > MAX_BODY {
            return Err(Error::Unsupported(
                "development-app response exceeds 1 MiB".into(),
            ));
        }
        Ok(PreviewResponse {
            status: response.status,
            headers: response.headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_on_the_registered_service() {
        for value in ["/", "/assets/app.js?v=1", "/search?q=a%20b"] {
            assert!(path(value).is_ok(), "{value}");
        }
        for value in [
            "http://evil/",
            "//evil/",
            "/../secret",
            "/%2e%2e/secret",
            "/a\\b",
            "/a%0d%0aHost:x",
            "/a#b",
            "/a/./b",
        ] {
            assert!(path(value).is_err(), "{value}");
        }
    }
}
