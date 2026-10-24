//! Per-harness installs: `plugins/branchyard/install/<id>.toml`, one
//! manifest per harness, saying how to detect it (`binaries` on `PATH`),
//! what it gets, and which packaged files go where:
//!
//! ```toml
//! harness = "codex"
//! binaries = ["codex"]
//! provides = ["hooks", "skills"]
//! [[file]]
//! from = "harnesses/codex/hooks.json"   # relative to plugins/branchyard
//! to = "~/.codex/hooks.json"            # ~ = home; {project} = repository root
//! how = "merge-json"                    # write | merge-json | merge-toml | merge-yaml
//! at = ""                               # merge-*: dotted key to merge under
//! [[note]]
//! text = "Open Codex and run /hooks to trust the Branchyard hooks."
//! ```
//!
//! The whole of `plugins/branchyard` is embedded at build time
//! (`build.rs`), so an installed `by` needs no checkout. `by init plugin`
//! turns the chosen manifests into an ordinary plan: `write` files are
//! diffed against what exists (and replacing a different file needs
//! consent: `--force`, or the wizard's question); `merge-*` files are the
//! person's file with the fragment merged in ([`merge`]), shown as a diff,
//! and a merge that would have to replace anything is refused.

pub mod merge;
mod yaml;

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::plan::{ArtifactKind, PlannedFile};
use crate::probe::{harness_label, Facts, Probe};

pub use yaml::merge as merge_yaml;

mod bundle {
    include!(concat!(env!("OUT_DIR"), "/bundle.rs"));
}

/// A plugin directory's text files, by path relative to it: the one built
/// into `by` ([`BUILT`]), or a test's fixtures.
#[derive(Clone, Copy, Debug)]
pub struct Bundle<'a>(pub &'a [(&'a str, &'a str)]);

/// Every text file of `plugins/branchyard`, embedded when `by` was built.
pub const BUILT: Bundle<'static> = Bundle(bundle::FILES);

impl<'a> Bundle<'a> {
    /// One file.
    pub fn file(&self, path: &str) -> Option<&'a str> {
        self.0
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, text)| *text)
    }

    /// The files under the directory `dir`, by path relative to it.
    pub fn dir(&self, dir: &str) -> Vec<(&'a str, &'a str)> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.0
            .iter()
            .filter_map(|(p, text)| p.strip_prefix(prefix.as_str()).map(|rest| (rest, *text)))
            .collect()
    }

    /// Every manifest under `install/`, by harness id, each checked
    /// ([`Bundle::check`]).
    pub fn manifests(&self) -> Result<Vec<Manifest>, String> {
        let mut out = Vec::new();
        for (path, text) in self.0 {
            let Some(id) = path
                .strip_prefix("install/")
                .and_then(|name| name.strip_suffix(".toml"))
                .filter(|id| !id.contains('/'))
            else {
                continue;
            };
            let manifest: Manifest = toml::from_str(text).map_err(|e| format!("{path}: {e}"))?;
            let problems = self.check(&manifest, id);
            if !problems.is_empty() {
                return Err(format!("{path}: {}", problems.join("; ")));
            }
            out.push(manifest);
        }
        out.sort_by(|a, b| a.harness.cmp(&b.harness));
        Ok(out)
    }
}

/// How a file is installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum How {
    /// Write the file (or every file under a directory) as packaged.
    #[default]
    Write,
    /// Merge a JSON fragment into the target ([`merge::json`]).
    MergeJson,
    /// Merge a TOML fragment, keeping formatting ([`merge::toml`]).
    MergeToml,
    /// Merge a block-YAML fragment by indentation ([`merge_yaml`]).
    MergeYaml,
}

impl How {
    /// The name the manifest uses: `write`, `merge-json`, ...
    pub fn label(self) -> &'static str {
        match self {
            How::Write => "write",
            How::MergeJson => "merge-json",
            How::MergeToml => "merge-toml",
            How::MergeYaml => "merge-yaml",
        }
    }
}

/// One `[[file]]` of a manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InstallFile {
    /// Relative to `plugins/branchyard`; a directory installs every file
    /// under it.
    pub from: String,
    /// `~/...` under the home directory, `{project}/...` under the
    /// repository root, or absolute.
    pub to: String,
    /// How it is installed; `write` when left out.
    #[serde(default)]
    pub how: How,
    /// For `merge-*`: the dotted key the fragment merges under.
    #[serde(default)]
    pub at: String,
}

/// A `[[note]]`: something only the person can do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Note {
    /// Printed with the plan and after writing.
    pub text: String,
}

