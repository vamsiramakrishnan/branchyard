//! Real PTY interaction with bounded replay and revocable caller authority.

use std::collections::VecDeque;
use std::io::{self, BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use branchyard_sandbox::Process as _;
use branchyard_support::{best_effort, CondvarExt as _, LockExt as _};
use serde::{Deserialize, Serialize};

use super::{WorkspaceProcess, WorkspaceRuntime};
use crate::Error;

const MAX_FRAME: usize = 32 * 1024;
const MAX_INPUT: usize = 16 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;

/// Operator-selected terminal executable and initial geometry.
#[derive(Clone, Debug)]
pub struct TerminalOptions {
    /// Python with the standard Unix `pty` module at the workspace execution site.
    pub python: String,
    /// An operator-approved shell; never a browser-provided executable.
    pub shell: String,
    /// Visible terminal rows, from 1 to 1000.
    pub rows: u16,
    /// Visible terminal columns, from 1 to 1000.
    pub columns: u16,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            python: "python3".into(),
            shell: "/bin/sh".into(),
            rows: 24,
            columns: 80,
        }
    }
}

/// A bounded terminal replay page. Cursors count output bytes, not characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutput {
    /// Cursor immediately after these bytes.
    pub cursor: u64,
    /// Exact terminal bytes, including VT escape sequences.
    pub data: Vec<u8>,
    /// Earlier bytes were evicted before the requested cursor.
    pub truncated: bool,
    /// The bridge has closed, failed, or lost authorization.
    pub closed: bool,
    /// A diagnostic, absent while the stream remains healthy.
    pub error: Option<String>,
}

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    ready: bool,
    closed: bool,
    error: Option<String>,
}

struct Inner {
    process: Mutex<Option<WorkspaceProcess>>,
    input: Mutex<Option<Box<dyn io::Write + Send>>>,
    output: Mutex<Output>,
    changed: Condvar,
    stopping: AtomicBool,
    authorized: Arc<dyn Fn() -> Result<(), Error> + Send + Sync>,
    pty_service: Mutex<Option<crate::services::Registration>>,
}

/// A real PTY whose server authorization callback is rechecked while attached.
///
/// Its owner must require the operator's shell capability, the run scope and
/// workspace editor membership before construction. The callback must re-read
/// revocable authority; it runs before every action and at least every second.
pub struct Terminal {
    inner: Arc<Inner>,
    readers: Vec<JoinHandle<()>>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Frame {
    Ready { pid: u32 },
    Output { data: String },
    Closed,
}

fn geometry(rows: u16, columns: u16) -> Result<(), Error> {
    if rows == 0 || rows > 1000 || columns == 0 || columns > 1000 {
        return Err(Error::Unsupported(
            "terminal dimensions must be between 1 and 1000".into(),
        ));
    }
    Ok(())
}

impl Inner {
    fn end(&self, error: Option<String>) {
        let mut output = self.output.lock_recovering("terminal output");
        output.closed = true;
        if error.is_some() {
            output.error = error;
        }
        self.changed.notify_all();
    }

    fn check(&self) -> Result<(), Error> {
        if let Err(error) = (self.authorized)() {
            self.end(Some("workspace access was revoked".into()));
            self.close();
            return Err(error);
        }
        let lease = self
            .process
            .lock_recovering("terminal process")
            .as_ref()
            .map_or(Ok(()), WorkspaceProcess::check_lease);
        if let Err(error) = lease {
            self.end(Some("workspace execution lease was lost".into()));
            self.close();
            return Err(error);
        }
        Ok(())
    }

    fn command(&self, value: &serde_json::Value) -> Result<(), Error> {
        let mut input = self.input.lock_recovering("terminal input");
        let input = input
            .as_mut()
            .ok_or_else(|| Error::State("terminal is closed".into()))?;
        serde_json::to_writer(&mut *input, value)
            .map_err(|error| Error::State(error.to_string()))?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }

