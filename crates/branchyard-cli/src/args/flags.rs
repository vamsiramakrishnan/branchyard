//! The flag groups commands flatten ([`Limits`], [`Perms`], [`Launch`], …) and
//! the checked flag sets ([`Flags`], [`Checked`]) whose combinations clap
//! cannot express.

use super::*;

/// Flags whose combination is checked after clap parses them, producing
/// [`Flags::Output`]; flattened into a command as [`Checked<Self>`].
pub trait Flags: Args + FromArgMatches {
    type Output;
    /// The checked value, or the usage error's message.
    fn check(self) -> Result<Self::Output, String>;
}

/// `F`, checked while parsing: a failed [`Flags::check`] is a usage error
/// like any other. Dereferences to the checked value.
pub struct Checked<F: Flags>(F::Output, PhantomData<fn() -> F>);

impl<F: Flags> Checked<F> {
    pub fn new(output: F::Output) -> Checked<F> {
        Checked(output, PhantomData)
    }

    #[cfg(test)]
    pub fn into_inner(self) -> F::Output {
        self.0
    }
}

impl<F: Flags> Deref for Checked<F> {
    type Target = F::Output;
    fn deref(&self) -> &F::Output {
        &self.0
    }
}

/// `branchyard.toml` defaults fill what the flags left unset
/// (`crate::defaults`).
impl<F: Flags> DerefMut for Checked<F> {
    fn deref_mut(&mut self) -> &mut F::Output {
        &mut self.0
    }
}

impl<F: Flags> Clone for Checked<F>
where
    F::Output: Clone,
{
    fn clone(&self) -> Self {
        Checked::new(self.0.clone())
    }
}

impl<F: Flags> fmt::Debug for Checked<F>
where
    F::Output: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<F: Flags> PartialEq for Checked<F>
where
    F::Output: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<F: Flags> FromArgMatches for Checked<F> {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let flags = F::from_arg_matches(matches)?;
        flags
            .check()
            .map(Checked::new)
            .map_err(|message| clap::Error::raw(ErrorKind::ArgumentConflict, message))
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl<F: Flags> Args for Checked<F> {
    fn group_id() -> Option<clap::Id> {
        F::group_id()
    }

    fn augment_args(cmd: clap::Command) -> clap::Command {
        F::augment_args(cmd)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        F::augment_args_for_update(cmd)
    }
}

/// An argument vector split from one string like a shell (`--check`,
/// `--command`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Argv(pub Vec<String>);

/// A comma-separated list, trimmed, without empty or repeated entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct List(pub Vec<String>);

impl Deref for List {
    type Target = Vec<String>;
    fn deref(&self) -> &Vec<String> {
        &self.0
    }
}

/// `--stall-action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StallActionArg {
    /// Record the stall and keep running (default)
    Notify,
    /// Record the stall, then interrupt the turn
    Interrupt,
}

impl From<StallActionArg> for branchyard::StallAction {
    fn from(action: StallActionArg) -> Self {
        match action {
            StallActionArg::Notify => branchyard::StallAction::Notify,
            StallActionArg::Interrupt => branchyard::StallAction::Interrupt,
        }
    }
}

/// `--after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AfterArg {
    /// Its last turn ended ready or no_changes, or it was merged (default)
    Settled,
    /// It was integrated into the parent
    Integrated,
}

impl From<AfterArg> for branchyard::After {
    fn from(after: AfterArg) -> Self {
        match after {
            AfterArg::Settled => branchyard::After::Settled,
            AfterArg::Integrated => branchyard::After::Integrated,
        }
    }
}

/// `--provider`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderArg {
    /// A process on this host (the default for a new branch)
    Local,
    /// A Microsandbox microVM; needs --image
    Microsandbox,
    /// An Agent Substrate actor; needs the --substrate-* endpoints and key
    Substrate,
    /// The machine an environment recipe makes (`recipe:NAME`)
    Recipe(String),
}

/// `local`, `microsandbox`, `substrate` or `recipe:NAME`.
pub(crate) fn provider_arg(value: &str) -> Result<ProviderArg, String> {
    match value {
        "local" => Ok(ProviderArg::Local),
        "microsandbox" => Ok(ProviderArg::Microsandbox),
        "substrate" => Ok(ProviderArg::Substrate),
        other => match other.strip_prefix("recipe:") {
            Some(name) if branchyard_setup::config::valid_recipe_name(name) => {
                Ok(ProviderArg::Recipe(name.to_owned()))
            }
            Some(name) => Err(format!(
                "{name:?} is not a recipe name (1 to 64 of a-z, 0-9, '.', '_' and '-', \
                 starting with a letter or digit)"
            )),
            None => Err(format!(
                "{other:?} is not a provider [possible values: local, microsandbox, substrate, \
                 recipe:NAME]"
            )),
        },
    }
}

/// `--check`, the budget, turn and time limits, and stall detection.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Checks and limits")]
pub struct Limits {
    /// Command that must pass before the branch merges, such as "cargo test": split into words
    /// like a shell does, then run directly, not through a shell (use `sh -c "..."` for pipes)
    #[arg(long, value_name = "CMD", value_parser = check_argv)]
    check: Option<Argv>,
    /// Stop once the harness's own cost estimate, or on the model gateway its metered cost,
    /// exceeds this many dollars
    #[arg(long, value_name = "DOLLARS", value_parser = usd, allow_negative_numbers = true)]
    budget_usd: Option<f64>,
    /// Stop after N turns
    #[arg(long, value_name = "N", value_parser = positive_turns, allow_negative_numbers = true)]
    max_turns: Option<u32>,
    /// Interrupt the turn after this many minutes (or a duration such as 90s or 2h)
    #[arg(long, value_name = "MINUTES", value_parser = max_minutes, allow_negative_numbers = true)]
    max_minutes: Option<Duration>,
    /// Mark the branch stalled after this many minutes with no harness activity (or 90s, 2h, ...)
    #[arg(long, value_name = "MINUTES", value_parser = stall_after, allow_negative_numbers = true)]
    stall_after: Option<Duration>,
    /// What a stall does
    #[arg(long, value_name = "ACTION", requires = "stall_after")]
    stall_action: Option<StallActionArg>,
}

impl Limits {
    fn apply(self, task: &mut TaskArgs) {
        task.check = self.check.map(|argv| argv.0);
        task.budget_usd = self.budget_usd;
        task.max_turns = self.max_turns;
        task.max_duration = self.max_minutes;
        task.stall_after = self.stall_after;
        task.stall_action = self.stall_action.map(Into::into).unwrap_or_default();
    }
}

/// How tool permission requests are answered.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Permissions")]
pub struct Perms {
    /// Grant every tool permission request without asking; the harness may write files and run
    /// commands
    #[arg(short, long, conflicts_with = "ask")]
    yes: bool,
    /// Ask on the terminal for each tool permission request
    #[arg(long)]
    ask: bool,
    /// Answer tool permission requests by a named preset: read-only, edit-worktree, or full
    /// (as --yes); see docs/egress.md#permission-presets
    #[arg(
        long,
        value_name = "PRESET",
        value_parser = branchyard::PolicyPreset::parse,
        conflicts_with_all = ["yes", "ask"]
    )]
    permissions: Option<branchyard::PolicyPreset>,
    /// Run a profile that does not route tool permission requests to Branchyard (Antigravity,
    /// Pi, Amp); its tools run under the harness's own configuration
    #[arg(long)]
    allow_unapproved_tools: bool,
}

impl Perms {
    pub(crate) fn apply(self, task: &mut TaskArgs) {
        task.permissions = match (self.yes, self.ask, self.permissions) {
            (true, _, _) => Permissions::Yes,
            (false, true, _) => Permissions::Ask,
            (false, false, Some(preset)) => Permissions::Preset(preset),
            (false, false, None) => Permissions::Unset,
        };
        task.unapproved_tools = self.allow_unapproved_tools;
    }
}

