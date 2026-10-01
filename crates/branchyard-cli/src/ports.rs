// Derived from stablyai/orca at revision
// 280733273545f0b3eeedc1be54b14d406239030e:
// src/main/ports/local-workspace-platform-port-scanner.ts,
// src/main/ports/local-workspace-port-attribution.ts and
// src/main/ports/local-workspace-port-address.ts.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust, synchronous
// and uncached (no metadata cache, scan worker or timeout backoff); the
// Windows netstat path is left out; attribution maps a listener to a
// Branchyard branch rather than an Orca worktree, and first by the
// BRANCHYARD_BRANCH and BRANCHYARD_ROOT variables a script or harness was
// started with, then by its own or an ancestor's working directory (the
// process tree), then by its command line; advertised-URL watching is not
// ported.

//! Which TCP ports each branch's processes listen on: `by workspace ports`,
//! `by show`, and `by watch`'s detail pane (`b` opens one in a browser,
//! `K` stops its processes). On Linux from `/proc`; elsewhere from `lsof`
//! (macOS; untested). See `docs/workspace.md#listening-ports`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

/// A listening socket as the platform reports it, with what is known of
/// its process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Raw {
    pub host: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
}

/// A listener attributed to a branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Listener {
    pub branch: String,
    /// What it is bound to, and what to connect to (`localhost` for a
    /// wildcard bind).
    pub host: String,
    pub connect_host: String,
    pub port: u16,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub command: Option<String>,
    /// How it was attributed: `env` (its environment names the branch),
    /// `cwd` (it runs in the worktree), `ancestor` (a parent process does),
    /// or `command` (its command line names the worktree).
    pub by: &'static str,
    /// `http`, `https` or `unknown`, guessed from the port as Orca does.
    pub protocol: &'static str,
}

impl Listener {
    pub fn url(&self) -> String {
        let scheme = match self.protocol {
            "https" => "https",
            _ => "http",
        };
        let host = match self.connect_host.contains(':') {
            true => format!("[{}]", self.connect_host),
            false => self.connect_host.clone(),
        };
        format!("{scheme}://{host}:{}/", self.port)
    }
}

/// Orca's `connectHostForBindHost`.
pub fn connect_host(host: &str) -> String {
    match host {
        "*" | "0.0.0.0" | "::" => "localhost".into(),
        other => other.into(),
    }
}

/// Orca's `inferProtocol`.
fn protocol(port: u16) -> &'static str {
    match port {
        443 | 8443 => "https",
        80 | 3000 | 3001 | 4200 | 5000 | 5173 | 5174 | 8000 | 8080 | 8888 => "http",
        _ => "unknown",
    }
}

/// Orca's `parseProcAddress`: `0100007F:1F90` is 127.0.0.1:8080.
pub fn parse_proc_address(hex: &str) -> Option<(String, u16)> {
    let (addr, port) = hex.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok().filter(|p| *p != 0)?;
    match addr.len() {
        8 => {
            let byte = |i: usize| u8::from_str_radix(&addr[i..i + 2], 16).ok();
            Some((
                format!("{}.{}.{}.{}", byte(6)?, byte(4)?, byte(2)?, byte(0)?),
                port,
            ))
        }
        32 => {
            if addr == "00000000000000000000000000000000" {
                return Some(("::".into(), port));
            }
            if addr == "00000000000000000000000001000000" {
                return Some(("::1".into(), port));
            }
            let mut groups = Vec::new();
            for i in (0..32).step_by(8) {
                let chunk = &addr[i..i + 8];
                let reversed = format!(
                    "{}{}{}{}",
                    &chunk[6..8],
                    &chunk[4..6],
                    &chunk[2..4],
                    &chunk[0..2]
                );
                for half in [&reversed[0..4], &reversed[4..8]] {
                    let trimmed = half.trim_start_matches('0');
                    groups.push(match trimmed.is_empty() {
                        true => "0".to_owned(),
                        false => trimmed.to_ascii_lowercase(),
                    });
                }
            }
            Some((groups.join(":"), port))
        }
        _ => None,
    }
}

