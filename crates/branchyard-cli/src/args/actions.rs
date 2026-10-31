//! The argument groups and nested subcommands of the commands that take an
//! action (`by approvals`, `by sync`, `by models`, `by harnesses`, …).

use super::*;

/// Operator workspace management, separate from branch delegation.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum McpCommand {
    /// Manage authorized code and knowledge workspaces over MCP on stdio
    ///
    /// Use --token-file for a scoped bearer credential, or set
    /// BRANCHYARD_MANAGER_TOKEN. Add --ca-file for private trust roots.
    Manager {
        /// Branchyard server URL
        #[arg(long, value_name = "URL", value_parser = non_blank)]
        url: String,
        /// Print startup errors as JSON; MCP messages always use the stdio protocol
        #[arg(long)]
        json: bool,
    },
}

/// `by handoff`: relay a settled result without starting a recipient turn.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct HandoffArgs {
    /// The settled branch whose current result to relay
    pub source: String,
    /// The branch that receives the frozen result in its next turn
    #[arg(long, value_name = "BRANCH", value_parser = non_blank)]
    pub to: String,
    /// An immutable artifact to share with the recipient; repeatable
    #[arg(long = "artifact", value_name = "ID", value_parser = non_blank)]
    pub artifacts: Vec<String>,
    /// Context for the recipient, beside the result and its provenance
    #[arg(long, value_name = "TEXT", default_value = "")]
    pub note: String,
    /// Refuse if this JSON source version (incarnation, turn, result_event) is stale
    #[arg(long, value_name = "JSON", value_parser = non_blank)]
    pub expected_version: Option<String>,
    /// Print the durable message and frozen handoff as JSON
    #[arg(long)]
    pub json: bool,
}

/// `by approvals`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct ApprovalsArgs {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub action: Option<ApprovalsAction>,
}

/// `by effects`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct EffectsArgs {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    /// Only this branch's
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,
    #[command(subcommand)]
    pub action: Option<EffectsAction>,
}

/// `by undo`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct UndoArgs {
    /// The branch to undo
    pub branch: String,
    /// Go back to this checkpoint (default: the branch's base, 0); effects of later turns are
    /// planned
    #[arg(long, value_name = "TURN")]
    pub to: Option<u32>,
    /// Print the plan and change nothing
    #[arg(long)]
    pub plan: bool,
    /// Undo only these upstream effects (ids or their ends)
    #[arg(long, value_name = "ID", num_args = 1..)]
    pub only: Vec<String>,
    /// Undo every reversible effect without a confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print JSON
    #[arg(long)]
    pub json: bool,
}

/// `by approvals`' actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum ApprovalsAction {
    /// List the approvals waiting (the default)
    Ls {
        /// Answered ones too
        #[arg(long)]
        all: bool,
    },
    /// Allow it: the waiting call or tool goes ahead, a staged effect is performed
    Allow {
        /// The approval's id, or the end of it
        #[arg(required_unless_present = "branch")]
        id: Option<String>,
        /// The oldest approval waiting on this branch instead
        #[arg(long, value_name = "BRANCH", conflicts_with = "id")]
        branch: Option<String>,
        /// Why, recorded with the answer
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
    },
    /// Deny it: the call or tool is refused, a staged effect is discarded
    Deny {
        /// The approval's id, or the end of it
        #[arg(required_unless_present = "branch")]
        id: Option<String>,
        /// The oldest approval waiting on this branch instead
        #[arg(long, value_name = "BRANCH", conflicts_with = "id")]
        branch: Option<String>,
        /// Why, recorded with the answer
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
    },
}

/// `by effects`' actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum EffectsAction {
    /// One entry and the events it is projected from
    Show {
        /// The entry's id, or the end of it
        id: String,
    },
    /// Perform a staged effect for real: promote its draft, or make the held call
    Promote {
        /// The entry's id, or the end of it
        id: String,
    },
    /// Settle entries whose outcome is unknown through their operation's lookup
    Reconcile,
}

/// `by plan`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum PlanAction {
    /// Show a branch's plan, its task list and its phase
    Show {
        /// The branch whose plan to show
        branch: String,
    },
    /// Approve the plan and run it as the branch's next turn, with normal permissions
    Approve {
        /// The branch whose plan to approve
        branch: String,
        /// Edit the plan in your editor first; what you save is what is approved
        #[arg(long, conflicts_with = "file")]
        edit: bool,
        /// Approve this file's text instead of the proposed plan
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        file: Option<String>,
        /// The editor for --edit: a known name or a command line that waits until the file is
        /// closed (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR", requires = "edit")]
        editor: Option<String>,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
    /// Reject the plan: the branch ends, or with --replan it plans again with your reason
    Reject {
        /// The branch whose plan to reject
        branch: String,
        /// Why; with --replan, the branch's next planning turn gets it
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        reason: Option<String>,
        /// Plan again (read-only) instead of ending the branch
        #[arg(long)]
        replan: bool,
        #[command(flatten)]
        task: Checked<SendFlags>,
    },
}

