//! Live output while branches run, and the permission policy for them. The
//! engine calls both from any branch's thread, so everything goes through
//! one mutex: output lines never tear, and a permission prompt holds the
//! terminal until it is answered. Decisions are printed from the engine's
//! record of them, whoever made them.

use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use branchyard::{Activity, BranchEvent, Event, PermissionDecision, PermissionRequest, Policy};

use crate::args::Permissions;
use crate::notify::Notifier;
use crate::render::{compact_input, Renderer};

/// How permission requests will be answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    AllowAll,
    Ask,
    /// A named preset's rules.
    Preset(branchyard::PolicyPreset),
    /// Deny everything; `notice` says why, because the user chose nothing.
    DenyAll {
        notice: bool,
    },
}

/// `--yes` and `--ask` win; otherwise ask only when there is a terminal to
/// ask on and to read the answer from.
pub fn choose(permissions: Permissions, stdin_tty: bool, stderr_tty: bool) -> Choice {
    match permissions {
        Permissions::Yes => Choice::AllowAll,
        Permissions::Ask => Choice::Ask,
        Permissions::Preset(preset) => Choice::Preset(preset),
        Permissions::Unset if stdin_tty && stderr_tty => Choice::Ask,
        Permissions::Unset => Choice::DenyAll { notice: true },
    }
}

pub const DENY_NOTICE: &str = "no terminal to ask on, so tool permission requests will be denied; \
     pass --yes to allow them all or --ask to prompt";

/// Reads one answer to a question. Tests substitute their own.
pub type Prompter = Box<dyn Fn(&str) -> io::Result<String> + Send + Sync>;

pub struct Console {
    state: Mutex<State>,
    prompter: Prompter,
    /// Says when a branch needs the person or ends; see [`crate::notify`].
    notifier: Option<Notifier>,
}

struct State {
    renderer: Renderer,
    out: Box<dyn Write + Send>,
}

impl State {
    /// Output is best effort: a closed stdout must not fail the branch.
    fn write(&mut self, text: &str) {
        if !text.is_empty() {
            let _ = self.out.write_all(text.as_bytes());
            let _ = self.out.flush();
        }
    }
}

impl Console {
    pub fn new(renderer: Renderer, out: Box<dyn Write + Send>, prompter: Prompter) -> Self {
        Console {
            state: Mutex::new(State { renderer, out }),
            prompter,
            notifier: None,
        }
    }

