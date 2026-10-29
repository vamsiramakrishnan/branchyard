//! Remote mode: the same commands against a Branchyard server, through
//! `branchyard-client`. Output is rendered by the same code as local mode
//! from the same SDK values, so it matches line for line.
//!
//! A command that runs harnesses submits an operation, follows the
//! repository's event stream from the operation's cursor, and prints the
//! summary from the finished operation. Interrupting `by` stops only the
//! watching; the server finishes the work.

use std::collections::HashSet;
use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use branchyard::{
    Activity, BranchEvent, BranchInfo, Children, Envelope, EventPage, Inspection, Merged, Provider,
    Sent,
};
use branchyard_client::api::{
    BudgetSpec, ErrorBody, FeedEntry, ForkRequest, GraphRequest, MapRequest, MergeRequest,
    Operation, OperationResult, OperationState, PolicyMode, PolicySpec, ReincarnateRequest,
    RuleSpec, SendRequest, SpawnRequest, TaskRequest,
};
use branchyard_client::{new_key, Client, Repo};

use crate::args::{Globals, Permissions, RigArgs, SpawnArgs, TaskArgs};
use crate::commands::{self, branch_outcome, print, Env, Failure, Outcome, Prompt};
use crate::console::Console;
use crate::errors::Report;
use crate::json;
use crate::render::{self, Renderer};
use crate::rig::{Fallback, RigPlan};

pub struct Remote {
    pub client: Client,
    pub repo: Repo,
    /// The global flags that reach this server and repository, for a `by`
    /// this one starts (as `by watch`'s actions do).
    pub args: Vec<String>,
}

/// What a remote command with no token says, in the error style: the three
/// places one can be given, and where `by serve` writes one.
fn needs_token() -> Failure {
    Report::usage("remote mode needs a token")
        .next("pass --token-file FILE, or set BRANCHYARD_TOKEN_FILE")
        .next(
            "set token_file under [remote] in branchyard.toml (`by serve` writes one at \
             .branchyard/server/token on the server's host)",
        )
        .into()
}

/// What to do when a server does not answer; `by doctor`'s `server.reachable`
/// row says the same.
pub(crate) const UNREACHABLE_FIX: &str =
    "start a server with `by serve` on that host, or fix the URL, token or CA file ([remote] \
     url, token_file, ca_file)";

impl Remote {
    /// Connect as `globals` say, choosing the server's only repository when
    /// none is named.
    pub fn connect(globals: &Globals) -> Result<Remote, Failure> {
        let url = globals
            .remote
            .as_deref()
            .ok_or_else(|| Report::usage("no server URL").next("pass --remote URL"))?;
        // `ssh://`: a server this starts on the host, reached through a
        // forwarded Unix socket, with the token it fetched (docs/remote-ssh.md).
        let tunnel = match crate::ssh_remote::is_ssh(url) {
            true => {
                if globals.token_file.is_some() || globals.ca_file.is_some() {
                    return Err(Report::usage(
                        "an ssh:// remote fetches its own token and needs no CA file",
                    )
                    .next(
                        "drop --token-file and --ca-file (or unset BRANCHYARD_TOKEN_FILE and \
                         BRANCHYARD_CA_FILE)",
                    )
                    .into());
                }
                Some(crate::ssh_remote::connect(url)?)
            }
            false => None,
        };
        let tunnel_token = tunnel.as_ref().map(|t| t.token_file.display().to_string());
        let url = tunnel.as_ref().map_or(url, |t| t.url.as_str());
        let token_file = match &tunnel_token {
            Some(file) => file.as_str(),
            None => globals.token_file.as_deref().ok_or_else(needs_token)?,
        };
        let mut client = Client::from_token_file(url, token_file)?;
        if let Some(ca) = &globals.ca_file {
            client = client.with_ca_file(ca)?;
        }
        let name = match &globals.repo {
            Some(name) => name.clone(),
            None => {
                let repos = client.repos()?;
                match repos.as_slice() {
                    [only] => only.name.clone(),
                    [] => {
                        return Err(Report::new(
                            "config",
                            format!("the server at {url} serves no repositories"),
                        )
                        .next("start it with one: by serve --repo NAME=PATH (on its host)")
                        .into())
                    }
                    several => {
                        let names: Vec<&str> = several.iter().map(|r| r.name.as_str()).collect();
                        return Err(Report::usage(format!(
                            "the server serves {}: name one",
                            names.join(", ")
                        ))
                        .next(format!("pass --repo {}, or set BRANCHYARD_REPO", names[0]))
                        .next("set repo under [remote] in branchyard.toml")
                        .into());
                    }
                }
            }
        };
        let repo = client.repo(&name);
        let mut args = vec![
            "--remote".to_owned(),
            url.to_owned(),
            "--token-file".to_owned(),
            token_file.to_owned(),
            "--repo".to_owned(),
            name,
        ];
        if let Some(ca) = &globals.ca_file {
            args.extend(["--ca-file".to_owned(), ca.clone()]);
        }
        Ok(Remote { client, repo, args })
    }

    /// Where commands run, for messages.
    pub fn label(&self) -> String {
        format!("{} ({})", self.client.endpoint(), self.repo.name())
    }
}

pub const REMOTE_DENY_NOTICE: &str = "remote mode cannot ask, so tool permission requests will \
     be denied; pass --yes to allow them all";

/// The provider `--provider` names, as the server will see it. Paths and
/// `--pass-env` names are the server's: a Substrate key is not made
/// absolute here, since a relative path would name a file on this machine.
fn provider(task: &TaskArgs) -> Result<Option<Provider>, Failure> {
    if let Some(recipe) = &task.recipe {
        return Err(Report::usage(format!(
            "--provider recipe:{} runs on the machine that has the repository: a server does not \
             run environment recipes (docs/recipes.md)",
            recipe.name
        ))
        .next("run it without --remote, in the repository")
        .into());
    }
    let mut provider = commands::provider(task)?;
    if let (Some(Provider::Substrate(options)), Some(args)) = (&mut provider, &task.substrate) {
        options.key = args.key.clone().into();
        if !options.key.is_absolute() {
            return Err(Report::usage(format!(
                "--substrate-key names a file on the server in remote mode, not {}",
                args.key
            ))
            .next("give the key's absolute path on the server: --substrate-key /PATH")
            .into());
        }
    }
    Ok(provider)
}

/// The policy to send, and the notice to print once the server accepts
/// the work. `--ask` needs a terminal where the harness runs.
fn permissions(task: &TaskArgs) -> Result<(PolicySpec, Option<&'static str>), Failure> {
    match task.permissions {
        Permissions::Yes => Ok((PolicySpec::allow_all(), None)),
        Permissions::Ask => Err(Report::usage(
            "--ask is not available in remote mode, where the harness runs on the server",
        )
        .next("pass --yes, or --permissions read-only|edit-worktree, or leave requests denied")
        .into()),
        Permissions::Preset(preset) => Ok((PolicySpec::preset(preset), None)),
        Permissions::Unset => Ok((PolicySpec::default(), Some(REMOTE_DENY_NOTICE))),
    }
}

fn budget(task: &TaskArgs) -> BudgetSpec {
    BudgetSpec {
        max_usd: task.budget_usd,
        max_turns: task.max_turns,
        max_seconds: task.max_duration.map(|d| d.as_secs_f64()),
        stall_after_seconds: task.stall_after.map(|d| d.as_secs_f64()),
        stall_action: task.stall_after.map(|_| task.stall_action),
    }
}

/// Say where the server will run the work (`commands::banner`), then the
/// notice, if any. `isolated`: the request asks for a private HOME on the
/// server.
fn announce(remote: &Remote, notice: Option<&str>, provider: Option<&Provider>, isolated: bool) {
    eprintln!(
        "{}",
        commands::banner(provider, isolated, Some(&remote.label()))
    );
    if let Some(notice) = notice {
        eprintln!("by: {notice}");
    }
}