/// `by knowledge`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum KnowledgeAction {
    /// The repository's entries (default: proposed and adopted)
    List {
        /// Only entries with this status: proposed, adopted or rejected
        #[arg(long, value_name = "STATUS", value_parser = knowledge_status, conflicts_with = "all")]
        status: Option<branchyard::KnowledgeStatus>,
        /// Every entry, rejected ones too
        #[arg(long)]
        all: bool,
    },
    /// One entry
    Show {
        /// The entry's number
        id: u64,
    },
    /// Walk the proposed entries one at a time: adopt, reject, edit or skip each
    Review {
        /// The editor for edits (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR")]
        editor: Option<String>,
    },
    /// Adopt entries: from now on, matching branches are given them
    Adopt {
        #[arg(required = true, value_name = "ID")]
        /// Entry numbers, as `by knowledge review` lists them
        ids: Vec<u64>,
    },
    /// Reject entries: they are not used, and the same text is not proposed again
    Reject {
        #[arg(required = true, value_name = "ID")]
        /// Entry numbers, as `by knowledge review` lists them
        ids: Vec<u64>,
        /// Why
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Change an entry's text (in your editor without --text) or scope
    Edit {
        /// The entry's number
        id: u64,
        /// The new text
        #[arg(long, value_name = "TEXT", value_parser = non_blank)]
        text: Option<String>,
        /// The path glob it applies to; \"\" for the whole repository
        #[arg(long, value_name = "GLOB")]
        path: Option<String>,
        /// The kind of task it applies to; \"\" for every kind
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// The editor (default: $VISUAL, then $EDITOR)
        #[arg(long, value_name = "EDITOR", conflicts_with = "text")]
        editor: Option<String>,
    },
    /// Add an entry you wrote, adopted (or only proposed, with --propose)
    Add {
        #[arg(value_parser = non_blank)]
        /// The entry's text
        text: String,
        /// Only for tasks touching files matching this glob, such as crates/parser/**
        #[arg(long, value_name = "GLOB")]
        path: Option<String>,
        /// Only for tasks of this kind
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
        /// Add it as proposed, for review, instead of adopted
        #[arg(long)]
        propose: bool,
    },
    /// Remove an entry
    Rm {
        /// The entry's number
        id: u64,
    },
    /// Propose entries from a branch now
    ///
    /// Propose entries from a branch now: the corrections sent into it and the review
    /// comments it addressed, or a distiller harness's proposals
    Distill {
        /// The branch to propose entries from
        branch: String,
        /// Ask this harness to distill, read-only on a scratch branch (default: [knowledge]
        /// distiller, else the deterministic extractor)
        #[arg(long, value_name = "ID", conflicts_with = "deterministic")]
        harness: Option<String>,
        /// Launch the distiller with this instead of its executable, for development and testing
        #[arg(long, value_name = "CMD", value_parser = command_argv, requires = "harness")]
        command: Option<Argv>,
        /// Use only the deterministic extractor, even when [knowledge] names a distiller
        #[arg(long)]
        deterministic: bool,
    },
    /// Adopted entries as an AGENTS.md-style Markdown file
    Export {
        /// Write it here instead of stdout
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        out: Option<String>,
    },
}

pub(crate) fn knowledge_status(text: &str) -> Result<branchyard::KnowledgeStatus, String> {
    text.parse().map_err(|e: branchyard::Error| match e {
        branchyard::Error::Unsupported(why) => why,
        other => other.to_string(),
    })
}

/// `by fleet`'s actions.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum FleetAction {
    /// Recorded outcomes per kind and candidate: runs, merged, judged best, failed, cost, time
    Stats {
        /// Only this kind
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
    },
    /// What the router would pick for a prompt, without running anything
    Route {
        /// The task
        prompt: String,
        /// The task's kind instead of the classifier's
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
        /// Attempts, as for `by fan --auto`
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=16))]
        attempts: Option<u32>,
        /// Seed the router for a reproducible pick
        #[arg(long, value_name = "N")]
        seed: Option<u64>,
    },
    /// What to know before picking a harness for a task
    ///
    /// What to know before picking a harness for a task: the candidates with whether they
    /// can run here, their recorded outcomes for its kind and the router's own pick. Inside
    /// a harness, as its branch: its task's fleet entry, or the harnesses it may run, and
    /// its budget left
    Advise {
        /// The task
        prompt: String,
        /// The task's kind instead of the classifier's
        #[arg(long, value_name = "KIND", value_parser = task_kind)]
        kind: Option<branchyard::TaskKind>,
        /// Seed the router's pick for a reproducible advice
        #[arg(long, value_name = "N")]
        seed: Option<u64>,
    },
}

/// Options for `--provider microsandbox`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SandboxArgs {
    pub image: String,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub pass_env: Vec<String>,
    /// `--keep-sandbox`, `--sandbox-snapshots`, `--max-paused`,
    /// `--max-kept-minutes`.
    pub lifecycle: LifecycleArgs,
    /// `--live-branch`: declare the SDK's pause, live branch and full
    /// snapshots (unqualified).
    pub live_branch: bool,
}

/// What happens to a sandboxed branch's sandbox between turns; see
/// docs/sandbox-snapshots.md.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LifecycleArgs {
    pub keep: Option<branchyard::SandboxKeep>,
    pub snapshots: Option<u32>,
    pub max_paused: Option<u32>,
    pub max_kept_minutes: Option<u32>,
}

