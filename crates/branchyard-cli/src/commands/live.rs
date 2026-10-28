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
            seats: None,
            workspace: None,
            actor: None,
            // A local harness inherits this process's environment,
            // `TRACEPARENT` included.
            trace_parent: None,
            plan: task.plan,
            goal: crate::plan_cmd::goal(task),
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
        branch_outcome(branch.info())
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
