//! Bounded, authorized document previews derived from immutable artifacts.
//!
//! Conversion runs in a networkless Bubblewrap namespace. Cache entries contain
//! derivatives only; every HTTP request reads the original through its current
//! artifact grant before using a cached response or image.

mod cache;
mod routes;
mod sandbox;

pub(crate) use routes::{authorized_input, router};

use cache::{cache_key, materialize_helper, safe_asset, suffix, validate_response};

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::ApiError;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_INPUT: u64 = 64 * 1024 * 1024;
const MAX_CONVERSIONS: usize = 2;
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
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
    slots: Arc<Semaphore>,
    settings: sandbox::Settings,
}

struct Cached {
    value: Value,
    image: Option<Vec<u8>>,
}

/// Admission held from original materialization through derivative delivery.
///
/// HTTP adapters acquire this before reading the original. Its methods use the
/// same permit throughout conversion; they never acquire a second slot.
pub struct PreviewPermit<'a> {
    service: &'a PreviewService,
    admission: MaterializationGuard,
}

/// Admission retained by a blocking original reader, even after disconnection.
#[derive(Clone)]
pub struct MaterializationGuard {
    _permit: Arc<OwnedSemaphorePermit>,
}

impl PreviewPermit<'_> {
    /// Retain this in a blocking loader so cancellation cannot free its slot early.
    pub fn materialization_guard(&self) -> MaterializationGuard {
        self.admission.clone()
    }
    /// Preview already-authorized bytes while retaining this request's slot.
    pub async fn preview(
        self,
        input: PreviewInput,
        options: PreviewOptions,
    ) -> Result<Value, ApiError> {
        Ok(self
            .service
            .derive(input, options, self.admission.clone())
            .await?
            .value)
    }

    /// Return the matching PNG bytes, pinned before releasing the cache lock.
    pub async fn asset(
        self,
        input: PreviewInput,
        options: PreviewOptions,
        asset: &str,
    ) -> Result<Vec<u8>, ApiError> {
        if !safe_asset(asset) {
            return Err(unknown_asset());
        }
        let cached = self
            .service
            .derive(input, options, self.admission.clone())
            .await?;
        if cached.value.get("image").and_then(Value::as_str) != Some(asset) {
            return Err(unknown_asset());
        }
        cached.image.ok_or_else(unknown_asset)
    }
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
            slots: Arc::new(Semaphore::new(MAX_CONVERSIONS)),
            settings,
        })
    }

    /// Admit a request before copying or allocating original artifact bytes.
    pub fn admit(&self) -> Result<PreviewPermit<'_>, ApiError> {
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "preview_busy",
                "both document renderers are busy; retry shortly",
            )
            .detail(json!({"retry_after": 2}))
        })?;
        Ok(PreviewPermit {
            service: self,
            admission: MaterializationGuard {
                _permit: Arc::new(slot),
            },
        })
    }

    /// Reject malformed image names before admitting or materializing a source.
    pub fn admit_asset(&self, asset: &str) -> Result<PreviewPermit<'_>, ApiError> {
        if !safe_asset(asset) {
            return Err(unknown_asset());
        }
        self.admit()
    }

    /// Derive a preview for an input the caller has already materialized.
    ///
    /// HTTP handlers use [`Self::admit`] before reading their original instead.
    pub async fn preview(
        &self,
        input: PreviewInput,
        options: PreviewOptions,
    ) -> Result<Value, ApiError> {
        self.admit()?.preview(input, options).await
    }

    /// Read exactly the image named by this authorized preview response.
    pub async fn asset(
        &self,
        input: PreviewInput,
        options: PreviewOptions,
        asset: &str,
    ) -> Result<Vec<u8>, ApiError> {
        self.admit_asset(asset)?.asset(input, options, asset).await
    }

    async fn derive(
        &self,
        input: PreviewInput,
        options: PreviewOptions,
        admission: MaterializationGuard,
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
        let runtime = self.settings.runtime().await?;
        let key = cache_key(&input, &options, &runtime.identity())?;
        if let Some(cached) =
            cache::lookup(self.root.clone(), key.clone(), admission.clone()).await?
        {
            return Ok(cached);
        }
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
            return Ok(Cached { value, image: None });
        }
        let derived = output.join("derived");
        if matches!(
            value.get("kind").and_then(Value::as_str),
            Some("table" | "text")
        ) {
            // JSON-only helper modes intentionally create no output directory.
            match fs::create_dir(&derived) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        validate_response(&value, &derived)?;
        fs::write(
            derived.join("result.json"),
            serde_json::to_vec(&value).map_err(json_error)?,
        )
        .map_err(io_error)?;
        cache::publish(self.root.clone(), key, derived, value, admission).await
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