/// `by ls`'s line for the server's queued work whose branches do not exist
/// yet, which the table cannot show: empty when there is none, or when
/// the server does not say.
pub fn queued_line(remote: &Remote, infos: &[BranchInfo]) -> String {
    let Ok(operations) = remote.repo.operations(None) else {
        return String::new();
    };
    queued_names_line(&operations, infos)
}

/// [`queued_line`] from the server's queued and running operations.
fn queued_names_line(operations: &[Operation], infos: &[BranchInfo]) -> String {
    let names: Vec<&str> = operations
        .iter()
        .filter(|op| op.state == OperationState::Queued)
        .flat_map(|op| &op.branches)
        .filter(|name| !infos.iter().any(|info| &info.name == *name))
        .map(String::as_str)
        .collect();
    match names.as_slice() {
        [] => String::new(),
        [first, ..] => format!(
            "\nqueued on the server, not started yet: {} (by show {first} shows its operation)\n",
            names.join(", ")
        ),
    }
}

/// [`announce`] for work that continues `branch` on the server: a send, a
/// plan decision, a fork, a reincarnation or a child it delegates. Its
/// harness runs where `branch`'s does, with a private home when `branch`
/// has one ([`continuing_banner`]), as the local commands say
/// (`Live::start_for`). When the branch cannot be read, the flags alone.
fn announce_for(
    remote: &Remote,
    notice: Option<&str>,
    provider: Option<&Provider>,
    isolated: bool,
    branch: &str,
) {
    match remote.repo.branch(branch) {
        Ok(info) => {
            eprintln!(
                "{}",
                continuing_banner(&remote.label(), provider, isolated, &info)
            );
            if let Some(notice) = notice {
                eprintln!("by: {notice}");
            }
        }
        Err(_) => announce(remote, notice, provider, isolated),
    }
}

/// The banner of work that continues `info` on the server at `label`: the
/// provider the flags name, else the one the branch was started with, and
/// a private home when the flags ask for one or the branch has one.
fn continuing_banner(
    label: &str,
    provider: Option<&Provider>,
    isolated: bool,
    info: &BranchInfo,
) -> String {
    let isolated = isolated || info.isolated;
    match (provider, info.provider.as_str()) {
        // The record names a sandbox provider, not its options: say which.
        (None, recorded) if recorded != "local" => format!(
            "by: remote mode on {label}: harnesses run in the {recorded} sandbox the branch \
             was started in, on the server"
        ),
        (provider, _) => commands::banner(provider, isolated, Some(label)),
    }
}

/// Activity on stdout, or on stderr with `--json` so stdout holds only the
/// result.
fn live_console(env: &Env, prefixed: bool, json: bool) -> Arc<Console> {
    let out: Box<dyn io::Write + Send> = match json {
        true => Box::new(io::stderr()),
        false => Box::new(io::stdout()),
    };
    Arc::new(
        Console::new(
            Renderer::new(render::Style { color: env.color }, prefixed),
            out,
            Box::new(|_| Err(io::Error::other("remote mode does not ask"))),
        )
        .with_notifier(env.notifier()),
    )
}

fn failed(error: ErrorBody) -> Failure {
    Failure::Remote(branchyard_client::Error::Api {
        status: 0,
        error: Box::new(error),
    })
}

/// The engine error a server error stands for, for `--json` output: its
/// kind is the one a local `by` reports ([`crate::errors::local_kind`]).
pub fn sdk_error(error: branchyard_client::Error) -> branchyard::Error {
    match error {
        branchyard_client::Error::Api { error, .. } => {
            let kind = crate::errors::local_kind(&error.code, error.detail.as_ref());
            // A failed check's siblings or the branches on whose own work
            // it failed, every check of the integration, why a harness
            // cannot start and which profile routes no approvals, as a
            // local `by` reports them.
            let detail = error.detail.as_ref().and_then(|d| {
                let mut local = d
                    .get("shared")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                for key in [
                    "checks",
                    "own_work",
                    "harness",
                    "cause",
                    "unsupported",
                    "profile",
                ] {
                    if let Some(value) = d.get(key) {
                        local[key] = value.clone();
                    }
                }
                local
                    .as_object()
                    .is_some_and(|l| !l.is_empty())
                    .then(|| Box::new(local))
            });
            branchyard::Error::Remote {
                kind,
                message: error.message,
                detail,
            }
        }
        other => branchyard::Error::Remote {
            kind: match &other {
                branchyard_client::Error::Transport { .. } => "remote_unreachable",
                branchyard_client::Error::Config(_) => "config",
                _ => "remote_protocol",
            }
            .into(),
            message: other.to_string(),
            detail: None,
        },
    }
}

impl From<Failure> for branchyard::Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Sdk(error) => error,
            Failure::Remote(error) => sdk_error(error),
            Failure::Message(message) => branchyard::Error::Usage(message),
            Failure::Report(report) => branchyard::Error::Remote {
                kind: report.kind.clone(),
                message: report.said(),
                detail: report.detail.map(Box::new),
            },
            other => branchyard::Error::Remote {
                kind: "io".into(),
                message: other.to_string(),
                detail: None,
            },
        }
    }
}

/// The result of a finished operation, or its error.
fn result(op: Operation) -> Result<OperationResult, Failure> {
    match op.state {
        OperationState::Succeeded => Ok(op.result.unwrap_or_default()),
        _ => Err(failed(op.error.unwrap_or(ErrorBody {
            code: "unknown".into(),
            message: format!("operation {} ended {:?} without an error", op.id, op.state),
            detail: None,
        }))),
    }
}

