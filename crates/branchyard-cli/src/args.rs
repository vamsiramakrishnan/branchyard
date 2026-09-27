//! Command-line parsing for `by`.
//!
//! Hand-written to avoid a dependency. Each command declares its positionals
//! and flags once in [`COMMANDS`]; parsing and help text both read from it.

use std::fmt;
use std::time::Duration;

/// How tool permission requests are answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Permissions {
    /// `--yes`: allow every request.
    Yes,
    /// `--ask`: prompt on the terminal.
    Ask,
    /// Neither flag: decided by whether a terminal is attached.
    #[default]
    Unset,
}

/// Options shared by `run`, `fan`, `send` and `fork`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskArgs {
    pub harness: Option<String>,
    pub name: Option<String>,
    pub base: Option<String>,
    /// Check argument vector, split from one quoted string.
    pub check: Option<Vec<String>>,
    pub budget_usd: Option<f64>,
    pub max_turns: Option<u32>,
    /// From `--max-minutes`.
    pub max_duration: Option<Duration>,
    pub permissions: Permissions,
    pub isolated: bool,
    /// Executable and fixed arguments replacing the profile's.
    pub command: Option<Vec<String>>,
    /// `--provider microsandbox` and its options; `None` keeps the default.
    pub sandbox: Option<SandboxArgs>,
    /// `--provider substrate` and its options.
    pub substrate: Option<SubstrateArgs>,
    /// `--provider local`.
    pub local: bool,
    /// From `--delegate[=DEPTH]`: levels of children the harness may create.
    pub delegate: Option<u32>,
    /// `--allow-delegation`: auto-allow the harness's own `by` delegation
    /// commands.
    pub allow_delegation: bool,
    /// `--allow-unapproved-tools`: run a profile whose tools Branchyard's
    /// policy never sees.
    pub unapproved_tools: bool,
    /// `--secret`, `--auth`, `--mcp`, `--model`, `--effort` and
    /// `--telemetry`; `None` when none was given.
    pub provision: Option<branchyard::Provisioning>,
    /// `--instructions FILE`, read when the command runs.
    pub instructions: Option<String>,
}

/// Options for `--provider microsandbox`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SandboxArgs {
    pub image: String,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub pass_env: Vec<String>,
}

/// Options for `--provider substrate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubstrateArgs {
    pub endpoint: String,
    pub router: String,
    pub template: String,
    pub key: String,
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
}

/// Which `--provider` was chosen, with its options.
#[derive(Clone, Debug, PartialEq)]
enum Chosen {
    Local,
    Microsandbox(SandboxArgs),
    Substrate(Box<SubstrateArgs>),
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
    pub deny: Vec<String>,
    /// `--seat`: the rig seat the child fills.
    pub seat: Option<String>,
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

/// `by artifact publish|list|get|share ...`; see `docs/storage.md`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactArgs {
    pub action: String,
    /// `publish`'s file, or `get`/`share`'s id.
    pub arg: Option<String>,
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

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Run {
        prompt: String,
        task: TaskArgs,
    },
    Fan {
        prompt: String,
        harnesses: Vec<String>,
        task: TaskArgs,
    },
    Send {
        branch: String,
        prompt: String,
        task: TaskArgs,
        /// `--steer`: into the running turn rather than a new one.
        steer: bool,
        json: bool,
    },
    Fork {
        branch: String,
        prompt: String,
        fresh_session: bool,
        task: TaskArgs,
    },
    Ls {
        json: bool,
    },
    Show {
        branch: String,
        json: bool,
    },
    Diff {
        branch: String,
    },
    Log {
        branch: String,
        json: bool,
        follow: bool,
    },
    Merge {
        branch: String,
        into: Option<String>,
    },
    Rm {
        branch: String,
        keep_credentials: bool,
    },
    Harnesses {
        json: bool,
    },
    Watch {
        /// Seconds between refreshes.
        interval: Duration,
        /// Print one frame and exit.
        once: bool,
    },
    /// `by serve`: arguments for the server's own parser.
    Serve {
        args: Vec<String>,
    },
    /// `by mcp`: the arguments for Branchyard's MCP server.
    Mcp {
        args: Vec<String>,
    },
    Spawn {
        prompt: String,
        spawn: SpawnArgs,
    },
    Inspect {
        branch: Option<String>,
        json: bool,
    },
    Events {
        branch: Option<String>,
        cursor: Option<usize>,
        limit: Option<usize>,
        json: bool,
    },
    Integrate {
        branch: String,
        json: bool,
    },
    Cancel {
        branch: String,
        json: bool,
    },
    Children {
        branch: Option<String>,
        json: bool,
    },
    Rig(RigArgs),
    Artifact(ArtifactArgs),
    Scratch(ScratchArgs),
    /// General help, or one command's.
    Help {
        topic: Option<&'static Spec>,
    },
    Version,
}

/// A usage error: exit code 2.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageError {
    pub message: String,
    /// The command whose help would explain the mistake.
    pub command: Option<&'static str>,
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Options before the command, choosing where commands run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Globals {
    /// Server URL: run against a Branchyard server instead of locally.
    pub remote: Option<String>,
    pub token_file: Option<String>,
    /// Repository name on the server.
    pub repo: Option<String>,
    /// Extra CA certificates for `https`.
    pub ca_file: Option<String>,
}

impl Globals {
    /// Fill what the command line left unset from `BRANCHYARD_REMOTE`,
    /// `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO` and `BRANCHYARD_CA_FILE`.
    pub fn with_env(mut self, get: impl Fn(&str) -> Option<String>) -> Globals {
        let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        self.remote = self.remote.or_else(|| get("BRANCHYARD_REMOTE"));
        self.token_file = self.token_file.or_else(|| get("BRANCHYARD_TOKEN_FILE"));
        self.repo = self.repo.or_else(|| get("BRANCHYARD_REPO"));
        self.ca_file = self.ca_file.or_else(|| get("BRANCHYARD_CA_FILE"));
        self
    }
}

const GLOBALS: [(&str, &str); 4] = [
    ("remote", "URL"),
    ("token-file", "FILE"),
    ("repo", "NAME"),
    ("ca-file", "FILE"),
];

/// Split leading global options from the command's arguments.
pub fn parse_globals(args: &[String]) -> Result<(Globals, &[String]), UsageError> {
    let mut globals = Globals::default();
    let mut rest = args;
    while let Some(arg) = rest.first() {
        let Some(long) = arg.strip_prefix("--") else {
            break;
        };
        let (name, inline) = match long.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (long, None),
        };
        let Some((name, placeholder)) = GLOBALS.iter().find(|(n, _)| *n == name) else {
            break;
        };
        let error = |message: String| UsageError {
            message,
            command: None,
        };
        let (value, used) = match inline {
            Some(value) => (value, 1),
            None => match rest.get(1) {
                Some(value) => (value.clone(), 2),
                None => return Err(error(format!("--{name} needs a value {placeholder}"))),
            },
        };
        if value.is_empty() {
            return Err(error(format!("--{name} needs a value {placeholder}")));
        }
        let slot = match *name {
            "remote" => &mut globals.remote,
            "token-file" => &mut globals.token_file,
            "repo" => &mut globals.repo,
            _ => &mut globals.ca_file,
        };
        if slot.is_some() {
            return Err(error(format!("--{name} given twice")));
        }
        *slot = Some(value);
        rest = &rest[used..];
    }
    Ok((globals, rest))
}

#[derive(Debug, PartialEq)]
pub struct Flag {
    pub long: &'static str,
    /// Value placeholder; `None` for a switch. A placeholder in brackets,
    /// such as `[=DEPTH]`, is an optional value given only as `--flag=value`.
    pub value: Option<&'static str>,
    pub help: &'static str,
}

#[derive(Debug, PartialEq)]
pub struct Spec {
    pub name: &'static str,
    pub positionals: &'static [&'static str],
    pub summary: &'static str,
    pub flags: &'static [Flag],
}

impl Spec {
    pub fn usage(&self) -> String {
        let mut usage = format!("by {}", self.name);
        for positional in self.positionals {
            match positional.strip_suffix('?') {
                Some(optional) => usage.push_str(&format!(" [<{optional}>]")),
                None => usage.push_str(&format!(" <{positional}>")),
            }
        }
        if !self.flags.is_empty() {
            usage.push_str(" [options]");
        }
        usage
    }
}

