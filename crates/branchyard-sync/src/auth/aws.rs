//! AWS credentials, looked for where the AWS SDKs look: the variables
//! (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`),
//! the shared credentials file (`AWS_SHARED_CREDENTIALS_FILE`, else
//! `~/.aws/credentials`, profile `AWS_PROFILE` or `default`), a container's
//! credentials endpoint (`AWS_CONTAINER_CREDENTIALS_FULL_URI` or
//! `..._RELATIVE_URI`), and the instance metadata service (IMDSv2;
//! `AWS_EC2_METADATA_SERVICE_ENDPOINT` names another). Temporary
//! credentials are cached until five minutes before they run out.

use branchyard_support::LockExt as _;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::auth::sigv4::Credentials;
use crate::error::{Error, Result};
use crate::http::{send, Request, Url};

pub struct AwsCredentials {
    cached: Mutex<Option<Credentials>>,
    profile: Option<String>,
}

const REFRESH_EARLY_MS: u64 = 5 * 60 * 1000;

impl AwsCredentials {
    pub fn new(profile: Option<String>) -> AwsCredentials {
        AwsCredentials {
            cached: Mutex::new(None),
            profile,
        }
    }

    /// Credentials good for at least a few more minutes at `now_ms`.
    pub fn get(&self, now_ms: u64) -> Result<Credentials> {
        let mut cached = self.cached.lock_recovering("cached");
        if let Some(c) = cached.as_ref() {
            if c.expires_ms.is_none_or(|t| t > now_ms + REFRESH_EARLY_MS) {
                return Ok(c.clone());
            }
        }
        let fresh = self.find()?;
        *cached = Some(fresh.clone());
        Ok(fresh)
    }

    fn find(&self) -> Result<Credentials> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if let (Some(access_key), Some(secret_key)) =
            (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY"))
        {
            return Ok(Credentials {
                access_key,
                secret_key,
                session_token: var("AWS_SESSION_TOKEN"),
                expires_ms: None,
            });
        }
        let profile = self
            .profile
            .clone()
            .or_else(|| var("AWS_PROFILE"))
            .unwrap_or_else(|| "default".into());
        let file = var("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".aws/credentials")));
        if let Some(text) = file.and_then(|f| std::fs::read_to_string(f).ok()) {
            if let Some(c) = from_profile(&text, &profile) {
                return Ok(c);
            }
        }
        if let Some(url) = var("AWS_CONTAINER_CREDENTIALS_FULL_URI").or_else(|| {
            var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                .map(|p| format!("http://169.254.170.2{p}"))
        }) {
            let mut request = Request::new("GET", Url::parse(&url)?);
            if let Some(token) = var("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
                request = request.header("Authorization", token);
            }
            let response = send(&request)?;
            if !response.ok() {
                return Err(response.error("container credentials"));
            }
            return from_json(&response.json()?);
        }
        if var("AWS_EC2_METADATA_DISABLED").as_deref() == Some("true") {
            return Err(no_credentials());
        }
        let base = var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
            .unwrap_or_else(|| "http://169.254.169.254".into());
        imds(base.trim_end_matches('/')).map_err(|e| no_credentials().context(e))
    }
}

fn no_credentials() -> Error {
    Error::config(
        "no AWS credentials: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, a profile in \
         ~/.aws/credentials, or run with an instance or container role",
    )
}

fn from_profile(text: &str, profile: &str) -> Option<Credentials> {
    let mut in_profile = false;
    let (mut access, mut secret, mut token) = (None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_matches(['[', ']']).trim();
            in_profile = name == profile || name == format!("profile {profile}");
            continue;
        }
        if !in_profile {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().to_owned();
            match k.trim() {
                "aws_access_key_id" => access = Some(v),
                "aws_secret_access_key" => secret = Some(v),
                "aws_session_token" => token = Some(v),
                _ => {}
            }
        }
    }
    Some(Credentials {
        access_key: access?,
        secret_key: secret?,
        session_token: token,
        expires_ms: None,
    })
}

fn from_json(value: &serde_json::Value) -> Result<Credentials> {
    let text = |k: &str| value.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    Ok(Credentials {
        access_key: text("AccessKeyId").ok_or_else(|| Error::refused("no AccessKeyId"))?,
        secret_key: text("SecretAccessKey").ok_or_else(|| Error::refused("no SecretAccessKey"))?,
        session_token: text("Token"),
        expires_ms: text("Expiration")
            .and_then(|t| branchyard_support::time::parse_rfc3339(&t).ok()),
    })
}

/// IMDSv2 at `base`: a session token, the role's name, then its
/// credentials.
pub fn imds(base: &str) -> Result<Credentials> {
    let token = send(
        &Request::new("PUT", Url::parse(&format!("{base}/latest/api/token"))?)
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .body(Vec::new()),
    )?;
    if !token.ok() {
        return Err(token.error("instance metadata token"));
    }
    let token = token.text();
    let path = format!("{base}/latest/meta-data/iam/security-credentials/");
    let role = send(
        &Request::new("GET", Url::parse(&path)?).header("X-aws-ec2-metadata-token", token.clone()),
    )?;
    if !role.ok() {
        return Err(role.error("instance role"));
    }
    let role = role.text().lines().next().unwrap_or("").trim().to_owned();
    let creds = send(
        &Request::new("GET", Url::parse(&format!("{path}{role}"))?)
            .header("X-aws-ec2-metadata-token", token),
    )?;
    if !creds.ok() {
        return Err(creds.error("instance credentials"));
    }
    from_json(&creds.json()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_read() {
        let text = "[default]\naws_access_key_id = A\naws_secret_access_key = S\n\n[profile work]\naws_access_key_id=W\naws_secret_access_key=X\naws_session_token=T\n";
        let c = from_profile(text, "default").unwrap();
        assert_eq!((c.access_key.as_str(), c.secret_key.as_str()), ("A", "S"));
        let c = from_profile(text, "work").unwrap();
        assert_eq!(c.session_token.as_deref(), Some("T"));
        assert!(from_profile(text, "missing").is_none());
    }
}
