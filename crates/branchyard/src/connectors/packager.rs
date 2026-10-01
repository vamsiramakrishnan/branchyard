//! Where connector packages come from: the bundles a gateway serves, the
//! harness package of each (`anvil package harness`), and the index of a
//! grant (`anvil connectors index`). [`Packager`] is the seam; tests use a
//! fake, [`AnvilPackager`] runs Anvil's CLI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A bundle a gateway serves: a directory with a canonical `air.yaml` or
/// `air.json`, named by its path under the bundle root (the connector id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    /// The connector id: the bundle's path under the root, folded as the
    /// fleet folds it (`shipping/v2` is `shipping_v2`).
    pub id: String,
    pub path: PathBuf,
    /// The bundle's content hash (BLAKE3 of its files, hex), which keys the
    /// package cache.
    pub hash: String,
}

/// Packages connectors for harnesses.
pub trait Packager: Send + Sync + std::fmt::Debug {
    /// The bundles the gateway serves.
    fn served(&self) -> Result<Vec<Bundle>, String>;
    /// Write `bundle`'s harness package, gateway mode, into `out`, an empty
    /// directory: `SKILL.md`, `reference/`, the SDKs and the CLI.
    fn package(&self, bundle: &Bundle, out: &Path) -> Result<(), String>;
    /// Write the index of `bundles` for the grant in the JSON file
    /// `grants` (a list of grant entries) to `out`.
    fn index(&self, grants: &Path, bundles: &[Bundle], out: &Path) -> Result<(), String>;
}

/// Anvil's CLI: `anvil package harness <bundle> --out <dir> --connector <id>` and
/// `anvil connectors index --grants <file> --out INDEX.md <bundle...>`,
/// over the bundles found under `root`.
#[derive(Clone, Debug)]
pub struct AnvilPackager {
    /// The command and any leading arguments, such as `["anvil"]` or
    /// `["node", "/opt/anvil/packages/cli/dist/bin-anvil.js"]`.
    pub command: Vec<String>,
    /// The bundle root the gateway serves (`anvil serve mcp <root> --fleet`).
    pub root: PathBuf,
}

impl AnvilPackager {
    fn run(&self, args: &[&str]) -> Result<(), String> {
        let (program, lead) = self
            .command
            .split_first()
            .ok_or("no anvil command is configured")?;
        let output = Command::new(program)
            .args(lead)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("could not run {program}: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr.trim().lines().last().unwrap_or("").trim();
        Err(format!(
            "anvil {} failed ({}){}",
            args.first().copied().unwrap_or(""),
            output.status,
            match last.is_empty() {
                true => String::new(),
                false => format!(": {last}"),
            }
        ))
    }
}

impl Packager for AnvilPackager {
    fn served(&self) -> Result<Vec<Bundle>, String> {
        discover(&self.root)
    }

    fn package(&self, bundle: &Bundle, out: &Path) -> Result<(), String> {
        let (path, out) = (bundle.path.display().to_string(), out.display().to_string());
        // The connector id is the folded bundle path, as the fleet serves it.
        self.run(&[
            "package",
            "harness",
            &path,
            "--out",
            &out,
            "--connector",
            &bundle.id,
        ])
    }

    fn index(&self, grants: &Path, bundles: &[Bundle], out: &Path) -> Result<(), String> {
        let (grants, out) = (grants.display().to_string(), out.display().to_string());
        let paths: Vec<String> = bundles
            .iter()
            .map(|b| b.path.display().to_string())
            .collect();
        let mut args = vec!["connectors", "index", "--grants", &grants, "--out", &out];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args)
    }
}

/// Directories skipped while looking for bundles.
const SKIP: &[&str] = &["node_modules", "target", "dist"];
/// How deep under the root a bundle may be.
const DEPTH: usize = 4;

/// Every bundle under `root`, as Anvil's fleet discovers them: a directory
/// holding `air.yaml` or `air.json`, not looked into further. Sorted by id.
pub fn discover(root: &Path) -> Result<Vec<Bundle>, String> {
    if !root.is_dir() {
        return Err(format!(
            "the bundle root {} is not a directory",
            root.display()
        ));
    }
    let mut out = Vec::new();
    walk(root, root, 0, &mut out)?;
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Bundle>) -> Result<(), String> {
    if dir != root && (dir.join("air.yaml").is_file() || dir.join("air.json").is_file()) {
        let rel = dir
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let id = branchyard_provision::connectors::fold_connector(&rel);
        if branchyard_provision::connectors::check_connector(&id).is_ok() {
            out.push(Bundle {
                hash: hash_dir(dir)?,
                id,
                path: dir.to_path_buf(),
            });
        }
        return Ok(());
    }
    if depth >= DEPTH {
        return Ok(());
    }
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || SKIP.contains(&name.as_str()) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk(root, &entry.path(), depth + 1, out)?;
        }
    }
    Ok(())
}

/// BLAKE3 over a directory's regular files: each relative path, length and
/// content, in path order. Links are not followed.
pub fn hash_dir(dir: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut hasher = blake3::Hasher::new();
    for rel in files {
        let bytes = fs::read(dir.join(&rel)).map_err(|e| format!("read {rel}: {e}"))?;
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if kind.is_dir() {
            if name == "node_modules" || name == ".git" {
                continue;
            }
            collect(root, &entry.path(), out)?;
        } else if kind.is_file() {
            let rel = entry
                .path()
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            out.push(rel);
        }
    }
    Ok(())
}

/// Copy a directory tree: directories, regular files with their modes,
/// and nothing else (links are skipped).
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("create {}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("read {}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)
                .map_err(|e| format!("copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_are_found_by_their_air_file_and_hashed_by_content() {
        let root = tempfile::tempdir().unwrap();
        let mk = |rel: &str, file: &str, body: &str| {
            let dir = root.path().join(rel);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(file), body).unwrap();
        };
        mk("github", "air.yaml", "service: github");
        mk("github/nested", "air.yaml", "not a separate bundle");
        mk("shipping/v2", "air.json", "{}");
        mk("notes", "README.md", "no air file");
        mk(".hidden", "air.yaml", "skipped");
        let found = discover(root.path()).unwrap();
        let ids: Vec<&str> = found.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["github", "shipping_v2"]);
        let before = found[0].hash.clone();
        mk("github", "air.yaml", "service: github2");
        assert_ne!(discover(root.path()).unwrap()[0].hash, before);
        assert!(discover(&root.path().join("missing")).is_err());
    }
}