const HARNESS: Flag = Flag {
    long: "harness",
    value: Some("ID"),
    help: "Harness or profile ID (default: claude-code)",
};
const HARNESS_LIST: Flag = Flag {
    long: "harness",
    value: Some("ID,ID,..."),
    help: "Harnesses to run on, one branch each (required)",
};
const NAME: Flag = Flag {
    long: "name",
    value: Some("NAME"),
    help: "Branch name (default: a slug of the prompt)",
};
const BASE: Flag = Flag {
    long: "base",
    value: Some("REV"),
    help: "Base revision (default: HEAD)",
};
const CHECK: Flag = Flag {
    long: "check",
    value: Some("\"CMD ARGS\""),
    help: "Check to pass before merging; split like a shell, run without one",
};
const BUDGET_USD: Flag = Flag {
    long: "budget-usd",
    value: Some("X"),
    help: "Stop once the harness's own cost estimate exceeds X dollars",
};
const MAX_TURNS: Flag = Flag {
    long: "max-turns",
    value: Some("N"),
    help: "Stop after N turns",
};
const MAX_MINUTES: Flag = Flag {
    long: "max-minutes",
    value: Some("N"),
    help: "Interrupt the turn after N minutes",
};
const ISOLATED: Flag = Flag {
    long: "isolated",
    value: None,
    help: "Scrubbed environment and a private HOME; the harness is then usually not logged in",
};
const COMMAND: Flag = Flag {
    long: "command",
    value: Some("\"PATH ARGS\""),
    help: "Launch this instead of the profile's executable, for development and testing",
};
const PROVIDER: Flag = Flag {
    long: "provider",
    value: Some("local|microsandbox|substrate"),
    help: "Where the harness runs (default: local, or the branch's own)",
};
const IMAGE: Flag = Flag {
    long: "image",
    value: Some("REF"),
    help: "OCI image with the harness installed (microsandbox)",
};
const CPUS: Flag = Flag {
    long: "cpus",
    value: Some("N"),
    help: "Virtual CPUs for the sandbox (microsandbox)",
};
const MEMORY: Flag = Flag {
    long: "memory",
    value: Some("MIB"),
    help: "Sandbox memory in MiB (microsandbox)",
};
const PASS_ENV: Flag = Flag {
    long: "pass-env",
    value: Some("NAME,NAME,..."),
    help: "Variables to copy into the sandbox, such as API keys; nothing else is",
};
const SUBSTRATE_ENDPOINT: Flag = Flag {
    long: "substrate-endpoint",
    value: Some("URL"),
    help: "Agent Substrate Control API, https://HOST:PORT (substrate)",
};
const SUBSTRATE_ROUTER: Flag = Flag {
    long: "substrate-router",
    value: Some("URL"),
    help: "Router URL of an actor's bridge, with {atespace} and {actor} (substrate)",
};
const SUBSTRATE_TEMPLATE: Flag = Flag {
    long: "substrate-template",
    value: Some("NAME"),
    help: "Actor template that runs branchyard-bridge and the harness (substrate)",
};
const SUBSTRATE_KEY: Flag = Flag {
    long: "substrate-key",
    value: Some("FILE"),
    help: "Bridge signing key from `branchyard-bridge keygen` (substrate)",
};
const SUBSTRATE_ATESPACE: Flag = Flag {
    long: "substrate-atespace",
    value: Some("NAME"),
    help: "Atespace for the actors (substrate; default: default)",
};
const SUBSTRATE_WORKDIR: Flag = Flag {
    long: "substrate-workdir",
    value: Some("PATH"),
    help: "Where the worktree is copied in the actor (substrate; default: /workspace)",
};
const SUBSTRATE_HOME: Flag = Flag {
    long: "substrate-home",
    value: Some("PATH"),
    help: "The harness's HOME in the actor (substrate; default: /branchyard/home)",
};
const SUBSTRATE_CA: Flag = Flag {
    long: "substrate-ca",
    value: Some("FILE"),
    help: "PEM authorities for the TLS Control API, and the router (substrate)",
};
const SUBSTRATE_CLIENT_CERT: Flag = Flag {
    long: "substrate-client-cert",
    value: Some("FILE"),
    help: "PEM client certificate for the Control API, mutual TLS (substrate)",
};
const SUBSTRATE_CLIENT_KEY: Flag = Flag {
    long: "substrate-client-key",
    value: Some("FILE"),
    help: "PEM key of --substrate-client-cert (substrate)",
};
const SUBSTRATE_ROUTER_CA: Flag = Flag {
    long: "substrate-router-ca",
    value: Some("FILE"),
    help: "PEM authorities for the TLS router, if not --substrate-ca (substrate)",
};
const SUBSTRATE_INSECURE: Flag = Flag {
    long: "substrate-insecure",
    value: None,
    help: "Allow http:// or ws:// to hosts other than loopback (substrate)",
};
const YES: Flag = Flag {
    long: "yes",
    value: None,
    help: "Allow every tool permission request",
};
const ASK: Flag = Flag {
    long: "ask",
    value: None,
    help: "Ask on the terminal for each tool permission request",
};
const FRESH_SESSION: Flag = Flag {
    long: "fresh-session",
    value: None,
    help: "Start a new session if the harness cannot fork its conversation",
};
const STEER: Flag = Flag {
    long: "steer",
    value: None,
    help: "Add the prompt to the branch's running turn without interrupting it, instead of \
           starting a new turn; refused when no turn runs or the harness cannot take it",
};
const JSON: Flag = Flag {
    long: "json",
    value: None,
    help: "Print JSON",
};
const FOLLOW: Flag = Flag {
    long: "follow",
    value: None,
    help: "Keep printing events as they are recorded, until interrupted; with --json, one object per line",
};
const INTERVAL: Flag = Flag {
    long: "interval",
    value: Some("SECS"),
    help: "Seconds between refreshes (default: 1)",
};
const ONCE: Flag = Flag {
    long: "once",
    value: None,
    help: "Print the tree once and exit",
};
const DELEGATE: Flag = Flag {
    long: "delegate",
    value: Some("[=DEPTH]"),
    help: "Let the harness create child branches through Branchyard's MCP tools, DEPTH levels deep (default 1)",
};
const ROOT: Flag = Flag {
    long: "root",
    value: Some("DIR"),
    help: "Repository root",
};
const BRANCH: Flag = Flag {
    long: "branch",
    value: Some("NAME"),
    help: "The branch whose turn this server serves",
};
const ALLOW_UNAPPROVED_TOOLS: Flag = Flag {
    long: "allow-unapproved-tools",
    value: None,
    help: "Run a profile that does not route tool permission requests to Branchyard (Antigravity, Pi, Amp); its tools run under the harness's own configuration",
};
const ALLOW_DELEGATION: Flag = Flag {
    long: "allow-delegation",
    value: None,
    help: "Allow the harness's own `by spawn|inspect|events|send|integrate|cancel|children` commands without asking; nothing else",
};
const PARENT: Flag = Flag {
    long: "parent",
    value: Some("BRANCH"),
    help: "The delegating branch (outside a harness; inside one, it is the harness's own)",
};
const WAIT: Flag = Flag {
    long: "wait",
    value: None,
    help: "Wait for the child's turn to end and show it (outside a harness, spawn always waits)",
};
const MAX_DEPTH: Flag = Flag {
    long: "max-depth",
    value: Some("N"),
    help: "Levels the child may delegate below itself (default: one fewer than the parent)",
};
const DENY: Flag = Flag {
    long: "deny",
    value: Some("TOOL,TOOL,..."),
    help: "Tools the child is denied outright; a trailing * matches a prefix",
};
const SEAT: Flag = Flag {
    long: "seat",
    value: Some("NAME"),
    help: "In a rig, the seat the child fills; it sets the child's harness, limits, check and instructions",
};
const CURSOR: Flag = Flag {
    long: "cursor",
    value: Some("N"),
    help: "Start at event N (default: the most recent)",
};
const LIMIT: Flag = Flag {
    long: "limit",
    value: Some("N"),
    help: "At most N events (default 50, at most 200)",
};
const KEEP_CREDENTIALS: Flag = Flag {
    long: "keep-credentials",
    value: None,
    help: "Keep the credential files provisioning wrote in a home a fork still uses",
};
const ACT_AS: Flag = Flag {
    long: "branch",
    value: Some("NAME"),
    help: "Act as this branch (outside a harness; inside one, it is the harness's own)",
};
const ARTIFACT_LABEL: Flag = Flag {
    long: "label",
    value: Some("KEY=VALUE"),
    help: "A label on the published artifact; repeatable",
};
const ARTIFACT_OUT: Flag = Flag {
    long: "out",
    value: Some("PATH"),
    help: "Where to write the artifact's bytes",
};
const ARTIFACT_TO: Flag = Flag {
    long: "to",
    value: Some("BRANCH"),
    help: "The branch to share with",
};
const MEDIA_TYPE: Flag = Flag {
    long: "media-type",
    value: Some("TYPE"),
    help: "The artifact's media type (default: application/octet-stream)",
};
const INTO: Flag = Flag {
    long: "into",
    value: Some("TARGET"),
    help: "Local branch to merge into (default: the current branch)",
};