/// `--no-preflight`, for the commands that start a harness turn in this
/// repository (`run`, `fan`, `send`).
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[command(next_help_heading = "Launch")]
pub struct Preflight {
    /// Skip the check, before any branch is created, that the harness is installed and not
    /// verified logged out (it is not made for --command or a routed run)
    #[arg(long)]
    no_preflight: bool,
}

impl Preflight {
    fn apply(self, task: &mut TaskArgs) {
        task.no_preflight = self.no_preflight;
    }
}

/// Where and how the harness is launched.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Launch")]
pub struct Launch {
    /// Private HOME, not a sandbox; the harness is then usually not logged in
    ///
    /// A private HOME at .branchyard/homes/BRANCH, without the ANTHROPIC*, CLAUDE*, OPENAI*
    /// and CODEX* variables. It is not a sandbox: the harness still runs as your user and can
    /// read your files and reach the network (--provider chooses a sandbox; see
    /// docs/modes.md). --isolated=false turns off [defaults] isolated = true for this run.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    isolated: Option<bool>,
    /// Launch this instead of the profile's executable, for development and testing
    #[arg(long, value_name = "CMD", value_parser = command_argv)]
    command: Option<Argv>,
    /// Where the harness runs: local, microsandbox, substrate or recipe:NAME (default: local,
    /// or the branch's own)
    #[arg(long, value_name = "PROVIDER", value_parser = provider_arg)]
    provider: Option<ProviderArg>,
    /// Variables to copy into the sandbox, such as API keys; nothing else is
    #[arg(long, value_name = "NAME,NAME,...", value_parser = variable_names)]
    pass_env: Option<List>,
    #[command(flatten)]
    microsandbox: MicrosandboxFlags,
    #[command(flatten)]
    substrate: SubstrateFlags,
    #[command(flatten)]
    recipe: RecipeFlags,
    #[command(flatten)]
    lifecycle: LifecycleFlags,
    /// With --remote: only a worker carrying this label runs it (repeatable)
    #[arg(long = "require-label", value_name = "LABEL")]
    require_label: Vec<String>,
    /// With --remote: the operation's priority, -10 to 10 (default 0, or a spawned child's
    /// parent's); higher runs first
    #[arg(
        long,
        value_name = "N",
        allow_negative_numbers = true,
        value_parser = clap::value_parser!(i32).range(-10..=10)
    )]
    priority: Option<i32>,
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Recipe provider (--provider recipe:NAME)")]
pub struct RecipeFlags {
    /// Where the worktree is copied on the machine (default:
    /// /tmp/branchyard/<branch>-<hash>/workspace)
    #[arg(long, value_name = "PATH")]
    recipe_workdir: Option<String>,
    /// The harness's HOME on the machine (default: beside the workdir)
    #[arg(long, value_name = "PATH")]
    recipe_home: Option<String>,
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(
    next_help_heading = "Sandbox lifecycle (--provider microsandbox, substrate or recipe:NAME)"
)]
pub struct LifecycleFlags {
    /// Between turns: destroy the sandbox (default) or pause it for the next turn
    #[arg(long, value_name = "WHAT", value_parser = keep_sandbox)]
    keep_sandbox: Option<branchyard::SandboxKeep>,
    /// Checkpoints that also keep a provider snapshot, with a kept sandbox (default 3)
    #[arg(long, value_name = "N")]
    sandbox_snapshots: Option<u32>,
    /// Kept sandboxes of this provider in the repository before the least recently used
    /// is destroyed (default 4)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    max_paused: Option<u32>,
    /// Minutes a kept sandbox may go unused before it is destroyed instead of resumed
    /// (default: never)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    max_kept_minutes: Option<u32>,
}

impl LifecycleFlags {
    fn first_given(&self) -> Option<&'static str> {
        [
            ("keep-sandbox", self.keep_sandbox.is_some()),
            ("sandbox-snapshots", self.sandbox_snapshots.is_some()),
            ("max-paused", self.max_paused.is_some()),
            ("max-kept-minutes", self.max_kept_minutes.is_some()),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }

    fn args(&self) -> LifecycleArgs {
        LifecycleArgs {
            keep: self.keep_sandbox,
            snapshots: self.sandbox_snapshots,
            max_paused: self.max_paused,
            max_kept_minutes: self.max_kept_minutes,
        }
    }
}

pub(crate) fn keep_sandbox(value: &str) -> Result<branchyard::SandboxKeep, String> {
    match value {
        "pause" => Ok(branchyard::SandboxKeep::Pause),
        "destroy" => Ok(branchyard::SandboxKeep::Destroy),
        other => Err(format!("{other:?} is not pause or destroy")),
    }
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Microsandbox provider (--provider microsandbox)")]
pub struct MicrosandboxFlags {
    /// OCI image with the harness installed
    #[arg(long, value_name = "REF")]
    image: Option<String>,
    /// Virtual CPUs for the sandbox
    #[arg(long, value_name = "N", value_parser = cpus)]
    cpus: Option<u8>,
    /// Sandbox memory in MiB
    #[arg(long, value_name = "MIB", value_parser = memory)]
    memory: Option<u32>,
    /// Use the SDK's pause, live branching and full snapshots (unqualified; needs KVM)
    #[arg(long)]
    live_branch: bool,
}

#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Substrate provider (--provider substrate)")]
pub struct SubstrateFlags {
    /// Agent Substrate Control API, https://HOST:PORT
    #[arg(long, value_name = "URL")]
    substrate_endpoint: Option<String>,
    /// Router base URL; requests identify the actor with its routing header
    #[arg(long, value_name = "URL")]
    substrate_router: Option<String>,
    /// Actor template that runs branchyard-bridge and the harness
    #[arg(long, value_name = "NAME")]
    substrate_template: Option<String>,
    /// Bridge signing key from `branchyard-bridge keygen`
    #[arg(long, value_name = "FILE")]
    substrate_key: Option<String>,
    /// File holding the Control bearer token, reread for every request
    #[arg(long, value_name = "FILE")]
    substrate_api_token_file: Option<String>,
    /// Verified TLS service name for Control, including port-forwards
    #[arg(long, value_name = "NAME")]
    substrate_server_name: Option<String>,
    /// Actor egress policy (default: deny); use --substrate-allow-host for an allowlist
    #[arg(long, value_parser = ["deny", "unrestricted"], conflicts_with = "substrate_allow_host")]
    substrate_egress: Option<String>,
    /// DNS host the actor may reach; repeat for more hosts, *. matches one label
    #[arg(long, value_name = "HOST")]
    substrate_allow_host: Vec<String>,
    /// Atespace for the actors (default: default)
    #[arg(long, value_name = "NAME")]
    substrate_atespace: Option<String>,
    /// Where the worktree is copied in the actor (default: /workspace)
    #[arg(long, value_name = "PATH")]
    substrate_workdir: Option<String>,
    /// The harness's HOME in the actor (default: /branchyard/home)
    #[arg(long, value_name = "PATH")]
    substrate_home: Option<String>,
    /// PEM authorities for the TLS Control API, and the router
    #[arg(long, value_name = "FILE")]
    substrate_ca: Option<String>,
    /// PEM client certificate for the Control API, mutual TLS
    #[arg(long, value_name = "FILE")]
    substrate_client_cert: Option<String>,
    /// PEM key of --substrate-client-cert
    #[arg(long, value_name = "FILE")]
    substrate_client_key: Option<String>,
    /// PEM authorities for the TLS router, if not --substrate-ca
    #[arg(long, value_name = "FILE")]
    substrate_router_ca: Option<String>,
    /// Allow http:// or ws:// to hosts other than loopback
    #[arg(long)]
    substrate_insecure: bool,
}

impl SubstrateFlags {
    /// The first substrate flag given, for "needs --provider substrate".
    fn first_given(&self) -> Option<&'static str> {
        [
            ("substrate-endpoint", self.substrate_endpoint.is_some()),
            ("substrate-router", self.substrate_router.is_some()),
            ("substrate-template", self.substrate_template.is_some()),
            ("substrate-key", self.substrate_key.is_some()),
            (
                "substrate-api-token-file",
                self.substrate_api_token_file.is_some(),
            ),
            (
                "substrate-server-name",
                self.substrate_server_name.is_some(),
            ),
            ("substrate-egress", self.substrate_egress.is_some()),
            (
                "substrate-allow-host",
                !self.substrate_allow_host.is_empty(),
            ),
            ("substrate-atespace", self.substrate_atespace.is_some()),
            ("substrate-workdir", self.substrate_workdir.is_some()),
            ("substrate-home", self.substrate_home.is_some()),
            ("substrate-ca", self.substrate_ca.is_some()),
            (
                "substrate-client-cert",
                self.substrate_client_cert.is_some(),
            ),
            ("substrate-client-key", self.substrate_client_key.is_some()),
            ("substrate-router-ca", self.substrate_router_ca.is_some()),
            ("substrate-insecure", self.substrate_insecure),
        ]
        .into_iter()
        .find_map(|(flag, given)| given.then_some(flag))
    }
}