/// Orca's `parseProcNetTcp`: the listening sockets (state `0A`) of
/// `/proc/net/tcp` or `tcp6`, with their inodes.
pub fn parse_proc_net_tcp(content: &str) -> Vec<(String, u16, u64)> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (host, port) = parse_proc_address(fields[1])?;
            let inode: u64 = fields[9].parse().ok().filter(|i| *i != 0)?;
            Some((host, port, inode))
        })
        .collect()
}

/// Orca's `parseAddressWithPort`, for `lsof`'s `n` field.
fn parse_address(value: &str) -> Option<(String, u16)> {
    let trimmed = value.trim().trim_end_matches(" (LISTEN)");
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return Some((host.to_owned(), port.parse().ok()?));
    }
    let (host, port) = trimmed.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
    Some((host.to_owned(), port))
}

/// Orca's `parseLsofListeningOutput` (`lsof -nP -iTCP -sTCP:LISTEN -F pcn`).
pub fn parse_lsof(output: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    let (mut pid, mut process) = (None, None);
    for line in output.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().ok();
                process = None;
            }
            'c' => process = Some(value.to_owned()),
            'n' => {
                if let Some((host, port)) = parse_address(value) {
                    out.push(Raw {
                        host,
                        port,
                        pid,
                        process: process.clone(),
                        ..Raw::default()
                    });
                }
            }
            _ => {}
        }
    }
    dedupe(out)
}

/// Orca's `dedupeRawPorts`: one row per connect host, port and pid.
fn dedupe(ports: Vec<Raw>) -> Vec<Raw> {
    let mut seen = HashSet::new();
    ports
        .into_iter()
        .filter(|p| seen.insert((connect_host(&p.host), p.port, p.pid)))
        .collect()
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// What `/proc` says about a process.
#[derive(Clone, Debug, Default)]
struct Process {
    parent: Option<u32>,
    cwd: Option<String>,
    command: Option<String>,
    name: Option<String>,
    /// `BRANCHYARD_BRANCH` and `BRANCHYARD_ROOT` from its environment,
    /// readable only for your own processes.
    branch: Option<(String, Option<String>)>,
}

fn proc_info(pid: u32) -> Process {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    let parent = read_text(dir.join("stat")).and_then(|stat| {
        // `pid (comm) state ppid …`; comm may hold spaces and parentheses.
        let after = &stat[stat.rfind(')')? + 1..];
        after.split_whitespace().nth(1)?.parse().ok()
    });
    let environ = fs::read(dir.join("environ")).ok().map(|bytes| {
        let mut vars = HashMap::new();
        for entry in bytes.split(|b| *b == 0) {
            let entry = String::from_utf8_lossy(entry);
            if let Some((k, v)) = entry.split_once('=') {
                if k == "BRANCHYARD_BRANCH" || k == "BRANCHYARD_ROOT" {
                    vars.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        vars
    });
    Process {
        parent,
        cwd: fs::read_link(dir.join("cwd"))
            .ok()
            .map(|p| p.display().to_string()),
        command: read_text(dir.join("cmdline"))
            .map(|c| {
                c.split('\0')
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_owned()
            })
            .filter(|c| !c.is_empty()),
        name: read_text(dir.join("comm"))
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty()),
        branch: environ.and_then(|mut vars| {
            let branch = vars.remove("BRANCHYARD_BRANCH").filter(|b| !b.is_empty())?;
            Some((branch, vars.remove("BRANCHYARD_ROOT")))
        }),
    }
}

/// Orca's `scanLinuxProcPorts`: sockets from `/proc/net/tcp{,6}`, owners
/// by the socket inodes in each process's `fd/`.
fn scan_linux() -> Vec<Raw> {
    let mut sockets = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Some(content) = read_text(file) {
            sockets.extend(parse_proc_net_tcp(&content));
        }
    }
    let wanted: HashSet<u64> = sockets.iter().map(|s| s.2).collect();
    let mut owner: HashMap<u64, u32> = HashMap::new();
    if !wanted.is_empty() {
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(link) = fs::read_link(fd.path()) else {
                    continue;
                };
                let link = link.display().to_string();
                if let Some(inode) = link
                    .strip_prefix("socket:[")
                    .and_then(|r| r.strip_suffix(']'))
                    .and_then(|i| i.parse::<u64>().ok())
                {
                    if wanted.contains(&inode) {
                        owner.insert(inode, pid);
                    }
                }
            }
        }
    }
    let raw = sockets
        .into_iter()
        .map(|(host, port, inode)| {
            let pid = owner.get(&inode).copied();
            let info = pid.map(proc_info).unwrap_or_default();
            Raw {
                host,
                port,
                pid,
                process: info.name,
                command: info.command,
                cwd: info.cwd,
            }
        })
        .collect();
    dedupe(raw)
}