const SECRET: Flag = Flag {
    long: "secret",
    value: Some("NAME[=VAR|=@FILE]"),
    help: "A credential for the harness, such as ANTHROPIC_API_KEY, from the variable of that name, VAR, or FILE; written only into its private home (--isolated or a sandbox). Repeatable",
};
const AUTH: Flag = Flag {
    long: "auth",
    value: Some("METHOD"),
    help: "The authentication method when the secrets allow several: api-key, oauth-token, auth-file, vertex-ai",
};
const MCP: Flag = Flag {
    long: "mcp",
    value: Some("NAME=COMMAND"),
    help: "A stdio MCP server for the harness, COMMAND an absolute path with its arguments. Repeatable",
};
const INSTRUCTIONS: Flag = Flag {
    long: "instructions",
    value: Some("FILE"),
    help: "Standing instructions for the harness, read from FILE",
};
const MODEL: Flag = Flag {
    long: "model",
    value: Some("NAME"),
    help: "The model, or a size alias (small, medium, large, extra-large) where the harness defines one",
};
const EFFORT: Flag = Flag {
    long: "effort",
    value: Some("LEVEL"),
    help: "Reasoning effort: low, medium, high, xhigh, or 0-100",
};
const TELEMETRY: Flag = Flag {
    long: "telemetry",
    value: Some("URL|off"),
    help: "Send the harness's OpenTelemetry to this OTLP/gRPC collector, or turn it off",
};

/// Flags that may be given more than once.
const REPEATABLE: &[&str] = &["secret", "mcp", "label"];

