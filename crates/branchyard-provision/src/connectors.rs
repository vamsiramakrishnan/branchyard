//! Connector grants: which connectors, operations and accounts a branch's
//! harness may use through the connector gateway (`docs/connectors.md`).
//!
//! A grant is a list of [`GrantEntry`]s; an operation is allowed when some
//! entry allows it. Branchyard stores the grant with the branch, signs it
//! into the turn's token (`by_grants`), and narrows it for delegated
//! children with [`narrow`]; the gateway enforces it. Nothing here does I/O.
//!
//! The flag form, `--connector`, is
//!
//! ```text
//! CONNECTOR[@ACCOUNT][:MODE[:OPERATION,OPERATION...]]
//! ```
//!
//! where `MODE` is `read` (the default), `write`, or `write+confirm`, and
//! each operation is a glob over AIR operation ids (`*` matches any run of
//! characters, dots included; the default is `*`). So `github` and
//! `github:read` read everything, `github:write:issues.*` reads and writes
//! the issue operations, `github@work:read:issues.list,pulls.list` reads two
//! operations as the `work` account, and `github:write+confirm:issues.create`
//! may also run a mutation AIR says needs confirmation.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One entry of a grant, as the token's `by_grants` carries it:
/// `{"connector", "operations", "mode", "confirm", "account"}`. It also
/// deserializes from the `--connector` string (`github:write:issues.*`),
/// which is what the MCP `spawn` tool and the SDKs take; it always
/// serializes as the object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantEntry {
    /// The bundle id the gateway serves, such as `github`.
    pub connector: String,
    /// Globs over AIR operation ids; `["*"]`, the default, is every
    /// approved operation.
    #[serde(default = "every_operation")]
    pub operations: Vec<String>,
    /// `read`: only operations AIR classifies as reads. `write`: reads and
    /// mutations.
    #[serde(default)]
    pub mode: GrantMode,
    /// Whether a mutation that AIR says needs confirmation may run. On the
    /// wire only `"allow"` is written; its absence is `deny`.
    #[serde(default, skip_serializing_if = "Confirm::is_deny")]
    pub confirm: Confirm,
    /// One of the person's connected accounts for the connector; unset is
    /// their default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

fn every_operation() -> Vec<String> {
    vec!["*".to_owned()]
}

/// What an entry lets the harness do: read, or read and write.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantMode {
    #[default]
    Read,
    Write,
}

/// Whether a mutation that needs confirmation may run under an entry.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confirm {
    #[default]
    Deny,
    Allow,
}

impl Confirm {
    fn is_deny(&self) -> bool {
        *self == Confirm::Deny
    }
}

impl GrantEntry {
    /// Every operation of `connector`, read-only, default account.
    pub fn read(connector: &str) -> GrantEntry {
        GrantEntry {
            connector: connector.to_owned(),
            operations: every_operation(),
            mode: GrantMode::Read,
            confirm: Confirm::Deny,
            account: None,
        }
    }

    /// Parse the `--connector` form; see the module documentation.
    pub fn parse(text: &str) -> Result<GrantEntry, String> {
        let mut parts = text.splitn(3, ':');
        let head = parts.next().unwrap_or_default();
        let (connector, account) = match head.split_once('@') {
            Some((connector, account)) => (connector, Some(account.to_owned())),
            None => (head, None),
        };
        let (mode, confirm) = match parts.next() {
            None | Some("read") => (GrantMode::Read, Confirm::Deny),
            Some("write") => (GrantMode::Write, Confirm::Deny),
            Some("write+confirm") => (GrantMode::Write, Confirm::Allow),
            Some(other) => {
                return Err(format!(
                    "connector {connector}: mode {other:?} is not read, write or write+confirm"
                ))
            }
        };
        let operations = match parts.next() {
            None => every_operation(),
            Some(list) => list.split(',').map(|op| op.trim().to_owned()).collect(),
        };
        let entry = GrantEntry {
            connector: connector.to_owned(),
            operations,
            mode,
            confirm,
            account,
        };
        entry.check()?;
        Ok(entry)
    }

