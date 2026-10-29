//! The document converter's namespace, resource and process boundary.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt as _};

use super::{ApiError, PreviewOptions};

const TIMEOUT: Duration = Duration::from_secs(45);
const PIPE_BYTES: u64 = 2 * 1024 * 1024;

/// Operator-selected trusted executables, read once when the service is opened.
#[derive(Clone)]
pub(super) struct Settings {
    pub(super) bwrap: PathBuf,
    pub(super) python: PathBuf,
}

impl Settings {
    /// Read only the explicit renderer executable settings.
    pub(super) fn from_env() -> Self {
        Self {
            bwrap: std::env::var_os("BRANCHYARD_PREVIEW_BWRAP")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("bwrap")),
            python: std::env::var_os("BRANCHYARD_PREVIEW_PYTHON")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("python3")),
        }
    }

    /// Query the trusted interpreter without loading a document.
    pub(super) async fn runtime(&self) -> Result<Runtime, ApiError> {
        if !cfg!(target_os = "linux") {
            return Err(unavailable("Document conversion requires a Linux renderer. Configure a Linux Branchyard server with Bubblewrap and the artifact runtime."));
        }
        // This only queries the operator-selected interpreter. It does not load
        // the converter, document, user site packages or user configuration.
        let code = "import json,sys;print(json.dumps(dict(executable=sys.executable,prefix=sys.prefix,base_prefix=sys.base_prefix,version=sys.version)))";
        let args = ["-I", "-c", code].map(std::ffi::OsString::from);
        let (status, bytes) = run(&self.python, &args, Duration::from_secs(5)).await?;
        if !status.success() {
            return Err(unavailable("The selected Python interpreter could not report its runtime. Set BRANCHYARD_PREVIEW_PYTHON to a working artifact-runtime interpreter."));
        }
        let runtime: Runtime = serde_json::from_slice(&bytes).map_err(|_| {
            unavailable("The selected Python interpreter returned invalid runtime information.")
        })?;
        runtime.validate()?;
        Ok(runtime)
    }
}

/// The interpreter and its explicitly bound, trusted toolchain directories.
#[derive(Clone, Debug, Deserialize)]
pub(super) struct Runtime {
    pub(super) executable: PathBuf,
    pub(super) prefix: PathBuf,
    pub(super) base_prefix: PathBuf,
    pub(super) version: String,
}

impl Runtime {
    fn validate(&self) -> Result<(), ApiError> {
        for prefix in [&self.prefix, &self.base_prefix] {
            if !prefix.is_absolute()
                || matches!(
                    prefix.to_str(),
                    Some("/" | "/home" | "/root" | "/tmp" | "/etc")
                )
                || std::env::var_os("HOME").is_some_and(|home| prefix == Path::new(&home))
                || prefix
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(unavailable("Python's prefix is too broad to bind safely. Select a dedicated Python installation or virtual environment."));
            }
        }
        if !self.executable.is_absolute()
            || (!self.executable.starts_with(&self.prefix)
                && !self.executable.starts_with(&self.base_prefix))
        {
            return Err(unavailable("The selected interpreter must be inside its Python installation or virtual environment."));
        }
        Ok(())
    }

    /// Identify the exact selected interpreter and its installations.
    pub(super) fn identity(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.executable.display(),
            self.prefix.display(),
            self.base_prefix.display(),
            self.version
        )
    }
}

/// Run one trusted helper with only the source, output and Python runtime bound.
pub(super) async fn convert(
    settings: &Settings,
    runtime: &Runtime,
    source: &Path,
    helper: &Path,
    output: &Path,
    options: &PreviewOptions,
) -> Result<Value, ApiError> {
    let args = arguments(runtime, source, helper, output, options)?;
    let (status, bytes) = run(&settings.bwrap, &args, TIMEOUT).await?;
    let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable(
        "The isolated renderer could not start or returned no preview. Install Bubblewrap, enable unprivileged user namespaces, and set BRANCHYARD_PREVIEW_PYTHON to the artifact runtime. No unsandboxed conversion was attempted."
    ))?;
    let result = value.get("status").and_then(Value::as_str);
    if !matches!(result, Some("passed" | "unavailable" | "failed"))
        || (!status.success() && result == Some("passed"))
    {
        return Err(ApiError::internal(
            "renderer exit status and preview response disagree",
        ));
    }
    if result == Some("passed") {
        remove_internal_paths(&mut value);
    }
    Ok(value)
}