/// `lsof`, then `lsof -d cwd` and `ps` for the owners (Orca's macOS path;
/// untested here).
fn scan_lsof() -> Vec<Raw> {
    let Ok(out) = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"])
        .output()
    else {
        return Vec::new();
    };
    let mut ports = parse_lsof(&String::from_utf8_lossy(&out.stdout));
    let pids: Vec<String> = ports
        .iter()
        .filter_map(|p| p.pid)
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    if pids.is_empty() {
        return ports;
    }
    let list = pids.join(",");
    let mut cwd: HashMap<u32, String> = HashMap::new();
    if let Ok(out) = Command::new("lsof")
        .args(["-a", "-p", &list, "-d", "cwd", "-Fn"])
        .output()
    {
        let mut current = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(pid) = line.strip_prefix('p') {
                current = pid.parse().ok();
            } else if let (Some(path), Some(pid)) = (line.strip_prefix('n'), current) {
                cwd.insert(pid, path.to_owned());
            }
        }
    }
    let mut command: HashMap<u32, String> = HashMap::new();
    if let Ok(out) = Command::new("ps")
        .args(["-p", &list, "-o", "pid=", "-o", "command="])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim_start();
            if let Some((pid, rest)) = line.split_once(char::is_whitespace) {
                if let Ok(pid) = pid.parse() {
                    command.insert(pid, rest.trim().to_owned());
                }
            }
        }
    }
    for port in &mut ports {
        if let Some(pid) = port.pid {
            port.cwd = cwd.get(&pid).cloned();
            port.command = command.get(&pid).cloned();
        }
    }
    ports
}

/// Every listening TCP socket on this machine that can be seen.
pub fn scan() -> Vec<Raw> {
    match Path::new("/proc/net/tcp").exists() {
        true => scan_linux(),
        false => scan_lsof(),
    }
}

