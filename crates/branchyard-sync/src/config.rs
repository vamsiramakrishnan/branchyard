//! `[sync]`: where tasks sync to and how. The same settings come from
//! `branchyard.toml` (for `by sync` and a local `by serve`) and from a
//! server's configuration file (`"sync": {...}`).
//!
//! | Key | Meaning | Default |
//! |---|---|---|
//! | `remote` | `gs://`, `s3://`, `az://`, `file:///`, or `git+https://`/`git+ssh://`/`git+file://` | required |
//! | `encrypt` | `none`, `passphrase` (from `BRANCHYARD_SYNC_PASSPHRASE` or `passphrase_file`), or a `kms://` URL | `none` |
//! | `algorithm` | `aes-256-gcm` or `chacha20-poly1305` for a new encrypted remote | `aes-256-gcm` |
//! | `interval` | how often the replicator looks for changes (`30s`, `5m`) | `1m` |
//! | `bandwidth` | bytes a second, up and down together (`10MB/s`, `512KiB/s`) | unlimited |
//! | `concurrency` | objects in flight at once | 8 |
//! | `retention` | delete a task this long after its last change (`90d`); holds win | keep |
//! | `grace` | how long an unreferenced object waits before collection | `24h` |
//! | `quota` | the tenant's stored bytes (`50GB`) | none |
//! | `device` | this machine's name in conflict branches and leases | the host name |

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use branchyard::services::Clock;
use serde::{Deserialize, Serialize};

use crate::engine::{Options, Remote, Settings};
use crate::error::{Error, Result};
use crate::kms::Passphrase;
use crate::outbox::{Outbox, OutboxJournal};
use crate::seal::{Algorithm, Encryption};

/// The variable holding a sync passphrase.
pub const ENV_PASSPHRASE: &str = "BRANCHYARD_SYNC_PASSPHRASE";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// `30s`, `5m`, `2h`, `7d`, `500ms`, or a number of seconds.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let t = text.trim();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: f64 = number
        .parse()
        .map_err(|_| Error::config(format!("{text:?} is not a duration such as 30s, 5m or 7d")))?;
    let ms = match unit.trim() {
        "ms" => n,
        "" | "s" => n * 1000.0,
        "m" => n * 60_000.0,
        "h" => n * 3_600_000.0,
        "d" => n * 86_400_000.0,
        _ => {
            return Err(Error::config(format!(
                "{text:?} is not a duration such as 30s, 5m or 7d"
            )))
        }
    };
    if !(ms.is_finite() && ms >= 0.0) {
        return Err(Error::config(format!("{text:?} is not a duration")));
    }
    Ok(Duration::from_millis(ms as u64))
}

/// `500`, `64KB`, `10MB`, `1GiB`, with an optional `/s`.
pub fn parse_bytes(text: &str) -> Result<u64> {
    let t = text.trim().trim_end_matches("/s").trim();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: f64 = number
        .parse()
        .map_err(|_| Error::config(format!("{text:?} is not a size such as 10MB or 1GiB")))?;
    let scale: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mib" => 1048576.0,
        "g" | "gb" => 1e9,
        "gib" => 1073741824.0,
        "t" | "tb" => 1e12,
        "tib" => 1099511627776.0,
        _ => {
            return Err(Error::config(format!(
                "{text:?} is not a size such as 10MB or 1GiB"
            )))
        }
    };
    let bytes = n * scale;
    if !(bytes.is_finite() && bytes >= 1.0) {
        return Err(Error::config(format!("{text:?} must be at least one byte")));
    }
    Ok(bytes as u64)
}

impl SyncConfig {
    /// Check every value without opening anything.
    pub fn check(&self) -> Result<()> {
        crate::store::Location::parse(&self.remote)?;
        match self.encrypt.as_deref() {
            None | Some("none") | Some("passphrase") => {}
            Some(url) if url.starts_with("kms://") => {}
            Some(other) => {
                return Err(Error::config(format!(
                    "encrypt = {other:?}: use \"none\", \"passphrase\" or a kms:// URL"
                )))
            }
        }
        if let Some(a) = &self.algorithm {
            Algorithm::parse(a)?;
        }
        for d in [&self.interval, &self.retention, &self.grace]
            .into_iter()
            .flatten()
        {
            parse_duration(d)?;
        }
        for b in [&self.bandwidth, &self.quota].into_iter().flatten() {
            parse_bytes(b)?;
        }
        if self.concurrency == Some(0) {
            return Err(Error::config("concurrency must be at least 1"));
        }
        Ok(())
    }