impl Launch {
    fn apply(self, task: &mut TaskArgs) -> Result<(), String> {
        task.isolated = self.isolated == Some(true);
        task.isolation_off = self.isolated == Some(false);
        task.require_labels = self.require_label.clone();
        task.priority = self.priority;
        task.command = self.command.map(|argv| argv.0);
        let chosen = self.provider;
        let micro = &self.microsandbox;
        let recipe = matches!(chosen, Some(ProviderArg::Recipe(_)));
        if !recipe {
            let given = [
                ("recipe-workdir", self.recipe.recipe_workdir.is_some()),
                ("recipe-home", self.recipe.recipe_home.is_some()),
            ]
            .into_iter()
            .find_map(|(flag, given)| given.then_some(flag));
            if let Some(flag) = given {
                return Err(format!("--{flag} needs --provider recipe:NAME"));
            }
        } else if self.lifecycle.sandbox_snapshots.is_some() {
            return Err(
                "--sandbox-snapshots does not apply to a recipe's machine, which has no \
                 snapshots"
                    .into(),
            );
        }
        if chosen != Some(ProviderArg::Microsandbox) {
            let given = [
                ("image", micro.image.is_some()),
                ("cpus", micro.cpus.is_some()),
                ("memory", micro.memory.is_some()),
                ("live-branch", micro.live_branch),
            ]
            .into_iter()
            .find_map(|(flag, given)| given.then_some(flag));
            if let Some(flag) = given {
                return Err(format!("--{flag} needs --provider microsandbox"));
            }
        }
        if chosen != Some(ProviderArg::Substrate) {
            if let Some(flag) = self.substrate.first_given() {
                return Err(format!("--{flag} needs --provider substrate"));
            }
        }
        if matches!(chosen, None | Some(ProviderArg::Local)) && self.pass_env.is_some() {
            return Err(
                "--pass-env needs --provider microsandbox, substrate or recipe:NAME".into(),
            );
        }
        if matches!(chosen, None | Some(ProviderArg::Local)) {
            if let Some(flag) = self.lifecycle.first_given() {
                return Err(format!(
                    "--{flag} needs --provider microsandbox, substrate or recipe:NAME"
                ));
            }
        }
        let lifecycle = self.lifecycle.args();
        let pass_env = self.pass_env.map(|list| list.0).unwrap_or_default();
        match chosen {
            None => {}
            Some(ProviderArg::Local) => task.local = true,
            Some(ProviderArg::Recipe(name)) => {
                let path = |value: Option<String>, flag: &str| match value {
                    Some(path) if !path.starts_with('/') => Err(format!(
                        "--{flag} is a path on the recipe's machine and must be absolute, not \
                         {path:?}"
                    )),
                    other => Ok(other),
                };
                task.recipe = Some(RecipeArgs {
                    name,
                    workdir: path(self.recipe.recipe_workdir, "recipe-workdir")?,
                    home: path(self.recipe.recipe_home, "recipe-home")?,
                    pass_env,
                    lifecycle,
                });
            }
            Some(ProviderArg::Microsandbox) => {
                let image = self
                    .microsandbox
                    .image
                    .filter(|image| !image.trim().is_empty())
                    .ok_or("--provider microsandbox needs --image")?;
                task.sandbox = Some(SandboxArgs {
                    image,
                    cpus: self.microsandbox.cpus,
                    memory_mib: self.microsandbox.memory,
                    pass_env,
                    lifecycle,
                    live_branch: self.microsandbox.live_branch,
                });
            }
            Some(ProviderArg::Substrate) => {
                let s = self.substrate;
                let required = |value: Option<String>, flag: &str| {
                    value
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| format!("--provider substrate needs --{flag}"))
                };
                task.substrate = Some(SubstrateArgs {
                    endpoint: required(s.substrate_endpoint, "substrate-endpoint")?,
                    router: required(s.substrate_router, "substrate-router")?,
                    template: required(s.substrate_template, "substrate-template")?,
                    key: required(s.substrate_key, "substrate-key")?,
                    api_token_file: s.substrate_api_token_file,
                    server_name: s.substrate_server_name,
                    egress: if !s.substrate_allow_host.is_empty() {
                        branchyard::config::SubstrateEgress::AllowHosts(s.substrate_allow_host)
                    } else if s.substrate_egress.as_deref() == Some("unrestricted") {
                        branchyard::config::SubstrateEgress::Unrestricted
                    } else {
                        branchyard::config::SubstrateEgress::Deny
                    },
                    atespace: s.substrate_atespace,
                    workdir: s.substrate_workdir,
                    home: s.substrate_home,
                    pass_env,
                    ca: s.substrate_ca,
                    client_cert: s.substrate_client_cert,
                    client_key: s.substrate_client_key,
                    router_ca: s.substrate_router_ca,
                    insecure: s.substrate_insecure,
                    lifecycle,
                });
            }
        }
        Ok(())
    }
}

/// Letting the harness delegate.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Delegation")]
pub struct Delegation {
    /// Let the harness create child branches through Branchyard's MCP tools, DEPTH levels
    /// deep (default 1)
    #[arg(
        long,
        value_name = "DEPTH",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "1",
        value_parser = delegate_depth
    )]
    delegate: Option<u32>,
    /// Allow the harness's own by commands that act as its branch (spawn, inspect, send,
    /// wait, discard, artifact, ask, ...; see docs/delegation.md) without asking; nothing else
    #[arg(long)]
    allow_delegation: bool,
    /// Do not start the harness's next turn on its own when its turn ends while its children
    /// run and they then settle
    #[arg(long)]
    no_wake: bool,
}

impl Delegation {
    fn apply(self, task: &mut TaskArgs) {
        task.delegate = self.delegate;
        task.allow_delegation = self.allow_delegation;
        task.no_wake = self.no_wake;
    }
}

