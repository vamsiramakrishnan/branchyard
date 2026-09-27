//! Server configuration: a JSON file, command-line flags, or both (flags
//! win). Relative paths in a file resolve against the file's directory.
//!
//! ```json
//! {
//!   "listen": "127.0.0.1:8421",
//!   "data_dir": "/var/lib/branchyard",
//!   "repos": { "app": "/srv/app" },
//!   "tokens": [{ "name": "ci", "token_file": "/etc/branchyard/ci.token" }],
//!   "tls": { "cert": "cert.pem", "key": "key.pem" },
//!   "max_body_bytes": 1048576,
//!   "max_running": 8,
//!   "shutdown_grace_seconds": 60,
//!   "harness_commands": { "codex": ["/opt/codex/bin/codex"] },
//!   "allow_client_commands": false
//! }
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8421";

/// A bearer token. Its secret never appears in `Debug` output or logs.
#[derive(Clone)]
pub struct Token {
    pub name: String,
    pub secret: String,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Token")
            .field("name", &self.name)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// A resolved configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// Name and path of each served repository.
    pub repos: Vec<(String, PathBuf)>,
    pub tokens: Vec<Token>,
    pub tls: Option<TlsFiles>,
    /// Serve plain HTTP on a non-loopback address. Only from the flag.
    pub insecure_bind: bool,
    pub max_body_bytes: usize,
    /// Operations running at once; more wait queued.
    pub max_running: usize,
    /// How long shutdown waits for running operations.
    pub shutdown_grace: Duration,
    /// Executable per harness, used when a request names none.
    pub harness_commands: BTreeMap<String, Vec<String>>,
    /// Accept a request's own `command`. Off by default: it lets any token
    /// holder choose what the server executes.
    pub allow_client_commands: bool,
    /// How often the feed looks in the repository's store for activity
    /// recorded by other processes.
    pub poll_interval: Duration,
    /// Log one line per request to stderr.
    pub log_requests: bool,
}

impl Config {
    /// Defaults for everything but repositories, tokens and data directory.
    pub fn new(data_dir: PathBuf) -> Config {
        Config {
            listen: DEFAULT_LISTEN.parse().expect("valid default address"),
            data_dir,
            repos: Vec::new(),
            tokens: Vec::new(),
            tls: None,
            insecure_bind: false,
            max_body_bytes: 1024 * 1024,
            max_running: 8,
            shutdown_grace: Duration::from_secs(60),
            harness_commands: BTreeMap::new(),
            allow_client_commands: false,
            poll_interval: Duration::from_millis(500),
            log_requests: true,
        }
    }

    /// Refuse what cannot work, and a plain-HTTP bind beyond loopback
    /// unless explicitly allowed. Returns a warning to print loudly when
    /// serving insecurely.
    pub fn validate(&self) -> Result<Option<String>, String> {
        if self.repos.is_empty() {
            return Err("no repositories to serve".into());
        }
        if self.tokens.is_empty() {
            return Err("no tokens configured; every request needs one".into());
        }
        for (name, _) in &self.repos {
            check_repo_name(name)?;
        }
        let mut names: Vec<&str> = self.repos.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        if let Some(pair) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("repository {} is configured twice", pair[0]));
        }
        for token in &self.tokens {
            if token.secret.len() < 16 {
                return Err(format!(
                    "token {} is shorter than 16 characters",
                    token.name
                ));
            }
        }
        if self.max_body_bytes < 1024 {
            return Err("max_body_bytes must be at least 1024".into());
        }
        if self.listen.ip().is_loopback() || self.tls.is_some() {
            return Ok(None);
        }
        if !self.insecure_bind {
            return Err(format!(
                "refusing to serve plain HTTP on {}, which is not a loopback address; \
                 configure TLS (--tls-cert and --tls-key) or pass --insecure-bind",
                self.listen
            ));
        }
        Ok(Some(format!(
            "WARNING: serving plain HTTP on {} with --insecure-bind. Bearer tokens, prompts \
             and code cross the network unencrypted; anyone on the path can read them and \
             replay the tokens.",
            self.listen
        )))
    }
}

/// Repository names are URL segments: lowercase `[a-z0-9._-]`, starting
/// with a letter or digit.
pub fn check_repo_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
        && name.starts_with(|c: char| c.is_ascii_alphanumeric());
    match ok {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a usable repository name: use lowercase letters, digits, '.', '_' \
             and '-', starting with a letter or digit"
        )),
    }
}