pub static COMMANDS: &[Spec] = &[
    Spec {
        name: "run",
        positionals: &["prompt"],
        summary: "Run a task on a new branch",
        flags: &[
            HARNESS,
            NAME,
            BASE,
            CHECK,
            BUDGET_USD,
            MAX_TURNS,
            MAX_MINUTES,
            YES,
            ASK,
            ISOLATED,
            COMMAND,
            PROVIDER,
            IMAGE,
            CPUS,
            MEMORY,
            PASS_ENV,
            SUBSTRATE_ENDPOINT,
            SUBSTRATE_ROUTER,
            SUBSTRATE_TEMPLATE,
            SUBSTRATE_KEY,
            SUBSTRATE_ATESPACE,
            SUBSTRATE_WORKDIR,
            SUBSTRATE_HOME,
            SUBSTRATE_CA,
            SUBSTRATE_CLIENT_CERT,
            SUBSTRATE_CLIENT_KEY,
            SUBSTRATE_ROUTER_CA,
            SUBSTRATE_INSECURE,
            DELEGATE,
            ALLOW_DELEGATION,
            ALLOW_UNAPPROVED_TOOLS,
            SECRET,
            AUTH,
            MCP,
            INSTRUCTIONS,
            MODEL,
            EFFORT,
            TELEMETRY,
        ],
    },
    Spec {
        name: "fan",
        positionals: &["prompt"],
        summary: "Run a task on several harnesses in parallel, then compare",
        flags: &[
            HARNESS_LIST,
            NAME,
            BASE,
            CHECK,
            BUDGET_USD,
            MAX_TURNS,
            MAX_MINUTES,
            YES,
            ASK,
            ISOLATED,
            COMMAND,
            PROVIDER,
            IMAGE,
            CPUS,
            MEMORY,
            PASS_ENV,
            SUBSTRATE_ENDPOINT,
            SUBSTRATE_ROUTER,
            SUBSTRATE_TEMPLATE,
            SUBSTRATE_KEY,
            SUBSTRATE_ATESPACE,
            SUBSTRATE_WORKDIR,
            SUBSTRATE_HOME,
            SUBSTRATE_CA,
            SUBSTRATE_CLIENT_CERT,
            SUBSTRATE_CLIENT_KEY,
            SUBSTRATE_ROUTER_CA,
            SUBSTRATE_INSECURE,
            DELEGATE,
            ALLOW_DELEGATION,
            ALLOW_UNAPPROVED_TOOLS,
            SECRET,
            AUTH,
            MCP,
            INSTRUCTIONS,
            MODEL,
            EFFORT,
            TELEMETRY,
        ],
    },
    Spec {
        name: "send",
        positionals: &["branch", "prompt"],
        summary: "Continue a branch's session with another prompt",
        flags: &[
            STEER,
            CHECK,
            BUDGET_USD,
            MAX_TURNS,
            MAX_MINUTES,
            YES,
            ASK,
            COMMAND,
            DELEGATE,
            ALLOW_DELEGATION,
            ALLOW_UNAPPROVED_TOOLS,
            SECRET,
            AUTH,
            MCP,
            INSTRUCTIONS,
            MODEL,
            EFFORT,
            TELEMETRY,
            JSON,
        ],
    },
    Spec {
        name: "fork",
        positionals: &["branch", "prompt"],
        summary: "Start a new branch from a branch's candidate and conversation",
        flags: &[
            NAME,
            FRESH_SESSION,
            CHECK,
            BUDGET_USD,
            MAX_TURNS,
            MAX_MINUTES,
            YES,
            ASK,
            ISOLATED,
            COMMAND,
            PROVIDER,
            IMAGE,
            CPUS,
            MEMORY,
            PASS_ENV,
            SUBSTRATE_ENDPOINT,
            SUBSTRATE_ROUTER,
            SUBSTRATE_TEMPLATE,
            SUBSTRATE_KEY,
            SUBSTRATE_ATESPACE,
            SUBSTRATE_WORKDIR,
            SUBSTRATE_HOME,
            SUBSTRATE_CA,
            SUBSTRATE_CLIENT_CERT,
            SUBSTRATE_CLIENT_KEY,
            SUBSTRATE_ROUTER_CA,
            SUBSTRATE_INSECURE,
            DELEGATE,
            ALLOW_DELEGATION,
            ALLOW_UNAPPROVED_TOOLS,
            SECRET,
            AUTH,
            MCP,
            INSTRUCTIONS,
            MODEL,
            EFFORT,
            TELEMETRY,
        ],
    },
    Spec {
        name: "ls",
        positionals: &[],
        summary: "List branches",
        flags: &[JSON],
    },
    Spec {
        name: "show",
        positionals: &["branch"],
        summary: "Show one branch",
        flags: &[JSON],
    },
    Spec {
        name: "diff",
        positionals: &["branch"],
        summary: "Show a branch's candidate diff against its base",
        flags: &[],
    },
    Spec {
        name: "log",
        positionals: &["branch"],
        summary: "Show a branch's recorded events",
        flags: &[JSON, FOLLOW],
    },
    Spec {
        name: "merge",
        positionals: &["branch"],
        summary: "Merge a branch's candidate after its check passes",
        flags: &[INTO],
    },
    Spec {
        name: "rm",
        positionals: &["branch"],
        summary: "Remove a branch's worktree and record",
        flags: &[KEEP_CREDENTIALS],
    },
    Spec {
        name: "harnesses",
        positionals: &[],
        summary: "List harness profiles and whether they are installed",
        flags: &[JSON],
    },
    Spec {
        name: "watch",
        positionals: &[],
        summary: "Watch every branch live: status, activity, cost",
        flags: &[INTERVAL, ONCE],
    },
    Spec {
        name: "serve",
        positionals: &[],
        summary: "Serve repositories over an authenticated HTTP API",
        flags: &[],
    },
    Spec {
        name: "spawn",
        positionals: &["prompt"],
        summary: "Delegate to a new child branch of this branch",
        flags: &[
            PARENT,
            SEAT,
            HARNESS,
            NAME,
            BASE,
            CHECK,
            BUDGET_USD,
            MAX_TURNS,
            MAX_MINUTES,
            MAX_DEPTH,
            DENY,
            ALLOW_UNAPPROVED_TOOLS,
            WAIT,
            YES,
            ASK,
            JSON,
        ],
    },
    Spec {
        name: "inspect",
        positionals: &["branch?"],
        summary: "Show a branch's status, candidate, cost, budget and last message",
        flags: &[JSON],
    },
    Spec {
        name: "events",
        positionals: &["branch?"],
        summary: "Show a branch's recorded events from a cursor",
        flags: &[CURSOR, LIMIT, JSON],
    },
    Spec {
        name: "integrate",
        positionals: &["branch"],
        summary: "Merge a delegated child into its parent's branch after its check passes",
        flags: &[JSON],
    },
    Spec {
        name: "cancel",
        positionals: &["branch"],
        summary: "Stop a branch's running turn and every turn delegated below it",
        flags: &[JSON],
    },
    Spec {
        name: "children",
        positionals: &["branch?"],
        summary: "List the branches a branch delegated to",
        flags: &[JSON],
    },
    Spec {
        name: "rig",
        positionals: &["check|run", "file", "prompt?"],
        summary: "Check a rig spec and print its plan, or run its root seat with a prompt",
        flags: &[NAME, BASE, COMMAND, ALLOW_UNAPPROVED_TOOLS, JSON],
    },
    Spec {
        name: "artifact",
        positionals: &["publish|list|get|share", "arg?"],
        summary: "Publish, list, read or share an immutable artifact (see docs/storage.md)",
        flags: &[
            NAME,
            MEDIA_TYPE,
            ARTIFACT_LABEL,
            ARTIFACT_OUT,
            ARTIFACT_TO,
            ACT_AS,
            JSON,
        ],
    },
    Spec {
        name: "scratch",
        positionals: &["create|list|lock|unlock|share", "name?"],
        summary: "Create, list, lock, unlock or share a scratch area (see docs/storage.md)",
        flags: &[ARTIFACT_TO, ACT_AS, JSON],
    },
    Spec {
        name: "mcp",
        positionals: &[],
        summary: "Serve a branch's delegation tools over MCP on stdio (started by the engine)",
        flags: &[ROOT, BRANCH],
    },
    Spec {
        name: "help",
        positionals: &[],
        summary: "Show help for by or one command",
        flags: &[],
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

/// Parse the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let Some(first) = args.first() else {
        return Ok(Command::Help { topic: None });
    };
    match first.as_str() {
        "help" | "-h" | "--help" => return help(args.get(1..).unwrap_or_default()),
        "-V" | "--version" => return Ok(Command::Version),
        _ => {}
    }
    if first == "serve" {
        // The server parses its own options, and prints its own help.
        return Ok(Command::Serve {
            args: args[1..].to_vec(),
        });
    }
    let spec = spec(first).ok_or_else(|| UsageError {
        message: format!("unknown command '{first}'"),
        command: None,
    })?;
    let m = Matches::parse(spec, &args[1..])?;
    if m.help {
        return Ok(Command::Help { topic: Some(spec) });
    }
    let mut positionals = m.positionals.iter().cloned();
    let mut optional = positionals.clone().skip(
        spec.positionals
            .iter()
            .filter(|p| !p.ends_with('?'))
            .count(),
    );
    let mut next = || positionals.next().expect("arity checked in Matches::parse");
    Ok(match spec.name {
        "run" => Command::Run {
            prompt: next(),
            task: m.task()?,
        },
        "fan" => {
            let list = m
                .value("harness")
                .ok_or_else(|| m.error("--harness is required"))?;
            Command::Fan {
                prompt: next(),
                harnesses: harness_list(list).map_err(|e| m.error(e))?,
                task: m.task()?,
            }
        }
        "send" => Command::Send {
            branch: next(),
            prompt: next(),
            task: m.task()?,
            steer: m.switch("steer"),
            json: m.switch("json"),
        },
        "fork" => Command::Fork {
            branch: next(),
            prompt: next(),
            fresh_session: m.switch("fresh-session"),
            task: m.task()?,
        },
        "ls" => Command::Ls {
            json: m.switch("json"),
        },
        "show" => Command::Show {
            branch: next(),
            json: m.switch("json"),
        },
        "diff" => Command::Diff { branch: next() },
        "log" => Command::Log {
            branch: next(),
            json: m.switch("json"),
            follow: m.switch("follow"),
        },
        "merge" => Command::Merge {
            branch: next(),
            into: m.value("into").map(str::to_owned),
        },
        "rm" => Command::Rm {
            branch: next(),
            keep_credentials: m.switch("keep-credentials"),
        },
        "harnesses" => Command::Harnesses {
            json: m.switch("json"),
        },
        "watch" => Command::Watch {
            interval: match m.value("interval") {
                None => Duration::from_secs(1),
                Some(text) => text
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s >= 0.05 && *s <= 3600.0)
                    .map(Duration::from_secs_f64)
                    .ok_or_else(|| {
                        m.error(format!(
                            "--interval needs a number of seconds from 0.05 to 3600, not '{text}'"
                        ))
                    })?,
            },
            once: m.switch("once"),
        },
        "spawn" => Command::Spawn {
            prompt: next(),
            spawn: SpawnArgs {
                task: m.task()?,
                parent: m.value("parent").map(str::to_owned),
                wait: m.switch("wait"),
                max_depth: m.number("max-depth")?,
                deny: match m.value("deny") {
                    Some(list) => {
                        harness_list(list).map_err(|e| m.error(e.replace("--harness", "--deny")))?
                    }
                    None => Vec::new(),
                },
                seat: m.value("seat").map(str::to_owned),
                json: m.switch("json"),
            },
        },
        "inspect" => Command::Inspect {
            branch: optional.next(),
            json: m.switch("json"),
        },
        "events" => Command::Events {
            branch: optional.next(),
            cursor: m.number("cursor")?,
            limit: m.number("limit")?,
            json: m.switch("json"),
        },
        "integrate" => Command::Integrate {
            branch: next(),
            json: m.switch("json"),
        },
        "cancel" => Command::Cancel {
            branch: next(),
            json: m.switch("json"),
        },
        "children" => Command::Children {
            branch: optional.next(),
            json: m.switch("json"),
        },
        "rig" => {
            let action = next();
            let file = next();
            let prompt = optional.next();
            let prompt = match (action.as_str(), prompt) {
                ("check", None) => None,
                ("check", Some(extra)) => {
                    return Err(m.error(format!(
                        "rig check takes a file only, not '{extra}'; to run it, use rig run"
                    )))
                }
                ("run", Some(prompt)) if !prompt.trim().is_empty() => Some(prompt),
                ("run", _) => return Err(m.error("rig run needs a prompt for the root seat")),
                (other, _) => {
                    return Err(m.error(format!("unknown rig action '{other}'; use check or run")))
                }
            };
            if prompt.is_none() {
                for flag in ["name", "base", "command", "allow-unapproved-tools"] {
                    if m.switch(flag) {
                        return Err(m.error(format!("--{flag} applies to rig run")));
                    }
                }
            }
            let command = match m.value("command") {
                None => None,
                Some(line) => {
                    let argv = split_words(line).map_err(|e| m.error(format!("--command: {e}")))?;
                    if argv.is_empty() {
                        return Err(m.error("--command needs an executable"));
                    }
                    Some(argv)
                }
            };
            Command::Rig(RigArgs {
                prompt,
                file,
                name: m.value("name").map(str::to_owned),
                base: m.value("base").map(str::to_owned),
                command,
                unapproved_tools: m.switch("allow-unapproved-tools"),
                json: m.switch("json"),
            })
        }
        "artifact" => {
            let action = next();
            let arg = optional.next();
            let labels = m
                .values("label")
                .iter()
                .map(|kv| match kv.split_once('=') {
                    Some((k, v)) => Ok((k.to_owned(), v.to_owned())),
                    None => Err(m.error(format!("--label needs KEY=VALUE, not '{kv}'"))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            match action.as_str() {
                "publish" | "list" | "get" | "share" => {}
                other => {
                    return Err(m.error(format!(
                        "unknown artifact action '{other}'; use publish, list, get or share"
                    )))
                }
            }
            if action == "publish" && arg.is_none() {
                return Err(m.error("artifact publish needs a file"));
            }
            if (action == "get" || action == "share") && arg.is_none() {
                return Err(m.error(format!("artifact {action} needs an id")));
            }
            if action == "get" && m.value("out").is_none() {
                return Err(m.error("artifact get needs --out PATH"));
            }
            if action == "share" && m.value("to").is_none() {
                return Err(m.error("artifact share needs --to BRANCH"));
            }
            Command::Artifact(ArtifactArgs {
                action,
                arg,
                name: m.value("name").map(str::to_owned),
                media_type: m.value("media-type").map(str::to_owned),
                labels,
                out: m.value("out").map(str::to_owned),
                to: m.value("to").map(str::to_owned),
                branch: m.value("branch").map(str::to_owned),
                json: m.switch("json"),
            })
        }
        "scratch" => {
            let action = next();
            let name = optional.next();
            match action.as_str() {
                "create" | "list" | "lock" | "unlock" | "share" => {}
                other => {
                    return Err(m.error(format!(
                        "unknown scratch action '{other}'; use create, list, lock, unlock or share"
                    )))
                }
            }
            if action != "list" && name.is_none() {
                return Err(m.error(format!("scratch {action} needs a name")));
            }
            if action == "share" && m.value("to").is_none() {
                return Err(m.error("scratch share needs --to BRANCH"));
            }
            Command::Scratch(ScratchArgs {
                action,
                name,
                to: m.value("to").map(str::to_owned),
                branch: m.value("branch").map(str::to_owned),
                json: m.switch("json"),
            })
        }
        "mcp" => {
            let mut args = Vec::new();
            for flag in ["root", "branch"] {
                let value = m
                    .value(flag)
                    .ok_or_else(|| m.error(format!("--{flag} is required")))?;
                args.extend([format!("--{flag}"), value.to_owned()]);
            }
            Command::Mcp { args }
        }
        other => unreachable!("command {other} has a spec but no parser"),
    })
}

fn help(rest: &[String]) -> Result<Command, UsageError> {
    match rest {
        [] => Ok(Command::Help { topic: None }),
        [topic] => match spec(topic) {
            Some(spec) => Ok(Command::Help { topic: Some(spec) }),
            None => Err(UsageError {
                message: format!("unknown command '{topic}'"),
                command: None,
            }),
        },
        [_, extra, ..] => Err(UsageError {
            message: format!("unexpected argument '{extra}'"),
            command: Some("help"),
        }),
    }
}

/// Raw flags and positionals, checked against a command's spec.
struct Matches {
    spec: &'static Spec,
    positionals: Vec<String>,
    flags: Vec<(&'static str, Option<String>)>,
    help: bool,
}

impl Matches {
    fn parse(spec: &'static Spec, args: &[String]) -> Result<Matches, UsageError> {
        let mut m = Matches {
            spec,
            positionals: Vec::new(),
            flags: Vec::new(),
            help: false,
        };
        let mut args = args.iter();
        let mut only_positionals = false;
        while let Some(arg) = args.next() {
            if only_positionals {
                m.positionals.push(arg.clone());
                continue;
            }
            match arg.as_str() {
                "--" => only_positionals = true,
                "-h" | "--help" => m.help = true,
                long if long.starts_with("--") => {
                    let (name, inline) = match long[2..].split_once('=') {
                        Some((name, value)) => (name, Some(value.to_owned())),
                        None => (&long[2..], None),
                    };
                    let flag = spec
                        .flags
                        .iter()
                        .find(|flag| flag.long == name)
                        .ok_or_else(|| m.error(format!("unknown option --{name}")))?;
                    if m.flags.iter().any(|(seen, _)| *seen == flag.long)
                        && !REPEATABLE.contains(&flag.long)
                    {
                        return Err(m.error(format!("--{name} given twice")));
                    }
                    let value = match (flag.value, inline) {
                        (None, None) => None,
                        (None, Some(_)) => return Err(m.error(format!("--{name} takes no value"))),
                        (Some(_), Some(value)) => Some(value),
                        (Some(placeholder), None) if placeholder.starts_with('[') => None,
                        (Some(placeholder), None) => {
                            Some(args.next().cloned().ok_or_else(|| {
                                m.error(format!("--{name} needs a value {placeholder}"))
                            })?)
                        }
                    };
                    m.flags.push((flag.long, value));
                }
                short if short.starts_with('-') && short.len() > 1 => {
                    return Err(m.error(format!("unknown option {short}")));
                }
                _ => m.positionals.push(arg.clone()),
            }
        }
        if m.help {
            return Ok(m);
        }
        let expected = spec.positionals;
        if let Some(missing) = expected
            .get(m.positionals.len())
            .filter(|p| !p.ends_with('?'))
        {
            return Err(m.error(format!("missing <{missing}>")));
        }
        if let Some(extra) = m.positionals.get(expected.len()) {
            let hint = if expected.contains(&"prompt") {
                " (quote a prompt that contains spaces)"
            } else {
                ""
            };
            return Err(m.error(format!("unexpected argument '{extra}'{hint}")));
        }
        Ok(m)
    }

    fn error(&self, message: impl Into<String>) -> UsageError {
        UsageError {
            message: message.into(),
            command: Some(self.spec.name),
        }
    }

    /// Every value of a repeatable flag, in order.
    fn values(&self, name: &str) -> Vec<&str> {
        self.flags
            .iter()
            .filter(|(flag, _)| *flag == name)
            .filter_map(|(_, value)| value.as_deref())
            .collect()
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(flag, _)| *flag == name)
            .and_then(|(_, value)| value.as_deref())
    }

    fn switch(&self, name: &str) -> bool {
        self.flags.iter().any(|(flag, _)| *flag == name)
    }

    /// A whole number given to `--name`, if it was given.
    fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, UsageError> {
        match self.value(name) {
            None => Ok(None),
            Some(text) => text
                .parse()
                .map(Some)
                .map_err(|_| self.error(format!("--{name} needs a whole number, not '{text}'"))),
        }
    }

    fn task(&self) -> Result<TaskArgs, UsageError> {
        let permissions = match (self.switch("yes"), self.switch("ask")) {
            (true, true) => return Err(self.error("--yes and --ask conflict; pick one")),
            (true, false) => Permissions::Yes,
            (false, true) => Permissions::Ask,
            (false, false) => Permissions::Unset,
        };
        let check = match self.value("check") {
            None => None,
            Some(line) => {
                let argv = split_words(line).map_err(|e| self.error(format!("--check: {e}")))?;
                if argv.is_empty() {
                    return Err(self.error("--check needs a command"));
                }
                Some(argv)
            }
        };
        let budget_usd = match self.value("budget-usd") {
            None => None,
            Some(text) => match text.parse::<f64>() {
                Ok(usd) if usd.is_finite() && usd > 0.0 => Some(usd),
                _ => {
                    return Err(self.error(format!(
                        "--budget-usd needs a positive number of dollars, not '{text}'"
                    )))
                }
            },
        };
        let max_turns = match self.value("max-turns") {
            None => None,
            Some(text) => match text.parse::<u32>() {
                Ok(turns) if turns > 0 => Some(turns),
                _ => {
                    return Err(self.error(format!(
                        "--max-turns needs a positive whole number, not '{text}'"
                    )))
                }
            },
        };
        let max_duration = match self.value("max-minutes") {
            None => None,
            Some(text) => match text
                .parse::<f64>()
                .ok()
                .filter(|m| m.is_finite() && *m > 0.0)
            {
                Some(minutes) => Some(
                    Duration::try_from_secs_f64(minutes * 60.0)
                        .map_err(|_| self.error(format!("--max-minutes {text} is too large")))?,
                ),
                None => {
                    return Err(self.error(format!(
                        "--max-minutes needs a positive number of minutes, not '{text}'"
                    )))
                }
            },
        };
        let command = match self.value("command") {
            None => None,
            Some(line) => {
                let argv = split_words(line).map_err(|e| self.error(format!("--command: {e}")))?;
                if argv.is_empty() {
                    return Err(self.error("--command needs an executable"));
                }
                Some(argv)
            }
        };
        let provider = self.provider()?;
        let delegate = match (self.switch("delegate"), self.value("delegate")) {
            (false, _) => None,
            (true, None) => Some(1),
            (true, Some(text)) => match text.parse::<u32>() {
                Ok(depth) if depth > 0 => Some(depth),
                _ => {
                    return Err(self.error(format!(
                        "--delegate=DEPTH needs a positive whole number, not '{text}'"
                    )))
                }
            },
        };
        let provision = self.provision()?;
        // `fan` reads `--harness` as a list; it is not one harness.
        let harness = match self.spec.name {
            "fan" => None,
            _ => self.value("harness").map(str::to_owned),
        };
        Ok(TaskArgs {
            harness,
            name: self.value("name").map(str::to_owned),
            base: self.value("base").map(str::to_owned),
            check,
            budget_usd,
            max_turns,
            max_duration,
            permissions,
            isolated: self.switch("isolated"),
            command,
            sandbox: match &provider {
                Some(Chosen::Microsandbox(args)) => Some(args.clone()),
                _ => None,
            },
            substrate: match &provider {
                Some(Chosen::Substrate(args)) => Some(SubstrateArgs::clone(args)),
                _ => None,
            },
            local: provider == Some(Chosen::Local),
            delegate,
            allow_delegation: self.switch("allow-delegation"),
            unapproved_tools: self.switch("allow-unapproved-tools"),
            provision,
            instructions: self.value("instructions").map(str::to_owned),
        })
    }

    /// The provisioning flags; `None` when none was given. `--instructions`
    /// is read later, from its file.
    fn provision(&self) -> Result<Option<branchyard::Provisioning>, UsageError> {
        use branchyard::{Effort, McpServerSpec, Provisioning, SecretSource, Telemetry};
        let mut spec = Provisioning::default();
        for text in self.values("secret") {
            spec.secrets
                .push(SecretSource::parse(text).map_err(|e| self.error(format!("--secret: {e}")))?);
        }
        for text in self.values("mcp") {
            spec.mcp_servers
                .push(McpServerSpec::parse(text).map_err(|e| self.error(format!("--mcp: {e}")))?);
        }
        spec.auth = self.value("auth").map(str::to_owned);
        spec.model = match self.value("model") {
            Some(model) if model.trim().is_empty() => {
                return Err(self.error("--model needs a model name"))
            }
            model => model.map(str::to_owned),
        };
        if let Some(text) = self.value("effort") {
            spec.effort =
                Some(Effort::parse(text).map_err(|e| self.error(format!("--effort: {e}")))?);
        }
        if let Some(text) = self.value("telemetry") {
            spec.telemetry =
                Some(Telemetry::parse(text).map_err(|e| self.error(format!("--telemetry: {e}")))?);
        }
        Ok((!spec.is_empty() || self.switch("instructions")).then_some(spec))
    }

    /// The chosen provider and its options; `None` when `--provider` was
    /// not given.
    fn provider(&self) -> Result<Option<Chosen>, UsageError> {
        let micro_flags = ["image", "cpus", "memory"];
        let substrate_flags = [
            "substrate-endpoint",
            "substrate-router",
            "substrate-template",
            "substrate-key",
            "substrate-atespace",
            "substrate-workdir",
            "substrate-home",
            "substrate-ca",
            "substrate-client-cert",
            "substrate-client-key",
            "substrate-router-ca",
            "substrate-insecure",
        ];
        let given = |flags: &[&'static str]| -> Option<&'static str> {
            flags.iter().copied().find(|flag| self.switch(flag))
        };
        let chosen = self.value("provider");
        if chosen != Some("microsandbox") {
            if let Some(flag) = given(&micro_flags) {
                return Err(self.error(format!("--{flag} needs --provider microsandbox")));
            }
        }
        if chosen != Some("substrate") {
            if let Some(flag) = given(&substrate_flags) {
                return Err(self.error(format!("--{flag} needs --provider substrate")));
            }
        }
        if matches!(chosen, None | Some("local")) && self.switch("pass-env") {
            return Err(self.error("--pass-env needs --provider microsandbox or substrate"));
        }
        let mut pass_env = Vec::new();
        if let Some(list) = self.value("pass-env") {
            for name in list.split(',').map(str::trim) {
                if name.is_empty() || name.contains('=') {
                    return Err(
                        self.error(format!("--pass-env takes variable names, not '{list}'"))
                    );
                }
                pass_env.push(name.to_owned());
            }
        }
        match chosen {
            None => Ok(None),
            Some("local") => Ok(Some(Chosen::Local)),
            Some("microsandbox") => {
                let image = self
                    .value("image")
                    .filter(|image| !image.trim().is_empty())
                    .ok_or_else(|| self.error("--provider microsandbox needs --image"))?;
                let cpus = match self.value("cpus") {
                    None => None,
                    Some(text) => match text.parse::<u8>() {
                        Ok(cpus) if cpus > 0 => Some(cpus),
                        _ => {
                            return Err(self.error(format!(
                                "--cpus needs a whole number from 1 to 255, not '{text}'"
                            )))
                        }
                    },
                };
                let memory_mib = match self.value("memory") {
                    None => None,
                    Some(text) => match text.parse::<u32>() {
                        Ok(mib) if mib > 0 => Some(mib),
                        _ => {
                            return Err(self.error(format!(
                                "--memory needs a positive whole number of MiB, not '{text}'"
                            )))
                        }
                    },
                };
                Ok(Some(Chosen::Microsandbox(SandboxArgs {
                    image: image.to_owned(),
                    cpus,
                    memory_mib,
                    pass_env,
                })))
            }
            Some("substrate") => {
                let required = |flag: &str| -> Result<String, UsageError> {
                    self.value(flag)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| self.error(format!("--provider substrate needs --{flag}")))
                };
                let optional = |flag: &str| self.value(flag).map(str::to_owned);
                Ok(Some(Chosen::Substrate(Box::new(SubstrateArgs {
                    endpoint: required("substrate-endpoint")?,
                    router: required("substrate-router")?,
                    template: required("substrate-template")?,
                    key: required("substrate-key")?,
                    atespace: optional("substrate-atespace"),
                    workdir: optional("substrate-workdir"),
                    home: optional("substrate-home"),
                    pass_env,
                    ca: optional("substrate-ca"),
                    client_cert: optional("substrate-client-cert"),
                    client_key: optional("substrate-client-key"),
                    router_ca: optional("substrate-router-ca"),
                    insecure: self.switch("substrate-insecure"),
                }))))
            }
            Some(other) => Err(self.error(format!(
                "--provider is local, microsandbox or substrate, not '{other}'"
            ))),
        }
    }
}

/// Split `claude-code,codex` into harness IDs. Duplicates are refused because
/// branch names derive from the harness.
fn harness_list(list: &str) -> Result<Vec<String>, String> {
    let mut harnesses: Vec<String> = Vec::new();
    for id in list.split(',').map(str::trim) {
        if id.is_empty() {
            return Err(format!("--harness has an empty entry in '{list}'"));
        }
        if harnesses.iter().any(|seen| seen == id) {
            return Err(format!("--harness lists {id} twice"));
        }
        harnesses.push(id.to_owned());
    }
    Ok(harnesses)
}

/// Split a command line into words the way a POSIX shell quotes them: single
/// quotes, double quotes and backslash escapes. There is no variable, glob
/// or operator expansion; the result runs without a shell.
pub fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().ok_or("trailing backslash")?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Quote `word` for a POSIX shell, leaving plain words as they are.
pub fn shell_quote(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Help for `by` itself.
pub fn general_help() -> String {
    let mut text = String::from(
        "by: delegate coding work to agent harnesses on git branches, and merge\n\
         only validated results.\n\nUsage: by <command> [options]\n\nCommands:\n",
    );
    for spec in COMMANDS {
        text.push_str(&format!("  {:<10} {}\n", spec.name, spec.summary));
    }
    text.push_str(
        "\nGlobal options, before the command:\n\
         \x20 --remote URL       Run commands on a Branchyard server (or BRANCHYARD_REMOTE)\n\
         \x20 --token-file FILE  The server's bearer token (or BRANCHYARD_TOKEN_FILE)\n\
         \x20 --repo NAME        Repository on the server, when it serves several\n\
         \x20                    (or BRANCHYARD_REPO)\n\
         \x20 --ca-file FILE     Also trust this CA for https (or BRANCHYARD_CA_FILE)\n\
         \nRun 'by help <command>' or 'by <command> --help' for its options.\n\
         Local mode: harnesses run as your operating-system user, with no other\n\
         isolation. State lives in .branchyard/ at the repository root. Remote mode:\n\
         harnesses run as the server's user, with no other isolation.\n",
    );
    text
}

/// Help for one command.
pub fn command_help(spec: &Spec) -> String {
    let mut text = format!("{}\n\nUsage: {}\n", spec.summary, spec.usage());
    if !spec.flags.is_empty() {
        text.push_str("\nOptions:\n");
        let label = |flag: &Flag| match flag.value {
            Some(value) if value.starts_with('[') => format!("--{}{value}", flag.long),
            Some(value) => format!("--{} {value}", flag.long),
            None => format!("--{}", flag.long),
        };
        let width = spec.flags.iter().map(|f| label(f).len()).max().unwrap_or(0);
        for flag in spec.flags {
            text.push_str(&format!("  {:<width$}  {}\n", label(flag), flag.help));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, UsageError> {
        parse(&split_words(line).unwrap())
    }

    fn err(line: &str) -> String {
        parse_str(line).unwrap_err().message
    }

    #[test]
    fn run_takes_every_task_option() {
        let command = parse_str(
            "run 'fix the flaky test' --harness codex --name flaky --base main \
             --check 'cargo test -p core' --budget-usd 2.5 --max-turns 3 --yes \
             --max-minutes 1.5 --isolated --command '/opt/codex/bin/codex --flag'",
        )
        .unwrap();
        assert_eq!(
            command,
            Command::Run {
                prompt: "fix the flaky test".into(),
                task: TaskArgs {
                    harness: Some("codex".into()),
                    name: Some("flaky".into()),
                    base: Some("main".into()),
                    check: Some(vec![
                        "cargo".into(),
                        "test".into(),
                        "-p".into(),
                        "core".into()
                    ]),
                    budget_usd: Some(2.5),
                    max_turns: Some(3),
                    max_duration: Some(Duration::from_secs(90)),
                    permissions: Permissions::Yes,
                    isolated: true,
                    command: Some(vec!["/opt/codex/bin/codex".into(), "--flag".into()]),
                    sandbox: None,
                    substrate: None,
                    local: false,
                    delegate: None,
                    allow_delegation: false,
                    unapproved_tools: false,
                    provision: None,
                    instructions: None,
                },
            }
        );
    }

    #[test]
    fn provisioning_flags_repeat_and_parse() {
        let Command::Run { task, .. } = parse_str(
            "run go --isolated --secret ANTHROPIC_API_KEY --secret CODEX_AUTH=@/run/auth.json \
             --secret OPENAI_API_KEY=MY_KEY --mcp 'docs=/usr/bin/docs-mcp --stdio' \
             --auth api-key --model large --effort 80 --telemetry http://127.0.0.1:4317 \
             --instructions rules.md",
        )
        .unwrap() else {
            panic!("not run")
        };
        let spec = task.provision.unwrap();
        assert_eq!(spec.secrets.len(), 3);
        assert_eq!(
            spec.secrets[1].from,
            Some(branchyard::SecretFrom::File {
                path: "/run/auth.json".into()
            })
        );
        assert_eq!(spec.mcp_servers[0].args, ["--stdio"]);
        assert_eq!(spec.auth.as_deref(), Some("api-key"));
        assert_eq!(spec.effort, Some(branchyard::Effort::Xhigh));
        assert_eq!(spec.telemetry.unwrap().endpoint(), "http://127.0.0.1:4317");
        assert_eq!(task.instructions.as_deref(), Some("rules.md"));
        let Command::Send { task, .. } = parse_str("send b go").unwrap() else {
            panic!("not send")
        };
        assert!(task.provision.is_none());
        for (line, error) in [
            ("run go --mcp docs=relative", "absolute command"),
            ("run go --secret 1BAD", "--secret"),
            ("run go --effort max", "--effort"),
            ("run go --telemetry collector:4317", "--telemetry"),
            ("run go --model a --model b", "--model given twice"),
        ] {
            assert!(err(line).contains(error), "{line}: {}", err(line));
        }
    }

    #[test]
    fn substrate_flags_configure_tls_and_the_insecure_escape() {
        let Command::Run { task, .. } = parse_str(
            "run go --provider substrate --substrate-endpoint https://control:443 \
             --substrate-router 'wss://router/{atespace}/{actor}/' --substrate-template t \
             --substrate-key k --substrate-ca ca.pem --substrate-client-cert c.pem \
             --substrate-client-key c.key --substrate-router-ca router-ca.pem",
        )
        .unwrap() else {
            panic!("not run")
        };
        let substrate = task.substrate.unwrap();
        assert_eq!(substrate.endpoint, "https://control:443");
        assert_eq!(substrate.ca.as_deref(), Some("ca.pem"));
        assert_eq!(substrate.client_cert.as_deref(), Some("c.pem"));
        assert_eq!(substrate.client_key.as_deref(), Some("c.key"));
        assert_eq!(substrate.router_ca.as_deref(), Some("router-ca.pem"));
        assert!(!substrate.insecure);
        let Command::Fork { task, .. } = parse_str(
            "fork b go --provider substrate --substrate-endpoint http://10.0.0.1:8080 \
             --substrate-router 'http://10.0.0.2/{actor}/' --substrate-template t \
             --substrate-key k --substrate-insecure",
        )
        .unwrap() else {
            panic!("not fork")
        };
        let substrate = task.substrate.unwrap();
        assert!(substrate.insecure && substrate.ca.is_none());
        assert_eq!(
            err("run go --substrate-insecure"),
            "--substrate-insecure needs --provider substrate"
        );
        assert_eq!(
            err("run go --provider local --substrate-ca ca.pem"),
            "--substrate-ca needs --provider substrate"
        );
    }

    #[test]
    fn provider_flags_select_and_configure_a_sandbox() {
        let Command::Run { task, .. } = parse_str(
            "run go --provider microsandbox --image ghcr.io/x/claude:1 --cpus 2 \
             --memory 4096 --pass-env 'ANTHROPIC_API_KEY, GH_TOKEN'",
        )
        .unwrap() else {
            panic!("not run")
        };
        assert_eq!(
            task.sandbox,
            Some(SandboxArgs {
                image: "ghcr.io/x/claude:1".into(),
                cpus: Some(2),
                memory_mib: Some(4096),
                pass_env: vec!["ANTHROPIC_API_KEY".into(), "GH_TOKEN".into()],
            })
        );
        assert!(!task.local);
        let Command::Fork { task, .. } = parse_str("fork b go --provider local").unwrap() else {
            panic!("not fork")
        };
        assert!(task.local && task.sandbox.is_none());
        assert_eq!(
            err("run go --provider microsandbox"),
            "--provider microsandbox needs --image"
        );
        assert_eq!(
            err("run go --image alpine"),
            "--image needs --provider microsandbox"
        );
        assert_eq!(
            err("run go --provider local --cpus 2"),
            "--cpus needs --provider microsandbox"
        );
        assert!(err("run go --provider docker").contains("local, microsandbox or substrate"));
        assert!(err("run go --provider microsandbox --image a --cpus 0").contains("--cpus"));
        assert!(err("run go --provider microsandbox --image a --memory 1g").contains("--memory"));
        assert!(err("run go --provider microsandbox --image a --pass-env A=1").contains("names"));
        assert_eq!(
            err("send b go --provider local"),
            "unknown option --provider"
        );
    }

    #[test]
    fn delegate_takes_an_optional_inline_depth() {
        let depth = |line: &str| match parse_str(line).unwrap() {
            Command::Run { task, .. } | Command::Send { task, .. } => task.delegate,
            other => panic!("{other:?}"),
        };
        assert_eq!(depth("run go"), None);
        assert_eq!(depth("run go --delegate"), Some(1));
        assert_eq!(
            depth("run --delegate go"),
            Some(1),
            "the prompt is not a depth"
        );
        assert_eq!(depth("run go --delegate=3"), Some(3));
        assert_eq!(depth("send b go --delegate=2"), Some(2));
        assert!(err("run go --delegate=0").contains("positive whole number"));
        assert!(err("run go --delegate=x").contains("positive whole number"));
        assert!(command_help(spec("run").unwrap()).contains("--delegate[=DEPTH]"));
    }

    #[test]
    fn delegation_commands_parse_with_optional_branches() {
        let Command::Spawn { prompt, spawn } = parse_str(
            "spawn 'fix it' --parent root --harness codex --name fix --budget-usd 0.5 \
             --max-depth 0 --deny Bash,mcp__* --wait --json --yes",
        )
        .unwrap() else {
            panic!("not spawn")
        };
        assert_eq!(prompt, "fix it");
        assert_eq!(spawn.parent.as_deref(), Some("root"));
        assert_eq!(spawn.task.harness.as_deref(), Some("codex"));
        assert_eq!(spawn.task.name.as_deref(), Some("fix"));
        assert_eq!(spawn.task.budget_usd, Some(0.5));
        assert_eq!(spawn.task.permissions, Permissions::Yes);
        assert_eq!(spawn.max_depth, Some(0));
        assert_eq!(spawn.deny, ["Bash", "mcp__*"]);
        assert!(spawn.wait && spawn.json);
        assert_eq!(
            parse_str("inspect").unwrap(),
            Command::Inspect {
                branch: None,
                json: false
            }
        );
        assert_eq!(
            parse_str("inspect kid --json").unwrap(),
            Command::Inspect {
                branch: Some("kid".into()),
                json: true
            }
        );
        assert_eq!(
            parse_str("events kid --cursor 7 --limit 3").unwrap(),
            Command::Events {
                branch: Some("kid".into()),
                cursor: Some(7),
                limit: Some(3),
                json: false
            }
        );
        assert_eq!(
            parse_str("children").unwrap(),
            Command::Children {
                branch: None,
                json: false
            }
        );
        assert_eq!(
            parse_str("integrate kid --json").unwrap(),
            Command::Integrate {
                branch: "kid".into(),
                json: true
            }
        );
        assert_eq!(
            parse_str("cancel kid").unwrap(),
            Command::Cancel {
                branch: "kid".into(),
                json: false
            }
        );
        assert_eq!(err("integrate"), "missing <branch>");
        assert_eq!(err("inspect a b"), "unexpected argument 'b'");
        assert!(err("events --cursor x").contains("whole number"));
        assert_eq!(
            spec("inspect").unwrap().usage(),
            "by inspect [<branch>] [options]"
        );
        let Command::Run { task, .. } = parse_str("run go --delegate --allow-delegation").unwrap()
        else {
            panic!("not run")
        };
        assert!(task.allow_delegation);
        let Command::Send { task, .. } = parse_str("send b go --allow-unapproved-tools").unwrap()
        else {
            panic!("not send")
        };
        assert!(task.unapproved_tools);
    }

    #[test]
    fn send_steer_is_a_switch() {
        let Command::Send {
            steer, json, task, ..
        } = parse_str("send b --steer --json also-this").unwrap()
        else {
            panic!("not send")
        };
        assert!(steer && json);
        assert_eq!(task, TaskArgs::default());
        let Command::Send { steer, .. } = parse_str("send b go").unwrap() else {
            panic!("not send")
        };
        assert!(!steer);
    }

    #[test]
    fn mcp_needs_a_root_and_a_branch() {
        assert_eq!(
            parse_str("mcp --root /r --branch b").unwrap(),
            Command::Mcp {
                args: vec!["--root".into(), "/r".into(), "--branch".into(), "b".into()]
            }
        );
        assert_eq!(err("mcp --root /r"), "--branch is required");
    }

    #[test]
    fn run_defaults_and_equals_form() {
        let Command::Run { prompt, task } = parse_str("run --ask --name=x \"do it\"").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(prompt, "do it");
        assert_eq!(task.name.as_deref(), Some("x"));
        assert_eq!(task.permissions, Permissions::Ask);
        let Command::Run { task, .. } = parse_str("run go").unwrap() else {
            panic!("not run")
        };
        assert_eq!(task, TaskArgs::default());
    }

    #[test]
    fn fan_requires_a_harness_list() {
        let Command::Fan {
            harnesses, task, ..
        } = parse_str("fan go --harness 'claude-code, codex' --max-turns 2").unwrap()
        else {
            panic!("not fan")
        };
        assert_eq!(harnesses, ["claude-code", "codex"]);
        assert_eq!(task.harness, None);
        assert_eq!(task.max_turns, Some(2));
        assert_eq!(err("fan go"), "--harness is required");
        assert_eq!(
            err("fan go --harness codex,codex"),
            "--harness lists codex twice"
        );
        assert!(err("fan go --harness codex,").contains("empty entry"));
    }

    #[test]
    fn send_and_fork_take_a_branch_and_a_prompt() {
        assert_eq!(
            parse_str("send flaky 'now add a test' --yes").unwrap(),
            Command::Send {
                branch: "flaky".into(),
                prompt: "now add a test".into(),
                task: TaskArgs {
                    permissions: Permissions::Yes,
                    ..TaskArgs::default()
                },
                steer: false,
                json: false,
            }
        );
        assert_eq!(
            parse_str("fork flaky 'try another way' --fresh-session --name alt").unwrap(),
            Command::Fork {
                branch: "flaky".into(),
                prompt: "try another way".into(),
                fresh_session: true,
                task: TaskArgs {
                    name: Some("alt".into()),
                    ..TaskArgs::default()
                },
            }
        );
        assert_eq!(err("send flaky"), "missing <prompt>");
        assert_eq!(err("fork"), "missing <branch>");
        assert_eq!(
            err("send flaky go --harness codex"),
            "unknown option --harness"
        );
    }

    #[test]
    fn inspection_commands() {
        assert_eq!(parse_str("ls").unwrap(), Command::Ls { json: false });
        assert_eq!(parse_str("ls --json").unwrap(), Command::Ls { json: true });
        assert_eq!(
            parse_str("show b --json").unwrap(),
            Command::Show {
                branch: "b".into(),
                json: true
            }
        );
        assert_eq!(
            parse_str("diff b").unwrap(),
            Command::Diff { branch: "b".into() }
        );
        assert_eq!(
            parse_str("log b").unwrap(),
            Command::Log {
                branch: "b".into(),
                json: false,
                follow: false
            }
        );
        assert_eq!(
            parse_str("log --follow b").unwrap(),
            Command::Log {
                branch: "b".into(),
                json: false,
                follow: true
            }
        );
        assert_eq!(
            parse_str("harnesses --json").unwrap(),
            Command::Harnesses { json: true }
        );
        assert_eq!(err("diff b --json"), "unknown option --json");
    }

    #[test]
    fn merge_and_rm() {
        assert_eq!(
            parse_str("merge b").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: None
            }
        );
        assert_eq!(
            parse_str("merge b --into release").unwrap(),
            Command::Merge {
                branch: "b".into(),
                into: Some("release".into())
            }
        );
        assert_eq!(
            parse_str("rm b").unwrap(),
            Command::Rm {
                branch: "b".into(),
                keep_credentials: false
            }
        );
        assert_eq!(
            parse_str("rm b --keep-credentials").unwrap(),
            Command::Rm {
                branch: "b".into(),
                keep_credentials: true
            }
        );
        assert_eq!(err("merge b --into"), "--into needs a value TARGET");
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(&[]).unwrap(), Command::Help { topic: None });
        assert_eq!(parse_str("help").unwrap(), Command::Help { topic: None });
        assert_eq!(parse_str("--help").unwrap(), Command::Help { topic: None });
        assert_eq!(
            parse_str("help merge").unwrap(),
            Command::Help {
                topic: spec("merge")
            }
        );
        // --help wins over otherwise invalid arguments.
        assert_eq!(
            parse_str("run --help").unwrap(),
            Command::Help { topic: spec("run") }
        );
        assert_eq!(
            parse_str("fan -h").unwrap(),
            Command::Help { topic: spec("fan") }
        );
        assert_eq!(parse_str("--version").unwrap(), Command::Version);
        assert_eq!(parse_str("-V").unwrap(), Command::Version);
        assert_eq!(err("help nope"), "unknown command 'nope'");
    }

    #[test]
    fn flag_errors_name_the_command() {
        let error = parse_str("run go --bogus").unwrap_err();
        assert_eq!(error.message, "unknown option --bogus");
        assert_eq!(error.command, Some("run"));
        assert_eq!(err("nope"), "unknown command 'nope'");
        assert_eq!(err("run go -x"), "unknown option -x");
        assert_eq!(
            err("run go --yes --ask"),
            "--yes and --ask conflict; pick one"
        );
        assert_eq!(err("run go --yes=1"), "--yes takes no value");
        assert_eq!(err("run go --name a --name b"), "--name given twice");
        assert_eq!(err("run go --harness"), "--harness needs a value ID");
        assert!(err("run go --budget-usd -1").contains("positive number"));
        assert!(err("run go --budget-usd NaN").contains("positive number"));
        assert!(err("run go --max-turns 0").contains("positive whole number"));
        assert!(err("run go --max-turns 1.5").contains("positive whole number"));
        assert!(err("run go --max-minutes 0").contains("positive number of minutes"));
        assert!(err("run go --max-minutes 1e300").contains("too large"));
        assert_eq!(err("run go --command ''"), "--command needs an executable");
        assert_eq!(err("send b go --isolated"), "unknown option --isolated");
        assert_eq!(err("run go --check ''"), "--check needs a command");
        assert_eq!(
            err("run go --check '\"cargo'"),
            "--check: unterminated double quote"
        );
        assert_eq!(
            err("run fix the test"),
            "unexpected argument 'the' (quote a prompt that contains spaces)"
        );
        assert_eq!(err("ls extra"), "unexpected argument 'extra'");
    }

    #[test]
    fn globals_come_before_the_command_and_fall_back_to_the_environment() {
        let argv = split_words("--remote http://h:1 --token-file=t --repo app ls --json").unwrap();
        let (globals, rest) = parse_globals(&argv).unwrap();
        assert_eq!(
            globals,
            Globals {
                remote: Some("http://h:1".into()),
                token_file: Some("t".into()),
                repo: Some("app".into()),
                ca_file: None,
            }
        );
        assert_eq!(parse(rest).unwrap(), Command::Ls { json: true });
        // After the command they are the command's options.
        let argv = split_words("ls --remote x").unwrap();
        let (globals, rest) = parse_globals(&argv).unwrap();
        assert_eq!(globals, Globals::default());
        assert_eq!(parse(rest).unwrap_err().message, "unknown option --remote");
        let env = |name: &str| match name {
            "BRANCHYARD_REMOTE" => Some("http://env:2".to_owned()),
            "BRANCHYARD_REPO" => Some(" ".to_owned()),
            _ => None,
        };
        let merged = Globals {
            token_file: Some("f".into()),
            ..Globals::default()
        }
        .with_env(env);
        assert_eq!(merged.remote.as_deref(), Some("http://env:2"));
        assert_eq!(merged.repo, None, "blank variables are unset");
        let flag_wins = Globals {
            remote: Some("http://flag:3".into()),
            ..Globals::default()
        }
        .with_env(env);
        assert_eq!(flag_wins.remote.as_deref(), Some("http://flag:3"));
        for (line, error) in [
            ("--remote", "--remote needs a value URL"),
            ("--remote= ls", "--remote needs a value URL"),
            ("--repo a --repo b ls", "--repo given twice"),
        ] {
            let argv = split_words(line).unwrap();
            assert_eq!(parse_globals(&argv).unwrap_err().message, error, "{line}");
        }
    }

    #[test]
    fn watch_and_serve() {
        assert_eq!(
            parse_str("watch").unwrap(),
            Command::Watch {
                interval: Duration::from_secs(1),
                once: false
            }
        );
        assert_eq!(
            parse_str("watch --interval 0.5 --once").unwrap(),
            Command::Watch {
                interval: Duration::from_millis(500),
                once: true
            }
        );
        assert!(err("watch --interval 0").contains("--interval needs"));
        assert_eq!(
            parse_str("serve --listen 127.0.0.1:0 --help").unwrap(),
            Command::Serve {
                args: vec!["--listen".into(), "127.0.0.1:0".into(), "--help".into()]
            }
        );
        assert_eq!(
            parse_str("help serve").unwrap(),
            Command::Help {
                topic: spec("serve")
            }
        );
    }

    #[test]
    fn double_dash_allows_prompts_that_look_like_flags() {
        let Command::Run { prompt, task } = parse_str("run --yes -- --explain-flags").unwrap()
        else {
            panic!("not run")
        };
        assert_eq!(prompt, "--explain-flags");
        assert_eq!(task.permissions, Permissions::Yes);
    }

    #[test]
    fn split_words_follows_shell_quoting() {
        let split = |s: &str| split_words(s).unwrap();
        assert_eq!(split("  cargo   test  "), ["cargo", "test"]);
        assert_eq!(split("sh -c 'echo $HOME'"), ["sh", "-c", "echo $HOME"]);
        assert_eq!(
            split(r#"say "a \"quoted\" \$word" and\ more"#),
            ["say", r#"a "quoted" $word"#, "and more"]
        );
        assert_eq!(split(r#""keep \n literal""#), [r"keep \n literal"]);
        assert_eq!(split("''"), [""]);
        assert_eq!(split("a'b'\"c\""), ["abc"]);
        assert!(split("").is_empty());
        assert_eq!(split_words("a\\").unwrap_err(), "trailing backslash");
        assert_eq!(split_words("'a").unwrap_err(), "unterminated single quote");
    }

    #[test]
    fn shell_quote_round_trips() {
        assert_eq!(shell_quote("fix-flaky_1.2"), "fix-flaky_1.2");
        assert_eq!(shell_quote("it's here"), r"'it'\''s here'");
        assert_eq!(shell_quote(""), "''");
        for word in ["plain", "two words", "it's", "$x", ""] {
            assert_eq!(split_words(&shell_quote(word)).unwrap(), [word]);
        }
    }

    #[test]
    fn help_lists_every_command_and_option() {
        let general = general_help();
        for spec in COMMANDS {
            assert!(general.contains(spec.name), "{}", spec.name);
            let help = command_help(spec);
            assert!(help.contains(&spec.usage()));
            for flag in spec.flags {
                assert!(help.contains(&format!("--{}", flag.long)));
            }
        }
        assert_eq!(
            spec("fork").unwrap().usage(),
            "by fork <branch> <prompt> [options]"
        );
        assert_eq!(spec("ls").unwrap().usage(), "by ls [options]");
        assert_eq!(spec("rm").unwrap().usage(), "by rm <branch> [options]");
    }
}
