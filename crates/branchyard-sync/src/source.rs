//! What is synced: a [`SyncSource`] is one task's git directory, its refs
//! in the task's own names, the chunks its commits point at, and its
//! conversation segments.
//!
//! [`BranchSource`] adapts what exists today, a Branchyard branch of an
//! ordinary repository: the branch is the task's `refs/heads/main`, its
//! checkpoints (`refs/branchyard/<name>/...`) are the task's
//! `refs/branchyard/...`, and conflict branches are kept beside it as
//! `refs/heads/conflict/<branch>/<device>/<n>`.
//!
//! A task repository (`docs/task-repos.md`) implements the trait over its
//! own git directory (`~/.branchyard/tasks/<id>/git` for a folder task,
//! the repository's for a code task): `refs` returns `refs/heads/*` and
//! `refs/branchyard/*` as they are, `local_ref` is the identity,
//! `chunk_dir` is `~/.branchyard/chunks`, `reachable_chunks` is its
//! `reachable_chunks` API, and `segments` lists closed conversation
//! segments not yet committed. See `docs/sync.md#task-repositories`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::git::Git;

/// A chunk's identity: the BLAKE3 hash of its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkId(pub [u8; 32]);

impl ChunkId {
    pub fn of(data: &[u8]) -> ChunkId {
        ChunkId(*blake3::hash(data).as_bytes())
    }

    pub fn hex(&self) -> String {
        crate::util::hex(&self.0)
    }

    pub fn parse(hex: &str) -> Result<ChunkId> {
        let bytes = crate::util::unhex(hex.trim())
            .filter(|b| b.len() == 32)
            .ok_or_else(|| Error::corrupt(format!("{hex:?} is not a chunk ID")))?;
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes);
        Ok(ChunkId(id))
    }

    pub fn hash(&self) -> blake3::Hash {
        blake3::Hash::from_bytes(self.0)
    }
}

/// Where a chunk lives in a chunk directory: `<aa>/<hex>`.
pub fn chunk_path(dir: &Path, id: &ChunkId) -> PathBuf {
    let hex = id.hex();
    dir.join(&hex[..2]).join(hex)
}

/// A chunk from a chunk directory, checked against its ID.
pub fn read_chunk(dir: &Path, id: &ChunkId) -> Result<Vec<u8>> {
    let data = std::fs::read(chunk_path(dir, id))?;
    match ChunkId::of(&data) == *id {
        true => Ok(data),
        false => Err(Error::corrupt(format!(
            "local chunk {} does not match its name",
            id.hex()
        ))),
    }
}

/// Write a chunk into a chunk directory (atomically: a temporary file
/// renamed into place).
pub fn write_chunk(dir: &Path, id: &ChunkId, data: &[u8]) -> Result<()> {
    let path = chunk_path(dir, id);
    if path.exists() {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| Error::local("chunk path"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.tmp",
        crate::util::hex(&crate::util::random_bytes(6)?)
    ));
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

/// A closed conversation segment: immutable once listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Its name in the task, such as `conversation/0007.jsonl`.
    pub name: String,
    pub path: PathBuf,
}

/// One task as sync sees it.
pub trait SyncSource: Send + Sync {
    /// The task's ID: 1 to 128 of `A-Z a-z 0-9 . _ - ~`.
    fn task_id(&self) -> &str;

    /// The git directory holding the task's objects and refs.
    fn git_dir(&self) -> &Path;

    /// The task's refs in its own names (`refs/heads/main`,
    /// `refs/heads/attempt/<n>`, `refs/branchyard/...`,
    /// `refs/heads/conflict/<device>/<n>`), to commits.
    fn refs(&self) -> Result<BTreeMap<String, String>>;

    /// The local ref a task ref is kept in; `None` when this source does
    /// not keep that ref (it is then left alone on pull).
    fn local_ref(&self, task_ref: &str) -> Option<String>;

    /// The chunk directory, `<aa>/<blake3 hex>` files.
    fn chunk_dir(&self) -> Option<&Path> {
        None
    }

    /// The chunks a commit's tree points at.
    fn reachable_chunks(&self, commit: &str) -> Result<Vec<ChunkId>> {
        let _ = commit;
        Ok(Vec::new())
    }

    /// Closed conversation segments to upload.
    fn segments(&self) -> Result<Vec<Segment>> {
        Ok(Vec::new())
    }

    /// Where pulled segments are written, by name.
    fn segment_dir(&self) -> Option<PathBuf> {
        None
    }

    /// How far the task's effect ledger reaches, recorded in the manifest.
    fn ledger_watermark(&self) -> Option<u64> {
        None
    }
}

/// The task ID of a branch: its name with `/` as `~` (which git refuses
/// in ref names, so the mapping is one to one).
pub fn task_id_for_branch(branch: &str) -> String {
    branch.replace('/', "~")
}

pub type ChunkFinder = Arc<dyn Fn(&Git, &str) -> Result<Vec<ChunkId>> + Send + Sync>;

