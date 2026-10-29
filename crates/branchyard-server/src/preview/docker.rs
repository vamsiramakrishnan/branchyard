//! Explicit local-daemon renderer with no host mounts or ambient credentials.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

use super::cache::{helper_files, read_regular, safe_asset, MAX_IMAGE};
use super::sandbox::{remove_internal_paths, run_input, unavailable};
use super::{io_error, json_error, ApiError, PreviewOptions, MAX_INPUT, MAX_RESPONSE_BYTES};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(45);
const PACKET_BYTES: u64 = MAX_RESPONSE_BYTES + 4 * MAX_IMAGE.div_ceil(3) + 1024;
const BOOTSTRAP: &str = include_str!("container.py");

/// Operator-selected local Docker executable, immutable image and interpreter.
#[derive(Clone)]
pub(super) struct Settings {
    pub(super) executable: PathBuf,
    pub(super) image: String,
    pub(super) python: String,
}

/// An already-local image resolved before cache lookup or conversion.
pub(super) struct Runtime {
    settings: Settings,
    image: String,
}

impl Settings {
    /// Read the explicit Docker settings without consulting ambient contexts.
    pub(super) fn from_env() -> Self {
        Self {
            executable: std::env::var_os("BRANCHYARD_PREVIEW_DOCKER")
                .map_or_else(|| PathBuf::from("docker"), PathBuf::from),
            image: std::env::var("BRANCHYARD_PREVIEW_DOCKER_IMAGE").unwrap_or_default(),
            python: std::env::var("BRANCHYARD_PREVIEW_DOCKER_PYTHON")
                .unwrap_or_else(|_| "/usr/local/bin/python3".into()),
        }
    }

    /// Verify the local image content identity and reject declared volumes.
    pub(super) async fn runtime(&self) -> Result<Runtime, ApiError> {
        if !image_id(&self.image)
            || !self.python.starts_with('/')
            || self
                .python
                .split('/')
                .any(|part| matches!(part, ".." | "."))
            || self.python.chars().any(char::is_control)
        {
            return Err(unavailable("Docker previews need BRANCHYARD_PREVIEW_DOCKER_IMAGE=sha256:<64 lowercase hex digits> for an already-local image and an absolute BRANCHYARD_PREVIEW_DOCKER_PYTHON path."));
        }
        let (status, output) = command(
            self,
            &[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                r#"{"id":{{json .Id}},"volumes":{{json (index .Config "Volumes")}}}"#.into(),
                self.image.clone().into(),
            ],
            None,
            COMMAND_TIMEOUT,
            MAX_RESPONSE_BYTES,
        )
        .await?;
        let inspected: Value = serde_json::from_slice(&output).map_err(|_| {
            unavailable("The pinned image could not report bounded local metadata.")
        })?;
        let image = inspected
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let volumes = inspected.get("volumes");
        if !status.success()
            || image != self.image
            || volumes.is_some_and(|value| {
                !value.is_null() && value.as_object().is_none_or(|v| !v.is_empty())
            })
        {
            return Err(unavailable("The pinned Docker preview image must exist locally and declare no writable volumes. Provision the artifact runtime image before serving previews; requests never pull images."));
        }
        Ok(Runtime {
            settings: self.clone(),
            image: image.into(),
        })
    }
}

impl Runtime {
    /// Bind the cache to the immutable image, interpreter and packet launcher.
    pub(super) fn identity(&self) -> String {
        // The actual invoked content ID, interpreter and packet implementation
        // all participate. Mutable tags can never reuse another image's cache.
        format!(
            "docker\n{}\n{}\n{}",
            self.image, self.settings.python, BOOTSTRAP
        )
    }
}

