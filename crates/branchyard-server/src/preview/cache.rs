//! Private immutable derivative cache and embedded helper staging.

use std::fs;
use std::path::Path as FilePath;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    io_error, json_error, ApiError, Cached, MaterializationGuard, PreviewInput, PreviewOptions,
    MAX_CONVERSIONS, MAX_RESPONSE_BYTES,
};

pub(super) const MAX_IMAGE: u64 = 16 * 1024 * 1024;
const MAX_JSON: u64 = MAX_RESPONSE_BYTES;
const MAX_DERIVATIVE: u64 = 64 * 1024 * 1024;
const CACHE_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const CACHE_ENTRIES: usize = 256;
const CACHE_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Select only supported filename suffixes; names never become paths.
pub(super) fn suffix(name: &str) -> Result<&str, ApiError> {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or_default();
    match extension.to_ascii_lowercase().as_str() {
        "csv" => Ok("csv"),
        "xlsx" => Ok("xlsx"),
        "docx" => Ok("docx"),
        "pptx" => Ok("pptx"),
        "pdf" => Ok("pdf"),
        "png" => Ok("png"),
        "jpg" | "jpeg" => Ok("jpg"),
        "webp" => Ok("webp"),
        "txt" => Ok("txt"),
        "md" | "markdown" => Ok("md"),
        "json" => Ok("json"),
        _ => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "preview_unsupported",
            "preview supports CSV, XLSX, DOCX, PPTX, PDF and raster images",
        )),
    }
}

/// Bind source identity, bytes, options and trusted renderer payload.
pub(super) fn cache_key(
    input: &PreviewInput,
    options: &PreviewOptions,
    runtime: &str,
) -> Result<String, ApiError> {
    let mut hash = Sha256::new();
    hash.update(
        serde_json::to_vec(&(
            input.tenant.as_str(),
            input.digest.as_str(),
            input.media_type.as_str(),
            suffix(&input.name)?,
            options,
            runtime,
            MAX_RESPONSE_BYTES,
            env!("CARGO_PKG_VERSION"),
        ))
        .map_err(json_error)?,
    );
    hash.update(include_str!("launcher.py"));
    hash.update(Sha256::digest(&input.bytes));
    for (path, contents) in helper_files()? {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(contents.as_bytes());
        hash.update([0]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Select only embedded Python modules and their trusted JSON resources.
pub(super) fn helper_files() -> Result<impl Iterator<Item = (&'static str, &'static str)>, ApiError>
{
    let skill = branchyard_setup::skills::skill("artifact-tools")
        .ok_or_else(|| ApiError::internal("embedded artifact toolkit is missing"))?;
    Ok(skill.files.iter().filter_map(|(path, contents)| {
        path.strip_prefix("scripts/")
            .filter(|path| {
                path.starts_with("branchyard_artifacts/")
                    && (path.ends_with(".py") || path.ends_with(".json"))
            })
            .map(|relative| (relative, *contents))
    }))
}

/// Write the selected helper into an owned private directory.
pub(super) fn materialize_helper(root: &FilePath) -> Result<(), ApiError> {
    for (path, contents) in helper_files()? {
        let path = root.join(path);
        let parent = path
            .parent()
            .ok_or_else(|| ApiError::internal("invalid embedded helper path"))?;
        fs::create_dir_all(parent).map_err(io_error)?;
        fs::write(path, contents).map_err(io_error)?;
    }
    fs::write(root.join("launcher.py"), include_str!("launcher.py")).map_err(io_error)
}

/// Accept only a single PNG basename, never a path or URL.
pub(super) fn safe_asset(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.contains("..")
        && name.ends_with(".png")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Read a bounded regular file while refusing symbolic links.
pub(super) fn read_regular(path: &FilePath, limit: u64) -> Result<Vec<u8>, ApiError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(ApiError::internal(
            "preview derivative is not a bounded regular file",
        ));
    }
    let bytes = fs::read(path).map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(ApiError::internal("preview derivative exceeded its bound"));
    }
    Ok(bytes)
}

/// Check response shape and every output before cache publication.
pub(super) fn validate_response(value: &Value, output: &FilePath) -> Result<(), ApiError> {
    let invalid = || ApiError::internal("renderer returned an invalid bounded preview");
    let directory = fs::symlink_metadata(output).map_err(io_error)?;
    if !directory.is_dir() || directory.file_type().is_symlink() {
        return Err(invalid());
    }
    if value.get("status").and_then(Value::as_str) != Some("passed") {
        return Err(invalid());
    }
    match value.get("kind").and_then(Value::as_str) {
        Some("table") => {
            let columns = value
                .get("columns")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            let rows = value
                .get("rows")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            if columns.len() > 256
                || rows.len() > 500
                || rows
                    .iter()
                    .any(|row| row.as_array().is_none_or(|cells| cells.len() > 256))
            {
                return Err(invalid());
            }
        }
        Some("pages") => {
            let image = value
                .get("image")
                .and_then(Value::as_str)
                .filter(|name| safe_asset(name))
                .ok_or_else(invalid)?;
            let bytes = read_regular(&output.join(image), MAX_IMAGE)?;
            if !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                || value
                    .get("pages")
                    .and_then(Value::as_u64)
                    .is_none_or(|pages| pages == 0 || pages > 500)
            {
                return Err(invalid());
            }
        }
        Some("text")
            if value
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.len() <= 256 * 1024) => {}
        _ => return Err(invalid()),
    }
    let mut total = 0;
    for entry in fs::read_dir(output).map_err(io_error)? {
        let metadata = fs::symlink_metadata(entry.map_err(io_error)?.path()).map_err(io_error)?;
        if !metadata.is_file() || metadata.len() > 32 * 1024 * 1024 {
            return Err(invalid());
        }
        total += metadata.len();
        if total > MAX_DERIVATIVE {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Read and validate one completed, unexpired cache entry.
pub(super) fn read_cached(directory: &FilePath) -> Result<Option<Value>, ApiError> {
    if !directory.exists() {
        return Ok(None);
    }
    if fs::symlink_metadata(directory)
        .map_err(io_error)?
        .file_type()
        .is_symlink()
    {
        return Err(ApiError::internal("linked preview cache entry refused"));
    }
    let bytes = read_regular(&directory.join("result.json"), MAX_JSON)?;
    let modified = fs::metadata(directory.join("result.json"))
        .and_then(|metadata| metadata.modified())
        .map_err(io_error)?;
    let now = branchyard_support::time::now_ms();
    let modified = branchyard_support::time::system_time_ms(modified).unwrap_or(now);
    if !fresh(modified, now) {
        return Err(ApiError::internal("preview cache entry expired"));
    }
    let value = serde_json::from_slice(&bytes).map_err(json_error)?;
    validate_response(&value, directory)?;
    Ok(Some(value))
}

/// Cache age uses the shared clock and never treats a future stamp as old.
pub(super) fn fresh(modified: u64, now: u64) -> bool {
    now.saturating_sub(modified) < CACHE_AGE_MS
}

/// Discard an incomplete derivative so a retry can derive it from the original.
pub(super) fn reusable_cache(directory: &FilePath) -> Result<Option<Value>, ApiError> {
    match read_cached(directory) {
        Ok(value) => Ok(value),
        Err(error) => {
            let metadata = match fs::symlink_metadata(directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_error(error)),
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(error);
            }
            remove_entry(directory)?;
            Ok(None)
        }
    }
}

fn remove_entry(path: &FilePath) -> Result<(), ApiError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

/// Read or publish cache state while holding the same short cross-process lock.
async fn locked<T: Send + 'static>(
    root: std::path::PathBuf,
    action: impl FnOnce(&FilePath) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    crate::api::blocking(move || {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".lock"))
            .map_err(io_error)?;
        file.lock().map_err(io_error)?;
        // A distinct file description for each critical section also serializes
        // threads in this process. Closing it releases the lock on every exit.
        action(&root)
    })
    .await?
}

