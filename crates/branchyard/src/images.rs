//! Toolchain images: sandbox images that hold harness CLIs at pinned
//! versions, say so in their labels, and are named by digest.
//!
//! `by images build` writes a build context from the harness catalog's
//! pinned install commands ([`crate::inventory::plan`]) and labels the image
//! ([`ImageLabels`]); [`ImageStore`] records what was built in
//! `.branchyard/images.json`. A label source ([`ImageLabelSource`]) reads an
//! image's labels without starting it, so routing ([`judge`]) can tell what
//! an image holds, and a turn's manifest can record the toolchain it was
//! admitted with. See `docs/images.md`.
//!
//! Nothing here holds a credential: the Dockerfile and the context are
//! written from the catalog and the labels alone.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use branchyard_support::LockExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::inventory::{self, InstallAction};

/// The prefix of every label this module writes.
pub const LABEL_PREFIX: &str = "dev.branchyard.";
/// `dev.branchyard.harness.<id>=<version>`: a harness installed in the image.
pub const LABEL_HARNESS: &str = "dev.branchyard.harness.";
/// The suffix of `dev.branchyard.harness.<id>.evidence=hermetic|live|catalog`.
pub const LABEL_EVIDENCE_SUFFIX: &str = ".evidence";
/// `dev.branchyard.by=<version>`: the `by` binary in the image.
pub const LABEL_BY: &str = "dev.branchyard.by";
/// `dev.branchyard.anvil=<digest>`: Anvil's toolchain under `/opt/anvil`.
pub const LABEL_ANVIL: &str = "dev.branchyard.anvil";
/// `dev.branchyard.toolchain=sha256:<hex>`: the hash of the other labels.
pub const LABEL_TOOLCHAIN: &str = "dev.branchyard.toolchain";
/// The version label of a harness installed by a script that takes none.
pub const UNPINNED: &str = "unpinned";
/// The default base image of a build: Node with npm, on Debian.
pub const DEFAULT_BASE: &str = "docker.io/library/node:22-bookworm-slim";
/// Where the toolchain image keeps Anvil.
pub const ANVIL_PREFIX: &str = "/opt/anvil";
/// The record file, relative to a repository's root.
pub const IMAGES_FILE: &str = ".branchyard/images.json";

/// The provider kinds an image can be built for (their serialized names).
pub const PROVIDERS: [&str; 4] = ["microsandbox", "container", "agent_sandbox", "substrate"];

/// Normalize a provider name typed on a command line (`agent-sandbox`) to
/// its serialized kind (`agent_sandbox`); `None` for one that runs no image.
pub fn provider_kind(name: &str) -> Option<&'static str> {
    let wanted = name.replace('-', "_");
    PROVIDERS.into_iter().find(|kind| *kind == wanted)
}

// ---------------------------------------------------------------------------
// Labels

/// What an image says it holds: the harness versions, the `by` binary and
/// Anvil's toolchain, and the hash of those (`dev.branchyard.toolchain`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageLabels {
    /// Harness catalog ID to version, or [`UNPINNED`].
    pub harnesses: BTreeMap<String, String>,
    /// Harness catalog ID to where its version came from: `live`,
    /// `hermetic` or `catalog` (see [`pinned_version`]). Absent for an image
    /// built before the evidence label existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<String, String>,
    /// The `by` version in the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The digest of Anvil's installed toolchain, when the image has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anvil: Option<String>,
}

impl ImageLabels {
    /// The labels except the toolchain hash, sorted by key.
    pub fn parts(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        for (id, version) in &self.harnesses {
            labels.insert(format!("{LABEL_HARNESS}{id}"), version.clone());
        }
        for (id, evidence) in &self.evidence {
            labels.insert(
                format!("{LABEL_HARNESS}{id}{LABEL_EVIDENCE_SUFFIX}"),
                evidence.clone(),
            );
        }
        if let Some(by) = &self.by {
            labels.insert(LABEL_BY.to_owned(), by.clone());
        }
        if let Some(anvil) = &self.anvil {
            labels.insert(LABEL_ANVIL.to_owned(), anvil.clone());
        }
        labels
    }

    /// `sha256:` and the hex SHA-256 of the sorted label set, one
    /// `key=value` line each, without the toolchain label itself.
    pub fn toolchain(&self) -> String {
        toolchain_of(&self.parts())
    }

    /// Every label the image carries, the toolchain hash included.
    pub fn all(&self) -> BTreeMap<String, String> {
        let mut labels = self.parts();
        labels.insert(LABEL_TOOLCHAIN.to_owned(), self.toolchain());
        labels
    }

    /// The labels of an image from its raw label map. `Ok(None)` when it
    /// carries no `dev.branchyard.` label at all (not a toolchain image);
    /// an error when the toolchain label does not match the labels beside
    /// it, which means they were edited.
    pub fn from_map(map: &BTreeMap<String, String>) -> Result<Option<ImageLabels>, String> {
        if !map.keys().any(|key| key.starts_with(LABEL_PREFIX)) {
            return Ok(None);
        }
        let mut labels = ImageLabels::default();
        for (key, value) in map {
            if let Some(id) = key
                .strip_prefix(LABEL_HARNESS)
                .and_then(|rest| rest.strip_suffix(LABEL_EVIDENCE_SUFFIX))
            {
                labels.evidence.insert(id.to_owned(), value.clone());
            } else if let Some(id) = key.strip_prefix(LABEL_HARNESS) {
                labels.harnesses.insert(id.to_owned(), value.clone());
            } else if key == LABEL_BY {
                labels.by = Some(value.clone());
            } else if key == LABEL_ANVIL {
                labels.anvil = Some(value.clone());
            }
        }
        if let Some(stated) = map.get(LABEL_TOOLCHAIN) {
            let computed = labels.toolchain();
            if *stated != computed {
                return Err(format!(
                    "its {LABEL_TOOLCHAIN} label is {stated}, but its other dev.branchyard \
                     labels hash to {computed}; they were changed after the build"
                ));
            }
        }
        Ok(Some(labels))
    }