/// Provisioning the harness's private home.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Provisioning")]
pub struct Provision {
    /// A credential for the harness, such as ANTHROPIC_API_KEY, from the variable of that name,
    /// VAR, or FILE; written only into its private home (--isolated or a sandbox). Repeatable
    #[arg(long = "secret", value_name = "NAME[=VAR|=@FILE]", value_parser = branchyard::SecretSource::parse)]
    secrets: Vec<branchyard::SecretSource>,
    /// The authentication method when the secrets allow several: api-key, auth-token,
    /// oauth-token, auth-file, vertex-ai
    #[arg(long, value_name = "METHOD")]
    auth: Option<String>,
    /// An MCP server for the harness: NAME=COMMAND starts a stdio server, COMMAND an absolute
    /// path with its arguments; NAME=https://URL connects to a streamable HTTP server where the
    /// harness can (Claude Code, ACP agents that advertise it). Repeatable
    #[arg(long = "mcp", value_name = "NAME=COMMAND|NAME=URL", value_parser = mcp_server)]
    mcp_servers: Vec<McpArg>,
    /// A header for an HTTP --mcp server, its value read each turn from the variable VAR or the
    /// file @FILE and never stored, as a --secret is (so it needs --isolated or a sandbox), such
    /// as 'search:Authorization=@/run/search-auth'. Repeatable
    #[arg(long = "mcp-header", value_name = "NAME:HEADER=VAR|@FILE", value_parser = mcp_header)]
    mcp_headers: Vec<McpHeader>,
    /// Standing instructions for the harness, read from FILE
    #[arg(long, value_name = "FILE")]
    instructions: Option<String>,
    /// The model, or a size alias (small, medium, large, extra-large) where the harness
    /// defines one
    #[arg(long, value_name = "NAME", value_parser = non_blank)]
    model: Option<String>,
    /// Reasoning effort: low, medium, high, xhigh, or 0-100
    #[arg(long, value_name = "LEVEL", value_parser = branchyard::Effort::parse)]
    effort: Option<branchyard::Effort>,
    /// Send the harness's OpenTelemetry to this OTLP/gRPC collector, or turn it off
    #[arg(long, value_name = "URL|off", value_parser = branchyard::Telemetry::parse)]
    telemetry: Option<branchyard::Telemetry>,
    /// A connector the harness may call through the gateway (docs/connectors.md):
    /// CONNECTOR[@ACCOUNT][:read|write|write+confirm[:OP,OP...]], such as github:read or
    /// 'github:write:issues.*'; only into its private home (--isolated or a sandbox). Repeatable
    #[arg(long = "connector", value_name = "GRANT", value_parser = branchyard::connectors::GrantEntry::parse)]
    connectors: Vec<branchyard::connectors::GrantEntry>,
    /// The hosts the harness may reach (docs/egress.md): open, none, or rules
    /// '[METHOD|METHOD ]HOST[:PORT][/PATH]' separated by commas, such as
    /// 'github.com,*.npmjs.org:443' or 'GET|HEAD api.github.com/repos/*' (a rule naming a
    /// method or path is held to inside the connection)
    #[arg(long, value_name = "open|none|HOSTS", value_parser = branchyard::Network::parse_flag)]
    network: Option<branchyard::Network>,
    /// Refuse to run where the network policy cannot be enforced (required), or run with the
    /// proxy's variables only and say so (best-effort, the default)
    #[arg(long, value_name = "MODE", value_parser = branchyard::NetworkEnforce::parse, requires = "network")]
    network_enforce: Option<branchyard::NetworkEnforce>,
    /// Call the harness's models through Branchyard's model gateway (docs/model-gateway.md),
    /// which holds the provider's key, meters every call and holds the budgets; =MODELS limits
    /// it to these models, globs separated by commas, such as 'claude-sonnet-*'
    #[arg(
        long,
        value_name = "MODELS",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "*",
        value_parser = branchyard::models::ModelAccess::parse_flag
    )]
    model_gateway: Option<branchyard::models::ModelAccess>,
}

impl Provision {
    fn apply(self, task: &mut TaskArgs) -> Result<(), String> {
        let mut secrets = self.secrets;
        let mut mcp_servers = Vec::new();
        let mut remote_mcp_servers = Vec::new();
        for server in self.mcp_servers {
            match server {
                McpArg::Stdio(spec) => mcp_servers.push(spec),
                McpArg::Http(spec) => remote_mcp_servers.push(spec),
            }
        }
        for header in self.mcp_headers {
            let server = remote_mcp_servers
                .iter_mut()
                .find(|s: &&mut branchyard::RemoteMcpSpec| s.name == header.server)
                .ok_or_else(|| {
                    format!(
                        "--mcp-header names {}, which no --mcp {}=https://... gives",
                        header.server, header.server
                    )
                })?;
            server
                .headers
                .insert(header.header.clone(), header.secret.name.clone());
            if !secrets.iter().any(|s| s.name == header.secret.name) {
                secrets.push(header.secret);
            }
        }
        let spec = branchyard::Provisioning {
            secrets,
            mcp_servers,
            remote_mcp_servers,
            auth: self.auth,
            model: self.model,
            effort: self.effort,
            telemetry: self.telemetry,
            connectors: self.connectors,
            network: self
                .network
                .map(|n| n.with_enforce(self.network_enforce.unwrap_or_default())),
            models: self.model_gateway,
            ..branchyard::Provisioning::default()
        };
        task.provision = (!spec.is_empty() || self.instructions.is_some()).then_some(spec);
        task.instructions = self.instructions;
        Ok(())
    }
}

/// One `--mcp`: a stdio server the harness starts, or an HTTP one it
/// connects to.
#[derive(Clone, Debug, PartialEq)]
pub enum McpArg {
    Stdio(branchyard::McpServerSpec),
    Http(branchyard::RemoteMcpSpec),
}

/// `--mcp NAME=COMMAND` or `--mcp NAME=https://URL`.
pub(crate) fn mcp_server(text: &str) -> Result<McpArg, String> {
    let (name, rest) = text
        .split_once('=')
        .ok_or_else(|| format!("an MCP server is NAME=COMMAND or NAME=URL, not {text:?}"))?;
    let url = rest.trim();
    if url.starts_with("https://") || url.starts_with("http://") {
        let spec = branchyard::RemoteMcpSpec {
            name: name.to_owned(),
            transport: branchyard::RemoteMcpTransport::Http,
            url: url.to_owned(),
            headers: Default::default(),
        };
        spec.check()?;
        return Ok(McpArg::Http(spec));
    }
    branchyard::McpServerSpec::parse(text).map(McpArg::Stdio)
}

/// One `--mcp-header NAME:HEADER=VAR|@FILE`: the header and the secret
/// that holds its value.
#[derive(Clone, Debug, PartialEq)]
pub struct McpHeader {
    server: String,
    header: String,
    secret: branchyard::SecretSource,
}

pub(crate) fn mcp_header(text: &str) -> Result<McpHeader, String> {
    let shape = || format!("an MCP header is NAME:HEADER=VAR or NAME:HEADER=@FILE, not {text:?}");
    let (server, rest) = text.split_once(':').ok_or_else(shape)?;
    let (header, source) = rest.split_once('=').ok_or_else(shape)?;
    if server.is_empty() || header.is_empty() || source.is_empty() {
        return Err(shape());
    }
    // A file's value gets a secret named for the server and header; a
    // variable's is the secret of that name, as `--secret VAR` is.
    let secret = match source.strip_prefix('@') {
        Some(_) => {
            let name: String = format!("MCP_{server}_{header}")
                .chars()
                .map(|c| match c.is_ascii_alphanumeric() {
                    true => c.to_ascii_uppercase(),
                    false => '_',
                })
                .collect();
            branchyard::SecretSource::parse(&format!("{name}={source}"))?
        }
        None => branchyard::SecretSource::parse(source)?,
    };
    Ok(McpHeader {
        server: server.to_owned(),
        header: header.to_owned(),
        secret,
    })
}

/// `--detach` and its `--json`, for `by run` and `by fork` (`by send` has
/// its own `--json`); see `crate::detach`.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Detached {
    /// Return as soon as the turn has started, leaving it running in the background; its
    /// output goes to a log under .branchyard/detached/ (local mode)
    #[arg(long, conflicts_with = "ask")]
    pub detach: bool,
    /// Print JSON: the branch as `by show --json` prints it once the turn has ended (activity
    /// goes to standard error); with --detach, the branch and the log's path
    #[arg(long)]
    pub json: bool,
}

