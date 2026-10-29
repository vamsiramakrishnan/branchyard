//! Bounded, authorized document previews derived from immutable artifacts.
//!
//! Conversion runs in a networkless Bubblewrap namespace. Cache entries contain
//! derivatives only; every HTTP request reads the original through its current
//! artifact grant before using a cached response or image.

mod cache;
mod routes;
mod sandbox;

pub(crate) use routes::{authorized_input, router};

use cache::{
    cache_key, cleanup_cache, materialize_helper, read_regular, reusable_cache, safe_asset, suffix,
    validate_response, MAX_IMAGE,
};

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use crate::error::ApiError;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const MAX_INPUT: u64 = 64 * 1024 * 1024;
const MAX_CONVERSIONS: usize = 2;
/// The requested document representation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum View {
    /// Choose a table for CSV/XLSX and pages for documents and images.
    #[default]
    Auto,
    /// Show inert CSV or spreadsheet cells.
    Table,
    /// Show one rasterized document page or slide.
    Pages,
}

impl View {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Table => "table",
            Self::Pages => "pages",
        }
    }
}

/// Bounded options included in the derivative's content identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreviewOptions {
    /// Requested representation.
    pub view: View,
    /// One-based page or slide number.
    pub page: u32,
    /// An optional worksheet name.
    pub sheet: Option<String>,
    /// Zero-based table row offset.
    pub offset: u32,
    /// Maximum table rows returned.
    pub limit: u32,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            view: View::Auto,
            page: 1,
            sheet: None,
            offset: 0,
            limit: 100,
        }
    }
}

impl PreviewOptions {
    /// Parse the route's query, refusing ambiguous or unsupported options.
    pub fn parse(query: Option<&str>) -> Result<Self, ApiError> {
        let mut options = Self::default();
        let mut seen = BTreeSet::new();
        for pair in query
            .unwrap_or_default()
            .split('&')
            .filter(|s| !s.is_empty())
        {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| ApiError::bad_request("preview options need a value"))?;
            let key = branchyard_client::http::decode(key);
            let value = branchyard_client::http::decode(value);
            if !seen.insert(key.clone()) {
                return Err(ApiError::bad_request("duplicate preview option"));
            }
            let number = || {
                value.parse::<u32>().map_err(|_| {
                    ApiError::bad_request("preview numbers must be non-negative integers")
                })
            };
            match key.as_str() {
                "view" => {
                    options.view = match value.as_str() {
                        "auto" => View::Auto,
                        "table" => View::Table,
                        "pages" => View::Pages,
                        _ => return Err(ApiError::bad_request("unknown preview view")),
                    }
                }
                "page" => options.page = number()?,
                "offset" => options.offset = number()?,
                "limit" => options.limit = number()?,
                "sheet" => options.sheet = Some(value),
                _ => return Err(ApiError::bad_request("unknown preview option")),
            }
        }
        options.validate()?;
        Ok(options)
    }

    fn validate(&self) -> Result<(), ApiError> {
        if !(1..=500).contains(&self.page)
            || !(1..=500).contains(&self.limit)
            || self.offset > 100_000
            || self.sheet.as_ref().is_some_and(|sheet| {
                sheet.is_empty() || sheet.len() > 256 || sheet.chars().any(char::is_control)
            })
        {
            return Err(ApiError::bad_request("preview options exceed their bounds"));
        }
        Ok(())
    }
}

/// Original bytes already authorized and digest-checked by the caller.
pub struct PreviewInput {
    /// Artifact bytes, never modified by conversion.
    pub bytes: Vec<u8>,
    /// Original display name, used only to select a known format suffix.
    pub name: String,
    /// Recorded media type, retained as part of the cache identity.
    pub media_type: String,
    /// Immutable artifact digest.
    pub digest: String,
    /// Authenticated tenant, never a client-supplied tenant override.
    pub tenant: String,
}

/// Isolated renderer and its private, bounded derivative cache.
pub struct PreviewService {
    root: PathBuf,
    slots: Semaphore,
    settings: sandbox::Settings,
}

struct Cached {
    value: Value,
    directory: Option<PathBuf>,
}

