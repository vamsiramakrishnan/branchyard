//! `by rig`: a declarative team of harnesses, lowered onto one root branch.
//!
//! A rig spec is a TOML file naming a root seat and the seats below it,
//! joined by `delegates_to` edges that form a tree. [`parse`] reads it
//! strictly: an unknown field, a field Branchyard cannot honor, or a value
//! of the wrong type is an error naming the field and its line. [`load`]
//! also reads the startup files the spec names. [`plan`] is pure: it
//! checks the tree, the harnesses, the budgets and the isolation secrets
//! need, and lowers the spec to the root branch's task options, its
//! delegation envelope, and the [`Seats`] its harness may spawn by name.
//! Nothing is started and nothing is written. See `docs/rigs.md`.
//!
//! The format borrows OpenRig's ideas (seats, pods, `delegates_to` edges,
//! startup files, restore policy); no OpenRig code is used.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use branchyard::{
    ChildBudget, Effort, Envelope, McpServerSpec, Provisioning, Seat, Seats, SecretSource,
    Telemetry,
};
use serde::Serialize;
use toml_edit::{Document, Item, TableLike};

/// The only spec version this build reads.
pub const VERSION: i64 = 1;

/// The harness a root seat runs when it names none.
const DEFAULT_HARNESS: &str = "claude-code";

/// Cost comparisons tolerate float rounding of this much.
const EPSILON_USD: f64 = 1e-9;

/// Names: lowercase letters, digits and hyphens, starting with a letter or
/// digit; at most this long.
const NAME_MAX: usize = 40;

/// Why a spec was refused: the field, its line when known, and the reason.
#[derive(Clone, Debug, PartialEq)]
pub struct RigError {
    /// The field's dotted path, such as `seats.lead.budget.max_usd`; empty
    /// for the file as a whole.
    pub field: String,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for RigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        if !self.field.is_empty() {
            write!(f, "{}: ", self.field)?;
        }
        f.write_str(&self.message)
    }
}

fn error(field: impl Into<String>, line: Option<usize>, message: impl Into<String>) -> RigError {
    RigError {
        field: field.into(),
        line,
        message: message.into(),
    }
}

/// A parsed rig spec. Startup files are read by [`load`].
#[derive(Clone, Debug, PartialEq)]
pub struct RigSpec {
    pub name: String,
    pub description: Option<String>,
    /// The root seat's name.
    pub root: String,
    pub root_line: Option<usize>,
    /// Startup files for every seat.
    pub startup: Vec<StartupFile>,
    pub pods: BTreeMap<String, Pod>,
    /// Seats in the order the file declares them.
    pub seats: Vec<SeatSpec>,
}

/// A group of seats sharing startup files. It has no runtime record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pod {
    pub description: Option<String>,
    pub startup: Vec<StartupFile>,
}

/// A file delivered to a seat's harness as standing instructions, never
/// written into a worktree.
#[derive(Clone, Debug, PartialEq)]
pub struct StartupFile {
    /// Relative to the spec's directory, without `..`.
    pub path: String,
    /// A missing required file is an error; a missing optional one is
    /// skipped.
    pub required: bool,
    /// The file's text, once [`load`] has read it; `None` before, or for a
    /// missing optional file.
    pub content: Option<String>,
    field: String,
    line: Option<usize>,
}

/// One seat as declared.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeatSpec {
    pub name: String,
    pub description: Option<String>,
    pub pod: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub auth: Option<String>,
    pub secrets: Vec<String>,
    pub mcp: Vec<McpServerSpec>,
    pub telemetry: Option<Telemetry>,
    pub isolated: bool,
    pub budget: ChildBudget,
    pub check: Option<Vec<String>>,
    pub policy: PolicySpec,
    pub delegates_to: Vec<String>,
    /// Ancestor seats, besides its parent (always allowed), this seat may
    /// `escalate` to.
    pub escalates_to: Vec<String>,
    pub instances: u32,
    /// Scratch areas a child in this seat is bound to, as
    /// `NAME:read_only` or `NAME:exclusive_write`.
    pub bindings: Vec<branchyard::Binding>,
    pub startup: Vec<StartupFile>,
    /// Lines of the seat's table and fields, by dotted field name
    /// relative to the seat (`""` for the seat itself).
    lines: BTreeMap<String, usize>,
}

impl SeatSpec {
    fn field(&self, name: &str) -> String {
        match name.is_empty() {
            true => format!("seats.{}", self.name),
            false => format!("seats.{}.{name}", self.name),
        }
    }

    fn line(&self, name: &str) -> Option<usize> {
        self.lines.get(name).or_else(|| self.lines.get("")).copied()
    }

    fn error(&self, name: &str, message: impl Into<String>) -> RigError {
        error(self.field(name), self.line(name), message)
    }
}

/// How the root seat's permission requests are answered. A child seat runs
/// under its parent's policy and may only add denials.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PolicySpec {
    /// Requests no rule decides: `allow`, `deny` (the default) or `ask`.
    pub default: Option<Fallback>,
    /// Denied outright, before `allow` and the default.
    pub deny: Vec<String>,
    /// Allowed, after `deny`.
    pub allow: Vec<String>,
    /// Allow the harness's own `by` delegation commands, as
    /// `--allow-delegation` does.
    pub delegation_commands: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    Allow,
    Deny,
    Ask,
}

/// What [`plan`] lowers a spec to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RigPlan {
    pub rig: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub root: RootPlan,
    /// The seats the root may spawn, and those below them. `None` for a
    /// rig of one seat.
    pub seats: Option<Seats>,
    /// Seats whose harness profile cannot route tool permission requests
    /// to the policy; running them needs `--allow-unapproved-tools`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unapproved_tools: Vec<String>,
    /// Optional startup files that were not found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_files: Vec<String>,
}

/// The root branch's options.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RootPlan {
    pub seat: String,
    /// The branch name: the rig's, unless `by rig run --name` gives one.
    pub name: String,
    pub harness: String,
    pub profile: String,
    pub budget: ChildBudget,
    pub policy: PolicyPlan,
    pub check: Option<Vec<String>>,
    pub isolated: bool,
    pub provision: Provisioning,
    /// The envelope the seats need, when there are any.
    pub delegation: Option<Envelope>,
}

/// The root's policy, lowered: deny rules, then allow rules, then the
/// delegation command rule, then the default.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PolicyPlan {
    pub default: Fallback,
    pub deny: Vec<String>,
    pub allow: Vec<String>,
    pub delegation_commands: bool,
}

/// OpenRig fields, and others a reader may expect, that Branchyard refuses
/// with a reason rather than as unknown.
const REFUSED_RIG: &[(&str, &str)] = &[
    (
        "culture_file",
        "Branchyard has no culture files; put shared guidance in startup.files",
    ),
    (
        "services",
        "Branchyard does not manage services for a rig; start them before the rig",
    ),
    (
        "edges",
        "declare delegation on the parent seat, as seats.<parent>.delegates_to; other edge kinds are not supported",
    ),
    (
        "permission_policy",
        "permission presets are not implemented; set the root seat's policy",
    ),
    (
        "managed_blocks",
        "instructions reach each harness through provisioning, never through files in the worktree",
    ),
];