    fn close(&self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        best_effort(
            "ask terminal PTY to close",
            self.command(&serde_json::json!({"close":true})),
        );
        self.input.lock_recovering("terminal input").take();
        let output = self.output.lock_recovering("terminal output");
        let (output, _) = self.changed.wait_timeout_while_recovering(
            output,
            Duration::from_secs(2),
            |output| !output.closed,
            "terminal close",
        );
        drop(output);
        let process = self.process.lock_recovering("terminal process").take();
        if let Some(mut process) = process {
            if !matches!(process.try_wait(), Ok(Some(_))) {
                best_effort("stop terminal process", process.kill());
            }
            process.teardown();
        }
        self.pty_service
            .lock_recovering("terminal PTY service")
            .take();
        self.end(None);
    }
}

impl Terminal {
    /// Start a PTY at the runtime's provider, retaining its exclusive branch lease.
    pub fn start(
        runtime: WorkspaceRuntime,
        options: TerminalOptions,
        authorized: Arc<dyn Fn() -> Result<(), Error> + Send + Sync>,
    ) -> Result<Self, Error> {
        authorized()?;
        geometry(options.rows, options.columns)?;
        let mut process = runtime.exec(vec![
            options.python,
            "-u".into(),
            "-c".into(),
            include_str!("pty_bridge.py").into(),
            options.shell,
            options.rows.to_string(),
            options.columns.to_string(),
        ])?;
        let input = process
            .take_stdin()
            .ok_or_else(|| Error::State("terminal bridge has no stdin".into()))?;
        let stdout = process
            .take_stdout()
            .ok_or_else(|| Error::State("terminal bridge has no stdout".into()))?;
        let stderr = process.take_stderr();
        let inner = Arc::new(Inner {
            process: Mutex::new(Some(process)),
            input: Mutex::new(Some(input)),
            output: Mutex::new(Output::default()),
            changed: Condvar::new(),
            stopping: AtomicBool::new(false),
            authorized,
            pty_service: Mutex::new(None),
        });
        let mut readers = Vec::new();
        let stream = inner.clone();
        readers.push(std::thread::spawn(move || {
            let result = read_output(stdout, &stream);
            stream.end(result.err().map(|error| error.to_string()));
            stream.close();
        }));
        if let Some(stderr) = stderr {
            readers.push(std::thread::spawn(move || {
                use std::io::Read as _;
                best_effort(
                    "drain terminal stderr",
                    io::copy(&mut stderr.take(64 * 1024), &mut io::sink()),
                );
            }));
        }
        let authority = inner.clone();
        readers.push(std::thread::spawn(move || {
            let mut output = authority.output.lock_recovering("terminal output");
            while !output.closed {
                let (next, _) = authority.changed.wait_timeout_recovering(
                    output,
                    Duration::from_secs(1),
                    "terminal authority",
                );
                output = next;
                if output.closed {
                    break;
                }
                drop(output);
                if authority.check().is_err() {
                    break;
                }
                output = authority.output.lock_recovering("terminal output");
            }
        }));
        let terminal = Self { inner, readers };
        let output = terminal.inner.output.lock_recovering("terminal output");
        let (output, _) = terminal.inner.changed.wait_timeout_while_recovering(
            output,
            Duration::from_secs(10),
            |o| !o.ready && !o.closed,
            "terminal ready",
        );
        if !output.ready {
            let error = output
                .error
                .clone()
                .unwrap_or_else(|| "terminal bridge did not become ready".into());
            drop(output);
            return Err(Error::State(error));
        }
        drop(output);
        Ok(terminal)
    }

    /// Send exact bytes to the PTY, bounded to one 16 KiB input frame.
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        self.inner.check()?;
        if bytes.len() > MAX_INPUT {
            return Err(Error::Unsupported("terminal input exceeds 16 KiB".into()));
        }
        self.inner
            .command(&serde_json::json!({"input":STANDARD.encode(bytes)}))
    }

    /// Change the actual PTY geometry, not only the browser's viewport.
    pub fn resize(&self, rows: u16, columns: u16) -> Result<(), Error> {
        self.inner.check()?;
        geometry(rows, columns)?;
        self.inner
            .command(&serde_json::json!({"resize":[rows,columns]}))
    }

    /// Read at most 64 KiB after a byte cursor, with bounded loss disclosure.
    pub fn read_after(&self, cursor: u64, limit: usize) -> Result<TerminalOutput, Error> {
        self.inner.check()?;
        if limit == 0 || limit > 64 * 1024 {
            return Err(Error::Unsupported(
                "terminal read limit must be 1 to 65536 bytes".into(),
            ));
        }
        let output = self.inner.output.lock_recovering("terminal output");
        let end = output.start.saturating_add(output.bytes.len() as u64);
        if cursor > end {
            return Err(Error::Unsupported(
                "terminal cursor is beyond the output".into(),
            ));
        }
        let actual = cursor.max(output.start);
        let data = output
            .bytes
            .iter()
            .skip((actual - output.start) as usize)
            .take(limit)
            .copied()
            .collect::<Vec<_>>();
        Ok(TerminalOutput {
            cursor: actual + data.len() as u64,
            data,
            truncated: cursor < output.start,
            closed: output.closed,
            error: output.error.clone(),
        })
    }

    /// Stop the PTY and its process group. Idempotent and also run on drop.
    pub fn close(&self) {
        self.inner.close();
    }

    /// Whether this terminal has closed, without exposing retained output.
    pub fn is_closed(&self) -> bool {
        self.inner.output.lock_recovering("terminal output").closed
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.close();
        for reader in self.readers.drain(..) {
            best_effort(
                "join terminal reader",
                reader.join().map_err(|_| "terminal reader stopped"),
            );
        }
    }
}

fn read_output(stdout: Box<dyn io::Read + Send>, inner: &Inner) -> Result<(), Error> {
    let mut reader = io::BufReader::new(stdout);
    loop {
        use std::io::Read as _;
        let mut frame = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_FRAME + 1) as u64)
            .read_until(b'\n', &mut frame)?;
        if read == 0 {
            return Ok(());
        }
        if frame.len() > MAX_FRAME || frame.last() != Some(&b'\n') {
            return Err(Error::Unsupported(
                "terminal output frame exceeds its byte limit".into(),
            ));
        }
        let event: Frame = serde_json::from_slice(&frame)
            .map_err(|error| Error::State(format!("invalid terminal bridge frame: {error}")))?;
        let mut output = inner.output.lock_recovering("terminal output");
        match event {
            Frame::Ready { pid } => {
                if let Some(process) = inner.process.lock_recovering("terminal process").as_ref() {
                    *inner.pty_service.lock_recovering("terminal PTY service") =
                        process.register_pty(pid)?;
                }
                output.ready = true;
            }
            Frame::Closed => output.closed = true,
            Frame::Output { data } => {
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| Error::State("invalid terminal output encoding".into()))?;
                output.bytes.extend(bytes);
                while output.bytes.len() > MAX_OUTPUT {
                    output.bytes.pop_front();
                    output.start += 1;
                }
            }
        }
        inner.changed.notify_all();
    }
}