impl LifecycleArgs {
    /// `--max-kept-minutes` as the engine's seconds.
    pub fn max_kept_secs(&self) -> Option<u64> {
        self.max_kept_minutes.map(|m| u64::from(m) * 60)
    }
}

/// Options for `--provider substrate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubstrateArgs {
    pub endpoint: String,
    pub router: String,
    pub template: String,
    pub key: String,
    /// Control bearer token source, reread per RPC.
    pub api_token_file: Option<String>,
    /// TLS service name used to verify Control.
    pub server_name: Option<String>,
    /// Explicit actor network policy.
    pub egress: branchyard::config::SubstrateEgress,
    pub atespace: Option<String>,
    pub workdir: Option<String>,
    pub home: Option<String>,
    pub pass_env: Vec<String>,
    /// `--substrate-ca`: authorities for an `https://` Control API, and for
    /// the router unless `router_ca` is given.
    pub ca: Option<String>,
    /// `--substrate-client-cert` and `--substrate-client-key`: a client
    /// certificate for the Control API (mutual TLS).
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    /// `--substrate-router-ca`: authorities for an `https://` or `wss://`
    /// router.
    pub router_ca: Option<String>,
    /// `--substrate-insecure`: allow `http://` or `ws://` to hosts other
    /// than loopback.
    pub insecure: bool,
    /// `--keep-sandbox`, `--sandbox-snapshots`, `--max-paused`,
    /// `--max-kept-minutes`.
    pub lifecycle: LifecycleArgs,
}

/// Options for `--provider recipe:NAME`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecipeArgs {
    /// The recipe's name, `[recipes.NAME]`.
    pub name: String,
    /// `--recipe-workdir`: where the worktree goes on the machine.
    pub workdir: Option<String>,
    /// `--recipe-home`: the harness's `HOME` on the machine.
    pub home: Option<String>,
    pub pass_env: Vec<String>,
    /// `--keep-sandbox` and `--max-paused`.
    pub lifecycle: LifecycleArgs,
}

/// Options of `by spawn`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnArgs {
    /// Harness, name, base, check, limits and permissions for the child.
    pub task: TaskArgs,
    /// The delegating branch, outside a harness.
    pub parent: Option<String>,
    pub wait: bool,
    pub max_depth: Option<u32>,
    /// `--max-children`: at most the parent's.
    pub max_children: Option<u32>,
    /// `--harnesses`: what the child may delegate to.
    pub harnesses: Option<Vec<String>>,
    pub deny: Vec<String>,
    /// `--seat`: the rig seat the child fills.
    pub seat: Option<String>,
    /// `--depends-on`: siblings the child waits for.
    pub depends_on: Vec<String>,
    /// `--after`: when each of them counts as done.
    pub after: branchyard::After,
    /// `--bind NAME:ACCESS`, repeatable.
    pub bindings: Vec<branchyard::Binding>,
    /// `--connector`, repeatable: the child's grant, narrowed to its
    /// parent's. Empty: its seat's or its parent's.
    pub connectors: Vec<branchyard::connectors::GrantEntry>,
    /// `--plan`: the child plans first, read-only.
    pub plan: bool,
    /// `--model`: the child's model. Unset: its seat's or its parent's.
    pub model: Option<String>,
    /// `--why`: the parent's reason for the harness and model, recorded on
    /// the child.
    pub why: Option<String>,
    /// `--result-schema`: the child's result contract's schema, as JSON
    /// text.
    pub result_schema: Option<String>,
    /// `--result-schema-file`: the same, in a file (`-` for standard
    /// input).
    pub result_schema_file: Option<String>,
    /// `--result-required`: the child must report a result.
    pub result_required: bool,
    /// Accept a current structured answer as the deliverable without a Git change.
    pub result_deliverable: bool,
    /// Optional producer-bound artifact capture for a required deliverable.
    pub result_capture: Option<branchyard::ResultCapture>,
    /// `--owns`, repeatable: the paths the child owns. Empty: none of its
    /// own, bounded only by the scope its parent works under.
    pub owns: Vec<String>,
    /// `--land`: leave the child to the engine to land.
    pub land: bool,
    /// `--review` and its `--review-*` flags: the child's reviewer before
    /// it lands. Boxed, so `Command` stays small.
    pub review: Option<Box<branchyard::ReviewSpec>>,
    pub json: bool,
}

/// `by graph show|apply|resume ...`; see `docs/graph.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphArgs {
    pub action: String,
    /// `show`'s branch, or `apply`'s proposal file (`-` for stdin).
    pub arg: Option<String>,
    /// The branch whose graph `apply` changes, outside a harness.
    pub parent: Option<String>,
    /// `--edits JSON`: the edits inline, instead of a file.
    pub edits: Option<String>,
    /// `--expected-revision N`, overriding a file's.
    pub expected_revision: Option<u64>,
    /// Permissions for the children's turns, outside a harness.
    pub task: TaskArgs,
    pub json: bool,
}