/// Structured results shared by root tasks and delegated children.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct ResultFlags {
    /// JSON Schema for the structured result
    /// using only type, properties, required, additionalProperties, items, enum, const, minimum,
    /// maximum, exclusiveMinimum, exclusiveMaximum, minLength, maxLength, pattern, minItems,
    /// maxItems, uniqueItems, anyOf, oneOf and annotations such as title and description;
    /// anything else is refused. Read accepted results with inspect, wait or compare
    #[arg(long, value_name = "JSON", value_parser = non_blank, conflicts_with = "result_schema_file", help_heading = "Result")]
    result_schema: Option<String>,
    /// Read the result schema from this file, or - for standard input
    #[arg(long, value_name = "PATH", value_parser = non_blank, help_heading = "Result")]
    result_schema_file: Option<String>,
    /// Require a structured result matching the schema
    #[arg(long, help_heading = "Result")]
    result_required: bool,
    /// Accept the native final JSON answer as the deliverable; no Git change is needed
    #[arg(long, help_heading = "Result")]
    result_deliverable: bool,
    /// Collect this worktree-relative output manifest after the provider returns files
    #[arg(long, value_name = "PATH", value_parser = non_blank, requires = "result_deliverable", help_heading = "Result")]
    result_artifacts: Option<String>,
    /// Maximum captured files (default: 32; at most 1024)
    #[arg(
        long,
        value_name = "COUNT",
        requires = "result_artifacts",
        help_heading = "Result"
    )]
    result_artifacts_max_files: Option<u32>,
    /// Maximum total captured bytes (default: 268435456)
    #[arg(
        long,
        value_name = "BYTES",
        requires = "result_artifacts",
        help_heading = "Result"
    )]
    result_artifacts_max_bytes: Option<u64>,
}

impl ResultFlags {
    fn capture(&self) -> Option<branchyard::ResultCapture> {
        self.result_artifacts.as_ref().map(|manifest| {
            let mut capture = branchyard::ResultCapture::new(manifest.clone());
            if let Some(max_files) = self.result_artifacts_max_files {
                capture.max_files = max_files;
            }
            if let Some(max_bytes) = self.result_artifacts_max_bytes {
                capture.max_bytes = max_bytes;
            }
            capture
        })
    }
    fn validate(&self) -> Result<(), String> {
        if (self.result_required || self.result_deliverable)
            && self.result_schema.is_none()
            && self.result_schema_file.is_none()
        {
            let flag = if self.result_deliverable {
                "--result-deliverable"
            } else {
                "--result-required"
            };
            return Err(format!(
                "{flag} needs the schema it applies to: --result-schema or --result-schema-file"
            ));
        }
        Ok(())
    }

    fn apply(self, task: &mut TaskArgs) -> Result<(), String> {
        self.validate()?;
        task.result_capture = self.capture();
        task.result_schema = self.result_schema;
        task.result_schema_file = self.result_schema_file;
        task.result_required = self.result_required;
        task.result_deliverable = self.result_deliverable;
        Ok(())
    }
}

/// `by run`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct RunFlags {
    /// Harness or profile ID (default: claude-code, or routed when there is a [fleet])
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    #[command(flatten)]
    route: RouteFlags,
    /// Take the task from this issue: GitHub's (URL, #N or N, through gh), linear:KEY, jira:KEY,
    /// gitlab:GROUP/PROJECT#N, or a Linear, Jira or GitLab URL; a prompt, if given, is added
    #[arg(long, value_name = "REF", value_parser = non_blank)]
    issue: Option<String>,
    /// Start from GitHub pull request N's head (fetched with gh and git); by pr then updates it
    #[arg(long, value_name = "N", conflicts_with_all = ["issue", "base"])]
    pr: Option<u64>,
    /// Branch name (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    /// Base revision (default: HEAD)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    /// Tools the harness is denied outright, before any permission answer, --yes included; a
    /// trailing * matches a prefix. Stored with the branch: later sends and every child it
    /// delegates to keep them, as with by spawn --deny. Repeatable
    #[arg(
        long,
        value_name = "TOOL,TOOL,...",
        value_parser = harness_list,
        help_heading = "Permissions"
    )]
    deny: Vec<List>,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    preflight: Preflight,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
    #[command(flatten)]
    plan_goal: PlanGoal,
    #[command(flatten)]
    result: ResultFlags,
}

impl Flags for RunFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        if self.route.auto && self.harness.is_some() {
            return Err("--auto routes through the [fleet] table; it takes no --harness".into());
        }
        let mut task = TaskArgs {
            harness: self.harness,
            name: self.name,
            base: self.base,
            issue: self.issue,
            pr: self.pr,
            auto: self.route.auto,
            kind: self.route.kind,
            seed: self.route.seed,
            ..TaskArgs::default()
        };
        self.limits.apply(&mut task);
        self.perms.apply(&mut task);
        task.deny = self.deny.into_iter().flat_map(|list| list.0).collect();
        self.launch.apply(&mut task)?;
        self.preflight.apply(&mut task);
        self.delegation.apply(&mut task);
        self.provision.apply(&mut task)?;
        self.plan_goal.apply(&mut task);
        self.result.apply(&mut task)?;
        Ok(task)
    }
}

/// Plan approval and goals, for `run` and `fan`; see docs/plans-and-goals.md.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Plan and goal")]
pub struct PlanGoal {
    /// Plan first: the first turn runs read-only and proposes a plan, and the branch waits for
    /// `by plan approve` (or reject) before anything changes
    #[arg(long)]
    plan: bool,
    /// A goal a judge must find evidence of before the branch is done; unmet, it gets follow-up
    /// turns with what is missing
    #[arg(long, value_name = "TEXT", value_parser = non_blank)]
    goal: Option<String>,
    /// Follow-up turns at most for an unmet goal (default 2), within the budget
    #[arg(long, value_name = "N", requires = "goal", value_parser = clap::value_parser!(u32).range(0..=20))]
    goal_rounds: Option<u32>,
    /// The goal's judge harness, read-only on a scratch branch (default: the [fleet] entry's
    /// goal_judge; without one, the branch's check and a non-empty diff decide)
    #[arg(long, value_name = "ID", requires = "goal")]
    goal_judge: Option<String>,
    /// Launch the goal judge with this instead of its executable, for development and testing
    #[arg(long, value_name = "CMD", value_parser = command_argv, requires = "goal_judge")]
    goal_judge_command: Option<Argv>,
}

impl PlanGoal {
    fn apply(self, task: &mut TaskArgs) {
        task.plan = self.plan;
        task.goal = self.goal;
        task.goal_rounds = self.goal_rounds;
        task.goal_judge = self.goal_judge;
        task.goal_judge_command = self.goal_judge_command.map(|a| a.0);
    }
}

/// Routing through the fleet table, for `run` and `fan`; see docs/fleet.md.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Routing")]
pub struct RouteFlags {
    /// Pick the harness from the [fleet] table in branchyard.toml, learning from outcomes, and
    /// fail over to the next candidate when a harness fails
    #[arg(long)]
    auto: bool,
    /// The task's kind (default: inferred from the prompt): bugfix, feature, refactor, review,
    /// research, docs, migration, tests, other
    #[arg(long, value_name = "KIND", value_parser = task_kind)]
    kind: Option<branchyard::TaskKind>,
    /// Seed the router, for a reproducible pick
    #[arg(long, value_name = "N")]
    seed: Option<u64>,
}

pub(crate) fn task_kind(text: &str) -> Result<branchyard::TaskKind, String> {
    text.parse().map_err(|e: branchyard::Error| match e {
        branchyard::Error::Unsupported(why) => why,
        other => other.to_string(),
    })
}

/// `by fan`'s options: `run`'s, with `--harness` a list, taken by the
/// command itself.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct FanFlags {
    /// Run in a private task without project files, even outside a Git repository
    #[arg(long, conflicts_with_all = ["base", "pr"])]
    no_files: bool,
    /// Branch name prefix (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    #[command(flatten)]
    route: RouteFlags,
    /// Take the task from this issue: GitHub's (URL, #N or N, through gh), linear:KEY, jira:KEY,
    /// gitlab:GROUP/PROJECT#N, or a Linear, Jira or GitLab URL; a prompt, if given, is added
    #[arg(long, value_name = "REF", value_parser = non_blank)]
    issue: Option<String>,
    /// Start from GitHub pull request N's head (fetched with gh and git); by pr then updates it
    #[arg(long, value_name = "N", conflicts_with_all = ["issue", "base"])]
    pr: Option<u64>,
    /// Base revision (default: HEAD)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    preflight: Preflight,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
    #[command(flatten)]
    plan_goal: PlanGoal,
    #[command(flatten)]
    result: ResultFlags,
}