/// `by log --follow` against a server: the branch's recorded events, then
/// each new one from the repository's event stream, until interrupted.
/// When the server goes away it says so once, reconnects after the last
/// entry it read, and says when it is back.
pub fn follow_log(remote: &Remote, branch: &str, out: &mut commands::Following<'_>) -> Outcome {
    let (stream, history, mut overlap) = log_and_stream(&remote.repo, branch)?;
    out.take(history)?;
    let (tx, rx) = mpsc::channel();
    let repo = remote.repo.clone();
    let name = branch.to_owned();
    std::thread::spawn(move || follow_branch(&repo, &name, stream, &tx));
    loop {
        match rx.recv_timeout(commands::FOLLOW_POLL) {
            Ok(Followed::Event(event)) if overlap.seen(&event) => {}
            Ok(Followed::Event(event)) => out.take(vec![*event])?,
            Ok(Followed::Lost(error)) => eprintln!("by: {error}; retrying"),
            Ok(Followed::Back) => eprintln!("by: reconnected to {}", remote.client.endpoint()),
            Ok(Followed::Refused(error)) => return Err(error.into()),
            // The events paused: held message text is complete.
            Err(RecvTimeoutError::Timeout) => out.take(Vec::new())?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// The branch's events so far, the repository's event stream opened where
/// they end, and the overlap: the branch's events read once the stream was
/// open, which are in the history and may also arrive on the stream.
///
/// The branch's events are read, the stream opened, and the branch's events
/// read again from where they ended, once: an event recorded between the
/// first read and the opening is caught by the second, and one recorded
/// after the opening comes on the stream as well, so the second read is the
/// overlap [`Overlap::seen`] drops. Reading again until nothing new came
/// would never end on a branch busier than one round trip.
fn log_and_stream(
    repo: &Repo,
    branch: &str,
) -> Result<
    (
        branchyard_client::EventStream,
        Vec<branchyard::RecordedEvent>,
        Overlap,
    ),
    Failure,
> {
    let page = repo.events(branch, 0)?;
    let mut events = page.events;
    let mut stream = repo.stream(None).max_failures(0);
    stream.open()?;
    let more = repo.events(branch, page.cursor)?.events;
    events.extend(more.iter().cloned());
    Ok((stream, events, Overlap(more.into())))
}

/// The branch's events read after the stream opened, not yet matched on it.
/// Those the stream also carries come first on it and in the same order, so
/// each stream event is looked for among them; one not found means the
/// stream has passed them all.
struct Overlap(std::collections::VecDeque<branchyard::RecordedEvent>);

impl Overlap {
    /// Whether `event`, from the stream, is one already shown.
    fn seen(&mut self, event: &branchyard::RecordedEvent) -> bool {
        match self.0.iter().position(|e| e == event) {
            Some(at) => {
                self.0.drain(..=at);
                true
            }
            None => {
                self.0.clear();
                false
            }
        }
    }
}

/// What the thread reading the stream for `by log --follow` passes on.
enum Followed {
    /// One of the branch's events.
    Event(Box<branchyard::RecordedEvent>),
    /// The server cannot be reached; retrying.
    Lost(branchyard_client::Error),
    /// Reconnected after [`Followed::Lost`].
    Back,
    /// The server refused the stream; retrying cannot help.
    Refused(branchyard_client::Error),
}

/// Pass `branch`'s entries from `stream` to `tx`, reconnecting after the
/// last entry read when the stream drops, until `tx`'s reader is gone.
fn follow_branch(
    repo: &Repo,
    branch: &str,
    mut stream: branchyard_client::EventStream,
    tx: &mpsc::Sender<Followed>,
) {
    let mut cursor = stream.cursor();
    loop {
        for item in stream.by_ref() {
            match item {
                Ok(entry) => {
                    cursor = Some(entry.seq);
                    if entry.branch != branch {
                        continue;
                    }
                    let event = branchyard::RecordedEvent {
                        at_ms: entry.at_ms,
                        activity: entry.activity,
                    };
                    if tx.send(Followed::Event(Box::new(event))).is_err() {
                        return;
                    }
                }
                // Dropped: reconnect below.
                Err(branchyard_client::Error::Transport { .. }) => break,
                Err(error) => {
                    branchyard_support::best_effort(
                        "report the refused stream",
                        tx.send(Followed::Refused(error)),
                    );
                    return;
                }
            }
        }
        let mut delays = branchyard_client::reconnect_backoff();
        let mut lost = false;
        stream = loop {
            let mut next = repo.stream(cursor).max_failures(0);
            match next.open() {
                Ok(_) => {
                    if lost && tx.send(Followed::Back).is_err() {
                        return;
                    }
                    break next;
                }
                // A restarting server: keep following and try again.
                Err(error @ branchyard_client::Error::Transport { .. }) => {
                    if !lost {
                        lost = true;
                        if tx.send(Followed::Lost(error)).is_err() {
                            return;
                        }
                    }
                    std::thread::sleep(delays.next().unwrap_or(Duration::from_secs(5)));
                }
                Err(error) => {
                    branchyard_support::best_effort(
                        "report the refused stream",
                        tx.send(Followed::Refused(error)),
                    );
                    return;
                }
            }
        };
    }
}

/// Render the operation's activity as it streams, until the operation has
/// finished and every entry up to its end cursor has been shown.
fn follow(remote: &Remote, op: &Operation, console: &Console) -> Result<Operation, Failure> {
    follow_prefixed(remote, op, console, None)
}

/// What [`follow_prefixed`] hears: the stream's entries and its end, and
/// the operation as each long poll of it answers.
enum Heard {
    Entry(Box<Result<FeedEntry, branchyard_client::Error>>),
    StreamEnded,
    Operation(Box<Result<Operation, branchyard_client::Error>>),
    /// The long poll failed in a way worth retrying ([`transient`]): the
    /// server is restarting, or briefly unreachable. Said once.
    Lost(Box<branchyard_client::Error>),
    /// It answers again after [`Heard::Lost`].
    Back,
}

/// How long one long poll of a followed operation lasts: the operation
/// finishing answers it at once; a queued one's reason for waiting
/// (`waiting`) is shown within this.
const FOLLOW_POLL: Duration = Duration::from_secs(5);

/// Whether a failed request is worth asking again: the server could not be
/// reached (a restart, a network blip), or it answered that it is stopping
/// or that a proxy in front of it could not reach it. The operations it
/// holds are in its store, and go on, or wait in its queue, regardless.
fn transient(error: &branchyard_client::Error) -> bool {
    match error {
        branchyard_client::Error::Transport { .. } => true,
        branchyard_client::Error::Api { status, .. } => matches!(status, 502..=504),
        _ => false,
    }
}

/// What a followed command says, once, when its server stops answering:
/// why, that the work is the server's, and what interrupting does.
fn lost_line(error: &branchyard_client::Error, id: &str) -> String {
    format!(
        "{error}; the server keeps operation {id}, and this waits for it to answer again \
         (Ctrl-C stops watching, not the work)"
    )
}

/// Long-poll operation `id` on `client` and pass each answer to `tx`, until
/// it has finished or `tx`'s reader is gone. While the server does not
/// answer ([`transient`]) it keeps asking, with the event stream's backoff,
/// and says so once ([`Heard::Lost`]) and again when it answers
/// ([`Heard::Back`]): a restarted server answers for the operations it held
/// before, from its store.
fn poll_operation(client: &Client, id: &str, tx: &mpsc::Sender<Heard>) {
    let mut delays: Option<Box<dyn Iterator<Item = Duration>>> = None;
    loop {
        match client.wait_timeout(id, FOLLOW_POLL) {
            Err(error) if transient(&error) => {
                let delays = match &mut delays {
                    Some(delays) => delays,
                    None => {
                        if tx.send(Heard::Lost(Box::new(error))).is_err() {
                            return;
                        }
                        delays.insert(Box::new(branchyard_client::reconnect_backoff()))
                    }
                };
                std::thread::sleep(delays.next().unwrap_or(Duration::from_secs(5)));
            }
            answer => {
                if delays.take().is_some() && tx.send(Heard::Back).is_err() {
                    return;
                }
                let last = answer.as_ref().map_or(true, |op| op.state.is_terminal());
                if tx.send(Heard::Operation(Box::new(answer))).is_err() || last {
                    return;
                }
            }
        }
    }
}

/// [`follow`], also showing every branch whose name starts with `prefix`:
/// a map's, which its operation cannot name when it is accepted.
fn follow_prefixed(
    remote: &Remote,
    op: &Operation,
    console: &Console,
    prefix: Option<&str>,
) -> Result<Operation, Failure> {
    let mut branches: HashSet<String> = op.branches.iter().cloned().collect();
    let (tx, rx) = mpsc::channel();
    // Reconnected by cursor for as long as the operation is followed, a
    // server restart included: the loop below ends when the operation has
    // finished and the stream has reached its end cursor.
    let stream = remote.repo.stream(Some(op.cursor)).max_failures(u32::MAX);
    let entries = tx.clone();
    std::thread::spawn(move || {
        for item in stream {
            if entries.send(Heard::Entry(Box::new(item))).is_err() {
                return;
            }
        }
        branchyard_support::best_effort(
            "say the event stream ended",
            entries.send(Heard::StreamEnded),
        );
    });
    // The operation's state by long polls, not by sleeping between reads.
    let (client, id) = (remote.client.clone(), op.id.clone());
    std::thread::spawn(move || poll_operation(&client, &id, &tx));
    let mut seen = op.cursor;
    let mut finished: Option<Operation> = None;
    let mut streaming = true;
    let mut said_waiting = false;
    loop {
        if let Some(done) = &finished {
            let drained = done.end_cursor.is_none_or(|end| seen >= end);
            if drained || !streaming {
                return Ok(done.clone());
            }
        }
        let Ok(heard) = rx.recv() else {
            return Err(Report::new(
                "remote_protocol",
                format!("lost track of operation {} on the server", op.id),
            )
            .next(format!(
                "by ls (the server goes on with operation {})",
                op.id
            ))
            .into());
        };
        match heard {
            Heard::Entry(item) if streaming => match *item {
                Ok(entry) => {
                    seen = entry.seq;
                    // A child a followed branch spawned is followed too, as
                    // a local run shows its children.
                    if let Activity::Delegation {
                        tool,
                        branch,
                        refused: false,
                        ..
                    } = &entry.activity
                    {
                        if tool == "spawn" && branches.contains(&entry.branch) {
                            branches.insert(branch.clone());
                        }
                    }
                    let prefixed = prefix.is_some_and(|p| entry.branch.starts_with(p));
                    // A fork at a checkpoint is recorded, not observed, by
                    // a local run, which shows its warning instead: so here.
                    let recorded_only = matches!(entry.activity, Activity::ForkedAt { .. });
                    if !recorded_only && (prefixed || branches.contains(&entry.branch)) {
                        console.event(&BranchEvent {
                            branch: entry.branch,
                            activity: entry.activity,
                        });
                    }
                }
                Err(error) => {
                    console.finish();
                    eprintln!("by: lost the event stream ({error}); waiting for the result");
                    streaming = false;
                }
            },
            Heard::Entry(_) => {}
            Heard::StreamEnded => streaming = false,
            Heard::Lost(error) => {
                console.finish();
                eprintln!("by: {}", lost_line(&error, &op.id));
            }
            Heard::Back => eprintln!("by: {} answers again", remote.client.endpoint()),
            Heard::Operation(op) => {
                let op = (*op)?;
                if let (Some(waiting), false) = (&op.waiting, said_waiting) {
                    eprintln!("by: {} is still queued: {waiting}", op.id);
                    said_waiting = true;
                }
                if op.state.is_terminal() {
                    finished = Some(op);
                }
            }
        }
    }
}

/// The provisioning flags for the server. `--instructions` is read here
/// and sent as text; a secret is sent by name only, because the server's
/// operator decides where each comes from.
fn provision(task: &TaskArgs) -> Result<Option<branchyard::Provisioning>, Failure> {
    let spec = commands::provision(task)?;
    if let Some(secret) = spec
        .iter()
        .flat_map(|s| &s.secrets)
        .find(|s| s.from.is_some())
    {
        return Err(Report::usage(format!(
            "--secret {name}=...: with --remote, secrets come from the server, whose operator \
             decides where each is read",
            name = secret.name
        ))
        .next(format!("pass --secret {name}", name = secret.name))
        .into());
    }
    Ok(spec)
}

pub fn run(env: &Env, remote: &Remote, prompt: &str, task: &TaskArgs) -> Outcome {
    let (request, notice, provider) = run_request(prompt, task)?;
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice, provider.as_ref(), task.isolated);
    finish_one(env, remote, &op, task.delegate.is_some())
}

/// [`run`] with `--json`: the activity goes to stderr, and the branch the
/// server's operation ended with is returned for the caller to print.
pub fn run_json(
    env: &Env,
    remote: &Remote,
    prompt: &str,
    task: &TaskArgs,
) -> Result<BranchInfo, Failure> {
    let (request, notice, provider) = run_request(prompt, task)?;
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice, provider.as_ref(), task.isolated);
    first_branch(follow_result(env, remote, &op)?)
}

/// Run a contracted native answer and report its current accepted output.
pub fn run_with_result(
    env: &Env,
    remote: &Remote,
    prompt: &str,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let task = admit_result(task)?;
    let info = run_json(env, remote, prompt, &task)?;
    finish_results(env, remote, &[info], json, true)
}

/// Run independent contracted coworkers and expose output completeness per branch.
pub fn fan_with_result(
    env: &Env,
    remote: &Remote,
    prompt: &str,
    harnesses: &[String],
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let task = admit_result(task)?;
    let infos = fan_json(env, remote, prompt, harnesses, &task)?;
    finish_results(env, remote, &infos, json, false)
}

fn admit_result(task: &TaskArgs) -> Result<TaskArgs, Failure> {
    let contract = commands::result_for_task(task)?;
    admit_result_checks(contract.as_ref(), task.check.is_some())?;
    let mut task = task.clone();
    if let Some(contract) = contract {
        task.result_schema = Some(contract.schema.to_string());
        task.result_schema_file = None;
    }
    Ok(task)
}

fn admit_result_checks(contract: Option<&branchyard::ResultContract>, has_check: bool) -> Outcome {
    if contract.is_some_and(|contract| contract.is_deliverable()) && has_check {
        return Err(Report::new("unsupported", "remote deliverable checks cannot be verified by this client")
            .next("run the coordinator on the server host, or use a remote compute provider from a local coordinator")
            .into());
    }
    Ok(())
}

fn result_next(remote: &Remote, branch: &str) -> String {
    let globals = remote
        .args
        .iter()
        .map(|word| crate::args::shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "by {globals} inspect {} --json",
        crate::args::shell_quote(branch)
    )
}

fn finish_results(
    env: &Env,
    remote: &Remote,
    infos: &[BranchInfo],
    as_json: bool,
    single: bool,
) -> Outcome {
    let infos = infos
        .iter()
        .map(|info| remote.repo.branch(&info.name))
        .collect::<Result<Vec<_>, _>>()?;
    let attempts = crate::attempts::remote_attempts_for_infos(&remote.repo, &infos)?;
    let views: Vec<_> = infos
        .iter()
        .zip(attempts)
        .map(|(info, attempt)| commands::ResultView::from_attempt(info, attempt))
        .collect();
    let failed = !infos.iter().zip(&views).any(|(info, view)| match view {
        Some(view) => view.outcome(info).is_ok(),
        None => branch_outcome(info).is_ok(),
    });
    if as_json {
        let values: Vec<_> = infos
            .iter()
            .zip(&views)
            .map(|(info, view)| {
                view.as_ref()
                    .map_or_else(|| json::branch(info), |view| view.json(info))
            })
            .collect();
        let value = if single {
            values
                .into_iter()
                .next()
                .ok_or_else(|| Report::new("internal", "result task returned no branch"))?
        } else {
            serde_json::json!({"branches": values})
        };
        print(&json::text(&value))?;
    } else {
        let refs: Vec<_> = infos.iter().collect();
        match commands::fan_summary(env, &refs) {
            Ok(()) => {}
            Err(Failure::Reported) => {}
            Err(error) => return Err(error),
        }
        for (info, view) in infos.iter().zip(&views) {
            if let Some(view) = view {
                let next = result_next(remote, &info.name);
                print(&view.text(Some(&next)))?;
            }
        }
    }
    if failed {
        Err(Failure::Reported)
    } else {
        Ok(())
    }
}

/// The first branch an operation ended with.
fn first_branch(result: OperationResult) -> Result<BranchInfo, Failure> {
    result
        .branches
        .into_iter()
        .next()
        .ok_or_else(|| missing("branch"))
}

/// `by run`'s task request, with the notice and provider it announces.
fn run_request(
    prompt: &str,
    task: &TaskArgs,
) -> Result<(TaskRequest, Option<&'static str>, Option<Provider>), Failure> {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = TaskRequest {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        harnesses: Vec::new(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        seats: None,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
        plan: task.plan,
        goal: goal_request(task),
        result: crate::commands::result_for_task(task)?,
        deny: task.deny.clone(),
    };
    Ok((request, notice, provider))
}

