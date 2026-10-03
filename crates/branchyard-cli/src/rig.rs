//! `by rig`: a declarative team of harnesses, lowered onto one root branch.
//!
//! A rig spec is a TOML file naming a root seat and the seats below it,
//! joined by `delegates_to` edges that form a tree. [`parse`] reads it
//! strictly: an unknown field, a field Branchyard cannot honor, or a value
//! of the wrong type is an error naming the field, its line and column.
//! [`validate`] checks and plans a spec held in memory. [`load`]
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
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use toml_edit::{Item, TableLike};

/// The only spec version this build reads.
pub const VERSION: i64 = 1;

/// The harness a root seat runs when it names none.
const DEFAULT_HARNESS: &str = "claude-code";

/// Cost comparisons tolerate float rounding of this much.
const EPSILON_USD: f64 = 1e-9;

/// Names: lowercase letters, digits and hyphens, starting with a letter or
/// digit; at most this long.
const NAME_MAX: usize = 40;

/// Why a spec was refused: the field, where it is when known, and the
/// reason.
#[derive(Clone, Debug, PartialEq)]
pub struct RigError {
    /// The field's dotted path, such as `seats.lead.budget.max_usd` or
    /// `startup.files[0]`; empty for the file as a whole.
    pub field: String,
    /// 1-based.
    pub line: Option<usize>,
    /// 1-based, in characters; known whenever `line` is.
    pub column: Option<usize>,
    pub message: String,
}

/// A 1-based line and column in the spec's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pos {
    line: usize,
    column: usize,
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

fn error(field: impl Into<String>, at: Option<Pos>, message: impl Into<String>) -> RigError {
    RigError {
        field: field.into(),
        line: at.map(|at| at.line),
        column: at.map(|at| at.column),
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
    root_at: Option<Pos>,
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
    at: Option<Pos>,
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
    /// Connector grants (`--connector` form); a seat's is narrowed to its
    /// parent seat's when it is spawned.
    pub connectors: Vec<branchyard::connectors::GrantEntry>,
    /// The hosts its harness may reach; unset, its parent's (the root's:
    /// open). Within its parent seat's.
    pub network: Option<branchyard::Network>,
    /// The models its harness may call through the model gateway; unset,
    /// its parent's (the root's: off the gateway). Within its parent
    /// seat's.
    pub models: Option<branchyard::models::ModelAccess>,
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
    /// Where the seat's table and fields are, by dotted field name
    /// relative to the seat (`""` for the seat itself).
    lines: BTreeMap<String, Pos>,
}

impl SeatSpec {
    fn field(&self, name: &str) -> String {
        match name.is_empty() {
            true => format!("seats.{}", self.name),
            false => format!("seats.{}.{name}", self.name),
        }
    }

    fn at(&self, name: &str) -> Option<Pos> {
        self.lines.get(name).or_else(|| self.lines.get("")).copied()
    }

    fn error(&self, name: &str, message: impl Into<String>) -> RigError {
        error(self.field(name), self.at(name), message)
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
    /// `permission_policy`: a named preset whose rules follow these (a
    /// child seat's contributes its denials only).
    pub preset: Option<branchyard::PolicyPreset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// The preset `permission_policy` named, whose rules are in `deny`,
    /// `allow` and `default` already.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<branchyard::PolicyPreset>,
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
        "a preset belongs to a seat: set seats.<name>.permission_policy",
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

/// Startup-table fields refused with a reason.
const REFUSED_STARTUP: &[(&str, &str)] = &[(
    "actions",
    "startup actions are not supported: the prompt you give is the first message, and drivers \
     refuse slash commands",
)];

/// Startup-file fields refused with a reason.
const REFUSED_FILE: &[(&str, &str)] = &[(
    "delivery_hint",
    "every startup file is delivered as standing instructions; skills and first-message text \
     are not supported",
)];

/// Parse a rig spec. The file's shape (known fields, their types) is
/// checked by deserializing it into the `Raw*` types below; the values
/// are then checked in code. Either way, an error names the field, its
/// line and its column.
pub fn parse(text: &str) -> Result<RigSpec, RigError> {
    let document = toml_edit::Document::parse(text).map_err(|e| {
        let at = e.span().map(|span| pos_of(text, span.start));
        error("", at, format!("not valid TOML: {}", e.message().trim()))
    })?;
    let fields = Fields::index(text, &document);
    let raw = RawRig::deserialize(toml_edit::de::Deserializer::from(document))
        .map_err(|e| fields.refusal(&e))?;
    raw.check(&fields)
}

/// Check the spec `text` as `by rig check` does, without reading any
/// file: parse it and lower it with [`plan`]. Startup files are not read,
/// so the plan lists each of them in `skipped_files` and a missing
/// required one is not an error here; `by rig check` (which uses
/// [`load`]) catches that. For callers that hold a spec in memory, such
/// as `by init rig` validating the file it is about to write.
// Not yet called outside the tests; `by init` backs its rig validator
// with it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn validate(text: &str) -> Result<RigPlan, RigError> {
    plan(&parse(text)?)
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
                    file.at,
                    format!("could not read {}: {e}", full.display()),
                ))
            }
        }
    }
    Ok(spec)
}