const REFUSED_SEAT: &[(&str, &str)] = &[
    (
        "collaborates_with",
        "Branchyard has no messaging between branches; a branch acts only on its descendants",
    ),
    (
        "can_observe",
        "a branch reads only its own descendants; there is no read grant for other branches",
    ),
    (
        "observes",
        "a branch reads only its own descendants; there is no read grant for other branches",
    ),
    (
        "spawned_by",
        "declare the edge on the parent seat, as seats.<parent>.delegates_to",
    ),
    (
        "continuity_policy",
        "Branchyard does not rebuild a conversation from briefs; a send resumes the harness's own session or fails",
    ),
    (
        "permission_policy",
        "permission presets are not implemented; set policy (the root's) or policy.deny (a child's)",
    ),
    (
        "cwd",
        "every branch runs in its own git worktree",
    ),
    (
        "command",
        "a seat names a harness; the executable is the server's, or the root's with by rig run --command",
    ),
    ("runtime", "name the harness with harness"),
    ("agent_ref", "name the harness with harness"),
    ("profile", "name the harness with harness, which takes a profile ID too"),
    (
        "provider",
        "a rig runs its branches where the root runs; sandbox providers are not supported for rigs yet",
    ),
    ("prompt", "the root's prompt is the one by rig run is given; a child's is the one its parent spawns it with"),
];

/// Parse a rig spec. Errors name the field and line.
pub fn parse(text: &str) -> Result<RigSpec, RigError> {
    let document = Document::parse(text).map_err(|e| {
        let line = e.span().map(|span| line_of(text, span.start));
        error("", line, format!("not valid TOML: {}", e.message().trim()))
    })?;
    Parser { text }.rig(document.as_table())
}

/// Read and parse the spec at `path`, then read each startup file it
/// names, relative to its directory.
pub fn load(path: &Path) -> Result<RigSpec, RigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| error("", None, format!("could not read {}: {e}", path.display())))?;
    let mut spec = parse(&text)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let files = spec.startup.iter_mut().chain(
        spec.pods
            .values_mut()
            .flat_map(|pod| pod.startup.iter_mut()),
    );
    let files: Vec<&mut StartupFile> = files
        .chain(
            spec.seats
                .iter_mut()
                .flat_map(|seat| seat.startup.iter_mut()),
        )
        .collect();
    for file in files {
        let full: PathBuf = dir.join(&file.path);
        match std::fs::read_to_string(&full) {
            Ok(text) => file.content = Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !file.required => {}
            Err(e) => {
                return Err(error(
                    file.field.clone(),
                    file.line,
                    format!("could not read {}: {e}", full.display()),
                ))
            }
        }
    }
    Ok(spec)
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

struct Parser<'a> {
    text: &'a str,
}

/// A table's entries: key, item, line.
type Entries<'t> = Vec<(String, &'t Item, Option<usize>)>;