/// `--goal` and its options as the server takes them: the judge is one of
/// the server's harnesses, with the server's command.
fn goal_request(task: &TaskArgs) -> Option<branchyard_client::api::GoalRequest> {
    task.goal
        .as_ref()
        .map(|text| branchyard_client::api::GoalRequest {
            text: text.clone(),
            rounds: task.goal_rounds,
            judge: task.goal_judge.clone(),
        })
}

/// `by plan approve` on a server: an operation running the branch's next
/// turn with the plan, followed like a send.
pub fn plan_approve(
    env: &Env,
    remote: &Remote,
    branch: &str,
    edited: Option<&str>,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let (send, notice) = send_request(task, Prompt::Text("plan approval"))?;
    let request = branchyard_client::knowledge_api::PlanApproveRequest {
        edited: edited.map(str::to_owned),
        send,
    };
    let op = remote.repo.approve_plan(branch, &request, &new_key())?;
    announce_for(remote, notice, None, false, branch);
    plan_finish(env, remote, &op, json)
}

/// `by plan reject` on a server.
pub fn plan_reject(
    env: &Env,
    remote: &Remote,
    branch: &str,
    reason: Option<&str>,
    replan: bool,
    task: &TaskArgs,
    json: bool,
) -> Outcome {
    let (send, notice) = send_request(task, Prompt::Text("plan rejection"))?;
    let request = branchyard_client::knowledge_api::PlanRejectRequest {
        reason: reason.map(str::to_owned),
        replan,
        send,
    };
    let op = remote.repo.reject_plan(branch, &request, &new_key())?;
    announce_for(remote, notice, None, false, branch);
    plan_finish(env, remote, &op, json)
}