/// The 1-based line and column (in characters) of byte `offset`.
fn pos_of(text: &str, offset: usize) -> Pos {
    let before = &text[..floor_char_boundary(text, offset)];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Pos {
        line: before.matches('\n').count() + 1,
        column: before[line_start..].chars().count() + 1,
    }
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Where every key and array element of the document is, by dotted field
/// name (`seats.lead.budget.max_usd`, `startup.files[0]`): the positions
/// errors cite, and what a deserialization error's byte span is mapped
/// back to.
struct Fields {
    entries: Vec<FieldSpan>,
}

struct FieldSpan {
    path: String,
    key: Option<Range<usize>>,
    value: Option<Range<usize>>,
    at: Option<Pos>,
}

impl Fields {
    fn index(text: &str, document: &toml_edit::Document<&str>) -> Fields {
        let mut fields = Fields {
            entries: Vec::new(),
        };
        fields.table(text, document.as_table(), "");
        fields
    }

    fn table(&mut self, text: &str, table: &dyn TableLike, prefix: &str) {
        for (key, item) in table.iter() {
            let path = join(prefix, key);
            let key_span = table.get_key_value(key).and_then(|(k, _)| k.span());
            let value_span = match item {
                Item::Value(value) => value.span(),
                _ => None,
            };
            self.push(text, path.clone(), key_span, value_span);
            match item {
                Item::Table(table) => self.table(text, table, &path),
                Item::Value(value) => self.value(text, value, &path),
                Item::ArrayOfTables(array) => {
                    for (index, table) in array.iter().enumerate() {
                        let path = format!("{path}[{index}]");
                        self.push(text, path.clone(), None, table.span());
                        self.table(text, table, &path);
                    }
                }
                Item::None => {}
            }
        }
    }

    fn value(&mut self, text: &str, value: &toml_edit::Value, path: &str) {
        match value {
            toml_edit::Value::Array(array) => {
                for (index, value) in array.iter().enumerate() {
                    let path = format!("{path}[{index}]");
                    self.push(text, path.clone(), None, value.span());
                    self.value(text, value, &path);
                }
            }
            toml_edit::Value::InlineTable(table) => self.table(text, table, path),
            _ => {}
        }
    }

    fn push(
        &mut self,
        text: &str,
        path: String,
        key: Option<Range<usize>>,
        value: Option<Range<usize>>,
    ) {
        let at = key
            .as_ref()
            .or(value.as_ref())
            .map(|span| pos_of(text, span.start));
        self.entries.push(FieldSpan {
            path,
            key,
            value,
            at,
        });
    }

    /// Where field `path` is written, if it is.
    fn at(&self, path: &str) -> Option<Pos> {
        self.entries
            .iter()
            .find(|entry| entry.path == path)
            .and_then(|entry| entry.at)
    }

    /// Every position below `prefix`, by name relative to it.
    fn below(&self, prefix: &str) -> BTreeMap<String, Pos> {
        let dotted = format!("{prefix}.");
        self.entries
            .iter()
            .filter_map(|entry| {
                let name = entry.path.strip_prefix(&dotted)?;
                Some((name.to_owned(), entry.at?))
            })
            .collect()
    }

    /// The field whose key is at byte `offset`, or else the innermost
    /// field whose value covers it.
    fn covering(&self, offset: usize) -> Option<&FieldSpan> {
        let covers = |span: &Option<Range<usize>>| {
            span.as_ref()
                .is_some_and(|span| span.contains(&offset) || span.start == offset)
        };
        let innermost = |key: bool| {
            self.entries
                .iter()
                .filter(|entry| covers(if key { &entry.key } else { &entry.value }))
                .max_by_key(|entry| entry.path.len())
        };
        innermost(true).or_else(|| innermost(false))
    }

    /// A deserialization error, as the field it is about and why.
    fn refusal(&self, e: &toml_edit::de::Error) -> RigError {
        let found = e.span().and_then(|span| self.covering(span.start));
        let field = found.map(|entry| entry.path.clone()).unwrap_or_default();
        let at = found.and_then(|entry| entry.at);
        let message = describe(&field, e.message());
        error(field, at, message)
    }
}

/// The fields refused with a reason where `field` is.
fn refused_at(field: &str) -> &'static [(&'static str, &'static str)] {
    let parent = field.rsplit_once('.').map_or("", |(parent, _)| parent);
    let seat = parent
        .strip_prefix("seats.")
        .is_some_and(|name| !name.contains(['.', '[']));
    if parent.is_empty() {
        REFUSED_RIG
    } else if seat {
        REFUSED_SEAT
    } else if parent == "startup" || parent.ends_with(".startup") {
        REFUSED_STARTUP
    } else if parent.ends_with(']') && parent.contains("startup.files[") {
        REFUSED_FILE
    } else {
        &[]
    }
}

/// A deserializer's message in this module's words: an unknown field
/// that is refused with a reason gives the reason, and a wrong type says
/// what the field must be.
fn describe(field: &str, message: &str) -> String {
    if let Some(rest) = message.strip_prefix("unknown field `") {
        let (key, rest) = rest.split_once('`').unwrap_or((rest, ""));
        let refused = refused_at(field);
        if let Some((_, why)) = refused.iter().find(|(name, _)| *name == key) {
            return format!("not supported: {why}");
        }
        let expected: Vec<&str> = rest
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|name| !refused.iter().any(|(refused, _)| refused == name))
            .collect();
        return match expected.is_empty() {
            true => "unknown field".to_owned(),
            false => format!("unknown field; expected one of: {}", expected.join(", ")),
        };
    }
    if let Some((got, expected)) = message
        .strip_prefix("invalid type: ")
        .and_then(|rest| rest.rsplit_once(", expected "))
    {
        let expected = match expected {
            "a sequence" => "an array",
            "a map" => "a table",
            "a boolean" => "true or false",
            "i64" | "u32" | "u64" => "a whole number",
            "f64" => "a number",
            other if other.starts_with("struct ") => "a table",
            other => other,
        };
        let got = match got {
            "sequence" => "array".to_owned(),
            "map" => "table".to_owned(),
            other => other.replacen("floating point", "float", 1),
        };
        return format!("must be {expected}, not {got}");
    }
    message.to_owned()
}

// The file's shape. Every table denies fields it does not declare; the
// values are checked by `check` below, so the messages (and the lines
// they cite) are this module's own. With the `schema` feature these types
// also generate `schema/rig.json`; see `schema`.

/// A Branchyard rig spec: a root seat and the seats below it, joined by
/// `delegates_to` edges that form a tree. See docs/rigs.md.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(title = "Branchyard rig spec"))]
#[serde(deny_unknown_fields)]
struct RawRig {
    /// The spec format's version; this build reads 1.
    #[cfg_attr(
        feature = "schema",
        schemars(required, schema_with = "schema::version")
    )]
    version: Option<i64>,
    /// The rig's name, and the root branch's unless `by rig run --name`
    /// gives one.
    #[cfg_attr(feature = "schema", schemars(required, schema_with = "schema::name"))]
    name: Option<String>,
    description: Option<String>,
    /// The name of the root seat, one of `seats`.
    #[cfg_attr(feature = "schema", schemars(required, schema_with = "schema::name"))]
    root: Option<String>,
    /// Startup files for every seat.
    startup: Option<RawStartup>,
    /// Groups of seats sharing startup files, by name.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::named::<RawPod>"))]
    pods: Ordered<RawPod>,
    /// The seats, by name: the root and those below it.
    #[cfg_attr(
        feature = "schema",
        schemars(required, schema_with = "schema::named::<RawSeat>")
    )]
    seats: Option<Ordered<RawSeat>>,
}

