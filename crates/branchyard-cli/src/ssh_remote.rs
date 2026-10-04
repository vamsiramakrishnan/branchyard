// Derived from generalaction/emdash
// apps/emdash-desktop/src/core/services/hosts/node/workspace-server/layout.ts
// and
// apps/emdash-desktop/src/core/services/hosts/node/workspace-server/provision/daemon-control.ts,
// at revision 873a3e2067f4abc136272ed2b61abea3a2c07bcf.
// Copyright 2026 General Action, Inc. Licensed under the Apache License,
// Version 2.0; the license text is in vendor/emdash/LICENSE.md.
// Modified for Branchyard: translated from TypeScript to Rust and POSIX sh.
// The remote layout (a private root under the remote home with a `run/`
// directory holding the server's Unix socket) and the start, status and stop
// actions run over one ssh connection are emdash's; emdash's own daemon,
// installer, version directories and SSH library are replaced by `by serve
// --listen-unix` started with nohup, the system `ssh` binary with a control
// master, and an `ssh -O forward` of the socket to a local one. The remote
// home check (absolute, normalized, no line breaks) is applied to the socket
// path the remote reports. A token file generated on the remote and read
// back over the ssh channel is Branchyard's.

//! `by --remote ssh://[user@]host[:port]/path/to/repo`: start (or reuse) a
//! `by serve` for that repository on the host, listening on a Unix socket
//! in a private directory, reached through the system `ssh` with a control
//! master that forwards the socket to a local one; every `--remote` command
//! then talks to the local socket with the ordinary client. See
//! docs/remote-ssh.md.
//!
//! The bearer token is generated on the remote at first use (in its
//! private directory, mode 0600) and read back on the ssh channel's stdout
//! into a 0600 local file: it never appears on a command line.
//!
//! Local state lives in one private directory per URL (`ctl`, the control
//! socket; `by.sock`, the forwarded socket; `token`; `master.log`), under
//! `$BRANCHYARD_SSH_DIR`, else `$XDG_RUNTIME_DIR/branchyard-ssh`, else
//! `~/.branchyard/ssh`. `BRANCHYARD_SSH` names the ssh program (default
//! `ssh`), `BRANCHYARD_SSH_BY` the remote `by` (default `by` on the remote
//! `PATH`), and `BRANCHYARD_SSH_SERVE_ARGS` extra `by serve` arguments for a
//! server this starts (split on whitespace).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use branchyard_recipe::quote;
use serde_json::json;

use crate::commands::{print, Failure, Outcome};

/// How long the control master stays up after its last use.
const PERSIST_SECONDS: u32 = 600;
/// Longest Unix socket path `bind` accepts everywhere (macOS: 104 bytes,
/// with its terminating NUL).
const MAX_SOCKET_PATH: usize = 100;

/// A parsed `ssh://` remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshUrl {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    /// The repository on the host: absolute, or `~/...` under its home.
    pub path: String,
}

impl SshUrl {
    pub fn parse(url: &str) -> Result<SshUrl, String> {
        let rest = url
            .strip_prefix("ssh://")
            .ok_or_else(|| format!("{url:?} is not an ssh:// URL"))?;
        let (authority, path) = rest.split_once('/').ok_or_else(|| {
            format!("{url:?} names no repository: ssh://[user@]host[:port]/path/to/repo")
        })?;
        let (user, hostport) = match authority.rsplit_once('@') {
            Some((user, hostport)) => (Some(user.to_owned()), hostport),
            None => (None, authority),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') || host.ends_with(']') => (
                host,
                Some(
                    port.parse::<u16>()
                        .ok()
                        .filter(|p| *p > 0)
                        .ok_or_else(|| format!("{url:?} has an invalid port {port:?}"))?,
                ),
            ),
            _ => (hostport, None),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        // Never something ssh would read as an option, or a shell as more
        // than one word.
        let word = |what: &str, value: &str| -> Result<(), String> {
            let bad = value.is_empty()
                || value.starts_with('-')
                || value.chars().any(|c| c.is_whitespace() || c.is_control())
                || value.contains(['\'', '"', '`', '$', '\\', ';', '&', '|', '<', '>', '/']);
            match bad {
                true => Err(format!("{url:?} has an unusable {what} {value:?}")),
                false => Ok(()),
            }
        };
        word("host", host)?;
        if let Some(user) = &user {
            word("user", user)?;
        }
        if path.is_empty() || path.contains(['\n', '\r', '\0']) || path.contains(['?', '#']) {
            return Err(format!(
                "{url:?} names no usable repository path: ssh://host/path/to/repo, or \
                 ssh://host/~/repo under the remote home"
            ));
        }
        // `ssh://host/~/repo` is under the remote home; anything else is
        // absolute.
        let path = match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => format!("/{path}"),
        };
        Ok(SshUrl {
            user,
            host: host.to_owned(),
            port,
            path,
        })
    }

    /// The URL in one spelling, which names its local directory.
    pub fn canonical(&self) -> String {
        let user = self
            .user
            .as_deref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default();
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        let path = self.path.strip_prefix('/').unwrap_or(&self.path);
        format!("ssh://{user}{}{port}/{path}", self.host)
    }

    /// The remote's private directory name: the repository path's digest.
    fn remote_key(&self) -> String {
        digest(&self.path)[..16].to_owned()
    }
}

