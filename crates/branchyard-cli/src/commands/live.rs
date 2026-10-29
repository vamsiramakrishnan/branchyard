//! [`Live`]: what a running turn prints as it goes, and the outcome of a branch's turn.

use super::*;

/// Console and policy for commands that run a harness.
pub(crate) struct Live {
    pub(crate) console: Arc<Console>,
    pub(crate) policy: Policy,
    /// The local-mode note's marker, written once a harness starts.
    note: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
}

impl Live {
    /// `branch` is the provider a send or fork inherits when the flags name
    /// none.
    pub(crate) fn start(
        env: &Env,
        task: &TaskArgs,
        prefixed: bool,
        branch: Option<Provider>,
    ) -> Live {
        Live::start_to(env, task, prefixed, false, branch)
    }

    /// With `json`, activity goes to stderr so stdout holds only the result.
    pub(crate) fn start_to(
        env: &Env,
        task: &TaskArgs,
        prefixed: bool,
        json: bool,
        branch: Option<Provider>,
    ) -> Live {
        Live::begin(env, task, prefixed, json, branch, false)
    }

    /// A send, fork, reincarnation or plan decision on `branch`: where it
    /// runs, and whether it has a private home, is what the banner says
    /// when the flags name no other provider.
    pub(crate) fn start_for(
        env: &Env,
        task: &TaskArgs,
        prefixed: bool,
        json: bool,
        branch: &branchyard::Branch,
    ) -> Result<Live, Failure> {
        let provider = branch.provider()?;
        Ok(Live::begin(
            env,
            task,
            prefixed,
            json,
            provider,
            branch.info().isolated,
        ))
    }

    /// `isolated`: the branch the work continues has a private home, whether
    /// or not `task` asks for one.
    pub(crate) fn begin(
        env: &Env,
        task: &TaskArgs,
        prefixed: bool,
        json: bool,
        branch: Option<Provider>,
        isolated: bool,
    ) -> Live {
        let out: Box<dyn Write + Send> = match json {
            true => Box::new(io::stderr()),
            false => Box::new(io::stdout()),
        };
        let console = Arc::new(
            Console::new(
                Renderer::new(env.style(), prefixed),
                out,
                Box::new(console::terminal_prompt),
            )
            .with_notifier(env.notifier()),
        );
        let choice = console::choose(task.permissions, env.stdin_tty, env.stderr_tty);
        // A recipe that cannot be used is refused by `options`, below.
        let provider = provider(task).ok().flatten().or(branch);
        let isolated = isolated || task.isolated;
        // The plain local mode is a fact about the repository's setup, said
        // until a run has started with it and not on every run after
        // (`local_note`); a private home, a sandbox or a server is said every
        // time.
        let plain_local = !isolated && Provider::is_local(provider.as_ref());
        let (due, marker) = match plain_local {
            true => local_note(),
            false => (true, None),
        };
        if due {
            eprintln!("{}", banner(provider.as_ref(), isolated, None));
        }
        // A preset's file scope, held by Landlock for a local harness, is
        // said on every run, as is a host that cannot hold it.
        if Provider::is_local(provider.as_ref()) {
            if let Some(line) = guard_line(task, isolated) {
                eprintln!("{line}");
            }
        }
        if choice == (Choice::DenyAll { notice: true }) {
            eprintln!("by: {}", console::DENY_NOTICE);
        }
        let policy = console::policy(choice, console.clone());
        Live {
            console,
            policy,
            note: Arc::new(std::sync::Mutex::new(marker)),
        }
    }