/// A group of seats sharing startup files.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Pod"))]
#[serde(deny_unknown_fields)]
struct RawPod {
    description: Option<String>,
    /// Startup files for the pod's seats.
    startup: Option<RawStartup>,
}

/// Files delivered to a harness as standing instructions, never written
/// into a worktree.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Startup"))]
#[serde(deny_unknown_fields)]
struct RawStartup {
    /// Paths relative to the spec's directory, without `..`; a bare path
    /// is required.
    files: Option<Vec<RawFile>>,
}

/// A startup file: a path (required), or `{ path, required }`.
enum RawFile {
    Path(String),
    Table(RawFileTable),
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "StartupFileTable"))]
#[serde(deny_unknown_fields)]
struct RawFileTable {
    #[cfg_attr(feature = "schema", schemars(required, with = "String"))]
    path: Option<String>,
    /// A missing optional file is skipped; a missing required one (the
    /// default) is an error.
    required: Option<bool>,
}

/// One seat.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Seat"))]
#[serde(deny_unknown_fields)]
struct RawSeat {
    /// The seat's role, given to its harness and to the parent's.
    description: Option<String>,
    /// The pod whose startup files this seat also gets.
    pod: Option<String>,
    /// A harness or profile ID; a child defaults to its parent's, the
    /// root to claude-code.
    harness: Option<String>,
    model: Option<String>,
    /// low, medium, high, xhigh, or a level from 0 to 100.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::effort"))]
    effort: Option<RawEffort>,
    /// A credential name the server resolves.
    auth: Option<String>,
    /// Names of secrets to write into the branch's private home; needs
    /// isolated = true here or above.
    secrets: Option<Vec<String>>,
    /// Stdio MCP servers, as name = "command line".
    #[serde(default)]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "schema::ordered::<String>")
    )]
    mcp: Option<Ordered<String>>,
    /// Connectors the seat's harness may call through the gateway, as
    /// `by run --connector` takes them (github:read, github:write:issues.*);
    /// each must be within its parent seat's. Needs isolated = true here
    /// or above.
    connectors: Option<Vec<String>>,
    /// The hosts the seat's harness may reach (docs/egress.md): "open",
    /// "none", or { allow = ["github.com", "*.npmjs.org:443"], enforce =
    /// "required" }. Unset: its parent seat's. Within its parent seat's.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::network"))]
    network: Option<branchyard::Network>,
    /// Models the seat's harness may call through the model gateway
    /// (docs/model-gateway.md), as globs ("claude-*"); its model calls then
    /// go through the gateway. Unset: its parent seat's. Within its parent
    /// seat's.
    models: Option<Vec<String>>,
    /// `off`, or an http:// or https:// OTLP collector endpoint.
    telemetry: Option<String>,
    /// Run in a home private to the branch; inherited by the seats below.
    isolated: Option<bool>,
    budget: Option<RawBudget>,
    /// The check a branch must pass to integrate: a command line, or its
    /// words.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::check"))]
    check: Option<RawCheck>,
    policy: Option<RawPolicy>,
    /// A permission preset: read-only, edit-worktree or full. Its rules
    /// follow the seat's own policy; a child seat takes its denials only.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(schema_with = "schema::preset"))]
    permission_policy: Option<String>,
    /// The seats this seat may spawn.
    delegates_to: Option<Vec<String>>,
    /// Ancestor seats, besides its parent, this seat may escalate to.
    escalates_to: Option<Vec<String>>,
    /// How many of this seat may run at once; 1 by default.
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    instances: Option<i64>,
    /// Scratch areas, as NAME:read_only or NAME:exclusive_write.
    bindings: Option<Vec<String>>,
    /// Startup files for this seat.
    startup: Option<RawStartup>,
    /// Only on_demand: the root starts alone and fills seats as it spawns.
    #[cfg_attr(feature = "schema", schemars(with = "Option<schema::Start>"))]
    start: Option<String>,
    /// Only resume_if_possible: a send resumes the branch's session.
    #[cfg_attr(feature = "schema", schemars(with = "Option<schema::Restore>"))]
    restore_policy: Option<String>,
}

/// Limits for one branch in this seat.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Budget"))]
#[serde(deny_unknown_fields)]
struct RawBudget {
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    max_usd: Option<f64>,
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    max_turns: Option<i64>,
    #[cfg_attr(feature = "schema", schemars(range(min = 0)))]
    max_minutes: Option<f64>,
}

/// How permission requests are answered. A child seat may set only deny.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Policy"))]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    /// Requests no rule decides: allow, deny (the default) or ask.
    #[cfg_attr(feature = "schema", schemars(with = "Option<Fallback>"))]
    default: Option<String>,
    /// Tool names, or prefixes ending in *, denied outright.
    deny: Option<Vec<String>>,
    /// Tool names, or prefixes ending in *, allowed after deny.
    allow: Option<Vec<String>>,
    /// Allow the harness's own `by` delegation commands.
    delegation_commands: Option<bool>,
}

/// `effort`: a name or a level.
enum RawEffort {
    Name(String),
    Level(i64),
}

/// `check`: a command line, or its words.
enum RawCheck {
    Line(String),
    Words(Vec<String>),
}

/// A table whose keys are names, in the order the file writes them.
struct Ordered<T>(Vec<(String, T)>);

impl<T> Default for Ordered<T> {
    fn default() -> Self {
        Ordered(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Ordered<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Tables<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for Tables<T> {
            type Value = Ordered<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered<T>, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Ordered(entries))
            }
        }
        deserializer.deserialize_map(Tables(std::marker::PhantomData))
    }
}

impl<'de> Deserialize<'de> for RawFile {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct File;
        impl<'de> Visitor<'de> for File {
            type Value = RawFile;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a path or {path, required}")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<RawFile, E> {
                Ok(RawFile::Path(text.to_owned()))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<RawFile, A::Error> {
                RawFileTable::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(RawFile::Table)
            }
        }
        deserializer.deserialize_any(File)
    }
}