fn remove_internal_paths(value: &mut Value) {
    const FIELDS: &[&str] = &[
        "status",
        "kind",
        "format",
        "columns",
        "rows",
        "sheets",
        "sheet",
        "offset",
        "next_offset",
        "warnings",
        "page",
        "pages",
        "image",
        "width",
        "height",
    ];
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| FIELDS.contains(&key.as_str()));
    }
}

fn arguments(
    runtime: &Runtime,
    source: &Path,
    helper: &Path,
    output: &Path,
    options: &PreviewOptions,
) -> Result<Vec<std::ffi::OsString>, ApiError> {
    runtime.validate()?;
    let mut args = Vec::new();
    let mut push = |values: &[&std::ffi::OsStr]| {
        args.extend(values.iter().map(|value| value.to_os_string()));
    };
    let os = std::ffi::OsStr::new;
    push(&[
        os("--die-with-parent"),
        os("--new-session"),
        os("--unshare-all"),
        os("--clearenv"),
        os("--proc"),
        os("/proc"),
        os("--dev"),
        os("/dev"),
        os("--tmpfs"),
        os("/tmp"),
        os("--dir"),
        os("/home"),
        os("--dir"),
        os("/home/preview"),
        os("--dir"),
        os("/input"),
    ]);
    let base = ["/usr", "/bin", "/lib", "/lib64"];
    for path in base {
        if Path::new(path).exists() {
            push(&[os("--ro-bind"), os(path), os(path)]);
        }
    }
    for (index, prefix) in [&runtime.prefix, &runtime.base_prefix]
        .into_iter()
        .enumerate()
    {
        if !base.iter().any(|base| prefix.starts_with(base))
            && (index == 0 || runtime.prefix != runtime.base_prefix)
        {
            push(&[os("--ro-bind"), prefix.as_os_str(), prefix.as_os_str()]);
        }
    }
    for path in ["/etc/fonts", "/etc/ld.so.cache"] {
        if Path::new(path).exists() {
            push(&[os("--ro-bind"), os(path), os(path)]);
        }
    }
    let source_name = source
        .file_name()
        .ok_or_else(|| ApiError::internal("preview source has no basename"))?;
    let input = Path::new("/input").join(source_name);
    push(&[
        os("--ro-bind"),
        source.as_os_str(),
        input.as_os_str(),
        os("--ro-bind"),
        helper.as_os_str(),
        os("/helper"),
        os("--bind"),
        output.as_os_str(),
        os("/output"),
        os("--chdir"),
        os("/tmp"),
    ]);
    for (name, value) in [
        ("HOME", "/home/preview"),
        ("TMPDIR", "/tmp"),
        ("PATH", "/usr/bin:/bin"),
        ("LANG", "C.UTF-8"),
        ("LC_ALL", "C.UTF-8"),
        ("PYTHONDONTWRITEBYTECODE", "1"),
        ("SAL_USE_VCLPLUGIN", "gen"),
    ] {
        push(&[os("--setenv"), os(name), os(value)]);
    }
    push(&[
        os("--"),
        runtime.executable.as_os_str(),
        os("-I"),
        os("/helper/launcher.py"),
        os("--file"),
        input.as_os_str(),
        os("--out"),
        os("/output/derived"),
        os("--view"),
        os(options.view.as_str()),
        os("--page"),
        os(&options.page.to_string()),
        os("--offset"),
        os(&options.offset.to_string()),
        os("--limit"),
        os(&options.limit.to_string()),
    ]);
    if let Some(sheet) = &options.sheet {
        push(&[os("--sheet"), os(sheet)]);
    }
    Ok(args)
}

async fn read_pipe(reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(PIPE_BYTES + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > PIPE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "renderer output limit exceeded",
        ));
    }
    Ok(bytes)
}

struct ProcessGroup(u32);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        branchyard_support::process::kill_group(self.0);
    }
}