    /// The version of `harness` the image holds, or `None`.
    pub fn holds(&self, harness: &str) -> Option<&str> {
        self.harnesses.get(harness).map(String::as_str)
    }

    /// `id version` for each harness, comma-separated, for a sentence.
    pub fn held(&self) -> String {
        match self.harnesses.is_empty() {
            true => "no harness".to_owned(),
            false => self
                .harnesses
                .iter()
                .map(|(id, version)| format!("{id} {version}"))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

fn toolchain_of(parts: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    for (key, value) in parts {
        hash.update(format!("{key}={value}\n").as_bytes());
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
}

// ---------------------------------------------------------------------------
// References

/// The `sha256:...` digest an image reference names, if it is pinned.
pub fn digest_of(reference: &str) -> Option<&str> {
    reference
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .filter(|digest| digest.starts_with("sha256:"))
}

/// An OCI reference split into the parts a registry is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    /// The registry's host and port, as it is reached.
    pub registry: String,
    /// The repository path, with `library/` added for a Docker Hub name.
    pub repository: String,
    /// What to fetch: the digest when pinned, else the tag (`latest`).
    pub reference: String,
}

impl ImageRef {
    /// Split `text` (`[host[:port]/]path[:tag][@sha256:...]`).
    pub fn parse(text: &str) -> Result<ImageRef, String> {
        let (name, digest) = match text.rsplit_once('@') {
            Some((name, digest)) => (name, Some(digest)),
            None => (text, None),
        };
        let (host, rest) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (Some(first), rest)
            }
            _ => (None, name),
        };
        let (path, tag) = match rest.rsplit_once(':') {
            Some((path, tag)) if !tag.contains('/') => (path, Some(tag)),
            _ => (rest, None),
        };
        if path.is_empty() || path.chars().any(char::is_whitespace) || path.starts_with('-') {
            return Err(format!("{text:?} is not an image reference"));
        }
        let registry = match host {
            None | Some("docker.io") | Some("index.docker.io") => "registry-1.docker.io".to_owned(),
            Some(host) => host.to_owned(),
        };
        let hub = registry == "registry-1.docker.io";
        let repository = match (hub, path.contains('/')) {
            (true, false) => format!("library/{path}"),
            _ => path.to_owned(),
        };
        Ok(ImageRef {
            registry,
            repository,
            reference: digest.or(tag).unwrap_or("latest").to_owned(),
        })
    }

    /// `http://` for a loopback registry, `https://` for any other.
    pub fn base_url(&self) -> String {
        let host = self.registry.split(':').next().unwrap_or_default();
        let local = host == "localhost" || host.starts_with("127.") || host == "[::1]";
        match local {
            true => format!("http://{}", self.registry),
            false => format!("https://{}", self.registry),
        }
    }
}

// ---------------------------------------------------------------------------
// Records

/// One image `by images build` made or `by images pin` adopted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRecord {
    /// `tag@digest` when the digest is known, else the tag alone.
    pub reference: String,
    /// The tag it was built under.
    pub tag: String,
    /// The registry digest (`sha256:...`); absent for a local build never pushed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The local image ID Docker reported, when the image was loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_id: Option<String>,
    /// The provider it was built for (`microsandbox`, `container`, ...).
    pub provider: String,
    /// Every label on the image.
    pub labels: BTreeMap<String, String>,
    /// When it was recorded, as RFC 3339.
    pub created: String,
    /// Whether it was pushed to its registry.
    #[serde(default)]
    pub pushed: bool,
}

impl ImageRecord {
    /// The record's labels as [`ImageLabels`], when they are Branchyard's.
    pub fn toolchain(&self) -> Option<ImageLabels> {
        ImageLabels::from_map(&self.labels).ok().flatten()
    }

    /// Whether `query` names this record: its reference, its tag, its digest
    /// or the reference without the digest.
    pub fn matches(&self, query: &str) -> bool {
        self.reference == query
            || self.tag == query
            || self.digest.as_deref() == Some(query)
            || digest_of(query).is_some_and(|d| self.digest.as_deref() == Some(d))
    }
}

#[derive(Default, Serialize, Deserialize)]
struct RecordFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    images: Vec<ImageRecord>,
}

/// `.branchyard/images.json` of one repository.
#[derive(Clone, Debug)]
pub struct ImageStore {
    path: PathBuf,
}