    pub fn with_notifier(mut self, notifier: Option<Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn event(&self, event: &BranchEvent) {
        {
            let mut state = self.lock();
            let text = state.renderer.activity(&event.branch, &event.activity);
            state.write(&text);
        }
        // After the line it is about, so a permission prompt's bell rings
        // with the prompt on screen.
        if let Some(notifier) = &self.notifier {
            notifier.observe(&event.branch, &event.activity);
        }
    }

    /// See [`Renderer::reserve`].
    pub fn reserve(&self, branches: &[String]) {
        self.lock().renderer.reserve(branches);
    }

    /// Prompt for one request. Holding the lock serializes prompts across
    /// branches and keeps other branches' output from landing in the prompt.
    pub fn ask(&self, branch: &str, request: &PermissionRequest) -> PermissionDecision {
        // Ring before the prompt waits; the recorded request, if it comes
        // later, is the same notice and is not said again.
        if let Some(notifier) = &self.notifier {
            notifier.observe(
                branch,
                &Activity::Harness(Event::PermissionRequested {
                    turn: None,
                    request: request.clone(),
                }),
            );
        }
        let mut state = self.lock();
        let pending = state.renderer.finish();
        state.write(&pending);
        let input = compact_input(&request.input);
        let what = match input.is_empty() {
            true => request.tool.clone(),
            false => format!("{}: {input}", request.tool),
        };
        let question = format!("{branch} wants {what}  [y/N] ");
        match (self.prompter)(&question) {
            Ok(answer) if is_yes(&answer) => PermissionDecision::Allow,
            Ok(_) => PermissionDecision::Deny {
                message: "Denied at the terminal.".into(),
            },
            Err(error) => PermissionDecision::Deny {
                message: format!("Denied: could not ask at the terminal ({error})."),
            },
        }
    }

    pub fn finish(&self) {
        let mut state = self.lock();
        let text = state.renderer.finish();
        state.write(&text);
    }
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The policy for `choice`. The engine records each decision, and the
/// console prints it from that record.
pub fn policy(choice: Choice, console: Arc<Console>) -> Policy {
    match choice {
        Choice::Ask => Policy::ask(move |branch, request| console.ask(branch, request)),
        Choice::AllowAll => Policy::allow_all(),
        Choice::Preset(preset) => preset.policy(),
        Choice::DenyAll { .. } => Policy::deny_all(),
    }
}

/// Ask on the controlling terminal, falling back to stderr and stdin.
pub fn terminal_prompt(question: &str) -> io::Result<String> {
    let mut answer = String::new();
    match OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(mut tty) => {
            tty.write_all(question.as_bytes())?;
            tty.flush()?;
            BufReader::new(tty).read_line(&mut answer)?;
        }
        Err(_) => {
            let mut stderr = io::stderr();
            stderr.write_all(question.as_bytes())?;
            stderr.flush()?;
            io::stdin().lock().read_line(&mut answer)?;
        }
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Style;
    use branchyard::{Activity, DecisionSource, PermissionKey};
    use serde_json::json;

    #[test]
    fn flags_win_over_terminal_state() {
        for (stdin, stderr) in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(choose(Permissions::Yes, stdin, stderr), Choice::AllowAll);
            assert_eq!(choose(Permissions::Ask, stdin, stderr), Choice::Ask);
        }
    }

    #[test]
    fn default_asks_only_with_a_full_terminal() {
        assert_eq!(choose(Permissions::Unset, true, true), Choice::Ask);
        let deny = Choice::DenyAll { notice: true };
        assert_eq!(choose(Permissions::Unset, false, true), deny);
        assert_eq!(choose(Permissions::Unset, true, false), deny);
        assert_eq!(choose(Permissions::Unset, false, false), deny);
    }

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn console(answer: &'static str, questions: Arc<Mutex<Vec<String>>>) -> (Arc<Console>, Buffer) {
        let out = Buffer::default();
        let prompter: Prompter = Box::new(move |question| {
            questions.lock().unwrap().push(question.to_owned());
            Ok(answer.to_owned())
        });
        let console = Console::new(
            Renderer::new(Style::PLAIN, false),
            Box::new(out.clone()),
            prompter,
        );
        (Arc::new(console), out)
    }

    fn bash() -> PermissionRequest {
        PermissionRequest {
            key: PermissionKey("k".into()),
            tool: "Bash".into(),
            input: json!({ "command": "cargo test" }),
        }
    }

    fn decision(allowed: bool, source: DecisionSource) -> BranchEvent {
        BranchEvent {
            branch: "b".into(),
            activity: Activity::Decision {
                tool: "Bash".into(),
                allowed,
                message: (!allowed).then(|| "Denied by Branchyard policy.".into()),
                source,
            },
        }
    }

    #[test]
    fn allow_all_and_deny_all_decide_without_asking() {
        let questions = Arc::new(Mutex::new(Vec::new()));
        let (console, out) = console("y", questions.clone());
        let allow = policy(Choice::AllowAll, console.clone());
        assert_eq!(allow.decide("b", &bash()), PermissionDecision::Allow);
        let deny = policy(Choice::DenyAll { notice: true }, console.clone());
        assert!(matches!(
            deny.decide("b", &bash()),
            PermissionDecision::Deny { .. }
        ));
        assert!(questions.lock().unwrap().is_empty());
        assert_eq!(out.text(), "", "decisions print from the engine's record");
        console.event(&decision(true, DecisionSource::Default));
        console.event(&decision(false, DecisionSource::Default));
        assert_eq!(
            out.text(),
            "allowed Bash\ndenied Bash: Denied by Branchyard policy.\n"
        );
    }

    #[test]
    fn ask_prompts_with_branch_tool_and_input() {
        let questions = Arc::new(Mutex::new(Vec::new()));
        let (console, out) = console("Yes\n", questions.clone());
        console.event(&BranchEvent {
            branch: "b".into(),
            activity: Activity::Harness(branchyard::Event::MessageDelta {
                turn: 1,
                text: "Running the tests".into(),
            }),
        });
        let ask = policy(Choice::Ask, console.clone());
        assert_eq!(ask.decide("fix-flaky", &bash()), PermissionDecision::Allow);
        assert_eq!(
            *questions.lock().unwrap(),
            ["fix-flaky wants Bash: cargo test  [y/N] "]
        );
        console.event(&decision(true, DecisionSource::Asked));
        assert_eq!(out.text(), "Running the tests\nallowed Bash\n");
    }

    #[test]
    fn anything_but_yes_denies() {
        for answer in ["", "\n", "n", "nope", "yess"] {
            let (console, _) = console(answer, Arc::default());
            assert!(matches!(
                console.ask("b", &bash()),
                PermissionDecision::Deny { .. }
            ));
        }
        assert!(is_yes(" Y\n"));
    }
}