/// `by rig check FILE` or `by rig run FILE PROMPT`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RigArgs {
    /// `run` with its prompt; `None` for `check`.
    pub prompt: Option<String>,
    pub file: String,
    /// The root branch's name, instead of the rig's.
    pub name: Option<String>,
    pub base: Option<String>,
    /// The root's executable, for development and testing.
    pub command: Option<Vec<String>>,
    pub unapproved_tools: bool,
    pub json: bool,
}

/// `by artifact publish|list|get|share|export|import ...`; see `docs/storage.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactArgs {
    pub action: String,
    /// `publish`'s file, `get`/`share`'s id, or `import`'s bundle file.
    pub arg: Option<String>,
    /// `export`'s artifact ids, one or more.
    pub ids: Vec<String>,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub labels: Vec<(String, String)>,
    pub out: Option<String>,
    pub to: Option<String>,
    pub branch: Option<String>,
    pub json: bool,
}

/// `by scratch create|list|lock|unlock|share ...`; see `docs/storage.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScratchArgs {
    pub action: String,
    pub name: Option<String>,
    pub to: Option<String>,
    pub branch: Option<String>,
    pub json: bool,
}

/// `by sync`'s arguments, parsed in a function of their own (it keeps
/// the stack frame of `Command`'s parser small).
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct SyncArgs {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    /// A branch, or a task ID (`<repository key>.<branch>`); every branch when omitted
    pub task: Option<String>,
    #[command(subcommand)]
    pub action: Option<SyncAction>,
}

/// `by sync ...`; see docs/sync.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum SyncAction {
    /// The remote, this machine's device name, each task's state and lag, what is queued, and
    /// the counters
    Status,
    /// Bring a task from the remote: create or fast-forward its refs here
    Pull {
        /// A branch, or a task ID from `by sync ls`
        task: String,
    },
    /// The tasks in the remote
    Ls,
    /// Collect garbage: unreferenced objects and tasks past retention
    ///
    /// Collect garbage: objects no manifest references, after their grace period; tasks
    /// past their retention unless held
    Gc {
        /// Say what would be deleted, and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Read a sample of objects back and check each against its name; repair chunks from here
    Scrub {
        /// Objects to read (all of them when larger than their number)
        #[arg(long, default_value_t = 100)]
        sample: usize,
        /// The sample's seed, to repeat one
        #[arg(long)]
        seed: Option<u64>,
    },
    /// Put a task on legal hold (never deleted or collected), or release it
    Hold {
        /// A branch, or a task ID
        task: String,
        /// Why, recorded with the hold
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Release the hold instead
        #[arg(long)]
        release: bool,
    },
    /// Delete a task from the remote (refused under a legal hold); `gc` reclaims its objects
    Rm {
        /// A branch, or a task ID
        task: String,
    },
    /// Rotate the tenant key
    ///
    /// Rotate the tenant key: a new key version, every object's data key rewrapped, the old
    /// versions retired
    RotateKey {
        /// Wrap the keys from now on with this instead: `passphrase` (the new one in
        /// BRANCHYARD_SYNC_NEW_PASSPHRASE) or a kms:// URL
        #[arg(long, value_name = "WRAPPER")]
        to: Option<String>,
    },
}

/// `by services ...`; see docs/registry.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ServicesAction {
    /// Reclaim the services whose lease ran out or whose owner is gone
    ///
    /// Expire services whose lease ran out or whose owner is gone, and reclaim what
    /// Branchyard started for them
    Gc,
}

/// `by models ...`; see docs/model-gateway.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ModelsAction {
    /// Ask each provider which models your keys can call
    ///
    /// Ask each provider whose key you have which models it serves, cache the answer, and
    /// mark the models no price covers
    #[command(after_help = MODELS_LIST_EXAMPLES)]
    List {
        /// Ask the providers again instead of showing the cache
        #[arg(long)]
        refresh: bool,
        /// A harness in the catalog: its static list of models beside what its key's provider lists
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
        /// Gemini's API base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_GEMINI_URL",
            default_value = "https://generativelanguage.googleapis.com"
        )]
        gemini_url: String,
        /// OpenAI's API base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_OPENAI_URL",
            default_value = "https://api.openai.com"
        )]
        openai_url: String,
        /// Anthropic's API base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_ANTHROPIC_URL",
            default_value = "https://api.anthropic.com"
        )]
        anthropic_url: String,
    },
}

/// `by catalog ...`; see docs/registry.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum CatalogAction {
    /// Refresh the connector and harness catalogs from live registries
    ///
    /// Fetch the official MCP registry's servers and the npm registry's latest harness
    /// versions, verify them and cache them with their checksums
    Refresh {
        /// The MCP registry's base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_MCP_REGISTRY",
            default_value = "https://registry.modelcontextprotocol.io"
        )]
        mcp_registry: String,
        /// The npm registry's base URL
        #[arg(
            long,
            value_name = "URL",
            env = "BRANCHYARD_NPM_REGISTRY",
            default_value = "https://registry.npmjs.org"
        )]
        npm_registry: String,
        /// The most pages of the MCP registry to read (100 servers each)
        #[arg(long, value_name = "N", default_value = "20")]
        max_pages: usize,
        /// Only the connectors, or only the harnesses
        #[arg(long, value_name = "WHICH", value_parser = ["connectors", "harnesses"])]
        only: Option<String>,
    },
    /// What the cache holds, where it came from, and whether its checksums verify
    Status,
}