impl Flags for FanFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        let no_files = self.no_files;
        let mut task = RunFlags {
            harness: None,
            route: self.route,
            issue: self.issue,
            pr: self.pr,
            name: self.name,
            base: self.base,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: self.preflight,
            delegation: self.delegation,
            provision: self.provision,
            plan_goal: self.plan_goal,
            result: self.result,
            deny: Vec::new(),
        }
        .check()?;
        task.no_files = no_files;
        Ok(task)
    }
}

/// `by task`, as its own struct and boxed: [`Command`]'s parser builds every
/// command's arguments in one function, whose debug-build stack frame is
/// near a test thread's limit, so this one adds only a call.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskCommand {
    /// Print JSON
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub action: TaskAction,
}

/// `by task`'s actions (docs/task-repos.md), each with its own flags.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum TaskAction {
    /// Start a task and run its first attempt
    ///
    /// Start a task and run its first attempt: in this repository, in a folder you grant
    /// (--folder), or with no files (--no-files)
    New(Box<TaskNew>),
    /// List tasks: this repository's, then those with a repository of their own
    Ls,
    /// Show a task: what was asked, its attempts and their conversations
    Show(TaskShow),
    /// Open an attempt's worktree in your editor (or print its path)
    Open(TaskOpen),
    /// Reset an attempt, its files and its conversation, to one of its checkpoints
    Rewind(TaskRewind),
    /// Start another attempt from an attempt's candidate, or from its checkpoint N
    Fork(Box<TaskFork>),
    /// Accept an attempt: merge it here, or apply it to the task's folder
    Accept(TaskAccept),
    /// Remove a task and its attempts (never the folder you granted)
    Rm(TaskRm),
}

/// `by task new`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskNew {
    /// What to do; quote it
    pub prompt: String,
    /// A folder to work on (not a git repository): attempts run in their own worktrees and
    /// the folder changes only when you accept one
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath, conflicts_with = "no_files")]
    pub folder: Option<std::path::PathBuf>,
    /// A task with no files: its conversation and results are its repository
    #[arg(long)]
    pub no_files: bool,
    /// With --folder or --no-files: files of at least this many bytes are stored as chunks
    /// (default 1048576)
    #[arg(long, value_name = "BYTES")]
    pub large_threshold: Option<u64>,
    #[command(flatten)]
    pub task: Checked<RunFlags>,
}

/// `by task show`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskShow {
    /// The task's ID (or a prefix of at least 4 characters) or one of its attempts
    pub task: String,
}

/// `by task open`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskOpen {
    /// The task, by ID or by one of its branches
    pub task: String,
    /// The attempt (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// The editor: code, cursor, zed, nvim, ... or a command line (default: $VISUAL, then
    /// $EDITOR)
    #[arg(long, value_name = "EDITOR", conflicts_with = "print")]
    pub editor: Option<String>,
    /// Print the worktree's path instead of opening it
    #[arg(long)]
    pub print: bool,
}

/// `by task rewind`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskRewind {
    /// The task whose attempt to reset, by ID or by one of its branches
    pub task: String,
    /// The attempt (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// The checkpoint: a turn number, or 0 for the attempt's base
    #[arg(long, value_name = "N")]
    pub to: u32,
    /// Confirm the reset without a prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// `by task fork`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskFork {
    /// The task to fork, by ID or by one of its branches
    pub task: String,
    /// The new attempt's prompt; quote it
    pub prompt: String,
    /// The attempt to fork (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// Fork from checkpoint N (0 is its base) instead of its candidate
    #[arg(long, value_name = "N")]
    pub at: Option<u32>,
    #[command(flatten)]
    pub flags: Checked<ForkFlags>,
}

/// `by task accept`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskAccept {
    /// The task whose attempt to accept, by ID or by one of its branches
    pub task: String,
    /// The attempt (default: the only one)
    #[arg(long, value_name = "BRANCH")]
    pub attempt: Option<String>,
    /// In a repository: the branch to merge into (default: the current branch)
    #[arg(long, value_name = "TARGET")]
    pub into: Option<String>,
}

/// `by task rm`.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct TaskRm {
    /// A branch, or a task ID
    pub task: String,
    /// Confirm the removal without a prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// `by map`'s actions besides running one.
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum MapAction {
    /// Run a recorded map again with its command line and items, skipping the items done
    Resume {
        /// The map to run again
        name: String,
        /// Run the items that failed again too
        #[arg(long)]
        retry_failed: bool,
    },
    /// The recorded maps and their progress
    Ls,
    /// A map's progress, its items' results and its reduce
    Show {
        /// The map's name
        name: String,
    },
    /// Forget a map's record (its branches stay; remove them with by rm)
    Rm {
        /// The map to forget
        name: String,
    },
}

/// `by map`'s run: the prompt template, where the items come from, the
/// answer's schema, the results, and the branches' options.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct MapArgs {
    /// The prompt template: {{item}}, {{item.FIELD}} (a path such as {{item.a.0.b}}), {{id}},
    /// {{index}} and {{map}} are replaced for each item
    #[arg(required = true)]
    pub prompt: Option<String>,
    /// Read the items from this file (- for standard input): JSON lines, a JSON array, CSV with a
    /// header, or one item per line, by its extension (default: standard input)
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub items: Option<String>,
    /// Read the items from this shell command's output, run in the repository as you
    #[arg(long, value_name = "CMD", conflicts_with = "items", value_parser = non_blank)]
    pub from_command: Option<String>,
    /// How the items are written: jsonl, json, csv or lines (default: the file's extension, else
    /// guessed from the first character: [ for json, { for jsonl, else lines)
    #[arg(long, value_name = "FORMAT", value_parser = item_format)]
    pub input_format: Option<branchyard::ItemFormat>,
    /// Each branch must answer with JSON matching this JSON Schema (a subset; see docs/map.md);
    /// without one, an item's result is its last reply's text
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub schema: Option<String>,
    /// Write the results here as each item ends: CSV for a .csv file, else JSON lines
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    pub out: Option<String>,
    /// Branches running at once (default 4)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=64))]
    pub concurrency: Option<u32>,
    /// New branches an item gets after its first fails (default 1)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=10))]
    pub retries: Option<u32>,
    /// Stop starting items once the map has cost X dollars, across runs (per branch, use
    /// --budget-usd)
    #[arg(long, value_name = "DOLLARS", value_parser = usd)]
    pub total_usd: Option<f64>,
    /// Then one more branch, given every result, answers this prompt: the map's summary
    #[arg(long, value_name = "PROMPT", value_parser = non_blank)]
    pub reduce: Option<String>,
    /// Write the reduce's answer to this file too
    #[arg(long, value_name = "FILE", requires = "reduce", value_hint = ValueHint::FilePath)]
    pub reduce_out: Option<String>,
    /// Remove an item's branches once its answer is recorded (failed items' branches are kept)
    #[arg(long)]
    pub rm: bool,
    /// Run the items that failed in an earlier run again
    #[arg(long)]
    pub retry_failed: bool,
    #[command(flatten)]
    pub task: Checked<MapFlags>,
}

pub(crate) fn item_format(text: &str) -> Result<branchyard::ItemFormat, String> {
    text.parse().map_err(|e: branchyard::Error| match e {
        branchyard::Error::Unsupported(why) => why,
        other => other.to_string(),
    })
}

/// `by map`'s branch options: `run`'s, without an issue, a pull request,
/// delegation, a plan or a goal.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct MapFlags {
    /// Harness or profile ID (default: claude-code, or routed when there is a [fleet])
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    /// The map's name, which its branches' names start with (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    /// Base revision of every branch (default: HEAD)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    #[command(flatten)]
    route: RouteFlags,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    provision: Provision,
}

