//! [`RecipeProvider`]: a [`SandboxProvider`] whose sandboxes are machines a
//! recipe's scripts create, and whose execs run there over the result's
//! transport.
//!
//! - `ensure` runs `create` (once per name) and keeps its result; a spec
//!   with mounts, an image or resource limits is refused, since the
//!   machine is whatever the script makes and cannot see this host's
//!   directories.
//! - `exec` runs the argument vector, without a shell of its own, in the
//!   result's `projectRoot` with the result's `env` and the spec's, through
//!   a small `sh` wrapper on the machine that first makes it a process
//!   group leader (with `setsid` when the transport did not), records the
//!   group in `~/.branchyard/recipe-exec/<sandbox>/`, and reports a
//!   program it cannot find before anything starts, so that `exec` fails
//!   with an I/O error as the contract asks.
//! - `kill`, `teardown` and `stop` signal that recorded group on the
//!   machine, since closing an ssh connection leaves remote processes
//!   running; `teardown` names what it found.
//! - `pause` runs `suspend` and `resume` runs `resume`, declared only when
//!   the recipe has both; `destroy` runs `destroy` and forgets the machine.
//!
//! With a state directory, each machine's result is kept in a file, so a
//! later process can exec in, pause, resume or destroy it by name.

use branchyard_support::LockExt as _;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ExitStatus as StdStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use branchyard_sandbox::{
    Capabilities, ExecSpec, ExitStatus, Operation, Process, ProviderError, SandboxInfo,
    SandboxProvider, SandboxSpec, SandboxState,
};
use serde::{Deserialize, Serialize};

use crate::transport::{quote, Transport};
use crate::{check_variable, parse_result, run, Mode, Payload, Recipe, RecipeResult};

const STARTED: &str = "by-recipe-started";
const MISSING: &str = "by-recipe-missing";

/// Makes the inner script a process group leader, then runs it.
const OUTER: &str = r#"p=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')
if [ "$p" != "$$" ] && command -v setsid >/dev/null 2>&1; then exec setsid sh -c "$1"; fi
exec sh -c "$1""#;

/// Kills every exec recorded for one sandbox: `$1` is its directory.
const STOP: &str = r#"for f in "$HOME/.branchyard/recipe-exec/$1"/*; do
  [ -f "$f" ] || continue
  kill -s KILL -- $(cat "$f") 2>/dev/null
  rm -f "$f"
done
:"#;

/// Names, then kills, what one exec left: `$1` its record.
const TEARDOWN: &str = r#"f="$HOME/.branchyard/recipe-exec/$1"
t=$(cat "$f" 2>/dev/null) || exit 0
case $t in
  -*) ps -eo pgid=,comm= 2>/dev/null | awk -v g="${t#-}" '$1 == g { print $2 }' ;;
  *) ps -o comm= -p "$t" 2>/dev/null ;;
esac
kill -s KILL -- $t 2>/dev/null
rm -f "$f"
:"#;

/// Kills one exec's group: `$1` its record.
const KILL: &str = r#"t=$(cat "$HOME/.branchyard/recipe-exec/$1" 2>/dev/null) && kill -s KILL -- $t 2>/dev/null
:"#;

/// Forgets one exec's record: `$1` it.
const FORGET: &str = r#"rm -f "$HOME/.branchyard/recipe-exec/$1"
:"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Held {
    Running,
    Paused,
    Stopped,
}