/// `by gateway ...`; see docs/connectors.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum GatewayAction {
    /// Start the gateway, supervised, in the background (or in this terminal)
    Start {
        /// Run in this terminal until interrupted, restarting the gateway if it exits
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the gateway started in the background
    Stop,
    /// Whether the gateway runs and listens, what it serves, and the signing keys
    Status,
    /// Put a new signing key first; tokens the previous one signed stay valid until they expire
    RotateKey {
        /// Older keys to keep publishing
        #[arg(long, value_name = "N", default_value = "1")]
        keep: usize,
    },
    /// Print the public keys the gateway verifies tokens against (JWKS)
    Jwks,
}

/// `by harnesses ACTION`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum HarnessesAction {
    /// Install a harness with the catalog's command, then check it
    ///
    /// Install a harness with the catalog's command, when [harnesses] install allows, then
    /// check it is there
    Install(HarnessChange),
    /// Install a harness again at a newer or pinned version, then check it
    Update(HarnessChange),
    /// Log in to a harness once, through its own flow, or store its API key
    Login {
        /// The harness's ID, as `by harnesses --all` lists it
        id: String,
        /// Store an API key, read from standard input, for the harness's key variable instead
        /// (in a 0600 file beside your user configuration, named in its [secrets])
        #[arg(long)]
        api_key: bool,
        /// Log the harness in to another model provider with that provider's key: zai (Z.ai's
        /// GLM models) or gemini. The key is read from standard input as --api-key reads it
        /// (once per provider; --api-key reads a new one), stored as ZAI_API_KEY or
        /// GEMINI_API_KEY, and [logins] records the choice; isolated and sandboxed branches
        /// get the endpoint, token variable and model, and runs in your own home the export line
        #[arg(long, value_name = "NAME")]
        provider: Option<String>,
        /// Answer yes to writing a setting the login needs (Antigravity's modelProvider, when
        /// GEMINI_API_KEY is set; with --provider, the harness's own configuration)
        #[arg(long)]
        yes: bool,
    },
    /// Every install, update and login recorded on this machine
    Log,
}

/// What `by harnesses install|update` takes.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct HarnessChange {
    /// The harness's ID, as `by harnesses --all` lists it
    pub id: String,
    /// The version to install (default: the version its profile was checked against, where
    /// the install command can take one)
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Install without asking, where [harnesses] install = "ask"
    #[arg(long)]
    pub yes: bool,
}

/// `by connectors ...`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ConnectorsAction {
    /// Every connector in catalog/connectors.toml: kind, authentication, credential names
    Catalog,
}

/// `by skills ...`; see docs/knowledge-skills.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum SkillsAction {
    /// The skills a new branch here runs with, or the set fixed for a branch
    ///
    /// The built-in skills, and the repository's from .agents/skills at HEAD with whether
    /// you trusted them; with a branch, the set fixed when it was created, staged or
    /// withheld and why.
    Ls {
        /// The branch whose set to show; inside a harness, your own. Without one, what a new
        /// branch here runs with
        branch: Option<String>,
    },
    /// Trust the repository's skills at HEAD, so new branches here stage them
    Trust,
    /// Forget the trust decision for this repository's skills
    Untrust,
}

/// `by workspace ...`; see docs/workspace.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    /// The effective [workspace], where it came from, whether it is trusted, and ports
    Show {
        /// One branch's workspace: what it was created with, its setup, its port
        branch: Option<String>,
    },
    /// Trust this repository's [workspace] scripts as they are now
    Trust,
    /// Forget the trust decision for this repository
    Untrust,
    /// Run a named [workspace.run] script in a branch's worktree; with --detach, several at once
    Run {
        /// The branch (default: $BRANCHYARD_BRANCH inside a harness), then the
        /// script's name (default: the one marked default, or the only one); with --detach,
        /// several names start each its own script, each with its own port
        #[arg(value_name = "BRANCH [NAME...]")]
        args: Vec<String>,
        /// Return as soon as the script has started, leaving it running in the background; its
        /// output goes to a log file
        #[arg(long)]
        detach: bool,
    },
    /// The TCP ports a branch's processes listen on (default: every branch's)
    Ports {
        /// One branch (default: every branch with a worktree here)
        branch: Option<String>,
    },
    /// Open a port a branch listens on in a browser ($BROWSER, xdg-open or open)
    Browse {
        /// The branch (default: $BRANCHYARD_BRANCH inside a harness)
        branch: Option<String>,
        /// The port (default: the branch's only one, or its reserved BRANCHYARD_PORT)
        #[arg(long)]
        port: Option<u16>,
        /// Print the URL instead of opening it
        #[arg(long)]
        print: bool,
    },
    /// Stop the processes listening on a branch's ports (SIGTERM)
    Kill {
        /// The branch (default: $BRANCHYARD_BRANCH inside a harness)
        branch: Option<String>,
        /// Only the process listening on this port
        #[arg(long)]
        port: Option<u16>,
        /// Stop the process without a confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

/// `by env ...`; see docs/environments.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum EnvAction {
    /// Every prepared environment and recorded failure, newest first
    List,
    /// One environment: its inputs, setup, what it holds (default: the current key)
    Show {
        /// A key, or the start of one
        key: Option<String>,
    },
    /// Build the current key's environment now from HEAD; a failure keeps the last good one
    Rebuild,
    /// Remove old environments; the newest of each recipe and linked ones stay
    Prune {
        /// Only these keys (or starts of keys), even the newest
        #[arg(value_name = "KEY")]
        keys: Vec<String>,
        /// Good environments kept per recipe (default 3)
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
        /// Remove those unused for this many days (default 14)
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
    /// The warm pool of ready worktrees (`[workspace.pool]`); see docs/pools.md
    #[command(subcommand)]
    Pool(PoolAction),
}

/// `by env pool ...`; see docs/pools.md.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum PoolAction {
    /// The pool's slots: ready, being made, being claimed
    Status,
    /// Make the pool full again, replacing stale slots
    ///
    /// Discard stale slots and make new ones until the pool is full (runs setup when the
    /// environment is not built)
    Fill,
    /// Remove every ready slot on this host
    Drain,
}