fn plan_finish(env: &Env, remote: &Remote, op: &Operation, json: bool) -> Outcome {
    if !json {
        return finish_one(env, remote, op, false);
    }
    let sent = follow_result(env, remote, op).and_then(|result| {
        result
            .branches
            .into_iter()
            .next()
            .map(|info| Sent {
                name: info.name,
                status: info.status,
            })
            .ok_or_else(|| branchyard::Error::State("the server returned no branch".into()))
    });
    commands::emit(true, sent, |_| String::new())
}

/// Follow an operation that runs one branch, then print its summary and
/// the branches it delegated to.
fn finish_one(env: &Env, remote: &Remote, op: &Operation, prefixed: bool) -> Outcome {
    let console = live_console(env, prefixed, false);
    let done = follow(remote, op, &console);
    console.finish();
    finish_result(env, remote, result(done?)?)
}

/// Print a finished operation's branch summary, and the branches it
/// delegated to, as local mode does after a turn.
fn finish_result(env: &Env, remote: &Remote, result: OperationResult) -> Outcome {
    let info = result
        .branches
        .into_iter()
        .next()
        .ok_or_else(|| missing("branch"))?;
    let style = render::Style { color: env.color };
    let check = inspect(remote, &info.name).ok().and_then(|i| i.check);
    print(&format!(
        "\n{}",
        render::summary_with_check(&info, check.as_deref(), style)
    ))?;
    if !result.descendants.is_empty() {
        let infos: Vec<&BranchInfo> = result.descendants.iter().collect();
        print(&format!(
            "\ndelegated\n{}",
            render::comparison_table(&infos, style)
        ))?;
    }
    branch_outcome(&info)
}

pub fn fan(
    env: &Env,
    remote: &Remote,
    prompt: &str,
    harnesses: &[String],
    task: &TaskArgs,
) -> Outcome {
    let (request, notice, provider) = fan_request(prompt, harnesses, task)?;
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice, provider.as_ref(), task.isolated);
    let console = live_console(env, true, false);
    console.reserve(&op.branches);
    let done = follow(remote, &op, &console);
    console.finish();
    let result = result(done?)?;
    let infos: Vec<&BranchInfo> = result
        .branches
        .iter()
        .chain(result.descendants.iter())
        .collect();
    commands::fan_summary(env, &infos)
}

/// [`fan`] with `--json`: the activity goes to stderr, and the branches (and
/// those they delegated to) are returned for the caller to print.
pub fn fan_json(
    env: &Env,
    remote: &Remote,
    prompt: &str,
    harnesses: &[String],
    task: &TaskArgs,
) -> Result<Vec<BranchInfo>, Failure> {
    let (request, notice, provider) = fan_request(prompt, harnesses, task)?;
    let op = remote.repo.submit_task(&request, &new_key())?;
    announce(remote, notice, provider.as_ref(), task.isolated);
    let console = live_console(env, true, true);
    console.reserve(&op.branches);
    let done = follow(remote, &op, &console);
    console.finish();
    let result = result(done?)?;
    Ok(result
        .branches
        .into_iter()
        .chain(result.descendants)
        .collect())
}

/// `by fan`'s task request, with the notice and provider it announces.
fn fan_request(
    prompt: &str,
    harnesses: &[String],
    task: &TaskArgs,
) -> Result<(TaskRequest, Option<&'static str>, Option<Provider>), Failure> {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = TaskRequest {
        prompt: prompt.to_owned(),
        harness: None,
        harnesses: harnesses.to_vec(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        seats: None,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
        plan: task.plan,
        goal: goal_request(task),
        result: crate::commands::result_for_task(task)?,
        deny: task.deny.clone(),
    };
    Ok((request, notice, provider))
}

/// `by map --remote`: the items and schema are read here, the map runs as
/// one operation on the server, and `--out` and `--reduce-out` are
/// written here from its result.
pub fn map(env: &Env, remote: &Remote, args: &crate::args::MapArgs, json: bool) -> Outcome {
    let task: &TaskArgs = &args.task;
    if crate::fleet_cmd::is_routed(task) || task.kind.is_some() {
        return Err(crate::fleet_cmd::local_only());
    }
    let cwd = std::env::current_dir()?;
    let items = crate::map_cmd::read_items(args, &cwd)?;
    let schema = crate::map_cmd::read_schema(args)?;
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let prompt = args.prompt.clone().unwrap_or_default();
    let request = MapRequest {
        name: task.name.clone(),
        items,
        schema,
        concurrency: args.concurrency,
        retries: args.retries,
        total_usd: args.total_usd,
        reduce: args.reduce.clone(),
        remove_done: args.rm,
        retry_failed: args.retry_failed,
        task: TaskRequest {
            prompt: prompt.clone(),
            harness: task.harness.clone(),
            harnesses: Vec::new(),
            name: None,
            base: task.base.clone(),
            budget: budget(task),
            policy,
            check: task.check.clone(),
            isolated: task.isolated,
            command: task.command.clone(),
            delegation: None,
            allow_delegation: false,
            unapproved_tools: task.unapproved_tools,
            provider: provider.clone(),
            provision: provision(task)?,
            seats: None,
            require_labels: task.require_labels.clone(),
            priority: task.priority,
            plan: false,
            goal: None,
            result: crate::commands::result_for_task(task)?,
            deny: Vec::new(),
        },
    };
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| branchyard::map_default_name(&prompt));
    let op = remote.repo.submit_map(&request, &new_key())?;
    announce(remote, notice, provider.as_ref(), task.isolated);
    eprintln!(
        "by: map {name}: {} item(s), running on the server as {}",
        request.items.len(),
        op.id
    );
    map_finish(env, remote, &op, &name, args, json)
}

/// `by map resume --remote`: the server runs the map again with the
/// request that started it.
pub fn map_resume(
    env: &Env,
    remote: &Remote,
    name: &str,
    retry_failed: bool,
    json: bool,
) -> Outcome {
    let op = remote.repo.resume_map(
        name,
        &branchyard_client::api::MapResumeRequest { retry_failed },
        &new_key(),
    )?;
    eprintln!("by: map {name}: resumed on the server as {}", op.id);
    let none = crate::args::MapArgs {
        prompt: None,
        items: None,
        from_command: None,
        input_format: None,
        schema: None,
        out: None,
        concurrency: None,
        retries: None,
        total_usd: None,
        reduce: None,
        reduce_out: None,
        rm: false,
        retry_failed,
        task: crate::args::Checked::new(TaskArgs::default()),
    };
    map_finish(env, remote, &op, name, &none, json)
}

fn map_finish(
    env: &Env,
    remote: &Remote,
    op: &Operation,
    name: &str,
    args: &crate::args::MapArgs,
    json: bool,
) -> Outcome {
    let console = live_console(env, true, json);
    let done = follow_prefixed(remote, op, &console, Some(&format!("{name}-")));
    console.finish();
    let report = result(done?)?.map.ok_or_else(|| missing("map"))?;
    let out = args.out.as_ref().map(std::path::PathBuf::from);
    if let Some(out) = &out {
        crate::map_cmd::write_atomic(out, &crate::map_cmd::results_text(out, &report))?;
    }
    if let (Some(path), Some(text)) = (
        &args.reduce_out,
        report.reduce.as_ref().and_then(|r| r.text.as_ref()),
    ) {
        crate::map_cmd::write_atomic(
            std::path::Path::new(path),
            &format!("{}\n", text.trim_end()),
        )?;
    }
    crate::map_cmd::finish(env, &report, out.as_deref(), json)
}