impl Held {
    fn state(self) -> SandboxState {
        match self {
            Held::Running => SandboxState::Running,
            Held::Paused => SandboxState::Paused,
            Held::Stopped => SandboxState::Stopped,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Machine {
    result: RecipeResult,
    held: Held,
}

/// See the module documentation.
pub struct RecipeProvider {
    recipe: Recipe,
    ssh: String,
    state_dir: Option<PathBuf>,
    machines: Mutex<BTreeMap<String, Machine>>,
    execs: AtomicU64,
}

impl RecipeProvider {
    /// A provider over `recipe`, with `ssh` as the ssh program.
    pub fn new(recipe: Recipe, ssh: impl Into<String>) -> RecipeProvider {
        RecipeProvider {
            recipe,
            ssh: ssh.into(),
            state_dir: None,
            machines: Mutex::new(BTreeMap::new()),
            execs: AtomicU64::new(0),
        }
    }

    /// Keep each machine's result in `dir`, for later processes.
    pub fn with_state_dir(mut self, dir: impl Into<PathBuf>) -> RecipeProvider {
        self.state_dir = Some(dir.into());
        self
    }

    pub fn recipe(&self) -> &Recipe {
        &self.recipe
    }

    /// What `create` (or the last `resume`) printed for `name`.
    pub fn result(&self, name: &str) -> Option<RecipeResult> {
        self.machine(name).map(|m| m.result)
    }

    /// How execs reach `name`.
    pub fn transport(&self, name: &str) -> Option<Transport> {
        self.machine(name)
            .map(|m| Transport::new(&m.result.connection, &self.ssh))
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.json", key(name))))
    }

    fn machine(&self, name: &str) -> Option<Machine> {
        let mut machines = self.machines.lock_recovering("machines");
        if let Some(machine) = machines.get(name) {
            return Some(machine.clone());
        }
        let text = std::fs::read_to_string(self.file(name)?).ok()?;
        let machine: Machine = serde_json::from_str(&text).ok()?;
        machines.insert(name.to_owned(), machine.clone());
        Some(machine)
    }

    fn record(&self, name: &str, machine: Option<Machine>) -> Result<(), ProviderError> {
        let mut machines = self.machines.lock_recovering("machines");
        match &machine {
            Some(machine) => {
                machines.insert(name.to_owned(), machine.clone());
            }
            None => {
                machines.remove(name);
            }
        }
        let Some(file) = self.file(name) else {
            return Ok(());
        };
        match machine {
            Some(machine) => {
                use std::os::unix::fs::OpenOptionsExt;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let text = serde_json::to_string_pretty(&machine).map_err(io::Error::other)?;
                let temporary = file.with_extension(format!("json.{}", std::process::id()));
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&temporary)?
                    .write_all(text.as_bytes())?;
                std::fs::rename(&temporary, &file)?;
            }
            None => {
                branchyard_support::cleanup_file(&file);
            }
        }
        Ok(())
    }

    /// Run a lifecycle script with Orca's payload on stdin.
    fn lifecycle(
        &self,
        name: &str,
        command: &str,
        mode: Mode,
        result: &RecipeResult,
    ) -> Result<String, ProviderError> {
        let payload = Payload {
            schema_version: 1,
            mode,
            recipe: &self.recipe.name,
            instance: name,
            recipe_result: result,
        };
        let mut input = serde_json::to_vec(&payload).map_err(io::Error::other)?;
        input.push(b'\n');
        let ran = run(&self.recipe, command, mode, name, Some(&input))?;
        match ran.success() {
            true => Ok(ran.stdout),
            false => Err(ProviderError::Runtime(ran.failure(&self.recipe.name, mode))),
        }
    }

    /// Run `script` on `name`'s machine and wait for it, with stdin closed.
    fn remote(&self, name: &str, script: &str, args: &[&str]) -> io::Result<String> {
        let Some(transport) = self.transport(name) else {
            return Ok(String::new());
        };
        let out = transport
            .command(script, args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// A sandbox name as a file name.
fn key(name: &str) -> String {
    name.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                true => c,
                false => '_',
            },
        )
        .collect::<String>()
        .trim_start_matches('.')
        .to_owned()
}

impl SandboxProvider for RecipeProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            exec: true,
            pause: self.recipe.can_pause(),
            // The machine is the recipe's; nothing here confines its network
            // (docs/egress.md).
            egress: false,
            ..Capabilities::default()
        }
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        spec.validate()?;
        if !spec.mounts.is_empty() {
            return Err(ProviderError::Invalid(format!(
                "recipe {}'s machine cannot mount this host's directories ({})",
                self.recipe.name,
                spec.mounts[0].host.display()
            )));
        }
        if spec.image.is_some() {
            return Err(ProviderError::Invalid(format!(
                "recipe {}'s machine is what its create script makes; it takes no image",
                self.recipe.name
            )));
        }
        if !spec.resources.is_unlimited() {
            return Err(ProviderError::Invalid(format!(
                "recipe {} sets its machine's resources itself; the spec cannot",
                self.recipe.name
            )));
        }
        if let Some(mut machine) = self.machine(&spec.name) {
            if machine.held == Held::Stopped {
                machine.held = Held::Running;
                self.record(&spec.name, Some(machine.clone()))?;
            }
            return Ok(SandboxInfo {
                name: spec.name.clone(),
                state: machine.held.state(),
            });
        }
        let ran = run(
            &self.recipe,
            &self.recipe.create,
            Mode::Create,
            &spec.name,
            None,
        )?;
        if !ran.success() {
            return Err(ProviderError::Runtime(
                ran.failure(&self.recipe.name, Mode::Create),
            ));
        }
        let result = parse_result(&ran.stdout).map_err(|e| {
            ProviderError::Runtime(format!("recipe {}: create: {e}", self.recipe.name))
        })?;
        self.record(
            &spec.name,
            Some(Machine {
                result,
                held: Held::Running,
            }),
        )?;
        Ok(SandboxInfo {
            name: spec.name.clone(),
            state: SandboxState::Running,
        })
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        Ok(self.machine(name).map(|m| SandboxInfo {
            name: name.to_owned(),
            state: m.held.state(),
        }))
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        let machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        match machine.held {
            Held::Running => {}
            Held::Paused => {
                return Err(ProviderError::Invalid(format!(
                    "{name} is paused; resume it first"
                )))
            }
            Held::Stopped => {
                return Err(ProviderError::Invalid(format!(
                    "{name} is stopped; ensure it again first"
                )))
            }
        }
        let Some(program) = spec.argv.first() else {
            return Err(ProviderError::Invalid(
                "the argument vector is empty".into(),
            ));
        };
        let cwd = match spec.cwd.as_os_str().is_empty() {
            true => machine.result.connection.project_root().to_owned(),
            false => spec.cwd.display().to_string(),
        };
        let mut env: BTreeMap<String, String> = machine.result.env.clone();
        for (name, value) in &spec.env {
            let (name, value) = (text(name)?, text(value)?);
            check_variable(&name).map_err(ProviderError::Invalid)?;
            env.insert(name, value);
        }
        let id = format!(
            "{}/{}-{}",
            key(name),
            std::process::id(),
            self.execs.fetch_add(1, Ordering::Relaxed)
        );
        let mut inner = format!(
            "d=\"$HOME/.branchyard/recipe-exec/{dir}\"; mkdir -p \"$d\" || exit 125\n\
             p=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')\n\
             if [ \"$p\" = \"$$\" ]; then echo \"-$$\" >\"$d/{file}\"; else echo \"$$\" >\"$d/{file}\"; fi\n\
             cd {cwd} 2>/dev/null || {{ echo \"by-recipe: cannot enter \"{cwd} >&2; exit 125; }}\n",
            dir = key(name),
            file = id.rsplit('/').next().unwrap_or_default(),
            cwd = quote(&cwd),
        );
        for (name, value) in &env {
            inner.push_str(&format!("export {}\n", quote(&format!("{name}={value}"))));
        }
        let program = quote(program);
        inner.push_str(&format!(
            "case {program} in */*) [ -f {program} ] && [ -x {program} ] ;; \
             *) command -v {program} >/dev/null 2>&1 ;; esac || \
             {{ echo {MISSING} >&2; exit 127; }}\n\
             echo {STARTED} >&2\nexec"
        ));
        for arg in &spec.argv {
            inner.push(' ');
            inner.push_str(&quote(arg));
        }
        let transport = Transport::new(&machine.result.connection, &self.ssh);
        let mut command = transport.command(OUTER, &[&inner]);
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Until the wrapper says the program starts, stderr is the
        // transport's and the wrapper's; whatever came before the marker is
        // kept for the caller.
        let mut stderr = BufReader::new(child.stderr.take().expect("piped"));
        let mut before = Vec::new();
        let mut line = Vec::new();
        let started = loop {
            line.clear();
            match stderr.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break false,
                Ok(_) => {}
            }
            if line.trim_ascii_end() == STARTED.as_bytes() {
                break true;
            }
            before.extend_from_slice(&line);
        };
        if !started {
            let status = child.wait()?;
            let said = String::from_utf8_lossy(&before);
            let missing = said.lines().any(|l| l.trim() == MISSING);
            let said: Vec<&str> = said.lines().filter(|l| l.trim() != MISSING).collect();
            let why = match missing {
                true => format!("{} not found on {name}'s machine", spec.argv[0]),
                false => format!(
                    "could not start {} on {name}'s machine through {} ({status}): {}",
                    spec.argv[0],
                    transport.describe(),
                    said.join(" / ").trim()
                ),
            };
            let kind = match missing {
                true => io::ErrorKind::NotFound,
                false => io::ErrorKind::Other,
            };
            return Err(ProviderError::Io(io::Error::new(kind, why)));
        }
        let stderr: Box<dyn Read + Send> = Box::new(io::Cursor::new(before).chain(stderr));
        Ok(Box::new(RemoteProcess {
            transport,
            record: id,
            stdin: child
                .stdin
                .take()
                .map(|w| Box::new(w) as Box<dyn Write + Send>),
            stdout: child
                .stdout
                .take()
                .map(|r| Box::new(r) as Box<dyn Read + Send>),
            stderr: Some(stderr),
            child,
            status: None,
            torn_down: false,
        }))
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        let mut machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if machine.held == Held::Running {
            self.remote(name, STOP, &[&key(name)])?;
            machine.held = Held::Stopped;
            self.record(name, Some(machine))?;
        }
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        let Some(machine) = self.machine(name) else {
            return Ok(());
        };
        if machine.held == Held::Running {
            // Best effort: the machine is going away regardless.
            let _ = self.remote(name, STOP, &[&key(name)]);
        }
        if let Some(destroy) = &self.recipe.destroy {
            self.lifecycle(name, destroy, Mode::Destroy, &machine.result)?;
        }
        self.record(name, None)
    }

    fn pause(&self, name: &str) -> Result<(), ProviderError> {
        let (Some(suspend), true) = (&self.recipe.suspend, self.recipe.can_pause()) else {
            return Err(ProviderError::unsupported(Operation::Pause));
        };
        let mut machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if machine.held == Held::Paused {
            return Ok(());
        }
        self.lifecycle(name, suspend, Mode::Suspend, &machine.result)?;
        machine.held = Held::Paused;
        self.record(name, Some(machine))
    }

    fn resume(&self, name: &str) -> Result<SandboxInfo, ProviderError> {
        let (Some(resume), true) = (&self.recipe.resume, self.recipe.can_pause()) else {
            return Err(ProviderError::unsupported(Operation::Resume));
        };
        let mut machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if machine.held != Held::Running {
            let stdout = self.lifecycle(name, resume, Mode::Resume, &machine.result)?;
            machine.result = parse_result(&stdout).map_err(|e| {
                ProviderError::Runtime(format!("recipe {}: resume: {e}", self.recipe.name))
            })?;
            machine.held = Held::Running;
            self.record(name, Some(machine))?;
        }
        Ok(SandboxInfo {
            name: name.to_owned(),
            state: SandboxState::Running,
        })
    }
}