    /// Refuse an entry the gateway could not match exactly: a connector,
    /// account or operation glob with characters outside its set, or no
    /// operations.
    pub fn check(&self) -> Result<(), String> {
        check_connector(&self.connector)?;
        if let Some(account) = &self.account {
            let ok = !account.is_empty()
                && account.len() <= 64
                && account
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
            if !ok {
                return Err(format!(
                    "connector {}: account {account:?} may hold only letters, digits, '_', '-' \
                     and '.'",
                    self.connector
                ));
            }
        }
        if self.operations.is_empty() {
            return Err(format!("connector {}: no operations", self.connector));
        }
        for op in &self.operations {
            let ok = !op.is_empty()
                && op.len() <= 200
                && op.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'_' | b'-' | b'.' | b'*' | b'?' | b'/' | b':')
                });
            if !ok {
                return Err(format!(
                    "connector {}: operation {op:?} may hold only letters, digits, '_', '-', \
                     '.', '/', ':', '*' and '?'",
                    self.connector
                ));
            }
        }
        Ok(())
    }

    /// Whether this entry allows `operation` on `connector` as `account`
    /// (`None`, the default account), for an operation that `writes` or
    /// not and that `needs_confirmation` or not. The gateway decides; this
    /// is the same rule, for tests and for explaining a grant.
    pub fn allows(
        &self,
        connector: &str,
        account: Option<&str>,
        operation: &str,
        writes: bool,
        needs_confirmation: bool,
    ) -> bool {
        self.connector == connector
            && self.account.as_deref() == account
            && self.operations.iter().any(|g| glob_match(g, operation))
            && (!writes || self.mode == GrantMode::Write)
            && (!(writes && needs_confirmation) || self.confirm == Confirm::Allow)
    }
}