impl PreviewService {
    /// Open a private cache. Missing renderer executables are reported on use.
    pub fn new(root: PathBuf) -> Result<Self, ApiError> {
        Self::with_settings(root, sandbox::Settings::from_env())
    }

    fn with_settings(root: PathBuf, settings: sandbox::Settings) -> Result<Self, ApiError> {
        fs::create_dir_all(&root).map_err(io_error)?;
        if fs::symlink_metadata(&root)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
        {
            return Err(ApiError::internal("preview cache must not be a symlink"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        }
        Ok(Self {
            root: fs::canonicalize(root).map_err(io_error)?,
            slots: Semaphore::new(MAX_CONVERSIONS),
            settings,
        })
    }

    /// Derive one bounded representation from authorized immutable bytes.
    pub async fn preview(
        &self,
        input: PreviewInput,
        options: PreviewOptions,
    ) -> Result<Value, ApiError> {
        Ok(self.derive(input, options).await?.value)
    }

    /// Read exactly the image named by this authorized preview response.
    pub async fn asset(
        &self,
        input: PreviewInput,
        options: PreviewOptions,
        asset: &str,
    ) -> Result<Vec<u8>, ApiError> {
        if !safe_asset(asset) {
            return Err(unknown_asset());
        }
        let cached = self.derive(input, options).await?;
        if cached.value.get("image").and_then(Value::as_str) != Some(asset) {
            return Err(unknown_asset());
        }
        let directory = cached.directory.ok_or_else(unknown_asset)?;
        read_regular(&directory.join(asset), MAX_IMAGE)
    }

    async fn derive(
        &self,
        input: PreviewInput,
        options: PreviewOptions,
    ) -> Result<Cached, ApiError> {
        options.validate()?;
        if input.bytes.len() as u64 > MAX_INPUT {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "preview_too_large",
                "document previews accept at most 64 MiB",
            ));
        }
        let suffix = suffix(&input.name)?;
        let _slot = self.slots.try_acquire().map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "preview_busy",
                "both document renderers are busy; retry shortly",
            )
            .detail(json!({"retry_after": 2}))
        })?;
        let runtime = self.settings.runtime().await?;
        let key = cache_key(&input, &options, &runtime.identity())?;
        let final_dir = self.root.join(&key);
        if let Some(value) = reusable_cache(&final_dir)? {
            return Ok(Cached {
                value,
                directory: Some(final_dir),
            });
        }
        cleanup_cache(&self.root)?;
        let stage = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(&self.root)
            .map_err(io_error)?;
        let input_dir = stage.path().join("input");
        let helper = stage.path().join("helper");
        let output = stage.path().join("output");
        for dir in [&input_dir, &helper, &output] {
            fs::create_dir(dir).map_err(io_error)?;
        }
        let source = input_dir.join(format!("source.{suffix}"));
        fs::write(&source, &input.bytes).map_err(io_error)?;
        materialize_helper(&helper)?;
        let value = sandbox::convert(
            &self.settings,
            &runtime,
            &source,
            &helper,
            &output,
            &options,
        )
        .await?;
        if value.get("status").and_then(Value::as_str) != Some("passed") {
            return Ok(Cached {
                value,
                directory: None,
            });
        }
        let derived = output.join("derived");
        validate_response(&value, &derived)?;
        fs::write(
            derived.join("result.json"),
            serde_json::to_vec(&value).map_err(json_error)?,
        )
        .map_err(io_error)?;
        match fs::rename(&derived, &final_dir) {
            Ok(()) => {}
            Err(error) => {
                // Another server may have published the same immutable derivative.
                if let Some(value) = reusable_cache(&final_dir)? {
                    return Ok(Cached {
                        value,
                        directory: Some(final_dir),
                    });
                }
                return Err(io_error(error));
            }
        }
        Ok(Cached {
            value,
            directory: Some(final_dir),
        })
    }
}

fn io_error(error: std::io::Error) -> ApiError {
    ApiError::internal(format!("preview storage: {error}"))
}
fn json_error(error: serde_json::Error) -> ApiError {
    ApiError::internal(format!("preview JSON: {error}"))
}
fn unknown_asset() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_preview_asset",
        "preview image not found",
    )
}

#[cfg(test)]
mod tests;
