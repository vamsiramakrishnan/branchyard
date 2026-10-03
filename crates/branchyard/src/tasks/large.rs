//! Large and binary files in a task's repository: cut into content-defined
//! chunks kept in a chunk store shared by every task, with a small pointer
//! file in git where the file's bytes would be.
//!
//! Chunks are cut FastCDC-style (a gear rolling hash with normalized
//! chunking): at least [`MIN_CHUNK`], at most [`MAX_CHUNK`], about
//! [`AVG_CHUNK`] on average. Each is named by its BLAKE3 hash and stored once
//! at `<store>/<aa>/<hash>`, where `aa` is the hash's first two characters,
//! so identical content is stored once across tasks.
//!
//! A pointer is a short text file ([`Pointer`]). Branchyard applies it
//! itself, never through git filters or git-lfs: when it commits a task's
//! files ([`stage`], and the folder snapshot), a file of at least the task's
//! threshold is stored as chunks and its index entry becomes the pointer,
//! marked `skip-worktree` so git leaves the real file on disk alone; when it
//! checks files out ([`restore`]: a new worktree, a rewind, an accept), each
//! pointer is replaced by the file it names, verified byte for byte.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_workspace::Git;

use crate::Error;

/// The first line of every pointer.
pub const POINTER_HEADER: &str = "branchyard-chunked/1";
/// The smallest chunk, except a file's last.
pub const MIN_CHUNK: usize = 16 * 1024;
/// The chunk size the cut points aim for.
pub const AVG_CHUNK: usize = 256 * 1024;
/// The largest chunk.
pub const MAX_CHUNK: usize = 4 * 1024 * 1024;
/// Files of at least this many bytes are chunked, unless the task's
/// repository says otherwise (`branchyard.largeFileThreshold`).
pub const DEFAULT_THRESHOLD: u64 = 1024 * 1024;
/// A blob larger than this is never read as a pointer.
const POINTER_MAX: u64 = 1024 * 1024;

const AVG_BITS: u32 = AVG_CHUNK.trailing_zeros();
/// Harder to match before the average size, easier after it: normalized
/// chunking keeps sizes near the average.
const MASK_SMALL: u64 = !0u64 << (64 - (AVG_BITS + 2));
const MASK_LARGE: u64 = !0u64 << (64 - (AVG_BITS - 2));

/// The gear table: 256 fixed pseudo-random words (SplitMix64 from a fixed
/// seed), so the same bytes are cut the same way everywhere, forever.
const GEAR: [u64; 256] = gear();

const fn gear() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x6272_616e_6368_7964; // "branchyd"
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

/// Where the first chunk of `data` ends: `data` holds at most
/// [`MAX_CHUNK`] bytes, or what is left of a file.
pub fn cut(data: &[u8]) -> usize {
    let n = data.len().min(MAX_CHUNK);
    if n <= MIN_CHUNK {
        return n;
    }
    let normal = AVG_CHUNK.min(n);
    let mut hash = 0u64;
    let mut i = MIN_CHUNK;
    while i < normal {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_SMALL == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < n {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_LARGE == 0 {
            return i + 1;
        }
        i += 1;
    }
    n
}

/// The chunk lengths of `data`, in order.
pub fn chunk_lengths(data: &[u8]) -> Vec<usize> {
    let mut lengths = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let length = cut(&data[at..]);
        lengths.push(length);
        at += length;
    }
    lengths
}

/// A large file as git stores it: its size, its BLAKE3 hash, and its
/// chunks in order with their sizes.
///
/// ```text
/// branchyard-chunked/1
/// size 2097152
/// blake3 <64 hex>
/// chunk <64 hex> 262144
/// chunk <64 hex> 1835008
/// ```
///
/// Lines end with `\n`; nothing else is allowed. The chunk sizes add up to
/// `size`. An empty file has no chunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub size: u64,
    pub blake3: String,
    pub chunks: Vec<(String, u64)>,
}