fn image_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn create_arguments(runtime: &Runtime) -> Vec<OsString> {
    [
        "create",
        "--rm",
        "--pull",
        "never",
        "--interactive",
        "--label",
        "io.branchyard.purpose=document-preview",
        "--network",
        "none",
        "--read-only",
        "--no-healthcheck",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "65532:65532",
        "--pids-limit",
        "128",
        "--memory",
        "768m",
        "--memory-swap",
        "768m",
        "--cpus",
        "1",
        "--ulimit",
        "nofile=128:128",
        "--log-driver",
        "none",
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,nodev,size=256m,mode=1777",
        "--tmpfs",
        "/output:rw,noexec,nosuid,nodev,size=64m,mode=1777",
        "--workdir",
        "/tmp",
        "--entrypoint",
        "/usr/bin/env",
        &runtime.image,
        "-i",
        "PATH=/usr/local/bin:/usr/bin:/bin",
        "HOME=/tmp/preview",
        "TMPDIR=/tmp",
        "LANG=C.UTF-8",
        "LC_ALL=C.UTF-8",
        "PYTHONDONTWRITEBYTECODE=1",
        "SAL_USE_VCLPLUGIN=svp",
        "/usr/bin/timeout",
        "--signal=KILL",
        "40",
        &runtime.settings.python,
        "-I",
        "-c",
        BOOTSTRAP,
        "/tmp/preview",
        "/output/derived",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

async fn command(
    settings: &Settings,
    args: &[OsString],
    input: Option<Vec<u8>>,
    timeout: Duration,
    pipe_bytes: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>), ApiError> {
    // Do not inherit Docker contexts, registry credentials, SSH forwarding or
    // daemon-selection variables. This backend can reach only the local socket.
    let config = tempfile::Builder::new()
        .prefix("by-preview-docker-")
        .tempdir()
        .map_err(io_error)?;
    let mut explicit = vec![
        "--host".into(),
        "unix:///var/run/docker.sock".into(),
        "--config".into(),
        config.path().as_os_str().to_os_string(),
    ];
    explicit.extend_from_slice(args);
    run_input(&settings.executable, &explicit, timeout, input, pipe_bytes).await
}

/// Stream one authorized source into a bounded, networkless container.
pub(super) async fn convert(
    runtime: &Runtime,
    source: &Path,
    _helper: &Path,
    output: &Path,
    options: &PreviewOptions,
) -> Result<Value, ApiError> {
    let payload = payload(source, options)?;
    let (status, bytes) = command(
        &runtime.settings,
        &create_arguments(runtime),
        None,
        COMMAND_TIMEOUT,
        MAX_RESPONSE_BYTES,
    )
    .await?;
    let id = std::str::from_utf8(&bytes)
        .map(str::trim)
        .unwrap_or_default();
    if !status.success() || id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unavailable("The local daemon could not create the hardened preview container. Check the operator Docker executable, daemon access and pinned artifact image."));
    }
    // Only the exact ID returned by our successful create can be removed.
    let result = command(
        &runtime.settings,
        &[
            "start".into(),
            "--attach".into(),
            "--interactive".into(),
            id.into(),
        ],
        Some(payload),
        CONVERSION_TIMEOUT,
        PACKET_BYTES,
    )
    .await;
    let cleanup = command(
        &runtime.settings,
        &["rm".into(), "--force".into(), id.into()],
        None,
        COMMAND_TIMEOUT,
        MAX_RESPONSE_BYTES,
    )
    .await;
    // Auto-removal may already have completed, so a nonzero rm exit is normal.
    // A failed cleanup process is still reported, and the in-container watchdog
    // independently bounds runtime even if this server disappears.
    cleanup?;
    let (status, bytes) = result?;
    let mut value = unpack(&bytes, output)?;
    let result = value.get("status").and_then(Value::as_str);
    if !matches!(result, Some("passed" | "failed" | "unavailable"))
        || (!status.success() && result == Some("passed"))
    {
        return Err(unavailable(
            "The container renderer did not complete a bounded preview.",
        ));
    }
    if result == Some("passed") {
        remove_internal_paths(&mut value);
    }
    Ok(value)
}

fn payload(source: &Path, options: &PreviewOptions) -> Result<Vec<u8>, ApiError> {
    let mut helpers = std::collections::BTreeMap::new();
    for (name, bytes) in helper_files()? {
        helpers.insert(name, STANDARD.encode(bytes));
    }
    helpers.insert(
        "launcher.py",
        STANDARD.encode(include_bytes!("launcher.py")),
    );
    serde_json::to_vec(&json!({
        "source": STANDARD.encode(read_regular(source, MAX_INPUT)?),
        "suffix": super::cache::suffix(&source.to_string_lossy())?,
        "helpers": helpers, "options": options,
    }))
    .map_err(json_error)
}

fn unpack(bytes: &[u8], output: &Path) -> Result<Value, ApiError> {
    let packet: Value = serde_json::from_slice(bytes).map_err(|_| unavailable("The isolated container returned no valid preview packet. Verify its Python and artifact runtime dependencies."))?;
    let value = packet
        .get("preview")
        .ok_or_else(|| ApiError::internal("preview packet has no metadata"))?;
    if serde_json::to_vec(value).map_err(json_error)?.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(ApiError::internal(
            "container preview metadata exceeds its bound",
        ));
    }
    if value.get("status").and_then(Value::as_str) == Some("passed")
        && value.get("kind").and_then(Value::as_str) == Some("pages")
    {
        let name = value
            .get("image")
            .and_then(Value::as_str)
            .filter(|name| safe_asset(name))
            .ok_or_else(|| ApiError::internal("invalid container image basename"))?;
        let encoded = packet
            .get("image")
            .and_then(Value::as_str)
            .filter(|image| image.len() as u64 <= 4 * MAX_IMAGE.div_ceil(3))
            .ok_or_else(|| ApiError::internal("container image exceeds encoded bound"))?;
        let image = STANDARD
            .decode(encoded)
            .map_err(|_| ApiError::internal("invalid container image encoding"))?;
        if image.len() as u64 > MAX_IMAGE || !image.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(ApiError::internal("invalid bounded container PNG"));
        }
        let derived = output.join("derived");
        std::fs::create_dir(&derived).map_err(io_error)?;
        std::fs::write(derived.join(name), image).map_err(io_error)?;
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests;