impl<'de> Deserialize<'de> for RawEffort {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Level;
        impl Visitor<'_> for Level {
            type Value = RawEffort;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("low, medium, high, xhigh, or 0-100")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<RawEffort, E> {
                Ok(RawEffort::Name(text.to_owned()))
            }
            fn visit_i64<E: de::Error>(self, level: i64) -> Result<RawEffort, E> {
                Ok(RawEffort::Level(level))
            }
        }
        deserializer.deserialize_any(Level)
    }
}

impl<'de> Deserialize<'de> for RawCheck {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Command;
        impl<'de> Visitor<'de> for Command {
            type Value = RawCheck;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a command line or an array of its words")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<RawCheck, E> {
                Ok(RawCheck::Line(text.to_owned()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<RawCheck, A::Error> {
                let mut words = Vec::new();
                while let Some(word) = seq.next_element()? {
                    words.push(word);
                }
                Ok(RawCheck::Words(words))
            }
        }
        deserializer.deserialize_any(Command)
    }
}

impl RawRig {
    /// The values: versions, names, paths, limits, and what each field
    /// may hold beyond its type.
    fn check(self, fields: &Fields) -> Result<RigSpec, RigError> {
        match self.version {
            None => {
                return Err(error(
                    "version",
                    None,
                    format!("required; write version = {VERSION}"),
                ))
            }
            Some(VERSION) => {}
            Some(v) => {
                return Err(error(
                    "version",
                    fields.at("version"),
                    format!("this by reads version {VERSION}, not {v}"),
                ))
            }
        }
        let name = self
            .name
            .ok_or_else(|| error("name", None, "required: the rig's name"))?;
        let name = named(name, "name", fields.at("name"))?;
        let root = self
            .root
            .ok_or_else(|| error("root", None, "required: the name of the root seat"))?;
        let startup = startup(self.startup, "startup", fields)?;
        let mut pods = BTreeMap::new();
        for (pod, raw) in self.pods.0 {
            let field = join("pods", &pod);
            named(pod.clone(), &field, fields.at(&field))?;
            let startup = startup_at(raw.startup, &field, fields)?;
            pods.insert(
                pod,
                Pod {
                    description: raw.description,
                    startup,
                },
            );
        }
        let raw_seats = self
            .seats
            .ok_or_else(|| error("seats", None, "required: at least the root seat"))?;
        let mut seats = Vec::new();
        for (seat, raw) in raw_seats.0 {
            let field = join("seats", &seat);
            named(seat.clone(), &field, fields.at(&field))?;
            seats.push(raw.check(seat, &field, fields)?);
        }
        Ok(RigSpec {
            name,
            description: self.description,
            root,
            root_at: fields.at("root"),
            startup,
            pods,
            seats,
        })
    }
}

/// The startup files of the table at `field` (`startup` below it).
fn startup_at(
    raw: Option<RawStartup>,
    field: &str,
    fields: &Fields,
) -> Result<Vec<StartupFile>, RigError> {
    startup(raw, &join(field, "startup"), fields)
}

fn startup(
    raw: Option<RawStartup>,
    field: &str,
    fields: &Fields,
) -> Result<Vec<StartupFile>, RigError> {
    let files = raw.and_then(|raw| raw.files).unwrap_or_default();
    let mut out = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let entry = format!("{field}.files[{index}]");
        let at = fields.at(&entry);
        let (path, required) = match file {
            RawFile::Path(path) => (path, true),
            RawFile::Table(table) => {
                let path = table
                    .path
                    .ok_or_else(|| error(&entry, at, "needs a path"))?;
                (path, table.required.unwrap_or(true))
            }
        };
        safe_path(&path, &entry, at)?;
        out.push(StartupFile {
            path,
            required,
            content: None,
            field: entry,
            at,
        });
    }
    Ok(out)
}