/// A Branchyard branch of an ordinary repository, as a task.
#[derive(Clone)]
pub struct BranchSource {
    task: String,
    git_dir: PathBuf,
    name: String,
    git_branch: String,
    chunk_dir: Option<PathBuf>,
    chunks: Option<ChunkFinder>,
    segments: Vec<Segment>,
    segment_dir: Option<PathBuf>,
}

impl BranchSource {
    /// The branch `name` (Branchyard's name; its checkpoints are under
    /// `refs/branchyard/<name>/`) whose git branch is `git_branch`.
    pub fn new(git_dir: &Path, name: &str, git_branch: &str) -> Result<BranchSource> {
        let task = task_id_for_branch(name);
        crate::manifest::check_task_id(&task)?;
        Ok(BranchSource {
            task,
            git_dir: git_dir.to_path_buf(),
            name: name.to_owned(),
            git_branch: git_branch.to_owned(),
            chunk_dir: None,
            chunks: None,
            segments: Vec::new(),
            segment_dir: None,
        })
    }

    /// The git directory of the repository at `root` (its `.git`, or the
    /// directory a `.git` file points at).
    pub fn git_dir_of(root: &Path) -> Result<PathBuf> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .map_err(|e| Error::local(format!("git: {e}")))?;
        if !output.status.success() {
            return Err(Error::local(format!(
                "{} is not a git repository",
                root.display()
            )));
        }
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    }

    /// Sync chunks from `dir`, finding a commit's chunks with `finder`.
    pub fn with_chunks(mut self, dir: &Path, finder: ChunkFinder) -> BranchSource {
        self.chunk_dir = Some(dir.to_path_buf());
        self.chunks = Some(finder);
        self
    }

    /// Sync `segments`, and write pulled ones under `dir`.
    pub fn with_segments(mut self, segments: Vec<Segment>, dir: &Path) -> BranchSource {
        self.segments = segments;
        self.segment_dir = Some(dir.to_path_buf());
        self
    }

    fn conflict_prefix(&self) -> String {
        format!("refs/heads/conflict/{}/", self.git_branch)
    }
}

impl SyncSource for BranchSource {
    fn task_id(&self) -> &str {
        &self.task
    }

    fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    fn refs(&self) -> Result<BTreeMap<String, String>> {
        let git = Git::new(&self.git_dir);
        let mut out = BTreeMap::new();
        if let Some(oid) = git.resolve(&format!("refs/heads/{}", self.git_branch))? {
            out.insert("refs/heads/main".to_owned(), oid);
        }
        let checkpoints = format!("refs/branchyard/{}/", self.name);
        for (name, oid) in git.refs(&checkpoints)? {
            if let Some(rest) = name.strip_prefix(&checkpoints) {
                out.insert(format!("refs/branchyard/{rest}"), oid);
            }
        }
        let conflicts = self.conflict_prefix();
        for (name, oid) in git.refs(&conflicts)? {
            if let Some(rest) = name.strip_prefix(&conflicts) {
                out.insert(format!("refs/heads/conflict/{rest}"), oid);
            }
        }
        Ok(out)
    }

    fn local_ref(&self, task_ref: &str) -> Option<String> {
        if task_ref == "refs/heads/main" {
            return Some(format!("refs/heads/{}", self.git_branch));
        }
        if let Some(rest) = task_ref.strip_prefix("refs/heads/conflict/") {
            return Some(format!("{}{rest}", self.conflict_prefix()));
        }
        if let Some(rest) = task_ref.strip_prefix("refs/branchyard/") {
            return Some(format!("refs/branchyard/{}/{rest}", self.name));
        }
        None
    }

    fn chunk_dir(&self) -> Option<&Path> {
        self.chunk_dir.as_deref()
    }

    fn reachable_chunks(&self, commit: &str) -> Result<Vec<ChunkId>> {
        match &self.chunks {
            Some(finder) => finder(&Git::new(&self.git_dir), commit),
            None => Ok(Vec::new()),
        }
    }

    fn segments(&self) -> Result<Vec<Segment>> {
        Ok(self.segments.clone())
    }

    fn segment_dir(&self) -> Option<PathBuf> {
        self.segment_dir.clone()
    }
}

/// The first line of a pointer file: the files a commit holds in place of
/// large ones, one `<blake3 hex> <size>` line per chunk after this one.
/// The task-repository work defines the real pointer format; this one
/// lets a branch carry chunks today, and tests use it.
pub const POINTER_MAGIC: &str = "branchyard-chunks v1";

/// A [`ChunkFinder`] reading pointer files from a commit's tree.
pub fn pointer_chunks() -> ChunkFinder {
    Arc::new(|git: &Git, commit: &str| {
        let mut out = Vec::new();
        for (_, blob) in git.tree(commit)? {
            let data = git.blob(&blob)?;
            if data.len() > 1 << 20 || !data.starts_with(POINTER_MAGIC.as_bytes()) {
                continue;
            }
            for line in String::from_utf8_lossy(&data).lines().skip(1) {
                if let Some(hex) = line.split_whitespace().next() {
                    out.push(ChunkId::parse(hex)?);
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    })
}