/// `plugins/branchyard/install/<harness>.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The harness's catalog id; the file is named for it.
    pub harness: String,
    /// Detected when any of these is on `PATH`.
    pub binaries: Vec<String>,
    /// What the install gives: hooks, commands, skills, statusline...
    #[serde(default)]
    pub provides: Vec<String>,
    /// The `[[file]]`s, installed in order.
    #[serde(default, rename = "file")]
    pub files: Vec<InstallFile>,
    /// The `[[note]]`s.
    #[serde(default, rename = "note")]
    pub notes: Vec<Note>,
}

/// Every manifest built into `by`, by harness id.
pub fn manifests() -> Result<Vec<Manifest>, String> {
    BUILT.manifests()
}

/// The manifest built into `by` for `harness`.
pub fn manifest(harness: &str) -> Result<Option<Manifest>, String> {
    Ok(manifests()?.into_iter().find(|m| m.harness == harness))
}

impl Bundle<'_> {
    /// What is wrong with a manifest named `<id>.toml` in this bundle.
    pub fn check(&self, manifest: &Manifest, id: &str) -> Vec<String> {
        let mut problems = Vec::new();
        if manifest.harness != id {
            problems.push(format!(
                "harness is {:?} but the file is named for {id:?}",
                manifest.harness
            ));
        }
        if manifest.binaries.is_empty() || manifest.binaries.iter().any(|b| b.trim().is_empty()) {
            problems.push("binaries must name at least one program".into());
        }
        for file in &manifest.files {
            let is_dir = !self.dir(&file.from).is_empty();
            match self.file(&file.from) {
                Some(text) => {
                    if let Err(e) = fragment_parses(file.how, text) {
                        problems.push(format!("{}: {e}", file.from));
                    }
                }
                None if is_dir && file.how == How::Write => {}
                None if is_dir => {
                    problems.push(format!("{}: a directory can only be written", file.from))
                }
                None => problems.push(format!(
                    "{}: no such file under plugins/branchyard",
                    file.from
                )),
            }
            let rooted = ["~/", "{project}/", "/"]
                .iter()
                .any(|p| file.to.starts_with(p));
            if !rooted {
                problems.push(format!(
                    "{}: `to` must start with ~/, {{project}}/ or /",
                    file.to
                ));
            }
            if file.how == How::Write && !file.at.is_empty() {
                problems.push(format!("{}: `at` is only for merge-*", file.to));
            }
        }
        for note in &manifest.notes {
            if note.text.trim().is_empty() {
                problems.push("a [[note]] has no text".into());
            }
        }
        problems
    }
}

/// Whether a packaged fragment is in its merge's own format.
fn fragment_parses(how: How, text: &str) -> Result<(), String> {
    match how {
        How::Write => Ok(()),
        How::MergeJson => serde_json::from_str::<serde_json::Value>(text)
            .map(|_| ())
            .map_err(|e| format!("not JSON: {e}")),
        How::MergeToml => text
            .parse::<toml_edit::DocumentMut>()
            .map(|_| ())
            .map_err(|e| format!("not TOML: {e}")),
        How::MergeYaml => yaml::merge(None, text, "").map(|_| ()),
    }
}

/// The ids of the manifests whose binaries are on `PATH`.
pub fn detect(probe: &dyn Probe) -> Vec<String> {
    manifests()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.binaries.iter().any(|b| probe.on_path(b)))
        .map(|m| m.harness)
        .collect()
}

/// One file a harness's install touches, as listed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ListedFile {
    /// Where it goes, `~` and `{project}` expanded.
    pub path: String,
    /// How it is installed.
    pub how: How,
    /// For a merge: the dotted key it merges under, when not the top level.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    /// What it installs, under `plugins/branchyard`.
    pub from: String,
}

/// A harness `by init plugin` can install for, as `by init plugin`
/// (no harnesses named) lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessInstall {
    /// The catalog id, as `by init plugin ID` takes it.
    pub harness: String,
    /// Its display name.
    pub label: String,
    /// One of its binaries is on `PATH`.
    pub detected: bool,
    /// The programs looked for on `PATH`.
    pub binaries: Vec<String>,
    /// What the install gives: hooks, commands, skills, statusline...
    pub provides: Vec<String>,
    /// Every file it writes or merges into.
    pub files: Vec<ListedFile>,
    /// What only the person can do afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// `by init plugin --json`: every installable harness, detected first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessList {
    /// [`crate::PROTOCOL`].
    pub protocol: String,
    /// Detected harnesses first, then the rest, each by id.
    pub harnesses: Vec<HarnessInstall>,
}