fn send_request(
    task: &TaskArgs,
    prompt: Prompt<'_>,
) -> Result<(SendRequest, Option<&'static str>), Failure> {
    let (policy, notice) = permissions(task)?;
    Ok((
        SendRequest {
            prompt: match prompt {
                Prompt::Text(text) => text.to_owned(),
                Prompt::Retry => String::new(),
            },
            retry: matches!(prompt, Prompt::Retry),
            budget: budget(task),
            policy,
            check: task.check.clone(),
            command: task.command.clone(),
            delegation: task.delegate.map(Envelope::depth),
            allow_delegation: task.allow_delegation,
            unapproved_tools: task.unapproved_tools,
            provision: provision(task)?,
            require_labels: task.require_labels.clone(),
            priority: task.priority,
            feedback: None,
        },
        notice,
    ))
}

/// `by --remote send`: `prompt`, or with `--retry` the cut-off turn's,
/// which the server reads.
pub fn send(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: Prompt<'_>,
    task: &TaskArgs,
) -> Outcome {
    let (request, notice) = send_request(task, prompt)?;
    let op = remote.repo.send(branch, &request, &new_key())?;
    // A `by review --detach` launcher waits for the server to accept it.
    crate::detach::accepted(branch);
    announce_for(remote, notice, None, false, branch);
    let delegating = task.delegate.is_some() || !remote.repo.branch(branch)?.children.is_empty();
    finish_one(env, remote, &op, delegating)
}

/// `by --remote review`: send `prompt` with `feedback`, which the server
/// records as data beside it.
pub fn send_feedback(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: &str,
    task: &TaskArgs,
    feedback: branchyard::Feedback,
) -> Outcome {
    let (mut request, notice) = send_request(task, Prompt::Text(prompt))?;
    request.feedback = Some(feedback);
    let op = remote.repo.send(branch, &request, &new_key())?;
    announce_for(remote, notice, None, false, branch);
    let delegating = task.delegate.is_some() || !remote.repo.branch(branch)?.children.is_empty();
    finish_one(env, remote, &op, delegating)
}

/// `send --json`: activity on stderr, then the branch's name and status,
/// as local mode prints them once the turn and its subtree have ended.
pub fn send_json(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: Prompt<'_>,
    task: &TaskArgs,
) -> Result<Sent, branchyard::Error> {
    let (request, notice) = send_request(task, prompt)?;
    let op = remote
        .repo
        .send(branch, &request, &new_key())
        .map_err(sdk_error)?;
    crate::detach::accepted(branch);
    announce_for(remote, notice, None, false, branch);
    let info = follow_result(env, remote, &op)?
        .branches
        .into_iter()
        .next()
        .ok_or_else(|| missing("branch"))?;
    Ok(Sent {
        name: info.name,
        status: info.status,
    })
}

/// Follow an operation with activity on stderr, and return its result or
/// its error as the engine's.
fn follow_result(
    env: &Env,
    remote: &Remote,
    op: &Operation,
) -> Result<OperationResult, branchyard::Error> {
    let console = live_console(env, true, true);
    let done = follow(remote, op, &console);
    console.finish();
    Ok(result(done?)?)
}

pub fn fork(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: &str,
    fresh_session: bool,
    task: &TaskArgs,
) -> Outcome {
    let (request, notice) = fork_request(prompt, fresh_session, None, task)?;
    let op = remote.repo.fork(branch, &request, &new_key())?;
    announce_for(
        remote,
        notice,
        request.provider.as_ref(),
        task.isolated,
        branch,
    );
    finish_one(env, remote, &op, task.delegate.is_some())
}

/// [`fork`] or [`fork_at`] with `--json`: the activity goes to stderr, and
/// the new branch is returned for the caller to print.
pub fn fork_json(
    env: &Env,
    remote: &Remote,
    branch: &str,
    prompt: &str,
    fresh_session: bool,
    at: Option<u32>,
    task: &TaskArgs,
) -> Result<BranchInfo, Failure> {
    let (request, notice) = fork_request(prompt, fresh_session, at, task)?;
    let op = remote.repo.fork(branch, &request, &new_key())?;
    announce_for(
        remote,
        notice,
        request.provider.as_ref(),
        task.isolated,
        branch,
    );
    let info = first_branch(follow_result(env, remote, &op)?)?;
    if at.is_some() {
        if let Ok(events) = remote.repo.events(&info.name, 0) {
            crate::attempts::announce_forked_at(&info.name, &events.events);
        }
    }
    Ok(info)
}

/// `by --remote fork BRANCH --at N`: the server forks at the checkpoint,
/// and this says how the new branch's session continues, as the local
/// command does, before its summary.
pub fn fork_at(
    env: &Env,
    remote: &Remote,
    branch: &str,
    turn: u32,
    prompt: &str,
    task: &TaskArgs,
) -> Outcome {
    let (request, notice) = fork_request(prompt, false, Some(turn), task)?;
    let op = remote.repo.fork(branch, &request, &new_key())?;
    announce_for(
        remote,
        notice,
        request.provider.as_ref(),
        task.isolated,
        branch,
    );
    let console = live_console(env, task.delegate.is_some(), false);
    let done = follow(remote, &op, &console);
    console.finish();
    let result = result(done?)?;
    if let Some(info) = result.branches.first() {
        if let Ok(events) = remote.repo.events(&info.name, 0) {
            crate::attempts::announce_forked_at(&info.name, &events.events);
        }
    }
    finish_result(env, remote, result)
}

/// A fork's request, from `by fork`'s flags.
fn fork_request(
    prompt: &str,
    fresh_session: bool,
    at: Option<u32>,
    task: &TaskArgs,
) -> Result<(ForkRequest, Option<&'static str>), Failure> {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = ForkRequest {
        prompt: prompt.to_owned(),
        name: task.name.clone(),
        fresh_session,
        at,
        harness: task.harness.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider,
        provision: provision(task)?,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
    };
    Ok((request, notice))
}

pub fn reincarnate(env: &Env, remote: &Remote, branch: &str, task: &TaskArgs) -> Outcome {
    let (policy, notice) = permissions(task)?;
    let provider = provider(task)?;
    let request = ReincarnateRequest {
        name: task.name.clone(),
        harness: task.harness.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        isolated: task.isolated,
        command: task.command.clone(),
        delegation: task.delegate.map(Envelope::depth),
        allow_delegation: task.allow_delegation,
        unapproved_tools: task.unapproved_tools,
        provider: provider.clone(),
        provision: provision(task)?,
        require_labels: task.require_labels.clone(),
        priority: task.priority,
    };
    let op = remote.repo.reincarnate(branch, &request, &new_key())?;
    announce_for(remote, notice, provider.as_ref(), task.isolated, branch);
    finish_one(env, remote, &op, task.delegate.is_some())
}