/// Orca's `normalizeComparablePath`.
fn normalize(path: &str) -> String {
    let mut out = String::new();
    for part in path.replace('\\', "/").split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if let Some(i) = out.rfind('/') {
                    out.truncate(i);
                }
            }
            part => {
                out.push('/');
                out.push_str(part);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Orca's `isSameOrDescendant`.
fn within(candidate: &str, parent: &str) -> bool {
    candidate == parent || candidate.starts_with(&format!("{}/", parent.trim_end_matches('/')))
}

/// Orca's `includesPathBoundary`: the command line names `path` as a whole
/// path, not as the prefix of a longer one.
fn names_path(command: &str, path: &str) -> bool {
    command.match_indices(path).any(|(i, _)| {
        let before = command[..i].chars().last();
        let after = command[i + path.len()..].chars().next();
        before.is_none_or(|c| c.is_whitespace() || "\"'=".contains(c))
            && after.is_none_or(|c| c.is_whitespace() || "\"'/:".contains(c))
    })
}

/// Which branch a listener belongs to, and how that is known. `worktrees`
/// maps each branch to its worktree; `root` is the repository root its
/// `BRANCHYARD_ROOT` must name. The deepest worktree wins, as in Orca.
fn owner(
    raw: &Raw,
    worktrees: &[(String, String)],
    root: &str,
    processes: &mut HashMap<u32, Process>,
) -> Option<(String, &'static str)> {
    let mut info = |pid: u32| -> Process {
        processes
            .entry(pid)
            .or_insert_with(|| match Path::new("/proc").is_dir() {
                true => proc_info(pid),
                false => Process::default(),
            })
            .clone()
    };
    let deepest = |pred: &dyn Fn(&str) -> bool| {
        worktrees
            .iter()
            .filter(|(_, path)| pred(path))
            .max_by_key(|(_, path)| path.len())
            .map(|(branch, _)| branch.clone())
    };
    let known: HashSet<&str> = worktrees.iter().map(|(b, _)| b.as_str()).collect();
    // The environment a Branchyard script or harness was started with,
    // on the process or the nearest ancestor that has one.
    let mut pid = raw.pid;
    let mut hops = 0;
    while let (Some(p), true) = (pid, hops < 32) {
        let process = info(p);
        if let Some((branch, branch_root)) = &process.branch {
            let same_root = branch_root
                .as_deref()
                .is_none_or(|r| normalize(r) == normalize(root));
            if same_root && known.contains(branch.as_str()) {
                return Some((branch.clone(), "env"));
            }
        }
        pid = process
            .parent
            .filter(|parent| *parent > 1 && Some(*parent) != pid);
        hops += 1;
    }
    if let Some(cwd) = raw.cwd.as_deref().map(normalize) {
        if let Some(branch) = deepest(&|path| within(&cwd, path)) {
            return Some((branch, "cwd"));
        }
    }
    let mut pid = raw.pid.and_then(|p| info(p).parent);
    let mut hops = 0;
    while let (Some(p), true) = (pid, hops < 32) {
        if p <= 1 {
            break;
        }
        let process = info(p);
        if let Some(cwd) = process.cwd.as_deref().map(normalize) {
            if let Some(branch) = deepest(&|path| within(&cwd, path)) {
                return Some((branch, "ancestor"));
            }
        }
        pid = process.parent;
        hops += 1;
    }
    let command = raw.command.as_deref()?;
    deepest(&|path| names_path(command, path)).map(|b| (b, "command"))
}

/// Attribute `raw` listeners to the branches whose worktrees `worktrees`
/// lists; others are left out.
pub fn attribute(raw: &[Raw], worktrees: &[(String, PathBuf)], root: &Path) -> Vec<Listener> {
    let worktrees: Vec<(String, String)> = worktrees
        .iter()
        .map(|(b, p)| {
            let p = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            (b.clone(), normalize(&p.display().to_string()))
        })
        .collect();
    let root = fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string();
    let mut processes = HashMap::new();
    let mut out: Vec<Listener> = raw
        .iter()
        .filter_map(|r| {
            let (branch, by) = owner(r, &worktrees, &root, &mut processes)?;
            Some(Listener {
                branch,
                host: r.host.clone(),
                connect_host: connect_host(&r.host),
                port: r.port,
                pid: r.pid,
                process: r.process.clone(),
                command: r.command.clone(),
                by,
                protocol: protocol(r.port),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        (&a.branch, a.port, &a.connect_host).cmp(&(&b.branch, b.port, &b.connect_host))
    });
    // The same port bound on IPv4 and IPv6 by one process is one row.
    out.dedup_by(|a, b| a.branch == b.branch && a.port == b.port && a.pid == b.pid);
    out
}

/// The listeners of every branch of `yard` that has a worktree here, by
/// branch.
pub fn of_yard(yard: &branchyard::Yard) -> BTreeMap<String, Vec<Listener>> {
    let worktrees: Vec<(String, PathBuf)> = yard
        .branches()
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.worktree.is_dir())
        .map(|b| (b.name, b.worktree))
        .collect();
    let mut out: BTreeMap<String, Vec<Listener>> = BTreeMap::new();
    if worktrees.is_empty() {
        return out;
    }
    for listener in attribute(&scan(), &worktrees, yard.root()) {
        out.entry(listener.branch.clone())
            .or_default()
            .push(listener);
    }
    out
}

/// One line per listener: `:5173 node (pid 4242, cwd) http://localhost:5173/`.
pub fn lines(listeners: &[Listener]) -> Vec<String> {
    listeners
        .iter()
        .map(|l| {
            let who = match (&l.process, l.pid) {
                (Some(name), Some(pid)) => format!("{name} (pid {pid}, by {})", l.by),
                (None, Some(pid)) => format!("pid {pid} (by {})", l.by),
                _ => format!("(by {})", l.by),
            };
            format!(":{} {who} {}", l.port, l.url())
        })
        .collect()
}

/// Open `url` in a browser: `$BROWSER`, else `xdg-open` (Linux) or
/// `open` (macOS).
pub fn open_browser(url: &str) -> Result<(), String> {
    let browser = std::env::var("BROWSER")
        .ok()
        .filter(|b| !b.trim().is_empty());
    let mut command = match browser {
        Some(b) => {
            let words = shlex::split(&b).ok_or_else(|| format!("$BROWSER {b:?} does not parse"))?;
            let (program, args) = words.split_first().ok_or("$BROWSER is empty")?;
            let mut c = Command::new(program);
            c.args(args);
            c
        }
        None if cfg!(target_os = "macos") => Command::new("open"),
        None => Command::new("xdg-open"),
    };
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let program = command.get_program().to_string_lossy().into_owned();
    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{program} {url} failed ({status})")),
        Err(e) => Err(format!(
            "could not run {program} to open {url} ({e}); set $BROWSER"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_net_tcp_parses() {
        let content = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
            0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1\n   \
            1: 0100007F:1F91 0100007F:9999 01 00000000:00000000 00:00000000 00000000  1000        0 12346 1\n   \
            2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 0 1\n";
        assert_eq!(
            parse_proc_net_tcp(content),
            [("127.0.0.1".to_owned(), 8080, 12345)]
        );
        assert_eq!(
            parse_proc_address("00000000000000000000000001000000:1F90"),
            Some(("::1".into(), 8080))
        );
        assert_eq!(
            parse_proc_address("B80D01200000000067452301EFCDAB89:0050"),
            Some(("2001:db8:0:0:123:4567:89ab:cdef".into(), 80))
        );
        assert_eq!(parse_proc_address("0100007F:0000"), None);
    }

    /// The vendored sources still have the shape this port follows.
    #[test]
    fn orcas_sources_are_the_ones_ported() {
        let scanner = include_str!(
            "../../../vendor/orca/src/main/ports/local-workspace-platform-port-scanner.ts"
        );
        assert!(scanner.contains("fields[3] !== '0A'"));
        assert!(scanner.contains("'-sTCP:LISTEN'"));
        let attribution =
            include_str!("../../../vendor/orca/src/main/ports/local-workspace-port-attribution.ts");
        assert!(attribution
            .contains("const startsOnBoundary = before === '' || /\\s|[\"'=]/.test(before)"));
        assert!(attribution
            .contains("const HTTPS_PORTS: Record<number, true> = { 443: true, 8443: true }"));
    }

    #[test]
    fn lsof_output_parses() {
        let out =
            "p4242\ncnode\nf12\nn127.0.0.1:5173\nf13\nn[::1]:5173\np7\ncpython3\nf3\nn*:8000\n";
        let ports = parse_lsof(out);
        assert_eq!(ports.len(), 3);
        assert_eq!((ports[2].host.as_str(), ports[2].port), ("*", 8000));
        assert_eq!(connect_host("*"), "localhost");
    }

    #[test]
    fn attribution_prefers_the_deepest_worktree_and_path_boundaries() {
        let trees = vec![
            ("a".to_owned(), "/r/.branchyard/worktrees/a".to_owned()),
            ("ab".to_owned(), "/r/.branchyard/worktrees/ab".to_owned()),
        ];
        let mut cache = HashMap::new();
        let raw = |cwd: Option<&str>, command: Option<&str>| Raw {
            host: "127.0.0.1".into(),
            port: 3000,
            cwd: cwd.map(str::to_owned),
            command: command.map(str::to_owned),
            ..Raw::default()
        };
        let who = |r: &Raw, cache: &mut HashMap<u32, Process>| owner(r, &trees, "/r", cache);
        assert_eq!(
            who(
                &raw(Some("/r/.branchyard/worktrees/ab/web"), None),
                &mut cache
            ),
            Some(("ab".into(), "cwd"))
        );
        assert_eq!(
            who(
                &raw(
                    Some("/elsewhere"),
                    Some("node /r/.branchyard/worktrees/a/server.js")
                ),
                &mut cache
            ),
            Some(("a".into(), "command"))
        );
        assert_eq!(
            who(
                &raw(None, Some("node /r/.branchyard/worktrees/abc/x.js")),
                &mut cache
            ),
            None
        );
        assert!(names_path("vite --root=/w/a", "/w/a"));
        assert!(!names_path("vite /w/ab", "/w/a"));
    }
}