/// A repository name from a directory name, such as `My App` to `my-app`.
pub fn repo_name_for(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-').to_owned();
    if name.is_empty() {
        "repo".into()
    } else {
        name
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    repos: BTreeMap<String, PathBuf>,
    #[serde(default)]
    tokens: Vec<FileToken>,
    tls: Option<FileTls>,
    max_body_bytes: Option<usize>,
    max_running: Option<usize>,
    shutdown_grace_seconds: Option<f64>,
    #[serde(default)]
    harness_commands: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    allow_client_commands: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileToken {
    name: String,
    token: Option<String>,
    token_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTls {
    cert: PathBuf,
    key: PathBuf,
}

/// Settings from a file, before flags and defaults.
#[derive(Debug, Default)]
pub struct Partial {
    pub listen: Option<SocketAddr>,
    pub data_dir: Option<PathBuf>,
    pub repos: Vec<(String, PathBuf)>,
    pub tokens: Vec<Token>,
    pub tls: Option<TlsFiles>,
    pub max_body_bytes: Option<usize>,
    pub max_running: Option<usize>,
    pub shutdown_grace: Option<Duration>,
    pub harness_commands: BTreeMap<String, Vec<String>>,
    pub allow_client_commands: bool,
    /// Warnings to print, such as a world-readable token file.
    pub warnings: Vec<String>,
}

/// Read the first line of a token file, warning when others can read it.
pub fn read_token_file(path: &Path, warnings: &mut Vec<String>) -> Result<String, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
    let token = text.lines().next().unwrap_or("").trim().to_owned();
    if token.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    if let Some(warning) = readable_by_others(path) {
        warnings.push(warning);
    }
    Ok(token)
}

#[cfg(unix)]
fn readable_by_others(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path).ok()?.permissions().mode();
    (mode & 0o077 != 0).then(|| {
        format!(
            "{} holds a token and is readable by other users (mode {:o}); chmod 600 it",
            path.display(),
            mode & 0o777
        )
    })
}

#[cfg(not(unix))]
fn readable_by_others(_: &Path) -> Option<String> {
    None
}

pub fn load_file(path: &Path) -> Result<Partial, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("config {}: {e}", path.display()))?;
    let file: FileConfig =
        serde_json::from_str(&text).map_err(|e| format!("config {}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let resolve = |p: PathBuf| if p.is_absolute() { p } else { dir.join(p) };
    let mut warnings = Vec::new();
    let mut tokens = Vec::new();
    let mut inline = false;
    for token in file.tokens {
        let secret = match (token.token, token.token_file) {
            (Some(secret), None) => {
                inline = true;
                secret
            }
            (None, Some(file)) => read_token_file(&resolve(file), &mut warnings)?,
            _ => {
                return Err(format!(
                    "config {}: token {} needs exactly one of token and token_file",
                    path.display(),
                    token.name
                ))
            }
        };
        tokens.push(Token {
            name: token.name,
            secret,
        });
    }
    if inline {
        if let Some(warning) = readable_by_others(path) {
            warnings.push(warning);
        }
    }
    let listen = match file.listen {
        Some(text) => Some(parse_listen(&text)?),
        None => None,
    };
    let shutdown_grace = match file.shutdown_grace_seconds {
        Some(s) if s.is_finite() && s >= 0.0 => Some(Duration::from_secs_f64(s)),
        Some(s) => return Err(format!("shutdown_grace_seconds {s} is not usable")),
        None => None,
    };
    Ok(Partial {
        listen,
        data_dir: file.data_dir.map(resolve),
        repos: file
            .repos
            .into_iter()
            .map(|(name, p)| (name, resolve(p)))
            .collect(),
        tokens,
        tls: file.tls.map(|t| TlsFiles {
            cert: resolve(t.cert),
            key: resolve(t.key),
        }),
        max_body_bytes: file.max_body_bytes,
        max_running: file.max_running,
        shutdown_grace,
        harness_commands: file.harness_commands,
        allow_client_commands: file.allow_client_commands,
        warnings,
    })
}

pub fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    text.parse()
        .map_err(|_| format!("{text:?} is not an address such as 127.0.0.1:8421 or [::1]:8421"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(listen: &str) -> Config {
        let mut c = Config::new(PathBuf::from("/tmp/x"));
        c.listen = listen.parse().unwrap();
        c.repos.push(("app".into(), PathBuf::from("/srv/app")));
        c.tokens.push(Token {
            name: "t".into(),
            secret: "0123456789abcdef".into(),
        });
        c
    }

    #[test]
    fn non_loopback_needs_tls_or_the_flag() {
        assert_eq!(config("127.0.0.1:0").validate(), Ok(None));
        assert_eq!(config("[::1]:0").validate(), Ok(None));
        let open = config("0.0.0.0:8421");
        assert!(open.validate().unwrap_err().contains("--insecure-bind"));
        let mut tls = open.clone();
        tls.tls = Some(TlsFiles {
            cert: "c".into(),
            key: "k".into(),
        });
        assert_eq!(tls.validate(), Ok(None));
        let mut insecure = open;
        insecure.insecure_bind = true;
        assert!(insecure.validate().unwrap().unwrap().starts_with("WARNING"));
    }

    #[test]
    fn names_tokens_and_repos_are_checked() {
        let mut c = config("127.0.0.1:0");
        c.tokens[0].secret = "short".into();
        assert!(c.validate().unwrap_err().contains("shorter than 16"));
        let mut c = config("127.0.0.1:0");
        c.repos.push(("app".into(), PathBuf::from("/other")));
        assert!(c.validate().unwrap_err().contains("twice"));
        assert!(check_repo_name("my-app.2").is_ok());
        assert!(check_repo_name("My").is_err());
        assert!(check_repo_name("-x").is_err());
        assert_eq!(repo_name_for(Path::new("/src/My App")), "my-app");
        assert_eq!(repo_name_for(Path::new("/")), "repo");
        let debug = format!("{:?}", config("127.0.0.1:0").tokens[0]);
        assert!(!debug.contains("0123456789abcdef"), "{debug}");
    }

    #[test]
    fn files_resolve_relative_paths_and_reject_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("branchyard-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("t.token"), "0123456789abcdef\n").unwrap();
        let path = dir.join("server.json");
        fs::write(
            &path,
            r#"{"listen": "127.0.0.1:0", "data_dir": "data", "repos": {"app": "repo"},
                "tokens": [{"name": "ci", "token_file": "t.token"}],
                "harness_commands": {"codex": ["/bin/codex"]}}"#,
        )
        .unwrap();
        let partial = load_file(&path).unwrap();
        assert_eq!(partial.data_dir, Some(dir.join("data")));
        assert_eq!(partial.repos, [("app".to_owned(), dir.join("repo"))]);
        assert_eq!(partial.tokens[0].secret, "0123456789abcdef");
        fs::write(&path, r#"{"listen": "127.0.0.1:0", "lisen": 1}"#).unwrap();
        assert!(load_file(&path).unwrap_err().contains("lisen"));
        let _ = fs::remove_dir_all(dir);
    }
}
