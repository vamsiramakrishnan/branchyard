//! The bridge: follows a repository's activity feed and keeps one Herdr
//! pane per branch, reporting each branch's state on it.
//!
//! A thread reads the feed ([`branchyard_client::EventStream`]): on start
//! it opens the stream, lists the branches, then applies each entry after
//! the stream's starting cursor, so nothing between the listing and the
//! stream is missed. When the stream drops (a server restart), it
//! reconnects after the last cursor it delivered. The main thread applies
//! entries, and reports at most once per debounce interval, only the
//! branches whose report changed.

use branchyard_support::best_effort;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use branchyard::BranchInfo;
use branchyard_client::api::FeedEntry;
use branchyard_client::{Client, Error, Repo};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::herdr::{Herdr, Opened};
use crate::model::{Branches, Report};

/// What the feed thread tells the main thread.
enum Feed {
    /// The branches as listed after the stream opened.
    Snapshot(Vec<BranchInfo>),
    Entry(FeedEntry),
    /// The server refused the stream for good.
    Fatal(String),
}

/// The branch-to-pane map, kept in the plugin's state directory so a
/// restarted bridge reuses its panes.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PaneMap {
    /// The server and repository the panes belong to.
    pub server: String,
    pub panes: BTreeMap<String, PaneEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneEntry {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

impl PaneMap {
    pub fn path(config: &Config) -> PathBuf {
        config.state_dir().join("panes.json")
    }

    pub fn load(config: &Config) -> PaneMap {
        std::fs::read_to_string(PaneMap::path(config))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, config: &Config) {
        let path = PaneMap::path(config);
        let write = || -> std::io::Result<()> {
            let dir = path
                .parent()
                .ok_or_else(|| std::io::Error::other("the pane map path has no directory"))?;
            std::fs::create_dir_all(dir)?;
            let temp = path.with_extension("json.tmp");
            std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(temp, &path)
        };
        if let Err(e) = write() {
            tracing::error!(path = %path.display(), error = %e, "cannot save the pane map");
        }
    }

    /// The branch shown in `pane_id`.
    pub fn branch_of(&self, pane_id: &str) -> Option<&str> {
        self.panes
            .iter()
            .find(|(_, entry)| entry.pane_id == pane_id)
            .map(|(branch, _)| branch.as_str())
    }
}

/// Connect as configured, choosing the server's only repository when none
/// is named, as `by --remote` does.
pub fn connect(config: &Config) -> Result<Repo, String> {
    let mut client = Client::from_token_file(config.remote()?, config.token_file()?)
        .map_err(|e| e.to_string())?;
    if let Some(ca) = config.get("BRANCHYARD_CA_FILE") {
        client = client.with_ca_file(ca).map_err(|e| e.to_string())?;
    }
    let name = match config.get("BRANCHYARD_REPO") {
        Some(name) => name.to_owned(),
        None => {
            let repos = client.repos().map_err(|e| e.to_string())?;
            match repos.as_slice() {
                [only] => only.name.clone(),
                _ => {
                    return Err(
                        "the server serves several repositories, or none; set BRANCHYARD_REPO"
                            .into(),
                    )
                }
            }
        }
    };
    Ok(client.repo(&name))
}

pub fn run(config: &Config) -> Result<(), String> {
    let repo = connect(config)?;
    let server = format!("{} {}", repo.client().endpoint(), repo.name());
    tracing::info!(%server, "following");
    let (tx, rx) = mpsc::channel();
    {
        let repo = repo.clone();
        thread::spawn(move || follow(repo, tx));
    }
    let mut bridge = Bridge::new(config, server);
    let debounce = config.debounce();
    let mut due: Option<Instant> = None;
    loop {
        let wait = match due {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(wait) {
            Ok(Feed::Snapshot(listed)) => {
                let names = bridge.branches.snapshot(&listed);
                bridge.dirty.extend(names);
            }
            Ok(Feed::Entry(entry)) => {
                if bridge.branches.apply(&entry.branch, &entry.activity) {
                    bridge.dirty.insert(entry.branch);
                }
            }
            Ok(Feed::Fatal(message)) => return Err(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the feed thread stopped".into())
            }
        }
        if bridge.dirty.is_empty() {
            due = None;
            continue;
        }
        let at = *due.get_or_insert_with(|| Instant::now() + debounce);
        if Instant::now() >= at {
            bridge.flush();
            due = None;
        }
    }
}

/// Read the feed forever, reconnecting after the last delivered cursor.
#[allow(clippy::let_underscore_must_use)] // ratchet: branchyard-herdr
fn follow(repo: Repo, tx: mpsc::Sender<Feed>) {
    let mut cursor: Option<u64> = None;
    let mut snapshot = true;
    let mut failures = 0u32;
    // 250 ms, doubling, at most 5 s; started afresh after an entry arrives.
    let mut delays = branchyard_client::reconnect_backoff();
    loop {
        if failures > 0 {
            if let Some(delay) = delays.next() {
                thread::sleep(delay);
            }
        }
        // Each drop comes back here, so this thread chooses the backoff
        // and says what it resumes from.
        let mut stream = repo.stream(cursor).max_failures(0);
        if snapshot {
            let listed = stream
                .open()
                .and_then(|start| Ok((start, repo.branches()?)));
            match listed {
                Ok((start, branches)) => {
                    tracing::info!(
                        branches = branches.len(),
                        cursor = start,
                        "listed branches; following from cursor"
                    );
                    cursor = Some(start);
                    snapshot = false;
                    if tx.send(Feed::Snapshot(branches)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    if refused(&e) {
                        let _ = tx.send(Feed::Fatal(e.to_string()));
                        return;
                    }
                    failures += 1;
                    tracing::warn!(error = %e, "retrying");
                    continue;
                }
            }
        } else {
            tracing::info!(cursor = cursor.unwrap_or(0), "reconnecting");
        }
        for item in stream.by_ref() {
            match item {
                Ok(entry) => {
                    failures = 0;
                    delays = branchyard_client::reconnect_backoff();
                    cursor = Some(entry.seq);
                    if tx.send(Feed::Entry(entry)).is_err() {
                        return;
                    }
                }
                Err(e) if e.code() == Some("cursor_out_of_range") => {
                    // The server's feed was reset: start over from a listing.
                    tracing::warn!(error = %e, "listing branches again");
                    snapshot = true;
                    cursor = None;
                }
                Err(e) if refused(&e) => {
                    let _ = tx.send(Feed::Fatal(e.to_string()));
                    return;
                }
                Err(e) => tracing::error!(error = %e, "event stream lost"),
            }
        }
        failures += 1;
    }
}

/// Errors retrying cannot fix: a wrong token or an unknown repository.
fn refused(error: &Error) -> bool {
    matches!(error, Error::Api { status, error } if *status < 500 && error.code != "cursor_out_of_range")
}

struct Bridge<'a> {
    config: &'a Config,
    herdr: Herdr,
    branches: Branches,
    panes: PaneMap,
    /// Panes from the saved map not yet checked with `herdr pane get`.
    unchecked: BTreeSet<String>,
    /// What each branch's pane last showed.
    reported: BTreeMap<String, Report>,
    dirty: BTreeSet<String>,
    seq: u64,
}

impl<'a> Bridge<'a> {
    fn new(config: &'a Config, server: String) -> Bridge<'a> {
        let mut panes = PaneMap::load(config);
        if panes.server != server {
            panes = PaneMap {
                server,
                panes: BTreeMap::new(),
            };
        }
        let unchecked = panes.panes.keys().cloned().collect();
        Bridge {
            config,
            herdr: Herdr::new(config.herdr()),
            branches: Branches::default(),
            panes,
            unchecked,
            reported: BTreeMap::new(),
            dirty: BTreeSet::new(),
            seq: 0,
        }
    }

    /// Strictly increasing across restarts, as Herdr's `--seq` needs:
    /// milliseconds since the epoch, or one more than the last.
    fn next_seq(&mut self) -> u64 {
        let now = branchyard_support::time::now_ms();
        self.seq = now.max(self.seq + 1);
        self.seq
    }

    fn flush(&mut self) {
        let dirty = std::mem::take(&mut self.dirty);
        let mut changed = false;
        for branch in dirty {
            let Some(tracked) = self.branches.branches.get(&branch) else {
                continue;
            };
            let report = tracked.report();
            if self.reported.get(&branch) == Some(&report) {
                continue;
            }
            // A branch merged before the bridge ever showed it needs no pane.
            if !self.panes.panes.contains_key(&branch)
                && matches!(tracked.status, branchyard::BranchStatus::Merged { .. })
            {
                continue;
            }
            match self.show(&branch, &report, &mut changed) {
                Ok(()) => {
                    tracing::info!(
                        %branch,
                        state = report.state.as_str(),
                        message = report.message.as_deref().unwrap_or(""),
                        "reported"
                    );
                    self.reported.insert(branch, report);
                }
                Err(e) => tracing::error!(%branch, error = %e, "reporting"),
            }
        }
        if changed {
            self.panes.save(self.config);
        }
    }

    /// Report on the branch's pane, opening one if it has none or its pane
    /// was closed.
    fn show(&mut self, branch: &str, report: &Report, changed: &mut bool) -> Result<(), String> {
        if self.unchecked.remove(branch) {
            let pane = &self.panes.panes[branch].pane_id;
            if !self.herdr.pane_exists(pane).map_err(|e| e.to_string())? {
                self.panes.panes.remove(branch);
                *changed = true;
            }
        }
        for attempt in 0..2 {
            let pane = match self.panes.panes.get(branch) {
                Some(entry) => entry.pane_id.clone(),
                None => {
                    let opened = self.open(branch)?;
                    self.panes.panes.insert(
                        branch.to_owned(),
                        PaneEntry {
                            pane_id: opened.pane_id.clone(),
                            tab_id: opened.tab_id,
                        },
                    );
                    *changed = true;
                    opened.pane_id
                }
            };
            let seq = self.next_seq();
            match self
                .herdr
                .report(&pane, report.state.as_str(), report.message.as_deref(), seq)
            {
                Ok(()) => return Ok(()),
                // Closed since: open another once.
                Err(e) if e.code.as_deref() == Some("pane_not_found") && attempt == 0 => {
                    self.panes.panes.remove(branch);
                    *changed = true;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        unreachable!("the second attempt returns")
    }

    /// A new tab running `by log --follow <branch>`, named after the branch.
    fn open(&self, branch: &str) -> Result<Opened, String> {
        let mut env = vec![("BRANCHYARD_HERDR_BRANCH".to_owned(), branch.to_owned())];
        env.extend(self.config.remote_env());
        let opened = self
            .herdr
            .open_tab(
                self.config.plugin_id(),
                "log",
                self.config.workspace(),
                &env,
            )
            .map_err(|e| e.to_string())?;
        let label = format!("by: {branch}");
        best_effort(
            "herdr.call",
            self.herdr.call(&[
                "pane".into(),
                "rename".into(),
                opened.pane_id.clone(),
                label.clone(),
            ]),
        );
        if let Some(tab) = &opened.tab_id {
            best_effort(
                "herdr.call",
                self.herdr
                    .call(&["tab".into(), "rename".into(), tab.clone(), label]),
            );
        }
        Ok(opened)
    }
}