impl RawSeat {
    fn check(self, name: String, field: &str, fields: &Fields) -> Result<SeatSpec, RigError> {
        let mut lines = fields.below(field);
        if let Some(at) = fields.at(field) {
            lines.insert(String::new(), at);
        }
        let mut seat = SeatSpec {
            name,
            instances: 1,
            lines,
            ..SeatSpec::default()
        };
        let fail = |name: &str, message: String| {
            error(join(field, name), fields.at(&join(field, name)), message)
        };
        seat.description = self.description;
        seat.pod = self.pod;
        seat.harness = nonempty(self.harness).map_err(|m| fail("harness", m))?;
        seat.model = nonempty(self.model).map_err(|m| fail("model", m))?;
        seat.auth = nonempty(self.auth).map_err(|m| fail("auth", m))?;
        seat.effort = match self.effort {
            None => None,
            Some(RawEffort::Name(text)) => {
                Some(Effort::parse(&text).map_err(|e| fail("effort", e))?)
            }
            Some(RawEffort::Level(level)) if (0..=100).contains(&level) => {
                Some(Effort::from_level(level))
            }
            Some(RawEffort::Level(_)) => {
                return Err(fail(
                    "effort",
                    "must be low, medium, high, xhigh, or 0-100".into(),
                ))
            }
        };
        for secret in self.secrets.unwrap_or_default() {
            let parsed = SecretSource::parse(&secret).map_err(|e| fail("secrets", e))?;
            if parsed.from.is_some() {
                return Err(fail(
                    "secrets",
                    format!(
                        "{secret}: a rig names secrets only; where each comes from is your \
                         environment's (locally) or the server operator's"
                    ),
                ));
            }
            seat.secrets.push(secret);
        }
        for (server, command) in self.mcp.unwrap_or_default().0 {
            let spec = McpServerSpec::parse(&format!("{server}={command}"))
                .and_then(|spec| spec.check().map(|()| spec))
                .map_err(|e| fail(&format!("mcp.{server}"), e))?;
            seat.mcp.push(spec);
        }
        for grant in self.connectors.unwrap_or_default() {
            let entry = branchyard::connectors::GrantEntry::parse(&grant)
                .map_err(|e| fail("connectors", e))?;
            seat.connectors.push(entry);
        }
        seat.network = self.network;
        if let Some(models) = self.models {
            let access = branchyard::models::ModelAccess { allow: models };
            access.check().map_err(|e| fail("models", e))?;
            seat.models = Some(access);
        }
        if let Some(text) = self.telemetry {
            seat.telemetry = Some(Telemetry::parse(&text).map_err(|e| fail("telemetry", e))?);
        }
        seat.isolated = self.isolated.unwrap_or(false);
        if let Some(budget) = self.budget {
            let positive = |value: Option<f64>, name: &str| match value {
                Some(value) if !(value.is_finite() && value > 0.0) => Err(fail(
                    name,
                    format!("must be a positive number, not {value}"),
                )),
                other => Ok(other),
            };
            seat.budget.max_usd = positive(budget.max_usd, "budget.max_usd")?;
            seat.budget.max_minutes = positive(budget.max_minutes, "budget.max_minutes")?;
            seat.budget.max_turns = budget
                .max_turns
                .map(|n| from_one(n).ok_or_else(|| fail("budget.max_turns", from_one_message())))
                .transpose()?;
        }
        if let Some(check) = self.check {
            let argv = match check {
                RawCheck::Line(text) => {
                    crate::args::split_words(&text).map_err(|e| fail("check", e))?
                }
                RawCheck::Words(words) => words,
            };
            if argv.is_empty() || argv[0].is_empty() {
                return Err(fail("check", "needs a command".into()));
            }
            seat.check = Some(argv);
        }
        if let Some(policy) = self.policy {
            seat.policy.default = match policy.default.as_deref() {
                None => None,
                Some("allow") => Some(Fallback::Allow),
                Some("deny") => Some(Fallback::Deny),
                Some("ask") => Some(Fallback::Ask),
                Some(other) => {
                    return Err(fail(
                        "policy.default",
                        format!(
                            "must be allow, deny or ask, not {other}; Branchyard answers every \
                             request and never bypasses permissions"
                        ),
                    ))
                }
            };
            seat.policy.deny = tools(policy.deny).map_err(|m| fail("policy.deny", m))?;
            seat.policy.allow = tools(policy.allow).map_err(|m| fail("policy.allow", m))?;
            seat.policy.delegation_commands = policy.delegation_commands.unwrap_or(false);
        }
        if let Some(name) = self.permission_policy {
            seat.policy.preset = Some(
                branchyard::PolicyPreset::parse(&name).map_err(|e| fail("permission_policy", e))?,
            );
        }
        seat.delegates_to = self.delegates_to.unwrap_or_default();
        let mut seen = BTreeSet::new();
        for target in &seat.delegates_to {
            if !seen.insert(target) {
                return Err(fail("delegates_to", format!("{target} is listed twice")));
            }
        }
        seat.escalates_to = self.escalates_to.unwrap_or_default();
        if let Some(n) = self.instances {
            seat.instances = from_one(n).ok_or_else(|| fail("instances", from_one_message()))?;
        }
        for text in self.bindings.unwrap_or_default() {
            let binding = branchyard::Binding::parse(&text).map_err(|e| fail("bindings", e))?;
            if seat.bindings.iter().any(|b| b.scratch == binding.scratch) {
                return Err(fail(
                    "bindings",
                    format!("scratch area {} is bound twice", binding.scratch),
                ));
            }
            seat.bindings.push(binding);
        }
        seat.startup = startup_at(self.startup, field, fields)?;
        match self.start.as_deref() {
            None | Some("on_demand") => {}
            Some("eager") => {
                return Err(fail(
                    "start",
                    "eager seats are not supported: the root starts alone and fills seats with \
                     by spawn --seat; use on_demand"
                        .into(),
                ))
            }
            Some(other) => return Err(fail("start", format!("must be on_demand, not {other}"))),
        }
        match self.restore_policy.as_deref() {
            None | Some("resume_if_possible") => {}
            Some(other @ ("relaunch_fresh" | "checkpoint_only")) => {
                return Err(fail(
                    "restore_policy",
                    format!(
                        "{other} is not supported: a send resumes the branch's session, and \
                         fails rather than start a fresh one"
                    ),
                ))
            }
            Some(other) => {
                return Err(fail(
                    "restore_policy",
                    format!("must be resume_if_possible, not {other}"),
                ))
            }
        }
        Ok(seat)
    }
}

fn join(path: &str, key: &str) -> String {
    match path.is_empty() {
        true => key.to_owned(),
        false => format!("{path}.{key}"),
    }
}

fn nonempty(text: Option<String>) -> Result<Option<String>, String> {
    match text {
        Some(text) if text.trim().is_empty() => Err("must not be empty".into()),
        other => Ok(other),
    }
}

/// A whole number from 1, as a `u32`.
fn from_one(n: i64) -> Option<u32> {
    u32::try_from(n).ok().filter(|n| *n > 0)
}

fn from_one_message() -> String {
    "must be a whole number from 1".into()
}

/// Tool patterns: exact names, or a prefix with a trailing `*`.
fn tools(tools: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let tools = tools.unwrap_or_default();
    for tool in &tools {
        if tool.trim().is_empty() || tool[..tool.len().saturating_sub(1)].contains('*') {
            return Err(format!(
                "{tool:?} is not a tool name or a prefix ending in *"
            ));
        }
    }
    Ok(tools)
}

/// A name usable as a branch name and a seat: lowercase letters, digits
/// and hyphens.
fn named(name: String, field: &str, at: Option<Pos>) -> Result<String, RigError> {
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
            at,
            format!(
                "{name:?} is not a usable name: lowercase letters, digits and inner hyphens, at \
                 most {NAME_MAX} characters"
            ),
        )),
    }
}

/// A startup file's path: relative, inside the spec's directory.
fn safe_path(path: &str, field: &str, at: Option<Pos>) -> Result<(), RigError> {
    let parsed = Path::new(path);
    let escapes = parsed
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    match path.is_empty() || escapes {
        true => Err(error(
            field,
            at,
            format!("{path:?} must be a relative path inside the spec's directory, without .."),
        )),
        false => Ok(()),
    }
}

/// `schema/rig.json`, generated from the `Raw*` types above.
#[cfg(feature = "schema")]
pub mod schema {
    use std::borrow::Cow;