impl Parser<'_> {
    fn line(&self, span: Option<Range<usize>>) -> Option<usize> {
        span.map(|span| line_of(self.text, span.start))
    }

    /// The entries of `table`, refusing unknown and refused keys.
    fn entries<'t>(
        &self,
        table: &'t dyn TableLike,
        path: &str,
        known: &[&str],
        refused: &[(&str, &str)],
    ) -> Result<Entries<'t>, RigError> {
        let mut out = Vec::new();
        for (key, item) in table.iter() {
            let line = table
                .get_key_value(key)
                .and_then(|(k, _)| self.line(k.span()))
                .or_else(|| self.line(item.span()));
            let field = join(path, key);
            if let Some((_, why)) = refused.iter().find(|(name, _)| *name == key) {
                return Err(error(field, line, format!("not supported: {why}")));
            }
            if !known.contains(&key) {
                return Err(error(
                    field,
                    line,
                    format!("unknown field; expected one of: {}", known.join(", ")),
                ));
            }
            out.push((key.to_owned(), item, line));
        }
        Ok(out)
    }

    fn table<'t>(
        &self,
        item: &'t Item,
        field: &str,
        line: Option<usize>,
    ) -> Result<&'t dyn TableLike, RigError> {
        item.as_table_like()
            .ok_or_else(|| error(field, line, format!("must be a table, not {}", kind(item))))
    }

    fn rig(&self, top: &toml_edit::Table) -> Result<RigSpec, RigError> {
        let entries = self.entries(
            top,
            "",
            &[
                "version",
                "name",
                "description",
                "root",
                "startup",
                "pods",
                "seats",
            ],
            REFUSED_RIG,
        )?;
        let mut version = None;
        let mut name = None;
        let mut description = None;
        let mut root = None;
        let mut startup = Vec::new();
        let mut pods = BTreeMap::new();
        let mut seats = None;
        for (key, item, line) in entries {
            match key.as_str() {
                "version" => version = Some((integer(item, &key, line)?, line)),
                "name" => name = Some(named(string(item, &key, line)?, &key, line)?),
                "description" => description = Some(string(item, &key, line)?),
                "root" => root = Some((string(item, &key, line)?, line)),
                "startup" => startup = self.startup(item, &key, line)?,
                "pods" => {
                    let table = self.table(item, &key, line)?;
                    for (pod, item, line) in self.entries_any(table) {
                        let field = join("pods", &pod);
                        named(pod.clone(), &field, line)?;
                        pods.insert(pod, self.pod(item, &field, line)?);
                    }
                }
                "seats" => seats = Some((item, line)),
                _ => unreachable!("checked against the known fields"),
            }
        }
        match version {
            None => {
                return Err(error(
                    "version",
                    None,
                    format!("required; write version = {VERSION}"),
                ))
            }
            Some((v, _)) if v == VERSION => {}
            Some((v, line)) => {
                return Err(error(
                    "version",
                    line,
                    format!("this by reads version {VERSION}, not {v}"),
                ))
            }
        }
        let name = name.ok_or_else(|| error("name", None, "required: the rig's name"))?;
        let (root, root_line) =
            root.ok_or_else(|| error("root", None, "required: the name of the root seat"))?;
        let (seats_item, seats_line) =
            seats.ok_or_else(|| error("seats", None, "required: at least the root seat"))?;
        let table = self.table(seats_item, "seats", seats_line)?;
        let mut seats = Vec::new();
        for (seat, item, line) in self.entries_any(table) {
            let field = join("seats", &seat);
            named(seat.clone(), &field, line)?;
            seats.push(self.seat(&seat, item, &field, line)?);
        }
        Ok(RigSpec {
            name,
            description,
            root,
            root_line,
            startup,
            pods,
            seats,
        })
    }

    /// Entries whose keys are names, not fields.
    fn entries_any<'t>(&self, table: &'t dyn TableLike) -> Entries<'t> {
        table
            .iter()
            .map(|(key, item)| {
                let line = table
                    .get_key_value(key)
                    .and_then(|(k, _)| self.line(k.span()))
                    .or_else(|| self.line(item.span()));
                (key.to_owned(), item, line)
            })
            .collect()
    }

    fn pod(&self, item: &Item, field: &str, line: Option<usize>) -> Result<Pod, RigError> {
        let table = self.table(item, field, line)?;
        let mut pod = Pod::default();
        for (key, item, line) in self.entries(table, field, &["description", "startup"], &[])? {
            let path = join(field, &key);
            match key.as_str() {
                "description" => pod.description = Some(string(item, &path, line)?),
                _ => pod.startup = self.startup(item, &path, line)?,
            }
        }
        Ok(pod)
    }

    fn startup(
        &self,
        item: &Item,
        field: &str,
        line: Option<usize>,
    ) -> Result<Vec<StartupFile>, RigError> {
        let table = self.table(item, field, line)?;
        let refused = [(
            "actions",
            "startup actions are not supported: the prompt you give is the first message, and \
             drivers refuse slash commands",
        )];
        let mut files = Vec::new();
        for (key, item, line) in self.entries(table, field, &["files"], &refused)? {
            let path = join(field, &key);
            let Some(array) = item.as_array() else {
                return Err(error(
                    &path,
                    line,
                    format!("must be an array of paths, not {}", kind(item)),
                ));
            };
            for (index, value) in array.iter().enumerate() {
                let entry = format!("{path}[{index}]");
                let line = self.line(value.span()).or(line);
                let (file, required) = match value {
                    toml_edit::Value::String(s) => (s.value().clone(), true),
                    toml_edit::Value::InlineTable(t) => {
                        let mut file = None;
                        let mut required = true;
                        for (key, item, line) in
                            self.entries(t, &entry, &["path", "required"], &[
                                ("delivery_hint", "every startup file is delivered as standing instructions; skills and first-message text are not supported"),
                            ])?
                        {
                            let at = join(&entry, &key);
                            match key.as_str() {
                                "path" => file = Some(string(item, &at, line)?),
                                _ => required = boolean(item, &at, line)?,
                            }
                        }
                        let file = file.ok_or_else(|| error(&entry, line, "needs a path"))?;
                        (file, required)
                    }
                    other => {
                        return Err(error(
                            &entry,
                            line,
                            format!(
                                "must be a path or {{path, required}}, not {}",
                                other.type_name()
                            ),
                        ))
                    }
                };
                safe_path(&file, &entry, line)?;
                files.push(StartupFile {
                    path: file,
                    required,
                    content: None,
                    field: entry,
                    line,
                });
            }
        }
        Ok(files)
    }

    fn seat(
        &self,
        name: &str,
        item: &Item,
        field: &str,
        line: Option<usize>,
    ) -> Result<SeatSpec, RigError> {
        let table = self.table(item, field, line)?;
        let mut seat = SeatSpec {
            name: name.to_owned(),
            instances: 1,
            ..SeatSpec::default()
        };
        if let Some(line) = self.line(item.span()).or(line) {
            seat.lines.insert(String::new(), line);
        }
        let known = [
            "description",
            "pod",
            "harness",
            "model",
            "effort",
            "auth",
            "secrets",
            "mcp",
            "telemetry",
            "isolated",
            "budget",
            "check",
            "policy",
            "delegates_to",
            "escalates_to",
            "instances",
            "bindings",
            "startup",
            "start",
            "restore_policy",
        ];
        for (key, item, line) in self.entries(table, field, &known, REFUSED_SEAT)? {
            let path = join(field, &key);
            if let Some(line) = line {
                seat.lines.insert(key.clone(), line);
            }
            match key.as_str() {
                "description" => seat.description = Some(string(item, &path, line)?),
                "pod" => seat.pod = Some(string(item, &path, line)?),
                "harness" => seat.harness = Some(nonempty(item, &path, line)?),
                "model" => seat.model = Some(nonempty(item, &path, line)?),
                "effort" => {
                    seat.effort = Some(match (item.as_str(), item.as_integer()) {
                        (Some(text), _) => {
                            Effort::parse(text).map_err(|e| error(&path, line, e))?
                        }
                        (_, Some(level)) if (0..=100).contains(&level) => Effort::from_level(level),
                        _ => {
                            return Err(error(
                                &path,
                                line,
                                "must be low, medium, high, xhigh, or 0-100",
                            ))
                        }
                    })
                }
                "auth" => seat.auth = Some(nonempty(item, &path, line)?),
                "secrets" => {
                    for secret in strings(item, &path, line)? {
                        let parsed =
                            SecretSource::parse(&secret).map_err(|e| error(&path, line, e))?;
                        if parsed.from.is_some() {
                            return Err(error(
                                &path,
                                line,
                                format!(
                                    "{secret}: a rig names secrets only; where each comes from \
                                     is your environment's (locally) or the server operator's"
                                ),
                            ));
                        }
                        seat.secrets.push(secret);
                    }
                }
                "mcp" => {
                    let servers = self.table(item, &path, line)?;
                    for (server, item, line) in self.entries_any(servers) {
                        let at = join(&path, &server);
                        let command = string(item, &at, line)?;
                        let spec = McpServerSpec::parse(&format!("{server}={command}"))
                            .and_then(|spec| spec.check().map(|()| spec))
                            .map_err(|e| error(&at, line, e))?;
                        seat.mcp.push(spec);
                    }
                }
                "telemetry" => {
                    let text = string(item, &path, line)?;
                    seat.telemetry =
                        Some(Telemetry::parse(&text).map_err(|e| error(&path, line, e))?);
                }
                "isolated" => seat.isolated = boolean(item, &path, line)?,
                "budget" => seat.budget = self.budget(item, &path, line, &mut seat.lines)?,
                "check" => {
                    let argv = match item.as_str() {
                        Some(text) => {
                            crate::args::split_words(text).map_err(|e| error(&path, line, e))?
                        }
                        None => strings(item, &path, line)?,
                    };
                    if argv.is_empty() || argv[0].is_empty() {
                        return Err(error(&path, line, "needs a command"));
                    }
                    seat.check = Some(argv);
                }
                "policy" => seat.policy = self.policy(item, &path, line, &mut seat.lines)?,
                "delegates_to" => {
                    seat.delegates_to = strings(item, &path, line)?;
                    let mut seen = BTreeSet::new();
                    for target in &seat.delegates_to {
                        if !seen.insert(target) {
                            return Err(error(&path, line, format!("{target} is listed twice")));
                        }
                    }
                }
                "escalates_to" => seat.escalates_to = strings(item, &path, line)?,
                "instances" => {
                    let n = integer(item, &path, line)?;
                    seat.instances = u32::try_from(n)
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| error(&path, line, "must be a whole number from 1"))?;
                }
                "bindings" => {
                    for text in strings(item, &path, line)? {
                        let binding =
                            branchyard::Binding::parse(&text).map_err(|e| error(&path, line, e))?;
                        if seat.bindings.iter().any(|b| b.scratch == binding.scratch) {
                            return Err(error(
                                &path,
                                line,
                                format!("scratch area {} is bound twice", binding.scratch),
                            ));
                        }
                        seat.bindings.push(binding);
                    }
                }
                "startup" => seat.startup = self.startup(item, &path, line)?,
                "start" => match string(item, &path, line)?.as_str() {
                    "on_demand" => {}
                    "eager" => {
                        return Err(error(
                            &path,
                            line,
                            "eager seats are not supported: the root starts alone and fills \
                             seats with by spawn --seat; use on_demand",
                        ))
                    }
                    other => {
                        return Err(error(
                            &path,
                            line,
                            format!("must be on_demand, not {other}"),
                        ))
                    }
                },
                "restore_policy" => match string(item, &path, line)?.as_str() {
                    "resume_if_possible" => {}
                    other @ ("relaunch_fresh" | "checkpoint_only") => {
                        return Err(error(
                            &path,
                            line,
                            format!(
                                "{other} is not supported: a send resumes the branch's session, \
                                 and fails rather than start a fresh one"
                            ),
                        ))
                    }
                    other => {
                        return Err(error(
                            &path,
                            line,
                            format!("must be resume_if_possible, not {other}"),
                        ))
                    }
                },
                _ => unreachable!("checked against the known fields"),
            }
        }
        Ok(seat)
    }

    fn budget(
        &self,
        item: &Item,
        field: &str,
        line: Option<usize>,
        lines: &mut BTreeMap<String, usize>,
    ) -> Result<ChildBudget, RigError> {
        let table = self.table(item, field, line)?;
        let mut budget = ChildBudget::default();
        for (key, item, line) in
            self.entries(table, field, &["max_usd", "max_turns", "max_minutes"], &[])?
        {
            let path = join(field, &key);
            if let Some(line) = line {
                lines.insert(format!("budget.{key}"), line);
            }
            match key.as_str() {
                "max_usd" => budget.max_usd = Some(positive(item, &path, line)?),
                "max_minutes" => budget.max_minutes = Some(positive(item, &path, line)?),
                _ => {
                    let n = integer(item, &path, line)?;
                    budget.max_turns = Some(
                        u32::try_from(n)
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| error(&path, line, "must be a whole number from 1"))?,
                    );
                }
            }
        }
        Ok(budget)
    }

    fn policy(
        &self,
        item: &Item,
        field: &str,
        line: Option<usize>,
        lines: &mut BTreeMap<String, usize>,
    ) -> Result<PolicySpec, RigError> {
        let table = self.table(item, field, line)?;
        let mut policy = PolicySpec::default();
        for (key, item, line) in self.entries(
            table,
            field,
            &["default", "deny", "allow", "delegation_commands"],
            &[],
        )? {
            let path = join(field, &key);
            if let Some(line) = line {
                lines.insert(format!("policy.{key}"), line);
            }
            match key.as_str() {
                "default" => {
                    policy.default = Some(match string(item, &path, line)?.as_str() {
                        "allow" => Fallback::Allow,
                        "deny" => Fallback::Deny,
                        "ask" => Fallback::Ask,
                        other => {
                            return Err(error(
                                &path,
                                line,
                                format!(
                                    "must be allow, deny or ask, not {other}; Branchyard answers \
                                     every request and never bypasses permissions"
                                ),
                            ))
                        }
                    })
                }
                "deny" => policy.deny = tools(item, &path, line)?,
                "allow" => policy.allow = tools(item, &path, line)?,
                _ => policy.delegation_commands = boolean(item, &path, line)?,
            }
        }
        Ok(policy)
    }
}

