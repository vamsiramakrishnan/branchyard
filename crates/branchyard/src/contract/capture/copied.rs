//! Declared outboxes over the existing guest exec transport, independent of Git.

use super::*;
use branchyard_sandbox::{ExecSpec, Process};
use branchyard_substrate::transfer::Guest;

const READER: &str = include_str!("read_file.py");

/// A private snapshot owned by this producer's fenced invocation.
pub(crate) struct CopiedCapture {
    capture: ResultCapture,
    snapshot: PathBuf,
}

/// Remove copied contents after collection, failure or an intermediate park.
pub(crate) struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        branchyard_support::cleanup_dir(&self.0);
    }
}

fn snapshot(yard: &Yard, fence: &Fence) -> PathBuf {
    yard.store().dir().join("capture-staging").join(format!(
        "{}-{}-{}",
        fence.incarnation, fence.turn, fence.generation
    ))
}

/// Reclaim only this fenced invocation's snapshot after completion or recovery.
pub(crate) fn staging_cleanup(yard: &Yard, fence: &Fence) -> Cleanup {
    Cleanup(snapshot(yard, fence))
}

impl CopiedCapture {
    /// Select only providers whose worktree travels through the guest transport.
    pub(crate) fn for_record(yard: &Yard, record: &Record, fence: &Fence) -> Option<Self> {
        if !matches!(
            record.provider,
            Some(crate::Provider::Substrate(_) | crate::Provider::Recipe(_))
        ) {
            return None;
        }
        let capture = record.result_contract.as_ref()?.capture.clone()?;
        Some(Self {
            capture,
            snapshot: snapshot(yard, fence),
        })
    }

    /// Check the explicit image prerequisite before starting a model turn.
    pub(crate) fn probe(&self, guest: &dyn Guest, root: &Path) -> Result<(), Error> {
        remote(guest, root, &["--probe".into()], 0, &mut std::io::sink())
            .map(|_| ())
            .map_err(|error| Error::Denied(format!(
                "result artifact capture on a copied provider requires Python3 with POSIX descriptor-relative opens in the image before the turn starts: {error}"
            )))
    }

    /// Copy only declared bounded contents; defer missing intermediate outboxes.
    pub(crate) fn receive(&self, guest: &dyn Guest, root: &Path) -> Result<(), Error> {
        branchyard_support::cleanup_dir(&self.snapshot);
        std::fs::create_dir_all(&self.snapshot)?;
        if let Err(error) = self.copy(guest, root) {
            branchyard_support::cleanup_dir(self.snapshot.join("contents"));
            std::fs::write(self.snapshot.join("error"), error.to_string())?;
        }
        Ok(())
    }

    fn copy(&self, guest: &dyn Guest, root: &Path) -> Result<(), Error> {
        check_config(&self.capture)?;
        let mut bytes = Vec::new();
        remote(
            guest,
            root,
            &[self.capture.manifest.clone(), MANIFEST_MAX.to_string()],
            MANIFEST_MAX,
            &mut bytes,
        )?;
        let manifest = manifest(&bytes, &self.capture)?;
        let contents = self.snapshot.join("contents");
        let directory = Path::new(&self.capture.manifest)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let mut remaining = self.capture.max_bytes;
        for entry in manifest.files {
            let relative = directory.join(&entry.path);
            let local = contents.join(&relative);
            let parent = local
                .parent()
                .ok_or_else(|| Error::Denied("captured file has no parent".into()))?;
            std::fs::create_dir_all(parent)?;
            let mut target = File::create(&local)?;
            let path = relative
                .to_str()
                .ok_or_else(|| Error::Denied("captured path is not UTF-8".into()))?;
            let (digest, size) = remote(
                guest,
                root,
                &[path.into(), remaining.to_string()],
                remaining,
                &mut target,
            )?;
            if digest != entry.sha256 {
                return Err(Error::Denied(
                    "artifact capture SHA-256 mismatch during transfer".into(),
                ));
            }
            target.sync_all()?;
            remaining -= size;
        }
        let path = contents.join(&self.capture.manifest);
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| Error::Denied("capture manifest has no parent".into()))?,
        )?;
        std::fs::write(path, bytes)?;
        std::fs::write(self.snapshot.join("complete"), b"")?;
        Ok(())
    }

    /// A successful private copy is the only source accepted for copied providers.
    pub(crate) fn contents(&self) -> Result<PathBuf, Error> {
        if let Ok(error) = std::fs::read_to_string(self.snapshot.join("error")) {
            return Err(Error::Denied(format!(
                "could not transfer declared artifact outbox: {error}"
            )));
        }
        if !self.snapshot.join("complete").is_file() {
            return Err(Error::Denied(
                "copied provider returned no completed artifact outbox snapshot".into(),
            ));
        }
        Ok(self.snapshot.join("contents"))
    }

    /// Whether this invocation has transfer evidence to check before receipt replay.
    pub(crate) fn has_snapshot(&self) -> bool {
        self.snapshot.exists()
    }

    /// Clean the snapshot regardless of the final collection outcome.
    pub(crate) fn cleanup_on_drop(&self) -> Cleanup {
        Cleanup(self.snapshot.clone())
    }
}