/// `by config ...`.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum ConfigAction {
    /// Every effective value and where it came from
    Show,
    /// The user and project files, and whether they exist
    Path,
    /// Load FILE, or every file by reads, strictly
    Validate {
        /// One file to check (default: the user and project files, and their merge)
        #[arg(value_hint = ValueHint::FilePath)]
        file: Option<String>,
    },
    /// The JSON Schema of the file (schema/branchyard.config.json)
    Schema,
}

/// Which step of the JSON protocol `by init` takes; none runs the wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitStep {
    /// `--next`: the next batch of questions, or the plan.
    Next,
    /// `--dry-run`: the plan, writing nothing.
    DryRun,
    /// `--apply`: write the plan.
    Apply,
}

/// `by init`, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitArgs {
    pub topic: Option<branchyard_setup::Topic>,
    pub step: Option<InitStep>,
    pub json: bool,
    /// `--answers FILE`, or `-` for stdin.
    pub answers: Option<String>,
    pub defaults: bool,
    pub force: bool,
    /// `by init plugin ID...`: the harnesses to install for.
    pub harnesses: Vec<String>,
    /// `by init plugin --all-detected`.
    pub all_detected: bool,
    /// `by init plugin --status`: what is installed, and what is left to do.
    pub status: bool,
}

/// `by init`'s options. At most one step; a step needs a topic; `--answers`
/// needs a step and `--force` needs `--apply`: all clap's to enforce.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(group(ArgGroup::new("step").args(["next", "dry_run", "apply"]).requires("topic")))]
pub struct InitFlags {
    /// What to set up (default: the wizard asks)
    #[arg(value_name = "TOPIC", value_parser = TopicParser)]
    topic: Option<branchyard_setup::Topic>,
    /// With plugin: the harnesses to install hooks, commands and skills for (by init plugin lists
    /// them)
    #[arg(value_name = "HARNESS", requires = "topic")]
    harnesses: Vec<String>,
    /// With plugin: every harness found on PATH
    #[arg(long, conflicts_with = "harnesses")]
    all_detected: bool,
    /// With plugin: per harness, whether its install is current, stale or missing, whether `by`
    /// is on PATH for its hooks, and the one-time steps still left to you; writes nothing
    #[arg(long, conflicts_with_all = ["all_detected", "next", "dry_run", "apply"])]
    status: bool,
    /// Print JSON (schema/setup.protocol.json): the topics, or the step's result
    #[arg(long)]
    json: bool,
    /// Print the next batch of at most four questions given the answers so far, or the plan
    /// when none remain
    #[arg(long, help_heading = "Protocol steps (for harnesses and scripts)")]
    next: bool,
    /// Print the plan: files, diffs and validation; write nothing
    #[arg(long, help_heading = "Protocol steps (for harnesses and scripts)")]
    dry_run: bool,
    /// Write the plan; refuses to replace a file that differs
    #[arg(long, help_heading = "Protocol steps (for harnesses and scripts)")]
    apply: bool,
    /// With --apply: replace files that differ (shown as diffs)
    #[arg(
        long,
        requires = "apply",
        // A requirement that conflicts with a given argument is not
        // enforced, so the other steps are refused by name.
        conflicts_with_all = ["next", "dry_run"],
        help_heading = "Protocol steps (for harnesses and scripts)"
    )]
    force: bool,
    /// A JSON object of answers by question id; - reads stdin
    #[arg(
        long,
        value_name = "FILE|-",
        requires = "step",
        value_hint = ValueHint::FilePath,
        help_heading = "Protocol steps (for harnesses and scripts)"
    )]
    answers: Option<String>,
    /// Take the default for every unanswered question
    #[arg(long)]
    defaults: bool,
}