fn text(value: &OsString) -> Result<String, ProviderError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Invalid(format!("{value:?} is not UTF-8")))
}

/// An exec on a recipe's machine.
struct RemoteProcess {
    transport: Transport,
    /// `<sandbox>/<exec>` under `~/.branchyard/recipe-exec` on the machine.
    record: String,
    child: Child,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    status: Option<ExitStatus>,
    torn_down: bool,
}

impl RemoteProcess {
    fn remote(&self, script: &str) -> io::Result<String> {
        let out = self
            .transport
            .command(script, &[&self.record])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn kill_local(&mut self) {
        branchyard_support::kill_group(self.child.id());
        branchyard_support::best_effort("kill child", self.child.kill());
    }
}

fn status(status: StdStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus {
        code: status.code(),
        signal: status.signal(),
    }
}

impl Process for RemoteProcess {
    fn id(&self) -> String {
        format!("{} via {}", self.record, self.transport.describe())
    }

    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?.map(status);
        }
        Ok(self.status)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        // Closing our end of stdin lets a reader on the machine finish.
        let waited = status(self.child.wait()?);
        self.status = Some(waited);
        Ok(waited)
    }

    fn kill(&mut self) -> io::Result<()> {
        self.remote(KILL)?;
        self.kill_local();
        Ok(())
    }

    fn teardown(&mut self) -> Vec<String> {
        self.torn_down = true;
        let names = self.remote(TEARDOWN).unwrap_or_default();
        self.kill_local();
        let _ = self.child.try_wait();
        names
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for RemoteProcess {
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        match self.try_wait() {
            Ok(Some(_)) => {
                let _ = self.remote(FORGET);
            }
            _ => {
                let _ = self.remote(KILL);
                let _ = self.teardown();
                branchyard_support::best_effort("reap child", self.child.wait());
            }
        }
    }
}