impl Flags for MapFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: self.harness,
            route: self.route,
            issue: None,
            pr: None,
            name: self.name,
            base: self.base,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: Preflight::default(),
            delegation: Delegation::default(),
            provision: self.provision,
            plan_goal: PlanGoal::default(),
            result: ResultFlags::default(),
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by send`'s task options: the branch keeps its harness, workspace and
/// provider.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct SendFlags {
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    /// Launch this instead of the profile's executable, for development and testing
    #[arg(long, value_name = "CMD", value_parser = command_argv, help_heading = "Launch")]
    command: Option<Argv>,
    #[command(flatten)]
    preflight: Preflight,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
    /// With --remote: only a worker carrying this label runs it (repeatable)
    #[arg(long = "require-label", value_name = "LABEL", help_heading = "Launch")]
    require_label: Vec<String>,
    /// With --remote: the operation's priority, -10 to 10 (default 0); higher runs first
    #[arg(
        long,
        value_name = "N",
        allow_negative_numbers = true,
        value_parser = clap::value_parser!(i32).range(-10..=10),
        help_heading = "Launch"
    )]
    priority: Option<i32>,
}

impl Flags for SendFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        let mut task = TaskArgs {
            command: self.command.map(|argv| argv.0),
            require_labels: self.require_label,
            priority: self.priority,
            ..TaskArgs::default()
        };
        self.limits.apply(&mut task);
        self.perms.apply(&mut task);
        self.preflight.apply(&mut task);
        self.delegation.apply(&mut task);
        self.provision.apply(&mut task)?;
        Ok(task)
    }
}

/// Operator sends distinguish session edits from provisioning replacements.
/// Other commands sharing [`SendFlags`] retain their replacement semantics.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct SendSessionFlags {
    #[command(flatten)]
    send: SendFlags,
}

impl Flags for SendSessionFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        let mut task = self.send.check()?;
        if let Some(spec) = &task.provision {
            let mut rest = spec.clone();
            rest.model = None;
            rest.effort = None;
            if rest.is_empty() && task.instructions.is_none() {
                task.session_patch = Some(branchyard::SessionPatch {
                    model: spec.model.clone(),
                    effort: spec.effort,
                });
                task.provision = None;
            }
        }
        Ok(task)
    }
}

/// `by fork`'s task options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct ForkFlags {
    /// The new branch's name (default: derived from the branch's)
    #[arg(short, long)]
    name: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
}

impl Flags for ForkFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: None,
            route: RouteFlags::default(),
            issue: None,
            pr: None,
            name: self.name,
            base: None,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: Preflight::default(),
            delegation: self.delegation,
            provision: self.provision,
            plan_goal: PlanGoal::default(),
            result: ResultFlags::default(),
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by reincarnate`'s task options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct ReincarnateFlags {
    /// The new branch's name (default: derived from the branch's)
    #[arg(short, long)]
    name: Option<String>,
    /// Harness or profile ID (default: the branch's)
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    perms: Perms,
    #[command(flatten)]
    launch: Launch,
    #[command(flatten)]
    delegation: Delegation,
    #[command(flatten)]
    provision: Provision,
}

impl Flags for ReincarnateFlags {
    type Output = TaskArgs;
    fn check(self) -> Result<TaskArgs, String> {
        RunFlags {
            harness: self.harness,
            route: RouteFlags::default(),
            issue: None,
            pr: None,
            name: self.name,
            base: None,
            limits: self.limits,
            perms: self.perms,
            launch: self.launch,
            preflight: Preflight::default(),
            delegation: self.delegation,
            provision: self.provision,
            plan_goal: PlanGoal::default(),
            result: ResultFlags::default(),
            deny: Vec::new(),
        }
        .check()
    }
}

/// `by spawn`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct SpawnFlags {
    /// The delegating branch (outside a harness; inside one, it is the harness's own)
    #[arg(long, value_name = "BRANCH")]
    parent: Option<String>,
    /// In a rig, the seat the child fills; it sets the child's harness, limits, check and
    /// instructions
    #[arg(long, value_name = "NAME")]
    seat: Option<String>,
    /// Harness or profile ID, such as claude-code or claude-code-acp (default: the parent's own
    /// profile); shown as both, `claude-code (claude-code-stream-json)`
    #[arg(long, value_name = "ID")]
    harness: Option<String>,
    /// The child's model, or a size alias (small, medium, large, extra-large) where its harness
    /// defines one; a cheaper one suits mechanical work (default: its seat's, else the parent's).
    /// Refused for a harness whose driver cannot choose one
    #[arg(long, value_name = "NAME", value_parser = non_blank)]
    model: Option<String>,
    /// Take the task from this issue: GitHub's (URL, #N or N, through gh), linear:KEY, jira:KEY,
    /// gitlab:GROUP/PROJECT#N, or a Linear, Jira or GitLab URL; a prompt, if given, is added
    #[arg(long, value_name = "REF", value_parser = non_blank)]
    issue: Option<String>,
    /// Branch name (default: a slug of the prompt)
    #[arg(short, long)]
    name: Option<String>,
    /// Base revision (default: the parent's candidate)
    #[arg(short, long, value_name = "REV")]
    base: Option<String>,
    /// Wait for the turn to end and show it (outside a harness, spawn always waits)
    #[arg(long)]
    wait: bool,
    /// Print JSON
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    limits: Limits,
    #[command(flatten)]
    graph: SpawnGraph,
    #[command(flatten)]
    perms: Perms,
}

/// Where a spawned child sits among its siblings.
#[derive(Args, Clone, Debug, Default, PartialEq)]
#[command(next_help_heading = "Delegation")]
pub struct SpawnGraph {
    /// Levels the child may delegate below itself (default: one fewer than the parent)
    #[arg(long, value_name = "N")]
    max_depth: Option<u32>,
    /// Children the child may have at once (default and most: the parent's)
    #[arg(long, value_name = "N")]
    max_children: Option<u32>,
    /// Harness or profile IDs the child may delegate to, each allowed to the parent (default:
    /// the parent's)
    #[arg(long, value_name = "ID,ID,...", value_parser = harness_list)]
    harnesses: Option<List>,
    /// Tools the child is denied outright; a trailing * matches a prefix. Repeatable
    #[arg(long, value_name = "TOOL,TOOL,...", value_parser = harness_list)]
    deny: Vec<List>,
    /// Siblings the child waits for: it is created waiting and starts once they have settled
    #[arg(long, value_name = "BRANCH,BRANCH,...", value_parser = harness_list)]
    depends_on: Option<List>,
    /// When each --depends-on branch counts as done
    #[arg(long, value_name = "WHEN")]
    after: Option<AfterArg>,
    /// Bind the child to a scratch area, ACCESS read_only or exclusive_write (which holds its
    /// writer lock for each turn); repeatable
    #[arg(long = "bind", value_name = "SCRATCH:ACCESS", value_parser = branchyard::Binding::parse)]
    bindings: Vec<branchyard::Binding>,
    /// A connector the child may use, within its parent's grant (default: its seat's or its
    /// parent's): CONNECTOR[@ACCOUNT][:read|write|write+confirm[:OP,OP...]]. Repeatable
    #[arg(long = "connector", value_name = "GRANT", value_parser = branchyard::connectors::GrantEntry::parse)]
    connectors: Vec<branchyard::connectors::GrantEntry>,
    /// Plan first: the child's first turn is read-only and its plan is escalated to the
    /// parent's inbox; it changes nothing until `by plan approve`
    #[arg(long)]
    plan: bool,
    /// Why this harness and model for this subtask, in a few words: recorded on the child, shown
    /// by `by log`, and counted by `by fleet stats` beside how the child turned out
    #[arg(long, value_name = "TEXT", value_parser = non_blank)]
    why: Option<String>,
    #[command(flatten)]
    result: ResultFlags,
    /// A path the child owns: a file, a directory (and everything below it) or a glob such as
    /// tests/parser_*.rs, relative to the repository. Its changes anywhere else are refused when
    /// it is integrated, and it is told them every turn. Repeatable, or comma-separated
    #[arg(long, value_name = "PATH,PATH,...", value_parser = harness_list)]
    owns: Vec<List>,
    /// Leave the child to the engine to land: once its turn ends ready while you wait on your
    /// children, it is integrated into your branch, its check on the merge included, and a child
    /// that cannot land is sent a turn to fix its work; your next turn says what landed
    #[arg(long)]
    land: bool,
    /// Have a reviewer on this harness, ideally another vendor's than the child's, read its work
    /// each time it is ready to land, before it lands; findings are sent to the child as a turn
    /// to fix its work. Needs --land, and a harness you may delegate to
    #[arg(long, value_name = "HARNESS", requires = "land", value_parser = non_blank)]
    review: Option<String>,
    /// The model the reviewer runs
    #[arg(long, value_name = "MODEL", requires = "review", value_parser = non_blank)]
    review_model: Option<String>,
    /// What the reviewer should look for, added to its prompt
    #[arg(long, value_name = "TEXT", requires = "review", value_parser = non_blank)]
    review_rubric: Option<String>,
    /// Each review's cost limit, from your budget; required when you have a cost limit
    #[arg(long, value_name = "DOLLARS", requires = "review", value_parser = usd)]
    review_budget_usd: Option<f64>,
}