fn digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// This machine's directory for one remote, and the files in it.
pub struct Local {
    pub dir: PathBuf,
}

impl Local {
    fn for_url(url: &SshUrl) -> Result<Local, Failure> {
        let base = match std::env::var_os("BRANCHYARD_SSH_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
                Some(dir) => PathBuf::from(dir).join("branchyard-ssh"),
                None => dirs::home_dir()
                    .ok_or_else(|| Failure::Message("no home directory for ssh state".into()))?
                    .join(".branchyard")
                    .join("ssh"),
            },
        };
        let dir = base.join(&digest(&url.canonical())[..16]);
        let local = Local { dir };
        let longest = local.socket().display().to_string().len();
        if longest > MAX_SOCKET_PATH {
            return Err(Failure::Message(format!(
                "{} is too long for a Unix socket path; set BRANCHYARD_SSH_DIR to a shorter \
                 directory",
                local.socket().display()
            )));
        }
        Ok(local)
    }

    fn create(&self) -> Result<(), Failure> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|e| Failure::Message(format!("create {}: {e}", self.dir.display())))?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| Failure::Message(format!("{}: {e}", self.dir.display())))
    }

    pub fn control(&self) -> PathBuf {
        self.dir.join("ctl")
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("by.sock")
    }

    pub fn token(&self) -> PathBuf {
        self.dir.join("token")
    }

    fn log(&self) -> PathBuf {
        self.dir.join("master.log")
    }

    /// Write the token, readable by this user only.
    fn write_token(&self, token: &str) -> Result<(), Failure> {
        use std::os::unix::fs::OpenOptionsExt;
        let temporary = self.dir.join(format!("token.{}", std::process::id()));
        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .and_then(|mut file| writeln!(file, "{token}"))
            .and_then(|()| fs::rename(&temporary, self.token()));
        written.map_err(|e| Failure::Message(format!("write {}: {e}", self.token().display())))
    }
}

/// The `ssh` program and the arguments every invocation for `url` shares.
struct Ssh<'a> {
    program: String,
    url: &'a SshUrl,
    local: &'a Local,
}