struct Running(Box<dyn Process>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.teardown();
    }
}

/// One ordinary exec, raw stdout, and a host-side byte bound and digest.
fn remote(
    guest: &dyn Guest,
    root: &Path,
    args: &[String],
    maximum: u64,
    target: &mut dyn Write,
) -> Result<(String, u64), Error> {
    let probe = args == ["--probe"];
    let mut arguments = Vec::new();
    if !probe {
        arguments.push(
            root.to_str()
                .ok_or_else(|| Error::Denied("capture workdir is not UTF-8".into()))?
                .to_owned(),
        );
        let path = relative(&args[0])?
            .into_iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        arguments.push(path);
        arguments.extend(args.iter().skip(1).cloned());
    } else {
        arguments.extend_from_slice(args);
    }
    let spec = ExecSpec {
        argv: ["python3", "-I", "-S", "-c", READER]
            .into_iter()
            .map(str::to_owned)
            .chain(arguments)
            .collect(),
        cwd: if probe {
            PathBuf::from("/")
        } else {
            root.to_path_buf()
        },
        env: BTreeMap::new(),
        writable: None,
    };
    let mut process = Running(
        guest
            .exec(&spec)
            .map_err(|error| Error::State(format!("exec artifact reader: {error}")))?,
    );
    drop(process.0.take_stdin());
    let errors = process.0.take_stderr().map(|stderr| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take(8192)
                .read_to_end(&mut bytes)
                .map(|_| String::from_utf8_lossy(&bytes).into_owned())
        })
    });
    let copied = (|| {
        let mut stream = process
            .0
            .take_stdout()
            .ok_or_else(|| Error::State("artifact reader stdout is not piped".into()))?;
        let mut digest = Sha256::new();
        let mut total = 0;
        let mut buffer = [0u8; 65536];
        loop {
            let size = stream.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            if size as u64 > maximum.saturating_sub(total) {
                return Err(Error::Denied(
                    "artifact transfer exceeds its byte limit".into(),
                ));
            }
            target.write_all(&buffer[..size])?;
            digest.update(&buffer[..size]);
            total += size as u64;
        }
        Ok((hex::encode(digest.finalize()), total))
    })();
    if copied.is_err() {
        branchyard_support::best_effort("stop an invalid artifact transfer", process.0.kill());
    }
    let status = process.0.wait()?;
    let stderr = match errors {
        Some(errors) => errors
            .join()
            .map_err(|_| Error::State("artifact reader stderr thread failed".into()))??,
        None => String::new(),
    };
    if !status.success() {
        return Err(Error::Denied(format!(
            "artifact reader failed with {status}: {}",
            stderr.trim()
        )));
    }
    copied
}
