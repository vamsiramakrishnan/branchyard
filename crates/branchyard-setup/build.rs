//! Embed every text file under `plugins/branchyard` in the library, so
//! `by init plugin` installs exactly what the plugin ships without a
//! checkout. Writes `$OUT_DIR/bundle.rs`: a sorted list of
//! `(path relative to plugins/branchyard, include_str!(its absolute path))`.
//! A file that is not UTF-8 text is left out with a warning; the install
//! manifests may not name one (`install::tests` checks every `from`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == "__pycache__" || name == ".DS_Store" {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&entry.path(), out),
            Ok(t) if t.is_file() => out.push(entry.path()),
            _ => {}
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let root = manifest.join("../../plugins/branchyard").canonicalize()?;
    // A directory is scanned whole: any file added, changed or removed
    // under it re-runs this script.
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)?
            .to_string_lossy()
            .replace('\\', "/");
        if relative.ends_with(".pyc") {
            continue;
        }
        if std::fs::read_to_string(&path).is_err() {
            println!("cargo:warning=plugins/branchyard/{relative} is not UTF-8 text; not embedded");
            continue;
        }
        entries.push((relative, path));
    }
    entries.sort();
    let mut code = String::from("pub const FILES: &[(&str, &str)] = &[\n");
    for (relative, path) in &entries {
        writeln!(
            code,
            "    ({relative:?}, include_str!({:?})),",
            path.display().to_string()
        )?;
    }
    code.push_str("];\n");
    std::fs::write(
        PathBuf::from(std::env::var("OUT_DIR")?).join("bundle.rs"),
        code,
    )?;
    Ok(())
}