impl<'a> Ssh<'a> {
    fn new(url: &'a SshUrl, local: &'a Local) -> Ssh<'a> {
        let program = std::env::var("BRANCHYARD_SSH")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "ssh".into());
        Ssh {
            program,
            url,
            local,
        }
    }

    /// `ssh` with the control path, port and user, before `extra`, then
    /// `-- host`.
    fn command(&self, extra: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("-S").arg(self.local.control()).args(extra);
        if let Some(port) = self.url.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(user) = &self.url.user {
            command.arg("-l").arg(user);
        }
        command.arg("--").arg(&self.url.host);
        command
    }

    fn run(&self, mut command: Command, what: &str) -> Result<Output, Failure> {
        command.stdin(Stdio::null());
        command.output().map_err(|e| {
            Failure::Message(format!(
                "{what}: cannot run {} ({e}); install OpenSSH's client or set BRANCHYARD_SSH",
                self.program
            ))
        })
    }

    fn master_running(&self) -> Result<bool, Failure> {
        let out = self.run(self.command(&["-O", "check"]), "ssh -O check")?;
        Ok(out.status.success())
    }

    /// Start the control master, unless one runs: it authenticates once
    /// (prompting on the terminal if it must), then goes to the
    /// background and stays up `PERSIST_SECONDS` after its last client.
    fn ensure_master(&self) -> Result<(), Failure> {
        if self.master_running()? {
            return Ok(());
        }
        let log = fs::File::create(self.local.log())
            .map_err(|e| Failure::Message(format!("{}: {e}", self.local.log().display())))?;
        let persist = format!("ControlPersist={PERSIST_SECONDS}");
        let mut command = self.command(&[
            "-M",
            "-N",
            "-f",
            "-o",
            &persist,
            "-o",
            "StreamLocalBindUnlink=yes",
            "-o",
            "ServerAliveInterval=30",
        ]);
        // Its stderr outlives this process (the master keeps it), so it
        // goes to a file, never a pipe someone waits on; prompts use the
        // terminal.
        command.stdout(Stdio::null()).stderr(log);
        let status = command.status().map_err(|e| {
            Failure::Message(format!(
                "cannot run {} ({e}); install OpenSSH's client or set BRANCHYARD_SSH",
                self.program
            ))
        })?;
        if !status.success() {
            let log = fs::read_to_string(self.local.log()).unwrap_or_default();
            return Err(Failure::Message(format!(
                "ssh to {} failed ({status}): {}",
                self.url.host,
                log.trim()
            )));
        }
        Ok(())
    }

    /// Run `script` with `sh -s` on the remote, its arguments quoted.
    fn script(&self, script: &str, args: &[String]) -> Result<Output, Failure> {
        let mut remote = String::from("sh -s --");
        for arg in args {
            remote.push(' ');
            remote.push_str(&quote(arg));
        }
        let mut command = self.command(&["-o", "ControlMaster=no", "-T"]);
        command
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| {
            Failure::Message(format!(
                "cannot run {} ({e}); install OpenSSH's client or set BRANCHYARD_SSH",
                self.program
            ))
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        child.wait_with_output().map_err(Failure::Io)
    }

    fn forward(&self, remote_socket: &str) -> Result<(), Failure> {
        let spec = format!("{}:{remote_socket}", self.local.socket().display());
        let out = self.run(
            self.command(&["-O", "forward", "-L", &spec]),
            "ssh -O forward",
        )?;
        if !out.status.success() {
            return Err(Failure::Message(format!(
                "could not forward {remote_socket} from {}: {}",
                self.url.host,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    fn exit_master(&self) -> Result<bool, Failure> {
        let out = self.run(self.command(&["-O", "exit"]), "ssh -O exit")?;
        Ok(out.status.success())
    }
}

/// The remote side, in POSIX sh: `$1` the repository, `$2` its key, `$3`
/// the remote `by`, then `by serve`'s extra arguments. Layout after
/// emdash's workspace server: `~/.branchyard/remote/KEY/` holds the token
/// and the log, and `run/` the socket and the pid.
const PRELUDE: &str = r#"set -u
repo=$1; key=$2; by=$3; shift 3
case $repo in "~") repo=$HOME ;; "~/"*) repo="$HOME/${repo#\~/}" ;; esac
umask 077
base="$HOME/.branchyard/remote"
root="$base/$key"; run="$root/run"
sock="$run/by.sock"; pidf="$run/by.pid"; tokf="$root/token"; log="$root/serve.log"
alive() { [ -s "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; }
"#;

const START: &str = r#"mkdir -p "$run" && chmod 700 "$base" "$root" "$run" || exit 70
if [ ! -s "$tokf" ]; then
  od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$tokf.new" && mv "$tokf.new" "$tokf" || exit 71
fi
state=started
if [ -S "$sock" ] && alive; then
  state=reused
else
  [ -d "$repo" ] || { echo "no directory $repo on the remote" >&2; exit 72; }
  command -v "$by" >/dev/null 2>&1 || {
    echo "$by is not on the remote PATH; install by there, or set BRANCHYARD_SSH_BY" >&2
    exit 75
  }
  rm -f "$sock"
  cd "$repo" || exit 72
  nohup "$by" serve --listen-unix "$sock" --token-file "$tokf" "$@" </dev/null >"$log" 2>&1 &
  echo $! >"$pidf"
  i=0
  while [ ! -S "$sock" ]; do
    if ! alive; then echo "by serve exited:" >&2; tail -n 20 "$log" >&2; exit 73; fi
    i=$((i + 1))
    if [ "$i" -gt 600 ]; then echo "by serve did not start:" >&2; tail -n 20 "$log" >&2; exit 74; fi
    sleep 0.1 2>/dev/null || sleep 1
  done
fi
printf 'state=%s\nsocket=%s\npid=%s\n' "$state" "$sock" "$(cat "$pidf")"
printf 'token=%s\n' "$(cat "$tokf")"
"#;

const STATUS: &str = r#"if [ -S "$sock" ] && alive; then state=running; else state=stopped; fi
printf 'state=%s\nsocket=%s\npid=%s\n' "$state" "$sock" "$(cat "$pidf" 2>/dev/null)"
"#;

const STOP: &str = r#"state=stopped
if alive; then
  pid=$(cat "$pidf"); kill -TERM "$pid" 2>/dev/null; i=0
  while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 900 ]; do i=$((i + 1)); sleep 0.1 2>/dev/null || sleep 1; done
  if kill -0 "$pid" 2>/dev/null; then state=running; else state=stopped; fi
fi
[ "$state" = stopped ] && rm -f "$pidf" "$sock"
printf 'state=%s\nsocket=%s\n' "$state" "$sock"
"#;

/// `key=value` lines.
fn fields(stdout: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// emdash's remote home check, on the path the remote reports.
fn check_remote_path(path: &str) -> Result<(), String> {
    let unusable = !path.starts_with('/')
        || path.contains(['\0', '\n', '\r'])
        || path.contains("//")
        || path.split('/').any(|c| c == "." || c == "..")
        || (path.len() > 1 && path.ends_with('/'));
    let normal = !unusable;
    match normal {
        true if path.len() <= MAX_SOCKET_PATH => Ok(()),
        true => Err(format!(
            "the remote socket path {path} is too long for a Unix socket; use a shorter home"
        )),
        false => Err(format!(
            "the remote reported an unusable socket path {path:?}"
        )),
    }
}

fn arguments(url: &SshUrl) -> Vec<String> {
    let by = std::env::var("BRANCHYARD_SSH_BY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "by".into());
    let mut args = vec![url.path.clone(), url.remote_key(), by];
    if let Ok(extra) = std::env::var("BRANCHYARD_SSH_SERVE_ARGS") {
        args.extend(serve_args(&extra));
    }
    args
}

/// `BRANCHYARD_SSH_SERVE_ARGS` as shell words, so an argument can hold
/// spaces when quoted. Text with an unterminated quote falls back to
/// whitespace splitting, as it always has.
fn serve_args(text: &str) -> Vec<String> {
    shlex::split(text).unwrap_or_else(|| text.split_whitespace().map(str::to_owned).collect())
}

fn remote_failure(what: &str, url: &SshUrl, out: &Output) -> Failure {
    let stderr = String::from_utf8_lossy(&out.stderr);
    Failure::Message(format!(
        "{what} on {} failed ({}): {}",
        url.host,
        out.status,
        stderr.trim()
    ))
}

/// A server reached over ssh: the local URL and token file the ordinary
/// client uses.
pub struct Tunnel {
    pub url: String,
    pub token_file: PathBuf,
}

/// Start or reuse the remote server and the forward; see the module
/// documentation.
pub fn connect(url: &str) -> Result<Tunnel, Failure> {
    let url = SshUrl::parse(url).map_err(Failure::Message)?;
    let local = Local::for_url(&url)?;
    local.create()?;
    let ssh = Ssh::new(&url, &local);
    ssh.ensure_master()?;
    let out = ssh.script(&format!("{PRELUDE}{START}"), &arguments(&url))?;
    if !out.status.success() {
        return Err(remote_failure("starting by serve", &url, &out));
    }
    let fields = fields(&out.stdout);
    let socket = field(&fields, "socket")
        .ok_or_else(|| remote_failure("starting by serve (no socket reported)", &url, &out))?;
    check_remote_path(socket).map_err(Failure::Message)?;
    let token = field(&fields, "token").unwrap_or("").trim();
    if token.len() < 16 || token.contains(char::is_whitespace) {
        return Err(Failure::Message(format!(
            "the remote token on {} is missing or too short",
            url.host
        )));
    }
    local.write_token(token)?;
    ssh.forward(socket)?;
    Ok(Tunnel {
        url: format!("unix:{}", local.socket().display()),
        token_file: local.token(),
    })
}

/// `by remote ssh status|stop [URL]`.
pub fn main(stop: bool, url: &str, json: bool) -> Outcome {
    let url = SshUrl::parse(url).map_err(Failure::Message)?;
    let local = Local::for_url(&url)?;
    let ssh = Ssh::new(&url, &local);
    let master = local.dir.is_dir() && ssh.master_running()?;
    if !stop {
        let remote = match master {
            true => {
                let out = ssh.script(&format!("{PRELUDE}{STATUS}"), &arguments(&url))?;
                if !out.status.success() {
                    return Err(remote_failure("by remote ssh status", &url, &out));
                }
                fields(&out.stdout)
            }
            false => Vec::new(),
        };
        let server = field(&remote, "state").unwrap_or("unknown");
        let pid = field(&remote, "pid").filter(|p| !p.is_empty());
        let forwarded = master && local.socket().exists();
        if json {
            let value = json!({
                "url": url.canonical(),
                "connection": if master { "up" } else { "down" },
                "server": server,
                "pid": pid,
                "remote_socket": field(&remote, "socket"),
                "local_socket": forwarded.then(|| local.socket().display().to_string()),
                "token_file": local.token().is_file().then(|| local.token().display().to_string()),
            });
            return print(&format!("{value:#}\n"));
        }
        let mut text = format!("{}\n", url.canonical());
        text.push_str(&format!(
            "  connection  {}\n",
            match master {
                true => "up (ssh control master)",
                false => "down (the next --remote command connects)",
            }
        ));
        text.push_str(&format!(
            "  server      {}{}\n",
            match master {
                true => server,
                false => "unknown while the connection is down",
            },
            pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
        ));
        if forwarded {
            text.push_str(&format!(
                "  local       unix:{}\n",
                local.socket().display()
            ));
        }
        return print(&text);
    }
    // Stopping needs the connection: open it if it is down.
    if !master {
        local.create()?;
        ssh.ensure_master()?;
    }
    let out = ssh.script(&format!("{PRELUDE}{STOP}"), &arguments(&url))?;
    if !out.status.success() {
        return Err(remote_failure("by remote ssh stop", &url, &out));
    }
    let remote = fields(&out.stdout);
    let server = field(&remote, "state").unwrap_or("unknown").to_owned();
    let closed = ssh.exit_master()?;
    for path in [local.socket(), local.token(), local.control()] {
        branchyard_support::cleanup_file(path);
    }
    if server != "stopped" {
        return Err(Failure::Message(format!(
            "by serve on {} did not stop within 90 seconds; the connection was closed",
            url.host
        )));
    }
    match json {
        true => print(&format!(
            "{:#}\n",
            json!({ "url": url.canonical(), "server": server, "connection": if closed { "closed" } else { "down" } })
        )),
        false => print(&format!(
            "stopped by serve on {} and closed the connection\n",
            url.canonical()
        )),
    }
}

pub const REMOTE_EXAMPLES: &str = "\
Examples:
  by --remote ssh://me@build.example/srv/app run \"fix the flaky test\" --yes
  by remote ssh status ssh://me@build.example/srv/app
  by --remote ssh://me@build.example/srv/app remote ssh stop";

/// `by remote ...`.
#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum RemoteAction {
    /// The ssh connection and the by serve it started on a host
    #[command(subcommand)]
    Ssh(SshAction),
}

/// `by remote ssh ...`.
#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum SshAction {
    /// Whether the connection is up, and the remote server is running
    Status {
        /// ssh://[user@]host[:port]/path (default: --remote)
        url: Option<String>,
    },
    /// Stop the remote by serve, close the connection, and forget the local socket and token
    Stop {
        /// ssh://[user@]host[:port]/path (default: --remote)
        url: Option<String>,
    },
}

/// `by remote ssh status|stop`.
pub fn command(globals: &crate::args::Globals, action: &RemoteAction, json: bool) -> Outcome {
    let RemoteAction::Ssh(action) = action;
    let (stop, url) = match action {
        SshAction::Status { url } => (false, url),
        SshAction::Stop { url } => (true, url),
    };
    let url = match url.as_deref().or(globals.remote.as_deref()) {
        Some(url) if is_ssh(url) => url.to_owned(),
        Some(url) => {
            return Err(Failure::Message(format!(
                "{url} is not an ssh:// remote; by remote ssh manages those only"
            )))
        }
        None => {
            return Err(Failure::Message(
                "name the remote: by remote ssh status ssh://host/path, or pass --remote".into(),
            ))
        }
    };
    main(stop, &url, json)
}

/// Whether `url` is an ssh remote.
pub fn is_ssh(url: &str) -> bool {
    url.starts_with("ssh://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_into_user_host_port_and_path() {
        let u = SshUrl::parse("ssh://me@build.example:2222/srv/app").unwrap();
        assert_eq!(
            (u.user.as_deref(), u.host.as_str(), u.port, u.path.as_str()),
            (Some("me"), "build.example", Some(2222), "/srv/app")
        );
        assert_eq!(u.canonical(), "ssh://me@build.example:2222/srv/app");
        let u = SshUrl::parse("ssh://box/~/src/app").unwrap();
        assert_eq!((u.user, u.port, u.path.as_str()), (None, None, "~/src/app"));
        let u = SshUrl::parse("ssh://[::1]:22/r").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", Some(22)));
        for bad in [
            "http://x/y",
            "ssh://host",
            "ssh://host/",
            "ssh://-oProxyCommand=x/r",
            "ssh://a b/r",
            "ssh://host:0/r",
            "ssh://host:x/r",
            "ssh://me;rm@host/r",
            "ssh://host/r?x",
        ] {
            assert!(SshUrl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn words_are_quoted_for_the_remote_shell() {
        for word in ["a b", "it's", "", "$HOME `x` \"q\" \\ ;&|*", "plain-1.2/x"] {
            assert_eq!(shlex::split(&quote(word)).unwrap(), [word], "{word:?}");
        }
        assert_eq!(quote("plain"), "plain");
    }

    #[test]
    fn serve_arguments_split_like_a_shell() {
        assert_eq!(serve_args("--a --b"), ["--a", "--b"]);
        assert_eq!(
            serve_args("--label 'two words' --name=\"x y\""),
            ["--label", "two words", "--name=x y"]
        );
        assert_eq!(serve_args("--bad 'open"), ["--bad", "'open"]);
        assert!(serve_args("  ").is_empty());
    }

    /// The vendored emdash sources still have the shape this follows
    /// (patches/ports.json): a socket in `run/` under a private root, and
    /// the remote home check.
    #[test]
    fn emdashs_layout_is_what_this_follows() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/emdash/apps/emdash-desktop/src/core/services/hosts/node/workspace-server/"
        );
        let layout = std::fs::read_to_string(format!("{dir}layout.ts")).unwrap();
        assert!(layout.contains("socketPath: path.posix.join(root, 'run/workspace.sock')"));
        assert!(layout.contains("path.posix.normalize(home) !== home"));
        let daemon = std::fs::read_to_string(format!("{dir}provision/daemon-control.ts")).unwrap();
        assert!(daemon.contains("args: [action, '--socket', layout.socketPath]"));
    }

    #[test]
    fn remote_paths_are_checked_as_emdash_checks_a_home() {
        assert!(check_remote_path("/home/me/.branchyard/remote/k/run/by.sock").is_ok());
        for bad in ["relative", "/a//b", "/a/../b", "/a/", "/a\nb"] {
            assert!(check_remote_path(bad).is_err(), "{bad:?}");
        }
        assert!(check_remote_path(&format!("/{}", "x".repeat(120))).is_err());
    }
}