fn join(path: &str, key: &str) -> String {
    match path.is_empty() {
        true => key.to_owned(),
        false => format!("{path}.{key}"),
    }
}

fn kind(item: &Item) -> &'static str {
    item.type_name()
}

fn string(item: &Item, field: &str, line: Option<usize>) -> Result<String, RigError> {
    item.as_str()
        .map(str::to_owned)
        .ok_or_else(|| error(field, line, format!("must be a string, not {}", kind(item))))
}

fn nonempty(item: &Item, field: &str, line: Option<usize>) -> Result<String, RigError> {
    let text = string(item, field, line)?;
    match text.trim().is_empty() {
        true => Err(error(field, line, "must not be empty")),
        false => Ok(text),
    }
}

fn boolean(item: &Item, field: &str, line: Option<usize>) -> Result<bool, RigError> {
    item.as_bool().ok_or_else(|| {
        error(
            field,
            line,
            format!("must be true or false, not {}", kind(item)),
        )
    })
}

fn integer(item: &Item, field: &str, line: Option<usize>) -> Result<i64, RigError> {
    item.as_integer().ok_or_else(|| {
        error(
            field,
            line,
            format!("must be a whole number, not {}", kind(item)),
        )
    })
}

fn positive(item: &Item, field: &str, line: Option<usize>) -> Result<f64, RigError> {
    let value = item
        .as_float()
        .or_else(|| item.as_integer().map(|i| i as f64))
        .ok_or_else(|| error(field, line, format!("must be a number, not {}", kind(item))))?;
    match value.is_finite() && value > 0.0 {
        true => Ok(value),
        false => Err(error(
            field,
            line,
            format!("must be a positive number, not {value}"),
        )),
    }
}

fn strings(item: &Item, field: &str, line: Option<usize>) -> Result<Vec<String>, RigError> {
    let array = item.as_array().ok_or_else(|| {
        error(
            field,
            line,
            format!("must be an array of strings, not {}", kind(item)),
        )
    })?;
    array
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                error(
                    format!("{field}[{index}]"),
                    line,
                    format!("must be a string, not {}", value.type_name()),
                )
            })
        })
        .collect()
}

/// Tool patterns: exact names, or a prefix with a trailing `*`.
fn tools(item: &Item, field: &str, line: Option<usize>) -> Result<Vec<String>, RigError> {
    let tools = strings(item, field, line)?;
    for tool in &tools {
        if tool.trim().is_empty() || tool[..tool.len().saturating_sub(1)].contains('*') {
            return Err(error(
                field,
                line,
                format!("{tool:?} is not a tool name or a prefix ending in *"),
            ));
        }
    }
    Ok(tools)
}

/// A name usable as a branch name and a seat: lowercase letters, digits
/// and hyphens.
fn named(name: String, field: &str, line: Option<usize>) -> Result<String, RigError> {
    let valid = !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    match valid {
        true => Ok(name),
        false => Err(error(
            field,
            line,
            format!(
                "{name:?} is not a usable name: lowercase letters, digits and inner hyphens, at \
                 most {NAME_MAX} characters"
            ),
        )),
    }
}

/// A startup file's path: relative, inside the spec's directory.
fn safe_path(path: &str, field: &str, line: Option<usize>) -> Result<(), RigError> {
    let parsed = Path::new(path);
    let escapes = parsed
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    match path.is_empty() || escapes {
        true => Err(error(
            field,
            line,
            format!("{path:?} must be a relative path inside the spec's directory, without .."),
        )),
        false => Ok(()),
    }
}