async fn run(
    program: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>), ApiError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let mut child = command.spawn().map_err(|_| unavailable(
        "A preview executable is unavailable. Install Bubblewrap and Python, then configure BRANCHYARD_PREVIEW_BWRAP and BRANCHYARD_PREVIEW_PYTHON."
    ))?;
    let pid = child
        .id()
        .ok_or_else(|| ApiError::internal("renderer has no process identifier"))?;
    let group = ProcessGroup(pid);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::internal("renderer stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ApiError::internal("renderer stderr was not captured"))?;
    let completed = tokio::time::timeout(timeout, async {
        tokio::try_join!(child.wait(), read_pipe(stdout), read_pipe(stderr))
    })
    .await;
    match completed {
        Ok(Ok((status, stdout, _stderr))) => Ok((status, stdout)),
        other => {
            branchyard_support::process::kill_group(group.0);
            branchyard_support::best_effort("reap document renderer", child.wait().await);
            match other {
                Err(_) => Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "preview_timeout",
                    "document conversion exceeded its time limit",
                )),
                Ok(Err(_)) => Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "preview_output_limit",
                    "document conversion exceeded its output bound or lost a pipe",
                )),
                Ok(Ok(_)) => Err(ApiError::internal("unexpected renderer completion")),
            }
        }
    }
}

fn unavailable(message: &str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "preview_unavailable", message)
        .detail(serde_json::json!({"status":"unavailable", "next":"Install the documented artifact runtime and Bubblewrap on the rendering server; no fallback runs outside the sandbox."}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_arguments_bind_only_explicit_runtime_and_document_paths() {
        let runtime = Runtime {
            executable: "/usr/bin/python3".into(),
            prefix: "/usr".into(),
            base_prefix: "/usr".into(),
            version: "fixture".into(),
        };
        let args = arguments(
            &runtime,
            Path::new("/owned/input/source.csv"),
            Path::new("/owned/helper"),
            Path::new("/owned/output"),
            &PreviewOptions::default(),
        )
        .unwrap();
        let values: Vec<_> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(values.contains(&"--unshare-all".to_owned()));
        assert!(values.contains(&"--clearenv".to_owned()));
        assert!(values
            .windows(3)
            .any(|a| a == ["--bind", "/owned/output", "/output"]));
        assert!(!values
            .iter()
            .any(|a| a == "/" || a == "/home/user" || a == "/etc" || a == "/sys"));
    }

    #[test]
    fn runtime_rejects_broad_or_unbound_prefixes() {
        let mut runtime = Runtime {
            executable: "/usr/bin/python3".into(),
            prefix: "/".into(),
            base_prefix: "/usr".into(),
            version: "fixture".into(),
        };
        assert!(runtime.validate().is_err());
        runtime.prefix = "/opt/venv".into();
        runtime.executable = "/other/python".into();
        assert!(runtime.validate().is_err());
    }

    #[test]
    fn temporary_virtual_environments_are_bound_after_private_tmp() {
        let runtime = Runtime {
            executable: "/tmp/runtime/bin/python".into(),
            prefix: "/tmp/runtime".into(),
            base_prefix: "/usr".into(),
            version: "fixture".into(),
        };
        let args = arguments(
            &runtime,
            Path::new("/owned/source.csv"),
            Path::new("/owned/helper"),
            Path::new("/owned/output"),
            &PreviewOptions::default(),
        )
        .unwrap();
        let values: Vec<_> = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        let private_tmp = values
            .windows(2)
            .position(|a| a == ["--tmpfs", "/tmp"])
            .unwrap();
        let venv = values
            .windows(3)
            .position(|a| a == ["--ro-bind", "/tmp/runtime", "/tmp/runtime"])
            .unwrap();
        assert!(private_tmp < venv);
    }

    #[tokio::test]
    async fn pipes_stop_at_the_declared_bound() {
        let bytes = std::io::Cursor::new(vec![0_u8; PIPE_BYTES as usize + 1]);
        assert!(read_pipe(bytes).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deadline_kills_and_reaps_the_owned_process_group() {
        let error = run(
            Path::new("/bin/sh"),
            &["-c".into(), "while :; do :; done".into()],
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert_eq!(error.body.code, "preview_timeout");
        let (status, output) = run(
            Path::new("/bin/sh"),
            &["-c".into(), "printf ready".into()],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(status.success());
        assert_eq!(output, b"ready");
    }
}