    pub(crate) fn options(&self, task: &TaskArgs) -> Result<TaskOptions, Failure> {
        if !task.require_labels.is_empty() {
            return Err(Report::usage(
                "--require-label chooses among a server's workers, and this command runs here",
            )
            .next("add --remote URL, or drop --require-label")
            .into());
        }
        if task.priority.is_some() {
            return Err(Report::usage(
                "--priority orders a server's queue, and this command runs here",
            )
            .next("add --remote URL, or drop --priority")
            .into());
        }
        let console = self.console.clone();
        let note = self.note.clone();
        let exe = std::env::current_exe().ok();
        let policy = match (&exe, task.allow_delegation) {
            (Some(by), true) => self.policy.clone().allow_delegation_commands(by),
            _ => self.policy.clone(),
        };
        Ok(TaskOptions {
            harness: task.harness.clone(),
            name: task.name.clone(),
            base: task.base.clone(),
            budget: Budget {
                max_usd: task.budget_usd,
                max_turns: task.max_turns,
                max_duration: task.max_duration,
                stall_after: task.stall_after,
                stall_action: task.stall_action,
                hold_cap: None,
                follow_up_grace: None,
            },
            policy,
            check: task.check.clone(),
            observer: Some(Arc::new(move |event| {
                // A `--detach` launcher waits for this turn to start.
                crate::detach::observe(event);
                // A harness started: the local-mode note was said for it.
                if matches!(event.activity, Activity::Harness(_)) {
                    note_said(&note);
                }
                console.event(event)
            })),
            isolated: task.isolated,
            command: task.command.clone(),
            provider: provider(task)?,
            delegation: task.delegate.map(|depth| match task.no_wake {
                true => Envelope::depth(depth).no_wake(),
                false => Envelope::depth(depth),
            }),
            delegation_cli: exe,
            delegation_server: None,
            unapproved_tools: task.unapproved_tools,
            provision: provision(task)?,
            session_patch: task.session_patch.clone(),
            seats: None,
            workspace: None,
            actor: None,
            // A local harness inherits this process's environment,
            // `TRACEPARENT` included.
            trace_parent: None,
            plan: task.plan,
            goal: crate::plan_cmd::goal(task),
            result: result_for_task(task)?,
            fleet: task.fleet.clone(),
            join_task: None,
            deny: task.deny.clone(),
        })
    }

    /// [`Live::finish`], or with `json` (the console then writing to stderr:
    /// [`Live::start_to`]) the branch as `by show --json` prints it.
    pub(crate) fn finish_as(
        self,
        env: &Env,
        result: Result<Branch, branchyard::Error>,
        json: bool,
    ) -> Outcome {
        if !json {
            return self.finish(env, result);
        }
        let branch = match result {
            Ok(branch) => branch,
            Err(error) => {
                self.console.finish();
                return fail(true, &error);
            }
        };
        let descendants = wait_for_descendants(&[&branch]);
        let branch = branch.yard().branch(&branch.info().name).unwrap_or(branch);
        self.console.finish();
        descendants?;
        if let Some(view) = ResultView::local(branch.yard(), branch.info())? {
            print(&(json::text(&view.json(branch.info())) + "\n"))?;
            return view.outcome(branch.info());
        }
        show(env, &Target::Local, &branch.info().name, true, false)?;
        branch_outcome(branch.info())
    }

    /// Print the closing summary for one branch, once every branch it
    /// delegated to on this process has finished.
    pub(crate) fn finish(self, env: &Env, result: Result<Branch, branchyard::Error>) -> Outcome {
        let branch = match result {
            Ok(branch) => branch,
            Err(error) => {
                self.console.finish();
                return Err(error.into());
            }
        };
        let descendants = wait_for_descendants(&[&branch]);
        // A branch woken when its children settled ran more turns since.
        let branch = branch.yard().branch(&branch.info().name).unwrap_or(branch);
        self.console.finish();
        let check = check_of(branch.yard(), &branch.info().name);
        print(&format!(
            "\n{}",
            render::summary_with_check(branch.info(), check.as_deref(), env.style())
        ))?;
        if let Some(descendants) = descendants? {
            let infos: Vec<&BranchInfo> = descendants.iter().collect();
            print(&format!(
                "\ndelegated\n{}",
                render::comparison_table(&infos, env.style())
            ))?;
        }
        if let Some(view) = ResultView::local(branch.yard(), branch.info())? {
            print(&view.text(None))?;
            return view.outcome(branch.info());
        }
        branch_outcome(branch.info())
    }
}

/// One branch's structured-output view, shared by local and remote finishers.
pub(crate) struct ResultView {
    attempt: branchyard::Attempt,
    complete: bool,
    required: bool,
    error: Option<String>,
}