    pub fn interval(&self) -> Duration {
        self.interval
            .as_deref()
            .and_then(|i| parse_duration(i).ok())
            .unwrap_or(Duration::from_secs(60))
            .max(Duration::from_secs(1))
    }

    /// The encryption asked for, with its passphrase or KMS client.
    pub fn encryption(&self) -> Result<Encryption> {
        let algorithm = match &self.algorithm {
            Some(a) => Algorithm::parse(a)?,
            None => Algorithm::Aes256Gcm,
        };
        match self.encrypt.as_deref() {
            None | Some("none") => Ok(Encryption::None),
            Some("passphrase") => {
                let passphrase = match &self.passphrase_file {
                    Some(path) => std::fs::read_to_string(path)
                        .map_err(|e| {
                            Error::config(format!("passphrase_file {}: {e}", path.display()))
                        })?
                        .trim_end_matches(['\n', '\r'])
                        .to_owned(),
                    None => std::env::var(ENV_PASSPHRASE).map_err(|_| {
                        Error::config(format!(
                            "encrypt = \"passphrase\" needs {ENV_PASSPHRASE} or passphrase_file"
                        ))
                    })?,
                };
                Ok(Encryption::Envelope {
                    wrapper: Box::new(Passphrase::new(&passphrase)?),
                    algorithm,
                })
            }
            Some(url) => Ok(Encryption::Envelope {
                wrapper: crate::kms::from_url(url)?,
                algorithm,
            }),
        }
    }

    pub fn settings(&self, device: &str) -> Result<Settings> {
        let mut settings = Settings {
            device: device.to_owned(),
            ..Settings::default()
        };
        if let Some(c) = self.concurrency {
            settings.concurrency = c.max(1);
        }
        if let Some(r) = &self.retention {
            settings.retention = Some(parse_duration(r)?);
        }
        if let Some(g) = &self.grace {
            settings.grace = parse_duration(g)?;
        }
        crate::gc::check_grace(settings.grace, settings.writer_ttl)?;
        if let Some(q) = &self.quota {
            settings.quota_bytes = Some(parse_bytes(q)?);
        }
        Ok(settings)
    }

    /// Open the remote, with resumable uploads journaled in `outbox`.
    pub fn open(&self, outbox: &Arc<Outbox>, clock: Clock) -> Result<Remote> {
        self.check()?;
        let device = outbox.device(self.device.as_deref())?;
        let store = crate::store::open(&self.remote)?;
        Remote::open(
            store,
            Options {
                encryption: self.encryption()?,
                settings: self.settings(&device)?,
                clock,
                bandwidth: self.bandwidth.as_deref().map(parse_bytes).transpose()?,
                journal: Arc::new(OutboxJournal(outbox.clone())),
                ..Options::default()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86400)
        );
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("5y").is_err());
        assert_eq!(parse_bytes("10MB/s").unwrap(), 10_000_000);
        assert_eq!(parse_bytes("1MiB").unwrap(), 1_048_576);
        assert_eq!(parse_bytes("512").unwrap(), 512);
        assert!(parse_bytes("0").is_err());
        assert!(parse_bytes("lots").is_err());
    }

    #[test]
    fn configs_are_checked() {
        let mut c = SyncConfig {
            remote: "gs://bucket/p".into(),
            ..SyncConfig::default()
        };
        c.check().unwrap();
        c.encrypt = Some("rot13".into());
        assert!(c.check().is_err());
        c.encrypt = Some("kms://gcp/projects/p/locations/l/keyRings/r/cryptoKeys/k".into());
        c.check().unwrap();
        c.remote = "ftp://x".into();
        assert!(c.check().is_err());
        let c = SyncConfig {
            remote: "file:///tmp/x".into(),
            grace: Some("1m".into()),
            ..SyncConfig::default()
        };
        assert!(
            c.settings("d").is_err(),
            "a grace shorter than a writer's mark"
        );
    }
}