impl Flags for InitFlags {
    type Output = InitArgs;
    fn check(self) -> Result<InitArgs, String> {
        let step = match (self.next, self.dry_run, self.apply) {
            (true, _, _) => Some(InitStep::Next),
            (_, true, _) => Some(InitStep::DryRun),
            (_, _, true) => Some(InitStep::Apply),
            _ => None,
        };
        let plugin = self.topic == Some(branchyard_setup::Topic::Plugin);
        if (!self.harnesses.is_empty() || self.all_detected || self.status) && !plugin {
            return Err("harnesses, --all-detected and --status go with by init plugin".into());
        }
        if self.json && self.topic.is_some() && step.is_none() && !plugin {
            return Err("with --json and a topic, give --next, --dry-run or --apply".into());
        }
        Ok(InitArgs {
            topic: self.topic,
            step,
            json: self.json,
            answers: self.answers,
            defaults: self.defaults,
            force: self.force,
            harnesses: self.harnesses,
            all_detected: self.all_detected,
            status: self.status,
        })
    }
}

/// `by setup`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct SetupFlags {
    /// Guide repository selection, compute setup and readiness checks
    #[arg(long)]
    pub guided: bool,
    /// Bring configured prerequisites up and include their readiness in the result
    #[arg(long)]
    pub up: bool,
    /// Set up this repository instead of the current directory
    #[arg(long, value_name = "PATH")]
    pub directory: Option<std::path::PathBuf>,
    /// Take every default and ask nothing (the only way to write without a terminal)
    #[arg(long)]
    pub yes: bool,
    /// Show what would be detected, written and installed; write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Print one JSON object: detected, written, installed, todo, next
    #[arg(long)]
    pub json: bool,
    /// Install hooks only for these harnesses, named even if not on PATH (default: every harness
    /// on PATH that has an install; by init plugin lists them)
    #[arg(
        long = "harness",
        value_name = "ID",
        num_args = 1..,
        value_delimiter = ',',
        conflicts_with = "no_hooks"
    )]
    pub harnesses: Vec<String>,
    /// Do not install hooks, commands or skills into any harness
    #[arg(long)]
    pub no_hooks: bool,
    /// Do not write branchyard.toml
    #[arg(long)]
    pub no_config: bool,
    /// Ask the harnesses again instead of reading the inventory `by harnesses` cached
    #[arg(long)]
    pub refresh: bool,
}

/// `by init`'s topics, from [`branchyard_setup::Topic::ALL`]: completed by
/// every shell, listed in the man page, and refused with the list.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TopicParser;

impl TypedValueParser for TopicParser {
    type Value = branchyard_setup::Topic;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let known = |text: String| {
            branchyard_setup::Topic::parse(&text).ok_or_else(|| {
                let ids: Vec<&str> = branchyard_setup::Topic::ALL
                    .iter()
                    .map(|t| t.id())
                    .collect();
                format!("unknown topic '{text}'; use {}", ids.join(", "))
            })
        };
        StringValueParser::new()
            .try_map(known)
            .parse_ref(cmd, arg, value)
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            branchyard_setup::Topic::ALL
                .into_iter()
                .map(|t| PossibleValue::new(t.id()).help(t.summary())),
        ))
    }
}

/// `by graph ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum GraphAction {
    /// Show a branch's children, their dependencies and the graph's revision
    Show {
        /// Default: this harness's own branch
        branch: Option<String>,
    },
    /// Apply a graph proposal to a branch's children, atomically
    #[command(
        group(clap::ArgGroup::new("proposal").required(true).args(["file", "edits"])),
        after_help = GRAPH_APPLY_HELP
    )]
    Apply {
        /// The proposal, {"expected_revision", "edits"}; - for stdin
        file: Option<String>,
        /// The branch whose graph changes (outside a harness; inside one, it is the
        /// harness's own)
        #[arg(long, value_name = "BRANCH")]
        parent: Option<String>,
        /// The proposal's edits inline, as a JSON array, instead of a file
        #[arg(long, value_name = "JSON", requires = "expected_revision")]
        edits: Option<String>,
        /// The graph revision the proposal was made against (by graph show); overrides the
        /// file's
        #[arg(long, value_name = "N")]
        expected_revision: Option<u64>,
        #[command(flatten)]
        perms: Perms,
    },
    /// Start dependents whose prerequisites settled while no engine was running
    Resume {
        #[command(flatten)]
        perms: Perms,
    },
}

impl GraphAction {
    pub fn into_args(self, json: bool) -> GraphArgs {
        let mut args = GraphArgs {
            json,
            ..GraphArgs::default()
        };
        let perms = match self {
            GraphAction::Show { branch } => {
                args.action = "show".into();
                args.arg = branch;
                return args;
            }
            GraphAction::Apply {
                file,
                parent,
                edits,
                expected_revision,
                perms,
            } => {
                args.action = "apply".into();
                args.arg = file;
                args.parent = parent;
                args.edits = edits;
                args.expected_revision = expected_revision;
                perms
            }
            GraphAction::Resume { perms } => {
                args.action = "resume".into();
                perms
            }
        };
        perms.apply(&mut args.task);
        args
    }
}