fn pin(value: Value, directory: &FilePath) -> Result<Cached, ApiError> {
    let image = match value.get("image").and_then(Value::as_str) {
        Some(name) if safe_asset(name) => Some(read_regular(&directory.join(name), MAX_IMAGE)?),
        _ => None,
    };
    Ok(Cached { value, image })
}

/// Pin a cache hit's image bytes before any eviction can remove its files.
pub(super) async fn lookup(
    root: std::path::PathBuf,
    key: String,
    admission: MaterializationGuard,
) -> Result<Option<Cached>, ApiError> {
    locked(root, move |root| {
        let _admission = admission;
        let directory = root.join(key);
        match reusable_cache(&directory)? {
            Some(value) => Ok(Some(pin(value, &directory)?)),
            None => {
                cleanup_cache(root)?;
                Ok(None)
            }
        }
    })
    .await
}

/// Atomically publish derivatives under the read/eviction lock, then pin bytes.
pub(super) async fn publish(
    root: std::path::PathBuf,
    key: String,
    staged: std::path::PathBuf,
    value: Value,
    admission: MaterializationGuard,
) -> Result<Cached, ApiError> {
    locked(root, move |root| {
        let _admission = admission;
        let directory = root.join(key);
        if let Some(existing) = reusable_cache(&directory)? {
            return pin(existing, &directory);
        }
        cleanup_cache(root)?;
        fs::rename(staged, &directory).map_err(io_error)?;
        pin(value, &directory)
    })
    .await
}

/// Evict expired/over-budget owned entries and abandoned staging directories.
pub(super) fn cleanup_cache(root: &FilePath) -> Result<(), ApiError> {
    let now = branchyard_support::time::now_ms();
    let mut entries = Vec::new();
    for entry in fs::read_dir(root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(error)),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let modified =
            branchyard_support::time::system_time_ms(metadata.modified().map_err(io_error)?)
                .unwrap_or(now);
        if name.starts_with(".stage-")
            && now.saturating_sub(modified) > Duration::from_secs(600).as_millis() as u64
        {
            remove_entry(&entry.path())?;
        } else if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut bytes = 0;
            let files = match fs::read_dir(entry.path()) {
                Ok(files) => files,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(error)),
            };
            for file in files {
                let file = file.map_err(io_error)?;
                match file.metadata() {
                    Ok(metadata) => bytes += metadata.len(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error(error)),
                }
            }
            entries.push((modified, entry.path(), bytes));
        }
    }
    entries.sort_by_key(|entry| entry.0);
    let mut total: u64 = entries.iter().map(|entry| entry.2).sum();
    let mut count = entries.len();
    for (modified, path, bytes) in entries {
        if count < CACHE_ENTRIES
            && total.saturating_add(MAX_DERIVATIVE.saturating_mul(MAX_CONVERSIONS as u64))
                <= CACHE_BYTES
            && now.saturating_sub(modified) < CACHE_AGE_MS
        {
            break;
        }
        remove_entry(&path)?;
        count -= 1;
        total = total.saturating_sub(bytes);
    }
    Ok(())
}