impl ResultView {
    /// Read output and run a declared deliverable check on its exact tree.
    /// A branch without a contract retains the legacy finishing path.
    pub(crate) fn local(yard: &Yard, info: &BranchInfo) -> Result<Option<Self>, Failure> {
        if yard.branch(&info.name)?.result_contract()?.is_none() {
            return Ok(None);
        }
        let names = std::slice::from_ref(&info.name);
        let mut attempts = yard.compare(names, false)?;
        let mut attempt = attempts
            .pop()
            .ok_or_else(|| Report::new("internal", "result inspection returned no attempt"))?;
        if attempt.is_deliverable()
            && Self::same_snapshot(info, &attempt)
            && Self::successful(info)
            && attempt.result.is_some()
            && attempt.result_error.is_none()
            && matches!(attempt.check, branchyard::AttemptCheck::NotRun)
        {
            attempt = yard.compare(names, true)?.pop().ok_or_else(|| {
                Report::new("internal", "checked result inspection returned no attempt")
            })?;
        }
        Ok(Self::from_attempt(info, attempt))
    }

    /// Use core result assessment supplied by a judge or remote adapter.
    pub(crate) fn from_attempt(
        info: &BranchInfo,
        mut attempt: branchyard::Attempt,
    ) -> Option<Self> {
        let contract = attempt.result_contract.as_ref()?;
        let deliverable = contract.is_deliverable();
        let required = deliverable || (contract.required && info.depth == 0);
        let mut error = attempt.result_error.clone();
        if !Self::same_snapshot(info, &attempt) {
            attempt.result = None;
            attempt.result_event = None;
            error = Some(
                "branch changed while its structured result was inspected; inspect it again".into(),
            );
        }
        if error.is_none() && attempt.result.is_none() {
            error = Some("no accepted structured result in the latest turn".into());
        }
        if error.is_none() && !Self::successful(info) {
            error = Some("the native task did not complete successfully".into());
        }
        if error.is_none() && deliverable {
            error = match &attempt.check {
                branchyard::AttemptCheck::None
                | branchyard::AttemptCheck::Passed
                | branchyard::AttemptCheck::PassedAtMerge => None,
                branchyard::AttemptCheck::Failed { .. } => {
                    Some("the configured check failed".into())
                }
                branchyard::AttemptCheck::TimedOut { .. } => {
                    Some("the configured check timed out".into())
                }
                branchyard::AttemptCheck::Error { message } => {
                    Some(format!("the configured check could not run: {message}"))
                }
                _ => Some("the configured check has not passed".into()),
            };
        }
        let complete = error.is_none() && attempt.result.is_some();
        Some(Self {
            attempt,
            complete,
            required,
            error,
        })
    }

    fn successful(info: &BranchInfo) -> bool {
        matches!(
            info.status,
            BranchStatus::Ready | BranchStatus::NoChanges | BranchStatus::Merged { .. }
        )
    }

    fn same_snapshot(info: &BranchInfo, attempt: &branchyard::Attempt) -> bool {
        attempt.turns == info.turns
            && attempt.status == info.status
            && attempt.candidate.as_deref()
                == info
                    .candidate
                    .as_ref()
                    .map(|candidate| candidate.commit.as_str())
    }

    /// Whether incomplete output makes the root task unsuccessful.
    pub(crate) fn required(&self) -> bool {
        self.required
    }
    /// Whether this turn supplied accepted output and any deliverable check passed.
    pub(crate) fn complete(&self) -> bool {
        self.complete
    }
    /// Whether the output itself can be selected without a Git candidate.
    pub(crate) fn is_deliverable(&self) -> bool {
        self.attempt.is_deliverable()
    }

    /// Keep the native BranchInfo shape/status and add explicit output evidence.
    pub(crate) fn json(&self, info: &BranchInfo) -> serde_json::Value {
        let mut value = json::branch(info);
        value["result_contract"] = serde_json::json!(self.attempt.result_contract);
        value["result"] = serde_json::json!(self.attempt.result);
        value["result_event"] = serde_json::json!(self.attempt.result_event);
        value["result_error"] = serde_json::json!(self.error);
        value["result_complete"] = serde_json::json!(self.complete);
        if self.is_deliverable() {
            value["result_check"] = serde_json::json!(self.attempt.check);
        }
        value
    }