impl<'de> Deserialize<'de> for GrantEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<GrantEntry, D::Error> {
        /// The object form, field for field.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            connector: String,
            #[serde(default = "every_operation")]
            operations: Vec<String>,
            #[serde(default)]
            mode: GrantMode,
            #[serde(default)]
            confirm: Confirm,
            #[serde(default)]
            account: Option<String>,
        }

        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = GrantEntry;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a connector grant: `github:write:issues.*` or {\"connector\", ...}")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<GrantEntry, E> {
                GrantEntry::parse(text).map_err(E::custom)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<GrantEntry, A::Error> {
                let fields =
                    Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(GrantEntry {
                    connector: fields.connector,
                    operations: fields.operations,
                    mode: fields.mode,
                    confirm: fields.confirm,
                    account: fields.account,
                })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl fmt::Display for GrantEntry {
    /// The `--connector` form, which [`GrantEntry::parse`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.connector)?;
        if let Some(account) = &self.account {
            write!(f, "@{account}")?;
        }
        let mode = match (self.mode, self.confirm) {
            (GrantMode::Read, _) => "read",
            (GrantMode::Write, Confirm::Deny) => "write",
            (GrantMode::Write, Confirm::Allow) => "write+confirm",
        };
        write!(f, ":{mode}")?;
        if self.operations != every_operation() {
            write!(f, ":{}", self.operations.join(","))?;
        }
        Ok(())
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for GrantEntry {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GrantEntry".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Either wire form: the `--connector` string, or the object the
        // token carries (what `Serialize` writes).
        schemars::json_schema!({
            "description": "One entry of a connector grant (docs/connectors.md#grants): the \
                            flag string CONNECTOR[@ACCOUNT][:read|write|write+confirm[:OP,...]] \
                            such as \"github:write:issues.*\", or the object \
                            {\"connector\", \"operations\", \"mode\", \"confirm\", \"account\"}; \
                            always written back as the object.",
            "anyOf": [
                {
                    "type": "string",
                    "pattern": "^[A-Za-z0-9_-]{1,64}(@[A-Za-z0-9_.-]{1,64})?(:(read|write|write\\+confirm)(:[^:]+)?)?$"
                },
                {
                    "type": "object",
                    "properties": {
                        "connector": {
                            "type": "string",
                            "description": "The bundle id the gateway serves, such as `github`."
                        },
                        "operations": {
                            "type": "array",
                            "items": { "type": "string" },
                            "default": ["*"],
                            "description": "Globs over AIR operation ids; `[\"*\"]`, the default, is every approved operation."
                        },
                        "mode": generator.subschema_for::<GrantMode>(),
                        "confirm": generator.subschema_for::<Confirm>(),
                        "account": {
                            "type": ["string", "null"],
                            "description": "One of the person's connected accounts for the connector; unset is their default."
                        }
                    },
                    "required": ["connector"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

/// A connector id: the id the gateway serves a bundle under, its
/// workspace-relative path folded as Anvil's fleet folds it
/// ([`fold_connector`]): 1 to 64 letters, digits, `_` and `-`, such as
/// `github` or `shipping_v2`.
pub fn check_connector(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    match ok {
        true => Ok(()),
        false => Err(format!(
            "{id:?} is not a connector id (1 to 64 letters, digits, '_' and '-')"
        )),
    }
}

/// A bundle's workspace-relative path as a connector id: every run of
/// characters other than letters, digits, `_` and `-` becomes one `_`, and
/// leading and trailing `_` go, as Anvil's fleet prefixes do
/// (`shipping/v2` is `shipping_v2`).
pub fn fold_connector(path: &str) -> String {
    let mut out = String::new();
    let mut folding = false;
    for c in path.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
            folding = false;
        } else if !folding {
            out.push('_');
            folding = true;
        }
    }
    let trimmed = out.trim_matches('_');
    match trimmed.is_empty() {
        true => "bundle".to_owned(),
        false => trimmed.to_owned(),
    }
}

/// Whether `glob` matches `text`: `*` matches any run of characters,
/// dots included, `?` any one; everything else matches itself (Anvil's
/// rule).
pub fn glob_match(glob: &str, text: &str) -> bool {
    let (g, t) = (glob.as_bytes(), text.as_bytes());
    let (mut gi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if gi < g.len() && g[gi] == b'*' {
            star = Some((gi, ti));
            gi += 1;
        } else if gi < g.len() && (g[gi] == t[ti] || g[gi] == b'?') {
            gi += 1;
            ti += 1;
        } else if let Some((sg, st)) = star {
            gi = sg + 1;
            ti = st + 1;
            star = Some((sg, st + 1));
        } else {
            return false;
        }
    }
    g[gi..].iter().all(|&b| b == b'*')
}

/// Whether every operation id `inner` matches, `outer` matches too. Sound
/// but not complete: when it cannot tell, it says no, so a narrowing built
/// on it is never wider than both sides.
pub fn glob_covers(outer: &str, inner: &str) -> bool {
    let wild = |c: char| c == '*' || c == '?';
    if !inner.contains(wild) {
        return glob_match(outer, inner);
    }
    if outer == inner || outer.bytes().all(|b| b == b'*') {
        return true;
    }
    // `issues.*` covers `issues.comments.*`: a prefix and one trailing star.
    match outer.strip_suffix('*') {
        Some(prefix) if !prefix.contains(wild) => {
            let literal = &inner[..inner.find(wild).unwrap_or(inner.len())];
            literal.starts_with(prefix)
        }
        _ => false,
    }
}

/// The operations both glob lists allow, as far as [`glob_covers`] can
/// tell: each glob of one side that the other side covers.
fn common_operations(a: &[String], b: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in a {
        for y in b {
            let common = match (glob_covers(y, x), glob_covers(x, y)) {
                (true, _) => Some(x),
                (false, true) => Some(y),
                (false, false) => None,
            };
            if let Some(op) = common {
                if !out.contains(op) {
                    out.push(op.clone());
                }
            }
        }
    }
    out
}

/// The intersection of two entries: what both allow, or `None`.
pub fn intersect_entry(a: &GrantEntry, b: &GrantEntry) -> Option<GrantEntry> {
    if a.connector != b.connector || a.account != b.account {
        return None;
    }
    let operations = common_operations(&a.operations, &b.operations);
    if operations.is_empty() {
        return None;
    }
    Some(GrantEntry {
        connector: a.connector.clone(),
        operations,
        mode: a.mode.min(b.mode),
        confirm: a.confirm.min(b.confirm),
        account: a.account.clone(),
    })
}

/// The intersection of two grants: every entry both allow.
pub fn intersect(a: &[GrantEntry], b: &[GrantEntry]) -> Vec<GrantEntry> {
    let mut out: Vec<GrantEntry> = Vec::new();
    for x in a {
        for y in b {
            if let Some(entry) = intersect_entry(x, y) {
                if !out.contains(&entry) {
                    out.push(entry);
                }
            }
        }
    }
    out
}

/// A delegated child's grant: `requested` narrowed to what `parent` allows.
/// `None` requested inherits the parent's grant. A requested entry that
/// the parent allows nothing of is refused by name, rather than silently
/// dropped; one the parent allows part of is narrowed to that part.
pub fn narrow(
    requested: Option<&[GrantEntry]>,
    parent: &[GrantEntry],
) -> Result<Vec<GrantEntry>, String> {
    let Some(requested) = requested else {
        return Ok(parent.to_vec());
    };
    let mut out: Vec<GrantEntry> = Vec::new();
    for entry in requested {
        let narrowed = intersect(std::slice::from_ref(entry), parent);
        if narrowed.is_empty() {
            return Err(format!(
                "connector grant {entry} is not within the parent's grant ({})",
                describe(parent)
            ));
        }
        for entry in narrowed {
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    Ok(out)
}

/// A grant in the flag form, comma-separated; `none` when empty.
pub fn describe(grant: &[GrantEntry]) -> String {
    match grant.is_empty() {
        true => "none".to_owned(),
        false => grant
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The connectors a grant names, in order, once each.
pub fn connectors(grant: &[GrantEntry]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in grant {
        if !out.contains(&entry.connector) {
            out.push(entry.connector.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(text: &str) -> GrantEntry {
        GrantEntry::parse(text).unwrap()
    }

    #[test]
    fn the_flag_form_parses_and_prints_back() {
        let plain = e("github");
        assert_eq!(plain, GrantEntry::read("github"));
        assert_eq!(plain.to_string(), "github:read");
        let full = e("github@work:write+confirm:issues.*,pulls.list");
        assert_eq!(full.account.as_deref(), Some("work"));
        assert_eq!(full.mode, GrantMode::Write);
        assert_eq!(full.confirm, Confirm::Allow);
        assert_eq!(full.operations, ["issues.*", "pulls.list"]);
        assert_eq!(GrantEntry::parse(&full.to_string()).unwrap(), full);
        assert_eq!(e("shipping_v2:write").connector, "shipping_v2");
        assert_eq!(fold_connector("shipping/v2"), "shipping_v2");
        assert_eq!(fold_connector("/a.b//c-d/"), "a_b_c-d");
        assert_eq!(fold_connector("///"), "bundle");
        for bad in [
            "",
            "github:admin",
            "github:read:",
            "github:read:a b",
            "../x",
            "github@:read",
            "gi thub",
            "/abs",
            "shipping/v2",
            "a.b",
        ] {
            assert!(GrantEntry::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn the_wire_form_is_the_contracts() {
        let entry = e("github@work:read:issues.*,pulls.list");
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "connector": "github", "operations": ["issues.*", "pulls.list"],
                "mode": "read", "account": "work"
            })
        );
        // Only "allow" is written, as the gateway reads it.
        let allow = serde_json::to_value(e("github:write+confirm")).unwrap();
        assert_eq!(allow["confirm"], "allow");
        let minimal: GrantEntry = serde_json::from_str(r#"{"connector":"github"}"#).unwrap();
        assert_eq!(minimal, GrantEntry::read("github"));
        assert!(serde_json::from_str::<GrantEntry>(r#"{"connector":"g","extra":1}"#).is_err());
    }

    #[test]
    fn globs_match_and_cover() {
        assert!(glob_match("*", "issues.list"));
        assert!(glob_match("issues.*", "issues.comments.create"));
        assert!(!glob_match("issues.*", "pulls.list"));
        assert!(glob_match("*.list", "pulls.list"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("issues.?et", "issues.get"));
        assert!(!glob_match("issues.?et", "issues.gget"));
        assert!(glob_covers("issues.*", "issues.?et"));
        assert!(!glob_covers("issues.?et", "issues.*"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_covers("*", "issues.*"));
        assert!(glob_covers("issues.*", "issues.comments.*"));
        assert!(glob_covers("issues.*", "issues.list"));
        assert!(!glob_covers("issues.list", "issues.*"));
        assert!(!glob_covers("issues.*", "*"));
        // Unknown cases say no.
        assert!(!glob_covers("*.list", "issues.*"));
    }

    #[test]
    fn entries_allow_by_mode_confirmation_and_account() {
        let read = e("github:read:issues.*");
        assert!(read.allows("github", None, "issues.list", false, false));
        assert!(!read.allows("github", None, "issues.create", true, false));
        assert!(!read.allows("github", Some("work"), "issues.list", false, false));
        assert!(!read.allows("linear", None, "issues.list", false, false));
        let write = e("github:write");
        assert!(write.allows("github", None, "issues.create", true, false));
        assert!(!write.allows("github", None, "repos.delete", true, true));
        assert!(e("github:write+confirm").allows("github", None, "repos.delete", true, true));
    }

    /// The wire takes the object or the `--connector` string, and writes
    /// the object; a bad string fails with the parser's reason.
    #[test]
    fn an_entry_deserializes_from_the_object_or_the_flag() {
        let object: GrantEntry = serde_json::from_str(
            r#"{"connector": "github", "operations": ["issues.*"], "mode": "write", "account": "work"}"#,
        )
        .unwrap();
        let flag: GrantEntry = serde_json::from_str(r#""github@work:write:issues.*""#).unwrap();
        assert_eq!(object, flag);
        assert_eq!(
            serde_json::to_string(&flag).unwrap(),
            r#"{"connector":"github","operations":["issues.*"],"mode":"write","account":"work"}"#
        );
        let list: Vec<GrantEntry> =
            serde_json::from_str(r#"["github:read", {"connector": "linear"}]"#).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].to_string(), "linear:read");
        let bad = serde_json::from_str::<GrantEntry>(r#""github:admin""#).unwrap_err();
        assert!(
            bad.to_string().contains("not read, write or write+confirm"),
            "{bad}"
        );
        let unknown =
            serde_json::from_str::<GrantEntry>(r#"{"connector": "github", "scope": "x"}"#)
                .unwrap_err();
        assert!(unknown.to_string().contains("unknown field"), "{unknown}");
    }

    #[test]
    fn a_child_is_only_ever_narrower() {
        let parent = vec![e("github:write:issues.*"), e("linear:read")];
        // Nothing asked: the parent's.
        assert_eq!(narrow(None, &parent).unwrap(), parent);
        // Narrower in mode and operations.
        let child = narrow(Some(&[e("github:read:issues.list")]), &parent).unwrap();
        assert_eq!(child, vec![e("github:read:issues.list")]);
        // Wider in mode, confirmation and operations: cut to the parent's.
        let child = narrow(Some(&[e("github:write+confirm")]), &parent).unwrap();
        assert_eq!(child, vec![e("github:write:issues.*")]);
        let child = narrow(Some(&[e("linear:write")]), &parent).unwrap();
        assert_eq!(child, vec![e("linear:read")]);
        // Nothing in common: refused by name.
        let refused = narrow(Some(&[e("slack:read")]), &parent).unwrap_err();
        assert!(refused.contains("slack:read"), "{refused}");
        let refused = narrow(Some(&[e("github:read:pulls.*")]), &parent).unwrap_err();
        assert!(refused.contains("pulls"), "{refused}");
        // Another account is not the default one.
        assert!(narrow(Some(&[e("github@work:read")]), &parent).is_err());
        // No grant: nothing can be asked for.
        assert!(narrow(Some(&[e("github:read")]), &[]).is_err());
        assert_eq!(narrow(None, &[]).unwrap(), vec![]);
        // Every child entry is allowed by some parent entry, for any
        // operation, at every step of a chain.
        let grandchild = narrow(Some(&[e("github:write:*")]), &child_of(&parent)).unwrap();
        for op in ["issues.list", "issues.create", "pulls.list", "x"] {
            for writes in [false, true] {
                let allowed = |g: &[GrantEntry]| {
                    g.iter()
                        .any(|en| en.allows("github", None, op, writes, false))
                };
                if allowed(&grandchild) {
                    assert!(allowed(&parent), "{op} {writes}");
                }
            }
        }
    }

    fn child_of(parent: &[GrantEntry]) -> Vec<GrantEntry> {
        narrow(Some(&[e("github:write:issues.c*")]), parent).unwrap()
    }

    #[test]
    fn describe_and_connectors() {
        let grant = vec![e("github:read"), e("github@work:write"), e("linear")];
        assert_eq!(connectors(&grant), ["github", "linear"]);
        assert_eq!(
            describe(&grant),
            "github:read github@work:write linear:read"
        );
        assert_eq!(describe(&[]), "none");
    }
}