fn is_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Pointer {
    /// The pointer `bytes` hold, or `None` when they are not exactly one.
    pub fn parse(bytes: &[u8]) -> Option<Pointer> {
        if bytes.len() as u64 > POINTER_MAX {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let body = text.strip_suffix('\n')?;
        let mut lines = body.split('\n');
        if lines.next()? != POINTER_HEADER {
            return None;
        }
        let size = lines.next()?.strip_prefix("size ")?.parse::<u64>().ok()?;
        let blake3 = lines.next()?.strip_prefix("blake3 ")?.to_owned();
        if !is_hash(&blake3) {
            return None;
        }
        let mut chunks = Vec::new();
        for line in lines {
            let (hash, length) = line.strip_prefix("chunk ")?.split_once(' ')?;
            let length = length.parse::<u64>().ok()?;
            if !is_hash(hash) || length == 0 {
                return None;
            }
            chunks.push((hash.to_owned(), length));
        }
        (chunks.iter().map(|(_, l)| l).sum::<u64>() == size).then_some(Pointer {
            size,
            blake3,
            chunks,
        })
    }

    pub fn render(&self) -> String {
        let mut text = format!(
            "{POINTER_HEADER}\nsize {}\nblake3 {}\n",
            self.size, self.blake3
        );
        for (hash, length) in &self.chunks {
            text.push_str(&format!("chunk {hash} {length}\n"));
        }
        text
    }
}

static TEMP: AtomicU64 = AtomicU64::new(0);

/// A name for a temporary file beside `path`, unique in this process.
pub(crate) fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        ".{name}.by-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The chunk store: `<dir>/<aa>/<blake3>`, written once per chunk and
/// verified on every read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkStore {
    dir: PathBuf,
}

impl ChunkStore {
    pub fn new(dir: impl Into<PathBuf>) -> ChunkStore {
        ChunkStore { dir: dir.into() }
    }