/// `spawn --parent` on the server: the child runs there, and this waits
/// for it as the local command does. Activity goes to stderr with
/// `--json`.
pub fn spawn(
    env: &Env,
    remote: &Remote,
    parent: &str,
    prompt: &str,
    args: &SpawnArgs,
    contract: Option<branchyard::ResultContract>,
) -> Result<Inspection, branchyard::Error> {
    let task = &args.task;
    let (policy, notice) = permissions(task)?;
    let request = SpawnRequest {
        prompt: prompt.to_owned(),
        harness: task.harness.clone(),
        name: task.name.clone(),
        base: task.base.clone(),
        budget: budget(task),
        policy,
        check: task.check.clone(),
        max_depth: args.max_depth,
        deny: args.deny.clone(),
        unapproved_tools: task.unapproved_tools,
        seat: args.seat.clone(),
        depends_on: args.depends_on.clone(),
        after: args.after,
        bindings: args.bindings.clone(),
        require_labels: task.require_labels.clone(),
        priority: task.priority,
        connectors: (!args.connectors.is_empty()).then(|| args.connectors.clone()),
        max_children: args.max_children,
        harnesses: args.harnesses.clone(),
        plan: args.plan,
        model: args.model.clone(),
        result: contract,
        why: args.why.clone(),
        owns: args.owns.clone(),
        land: args.land,
        review: args.review.as_deref().cloned(),
    };
    let op = remote
        .repo
        .spawn(parent, &request, &new_key())
        .map_err(sdk_error)?;
    announce_for(remote, notice, None, false, parent);
    let console = live_console(env, true, args.json);
    let done = follow(remote, &op, &console);
    console.finish();
    result(done?)?
        .inspection
        .ok_or_else(|| branchyard::Error::State("the server returned no inspection".into()))
}

/// `graph apply --parent` on the server: the proposal commits there, and
/// its children run there.
pub fn apply_graph(
    remote: &Remote,
    parent: &str,
    proposal: branchyard::GraphProposal,
    task: &TaskArgs,
) -> Result<branchyard::GraphApplied, branchyard::Error> {
    let (policy, notice) = permissions(task)?;
    let request = GraphRequest {
        expected_revision: proposal.expected_revision,
        edits: proposal.edits,
        policy,
        unapproved_tools: task.unapproved_tools,
    };
    let applied = remote
        .repo
        .apply_graph(parent, &request)
        .map_err(sdk_error)?;
    announce(remote, notice, None, false);
    Ok(applied)
}

/// `integrate` on the server: merge a delegated child into its parent.
pub fn integrate(remote: &Remote, branch: &str) -> Result<Merged, branchyard::Error> {
    let op = remote
        .repo
        .integrate(branch, &new_key())
        .map_err(sdk_error)?;
    let done = remote.client.wait(&op.id).map_err(sdk_error)?;
    result(done)?
        .merged
        .ok_or_else(|| branchyard::Error::State("the server returned no merge".into()))
}

/// `integrate a b c` on the server: several children of one parent,
/// together.
pub fn integrate_all(
    remote: &Remote,
    branches: &[&str],
) -> Result<branchyard::MergedAll, branchyard::Error> {
    let op = remote
        .repo
        .integrate_all(branches, &new_key())
        .map_err(sdk_error)?;
    let done = remote.client.wait(&op.id).map_err(sdk_error)?;
    result(done)?
        .merged_all
        .ok_or_else(|| branchyard::Error::State("the server returned no merge".into()))
}

/// `check` on the server: the branch's check on its current work, as
/// integrating it would run it. The check can run for as long as an
/// integration's, so the command waits for the operation; interrupting it
/// stops waiting, not the check.
pub fn check(remote: &Remote, branch: &str) -> Result<branchyard::CheckReport, branchyard::Error> {
    let op = remote.repo.check(branch, &new_key()).map_err(sdk_error)?;
    let done = remote.client.wait(&op.id).map_err(sdk_error)?;
    result(done)?
        .check_report
        .ok_or_else(|| branchyard::Error::State("the server returned no check report".into()))
}

pub fn inspect(remote: &Remote, branch: &str) -> Result<Inspection, branchyard::Error> {
    remote.repo.inspect(branch).map_err(sdk_error)
}

pub fn events(
    remote: &Remote,
    branch: &str,
    cursor: Option<usize>,
    limit: usize,
) -> Result<EventPage, branchyard::Error> {
    remote
        .repo
        .event_page(branch, cursor, limit)
        .map_err(sdk_error)
}

pub fn children(remote: &Remote, branch: &str) -> Result<Children, branchyard::Error> {
    remote.repo.children(branch).map_err(sdk_error)
}

pub fn merge(
    remote: &Remote,
    branch: &str,
    into: Option<&str>,
) -> Result<branchyard::Merged, Failure> {
    let request = MergeRequest {
        target: into.map(str::to_owned),
    };
    let op = match remote.repo.merge(branch, &request, &new_key()) {
        Err(error) if error.code() == Some("detached_head") => {
            return Err(commands::detached_head().into())
        }
        other => other?,
    };
    let done = remote.client.wait(&op.id)?;
    result(done)?.merged.ok_or_else(|| missing("merge"))
}

/// `by rig run` on the server: the lowered plan as one task request, with
/// its seats. Needs a server that allows delegation, since a rig's root
/// delegates.
pub fn rig(env: &Env, remote: &Remote, plan: &RigPlan, prompt: &str, args: &RigArgs) -> Outcome {
    let root = &plan.root;
    admit_result_checks(root.result.as_ref(), root.check.is_some())?;
    let (mode, notice) = match root.policy.default {
        Fallback::Allow => (PolicyMode::Allow, None),
        Fallback::Deny => (PolicyMode::Deny, Some(REMOTE_DENY_NOTICE)),
        Fallback::Ask => {
            let error = branchyard::Error::Unsupported(
                "policy.default = \"ask\" is not available in remote mode, where the harness \
                 runs on the server; use allow or deny"
                    .into(),
            );
            return rig_failed(args.json, error);
        }
    };
    let rules = root
        .policy
        .deny
        .iter()
        .map(|tool| RuleSpec {
            tool: tool.clone(),
            allow: false,
        })
        .chain(root.policy.allow.iter().map(|tool| RuleSpec {
            tool: tool.clone(),
            allow: true,
        }))
        .collect();
    let request = TaskRequest {
        prompt: prompt.to_owned(),
        harness: Some(root.harness.clone()),
        harnesses: Vec::new(),
        name: Some(args.name.clone().unwrap_or_else(|| root.name.clone())),
        base: args.base.clone(),
        budget: BudgetSpec {
            max_usd: root.budget.max_usd,
            max_turns: root.budget.max_turns,
            max_seconds: root.budget.max_minutes.map(|m| m * 60.0),
            ..BudgetSpec::default()
        },
        policy: PolicySpec {
            mode,
            rules,
            preset: None,
        },
        check: root.check.clone(),
        isolated: root.isolated,
        command: args.command.clone(),
        delegation: root.delegation.clone(),
        allow_delegation: root.policy.delegation_commands,
        unapproved_tools: args.unapproved_tools,
        provider: None,
        provision: Some(root.provision.clone()),
        seats: plan.seats.clone(),
        require_labels: Vec::new(),
        priority: None,
        plan: false,
        goal: None,
        result: root.result.clone(),
        deny: Vec::new(),
    };
    let op = match remote.repo.submit_task(&request, &new_key()) {
        Ok(op) => op,
        Err(error) if args.json => return rig_failed(true, sdk_error(error)),
        Err(error) => return Err(error.into()),
    };
    announce(remote, notice, None, false);
    if !args.json && root.result.is_none() {
        return finish_one(env, remote, &op, true);
    }
    let mut result = match follow_result(env, remote, &op) {
        Ok(result) => result,
        Err(error) => return rig_failed(true, error),
    };
    let Some(info) = result.branches.first() else {
        return Err(missing("branch"));
    };
    // Read again, as local mode does: the root gained its children.
    let root = remote.repo.branch(&info.name)?;
    let view = if plan.root.result.is_some() {
        let mut attempts =
            crate::attempts::remote_attempts_for_infos(&remote.repo, std::slice::from_ref(&root))?;
        let attempt = attempts
            .pop()
            .ok_or_else(|| missing("rig root assessment"))?;
        commands::ResultView::from_attempt(&root, attempt)
    } else {
        None
    };
    if args.json {
        return commands::print_rig_run(&plan.rig, &root, &result.descendants, view.as_ref());
    }
    if let Some(info) = result.branches.first_mut() {
        *info = root.clone();
    }
    match finish_result(env, remote, result) {
        Ok(()) | Err(Failure::Reported) => {}
        Err(error) => return Err(error),
    }
    match view {
        Some(view) => {
            print(&view.text(Some(&result_next(remote, &root.name))))?;
            view.outcome(&root)
        }
        None => branch_outcome(&root),
    }
}

