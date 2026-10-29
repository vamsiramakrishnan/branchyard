//! [`Command`]: every `by` command, with its positionals and flags.

use super::*;

#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum Command {
    /// Run a task on a new branch; waits for the turn (--detach returns at once)
    #[command(display_order = 100, after_help = RUN_EXAMPLES)]
    Run {
        /// The task for the harness; quote it (with --issue, added to the issue's text)
        #[arg(
            required_unless_present_any = ["issue", "pr"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        #[command(flatten)]
        detached: Detached,
        #[command(flatten)]
        task: Checked<RunFlags>,
    },
    /// Run a task on several harnesses in parallel; waits, then compares
    #[command(display_order = 101, after_help = FAN_EXAMPLES)]
    Fan {
        /// The task for every harness; quote it (with --issue, added to the issue's text)
        #[arg(
            required_unless_present_any = ["issue", "pr"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Harnesses to run on, one branch each (default with a [fleet]: routed, as --auto)
        #[arg(long = "harness", value_name = "ID,ID,...", value_parser = harness_list)]
        harnesses: Option<List>,
        /// Branches to start when routed (default: the [fleet] entry's attempts)
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=16))]
        attempts: Option<u32>,
        /// Then judge the attempts and propose one, as `by judge` does
        #[arg(long)]
        judge: bool,
        /// Print JSON: the branches, as `by ls --json` prints them (activity goes to standard
        /// error), and with --judge the judgement
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        task: Checked<FanFlags>,
    },
    /// Run one prompt over a list of items, a branch for each
    ///
    /// Every item of the list gets its own branch, and the answers are collected.
    #[command(
        display_order = 101,
        after_help = MAP_EXAMPLES,
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true
    )]
    Map {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<MapAction>,
        #[command(flatten)]
        map: MapArgs,
    },
    /// Send a branch another prompt; waits for the turn (--detach returns at once)
    #[command(display_order = 102, after_help = SEND_EXAMPLES)]
    Send {
        /// The branch to send to; `by ls` lists them
        branch: String,
        /// The next prompt; quote it
        #[arg(
            required_unless_present_any = ["retry", "prompt_file"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Read the prompt from this file instead, `-` for standard input: for a long prompt,
        /// or one a shell would mangle
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Add the prompt to the branch's running turn without interrupting it, instead of
        /// starting a new turn; refused when no turn runs or the harness cannot take it
        #[arg(long)]
        steer: bool,
        /// Submit again the prompt of the branch's last turn that was cut off when the engine
        /// running it stopped (the recovery note names it), instead of a new prompt
        #[arg(long, conflicts_with_all = ["steer", "prompt", "prompt_file"])]
        retry: bool,
        /// Wait for the turn to end and show it (outside a harness, send always waits)
        #[arg(long)]
        wait: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Return as soon as the turn has started, leaving it running in the background; its
        /// output goes to a log under .branchyard/detached/ (local mode)
        #[arg(long, conflicts_with_all = ["ask", "steer", "wait"])]
        detach: bool,
        #[command(flatten)]
        task: Checked<SendSessionFlags>,
    },
    /// Add to a branch's running turn without interrupting it
    ///
    /// The same as `by send --steer`.
    #[command(display_order = 102, after_help = STEER_EXAMPLES)]
    Steer {
        /// The branch whose running turn to add to
        branch: String,
        /// What to add; quote it
        #[arg(
            required_unless_present = "prompt_file",
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Read it from this file instead, `-` for standard input: for a long text, or one a
        /// shell would mangle
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Comment on a branch's diff in your editor and send the comments
    ///
    /// Opens the diff in your editor, then sends every comment as one prompt.
    #[command(display_order = 111, after_help = REVIEW_EXAMPLES)]
    Review {
        /// The branch whose diff to review
        branch: String,
        /// Print the prompt the comments make instead of sending it
        #[arg(long)]
        print: bool,
        /// The editor: a known name or a command line that waits until the file is closed, such
        /// as "code --wait" (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR", conflicts_with = "file")]
        editor: Option<String>,
        /// Read the comments from this edited review file instead of opening an editor
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        file: Option<String>,
        /// Return as soon as the send has started, leaving it running in the background; its
        /// output goes to a log beside the draft
        #[arg(long, conflicts_with = "print")]
        detach: bool,
        /// Print JSON: the prompt with --print, else what `by send --json` prints
        #[arg(long, conflicts_with = "detach")]
        json: bool,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
    /// Fork a branch's work and conversation; waits (--detach returns at once)
    #[command(display_order = 103, after_help = FORK_EXAMPLES)]
    Fork {
        /// The branch to fork
        branch: String,
        /// The new branch's prompt; quote it
        prompt: String,
        /// Start a new session if the harness cannot fork its conversation
        #[arg(long)]
        fresh_session: bool,
        /// Fork from the branch's checkpoint N (0 is its base) instead of its candidate; the
        /// session forks only if it ended there, else a fresh one starts with a summary
        #[arg(long, value_name = "N", conflicts_with = "fresh_session")]
        at: Option<u32>,
        #[command(flatten)]
        detached: Detached,
        #[command(flatten)]
        task: Checked<ForkFlags>,
    },
    /// Fork a branch's candidate into a fresh session with a generated handoff brief
    #[command(display_order = 104)]
    Reincarnate {
        /// The branch to start over from
        branch: String,
        #[command(flatten)]
        task: Checked<ReincarnateFlags>,
    },
    /// Merge a branch's candidate after its check passes
    #[command(display_order = 105, after_help = MERGE_EXAMPLES)]
    Merge {
        /// The branch to merge
        branch: String,
        /// Local branch to merge into (default: the current branch)
        #[arg(long, value_name = "TARGET")]
        into: Option<String>,
        /// Then remove the branch, running its workspace teardown, as `by rm` does
        #[arg(long)]
        rm: bool,
        /// Also perform the branch's staged effects (drafts and held calls), as approving
        /// each would
        #[arg(long)]
        promote_effects: bool,
        /// Print JSON: the branch, what it was merged into, the merge commit and the check's
        /// result
        #[arg(long)]
        json: bool,
    },
    /// A branch's workspace: trust its scripts, show it, or run a named script in it
    #[command(display_order = 110, subcommand_required = true, after_help = WORKSPACE_EXAMPLES)]
    Workspace {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Prepared environments: list, show, rebuild or prune them; the warm pool
    #[command(display_order = 111, subcommand_required = true, after_help = ENV_EXAMPLES)]
    Env {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: EnvAction,
    },
    /// Environment recipes: repository scripts that create a VM
    ///
    /// List, check and trust them (see docs/recipes.md).
    #[command(
        display_order = 112,
        subcommand_required = true,
        after_help = crate::recipe_cmd::RECIPE_EXAMPLES
    )]
    Recipe {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: crate::recipe_cmd::RecipeAction,
    },
    /// Remove a branch and free its name
    ///
    /// Removes the branch's worktree and record. A removed child leaves its parent's
    /// children, and what its subtree spent still counts in the parent's budget.
    #[command(display_order = 106)]
    Rm {
        /// The branch to remove
        branch: String,
        /// Keep the credential files provisioning wrote in a home a fork still uses
        #[arg(long)]
        keep_credentials: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Reset a branch to one of its per-turn checkpoints
    #[command(display_order = 107, after_help = REWIND_EXAMPLES)]
    Rewind {
        /// The branch to reset
        branch: String,
        /// The checkpoint to go to: a turn number, or 0 for the branch's base
        #[arg(long, value_name = "N")]
        to: u32,
        /// Confirm the reset without a prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Tasks: what was asked, and its attempts
    ///
    /// In this repository, a folder you grant, or none.
    #[command(display_order = 107, subcommand_required = true, after_help = TASK_EXAMPLES)]
    Task(Box<TaskCommand>),
    /// Apply a branch's changes to this checkout to try them; --off restores it
    #[command(display_order = 108, after_help = TRY_EXAMPLES)]
    Try {
        /// The branch to try (another branch's try is turned off first)
        #[arg(required_unless_present_any = ["off", "status"], conflicts_with_all = ["off", "status"])]
        branch: Option<String>,
        /// Restore the checkout to what it held before the try
        #[arg(long, conflicts_with = "status")]
        off: bool,
        /// Show what is being tried
        #[arg(long)]
        status: bool,
        /// With --off: restore even files changed since the try, discarding those changes
        #[arg(long, conflicts_with_all = ["branch", "status"])]
        force: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Relay a settled result and selected artifacts without steering its recipient
    #[command(display_order = 226)]
    Handoff(HandoffArgs),
    /// Compare attempts side by side, and pick one to merge
    #[command(display_order = 109, after_help = COMPARE_EXAMPLES)]
    Compare {
        /// Branches to compare
        #[arg(required_unless_present_any = ["fan", "diff"], conflicts_with = "fan")]
        branches: Vec<String>,
        /// Compare the branches one `by fan` started, named NAME-<harness>
        #[arg(long, value_name = "NAME")]
        fan: Option<String>,
        /// Run each branch's check on its exact candidate, in a private worktree
        #[arg(long)]
        check: bool,
        /// Show the diff from attempt A's candidate to B's
        #[arg(long, num_args = 2, value_names = ["A", "B"], conflicts_with = "pick")]
        diff: Option<Vec<String>>,
        /// Merge this attempt (its check must pass, as with by merge)
        #[arg(long, value_name = "BRANCH")]
        pick: Option<String>,
        /// Local branch to merge the pick into (default: the current branch)
        #[arg(long, value_name = "TARGET", requires = "pick")]
        into: Option<String>,
        /// After the pick merges, remove the other attempts
        #[arg(long, requires = "pick")]
        discard_others: bool,
        /// Confirm removing the other attempts without a prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Score attempts at one task, and propose or merge one
    ///
    /// Optionally with a judge harness.
    #[command(display_order = 109, after_help = JUDGE_EXAMPLES)]
    Judge {
        /// A `by fan`'s name, or the branches to judge
        #[arg(required = true, value_name = "FAN|BRANCH")]
        targets: Vec<String>,
        /// Ask this harness for a verdict (default: the [fleet] entry's judge, if any)
        #[arg(long, value_name = "ID", conflicts_with = "deterministic")]
        harness: Option<String>,
        /// Score without a judge harness, even when the [fleet] names one
        #[arg(long)]
        deterministic: bool,
        /// Require a valid model verdict; a failed judge leaves every attempt unpickable
        #[arg(long, conflicts_with = "deterministic")]
        require_judge: bool,
        /// Launch the judge harness with this instead of its executable, for development and
        /// testing
        #[arg(long, value_name = "CMD", value_parser = command_argv, requires = "harness")]
        command: Option<Argv>,
        /// More rubric for the judge harness, after the default
        #[arg(long, value_name = "TEXT")]
        rubric: Option<String>,
        /// Merge the proposed pick, as `by compare --pick` does
        #[arg(long)]
        pick: bool,
        /// Local branch to merge the pick into (default: the current branch)
        #[arg(long, value_name = "TARGET", requires = "pick")]
        into: Option<String>,
        /// After the pick merges, remove the other attempts
        #[arg(long, requires = "pick")]
        discard_others: bool,
        /// Confirm removing the other attempts without a prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Approvals waiting for an answer: list them, allow or deny one
    #[command(display_order = 114, after_help = APPROVALS_EXAMPLES)]
    Approvals(ApprovalsArgs),
    /// The effect ledger: what branches did outside the machine
    ///
    /// Show, promote or reconcile an entry.
    #[command(display_order = 115, after_help = EFFECTS_EXAMPLES)]
    Effects(EffectsArgs),
    /// Rewind a branch and undo what it did upstream
    ///
    /// Rewinds its files and conversation, and undoes what it did upstream where the
    /// upstream allows.
    #[command(display_order = 116, after_help = UNDO_EXAMPLES)]
    Undo(UndoArgs),
    /// A branch's plan: show it, approve it (as proposed or edited) or reject it
    #[command(display_order = 112, subcommand_required = true, after_help = PLAN_EXAMPLES)]
    Plan {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Repository knowledge: what agents are told
    ///
    /// Review, adopt, reject, edit, add, remove or export it.
    #[command(display_order = 113, subcommand_required = true, after_help = KNOWLEDGE_EXAMPLES)]
    Knowledge {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: KnowledgeAction,
    },
    /// The fleet table's routing: statistics and where a prompt goes
    ///
    /// Outcome statistics, or what a prompt would be routed to.
    #[command(display_order = 207, subcommand_required = true, after_help = FLEET_EXAMPLES)]
    Fleet {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: FleetAction,
    },
    /// Open or update a ready branch's GitHub pull request
    ///
    /// Pushes the branch first; --watch feeds CI and reviews back.
    #[command(display_order = 110, after_help = PR_EXAMPLES)]
    Pr {
        /// The branch to open the pull request for
        branch: String,
        #[command(flatten)]
        pr: Checked<PrFlags>,
    },
    /// List branches
    #[command(display_order = 200)]
    Ls {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize branches, turns and cost (and, with --remote, the server's queue)
    #[command(display_order = 209)]
    Stats {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one branch
    #[command(display_order = 201)]
    Show {
        /// The branch to show; `by ls` lists them
        branch: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Ask GitHub (through gh) for the branch's pull request state first, instead of
        /// showing the last one recorded (local mode)
        #[arg(long)]
        refresh: bool,
    },
    /// Show a branch's candidate diff against its base
    #[command(display_order = 202)]
    Diff {
        /// The branch whose changes to show
        branch: String,
    },
    /// Show a branch's recorded events
    #[command(display_order = 203)]
    Log {
        /// The branch whose events to show
        branch: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Keep printing events as they are recorded, until interrupted; with --json, one
        /// object per line
        #[arg(short, long)]
        follow: bool,
    },
    /// Watch every branch live: status, activity, cost
    #[command(display_order = 204, after_help = WATCH_EXAMPLES)]
    Watch {
        /// Time between refreshes: seconds, or with a unit such as 250ms or 2s
        #[arg(long, value_name = "SECONDS", default_value = "1", value_parser = watch_interval)]
        interval: Duration,
        /// Print the tree once and exit
        #[arg(long)]
        once: bool,
    },
    /// Harnesses installed here or elsewhere; install, update, log in
    ///
    /// Shows each harness with its version, login and quota; installs, updates or logs in
    /// to one.
    #[command(display_order = 205, after_help = HARNESSES_EXAMPLES)]
    Harnesses {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        /// Every harness CLI in catalog/harnesses.toml, with install and login commands, API-key
        /// variables and models where known, marking the ones Branchyard can drive
        #[arg(long, conflicts_with_all = ["profiles", "on"])]
        all: bool,
        /// The harness profiles Branchyard drives, and whether each is on PATH
        #[arg(long, conflicts_with = "on")]
        profiles: bool,
        /// Detect again rather than use what was detected in the last minute
        #[arg(long)]
        refresh: bool,
        /// Another machine: ssh://[user@]host[:port], or recipe:NAME (a fresh machine from the
        /// repository's recipe)
        #[arg(long, value_name = "TARGET", global = true)]
        on: Option<String>,
        #[command(subcommand)]
        action: Option<HarnessesAction>,
    },
    /// Each Claude Code and Codex login's usage and when it resets
    ///
    /// The 5-hour and weekly usage of each local login.
    #[command(display_order = 208, after_help = USAGE_EXAMPLES)]
    Usage {
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Make a session already on this machine a branch
    ///
    /// A Claude Code or Codex session; --list shows them.
    #[command(display_order = 112, after_help = ADOPT_EXAMPLES)]
    Adopt {
        /// The session's ID, or a unique start of it (default: list them)
        session: Option<String>,
        /// List this repository's sessions (the default without SESSION)
        #[arg(long)]
        list: bool,
        /// The branch's name (default: a slug of the session's first prompt)
        #[arg(short, long, conflicts_with = "list")]
        name: Option<String>,
        /// Do not carry the session directory's uncommitted changes into the branch
        #[arg(long, conflicts_with = "list")]
        no_diff: bool,
        /// The profile its turns run with, one of the session's harness's (default: the
        /// harness's default profile), such as claude-code-acp
        #[arg(long, value_name = "ID", conflicts_with = "list")]
        harness: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// The catalog of connectors the connector gateway (Anvil) can adopt
    ///
    /// See docs/connectors.md.
    #[command(display_order = 207, subcommand_required = true)]
    Connectors {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ConnectorsAction,
    },
    /// Open a branch's worktree in your editor
    #[command(display_order = 206, after_help = OPEN_EXAMPLES)]
    Open {
        /// The branch whose worktree to open
        branch: String,
        /// The editor: code, cursor, zed, windsurf, subl, idea, nvim, ... or a command line
        /// (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR")]
        editor: Option<String>,
        /// Print the worktree's path instead of opening it
        #[arg(long, conflicts_with = "editor")]
        print: bool,
    },
    /// Delegate to a new child branch of this branch
    #[command(hide = true, display_order = 300, after_help = SPAWN_EXAMPLES)]
    Spawn {
        /// The child's task; quote it (with --issue, added to the issue's text)
        #[arg(
            required_unless_present_any = ["issue", "prompt_file"],
            default_value = "",
            hide_default_value = true
        )]
        prompt: String,
        /// Read the child's task from this file instead, `-` for standard input: for a long
        /// prompt, or one a shell would mangle
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        #[command(flatten)]
        spawn: Checked<SpawnFlags>,
    },
    /// Hand the engine a plan: several children at once
    ///
    /// With the brief they share; then end your turn, and you are woken with a summary when
    /// they have settled.
    #[command(hide = true, display_order = 300, after_help = DELEGATE_EXAMPLES)]
    Delegate {
        /// The plan as JSON, `-` for standard input: {"brief": TEXT, "tasks": [TASK, ...]},
        /// each task as the spawn tool takes a child ({"prompt", "name", "owns", "depends_on",
        /// "budget_usd", "check", "harness", ...}); depends_on may name another task
        #[arg(long, value_name = "PATH")]
        plan: String,
        /// The brief the children share, in place of the plan's: the goal, the conventions,
        /// the interfaces between their parts
        #[arg(long, value_name = "TEXT", value_parser = non_blank)]
        brief: Option<String>,
        /// Leave every task to the engine to land, as `spawn --land` does: each is integrated
        /// into your branch when it is ready, and your next turn says what landed
        #[arg(long)]
        land: bool,
        /// The delegating branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, value_name = "BRANCH")]
        parent: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        perms: Perms,
    },
    /// Show or add to this branch's brief for its children
    ///
    /// The brief every child it delegates to is given each turn; with TEXT, add an entry
    /// first, sent to the children running now.
    #[command(hide = true, display_order = 300)]
    Brief {
        /// A new entry: a decision every child must follow, such as an interface you settled
        #[arg(conflicts_with = "file")]
        text: Option<String>,
        /// Read the new entry from this file instead, `-` for standard input
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// The delegating branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, value_name = "BRANCH")]
        parent: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a branch's status, candidate, cost, budget and last message
    #[command(hide = true, display_order = 301)]
    Inspect {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a branch's recorded events from a cursor
    #[command(hide = true, display_order = 302)]
    Events {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Start at event N (default: the most recent)
        #[arg(long, value_name = "N")]
        cursor: Option<usize>,
        /// At most N events (default 50, at most 200)
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Merge delegated children into this branch after their check
    ///
    /// Merges delegated children into their parent's branch after their check passes;
    /// several are merged together, all or none, and checked once on the result.
    #[command(hide = true, display_order = 303)]
    Integrate {
        #[arg(required = true, value_name = "BRANCH")]
        /// The children to merge, all of one parent
        branches: Vec<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Run a branch's check on its current work, without merging
    ///
    /// Runs the check on the branch's work merged into its parent's branch, as integrate
    /// would, without integrating it; run it on yourself before you finish.
    #[command(display_order = 303)]
    Check {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Wait for branches to settle
    ///
    /// Waits for delegated branches to settle: all of them, or the first with --any. Each
    /// is printed as it settles (with --json, as a JSON line on stderr), then the result.
    #[command(display_order = 303)]
    Wait {
        /// Default, inside a harness: its children still running
        #[arg(value_name = "BRANCH")]
        branches: Vec<String>,
        /// Return when the first of them settles
        #[arg(long, conflicts_with = "all")]
        any: bool,
        /// Return when all of them have settled (the default)
        #[arg(long)]
        all: bool,
        /// Give up after this many seconds; the result says timed_out, and by exits 1. Keep it
        /// under any limit of your own (a shell `timeout`, a tool call's time limit): a wait
        /// stopped from outside prints no result, only the branches that settled before
        #[arg(long, value_name = "SECONDS", value_parser = seconds)]
        timeout: Option<f64>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Stop a branch's running turn and the turns below it
    ///
    /// Stops the turn and every turn delegated below it (a settled child: by discard).
    #[command(display_order = 304)]
    Cancel {
        /// The branch whose turn to stop
        branch: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Set a settled child aside; it is never integrated
    ///
    /// It ends discarded with your reason, keeps its record and cost, and frees its slot in
    /// its parent's max_children.
    #[command(hide = true, display_order = 304, after_help = DISCARD_EXAMPLES)]
    Discard {
        /// The settled child to set aside
        branch: String,
        /// Why, recorded with it (default: who discarded it)
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// List the branches a branch delegated to
    #[command(hide = true, display_order = 305)]
    Children {
        /// Default: this harness's own branch
        branch: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a branch's children and dependencies, or apply a proposal
    ///
    /// See docs/graph.md.
    #[command(hide = true, display_order = 306, subcommand_required = true)]
    Graph {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: GraphAction,
    },
    /// Ask this branch's parent a question
    #[command(hide = true, display_order = 307)]
    Ask {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// The question
        text: String,
        /// Block up to this many seconds for an answer (default: return once the question is sent)
        #[arg(long = "wait", value_name = "SECONDS", value_parser = seconds, allow_negative_numbers = true)]
        wait_seconds: Option<f64>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Report to this branch's parent, optionally with a JSON result
    ///
    /// A result is checked against the contract the parent gave at spawn (`by inspect` shows it
    /// as result_contract); a mismatch is refused naming the path and the rule, and nothing is
    /// recorded. The parent reads the latest result in `by inspect` and `by wait`.
    #[command(hide = true, display_order = 308)]
    Report {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// What to report
        text: String,
        /// The branch's result, as JSON text
        #[arg(long, value_name = "JSON", value_parser = non_blank, conflicts_with = "result_file")]
        result: Option<String>,
        /// Read the result from this file instead, `-` for standard input
        #[arg(long, value_name = "PATH", value_parser = non_blank)]
        result_file: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Escalate to this branch's parent, or further up if its rig seat allows
    #[command(hide = true, display_order = 309)]
    Escalate {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// What to escalate
        text: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// State this branch's own status in one line, beside the one Branchyard derives
    ///
    /// A claim the harness makes about itself, shown in `by log`, `by watch`, `by show` and
    /// `by inspect`; it changes nothing about the branch. `blocked` and `failure` are also
    /// escalated to the parent's inbox.
    #[command(hide = true, display_order = 312)]
    ReportStatus {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// What the harness is doing
        #[arg(value_parser = ["working", "idle", "blocked", "complete", "failure"])]
        state: String,
        /// One line, at most 280 characters
        summary: String,
        /// An http(s) URL with more, such as a pull request or an issue
        #[arg(long, value_name = "URL")]
        link: Option<String>,
        /// How long the claim holds (whole seconds, 1s to 7d, such as 10m or 2h); after it, the
        /// report is shown as stale
        #[arg(long, value_name = "DURATION", value_parser = report_ttl)]
        ttl: Option<Duration>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Answer a message (usually a question) from a descendant
    #[command(hide = true, display_order = 310)]
    Answer {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// The message's number, as `by inbox` lists it
        message_id: u64,
        /// The answer
        text: String,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// List messages addressed to this branch
    #[command(hide = true, display_order = 311)]
    Inbox {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long = "as", value_name = "BRANCH")]
        as_branch: Option<String>,
        /// Only messages not yet delivered to a turn
        #[arg(long)]
        unread: bool,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Check a rig spec and print its plan, or run its root seat with a prompt
    #[command(display_order = 400, subcommand_required = true)]
    Rig {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: RigAction,
    },
    /// Publish, list, read, share, export or import an artifact (see docs/storage.md)
    #[command(display_order = 401, subcommand_required = true)]
    Artifact {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, global = true, value_name = "NAME")]
        branch: Option<String>,
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ArtifactAction,
    },
    /// Create, list, lock, unlock or share a scratch area (see docs/storage.md)
    #[command(display_order = 402, subcommand_required = true)]
    Scratch {
        /// Act as this branch (outside a harness; inside one, it is the harness's own)
        #[arg(long, global = true, value_name = "NAME")]
        branch: Option<String>,
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ScratchAction,
    },
    /// Serve repositories over an authenticated HTTP API (see 'by serve --help')
    // Its options are the server's own: `main` hands everything after `serve`
    // to `branchyard_server::cli` (see `server_call`) before this parser runs.
    #[command(hide = true, display_order = 500, disable_help_flag = true)]
    Serve {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "SERVER_OPTIONS"
        )]
        /// The server's own options (see `by serve --help`)
        args: Vec<String>,
    },
    /// Run the operations queued in a PostgreSQL database (see 'by worker --help')
    // `by serve --worker`; like `serve`, handed to the server's own parser.
    #[command(hide = true, display_order = 501, disable_help_flag = true)]
    Worker {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "SERVER_OPTIONS"
        )]
        /// The server's own options (see `by worker --help`)
        args: Vec<String>,
    },
    /// Serve a branch's delegation tools over MCP on stdio (started by the engine)
    #[command(hide = true, display_order = 502)]
    Mcp {
        /// Repository root
        #[arg(long, value_name = "DIR", required_unless_present = "list_tools")]
        root: Option<String>,
        /// The branch whose turn this server serves
        #[arg(long, value_name = "NAME", required_unless_present = "list_tools")]
        branch: Option<String>,
        /// Print the tools it serves, with their input schemas, as JSON, and exit; needs no
        /// repository, branch or token
        #[arg(long, conflicts_with_all = ["root", "branch"])]
        list_tools: bool,
    },
    /// Run tasks on a schedule or from webhooks
    ///
    /// Add, list, show, test, enable, disable and rm triggers, and see their runs (see
    /// docs/triggers.md).
    #[command(
        display_order = 503,
        subcommand_required = true,
        after_help = crate::trigger_cmd::TRIGGER_EXAMPLES
    )]
    Trigger {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: crate::trigger_cmd::TriggerAction,
    },
    /// Servers that --remote ssh:// started on other hosts
    ///
    /// Their status, or stop one (see docs/remote-ssh.md).
    #[command(
        display_order = 504,
        subcommand_required = true,
        after_help = crate::ssh_remote::REMOTE_EXAMPLES
    )]
    Remote {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: crate::ssh_remote::RemoteAction,
    },
    /// Print a shell completion script for by
    #[command(hide = true, display_order = 600, after_help = COMPLETIONS_EXAMPLES)]
    Completions {
        #[arg(value_enum)]
        /// The shell to print the script for
        shell: clap_complete::Shell,
    },
    /// Print by's man page (roff), for `man -l -` or a man directory
    #[command(hide = true, display_order = 601)]
    Man {
        /// Print the command reference as Markdown instead (docs/cli-reference.md)
        #[arg(long)]
        markdown: bool,
    },
    /// Set this repository up in one command
    ///
    /// Project defaults, hooks for the harnesses on PATH, and what only you can do.
    #[command(display_order = 602, after_help = SETUP_EXAMPLES)]
    Setup {
        #[command(flatten)]
        setup: SetupFlags,
    },
    /// Set up Branchyard by interview
    ///
    /// Project defaults, a server, a rig, a deployment, skills.
    #[command(display_order = 602, after_help = INIT_EXAMPLES)]
    Init {
        #[command(flatten)]
        init: Checked<InitFlags>,
    },
    /// Show, locate or validate branchyard.toml and the user configuration
    #[command(display_order = 603, subcommand_required = true, after_help = CONFIG_EXAMPLES)]
    Config {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Check that this machine and repository are ready, and say what to fix
    #[command(display_order = 604, after_help = DOCTOR_EXAMPLES)]
    Doctor {
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Check the repository at PATH instead of the one around the current directory
        #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
        path: Option<String>,
        /// Ask the harnesses again instead of reading the inventory `by harnesses` cached
        #[arg(long)]
        refresh: bool,
    },
    /// Bring the configured backends up
    ///
    /// Installs harnesses, starts the gateway, and says what only you can do.
    #[command(display_order = 605, after_help = UP_EXAMPLES)]
    Up {
        /// Print JSON
        #[arg(long)]
        json: bool,
        /// Inspect readiness without installing harnesses or starting services
        #[arg(long)]
        plan: bool,
        /// Make the installs the [harnesses] policy would ask about, without asking
        #[arg(long)]
        yes: bool,
    },
    /// Prepare or check the saved Agent Substrate connection
    #[command(display_order = 605)]
    Substrate {
        #[command(subcommand)]
        action: crate::substrate_cmd::Action,
    },
    /// Plan or provision dedicated cloud workers from this machine; operator only
    #[command(display_order = 605)]
    Compute {
        #[command(subcommand)]
        action: crate::cloud_setup::Action,
    },
    /// Tell a harness session what changed in this repository
    ///
    /// Tells a Claude Code session (or, with --harness, another harness's) what changed in
    /// this repository's branches. Run by the plugin's hooks with the hook's JSON on stdin;
    /// prints nothing and exits 0 on any problem.
    #[command(hide = true, display_order = 606, after_help = HOOK_EXAMPLES)]
    Hook {
        /// Read and answer in this harness's hook dialect: claude-code (the default), codex,
        /// gemini-cli, qwen-code, github-copilot, droid, deepseek-harness, cursor, antigravity,
        /// kiro, hermes, or generic
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
        /// session-start or user-prompt-submit
        event: String,
    },
    /// Print one line about the branches for a status line
    ///
    /// For Claude Code's (or, with --harness, another harness's) status line. Reads its
    /// JSON on stdin; prints nothing outside a repository Branchyard uses.
    #[command(hide = true, display_order = 607, after_help = STATUSLINE_EXAMPLES)]
    Statusline {
        /// Read the directory from this harness's status-line JSON (as for `by hook --harness`)
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
    },
    /// Run, stop or inspect the connector gateway (Anvil), and its keys
    ///
    /// The connector gateway (Anvil) serves this repository's connectors to branches.
    #[command(display_order = 403, subcommand_required = true, after_help = GATEWAY_EXAMPLES)]
    Gateway {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: GatewayAction,
    },
    /// The model gateway: routes, backends, budgets and cost
    ///
    /// What its calls cost; `list` asks the providers which models your keys can call.
    #[command(
        display_order = 405,
        after_help = MODELS_EXAMPLES,
        args_conflicts_with_subcommands = true
    )]
    Models {
        /// Usage over this period (UTC): day, month, or all
        #[arg(long, value_name = "PERIOD", default_value = "month", value_parser = ["day", "month", "all"])]
        period: String,
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: Option<ModelsAction>,
    },
    /// Connect your account for a connector through the connector gateway (Anvil)
    #[command(display_order = 404, after_help = CONNECT_EXAMPLES)]
    Connect {
        /// The connector, as the gateway serves it, such as github
        connector: String,
        /// Which of your accounts to connect (default: your default account)
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        /// For a key-based connector: read the API key or personal token from stdin
        #[arg(long)]
        api_key_stdin: bool,
        /// Also open the authorization URL in your browser
        #[arg(long)]
        open: bool,
    },
    /// What Branchyard started or found, with health and owner
    ///
    /// Gateways, proxies, servers, workers, sandboxes and pool keepers, with health, lease
    /// and owner; gc reclaims what expired.
    #[command(display_order = 405, after_help = SERVICES_EXAMPLES)]
    Services {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        /// Only services of this kind, such as connector_gateway or server
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Also those that left or were reclaimed (kept for an hour)
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        action: Option<ServicesAction>,
    },
    /// Sync tasks to durable storage: push and pull
    ///
    /// A branch, or every branch that changed.
    #[command(
        display_order = 407,
        after_help = SYNC_EXAMPLES,
        args_conflicts_with_subcommands = true,
        subcommand_negates_reqs = true
    )]
    Sync(SyncArgs),
    /// Refresh the connector and harness catalogs from live registries
    ///
    /// Or show what is cached.
    #[command(display_order = 406, subcommand_required = true, after_help = CATALOG_EXAMPLES)]
    Catalog {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: CatalogAction,
    },
}

impl Command {
    /// The recipe `--provider recipe:NAME` names, for commands that take
    /// it: a server never runs one, so remote mode refuses it before
    /// connecting.
    pub fn recipe(&self) -> Option<&str> {
        let task: &TaskArgs = match self {
            Command::Run { task, .. } => task,
            Command::Fan { task, .. } => task,
            Command::Fork { task, .. } => task,
            Command::Reincarnate { task, .. } => task,
            Command::Send { task, .. } => task,
            Command::Review { task, .. } => task,
            Command::Spawn { spawn, .. } => &spawn.task,
            Command::Task(task) => match &task.action {
                TaskAction::New(new) => &new.task,
                TaskAction::Fork(fork) => &fork.flags,
                _ => return None,
            },
            _ => return None,
        };
        task.recipe.as_ref().map(|r| r.name.as_str())
    }
}