    /// `$BRANCHYARD_HOME/chunks`, shared by every task on this machine.
    pub fn at_home() -> ChunkStore {
        ChunkStore::new(super::home().join("chunks"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where chunk `hash` is, or would be.
    pub fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(&hash[..2.min(hash.len())]).join(hash)
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    /// Store `data`, unless a chunk with its hash is already there; returns
    /// the hash. A chunk is written to a temporary name and renamed, so a
    /// reader never sees part of one.
    pub fn put(&self, data: &[u8]) -> Result<String, Error> {
        let hash = blake3::hash(data).to_hex().to_string();
        let path = self.path(&hash);
        if path.is_file() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = temp_beside(&path);
        let written = (|| {
            let mut file = fs::File::create(&temp)?;
            file.write_all(data)?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written?;
        Ok(hash)
    }

    /// Chunk `hash`, verified against its name.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, Error> {
        let path = self.path(hash);
        let data = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::State(format!(
                "chunk {hash} is missing from the chunk store {}",
                self.dir.display()
            )),
            _ => Error::Io(e),
        })?;
        let actual = blake3::hash(&data).to_hex().to_string();
        if actual != hash {
            return Err(Error::State(format!(
                "chunk {} is corrupt: its content hashes to {actual}",
                path.display()
            )));
        }
        Ok(data)
    }

    /// Cut the file at `path` into chunks, store them, and return its
    /// pointer. The file is read once, in pieces of at most
    /// [`MAX_CHUNK`] bytes.
    pub fn store_file(&self, path: &Path) -> Result<Pointer, Error> {
        let mut file = fs::File::open(path)?;
        let mut whole = blake3::Hasher::new();
        let mut buffer: Vec<u8> = Vec::with_capacity(MAX_CHUNK);
        let mut chunks = Vec::new();
        let mut size = 0u64;
        let mut eof = false;
        loop {
            while !eof && buffer.len() < MAX_CHUNK {
                let start = buffer.len();
                buffer.resize(MAX_CHUNK, 0);
                let read = file.read(&mut buffer[start..])?;
                buffer.truncate(start + read);
                eof = read == 0;
            }
            if buffer.is_empty() {
                break;
            }
            let length = cut(&buffer);
            let chunk = &buffer[..length];
            whole.update(chunk);
            chunks.push((self.put(chunk)?, length as u64));
            size += length as u64;
            buffer.drain(..length);
        }
        Ok(Pointer {
            size,
            blake3: whole.finalize().to_hex().to_string(),
            chunks,
        })
    }

    /// Write the file `pointer` names to `dest`, through a temporary file
    /// that is renamed only once its BLAKE3 hash matches the pointer's.
    pub fn restore_file(
        &self,
        pointer: &Pointer,
        dest: &Path,
        executable: bool,
    ) -> Result<(), Error> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = temp_beside(dest);
        let written = (|| {
            let mut file = fs::File::create(&temp)?;
            let mut whole = blake3::Hasher::new();
            for (hash, length) in &pointer.chunks {
                let data = self.get(hash)?;
                if data.len() as u64 != *length {
                    return Err(Error::State(format!(
                        "chunk {hash} has {} bytes; the pointer says {length}",
                        data.len()
                    )));
                }
                whole.update(&data);
                file.write_all(&data)?;
            }
            let actual = whole.finalize().to_hex().to_string();
            if actual != pointer.blake3 {
                return Err(Error::State(format!(
                    "{} reassembled to {actual}, not {}",
                    dest.display(),
                    pointer.blake3
                )));
            }
            file.sync_all()?;
            drop(file);
            set_executable(&temp, executable)?;
            // A link or directory in the way is replaced, as git would.
            if fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir()) {
                fs::remove_dir_all(dest)?;
            }
            fs::rename(&temp, dest)?;
            Ok(())
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written
    }
}

#[cfg(unix)]
pub(crate) fn set_executable(path: &Path, executable: bool) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    let mode = match executable {
        true => mode | ((mode & 0o444) >> 2),
        false => mode & !0o111,
    };
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_executable(_: &Path, _: bool) -> Result<(), Error> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(crate) fn is_executable(_: &fs::Metadata) -> bool {
    false
}

/// The BLAKE3 hash of the file at `path`.
pub fn hash_file(path: &Path) -> Result<String, Error> {
    let mut hasher = blake3::Hasher::new();
    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// How a task's repository chunks large files, from its git config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Large {
    pub threshold: u64,
    pub store: ChunkStore,
}

fn git_err(error: branchyard_workspace::GitError) -> Error {
    crate::git::error(error)
}

/// Where git runs for a work tree: a worktree (at its top), or a folder
/// with its git directory and index elsewhere.
#[derive(Clone, Debug)]
pub(crate) struct At {
    pub dir: PathBuf,
    env: Vec<(&'static str, PathBuf)>,
}

impl At {
    /// The worktree, or git directory, at `dir`.
    pub fn new(dir: &Path) -> At {
        At {
            dir: dir.to_path_buf(),
            env: Vec::new(),
        }
    }

    /// `folder` as the work tree of `git_dir`, with an index of its own.
    pub fn folder(git_dir: &Path, folder: &Path, index: &Path) -> At {
        At {
            dir: folder.to_path_buf(),
            env: vec![
                ("GIT_DIR", git_dir.to_path_buf()),
                ("GIT_WORK_TREE", folder.to_path_buf()),
                ("GIT_INDEX_FILE", index.to_path_buf()),
            ],
        }
    }

    pub fn git(&self) -> Git {
        let mut git = Git::new(&self.dir);
        for (key, value) in &self.env {
            git = git.env(key, value);
        }
        git
    }
}

fn nul_list(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// The index entries of the worktree at `dir`: path to (mode, blob).
fn index_entries(at: &At) -> Result<BTreeMap<String, (String, String)>, Error> {
    let out = at
        .git()
        .args(["ls-files", "-z", "-s"])
        .run_bytes()
        .map_err(git_err)?;
    let mut entries = BTreeMap::new();
    for record in nul_list(&out) {
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split(' ');
        if let (Some(mode), Some(blob)) = (fields.next(), fields.next()) {
            entries.insert(path.to_owned(), (mode.to_owned(), blob.to_owned()));
        }
    }
    Ok(entries)
}

/// The paths of the worktree at `dir` whose index entry is `skip-worktree`.
fn skipped(at: &At) -> Result<Vec<String>, Error> {
    let out = at
        .git()
        .args(["ls-files", "-z", "-v"])
        .run_bytes()
        .map_err(git_err)?;
    Ok(nul_list(&out)
        .into_iter()
        .filter_map(|r| r.strip_prefix("S ").map(str::to_owned))
        .collect())
}

/// Read blob `id` in `dir`, if it could be a pointer.
fn read_pointer(at: &At, id: &str) -> Result<Option<Pointer>, Error> {
    let size = at
        .git()
        .args(["cat-file", "-s", id])
        .run()
        .map_err(git_err)?;
    if size.trim().parse::<u64>().unwrap_or(u64::MAX) > POINTER_MAX {
        return Ok(None);
    }
    let bytes = at
        .git()
        .args(["cat-file", "blob", id])
        .run_bytes()
        .map_err(git_err)?;
    Ok(Pointer::parse(&bytes))
}

/// Write `text` as a blob in `dir`'s repository.
pub(crate) fn write_blob(at: &At, text: &str) -> Result<String, Error> {
    Ok(at
        .git()
        .args(["hash-object", "-w", "--stdin"])
        .stdin(text.as_bytes().to_vec())
        .run()
        .map_err(git_err)?
        .trim()
        .to_owned())
}

/// Set or clear `skip-worktree` on `paths`.
fn mark(at: &At, paths: &[String], skip: bool) -> Result<(), Error> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    at.git()
        .args(["update-index", "-z"])
        .arg(match skip {
            true => "--skip-worktree",
            false => "--no-skip-worktree",
        })
        .arg("--stdin")
        .stdin(input)
        .run()
        .map_err(git_err)?;
    Ok(())
}

/// The files git would add in `dir` (tracked, and untracked but not
/// ignored) that are regular files of at least `threshold` bytes, with
/// whether each is executable.
fn large_files(at: &At, threshold: u64) -> Result<BTreeMap<String, bool>, Error> {
    let out = at
        .git()
        .args(["ls-files", "-z", "-c", "-o", "--exclude-standard"])
        .run_bytes()
        .map_err(git_err)?;
    let mut large = BTreeMap::new();
    for path in nul_list(&out) {
        let Ok(meta) = fs::symlink_metadata(at.dir.join(&path)) else {
            continue;
        };
        if meta.is_file() && meta.len() >= threshold {
            large.insert(path, is_executable(&meta));
        }
    }
    Ok(large)
}

/// Before a snapshot of the worktree at `dir`: every large file becomes
/// a pointer in the index (marked `skip-worktree`, so `git add` and
/// `git status` leave the real file alone), and a file that is no longer
/// large, or is gone, is handed back to git. Returns the paths staged as
/// pointers.
pub(crate) fn stage(at: &At, large: &Large) -> Result<Vec<String>, Error> {
    let entries = index_entries(at)?;
    let skipped = skipped(at)?;
    let files = large_files(at, large.threshold)?;
    // A skipped path that is no longer a large file goes back to git.
    let released: Vec<String> = skipped
        .iter()
        .filter(|p| !files.contains_key(*p))
        .cloned()
        .collect();
    mark(at, &released, false)?;
    let mut staged = Vec::new();
    let mut info = String::new();
    for (path, executable) in &files {
        let mode = match executable {
            true => "100755",
            false => "100644",
        };
        let current = match entries.get(path) {
            Some((m, blob)) if m == mode => read_pointer(at, blob)?,
            _ => None,
        };
        let unchanged = match &current {
            Some(pointer) => hash_file(&at.dir.join(path))? == pointer.blake3,
            None => false,
        };
        if !unchanged {
            let pointer = large.store.store_file(&at.dir.join(path))?;
            let blob = write_blob(at, &pointer.render())?;
            info.push_str(&format!("{mode} {blob}\t{path}\0"));
        }
        staged.push(path.clone());
    }
    if !info.is_empty() {
        at.git()
            .args(["update-index", "-z", "--add", "--index-info"])
            .stdin(info.into_bytes())
            .run()
            .map_err(git_err)?;
    }
    mark(at, &staged, true)?;
    Ok(staged)
}

/// The index entries of `dir` that are pointers, with their mode.
fn pointers_in_index(at: &At) -> Result<Vec<(String, String, Pointer)>, Error> {
    let entries = index_entries(at)?;
    let (out, _) = at
        .git()
        .args(["grep", "--cached", "-l", "-z", "-e"])
        .arg(format!("^{POINTER_HEADER}$"))
        .output()
        .map_err(git_err)?;
    let mut found = Vec::new();
    for path in nul_list(&out.stdout) {
        let Some((mode, blob)) = entries.get(&path) else {
            continue;
        };
        if let Some(pointer) = read_pointer(at, blob)? {
            found.push((path, mode.clone(), pointer));
        }
    }
    Ok(found)
}

/// After a checkout in the worktree at `dir` (a new worktree, a rewind):
/// every pointer in the index is replaced on disk by the file it names,
/// unless the file there already is it, and marked `skip-worktree`.
/// Returns the paths restored.
pub(crate) fn restore(at: &At, large: &Large) -> Result<Vec<String>, Error> {
    let mut restored = Vec::new();
    let mut marked = Vec::new();
    for (path, mode, pointer) in pointers_in_index(at)? {
        let dest = at.dir.join(&path);
        let current = match fs::symlink_metadata(&dest) {
            Ok(meta) if meta.is_file() => Some(hash_file(&dest)?),
            _ => None,
        };
        if current.as_deref() != Some(pointer.blake3.as_str()) {
            large
                .store
                .restore_file(&pointer, &dest, mode == "100755")?;
            restored.push(path.clone());
        }
        marked.push(path);
    }
    mark(at, &marked, true)?;
    Ok(restored)
}

/// Clear `skip-worktree` everywhere in `dir`, so a reset may replace the
/// files it covered.
pub(crate) fn release(at: &At) -> Result<(), Error> {
    let skipped = skipped(at)?;
    mark(at, &skipped, false)
}

/// The large files of `dir` changed since their pointer was staged: what
/// `git status` cannot see.
pub(crate) fn dirty(at: &At) -> Result<Vec<String>, Error> {
    let entries = index_entries(at)?;
    let mut changed = Vec::new();
    for path in skipped(at)? {
        let Some((_, blob)) = entries.get(&path) else {
            continue;
        };
        let Some(pointer) = read_pointer(at, blob)? else {
            continue;
        };
        let dest = at.dir.join(&path);
        let same = match fs::symlink_metadata(&dest) {
            Ok(meta) if meta.is_file() => hash_file(&dest)? == pointer.blake3,
            _ => false,
        };
        if !same {
            changed.push(path);
        }
    }
    Ok(changed)
}

/// Every chunk hash named by a pointer in any tree reachable from `rev`
/// in the repository at `dir` (a work tree or a git directory).
pub(crate) fn chunks_reachable(dir: &Path, rev: &str) -> Result<BTreeSet<String>, Error> {
    let objects = Git::new(dir)
        .args(["rev-list", "--objects", "--no-object-names", rev])
        .run()
        .map_err(git_err)?;
    let mut input = Vec::new();
    for id in objects.lines().filter(|l| !l.is_empty()) {
        input.extend_from_slice(id.as_bytes());
        input.push(b'\n');
    }
    let checked = Git::new(dir)
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .stdin(input)
        .run()
        .map_err(git_err)?;
    let header_len = POINTER_HEADER.len() as u64;
    let mut wanted = Vec::new();
    for line in checked.lines() {
        let mut fields = line.split(' ');
        if let (Some(id), Some("blob"), Some(size)) = (fields.next(), fields.next(), fields.next())
        {
            let size = size.parse::<u64>().unwrap_or(u64::MAX);
            if size > header_len && size <= POINTER_MAX {
                wanted.extend_from_slice(id.as_bytes());
                wanted.push(b'\n');
            }
        }
    }
    let mut chunks = BTreeSet::new();
    if wanted.is_empty() {
        return Ok(chunks);
    }
    let batch = Git::new(dir)
        .args(["cat-file", "--batch"])
        .stdin(wanted)
        .run_bytes()
        .map_err(git_err)?;
    let mut at = 0;
    while at < batch.len() {
        let Some(end) = batch[at..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&batch[at..at + end]).into_owned();
        at += end + 1;
        let size = header
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let body = &batch[at..(at + size).min(batch.len())];
        at += size + 1;
        if let Some(pointer) = Pointer::parse(body) {
            chunks.extend(pointer.chunks.into_iter().map(|(hash, _)| hash));
        }
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic bytes (xorshift), so chunk boundaries are stable.
    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn chunks_stay_within_bounds_and_cover_the_data() {
        let data = noise(9 * 1024 * 1024, 7);
        let lengths = chunk_lengths(&data);
        assert_eq!(lengths.iter().sum::<usize>(), data.len());
        let (last, rest) = lengths.split_last().unwrap();
        assert!(
            rest.iter().all(|l| (MIN_CHUNK..=MAX_CHUNK).contains(l)),
            "{lengths:?}"
        );
        assert!(*last <= MAX_CHUNK);
        assert!(lengths.len() > 4, "{lengths:?}");
    }

    #[test]
    fn an_insertion_moves_only_nearby_cut_points() {
        let data = noise(4 * 1024 * 1024, 11);
        let mut edited = data[..100_000].to_vec();
        edited.extend_from_slice(b"an insertion near the start");
        edited.extend_from_slice(&data[100_000..]);
        let hashes = |d: &[u8]| {
            let mut at = 0;
            let mut out = BTreeSet::new();
            for length in chunk_lengths(d) {
                out.insert(blake3::hash(&d[at..at + length]).to_hex().to_string());
                at += length;
            }
            out
        };
        let (a, b) = (hashes(&data), hashes(&edited));
        let shared = a.intersection(&b).count();
        assert!(shared + 2 >= a.len(), "{shared} of {} shared", a.len());
    }

    #[test]
    fn pointers_round_trip_and_reject_anything_else() {
        let pointer = Pointer {
            size: 30,
            blake3: "a".repeat(64),
            chunks: vec![("b".repeat(64), 10), ("c".repeat(64), 20)],
        };
        let text = pointer.render();
        assert!(
            text.starts_with("branchyard-chunked/1\nsize 30\n"),
            "{text}"
        );
        assert_eq!(Pointer::parse(text.as_bytes()), Some(pointer.clone()));
        let empty = Pointer {
            size: 0,
            blake3: "d".repeat(64),
            chunks: Vec::new(),
        };
        assert_eq!(Pointer::parse(empty.render().as_bytes()), Some(empty));
        // Sizes that do not add up, a missing newline, a bad hash, extra text.
        for bad in [
            text.replace("size 30", "size 31"),
            text.trim_end().to_owned(),
            text.replace(&"b".repeat(64), "xyz"),
            format!("{text}more\n"),
            "branchyard-chunked/2\nsize 0\n".to_owned(),
        ] {
            assert_eq!(Pointer::parse(bad.as_bytes()), None, "{bad}");
        }
    }

    #[test]
    fn the_store_deduplicates_and_verifies() {
        let dir = std::env::temp_dir().join(format!("by-chunks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = ChunkStore::new(&dir);
        let data = noise(3 * 1024 * 1024, 3);
        let file = dir.join("in.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, &data).unwrap();
        let first = store.store_file(&file).unwrap();
        let second = store.store_file(&file).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.size, data.len() as u64);
        assert_eq!(first.blake3, blake3::hash(&data).to_hex().to_string());
        let out = dir.join("out/restored.bin");
        store.restore_file(&first, &out, false).unwrap();
        assert_eq!(fs::read(&out).unwrap(), data);
        // A corrupted chunk is refused, and nothing is written.
        let victim = store.path(&first.chunks[0].0);
        fs::write(&victim, b"not the chunk").unwrap();
        let again = dir.join("out/again.bin");
        let error = store.restore_file(&first, &again, false).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert!(!again.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