fn rig_failed(json: bool, error: branchyard::Error) -> Outcome {
    commands::fail(json, &error)
}

/// A server's answer without what it must hold: this `by` and the server
/// do not speak the same API.
fn missing(what: &str) -> Failure {
    Report::new("remote_protocol", format!("the server returned no {what}"))
        .next("compare by --version here with the server's; they should match")
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, provider: &str, isolated: bool) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            git_branch: format!("by/{name}"),
            worktree: format!("/srv/app/.branchyard/worktrees/{name}").into(),
            prompt: "p".into(),
            harness: "gemini-cli".into(),
            profile: "gemini-cli-acp".into(),
            session: None,
            parent: None,
            children: Vec::new(),
            depth: 0,
            base: "0123456789abcdef".into(),
            candidate: None,
            status: branchyard::BranchStatus::Ready,
            turns: 1,
            cost_usd: None,
            created_at: 10_000,
            stalled: false,
            superseded_by: None,
            model: None,
            provider: provider.into(),
            isolated,
            guard: None,
        }
    }

    /// A send (or fork, or plan decision) on a branch with a private home
    /// says so, as `by send` does locally, though the send's own flags ask
    /// for nothing: on 27 October 2026 a send to an isolated Gemini CLI
    /// branch said `in the server's own HOME; no sandbox`.
    #[test]
    fn work_that_continues_a_branch_says_where_the_branch_runs() {
        let label = "http://127.0.0.1:8421 (app)";
        let isolated = continuing_banner(label, None, false, &info("b", "local", true));
        assert_eq!(
            isolated,
            commands::banner(None, true, Some(label)),
            "{isolated}"
        );
        assert!(isolated.contains("with a private HOME"), "{isolated}");
        let local = continuing_banner(label, None, false, &info("b", "local", false));
        assert!(local.contains("in the server's own HOME"), "{local}");
        // --isolated on a fork of a local branch still asks for a home.
        assert_eq!(
            continuing_banner(label, None, true, &info("b", "local", false)),
            isolated
        );
        // A sandboxed branch is never called "no sandbox".
        let sandboxed = continuing_banner(label, None, false, &info("b", "microsandbox", false));
        assert_eq!(
            sandboxed,
            "by: remote mode on http://127.0.0.1:8421 (app): harnesses run in the microsandbox \
             sandbox the branch was started in, on the server"
        );
        // A provider the flags name wins over the record. Built from its
        // wire form: only crates/branchyard/src/providers names a variant.
        let flagged: Provider = serde_json::from_str(r#"{"kind": "local"}"#).unwrap();
        assert_eq!(
            continuing_banner(
                label,
                Some(&flagged),
                false,
                &info("b", "microsandbox", false)
            ),
            local
        );
    }

    fn op(state: OperationState, branches: &[&str]) -> Operation {
        Operation {
            id: "op_1".into(),
            repo: "app".into(),
            kind: branchyard_client::api::OperationKind::Task,
            state,
            branches: branches.iter().map(|b| b.to_string()).collect(),
            cursor: 0,
            end_cursor: None,
            created_at_ms: 0,
            finished_at_ms: None,
            result: None,
            error: None,
            requires: Vec::new(),
            waiting: None,
            priority: 0,
        }
    }

    /// `by ls` against a server names the branches its queued operations
    /// will create, which have no row yet: ten runs with eight running
    /// read as eight on 27 October 2026.
    #[test]
    fn ls_names_the_branches_queued_work_will_create() {
        let infos = [info("f1", "local", false)];
        let operations = [
            op(OperationState::Running, &["f1"]),
            op(OperationState::Queued, &["f5"]),
            op(OperationState::Queued, &["f8"]),
            // A queued send to a branch that exists is in the table.
            op(OperationState::Queued, &["f1"]),
        ];
        assert_eq!(
            queued_names_line(&operations, &infos),
            "\nqueued on the server, not started yet: f5, f8 (by show f5 shows its operation)\n"
        );
        assert_eq!(queued_names_line(&operations[..1], &infos), "");
        assert_eq!(queued_names_line(&[], &[]), "");
    }

    /// A restarting server is asked again; a refusal is not.
    #[test]
    fn a_server_that_cannot_be_reached_or_is_stopping_is_asked_again() {
        let transport = branchyard_client::Error::Transport {
            endpoint: "http://127.0.0.1:8421".into(),
            message: "Connection refused (os error 111)".into(),
        };
        assert!(transient(&transport));
        let api = |status: u16, code: &str| branchyard_client::Error::Api {
            status,
            error: Box::new(ErrorBody {
                code: code.into(),
                message: code.into(),
                detail: None,
            }),
        };
        assert!(transient(&api(503, "shutting_down")));
        assert!(transient(&api(502, "http_502")));
        assert!(!transient(&api(401, "unauthorized")));
        assert!(!transient(&api(404, "unknown_operation")));
        assert!(!transient(&api(500, "internal")));
        assert_eq!(
            lost_line(&transport, "op_7"),
            "cannot reach http://127.0.0.1:8421: Connection refused (os error 111); the server \
             keeps operation op_7, and this waits for it to answer again (Ctrl-C stops \
             watching, not the work)"
        );
    }

    /// Stream events already read after the stream opened are dropped,
    /// in order; the first one not among them ends the matching, so a later
    /// repeat of the same event is shown.
    #[test]
    fn the_overlap_is_dropped_once() {
        let event = |at_ms: u64, text: &str| branchyard::RecordedEvent {
            at_ms,
            activity: Activity::Prompt(text.into()),
        };
        let mut overlap = Overlap([event(1, "before"), event(2, "after")].into());
        // `before` was recorded before the stream opened: it never comes,
        // and `after` is the first stream event.
        assert!(overlap.seen(&event(2, "after")));
        assert!(!overlap.seen(&event(3, "new")));
        assert!(!overlap.seen(&event(2, "after")), "matching has ended");
        let mut none = Overlap(Default::default());
        assert!(!none.seen(&event(1, "x")));
    }

    /// A server's failed integration check reads as a local one: the
    /// shared check's fields and every check of the integration, at the
    /// top of the detail.
    #[test]
    fn a_remote_check_failure_keeps_its_checks_and_siblings() {
        let checks = serde_json::json!([
            {"check": ["make", "test"], "branches": ["b"], "outcome": "failed"}
        ]);
        let error = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_failed".into(),
                message: "check failed:\nboom".into(),
                detail: Some(serde_json::json!({
                    "output_tail": "boom",
                    "checks": checks,
                    "shared": {"integrate_together": ["b", "e"]},
                })),
            }),
        });
        assert_eq!(error.kind(), "check_failed");
        let detail = error.detail().unwrap();
        assert_eq!(detail["checks"], checks);
        assert_eq!(detail["integrate_together"], serde_json::json!(["b", "e"]));
        let own = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_failed".into(),
                message: "check failed:\nboom".into(),
                detail: Some(serde_json::json!({ "output_tail": "boom", "own_work": ["b"] })),
            }),
        });
        assert_eq!(own.detail().unwrap()["own_work"], serde_json::json!(["b"]));
        let plain = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_failed".into(),
                message: "check failed:\nboom".into(),
                detail: Some(serde_json::json!({ "output_tail": "boom" })),
            }),
        });
        assert_eq!(plain.detail(), None);
        let not_started = sdk_error(branchyard_client::Error::Api {
            status: 422,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "check_not_started".into(),
                message: "check could not start: no such program".into(),
                detail: Some(serde_json::json!({ "checks": checks })),
            }),
        });
        assert_eq!(not_started.kind(), "check_not_started");
        assert_eq!(not_started.detail().unwrap()["checks"], checks);
    }
}