impl Flags for SpawnFlags {
    type Output = SpawnArgs;
    fn check(self) -> Result<SpawnArgs, String> {
        self.graph.result.validate()?;
        let result_capture = self.graph.result.capture();
        let mut task = TaskArgs {
            harness: self.harness,
            name: self.name,
            base: self.base,
            issue: self.issue,
            ..TaskArgs::default()
        };
        self.limits.apply(&mut task);
        self.perms.apply(&mut task);
        Ok(SpawnArgs {
            task,
            parent: self.parent,
            wait: self.wait,
            max_depth: self.graph.max_depth,
            max_children: self.graph.max_children,
            harnesses: self.graph.harnesses.map(|list| list.0),
            deny: self
                .graph
                .deny
                .into_iter()
                .flat_map(|list| list.0)
                .collect(),
            seat: self.seat,
            depends_on: self.graph.depends_on.map(|list| list.0).unwrap_or_default(),
            after: self.graph.after.map(Into::into).unwrap_or_default(),
            bindings: self.graph.bindings,
            connectors: self.graph.connectors,
            plan: self.graph.plan,
            why: self.graph.why,
            model: self.model,
            result_schema: self.graph.result.result_schema,
            result_schema_file: self.graph.result.result_schema_file,
            result_required: self.graph.result.result_required,
            result_deliverable: self.graph.result.result_deliverable,
            result_capture,
            owns: self
                .graph
                .owns
                .into_iter()
                .flat_map(|list| list.0)
                .collect(),
            land: self.graph.land,
            review: self.graph.review.map(|harness| {
                Box::new(branchyard::ReviewSpec {
                    harness,
                    model: self.graph.review_model,
                    rubric: self.graph.review_rubric,
                    budget_usd: self.graph.review_budget_usd,
                })
            }),
            json: self.json,
        })
    }
}

/// `by pr`, checked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrArgs {
    /// The git remote to push to.
    pub git_remote: String,
    /// The branch on the remote (default: the branch's git branch).
    pub head: Option<String>,
    pub base: Option<String>,
    pub title: Option<String>,
    pub draft: bool,
    /// `-R OWNER/REPO` for gh, when it cannot tell from the remotes.
    pub gh_repo: Option<String>,
    pub force: bool,
    pub no_check: bool,
    pub allow_failing_check: bool,
    pub allow_not_ready: bool,
    pub watch: bool,
    /// First poll interval of `--watch`; it backs off to ten times this.
    pub interval: Duration,
    /// `--max-rounds`: stop after delivering feedback this many times.
    pub max_rounds: Option<u32>,
    /// `--no-resolve`: leave review threads the watch fed back unresolved
    /// after a push addresses them.
    pub no_resolve: bool,
    /// Limits and permissions for the turns `--watch` starts.
    pub task: TaskArgs,
    pub json: bool,
}

/// `by pr`'s options.
#[derive(Args, Clone, Debug, Default, PartialEq)]
pub struct PrFlags {
    /// The git remote to push to (not --remote, which names a Branchyard server)
    #[arg(long, value_name = "NAME", default_value = "origin", value_parser = non_blank)]
    git_remote: String,
    /// The branch to push to on the remote (default: the branch's git branch, by/<name>)
    #[arg(long, value_name = "BRANCH", value_parser = non_blank)]
    head: Option<String>,
    /// The pull request's base branch (default: the repository's default branch)
    #[arg(long, value_name = "BRANCH", value_parser = non_blank)]
    base: Option<String>,
    /// The pull request's title (default: the issue's title, or the prompt's first line); on
    /// an update, only when given
    #[arg(long, value_parser = non_blank)]
    title: Option<String>,
    /// Open the pull request as a draft (when creating it)
    #[arg(long)]
    draft: bool,
    /// The GitHub repository, when gh cannot tell it from the git remotes
    #[arg(long, value_name = "OWNER/REPO", value_parser = non_blank)]
    gh_repo: Option<String>,
    /// Replace the remote branch even if the candidate does not descend from it
    #[arg(long)]
    force: bool,
    /// Push without running the branch's check on its candidate
    #[arg(long, help_heading = "Readiness")]
    no_check: bool,
    /// Push even though the branch's check fails on its candidate
    #[arg(long, help_heading = "Readiness", conflicts_with = "no_check")]
    allow_failing_check: bool,
    /// Push a branch that is not ready (failed, interrupted, at a limit) if it has a candidate
    #[arg(long, help_heading = "Readiness")]
    allow_not_ready: bool,
    /// Then follow the pull request: send failed CI checks and new review comments into the
    /// branch, and push again after each turn, until it is merged or closed
    #[arg(long, help_heading = "Watching")]
    watch: bool,
    /// With --watch: the first time between polls, backing off to ten times it
    #[arg(
        long,
        value_name = "SECONDS",
        default_value = "30",
        value_parser = watch_interval,
        requires = "watch",
        help_heading = "Watching"
    )]
    interval: Duration,
    /// With --watch: stop after delivering feedback N times
    #[arg(
        long,
        value_name = "N",
        value_parser = positive_turns,
        requires = "watch",
        help_heading = "Watching"
    )]
    max_rounds: Option<u32>,
    /// With --watch: do not reply "Addressed in <commit>" to, and resolve, the review threads it
    /// fed back once a pushed commit changes their files
    #[arg(long, requires = "watch", help_heading = "Watching")]
    no_resolve: bool,
    /// Print JSON
    #[arg(long, conflicts_with = "watch")]
    json: bool,
    #[command(flatten)]
    perms: Perms,
    /// Launch this instead of the profile's executable for --watch's turns, for development and
    /// testing
    #[arg(long, value_name = "CMD", value_parser = command_argv, help_heading = "Launch")]
    command: Option<Argv>,
}

impl Flags for PrFlags {
    type Output = PrArgs;
    fn check(self) -> Result<PrArgs, String> {
        let mut task = TaskArgs {
            command: self.command.map(|argv| argv.0),
            ..TaskArgs::default()
        };
        if !self.watch && (self.perms != Perms::default() || task.command.is_some()) {
            return Err(
                "permissions and --command apply to the turns --watch starts; add --watch".into(),
            );
        }
        self.perms.apply(&mut task);
        Ok(PrArgs {
            git_remote: self.git_remote,
            head: self.head,
            base: self.base,
            title: self.title,
            draft: self.draft,
            gh_repo: self.gh_repo,
            force: self.force,
            no_check: self.no_check,
            allow_failing_check: self.allow_failing_check,
            allow_not_ready: self.allow_not_ready,
            watch: self.watch,
            interval: self.interval,
            max_rounds: self.max_rounds,
            no_resolve: self.no_resolve,
            task,
            json: self.json,
        })
    }
}