/// Lower a loaded spec. Pure: checks the whole spec and builds the root's
/// options and seats, or names the first field it cannot honor.
pub fn plan(spec: &RigSpec) -> Result<RigPlan, RigError> {
    let by_name: BTreeMap<&str, &SeatSpec> =
        spec.seats.iter().map(|s| (s.name.as_str(), s)).collect();
    let root = *by_name.get(spec.root.as_str()).ok_or_else(|| {
        error(
            "root",
            spec.root_line,
            format!("{} is not a seat", spec.root),
        )
    })?;

    // The edges must form a tree below the root.
    let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
    for seat in &spec.seats {
        for target in &seat.delegates_to {
            if !by_name.contains_key(target.as_str()) {
                return Err(seat.error("delegates_to", format!("{target} is not a seat")));
            }
            if *target == spec.root {
                return Err(seat.error(
                    "delegates_to",
                    format!("{target} is the root seat, which is below no other"),
                ));
            }
            if *target == seat.name {
                return Err(seat.error("delegates_to", "a seat cannot delegate to itself"));
            }
            if let Some(other) = parent.insert(target, &seat.name) {
                return Err(seat.error(
                    "delegates_to",
                    format!("{target} is already below {other}; seats form a tree"),
                ));
            }
        }
    }
    let mut reached = BTreeSet::from([root.name.as_str()]);
    let mut queue = vec![root];
    while let Some(seat) = queue.pop() {
        for target in &seat.delegates_to {
            if reached.insert(target) {
                queue.push(by_name[target.as_str()]);
            }
        }
    }
    for seat in &spec.seats {
        if !reached.contains(seat.name.as_str()) {
            let why = match parent.contains_key(seat.name.as_str()) {
                true => "is in a cycle of delegates_to edges that the root does not reach",
                false => "is below no seat; list it in its parent's delegates_to, or remove it",
            };
            return Err(seat.error("", why));
        }
        if let Some(pod) = &seat.pod {
            if !spec.pods.contains_key(pod) {
                return Err(seat.error("pod", format!("{pod} is not a pod")));
            }
        }
    }

    // A seat may escalate only to an ancestor seat, besides its parent
    // (always allowed, and not named here). The root has no ancestor seat
    // at all.
    if !root.escalates_to.is_empty() {
        return Err(root.error(
            "escalates_to",
            "the root seat has no ancestor seat to escalate to",
        ));
    }
    for seat in &spec.seats {
        if seat.name == spec.root {
            continue;
        }
        for target in &seat.escalates_to {
            if !by_name.contains_key(target.as_str()) {
                return Err(seat.error("escalates_to", format!("{target} is not a seat")));
            }
            // The immediate parent is always allowed and is not what
            // escalates_to is for; only a seat further up counts.
            let mut ancestors = BTreeSet::new();
            let mut walk = parent
                .get(seat.name.as_str())
                .and_then(|p| parent.get(*p))
                .copied();
            while let Some(next) = walk {
                ancestors.insert(next);
                walk = parent.get(next).copied();
            }
            if !ancestors.contains(target.as_str()) {
                return Err(seat.error(
                    "escalates_to",
                    format!(
                        "{target} is not an ancestor of seat {} beyond its parent",
                        seat.name
                    ),
                ));
            }
        }
    }

    // Harnesses: a child defaults to its parent's.
    let mut harness: BTreeMap<&str, String> = BTreeMap::new();
    let mut unapproved = Vec::new();
    let mut order = Vec::new();
    let mut walk = vec![root];
    while let Some(seat) = walk.pop() {
        order.push(seat);
        let id = match (&seat.harness, parent.get(seat.name.as_str())) {
            (Some(id), _) => id.clone(),
            (None, Some(up)) => harness[up].clone(),
            (None, None) => DEFAULT_HARNESS.to_owned(),
        };
        let profile =
            branchyard::harness_profile(&id).map_err(|e| seat.error("harness", e.to_string()))?;
        if !profile.tool_approvals {
            unapproved.push(seat.name.clone());
        }
        harness.insert(&seat.name, id);
        walk.extend(seat.delegates_to.iter().rev().map(|t| by_name[t.as_str()]));
    }

    // A child's policy only narrows its parent's.
    for seat in &spec.seats {
        if seat.name == spec.root {
            continue;
        }
        let policy = &seat.policy;
        for (set, name) in [
            (policy.default.is_some(), "policy.default"),
            (!policy.allow.is_empty(), "policy.allow"),
            (policy.delegation_commands, "policy.delegation_commands"),
        ] {
            if set {
                return Err(seat.error(
                    name,
                    "a child runs under its parent's policy; a child seat may only add \
                     policy.deny",
                ));
            }
        }
    }

    // Each child fits in its parent's limits.
    for seat in &spec.seats {
        let children: Vec<&SeatSpec> = seat
            .delegates_to
            .iter()
            .map(|t| by_name[t.as_str()])
            .collect();
        if let Some(limit) = seat.budget.max_usd {
            let mut total = 0.0;
            for child in &children {
                let Some(usd) = child.budget.max_usd else {
                    return Err(child.error(
                        "budget",
                        format!(
                            "seat {} has a cost limit, so this seat needs budget.max_usd",
                            seat.name
                        ),
                    ));
                };
                total += usd * f64::from(child.instances);
            }
            if total > limit + EPSILON_USD {
                return Err(seat.error(
                    "budget.max_usd",
                    format!(
                        "its seats reserve ${total:.2} at once ({}), more than its ${limit:.2}",
                        children
                            .iter()
                            .map(|c| match c.instances {
                                1 => c.name.clone(),
                                n => format!("{n} x {}", c.name),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        }
        for child in &children {
            if let (Some(limit), Some(asked)) = (seat.budget.max_turns, child.budget.max_turns) {
                if asked > limit {
                    return Err(child.error(
                        "budget.max_turns",
                        format!("{asked} exceeds seat {}'s {limit}", seat.name),
                    ));
                }
            }
            if let (Some(limit), Some(asked)) = (seat.budget.max_minutes, child.budget.max_minutes)
            {
                if asked > limit {
                    return Err(child.error(
                        "budget.max_minutes",
                        format!("{asked} exceeds seat {}'s {limit}", seat.name),
                    ));
                }
            }
        }
    }

    // Secrets need a private home: this seat's or an ancestor's.
    let mut private: BTreeMap<&str, bool> = BTreeMap::new();
    for seat in &order {
        let inherited = parent.get(seat.name.as_str()).is_some_and(|up| private[up]);
        let own = inherited || seat.isolated;
        private.insert(&seat.name, own);
        if !seat.secrets.is_empty() && !own {
            return Err(seat.error(
                "secrets",
                "secrets are written only into a home private to the branch; set isolated = \
                 true on this seat or one above it",
            ));
        }
    }

    // Lower.
    let mut skipped = Vec::new();
    let mut table = BTreeMap::new();
    for seat in &spec.seats {
        if seat.name == spec.root {
            continue;
        }
        table.insert(
            seat.name.clone(),
            Seat {
                harness: harness[seat.name.as_str()].clone(),
                budget: seat.budget.clone(),
                check: seat.check.clone(),
                deny: seat.policy.deny.clone(),
                isolated: seat.isolated,
                provision: Some(provision(spec, seat, &by_name, &harness, &mut skipped)),
                delegates_to: seat.delegates_to.clone(),
                escalates_to: seat.escalates_to.clone(),
                instances: seat.instances,
                bindings: seat.bindings.clone(),
            },
        );
    }
    let seats = (!root.delegates_to.is_empty()).then(|| Seats {
        rig: spec.name.clone(),
        seat: root.name.clone(),
        delegates_to: root.delegates_to.clone(),
        escalates_to: Vec::new(),
        table,
    });
    let root_harness = harness[root.name.as_str()].clone();
    let profile = branchyard::harness_profile(&root_harness)
        .map_err(|e| root.error("harness", e.to_string()))?;
    let root_provision = provision(spec, root, &by_name, &harness, &mut skipped);
    skipped.sort();
    skipped.dedup();
    Ok(RigPlan {
        rig: spec.name.clone(),
        description: spec.description.clone(),
        root: RootPlan {
            seat: root.name.clone(),
            name: spec.name.clone(),
            harness: root_harness,
            profile: profile.profile,
            budget: root.budget.clone(),
            policy: PolicyPlan {
                default: root.policy.default.unwrap_or(Fallback::Deny),
                deny: root.policy.deny.clone(),
                allow: root.policy.allow.clone(),
                delegation_commands: root.policy.delegation_commands,
            },
            check: root.check.clone(),
            isolated: root.isolated,
            provision: root_provision,
            delegation: seats.as_ref().map(Seats::envelope),
        },
        seats,
        unapproved_tools: unapproved,
        skipped_files: skipped,
    })
}

/// A seat's provisioning: its model, effort, credentials, MCP servers and
/// telemetry, and instructions naming its seat and the seats it may spawn,
/// followed by the rig's, its pod's and its own startup files.
fn provision(
    spec: &RigSpec,
    seat: &SeatSpec,
    by_name: &BTreeMap<&str, &SeatSpec>,
    harness: &BTreeMap<&str, String>,
    skipped: &mut Vec<String>,
) -> Provisioning {
    let mut text = format!("# Rig {}: seat {}\n\n", spec.name, seat.name);
    text.push_str(&format!(
        "You fill seat `{}` in the Branchyard rig `{}`.",
        seat.name, spec.name
    ));
    if let Some(description) = &seat.description {
        text.push_str(&format!(" Your role: {}", description.trim()));
    }
    text.push_str("\n\n");
    match seat.delegates_to.is_empty() {
        true => text.push_str(
            "You do not delegate: do your task on your own branch and leave the result there.\n",
        ),
        false => {
            text.push_str(
                "You delegate only by seat, and only to these seats. Spawn a child with \
                 `by spawn --seat SEAT \"<task>\"` (Python: `branchyard.spawn(task, seat=SEAT)`; \
                 MCP: the spawn tool's `seat`). The seat sets the child's harness, budget, \
                 check and instructions; you may give it less budget, not more. Integrate a \
                 finished child with `by integrate <child>`.\n\n",
            );
            for target in &seat.delegates_to {
                let child = by_name[target.as_str()];
                let mut facts = vec![harness[target.as_str()].clone()];
                if child.instances > 1 {
                    facts.push(format!("up to {} at once", child.instances));
                }
                if let Some(usd) = child.budget.max_usd {
                    facts.push(format!("${usd:.2} each"));
                }
                if let Some(turns) = child.budget.max_turns {
                    facts.push(format!("{turns} turns"));
                }
                text.push_str(&format!("- `{target}` ({})", facts.join(", ")));
                if let Some(description) = &child.description {
                    text.push_str(&format!(": {}", description.trim()));
                }
                text.push('\n');
            }
        }
    }
    let pod = seat.pod.as_ref().and_then(|p| spec.pods.get(p));
    let files = spec
        .startup
        .iter()
        .chain(pod.into_iter().flat_map(|p| p.startup.iter()))
        .chain(seat.startup.iter());
    for file in files {
        match &file.content {
            Some(content) => {
                text.push_str(&format!(
                    "\n## From {}\n\n{}",
                    file.path,
                    content.trim_end()
                ));
                text.push('\n');
            }
            None => skipped.push(file.path.clone()),
        }
    }
    Provisioning {
        secrets: seat
            .secrets
            .iter()
            .map(|name| SecretSource {
                name: name.clone(),
                from: None,
            })
            .collect(),
        auth: seat.auth.clone(),
        mcp_servers: seat.mcp.clone(),
        // A rig declares only stdio servers.
        remote_mcp_servers: Vec::new(),
        instructions: Some(text),
        model: seat.model.clone(),
        effort: seat.effort,
        telemetry: seat.telemetry.clone(),
    }
}

/// `by rig check`'s text: the root, its envelope, and each seat.
pub fn render(plan: &RigPlan) -> String {
    let mut out = format!("rig {}", plan.rig);
    if let Some(description) = &plan.description {
        out.push_str(&format!(": {}", description.trim()));
    }
    out.push('\n');
    let root = &plan.root;
    out.push_str(&format!(
        "root   {} as branch {}, {} ({})\n",
        root.seat, root.name, root.harness, root.profile
    ));
    out.push_str(&format!("       budget {}\n", budget_text(&root.budget)));
    if let Some(check) = &root.check {
        out.push_str(&format!("       check {}\n", check.join(" ")));
    }
    let policy = &root.policy;
    let mut rules = Vec::new();
    if !policy.deny.is_empty() {
        rules.push(format!("deny {}", policy.deny.join(", ")));
    }
    if !policy.allow.is_empty() {
        rules.push(format!("allow {}", policy.allow.join(", ")));
    }
    if policy.delegation_commands {
        rules.push("allow by delegation commands".into());
    }
    let default = match policy.default {
        Fallback::Allow => "allow",
        Fallback::Deny => "deny",
        Fallback::Ask => "ask",
    };
    rules.push(format!("then {default}"));
    out.push_str(&format!("       policy {}\n", rules.join("; ")));
    if root.isolated {
        out.push_str("       isolated\n");
    }
    out.push_str(&provision_text(&root.provision));
    match &root.delegation {
        Some(envelope) => out.push_str(&format!(
            "       envelope depth {}, {} children, harnesses {}\n",
            envelope.max_depth,
            envelope.max_children,
            envelope.harnesses.join(", ")
        )),
        None => out.push_str("       no seats below; the root does not delegate\n"),
    }
    if let Some(seats) = &plan.seats {
        out.push_str("seats\n");
        let mut queue: Vec<(String, String, usize)> = seats
            .delegates_to
            .iter()
            .rev()
            .map(|s| (s.clone(), seats.seat.clone(), 1))
            .collect();
        while let Some((name, up, depth)) = queue.pop() {
            let seat = &seats.table[&name];
            let profile = branchyard::harness_profile(&seat.harness)
                .map(|p| p.profile)
                .unwrap_or_default();
            out.push_str(&format!(
                "{}{name} (under {up}), {} ({profile})",
                "  ".repeat(depth),
                seat.harness
            ));
            if seat.instances > 1 {
                out.push_str(&format!(", up to {} at once", seat.instances));
            }
            out.push('\n');
            let pad = "  ".repeat(depth + 1);
            out.push_str(&format!("{pad}budget {}\n", budget_text(&seat.budget)));
            if let Some(check) = &seat.check {
                out.push_str(&format!("{pad}check {}\n", check.join(" ")));
            }
            if !seat.deny.is_empty() {
                out.push_str(&format!("{pad}deny {}\n", seat.deny.join(", ")));
            }
            if seat.isolated {
                out.push_str(&format!("{pad}isolated\n"));
            }
            if let Some(provision) = &seat.provision {
                out.push_str(&provision_text(provision).replace("       ", &pad));
            }
            queue.extend(
                seat.delegates_to
                    .iter()
                    .rev()
                    .map(|s| (s.clone(), name.clone(), depth + 1)),
            );
        }
    }
    if !plan.unapproved_tools.is_empty() {
        out.push_str(&format!(
            "needs --allow-unapproved-tools: {} run profiles that do not route tool permission \
             requests to the policy\n",
            plan.unapproved_tools.join(", ")
        ));
    }
    if !plan.skipped_files.is_empty() {
        out.push_str(&format!(
            "skipped optional startup files: {}\n",
            plan.skipped_files.join(", ")
        ));
    }
    out
}

fn budget_text(budget: &ChildBudget) -> String {
    let mut parts = Vec::new();
    if let Some(usd) = budget.max_usd {
        parts.push(format!("${usd:.2}"));
    }
    if let Some(turns) = budget.max_turns {
        parts.push(format!("{turns} turns"));
    }
    if let Some(minutes) = budget.max_minutes {
        parts.push(format!("{minutes} min per turn"));
    }
    match parts.is_empty() {
        true => "unlimited".into(),
        false => parts.join(", "),
    }
}

fn provision_text(provision: &Provisioning) -> String {
    let pad = "       ";
    let mut out = String::new();
    if let Some(model) = &provision.model {
        out.push_str(&format!("{pad}model {model}\n"));
    }
    if let Some(effort) = provision.effort {
        out.push_str(&format!("{pad}effort {}\n", effort.as_str()));
    }
    if !provision.secrets.is_empty() {
        let names: Vec<&str> = provision.secrets.iter().map(|s| s.name.as_str()).collect();
        out.push_str(&format!("{pad}secrets {}\n", names.join(", ")));
    }
    if !provision.mcp_servers.is_empty() {
        let names: Vec<&str> = provision
            .mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        out.push_str(&format!("{pad}mcp {}\n", names.join(", ")));
    }
    if let Some(text) = &provision.instructions {
        let files: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("## From "))
            .collect();
        match files.is_empty() {
            true => out.push_str(&format!("{pad}instructions: its seat\n")),
            false => out.push_str(&format!(
                "{pad}instructions: its seat, {}\n",
                files.join(", ")
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
version = 1
name = "team"
root = "lead"

[seats.lead]
harness = "gemini-cli"
delegates_to = ["worker"]

[seats.worker]
"#;

    fn refused(text: &str, field: &str, needle: &str) -> RigError {
        let result = parse(text).and_then(|spec| plan(&spec));
        match result {
            Err(error) if error.field == field && error.message.contains(needle) => error,
            other => panic!("expected {field}: {needle:?}, got {other:?}"),
        }
    }

    /// `MINIMAL` with `line` added to its table `[table]`.
    fn with(table: &str, line: &str) -> String {
        let header = format!("[{table}]\n");
        assert!(MINIMAL.contains(&header), "{table}");
        MINIMAL.replacen(&header, &format!("{header}{line}\n"), 1)
    }

    #[test]
    fn a_minimal_rig_lowers_to_a_root_with_one_seat() {
        let plan = plan(&parse(MINIMAL).unwrap()).unwrap();
        assert_eq!(plan.root.seat, "lead");
        assert_eq!(plan.root.name, "team");
        assert_eq!(plan.root.profile, "gemini-cli-acp");
        assert_eq!(plan.root.policy.default, Fallback::Deny);
        let seats = plan.seats.unwrap();
        // A child defaults to its parent's harness.
        assert_eq!(seats.table["worker"].harness, "gemini-cli");
        assert_eq!(seats.delegates_to, ["worker"]);
        assert_eq!(
            plan.root.delegation,
            Some(Envelope {
                max_depth: 1,
                max_children: 1,
                harnesses: vec!["gemini-cli".into()],
            })
        );
        let worker = seats.table["worker"].provision.as_ref().unwrap();
        let text = worker.instructions.as_deref().unwrap();
        assert!(text.contains("You fill seat `worker` in the Branchyard rig `team`"));
        assert!(text.contains("You do not delegate"));
        let lead = plan.root.provision.instructions.unwrap();
        assert!(lead.contains("- `worker` (gemini-cli)"), "{lead}");
    }

    #[test]
    fn escalates_to_reaches_an_ancestor_seat_beyond_the_parent() {
        const THREE_LEVELS: &str = r#"
version = 1
name = "team"
root = "lead"

[seats.lead]
harness = "gemini-cli"
delegates_to = ["mid"]

[seats.mid]
delegates_to = ["leaf"]

[seats.leaf]
escalates_to = ["lead"]
"#;
        let plan = plan(&parse(THREE_LEVELS).unwrap()).unwrap();
        let seats = plan.seats.unwrap();
        assert_eq!(seats.table["leaf"].escalates_to, ["lead"]);

        refused(
            &THREE_LEVELS.replace("escalates_to = [\"lead\"]", "escalates_to = [\"mid\"]"),
            "seats.leaf.escalates_to",
            "not an ancestor",
        );
        refused(
            &THREE_LEVELS.replace("escalates_to = [\"lead\"]", "escalates_to = [\"gone\"]"),
            "seats.leaf.escalates_to",
            "is not a seat",
        );
        refused(
            &THREE_LEVELS.replace("[seats.lead]", "[seats.lead]\nescalates_to = [\"mid\"]"),
            "seats.lead.escalates_to",
            "no ancestor seat",
        );
    }

    #[test]
    fn fields_are_checked_by_name_type_and_value() {
        refused(
            &MINIMAL.replace("name = \"team\"", "name = 1"),
            "name",
            "must be a string",
        );
        let error = refused(
            "version = 1\nname = \"t\"\nroot = \"a\"\n[seats.a]\nbogus = 1\n",
            "seats.a.bogus",
            "unknown field",
        );
        assert_eq!(error.line, Some(5));
        assert!(error
            .to_string()
            .starts_with("line 5: seats.a.bogus: unknown field"));
        refused(
            "name = \"t\"\nroot = \"a\"\n[seats.a]\n",
            "version",
            "required",
        );
        refused(
            "version = 2\nname = \"t\"\nroot = \"a\"\n[seats.a]\n",
            "version",
            "reads version 1",
        );
        refused(
            &MINIMAL.replace("name = \"team\"", "name = \"Team\""),
            "name",
            "not a usable name",
        );
        refused(
            &MINIMAL.replace("root = \"lead\"", "root = \"boss\""),
            "root",
            "boss is not a seat",
        );
        refused(
            &with("seats.worker", "harness = 7"),
            "seats.worker.harness",
            "must be a string",
        );
        refused(
            &with("seats.worker", "harness = \"nope\""),
            "seats.worker.harness",
            "no harness or profile named nope",
        );
        refused(
            &with("seats.worker", "budget = { max_usd = -1 }"),
            "seats.worker.budget.max_usd",
            "positive",
        );
        refused(
            &with("seats.worker", "budget = { usd = 1 }"),
            "seats.worker.budget.usd",
            "unknown field",
        );
        refused(
            &with("seats.worker", "instances = 0"),
            "seats.worker.instances",
            "from 1",
        );
        refused(
            &with("seats.worker", "effort = \"huge\""),
            "seats.worker.effort",
            "",
        );
        refused(
            &with("seats.worker", "check = []"),
            "seats.worker.check",
            "needs a command",
        );
        refused(
            &with("seats.worker", "secrets = [\"KEY=OTHER\"]"),
            "seats.worker.secrets",
            "names secrets only",
        );
        refused(
            &with("seats.worker", "mcp = { docs = \"relative/cmd\" }"),
            "seats.worker.mcp.docs",
            "",
        );
        refused(
            &with("seats.worker", "startup = { files = [\"../outside.md\"] }"),
            "seats.worker.startup.files[0]",
            "without ..",
        );
        refused(
            &with("seats.worker", "startup = { files = [\"/etc/passwd\"] }"),
            "seats.worker.startup.files[0]",
            "relative path",
        );
        refused(
            &with("seats.worker", "startup = { actions = [] }"),
            "seats.worker.startup.actions",
            "not supported",
        );
        refused(
            &with("seats.worker", "pod = \"nope\""),
            "seats.worker.pod",
            "nope is not a pod",
        );
        refused(
            "version = 1\nname = \"t\"\nroot = \"a\"\n[seats.a]\n[pods.p]\nbogus = 1\n",
            "pods.p.bogus",
            "unknown field",
        );
        refused(
            "version = 1\nname = \"t\"\nroot = \"a\"\nseats = 3\n",
            "seats",
            "must be a table",
        );
        refused(
            "version = 1\nname = \"t\"\nroot = \"a\"\n[seats.a]\n[seats.a]\n",
            "",
            "not valid TOML",
        );
    }

    #[test]
    fn what_branchyard_cannot_honor_is_refused_by_name() {
        for (field, needle) in [
            (
                "collaborates_with = [\"lead\"]",
                "no messaging between branches",
            ),
            ("can_observe = [\"lead\"]", "no read grant"),
            ("spawned_by = \"lead\"", "seats.<parent>.delegates_to"),
            ("continuity_policy = {}", "does not rebuild a conversation"),
            (
                "permission_policy = \"builtin:yolo\"",
                "presets are not implemented",
            ),
            ("cwd = \".\"", "own git worktree"),
            ("command = \"x\"", "a seat names a harness"),
            ("runtime = \"codex\"", "harness"),
            ("provider = \"microsandbox\"", "sandbox providers"),
            ("start = \"eager\"", "eager seats are not supported"),
            (
                "restore_policy = \"relaunch_fresh\"",
                "fails rather than start a fresh one",
            ),
            ("restore_policy = \"checkpoint_only\"", "not supported"),
            ("policy = { default = \"yolo\" }", "never bypasses"),
        ] {
            let key = field.split(' ').next().unwrap();
            let expected = match key {
                "policy" => "seats.worker.policy.default".to_owned(),
                other => format!("seats.worker.{other}"),
            };
            refused(&with("seats.worker", field), &expected, needle);
        }
        for (field, needle) in [
            ("culture_file = \"c.md\"", "no culture files"),
            ("services = {}", "does not manage services"),
            ("edges = []", "delegates_to"),
            ("permission_policy = \"builtin:standard\"", "presets"),
        ] {
            let text = format!("{field}\n{MINIMAL}");
            refused(&text, field.split(' ').next().unwrap(), needle);
        }
        // Accepted values of the same fields.
        let accepted = with(
            "seats.worker",
            "start = \"on_demand\"\nrestore_policy = \"resume_if_possible\"",
        );
        plan(&parse(&accepted).unwrap()).unwrap();
    }

    #[test]
    fn the_seats_must_form_a_tree_below_the_root() {
        let base = "version = 1\nname = \"t\"\nroot = \"a\"\n";
        let spec = |seats: &str| format!("{base}{seats}");
        refused(
            &spec("[seats.a]\ndelegates_to = [\"x\"]\n"),
            "seats.a.delegates_to",
            "x is not a seat",
        );
        refused(
            &spec("[seats.a]\ndelegates_to = [\"a\"]\n"),
            "seats.a.delegates_to",
            "root seat",
        );
        refused(
            &spec("[seats.a]\ndelegates_to = [\"b\"]\n[seats.b]\ndelegates_to = [\"b\"]\n"),
            "seats.b.delegates_to",
            "cannot delegate to itself",
        );
        refused(
            &spec("[seats.a]\ndelegates_to = [\"b\", \"b\"]\n[seats.b]\n"),
            "seats.a.delegates_to",
            "listed twice",
        );
        refused(
            &spec("[seats.a]\ndelegates_to = [\"b\", \"c\"]\n[seats.b]\ndelegates_to = [\"d\"]\n[seats.c]\ndelegates_to = [\"d\"]\n[seats.d]\n"),
            "seats.c.delegates_to",
            "already below b",
        );
        refused(&spec("[seats.a]\n[seats.b]\n"), "seats.b", "below no seat");
        refused(
            &spec(
                "[seats.a]\n[seats.b]\ndelegates_to = [\"c\"]\n[seats.c]\ndelegates_to = [\"b\"]\n",
            ),
            "seats.b",
            "cycle",
        );
    }

    #[test]
    fn limits_fit_in_the_parents_and_policies_only_narrow() {
        let spec = |lead: &str, worker: &str| {
            format!(
                "version = 1\nname = \"t\"\nroot = \"lead\"\n[seats.lead]\ndelegates_to = [\"worker\"]\n{lead}\n[seats.worker]\n{worker}\n"
            )
        };
        refused(
            &spec("budget = { max_usd = 1 }", ""),
            "seats.worker.budget",
            "needs budget.max_usd",
        );
        refused(
            &spec(
                "budget = { max_usd = 1 }",
                "budget = { max_usd = 0.6 }\ninstances = 2",
            ),
            "seats.lead.budget.max_usd",
            "reserve $1.20 at once (2 x worker), more than its $1.00",
        );
        plan(
            &parse(&spec(
                "budget = { max_usd = 1.2 }",
                "budget = { max_usd = 0.6 }\ninstances = 2",
            ))
            .unwrap(),
        )
        .unwrap();
        refused(
            &spec("budget = { max_turns = 3 }", "budget = { max_turns = 4 }"),
            "seats.worker.budget.max_turns",
            "4 exceeds seat lead's 3",
        );
        refused(
            &spec(
                "budget = { max_minutes = 3 }",
                "budget = { max_minutes = 4 }",
            ),
            "seats.worker.budget.max_minutes",
            "exceeds seat lead's",
        );
        refused(
            &spec("", "policy = { allow = [\"Bash\"] }"),
            "seats.worker.policy.allow",
            "may only add",
        );
        refused(
            &spec("", "policy = { default = \"allow\" }"),
            "seats.worker.policy.default",
            "may only add",
        );
        refused(
            &spec("", "policy = { delegation_commands = true }"),
            "seats.worker.policy.delegation_commands",
            "may only add",
        );
        refused(
            &spec("", "policy = { deny = [\"a*b\"] }"),
            "seats.worker.policy.deny",
            "not a tool name",
        );
        refused(
            &spec("", "secrets = [\"OPENAI_API_KEY\"]"),
            "seats.worker.secrets",
            "isolated = true",
        );
        let isolated =
            plan(&parse(&spec("isolated = true", "secrets = [\"OPENAI_API_KEY\"]")).unwrap())
                .unwrap();
        let worker = &isolated.seats.unwrap().table["worker"];
        assert_eq!(
            worker.provision.as_ref().unwrap().secrets[0].name,
            "OPENAI_API_KEY"
        );
        assert!(
            !worker.isolated,
            "isolation is inherited at spawn, not copied"
        );
        let denied =
            plan(&parse(&spec("", "policy = { deny = [\"Edit\", \"mcp__*\"] }")).unwrap()).unwrap();
        assert_eq!(
            denied.seats.unwrap().table["worker"].deny,
            ["Edit", "mcp__*"]
        );
    }

    #[test]
    fn startup_files_become_instructions_in_order() {
        let dir = std::env::temp_dir().join(format!("by-rig-unit-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("g")).unwrap();
        std::fs::write(dir.join("g/all.md"), "ALL\n").unwrap();
        std::fs::write(dir.join("g/pod.md"), "POD\n").unwrap();
        std::fs::write(dir.join("g/own.md"), "OWN\n").unwrap();
        let text = "version = 1\nname = \"t\"\nroot = \"lead\"\n\
            [startup]\nfiles = [\"g/all.md\"]\n\
            [pods.p]\nstartup = { files = [\"g/pod.md\", { path = \"g/gone.md\", required = false }] }\n\
            [seats.lead]\ndelegates_to = [\"w\"]\n\
            [seats.w]\npod = \"p\"\nstartup = { files = [\"./g/own.md\"] }\n";
        std::fs::write(dir.join("rig.toml"), text).unwrap();
        let plan = plan(&load(&dir.join("rig.toml")).unwrap()).unwrap();
        assert_eq!(plan.skipped_files, ["g/gone.md"]);
        let seats = plan.seats.unwrap();
        let w = seats.table["w"]
            .provision
            .as_ref()
            .unwrap()
            .instructions
            .clone()
            .unwrap();
        let at = |needle: &str| w.find(needle).unwrap_or_else(|| panic!("{needle} in {w}"));
        assert!(at("# Rig t: seat w") < at("## From g/all.md\n\nALL"));
        assert!(at("ALL") < at("## From g/pod.md\n\nPOD"));
        assert!(at("POD") < at("## From ./g/own.md\n\nOWN"));
        let lead = plan.root.provision.instructions.unwrap();
        assert!(lead.contains("ALL") && !lead.contains("POD"), "{lead}");
        // A required file that is missing names its field.
        std::fs::remove_file(dir.join("g/own.md")).unwrap();
        let error = load(&dir.join("rig.toml")).unwrap_err();
        assert_eq!(error.field, "seats.w.startup.files[0]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The example rigs lower to the plans in `tests/golden/`. Rewrite them
    /// with `BY_UPDATE_GOLDEN=1`, then review the diff.
    #[test]
    fn the_example_rigs_lower_to_their_golden_plans() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let examples = root.join("../../examples/rigs");
        let mut names: Vec<String> = std::fs::read_dir(&examples)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".toml"))
            .collect();
        names.sort();
        assert!(names.len() >= 2, "{names:?}");
        for name in names {
            let plan = plan(&load(&examples.join(&name)).unwrap())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let text = format!("{}\n", serde_json::to_string_pretty(&plan).unwrap());
            let golden = root
                .join("tests/golden")
                .join(name.replace(".toml", ".plan.json"));
            if std::env::var_os("BY_UPDATE_GOLDEN").is_some() {
                std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
                std::fs::write(&golden, &text).unwrap();
            }
            let expected = std::fs::read_to_string(&golden).unwrap_or_else(|e| {
                panic!("{}: {e}; run with BY_UPDATE_GOLDEN=1", golden.display())
            });
            assert_eq!(
                text,
                expected,
                "{name} no longer lowers to {}",
                golden.display()
            );
        }
    }
}