impl ImageStore {
    /// The store of the repository rooted at `root`.
    pub fn new(root: &Path) -> ImageStore {
        ImageStore {
            path: root.join(IMAGES_FILE),
        }
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every record, oldest first. A missing file has none.
    pub fn list(&self) -> Result<Vec<ImageRecord>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<RecordFile>(&bytes)
                .map(|file| file.images)
                .map_err(|e| format!("{} is not an image record file: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    /// The newest record `query` names.
    pub fn find(&self, query: &str) -> Result<Option<ImageRecord>, String> {
        Ok(self
            .list()?
            .into_iter()
            .rev()
            .find(|record| record.matches(query)))
    }

    /// Add `record`, replacing an earlier one with the same reference.
    pub fn add(&self, record: ImageRecord) -> Result<(), String> {
        let mut images = self.list()?;
        images.retain(|old| old.reference != record.reference);
        images.push(record);
        let file = RecordFile { version: 1, images };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        crate::connectors::keys::write_atomic(&self.path, &bytes, 0o644)
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

// ---------------------------------------------------------------------------
// The build context

/// One harness to install in the image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessPin {
    /// The catalog ID.
    pub id: String,
    /// The shell command the image runs.
    pub command: String,
    /// The pinned version, or [`UNPINNED`].
    pub version: String,
    /// Where the version came from: `live`, `hermetic` or `catalog`; for an
    /// unpinned install, `catalog`.
    pub evidence: String,
    /// Why it is not pinned, when it is not.
    pub unpinned: Option<String>,
}

/// A version an image pins a harness to, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedVersion {
    /// The release: `0.170.0`.
    pub version: String,
    /// `live` or `hermetic` for a qualified pin (by its record), `catalog`
    /// for the version the default profile was checked against.
    pub evidence: String,
}

/// The version an image pins harness `id` to. This one function decides the
/// order: the qualified pin (`qualified_pin(id)`, written by `by harnesses
/// update ID --qualify`) wins, labelled with its evidence; else the version
/// the harness's default profile was checked against, labelled `catalog`;
/// else `None`.
pub fn pinned_version(id: &str) -> Option<PinnedVersion> {
    pinned_version_from(crate::qualified::qualified_pin(id), id)
}

fn pinned_version_from(
    qualified: Option<crate::qualified::QualifiedPin>,
    id: &str,
) -> Option<PinnedVersion> {
    match qualified {
        Some(pin) => Some(PinnedVersion {
            evidence: pin.evidence().to_owned(),
            version: pin.version,
        }),
        None => inventory::checked_version(id).map(|version| PinnedVersion {
            version,
            evidence: "catalog".to_owned(),
        }),
    }
}

/// The pinned install of harness `id`, from the catalog: the first command a
/// plain Node image can run (`npm`), pinned to the version its default
/// profile was checked against. A harness that only a piped installer can
/// install, or whose version is not recorded, is refused unless
/// `allow_unpinned`; it is then installed by that script and labelled
/// [`UNPINNED`].
pub fn pin_harness(id: &str, allow_unpinned: bool) -> Result<HarnessPin, String> {
    let tools = |names: &[&str]| -> BTreeMap<String, String> {
        names
            .iter()
            .map(|name| ((*name).to_owned(), (*name).to_owned()))
            .collect()
    };
    let narrow = inventory::plan(id, InstallAction::Install, None, &tools(&["npm"]));
    if let Ok(plan) = &narrow {
        // Rebuild the command for the version the order picks, which may be
        // a qualified pin newer than the catalog's.
        if let Some(chosen) = pinned_version(id) {
            let pinned = match &plan.pinned {
                Some(version) if *version == chosen.version => Some(plan.command.clone()),
                Some(_) => inventory::plan(
                    id,
                    InstallAction::Install,
                    Some(&chosen.version),
                    &tools(&["npm"]),
                )
                .ok()
                .map(|p| p.command),
                None => None,
            };
            if let Some(command) = pinned {
                return Ok(HarnessPin {
                    id: id.to_owned(),
                    command,
                    version: chosen.version,
                    evidence: chosen.evidence,
                    unpinned: None,
                });
            }
        }
    }
    // Nothing pinnable: what would the unpinned install be?
    let wide = inventory::plan(
        id,
        InstallAction::Install,
        None,
        &tools(&["npm", "curl", "bash", "sh"]),
    );
    let plan = match (wide, narrow) {
        (Ok(plan), _) => plan,
        (Err(_), Err(why)) => return Err(why),
        (Err(why), Ok(_)) => return Err(why),
    };
    let reason = plan
        .unpinned
        .clone()
        .unwrap_or_else(|| "its install takes no version".to_owned());
    if !allow_unpinned {
        return Err(format!(
            "{id} cannot be pinned in an image: `{}` ({reason}); pass --allow-unpinned to install \
             it anyway, which its label then records as {UNPINNED}",
            plan.command
        ));
    }
    Ok(HarnessPin {
        id: id.to_owned(),
        command: plan.command,
        version: UNPINNED.to_owned(),
        evidence: "catalog".to_owned(),
        unpinned: Some(reason),
    })
}

/// Everything a build context is written from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPlan {
    /// The provider the image is for (a serialized kind).
    pub provider: String,
    /// The `FROM` image.
    pub base: String,
    /// The harnesses to install, sorted by ID.
    pub harnesses: Vec<HarnessPin>,
    /// The `by` version the binary reports.
    pub by_version: String,
    /// The digest of Anvil's toolchain copied to [`ANVIL_PREFIX`], if any.
    pub anvil: Option<String>,
}

impl ContextPlan {
    /// The labels the built image carries.
    pub fn labels(&self) -> ImageLabels {
        ImageLabels {
            harnesses: self
                .harnesses
                .iter()
                .map(|h| (h.id.clone(), h.version.clone()))
                .collect(),
            evidence: self
                .harnesses
                .iter()
                .map(|h| (h.id.clone(), h.evidence.clone()))
                .collect(),
            by: Some(self.by_version.clone()),
            anvil: self.anvil.clone(),
        }
    }

    /// The Dockerfile. The same plan always gives the same text.
    pub fn dockerfile(&self) -> String {
        let mut text = String::new();
        text.push_str(
            "# Written by `by images build` (docs/images.md). Rebuild rather than edit.\n\
             #\n\
             # This image and its build context hold no credentials, tokens or keys. A harness\n\
             # logs in at run time, from the variables a turn passes (pass_env) or its private\n\
             # home, never from the image.\n",
        );
        text.push_str(&format!("# Provider: {}.\n", self.provider));
        text.push_str(&format!("FROM {}\n", self.base));
        let piped = self
            .harnesses
            .iter()
            .any(|h| h.command.contains("| ") || h.command.starts_with("curl"));
        // Git is a runtime prerequisite, whatever the harnesses: a container
        // or Agent Sandbox placement copies the workspace in with `git init`
        // in the guest (branchyard-substrate's transfer), and the default
        // slim base has none. A base that already has it (`--base`) is left
        // as it is.
        text.push_str(
            "RUN command -v git >/dev/null 2>&1 || { apt-get update \\\n \
             && apt-get install -y --no-install-recommends ca-certificates git \\\n \
             && rm -rf /var/lib/apt/lists/*; }\n",
        );
        if piped {
            text.push_str(
                "RUN apt-get update \\\n && apt-get install -y --no-install-recommends bash ca-certificates curl \\\n \
                 && rm -rf /var/lib/apt/lists/*\n",
            );
        }
        for harness in &self.harnesses {
            match &harness.unpinned {
                Some(why) => text.push_str(&format!("# {}: unpinned ({why})\n", harness.id)),
                None => text.push_str(&format!("# {} {}\n", harness.id, harness.version)),
            }
            text.push_str(&format!("RUN {}\n", harness.command));
        }
        text.push_str("COPY by /usr/local/bin/by\n");
        if self.anvil.is_some() {
            text.push_str(&format!("COPY anvil {ANVIL_PREFIX}\n"));
        }
        text.push_str(
            "RUN mkdir -p /workspace /branchyard/home /tools \\\n \
             && chown 1000:1000 /workspace /branchyard/home /tools\n",
        );
        for (key, value) in self.labels().all() {
            text.push_str(&format!("LABEL {key}={}\n", Value::String(value)));
        }
        text
    }
}

/// Write the build context for `plan` into `dir`: the Dockerfile, the `by`
/// binary copied from `by_binary`, and `anvil_from` (a directory made by
/// [`prepare_anvil`]) copied to `anvil/`.
pub fn write_context(
    dir: &Path,
    plan: &ContextPlan,
    by_binary: &Path,
    anvil_from: Option<&Path>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join("Dockerfile"), plan.dockerfile())
        .map_err(|e| format!("{}: {e}", dir.join("Dockerfile").display()))?;
    let target = dir.join("by");
    std::fs::copy(by_binary, &target)
        .map_err(|e| format!("copy {} to {}: {e}", by_binary.display(), target.display()))?;
    if let Some(from) = anvil_from {
        let to = dir.join("anvil");
        if from != to {
            if to.exists() {
                std::fs::remove_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
            }
            copy_tree(from, &to)?;
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        if kind.is_symlink() {
            let link = std::fs::read_link(entry.path())
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            if target.symlink_metadata().is_ok() {
                std::fs::remove_file(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            }
            std::os::unix::fs::symlink(&link, &target)
                .map_err(|e| format!("{}: {e}", target.display()))?;
        } else if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// How installing Anvil's toolchain into a context went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnvilInstall {
    /// Installed under the prefix; the digest `current` names.
    Installed(String),
    /// Not done, and why (said to the person, never an error).
    Skipped(String),
}

/// Run `anvil install --prefix PREFIX` (`anvil` is the command words of
/// `[connectors] anvil`) when it has an `install` command, as
/// `tools/session_setup.sh` does for a host, and read the digest its
/// `current` link names. An Anvil that is absent or lacks the command is
/// [`AnvilInstall::Skipped`].
pub fn prepare_anvil(anvil: &[String], prefix: &Path) -> AnvilInstall {
    let Some((program, fixed)) = anvil.split_first() else {
        return AnvilInstall::Skipped("[connectors] anvil is empty".into());
    };
    let guard = branchyard_support::child::command(program)
        .args(fixed)
        .args(["install", "--help"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match guard {
        Ok(status) if status.success() => {}
        Ok(_) => {
            return AnvilInstall::Skipped(format!(
                "{program} has no `install` command (anvil install --help failed)"
            ))
        }
        Err(e) => return AnvilInstall::Skipped(format!("{program} could not run: {e}")),
    }
    if let Err(e) = std::fs::create_dir_all(prefix) {
        return AnvilInstall::Skipped(format!("{}: {e}", prefix.display()));
    }
    let out = branchyard_support::child::command(program)
        .args(fixed)
        .args(["install", "--prefix"])
        .arg(prefix)
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return AnvilInstall::Skipped(format!(
                "anvil install --prefix failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
        Err(e) => return AnvilInstall::Skipped(format!("{program} could not run: {e}")),
    }
    match std::fs::read_link(prefix.join("current")) {
        Ok(target) => {
            let name = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            AnvilInstall::Installed(name)
        }
        Err(e) => AnvilInstall::Skipped(format!(
            "anvil install left no `current` link under {}: {e}",
            prefix.display()
        )),
    }
}

// ---------------------------------------------------------------------------
// Docker

/// The `docker` executable on `PATH`, if any.
pub fn docker_on_path() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("docker"))
            .find(|candidate| candidate.is_file())
    })
}

/// The command that builds `context` as `tag`.
pub fn build_command(tag: &str, context: &Path, push: bool, metadata: &Path) -> Vec<String> {
    vec![
        "docker".into(),
        "buildx".into(),
        "build".into(),
        "--tag".into(),
        tag.into(),
        "--metadata-file".into(),
        metadata.display().to_string(),
        if push { "--push" } else { "--load" }.into(),
        context.display().to_string(),
    ]
}

/// What `docker image inspect` said of an image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inspected {
    /// The local image ID (`sha256:...`).
    pub id: Option<String>,
    /// `repository@digest` entries.
    pub repo_digests: Vec<String>,
    /// The image's labels.
    pub labels: BTreeMap<String, String>,
}

/// `docker image inspect REF`: `Ok(None)` when docker is absent or the
/// image is not in its store, an error for anything else it said.
pub fn inspect(reference: &str) -> Result<Option<Inspected>, String> {
    if docker_on_path().is_none() {
        return Ok(None);
    }
    let out = branchyard_support::child::command("docker")
        .args(["image", "inspect", reference])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return match stderr.to_lowercase().contains("no such image") {
            true => Ok(None),
            false => Err(format!("docker image inspect: {}", stderr.trim())),
        };
    }
    let value: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("docker image inspect did not answer JSON: {e}"))?;
    let first = value
        .as_array()
        .and_then(|items| items.first())
        .ok_or("docker image inspect answered no image")?;
    let labels = first["Config"]["Labels"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    Ok(Some(Inspected {
        id: first["Id"].as_str().map(str::to_owned),
        repo_digests: first["RepoDigests"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        labels,
    }))
}

/// What a finished `docker buildx build` left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// The registry digest, read back; absent for a local image never pushed.
    pub digest: Option<String>,
    /// The local image ID, when the image was loaded.
    pub image_id: Option<String>,
}

/// Run the build of `context` as `tag` through `docker buildx` and read
/// back its digest: from the metadata file a push writes, else from the
/// loaded image's `RepoDigests`.
pub fn build(tag: &str, context: &Path, push: bool) -> Result<Built, String> {
    let metadata = context.join(".metadata.json");
    let argv = build_command(tag, context, push, &metadata);
    let out = branchyard_support::child::command(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("docker: {e}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = text.lines().rev().take(12).collect();
        return Err(format!(
            "docker buildx build failed ({}):\n{}",
            out.status,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ));
    }
    // buildx writes the metadata file only when it pushes; a missing file is no digest here.
    let from_metadata = match std::fs::read(&metadata) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["containerimage.digest"].as_str().map(str::to_owned)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", metadata.display())),
    };
    // The context is the build's input, not its output.
    if metadata.exists() {
        branchyard_support::best_effort(
            "remove the build's metadata file",
            std::fs::remove_file(&metadata),
        );
    }
    let loaded = if push { None } else { inspect(tag)? };
    let digest = from_metadata.or_else(|| {
        loaded.as_ref().and_then(|image| {
            image
                .repo_digests
                .iter()
                .find_map(|d| digest_of(d).map(str::to_owned))
        })
    });
    Ok(Built {
        digest,
        image_id: loaded.and_then(|image| image.id),
    })
}

// ---------------------------------------------------------------------------
// Reading labels without starting the image

/// A place an image's labels can be read from, without starting it.
pub trait ImageLabelSource: Send + Sync + fmt::Debug {
    /// The labels of `reference`. `Ok(None)`: this source does not hold the
    /// image. An error: it should have, and could not read it.
    fn labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, String>;
}

/// The labels `by images build` and `by images pin` recorded in a
/// repository's `.branchyard/images.json`.
#[derive(Clone, Debug)]
pub struct RecordedLabels {
    store: ImageStore,
}

impl RecordedLabels {
    /// The records of the repository rooted at `root`.
    pub fn new(root: &Path) -> RecordedLabels {
        RecordedLabels {
            store: ImageStore::new(root),
        }
    }
}

impl ImageLabelSource for RecordedLabels {
    fn labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, String> {
        Ok(self.store.find(reference)?.map(|record| record.labels))
    }
}

/// Labels from the local Docker daemon (`docker image inspect`).
#[derive(Clone, Copy, Debug, Default)]
pub struct DockerLabels;

impl ImageLabelSource for DockerLabels {
    fn labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, String> {
        Ok(inspect(reference)?.map(|image| image.labels))
    }
}

/// Sources tried in order: the first that holds the image answers.
#[derive(Debug, Default)]
pub struct LabelChain {
    sources: Vec<Arc<dyn ImageLabelSource>>,
}

impl LabelChain {
    /// A chain of `sources`.
    pub fn new(sources: Vec<Arc<dyn ImageLabelSource>>) -> LabelChain {
        LabelChain { sources }
    }
}

impl ImageLabelSource for LabelChain {
    fn labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, String> {
        let mut problems = Vec::new();
        for source in &self.sources {
            match source.labels(reference) {
                Ok(Some(labels)) => return Ok(Some(labels)),
                Ok(None) => {}
                Err(why) => problems.push(why),
            }
        }
        match problems.is_empty() {
            true => Ok(None),
            false => Err(problems.join("; ")),
        }
    }
}

/// A source that remembers what it was asked: one route asks about the
/// same image for each candidate.
#[derive(Debug)]
pub struct CachedLabels {
    inner: Arc<dyn ImageLabelSource>,
    seen: Mutex<BTreeMap<String, LabelAnswer>>,
}

/// What a source answered for one image: its labels, none, or why not.
type LabelAnswer = Result<Option<BTreeMap<String, String>>, String>;

impl CachedLabels {
    /// Remember the answers of `inner`.
    pub fn new(inner: Arc<dyn ImageLabelSource>) -> CachedLabels {
        CachedLabels {
            inner,
            seen: Mutex::new(BTreeMap::new()),
        }
    }
}

impl ImageLabelSource for CachedLabels {
    fn labels(&self, reference: &str) -> Result<Option<BTreeMap<String, String>>, String> {
        if let Some(known) = self.seen.lock_recovering("image labels").get(reference) {
            return known.clone();
        }
        let answer = self.inner.labels(reference);
        self.seen
            .lock_recovering("image labels")
            .insert(reference.to_owned(), answer.clone());
        answer
    }
}

/// What an image's labels say about a harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The image holds the harness at this version (or [`UNPINNED`]).
    Holds(String),
    /// The image is a toolchain image and does not hold it.
    Lacks {
        /// What it holds instead, as [`ImageLabels::held`] words it.
        held: String,
    },
    /// The labels cannot say: none are Branchyard's, or they could not be read.
    Unknown(String),
}