    use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};

    use super::{RawFile, RawFileTable, NAME_MAX, VERSION};

    /// A name: lowercase letters, digits and inner hyphens.
    const NAME_PATTERN: &str = "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$";

    /// The JSON Schema document, pretty-printed with a trailing newline,
    /// matching `schema/rig.json` byte for byte. Only the freshness test
    /// calls it; `by` is a binary with no command that prints it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn rig_json() -> String {
        let schema = schemars::schema_for!(super::RawRig);
        let mut value = serde_json::to_value(&schema).expect("a JSON Schema document serializes");
        without_null(&mut value);
        let mut text =
            serde_json::to_string_pretty(&value).expect("a JSON Schema document serializes");
        text.push('\n');
        text
    }

    /// TOML has no null: an optional field is one that may be left out,
    /// so drop the `null` alternatives and defaults schemars adds for
    /// `Option<T>`.
    #[cfg_attr(not(test), allow(dead_code))]
    fn without_null(value: &mut serde_json::Value) {
        use serde_json::Value;
        match value {
            Value::Object(map) => {
                if let Some(Value::Array(types)) = map.get_mut("type") {
                    types.retain(|t| t != "null");
                    if types.len() == 1 {
                        let only = types.remove(0);
                        map.insert("type".into(), only);
                    }
                }
                if let Some(Value::Array(branches)) = map.get_mut("anyOf") {
                    branches.retain(|b| b.get("type").is_none_or(|t| t != "null"));
                    if branches.len() == 1 {
                        let only = branches.remove(0);
                        map.remove("anyOf");
                        if let Value::Object(only) = only {
                            for (key, value) in only {
                                map.entry(key).or_insert(value);
                            }
                        }
                    }
                }
                if let Some(Value::Array(values)) = map.get_mut("enum") {
                    values.retain(|v| !v.is_null());
                }
                if map.get("default").is_some_and(Value::is_null) {
                    map.remove("default");
                }
                map.values_mut().for_each(without_null);
            }
            Value::Array(values) => values.iter_mut().for_each(without_null),
            _ => {}
        }
    }

    pub(super) fn version(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "The spec format's version; this build reads 1.",
            "type": "integer",
            "const": VERSION,
        })
    }

    pub(super) fn name(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": NAME_PATTERN,
            "maxLength": NAME_MAX,
        })
    }

    pub(super) fn named<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "propertyNames": { "pattern": NAME_PATTERN, "maxLength": NAME_MAX },
            "additionalProperties": generator.subschema_for::<T>(),
        })
    }

    pub(super) fn ordered<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "additionalProperties": generator.subschema_for::<T>(),
        })
    }

    pub(super) fn effort(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": "string", "enum": ["low", "medium", "high", "xhigh"] },
                { "type": "integer", "minimum": 0, "maximum": 100 },
            ],
        })
    }

    pub(super) fn preset(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "enum": branchyard::PolicyPreset::ALL.map(|p| p.name()),
        })
    }

    pub(super) fn network(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": "string", "enum": ["open", "none"] },
                {
                    "type": "object",
                    "properties": {
                        "allow": { "type": "array", "items": { "type": "string" } },
                        "enforce": { "type": "string", "enum": ["best_effort", "required"] },
                    },
                    "additionalProperties": false,
                },
            ],
        })
    }

    pub(super) fn check(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": "string", "minLength": 1 },
                { "type": "array", "items": { "type": "string" }, "minItems": 1 },
            ],
        })
    }

    /// `start`'s one accepted value.
    #[allow(dead_code)]
    #[derive(JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub(super) enum Start {
        OnDemand,
    }

    /// `restore_policy`'s one accepted value.
    #[allow(dead_code)]
    #[derive(JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub(super) enum Restore {
        ResumeIfPossible,
    }

    impl JsonSchema for RawFile {
        fn schema_name() -> Cow<'static, str> {
            "StartupFile".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "anyOf": [
                    { "type": "string" },
                    generator.subschema_for::<RawFileTable>(),
                ],
            })
        }
    }
}

/// Lower a loaded spec. Pure: checks the whole spec and builds the root's
/// options and seats, or names the first field it cannot honor.
pub fn plan(spec: &RigSpec) -> Result<RigPlan, RigError> {
    let by_name: BTreeMap<&str, &SeatSpec> =
        spec.seats.iter().map(|s| (s.name.as_str(), s)).collect();
    let root = *by_name
        .get(spec.root.as_str())
        .ok_or_else(|| error("root", spec.root_at, format!("{} is not a seat", spec.root)))?;

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
        if !seat.connectors.is_empty() && !own {
            return Err(seat.error(
                "connectors",
                "connectors are placed only into a home private to the branch; set isolated = \
                 true on this seat or one above it",
            ));
        }
    }
    // A seat's connectors are within its parent seat's, as a spawn will
    // narrow them; one with nothing in common is refused now.
    for seat in &order {
        let Some(up) = parent.get(seat.name.as_str()) else {
            continue;
        };
        let above = order
            .iter()
            .find(|s| s.name == *up)
            .map(|s| s.connectors.clone())
            .unwrap_or_default();
        if !seat.connectors.is_empty() {
            branchyard::connectors::narrow(Some(&seat.connectors), &above)
                .map_err(|why| seat.error("connectors", why))?;
        }
    }
    // So is its network policy: what it would have once spawned, its own
    // or its parent's, refused now if wider.
    let mut networks: BTreeMap<&str, Option<branchyard::Network>> = BTreeMap::new();
    for seat in &order {
        let above = parent
            .get(seat.name.as_str())
            .and_then(|up| networks.get(up).cloned().flatten());
        if let Some(network) = &seat.network {
            network.check().map_err(|why| seat.error("network", why))?;
        }
        let effective = branchyard::network_narrow(seat.network.as_ref(), above.as_ref())
            .map_err(|why| seat.error("network", why))?;
        networks.insert(&seat.name, effective);
    }
    // And its models.
    let mut models: BTreeMap<&str, Option<branchyard::models::ModelAccess>> = BTreeMap::new();
    for seat in &order {
        let above = parent
            .get(seat.name.as_str())
            .and_then(|up| models.get(up).cloned().flatten());
        let effective = branchyard::models::narrow(seat.models.as_ref(), above.as_ref())
            .map_err(|why| seat.error("models", why))?;
        models.insert(&seat.name, effective);
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
                deny: child_denials(&seat.policy),
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
            policy: root_policy(&root.policy),
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
        connectors: seat.connectors.clone(),
        network: seat.network.clone(),
        models: seat.models.clone(),
        instructions: Some(text),
        model: seat.model.clone(),
        effort: seat.effort,
        telemetry: seat.telemetry.clone(),
    }
}