/// Every manifest's harness, detected ones first.
pub fn listing(facts: &Facts) -> Result<HarnessList, String> {
    let mut harnesses: Vec<HarnessInstall> = manifests()?
        .into_iter()
        .map(|m| HarnessInstall {
            label: harness_label(&m.harness),
            detected: facts.install_detected.contains(&m.harness),
            files: m
                .files
                .iter()
                .map(|f| ListedFile {
                    path: expand(&f.to, facts),
                    how: f.how,
                    at: f.at.clone(),
                    from: f.from.clone(),
                })
                .collect(),
            notes: m.notes.iter().map(|n| n.text.clone()).collect(),
            harness: m.harness,
            binaries: m.binaries,
            provides: m.provides,
        })
        .collect();
    harnesses.sort_by_key(|h| !h.detected);
    Ok(HarnessList {
        protocol: crate::PROTOCOL.into(),
        harnesses,
    })
}

/// `to` as a plan path: `~/` under the home directory, `{project}/`
/// relative to the repository root.
pub fn expand(to: &str, facts: &Facts) -> String {
    if let Some(rest) = to.strip_prefix("~/") {
        let home = facts.home.clone().unwrap_or_else(|| "~".into());
        return format!("{}/{rest}", home.trim_end_matches('/'));
    }
    if let Some(rest) = to.strip_prefix("{project}/") {
        return rest.to_owned();
    }
    to.to_owned()
}

/// What a file is, for its validator.
fn kind_of(path: &str) -> ArtifactKind {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == "SKILL.md" {
        return ArtifactKind::Skill;
    }
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("json" | "toml" | "yaml" | "yml") => ArtifactKind::HarnessConfig,
        _ => ArtifactKind::Text,
    }
}

/// The files of `manifests` as a plan's files, each diffed against what
/// exists. Two files of the install that land on one path become one: a
/// merge merges into what the earlier one planned.
pub fn planned_files(manifests: &[Manifest], facts: &Facts, probe: &dyn Probe) -> Vec<PlannedFile> {
    planned_files_from(BUILT, manifests, facts, probe)
}

/// [`planned_files`] from another bundle (a test's fixtures).
pub fn planned_files_from(
    bundle: Bundle,
    manifests: &[Manifest],
    facts: &Facts,
    probe: &dyn Probe,
) -> Vec<PlannedFile> {
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut originals: BTreeMap<String, Option<String>> = BTreeMap::new();
    for manifest in manifests {
        for f in &manifest.files {
            let target = expand(&f.to, facts);
            let entries: Vec<(String, Result<String, String>)> = match (f.how, bundle.file(&f.from))
            {
                (How::Write, Some(body)) => vec![(target, Ok(body.to_owned()))],
                (How::Write, None) if !bundle.dir(&f.from).is_empty() => bundle
                    .dir(&f.from)
                    .into_iter()
                    .map(|(rest, body)| (crate::topics::join(&target, rest), Ok(body.to_owned())))
                    .collect(),
                (how, Some(fragment)) => {
                    let existing = match files.iter().find(|p| p.path == target) {
                        Some(p) if p.refusal.is_some() => continue,
                        Some(p) => Some(p.body().to_owned()),
                        None => probe.read(&target),
                    };
                    let merged = match how {
                        How::MergeJson => merge::json(existing.as_deref(), fragment, &f.at),
                        How::MergeToml => merge::toml(existing.as_deref(), fragment, &f.at),
                        _ => yaml::merge(existing.as_deref(), fragment, &f.at),
                    };
                    vec![(target, merged)]
                }
                (_, None) => vec![(
                    target,
                    Err(format!("{} is not in this build of by", f.from)),
                )],
            };
            let how = match f.at.as_str() {
                "" => f.how.label().to_owned(),
                at => format!("{} at {at}", f.how.label()),
            };
            for (path, result) in entries {
                let original = originals
                    .entry(path.clone())
                    .or_insert_with(|| probe.read(&path))
                    .clone();
                let file = match result {
                    Ok(body) => PlannedFile::installed(&path, kind_of(&path), body, original, &how),
                    Err(why) => PlannedFile::refused(&path, kind_of(&path), &how, why),
                };
                match files.iter().position(|p| p.path == path) {
                    Some(i) if files[i].refusal.is_some() => {}
                    Some(i) => files[i] = file,
                    None => files.push(file),
                }
            }
        }
    }
    files
}