/// Whether the image `reference` holds `harness`, from its labels.
pub fn judge(source: &dyn ImageLabelSource, reference: &str, harness: &str) -> Verdict {
    match source.labels(reference) {
        Err(why) => Verdict::Unknown(format!("its labels could not be read ({why})")),
        Ok(None) => Verdict::Unknown("no source holds it to read its labels".into()),
        Ok(Some(map)) => match ImageLabels::from_map(&map) {
            Err(why) => Verdict::Unknown(why),
            Ok(None) => Verdict::Unknown(
                "it carries no dev.branchyard labels (build one with `by images build`)".into(),
            ),
            Ok(Some(labels)) => match labels.holds(harness) {
                Some(version) => Verdict::Holds(version.to_owned()),
                None => Verdict::Lacks {
                    held: labels.held(),
                },
            },
        },
    }
}

/// What a turn's manifest records of the image it ran in.
pub fn manifest_image(
    source: &dyn ImageLabelSource,
    reference: &str,
) -> crate::manifest::ManifestImage {
    let toolchain = source
        .labels(reference)
        .ok()
        .flatten()
        .and_then(|map| map.get(LABEL_TOOLCHAIN).cloned());
    crate::manifest::ManifestImage {
        reference: reference.to_owned(),
        digest: digest_of(reference).map(str::to_owned),
        toolchain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> ImageLabels {
        ImageLabels {
            harnesses: [
                ("codex".to_owned(), "0.157.1".to_owned()),
                ("claude-code".to_owned(), UNPINNED.to_owned()),
            ]
            .into_iter()
            .collect(),
            evidence: BTreeMap::new(),
            by: Some("0.4.0".into()),
            anvil: None,
        }
    }

    #[test]
    fn the_toolchain_label_hashes_the_sorted_other_labels() {
        let l = labels();
        let expected = {
            let mut hash = Sha256::new();
            hash.update(
                b"dev.branchyard.by=0.4.0\ndev.branchyard.harness.claude-code=unpinned\n\
                  dev.branchyard.harness.codex=0.157.1\n",
            );
            format!("sha256:{}", hex::encode(hash.finalize()))
        };
        assert_eq!(l.toolchain(), expected);
        assert_eq!(l.all()[LABEL_TOOLCHAIN], expected);
        // Another version, another hash.
        let mut other = labels();
        other.harnesses.insert("codex".into(), "0.158.0".into());
        assert_ne!(other.toolchain(), expected);
    }

    #[test]
    fn labels_read_back_and_an_edited_set_is_refused() {
        let l = labels();
        assert_eq!(ImageLabels::from_map(&l.all()).unwrap(), Some(l.clone()));
        let mut edited = l.all();
        edited.insert(format!("{LABEL_HARNESS}codex"), "9.9.9".into());
        assert!(ImageLabels::from_map(&edited)
            .unwrap_err()
            .contains("changed after the build"));
        let plain: BTreeMap<String, String> = [("maintainer".to_owned(), "me".to_owned())].into();
        assert_eq!(ImageLabels::from_map(&plain).unwrap(), None);
    }

    #[test]
    fn references_split_into_what_a_registry_is_asked() {
        let r = ImageRef::parse("ghcr.io/me/toolchain:1@sha256:abc").unwrap();
        assert_eq!(
            (
                r.registry.as_str(),
                r.repository.as_str(),
                r.reference.as_str()
            ),
            ("ghcr.io", "me/toolchain", "sha256:abc")
        );
        assert_eq!(r.base_url(), "https://ghcr.io");
        let hub = ImageRef::parse("node:22").unwrap();
        assert_eq!(
            (
                hub.registry.as_str(),
                hub.repository.as_str(),
                hub.reference.as_str()
            ),
            ("registry-1.docker.io", "library/node", "22")
        );
        let local = ImageRef::parse("127.0.0.1:5000/team/tc@sha256:def").unwrap();
        assert_eq!(local.base_url(), "http://127.0.0.1:5000");
        assert_eq!(local.repository, "team/tc");
        assert!(ImageRef::parse("-bad").is_err());
        assert_eq!(digest_of("a/b:1@sha256:abc"), Some("sha256:abc"));
        assert_eq!(digest_of("a/b:1"), None);
    }

    #[test]
    fn a_context_holds_the_dockerfile_the_binary_and_the_toolchain_copy() {
        let dir = tempfile::tempdir().unwrap();
        let by = dir.path().join("by-binary");
        std::fs::write(&by, "binary").unwrap();
        let staged = dir.path().join("staged");
        std::fs::create_dir_all(staged.join("versions/abc")).unwrap();
        std::fs::write(staged.join("versions/abc/anvil"), "x").unwrap();
        std::os::unix::fs::symlink("versions/abc", staged.join("current")).unwrap();
        let plan = ContextPlan {
            provider: "microsandbox".into(),
            base: "base:1".into(),
            harnesses: Vec::new(),
            by_version: "0.4.0".into(),
            anvil: Some("abc".into()),
        };
        let out = dir.path().join("ctx");
        write_context(&out, &plan, &by, Some(&staged)).unwrap();
        // A second write replaces the copy rather than merging into it.
        std::fs::write(out.join("anvil/stale"), "old").unwrap();
        write_context(&out, &plan, &by, Some(&staged)).unwrap();
        assert_eq!(std::fs::read_to_string(out.join("by")).unwrap(), "binary");
        assert_eq!(
            std::fs::read_to_string(out.join("Dockerfile")).unwrap(),
            plan.dockerfile()
        );
        assert!(out.join("anvil/versions/abc/anvil").is_file());
        assert!(out.join("anvil/current").is_symlink());
        assert!(!out.join("anvil/stale").exists());
        // The push and load forms of the build command differ in one flag.
        let push = build_command("t:1", &out, true, &out.join("m.json"));
        let load = build_command("t:1", &out, false, &out.join("m.json"));
        assert_eq!(&push[..5], ["docker", "buildx", "build", "--tag", "t:1"]);
        assert!(push.contains(&"--push".to_owned()) && load.contains(&"--load".to_owned()));
        assert_eq!(push.last(), load.last());
    }

    #[test]
    fn an_anvil_that_cannot_install_is_skipped_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let none = prepare_anvil(&[], dir.path());
        assert!(matches!(none, AnvilInstall::Skipped(_)));
        let missing = prepare_anvil(&["/no/such/anvil".to_owned()], &dir.path().join("prefix"));
        assert!(
            matches!(&missing, AnvilInstall::Skipped(why) if why.contains("could not run")),
            "{missing:?}"
        );
        // `true` runs and succeeds but installs nothing: no `current` link.
        let empty = prepare_anvil(&["true".to_owned()], &dir.path().join("prefix"));
        assert!(
            matches!(&empty, AnvilInstall::Skipped(why) if why.contains("no `current` link")),
            "{empty:?}"
        );
        // `false` has no `install` command, as the guard sees it.
        let lacking = prepare_anvil(&["false".to_owned()], &dir.path().join("prefix"));
        assert!(
            matches!(&lacking, AnvilInstall::Skipped(why) if why.contains("no `install` command")),
            "{lacking:?}"
        );
    }

    #[test]
    fn provider_names_normalize() {
        assert_eq!(provider_kind("agent-sandbox"), Some("agent_sandbox"));
        assert_eq!(provider_kind("microsandbox"), Some("microsandbox"));
        assert_eq!(provider_kind("local"), None);
    }

    #[test]
    fn a_dockerfile_is_deterministic_and_carries_the_label_set() {
        let plan = ContextPlan {
            provider: "container".into(),
            base: DEFAULT_BASE.into(),
            harnesses: vec![HarnessPin {
                id: "codex".into(),
                command: "npm install -g @openai/codex@0.157.1".into(),
                version: "0.157.1".into(),
                evidence: "catalog".into(),
                unpinned: None,
            }],
            by_version: "0.4.0".into(),
            anvil: Some("abc123".into()),
        };
        let text = plan.dockerfile();
        assert_eq!(text, plan.dockerfile());
        assert!(text.contains("hold no credentials, tokens or keys"));
        assert!(text.contains("RUN npm install -g @openai/codex@0.157.1\n"));
        assert!(text.contains("COPY by /usr/local/bin/by\n"));
        assert!(text.contains("COPY anvil /opt/anvil\n"));
        assert!(text.contains("LABEL dev.branchyard.harness.codex=\"0.157.1\"\n"));
        assert!(text.contains(&format!(
            "LABEL dev.branchyard.toolchain=\"{}\"\n",
            plan.labels().toolchain()
        )));
        assert!(
            text.contains("RUN command -v git >/dev/null 2>&1 || { apt-get update")
                && text.contains("--no-install-recommends ca-certificates git"),
            "git is installed unless the base has it: {text}"
        );
        assert!(!text.contains("curl"), "no piped installer, no curl");
    }

    #[test]
    fn records_replace_by_reference_and_are_found_by_tag_or_digest() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path());
        assert!(store.list().unwrap().is_empty());
        let record = |digest: &str| ImageRecord {
            reference: format!("ghcr.io/me/tc:1@sha256:{digest}"),
            tag: "ghcr.io/me/tc:1".into(),
            digest: Some(format!("sha256:{digest}")),
            image_id: None,
            provider: "container".into(),
            labels: labels().all(),
            created: "2026-11-05T00:00:00.000Z".into(),
            pushed: true,
        };
        store.add(record(&"a".repeat(64))).unwrap();
        store.add(record(&"a".repeat(64))).unwrap();
        store.add(record(&"b".repeat(64))).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
        let newest = store.find("ghcr.io/me/tc:1").unwrap().unwrap();
        assert_eq!(newest.digest, Some(format!("sha256:{}", "b".repeat(64))));
        assert!(store
            .find(&format!("sha256:{}", "a".repeat(64)))
            .unwrap()
            .is_some());
        assert!(store.find("nope").unwrap().is_none());
        assert_eq!(newest.toolchain(), Some(labels()));
    }

    #[derive(Debug)]
    struct Fixed(Result<Option<BTreeMap<String, String>>, String>);

    impl ImageLabelSource for Fixed {
        fn labels(&self, _: &str) -> Result<Option<BTreeMap<String, String>>, String> {
            self.0.clone()
        }
    }

    #[test]
    fn a_verdict_comes_from_the_labels_alone() {
        let toolchain = Fixed(Ok(Some(labels().all())));
        assert_eq!(
            judge(&toolchain, "i", "codex"),
            Verdict::Holds("0.157.1".into())
        );
        assert!(matches!(
            judge(&toolchain, "i", "gemini-cli"),
            Verdict::Lacks { held } if held.contains("codex 0.157.1")
        ));
        let plain = Fixed(Ok(Some(
            [("a".to_owned(), "b".to_owned())].into_iter().collect(),
        )));
        assert!(matches!(judge(&plain, "i", "codex"), Verdict::Unknown(_)));
        let broken = Fixed(Err("boom".into()));
        assert!(
            matches!(judge(&broken, "i", "codex"), Verdict::Unknown(why) if why.contains("boom"))
        );
    }

    #[test]
    fn a_chain_asks_in_order_and_a_cache_asks_once() {
        let first: Arc<dyn ImageLabelSource> = Arc::new(Fixed(Ok(None)));
        let second: Arc<dyn ImageLabelSource> = Arc::new(Fixed(Ok(Some(labels().all()))));
        let chain = LabelChain::new(vec![first, second]);
        assert!(chain.labels("i").unwrap().is_some());
        let failing: Arc<dyn ImageLabelSource> = Arc::new(Fixed(Err("down".into())));
        let nothing = LabelChain::new(vec![failing]);
        assert_eq!(nothing.labels("i").unwrap_err(), "down");
        let cached = CachedLabels::new(Arc::new(chain));
        assert_eq!(cached.labels("i").unwrap(), cached.labels("i").unwrap());
    }

    #[test]
    fn a_qualified_pin_comes_before_the_catalog_version_and_names_its_evidence() {
        let catalog = pinned_version_from(None, "codex").unwrap();
        assert_eq!(catalog.evidence, "catalog");
        assert_eq!(Some(catalog.version), inventory::checked_version("codex"));
        let mut pin = crate::qualified::QualifiedPin {
            id: "codex".into(),
            version: "99.1.0".into(),
            package: "@openai/codex".into(),
            profile: "codex-app-server".into(),
            integrity: None,
            qualified_at: "2026-11-01T00:00:00Z".into(),
            live_record: None,
            checks: Vec::new(),
        };
        let hermetic = pinned_version_from(Some(pin.clone()), "codex").unwrap();
        assert_eq!(
            (hermetic.version.as_str(), hermetic.evidence.as_str()),
            ("99.1.0", "hermetic")
        );
        pin.live_record = Some("docs/qualification/codex.json".into());
        assert_eq!(
            pinned_version_from(Some(pin), "codex").unwrap().evidence,
            "live"
        );
        assert_eq!(pinned_version_from(None, "no-such-harness"), None);
    }

    #[test]
    fn evidence_labels_round_trip_and_are_part_of_the_toolchain_hash() {
        let mut with = labels();
        with.evidence = [("codex".to_owned(), "hermetic".to_owned())].into();
        let all = with.all();
        assert_eq!(all["dev.branchyard.harness.codex.evidence"], "hermetic");
        let read = ImageLabels::from_map(&all).unwrap().unwrap();
        assert_eq!(read, with);
        assert_eq!(read.holds("codex"), Some("0.157.1"));
        assert_ne!(with.toolchain(), labels().toolchain());
    }

    #[test]
    fn unpinnable_harnesses_are_refused_without_the_flag() {
        // Claude Code installs by a piped script only.
        let refused = pin_harness("claude-code", false).unwrap_err();
        assert!(refused.contains("--allow-unpinned"), "{refused}");
        let allowed = pin_harness("claude-code", true).unwrap();
        assert_eq!(allowed.version, UNPINNED);
        assert!(allowed.command.contains("| bash"));
        assert!(allowed.unpinned.is_some());
        // Codex installs by npm, pinned to the version its profile was checked against.
        let codex = pin_harness("codex", false).unwrap();
        assert!(codex.command.starts_with("npm install -g @openai/codex@"));
        assert_ne!(codex.version, UNPINNED);
        assert!(pin_harness("no-such-harness", false).is_err());
    }
}