/// The root's policy: its own rules, then its preset's, then the default
/// it names or else its preset's.
fn root_policy(policy: &PolicySpec) -> PolicyPlan {
    let preset = policy.preset.map(|p| p.rules());
    let extend = |own: &[String], more: Option<&[&str]>| {
        let mut all = own.to_vec();
        for tool in more.unwrap_or_default() {
            if !all.iter().any(|t| t == tool) {
                all.push((*tool).to_owned());
            }
        }
        all
    };
    let default = policy.default.unwrap_or(match &preset {
        Some(rules) if rules.default_allow => Fallback::Allow,
        _ => Fallback::Deny,
    });
    PolicyPlan {
        default,
        deny: extend(&policy.deny, preset.as_ref().map(|r| r.deny.as_slice())),
        allow: extend(&policy.allow, preset.as_ref().map(|r| r.allow.as_slice())),
        delegation_commands: policy.delegation_commands,
        preset: policy.preset,
    }
}

/// A child seat's denials: its own, then its preset's.
fn child_denials(policy: &PolicySpec) -> Vec<String> {
    let mut deny = policy.deny.clone();
    for tool in policy.preset.map(|p| p.rules().deny).unwrap_or_default() {
        if !deny.iter().any(|t| t == tool) {
            deny.push(tool.to_owned());
        }
    }
    deny
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
    let preset = policy.preset.map(|p| p.rules());
    let own = |tools: &[String], preset: Option<&Vec<&str>>| -> Vec<String> {
        tools
            .iter()
            .filter(|t| preset.is_none_or(|p| !p.contains(&t.as_str())))
            .cloned()
            .collect()
    };
    if let Some(name) = policy.preset {
        rules.push(format!("preset {name} ({})", name.summary()));
    }
    let deny = own(&policy.deny, preset.as_ref().map(|r| &r.deny));
    if !deny.is_empty() {
        rules.push(format!("deny {}", deny.join(", ")));
    }
    let allow = own(&policy.allow, preset.as_ref().map(|r| &r.allow));
    if !allow.is_empty() {
        rules.push(format!("allow {}", allow.join(", ")));
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
    if !provision.connectors.is_empty() {
        out.push_str(&format!(
            "{pad}connectors {}\n",
            provision
                .connectors
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if let Some(models) = &provision.models {
        out.push_str(&format!("{pad}models {models}\n"));
    }
    if let Some(network) = &provision.network {
        out.push_str(&format!("{pad}network {network}\n"));
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
    fn seat_networks_lower_to_provisioning_and_stay_within_their_parents() {
        let both = |root: &str, worker: &str| {
            with("seats.lead", root).replacen(
                "[seats.worker]\n",
                &format!("[seats.worker]\n{worker}\n"),
                1,
            )
        };
        let planned = plan(
            &parse(&both(
                "network = { allow = [\"*.example.com:443\", \"github.com\"], enforce = \"required\" }",
                "network = { allow = [\"api.example.com:443\"] }",
            ))
            .unwrap(),
        )
        .unwrap();
        let root = planned.root.provision.network.as_ref().unwrap();
        assert_eq!(root.to_string(), "*.example.com:443, github.com (required)");
        let worker = planned.seats.as_ref().unwrap().table["worker"]
            .provision
            .as_ref()
            .unwrap();
        assert_eq!(
            worker.network.as_ref().unwrap().rules(),
            ["api.example.com:443"]
        );
        assert!(render(&planned).contains("network *.example.com:443, github.com (required)"));
        // A seat that names none takes its parent's when it is spawned.
        let silent = plan(&parse(&with("seats.lead", "network = \"none\"")).unwrap()).unwrap();
        let worker = &silent.seats.as_ref().unwrap().table["worker"];
        assert_eq!(worker.provision.as_ref().unwrap().network, None);
        // Wider than its parent's, or not a rule: refused by field and line.
        for (root, worker, needle) in [
            ("network = \"none\"", "network = \"open\"", "open network"),
            (
                "network = { allow = [\"github.com:443\"] }",
                "network = { allow = [\"github.com\"] }",
                "not within the parent",
            ),
            ("", "network = { allow = [\"https://x\"] }", "not a URL"),
            ("", "network = \"closed\"", "is not \"open\""),
            (
                "",
                "network = { enforce = \"required\" }",
                "nothing to enforce",
            ),
        ] {
            let error = refused(&both(root, worker), "seats.worker.network", needle);
            assert!(error.line.is_some(), "{error:?}");
        }
    }

    #[test]
    fn seat_models_lower_to_provisioning_and_stay_within_their_parents() {
        let both = |root: &str, worker: &str| {
            with("seats.lead", root).replacen(
                "[seats.worker]\n",
                &format!("[seats.worker]\n{worker}\n"),
                1,
            )
        };
        let planned = plan(
            &parse(&both(
                "models = [\"claude-*\"]",
                "models = [\"claude-haiku-*\"]",
            ))
            .unwrap(),
        )
        .unwrap();
        let root = planned.root.provision.models.as_ref().unwrap();
        assert_eq!(root.allow, ["claude-*"]);
        let worker = planned.seats.as_ref().unwrap().table["worker"]
            .provision
            .as_ref()
            .unwrap();
        assert_eq!(worker.models.as_ref().unwrap().allow, ["claude-haiku-*"]);
        assert!(render(&planned).contains("models claude-*"));
        for (root, worker, needle) in [
            (
                "models = [\"claude-*\"]",
                "models = [\"gpt-*\"]",
                "not within the parent",
            ),
            ("", "models = [\"a b\"]", "' '"),
        ] {
            let error = refused(&both(root, worker), "seats.worker.models", needle);
            assert!(error.line.is_some(), "{error:?}");
        }
    }

    #[test]
    fn permission_presets_expand_on_the_root_and_deny_on_a_child() {
        let text = with(
            "seats.lead",
            "permission_policy = \"edit-worktree\"\npolicy = { deny = [\"Write\"] }",
        )
        .replacen(
            "[seats.worker]\n",
            "[seats.worker]\npermission_policy = \"read-only\"\n",
            1,
        );
        let planned = plan(&parse(&text).unwrap()).unwrap();
        let policy = &planned.root.policy;
        assert_eq!(policy.preset, Some(branchyard::PolicyPreset::EditWorktree));
        assert_eq!(policy.default, Fallback::Deny);
        // The seat's own rules come first, then the preset's.
        assert_eq!(policy.deny[0], "Write");
        assert!(policy.deny.contains(&"Bash".to_owned()));
        assert!(policy.allow.contains(&"Edit".to_owned()));
        assert!(policy.allow.contains(&"Read".to_owned()));
        let shown = render(&planned);
        assert!(
            shown.contains("policy preset edit-worktree (read, search and edit files"),
            "{shown}"
        );
        assert!(shown.contains("; deny Write; then deny"), "{shown}");
        // A child seat's preset adds its denials only.
        let worker = &planned.seats.as_ref().unwrap().table["worker"];
        for tool in ["Edit", "Bash", "WebFetch"] {
            assert!(worker.deny.contains(&tool.to_owned()), "{tool}");
        }
        // `full` as the root's preset allows by default.
        let full =
            plan(&parse(&with("seats.lead", "permission_policy = \"full\"")).unwrap()).unwrap();
        assert_eq!(full.root.policy.default, Fallback::Allow);
        // The JSON plan names the preset; a plan without one is unchanged.
        let json = serde_json::to_value(&planned.root.policy).unwrap();
        assert_eq!(json["preset"], "edit-worktree");
        let plain = plan(&parse(MINIMAL).unwrap()).unwrap();
        assert!(serde_json::to_value(&plain.root.policy)
            .unwrap()
            .get("preset")
            .is_none());
    }

    #[test]
    fn seat_connectors_are_parsed_isolated_and_within_their_parents() {
        let isolated = |root: &str, worker: &str| {
            let text = with("seats.lead", &format!("isolated = true\n{root}"));
            text.replacen(
                "[seats.worker]\n",
                &format!("[seats.worker]\n{worker}\n"),
                1,
            )
        };
        let planned = plan(
            &parse(&isolated(
                "connectors = [\"github:write:issues.*\"]",
                "connectors = [\"github:read:issues.list\"]",
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            planned.root.provision.connectors[0].to_string(),
            "github:write:issues.*"
        );
        let seats = planned.seats.unwrap();
        let worker = seats.table["worker"].provision.as_ref().unwrap();
        assert_eq!(worker.connectors[0].to_string(), "github:read:issues.list");
        assert!(render(
            &plan(&parse(&isolated("connectors = [\"github\"]", "")).unwrap()).unwrap()
        )
        .contains("connectors github:read"));
        // Outside the parent seat's grant.
        refused(
            &isolated(
                "connectors = [\"github:read\"]",
                "connectors = [\"slack:read\"]",
            ),
            "seats.worker.connectors",
            "not within the parent's grant",
        );
        refused(
            &with("seats.worker", "connectors = [\"github:admin\"]"),
            "seats.worker.connectors",
            "mode",
        );
        refused(
            &with("seats.worker", "connectors = [\"github\"]"),
            "seats.worker.connectors",
            "isolated = true",
        );
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
    fn errors_cite_the_innermost_field_its_line_and_column() {
        let base = "version = 1\nname = \"t\"\nroot = \"a\"\n[seats.a]\n";
        let at = |extra: &str, field: &str, needle: &str| {
            let error = refused(&format!("{base}{extra}"), field, needle);
            (error.line.unwrap(), error.column.unwrap())
        };
        assert_eq!(
            at(
                "budget = { usd = 1 }",
                "seats.a.budget.usd",
                "expected one of: max_usd"
            ),
            (5, 12)
        );
        assert_eq!(
            at(
                "delegates_to = [\"b\", 3]",
                "seats.a.delegates_to[1]",
                "must be a string, not integer `3`"
            ),
            (5, 22)
        );
        assert_eq!(
            at(
                "startup = { files = [{ path = \"x\", delivery_hint = \"skill\" }] }",
                "seats.a.startup.files[0].delivery_hint",
                "not supported: every startup file"
            ),
            (5, 36)
        );
        assert_eq!(
            at(
                "[seats.a.budget]\nmax_turns = 1.5",
                "seats.a.budget.max_turns",
                "must be a whole number, not float `1.5`"
            ),
            (6, 1)
        );
        at(
            "isolated = \"yes\"",
            "seats.a.isolated",
            "must be true or false",
        );
        at("secrets = \"X\"", "seats.a.secrets", "must be an array");
        at("effort = true", "seats.a.effort", "must be low, medium");
        at("effort = 300", "seats.a.effort", "0-100");
        at("check = [\"x\", 3]", "seats.a.check[1]", "must be a string");
        at("mcp = { docs = 3 }", "seats.a.mcp.docs", "must be a string");
        at(
            "startup = { files = [{ required = false }] }",
            "seats.a.startup.files[0]",
            "needs a path",
        );
        at("[[seats.a.x]]", "seats.a.x", "unknown field");
        // A refused field is left out of the fields an unknown one lists.
        let error = refused(
            &format!("{base}bogus = 1"),
            "seats.a.bogus",
            "unknown field",
        );
        assert!(!error.message.contains("collaborates_with"), "{error}");
        assert!(error.message.contains("delegates_to"), "{error}");
    }

    #[test]
    fn validate_plans_a_spec_without_reading_files() {
        let plan = validate(&with(
            "seats.worker",
            "startup = { files = [\"missing.md\"] }",
        ))
        .unwrap();
        assert_eq!(plan.skipped_files, ["missing.md"]);
        let error = validate("version = 1\nname = \"t\"\nroot = \"b\"\n[seats.a]\n").unwrap_err();
        assert_eq!((error.field.as_str(), error.line), ("root", Some(3)));
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
                "not a permission preset",
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
            (
                "permission_policy = \"builtin:standard\"",
                "belongs to a seat",
            ),
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
        let temp = tempfile::Builder::new()
            .prefix("by-rig-unit-")
            .tempdir()
            .unwrap();
        let dir = temp.path();
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
    }

    /// `schema/rig.json` is what the `Raw*` types generate. Rewrite it with
    /// `BY_UPDATE_SCHEMA=1 cargo test -p branchyard-cli --features schema
    /// rig_json`, then review the diff.
    #[cfg(feature = "schema")]
    #[test]
    fn rig_json_is_generated_and_fresh() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/rig.json");
        let generated = schema::rig_json();
        if std::env::var_os("BY_UPDATE_SCHEMA").is_some() {
            std::fs::write(&path, &generated).unwrap();
        }
        let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            checked_in == generated,
            "schema/rig.json is stale; regenerate with:\n\
             BY_UPDATE_SCHEMA=1 cargo test -p branchyard-cli --features schema rig_json"
        );
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