    /// Render the answer or an incomplete-output reason. A remote adapter
    /// supplies its own next command; local callers use the comparison hint.
    pub(crate) fn text(&self, next: Option<&str>) -> String {
        match &self.error {
            Some(error) => {
                let local_next = format!(
                    "by compare {} --check --json",
                    shell_quote(&self.attempt.branch)
                );
                format!(
                    "\nresult {}: incomplete: {error}\nnext: {}\n",
                    self.attempt.branch,
                    next.unwrap_or(&local_next)
                )
            }
            None => format!(
                "\nresult {}: complete; semantic accuracy not verified\n{}\n",
                self.attempt.branch,
                self.attempt
                    .result
                    .as_ref()
                    .map(|value| branchyard::contract::one_line(value, 1000))
                    .unwrap_or_default()
            ),
        }
    }

    /// Preserve native execution failures and enforce required root output.
    pub(crate) fn outcome(&self, info: &BranchInfo) -> Outcome {
        if self.required && !self.complete {
            Err(Failure::Reported)
        } else {
            branch_outcome(info)
        }
    }
}

/// Wait for every branch these delegated to, in this process or another,
/// saying which, and return them; `None` if there were none.
pub(crate) fn wait_for_descendants(
    branches: &[&Branch],
) -> Result<Option<Vec<BranchInfo>>, Failure> {
    let mut all = Vec::new();
    for branch in branches {
        if branch.info().status == BranchStatus::WaitingOnChildren {
            eprintln!(
                "by: {}'s turn ended while its children run; its next turn starts when they \
                 settle",
                branch.info().name
            );
        }
        let running: Vec<String> = branch
            .descendants()?
            .into_iter()
            .filter(|info| info.status == BranchStatus::Running)
            .map(|info| info.name)
            .collect();
        if !running.is_empty() {
            eprintln!(
                "by: waiting for {} delegated branch{} still running: {}",
                running.len(),
                if running.len() == 1 { "" } else { "es" },
                running.join(", ")
            );
        }
        all.extend(branch.wait_subtree()?);
    }
    Ok((!all.is_empty()).then_some(all))
}

/// A branch that failed is an error for scripts; one that stopped at a
/// limit or produced nothing is not.
pub fn branch_outcome(info: &BranchInfo) -> Outcome {
    match info.status {
        BranchStatus::Failed { .. } => Err(Failure::Reported),
        _ => Ok(()),
    }
}

/// The repository's `[workspace]` for a command that creates branches,
/// once its scripts are trusted (docs/workspace.md).
pub(crate) fn workspace(
    env: &Env,
    yard: &Yard,
) -> Result<Option<branchyard::WorkspaceSpec>, Failure> {
    crate::workspace_cmd::for_new_branch(env, yard.root())
}

#[cfg(test)]
mod result_view_tests {
    use super::*;

    #[test]
    fn incomplete_output_uses_the_adapters_explicit_next_command() {
        let view = ResultView {
            attempt: branchyard::Attempt {
                branch: "answer".into(),
                harness: "fake".into(),
                status: BranchStatus::NoChanges,
                turns: 1,
                cost_usd: None,
                tokens: None,
                duration_ms: None,
                check: branchyard::AttemptCheck::None,
                candidate: None,
                result_contract: None,
                result: None,
                result_event: None,
                result_error: None,
                files_changed: 0,
                insertions: 0,
                deletions: 0,
                files: Vec::new(),
                unique_files: Vec::new(),
            },
            complete: false,
            required: true,
            error: Some("no accepted answer".into()),
        };
        assert!(view
            .text(None)
            .contains("next: by compare answer --check --json"));
        let next = "by --remote URL inspect answer --json";
        let remote = view.text(Some(next));
        assert!(remote.contains(&format!("next: {next}")));
        assert!(!remote.contains("by compare"));
    }
}