/// `by rig ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum RigAction {
    /// Check a rig spec and print its plan
    Check {
        /// The rig spec, TOML (see docs/rigs.md)
        file: String,
    },
    /// Run a rig's root seat with a prompt
    Run {
        /// The rig spec, TOML (see docs/rigs.md)
        file: String,
        /// The root seat's task; quote it
        #[arg(value_parser = prompt_text)]
        prompt: String,
        /// The root branch's name (default: the rig's)
        #[arg(short, long)]
        name: Option<String>,
        /// Base revision (default: HEAD)
        #[arg(short, long, value_name = "REV")]
        base: Option<String>,
        /// Launch this instead of the root's executable, for development and testing
        #[arg(long, value_name = "CMD", value_parser = command_argv)]
        command: Option<Argv>,
        /// Run a profile that does not route tool permission requests to Branchyard
        #[arg(long)]
        allow_unapproved_tools: bool,
    },
}

impl RigAction {
    pub fn into_args(self, json: bool) -> RigArgs {
        match self {
            RigAction::Check { file } => RigArgs {
                file,
                json,
                ..RigArgs::default()
            },
            RigAction::Run {
                file,
                prompt,
                name,
                base,
                command,
                allow_unapproved_tools,
            } => RigArgs {
                prompt: Some(prompt),
                file,
                name,
                base,
                command: command.map(|argv| argv.0),
                unapproved_tools: allow_unapproved_tools,
                json,
            },
        }
    }
}

/// `by artifact ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum ArtifactAction {
    /// Publish a file as a new artifact of the acting branch
    Publish {
        /// The file to publish
        file: String,
        /// The artifact's name (default: the file's)
        #[arg(short, long)]
        name: Option<String>,
        /// The artifact's media type (default: application/octet-stream)
        #[arg(long, value_name = "TYPE")]
        media_type: Option<String>,
        /// A label on the artifact; repeatable
        #[arg(long = "label", value_name = "KEY=VALUE", value_parser = label)]
        labels: Vec<(String, String)>,
    },
    /// List the artifacts the acting branch may read
    List,
    /// Write an artifact's bytes to a file
    Get {
        /// The artifact's ID, as `by artifact list` shows it
        id: String,
        /// Where to write the artifact's bytes
        #[arg(short, long, value_name = "PATH")]
        out: String,
    },
    /// Let another branch read an artifact
    Share {
        /// The artifact's ID, as `by artifact list` shows it
        id: String,
        /// The branch to share with
        #[arg(long, value_name = "BRANCH")]
        to: String,
    },
    /// Write artifacts into one verified, deterministic tar bundle (local only)
    Export {
        #[arg(required = true)]
        /// The artifacts' IDs, as `by artifact list` shows them
        ids: Vec<String>,
        /// Where to write the bundle's tar
        #[arg(short, long, value_name = "PATH")]
        out: String,
    },
    /// Publish every artifact of a bundle as new artifacts (local only)
    Import {
        /// A bundle from 'by artifact export'
        file: String,
    },
}

impl ArtifactAction {
    pub fn into_args(self, branch: Option<String>, json: bool) -> ArtifactArgs {
        let mut args = ArtifactArgs {
            branch,
            json,
            ..ArtifactArgs::default()
        };
        args.action = match self {
            ArtifactAction::Publish {
                file,
                name,
                media_type,
                labels,
            } => {
                args.arg = Some(file);
                args.name = name;
                args.media_type = media_type;
                args.labels = labels;
                "publish"
            }
            ArtifactAction::List => "list",
            ArtifactAction::Get { id, out } => {
                args.arg = Some(id);
                args.out = Some(out);
                "get"
            }
            ArtifactAction::Share { id, to } => {
                args.arg = Some(id);
                args.to = Some(to);
                "share"
            }
            ArtifactAction::Export { ids, out } => {
                args.arg = ids.first().cloned();
                args.ids = ids;
                args.out = Some(out);
                "export"
            }
            ArtifactAction::Import { file } => {
                args.arg = Some(file);
                "import"
            }
        }
        .into();
        args
    }
}

/// `by scratch ...`.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum ScratchAction {
    /// Create a scratch area owned by the acting branch
    Create {
        /// The scratch area's name
        name: String,
    },
    /// List the scratch areas the acting branch may reach
    List,
    /// Take a scratch area's writer lock
    Lock {
        /// The scratch area's name
        name: String,
    },
    /// Release a scratch area's writer lock
    Unlock {
        /// The scratch area's name
        name: String,
    },
    /// Let another branch reach a scratch area
    Share {
        /// The scratch area to share
        name: String,
        /// The branch to share with
        #[arg(long, value_name = "BRANCH")]
        to: String,
    },
}

impl ScratchAction {
    pub fn into_args(self, branch: Option<String>, json: bool) -> ScratchArgs {
        let (action, name, to) = match self {
            ScratchAction::Create { name } => ("create", Some(name), None),
            ScratchAction::List => ("list", None, None),
            ScratchAction::Lock { name } => ("lock", Some(name), None),
            ScratchAction::Unlock { name } => ("unlock", Some(name), None),
            ScratchAction::Share { name, to } => ("share", Some(name), Some(to)),
        };
        ScratchArgs {
            action: action.into(),
            name,
            to,
            branch,
            json,
        }
    }
}
