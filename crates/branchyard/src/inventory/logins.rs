//! The one interpreter of `catalog/logins.toml`: a harness's table made
//! into the detection script's lines ([`lines`]) and what those lines
//! printed made into its [`Login`] ([`login`]). Adding a harness's login is
//! a table in that file and a golden case in
//! `crates/branchyard/tests/golden/logins/`, not code here.
//!
//! The lines print names and never a value: `env ID NAME` for a key
//! variable that is set, `file ID SHOWN` for a credential file, and `fact
//! ID ...` for what a probe found. A login is decided in one order for every
//! harness, each step reading only its table's fields:
//!
//! 1. its status command said logged in (verified), naming a variable it
//!    sends instead when one is set (`overridden_by`);
//! 2. the variable the model provider its configuration selects reads is
//!    set (a `provider` probe);
//! 3. a key variable is set, unless a gate keeps it for a fact not found;
//! 4. its status command said logged out (verified), saying which provider
//!    the configuration selects and what it lacks;
//! 5. a credential file exists;
//! 6. a probe's fact is a login (`grep`/`flag` with `login`, `providers`,
//!    `env-file`, `sqlite`), in table order; then a gated key that is unused;
//! 7. a login store nothing on the machine can read (`sqlite`): unknown;
//! 8. nothing: the table's `logged_out`, else a likely logout when it has
//!    credential files (not on macOS with `keychain_on_macos`), else unknown.
//!
//! A logged-in detail then gets each `grep`/`flag` probe's note.

use branchyard_controls::catalog::{self, Gate, LoginEntry, Probe, Runtime, StatusCheck};
use branchyard_recipe::quote;

use super::{
    key_variables, unknown_detail, version_below, Evidence, Found, Login, LoginState, Ran,
};

/// The table for `id`, or an empty one: a harness with none is read by its
/// catalog `auth_env` alone.
fn entry(id: &str) -> &'static LoginEntry {
    static EMPTY: std::sync::OnceLock<LoginEntry> = std::sync::OnceLock::new();
    catalog::login(id).unwrap_or_else(|| EMPTY.get_or_init(LoginEntry::default))
}

/// The executable `id` is started by, for people: `codex`, `omp`.
pub(super) fn program(id: &str) -> String {
    catalog::harness(id)
        .and_then(|e| e.binaries.first().cloned())
        .unwrap_or_else(|| id.to_owned())
}

/// The status command's arguments, when the table names one.
pub(super) fn status_args(id: &str) -> Option<&'static [String]> {
    entry(id).status.as_ref().map(|s| s.args.as_slice())
}

/// The key variables the table adds to the catalog's `auth_env`.
pub(super) fn extra_key_variables(id: &str) -> &'static [String] {
    &entry(id).env
}

/// Variables that count as a key but are not offered as the one to set.
pub(super) fn quiet_variables(id: &str) -> &'static [String] {
    &entry(id).quiet_env
}

/// Credential files, as `(sh word, shown)`.
pub(super) fn credential_files(id: &str) -> impl Iterator<Item = (&'static str, &'static str)> {
    entry(id)
        .files
        .iter()
        .filter(|f| sh_word(&f.path))
        .map(|f| (f.path.as_str(), f.shown.as_str()))
}

// What may go into the script: the table is embedded and tested, and the
// script is still made only of what passes these.

/// A name a script may print or test: `GEMINI_API_KEY`, `apiKeyEnv`.
fn identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A fact's or a program's name: `login-stored`, `bun`.
fn word(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A relative path or a plain value: `profiles/acp/cordis.patch.yml`.
fn plain(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
}

/// A word that goes inside double quotes, where `$HOME` and `${X:-...}`
/// expand and nothing runs: no quote, backslash, backtick or `$(`.
fn sh_word(text: &str) -> bool {
    !text.is_empty() && !text.contains(['"', '\\', '`', '\n']) && !text.contains("$(")
}

/// Text that goes inside single quotes.
fn single_quoted(text: &str) -> bool {
    !text.contains(['\'', '\n'])
}

/// Why a probe cannot go into the script, if it cannot: the first of its
/// values that is not the shape its place in the script needs.
fn unsafe_probe(probe: &Probe) -> Option<String> {
    let checks: Vec<(bool, &str, &str)> = match probe {
        Probe::Grep {
            file,
            pattern,
            fact,
            ..
        } => vec![
            (sh_word(file), "file", file),
            (single_quoted(pattern), "pattern", pattern),
            (word(fact), "fact", fact),
        ],
        Probe::Flag {
            var, value, fact, ..
        } => vec![
            (identifier(var), "variable", var),
            (plain(value), "value", value),
            (word(fact), "fact", fact),
        ],
        Probe::Provider {
            file,
            key,
            default,
            table,
            env_key,
            ..
        } => vec![
            (sh_word(file), "file", file),
            (identifier(key), "key", key),
            (plain(default), "default", default),
            (identifier(table), "table", table),
            (identifier(env_key), "env_key", env_key),
        ],
        Probe::Providers {
            dir,
            files,
            section,
            inline_key,
            key_env,
            ..
        } => {
            let mut checks = vec![
                (sh_word(dir), "dir", dir.as_str()),
                (identifier(section), "section", section),
                (identifier(inline_key), "inline_key", inline_key),
                (identifier(key_env), "key_env", key_env),
            ];
            checks.extend(files.iter().map(|f| (plain(f), "file", f.as_str())));
            checks
        }
        Probe::EnvFile { files, .. } => files
            .iter()
            .map(|f| (sh_word(f), "file", f.as_str()))
            .collect(),
        Probe::Sqlite { file, query, .. } => vec![
            (sh_word(file), "file", file),
            (single_quoted(query), "query", query),
        ],
    };
    checks
        .into_iter()
        .find(|(ok, ..)| !ok)
        .map(|(_, what, value)| format!("{what} {value:?}"))
}

/// Why a runtime cannot go into the script, if it cannot.
fn unsafe_runtime(runtime: &Runtime) -> Option<String> {
    (!word(&runtime.program)).then(|| format!("program {:?}", runtime.program))
}

/// What in `id`'s table cannot go into the script: empty when all of it
/// can (a test holds every table to that). Each such probe is left out of
/// the script.
#[cfg(test)]
fn problems(id: &str) -> Vec<String> {
    let entry = entry(id);
    let mut out: Vec<String> = entry.probes.iter().filter_map(unsafe_probe).collect();
    out.extend(entry.runtime.as_ref().and_then(unsafe_runtime));
    out.extend(
        entry
            .env
            .iter()
            .chain(&entry.quiet_env)
            .chain(entry.gates.iter().map(|g| &g.var))
            .chain(entry.status.iter().flat_map(|s| &s.overridden_by))
            .filter(|v| !identifier(v))
            .map(|v| format!("variable {v:?}")),
    );
    out.extend(
        entry
            .files
            .iter()
            .filter(|f| !sh_word(&f.path))
            .map(|f| format!("file {:?}", f.path)),
    );
    out
}

// The script.

/// The `sed` expression that prints the name of each `NAME_API_KEY=value`
/// line with a value (`export ` allowed; an empty value or a comment is
/// not a key).
const ENV_FILE_KEYS: &str = r"s/^[[:space:]]*\(export[[:space:]][[:space:]]*\)\{0,1\}\([A-Za-z_][A-Za-z0-9_]*_API_KEY\)[[:space:]]*=[[:space:]]*[^[:space:]#].*$/\2/p";

/// The lines `id`'s probes and runtime add to its subshell in the detection
/// script, where `$f` is its executable. Each prints `fact ID ...` or `env
/// ID NAME` lines, never a value.
pub(super) fn lines(id: &str) -> String {
    let entry = entry(id);
    let mut s = String::new();
    for probe in entry.probes.iter().filter(|p| unsafe_probe(p).is_none()) {
        s.push_str(&probe_lines(id, probe));
    }
    if let Some(runtime) = entry
        .runtime
        .as_ref()
        .filter(|r| unsafe_runtime(r).is_none())
    {
        let p = &runtime.program;
        s.push_str(&format!(
            "  head -c 64 \"$f\" 2>/dev/null | tr -d '\\0' | grep -q {p} && echo 'fact {id} runs-on-{p}'\n\
             \x20 if b=$(command -v {p} 2>/dev/null); then echo \"fact {id} {p} $(\"$b\" --version 2>/dev/null | head -n1)\"; else echo 'fact {id} {p} missing'; fi\n"
        ));
    }
    s
}

fn probe_lines(id: &str, probe: &Probe) -> String {
    match probe {
        Probe::Grep {
            file,
            pattern,
            fact,
            ..
        } => format!(
            "  grep -Eq '{pattern}' \"{file}\" 2>/dev/null && echo 'fact {id} {fact}'\n"
        ),
        Probe::Flag {
            var, value, fact, ..
        } => format!(
            "  [ \"${{{var}:-}}\" = {} ] && echo 'fact {id} {fact}'\n",
            quote(value)
        ),
        // The top-level key only (a `[profiles.x]` one is not read), and
        // that provider's own table's key.
        Probe::Provider {
            file,
            key,
            default,
            table,
            env_key,
            ..
        } => format!(
            "  cx=\"{file}\"\n\
             \x20 if [ -f \"$cx\" ]; then\n\
             \x20   mp=$(sed -n -e '/^\\[/q' -e 's/^{key}[[:space:]]*=[[:space:]]*\"\\([A-Za-z0-9_.-]*\\)\".*$/\\1/p' \"$cx\" 2>/dev/null | head -n1)\n\
             \x20   if [ -n \"$mp\" ] && [ \"$mp\" != {default} ]; then\n\
             \x20     echo \"fact {id} model-provider $mp\"\n\
             \x20     ek=$(sed -n \"/^\\[{table}\\.$mp\\]/,/^\\[/p\" \"$cx\" 2>/dev/null | sed -n 's/^{env_key}[[:space:]]*=[[:space:]]*\"\\([A-Za-z_][A-Za-z0-9_]*\\)\".*$/\\1/p' | head -n1)\n\
             \x20     if [ -n \"$ek\" ]; then\n\
             \x20       echo \"fact {id} provider-env $ek\"\n\
             \x20       eval \"[ -n \\\"\\${{$ek:-}}\\\" ]\" && echo \"env {id} $ek\"\n\
             \x20     fi\n\
             \x20   fi\n\
             \x20 fi\n"
        ),
        // A commented line does not count; each named variable is an `env`
        // line when set and a `key-unset` fact when not.
        Probe::Providers {
            dir,
            files,
            section,
            inline_key,
            key_env,
            ..
        } => format!(
            "  ph=\"{dir}\"\n\
             \x20 for pf in {}; do\n\
             \x20   [ -f \"$ph/$pf\" ] || continue\n\
             \x20   grep -Eq '^[[:space:]-]*{section}:' \"$ph/$pf\" 2>/dev/null && echo \"fact {id} providers $pf\"\n\
             \x20   grep -Eq '^[[:space:]-]*{inline_key}:[[:space:]]*[^[:space:]#]' \"$ph/$pf\" 2>/dev/null && echo \"fact {id} inline-key $pf\"\n\
             \x20   for pv in $(sed -n 's/^[[:space:]-]*{key_env}:[^A-Za-z_]*\\([A-Za-z_][A-Za-z0-9_]*\\).*$/\\1/p' \"$ph/$pf\" 2>/dev/null); do\n\
             \x20     if eval \"[ -n \\\"\\${{$pv:-}}\\\" ]\"; then echo \"env {id} $pv\"; else echo \"fact {id} key-unset $pv\"; fi\n\
             \x20   done\n\
             \x20 done\n",
            files.join(" ")
        ),
        Probe::EnvFile { files, .. } => {
            let words: Vec<String> = files.iter().map(|f| format!("\"{f}\"")).collect();
            format!(
                "  for ef in {}; do\n\
                 \x20   [ -f \"$ef\" ] || continue\n\
                 \x20   case \"$ef\" in \"$HOME\"/*) shown=\"~/${{ef#\"$HOME\"/}}\" ;; *) shown=\"$ef\" ;; esac\n\
                 \x20   for pv in $(sed -n '{ENV_FILE_KEYS}' \"$ef\" 2>/dev/null); do echo \"fact {id} env-file $pv $shown\"; done\n\
                 \x20 done\n",
                words.join(" ")
            )
        }
        // The providers' names are read, never a credential.
        Probe::Sqlite { file, query, .. } => format!(
            "  db=\"{file}\"\n\
             \x20 if [ -f \"$db\" ]; then\n\
             \x20   q='{query}'\n\
             \x20   if command -v sqlite3 >/dev/null 2>&1; then\n\
             \x20     for pv in $(sqlite3 -readonly \"$db\" \"$q\" 2>/dev/null); do echo \"fact {id} login $pv\"; done\n\
             \x20   elif command -v python3 >/dev/null 2>&1; then\n\
             \x20     for pv in $(python3 -I -c 'import sqlite3,sys\n\
             c=sqlite3.connect(\"file:\"+sys.argv[1]+\"?mode=ro\",uri=True)\n\
             print(\"\\n\".join(r[0] for r in c.execute(sys.argv[2])))' \"$db\" \"$q\" 2>/dev/null); do echo \"fact {id} login $pv\"; done\n\
             \x20   else\n\
             \x20     echo 'fact {id} login-store unread'\n\
             \x20   fi\n\
             \x20 fi\n"
        ),
    }
}

// The login.

/// `template` with each `{name}` replaced.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    values
        .iter()
        .fold(template.to_owned(), |text, (name, value)| {
            text.replace(&format!("{{{name}}}"), value)
        })
}

/// What `found` says of `id`'s login.
pub(super) fn login(id: &str, found: &Found, os: &str) -> Login {
    let entry = entry(id);
    let mut login = decide(id, entry, found, os);
    if login.state == LoginState::LoggedIn {
        for probe in &entry.probes {
            let (fact, found_note, missing_note) = match probe {
                Probe::Grep {
                    fact,
                    note_found,
                    note_missing,
                    ..
                }
                | Probe::Flag {
                    fact,
                    note_found,
                    note_missing,
                    ..
                } => (fact, note_found, note_missing),
                _ => continue,
            };
            let note = match found.has_fact(fact) {
                true => found_note,
                false => missing_note,
            };
            if let Some(note) = note {
                login.detail.push_str("; ");
                login.detail.push_str(note);
            }
        }
    }
    login
}

impl Found {
    fn has_fact(&self, fact: &str) -> bool {
        self.facts.iter().any(|f| f == fact)
    }

    /// The rest of each fact that starts with `name `.
    fn facts_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.facts
            .iter()
            .filter_map(move |f| f.strip_prefix(name)?.strip_prefix(' '))
    }

    fn has_env(&self, var: &str) -> bool {
        self.env.iter().any(|v| v == var)
    }
}

fn verified(state: LoginState, detail: String) -> Login {
    Login {
        state,
        evidence: Evidence::Verified,
        detail,
        ..Login::default()
    }
}

fn likely(state: LoginState, detail: String) -> Login {
    Login {
        state,
        evidence: Evidence::Likely,
        detail,
        ..Login::default()
    }
}

/// A likely login that rests on the key variable `var`.
fn keyed(var: &str, detail: String) -> Login {
    Login {
        key: Some(var.to_owned()),
        ..likely(LoginState::LoggedIn, detail)
    }
}

/// The model provider a `provider` probe found selected: the probe's
/// shown file and what its status covers, the provider's name, and the
/// variable its table names, when it names one.
struct Selected<'a> {
    shown: &'a str,
    status_covers: &'a str,
    name: &'a str,
    var: Option<&'a str>,
}

fn selected<'a>(entry: &'a LoginEntry, found: &'a Found) -> Option<Selected<'a>> {
    let (shown, status_covers) = entry.probes.iter().find_map(|p| match p {
        Probe::Provider {
            shown,
            status_covers,
            ..
        } => Some((shown.as_str(), status_covers.as_str())),
        _ => None,
    })?;
    Some(Selected {
        shown,
        status_covers,
        name: found.facts_named("model-provider").next()?,
        var: found.facts_named("provider-env").next(),
    })
}

/// What the status command said, and the detail that says so: only the
/// line that decided it, since a status command may print warnings first.
fn status_said(program: &str, check: &StatusCheck, ran: &Ran) -> Option<(LoginState, String)> {
    if ran.status.as_deref() == Some("timeout") {
        return None;
    }
    let shown = |summary: &str| format!("{program} {}: {summary}", check.args.join(" "));
    if let Some(field) = &check.json {
        let said = |want: bool| ran.lines.iter().any(|l| json_bool(l, field) == Some(want));
        let (state, summary) = match (said(false), said(true)) {
            (true, _) => (LoginState::LoggedOut, "not logged in"),
            (false, true) => (LoginState::LoggedIn, "logged in"),
            (false, false) => return None,
        };
        let method = check
            .method
            .as_deref()
            .and_then(|m| ran.lines.iter().find_map(|l| json_string(l, m)));
        return Some((
            state,
            shown(&match method {
                Some(method) => format!("{summary} ({method})"),
                None => summary.to_owned(),
            }),
        ));
    }
    let decided = |phrases: &[String]| {
        ran.lines.iter().find(|l| {
            let line = l.to_lowercase();
            phrases.iter().any(|p| line.contains(&p.to_lowercase()))
        })
    };
    if let Some(line) = decided(&check.logged_out) {
        return Some((LoginState::LoggedOut, shown(line.trim())));
    }
    decided(&check.logged_in).map(|line| (LoginState::LoggedIn, shown(line.trim())))
}

/// The boolean JSON field `field` on one line of pretty-printed or compact
/// JSON (`"loggedIn": true,`), its name read ignoring case.
fn json_bool(line: &str, field: &str) -> Option<bool> {
    let line = line.to_lowercase();
    let (_, rest) = line.split_once(&format!("\"{}\"", field.to_lowercase()))?;
    let value = rest.trim_start_matches([' ', ':']);
    [("true", true), ("false", false)]
        .into_iter()
        .find_map(|(word, said)| value.starts_with(word).then_some(said))
}

/// The string JSON field `field` on one line: `"authMethod": "oauth_token"`.
fn json_string(line: &str, field: &str) -> Option<String> {
    let (_, rest) = line.split_once(&format!("\"{field}\""))?;
    let value = rest.trim_start_matches([' ', ':']).trim_start_matches('"');
    let value = value.split('"').next()?;
    (!value.is_empty()).then(|| value.to_owned())
}

/// The login a probe's facts are, at step 6.
fn probe_login(probe: &Probe, found: &Found) -> Option<Login> {
    match probe {
        Probe::Grep { fact, login, .. } | Probe::Flag { fact, login, .. } => {
            let login = login.as_ref().filter(|_| found.has_fact(fact))?;
            Some(likely(LoginState::LoggedIn, login.clone()))
        }
        Probe::Providers {
            shown,
            configured,
            unset,
            ..
        } => {
            let file = found.facts_named("providers").next()?;
            let file = format!("{shown}/{file}");
            // A provider whose key variable is unset is a login the harness
            // refuses, unless a key sits in the file itself.
            let inline = found.facts_named("inline-key").next().is_some();
            let mut missing: Vec<&str> = found.facts_named("key-unset").collect();
            missing.sort_unstable();
            missing.dedup();
            match (inline, missing.first()) {
                (false, Some(first)) => Some(Login {
                    key: Some((*first).to_owned()),
                    ..likely(
                        LoginState::LoggedOut,
                        fill(unset, &[("file", &file), ("vars", &missing.join(", "))]),
                    )
                }),
                _ => Some(likely(
                    LoginState::LoggedIn,
                    fill(configured, &[("file", &file)]),
                )),
            }
        }
        Probe::EnvFile { login, .. } => {
            let rest = found.facts_named("env-file").next()?;
            let (var, file) = rest.split_once(' ').unwrap_or((rest, "its .env file"));
            Some(likely(
                LoginState::LoggedIn,
                fill(login, &[("var", var), ("file", file)]),
            ))
        }
        Probe::Sqlite { login, .. } => {
            let names: Vec<&str> = found.facts_named("login").collect();
            (!names.is_empty()).then(|| {
                likely(
                    LoginState::LoggedIn,
                    fill(login, &[("names", &names.join(", "))]),
                )
            })
        }
        Probe::Provider { .. } => None,
    }
}

fn decide(id: &str, entry: &LoginEntry, found: &Found, os: &str) -> Login {
    let program = program(id);
    let vars = key_variables(id);
    let gate = |var: &str| -> Option<&Gate> { entry.gates.iter().find(|g| g.var == var) };
    // 1. The harness's own answer, when it gave one.
    let check = entry.status.as_ref();
    let said = check
        .zip(found.status.as_ref())
        .and_then(|(check, ran)| status_said(&program, check, ran));
    if let (Some((LoginState::LoggedIn, detail)), Some(check)) = (&said, check) {
        // A variable the harness sends instead of the login (Codex's
        // CODEX_API_KEY) bills another account: say so.
        let name = catalog::harness(id).map_or(id, |e| e.name.as_str());
        let detail = match found.env.iter().find(|v| check.overridden_by.contains(v)) {
            Some(var) => format!(
                "{detail}; {var} is set, and {name} uses it instead of the login (unset it, or \
                 run isolated, to use the login)"
            ),
            None => detail.clone(),
        };
        return verified(LoginState::LoggedIn, detail);
    }
    // 2. The key the model provider its configuration selects reads: the
    // harness sends its requests with it, and its status command does not
    // speak of it.
    let provider = selected(entry, found);
    if let Some(p) = &provider {
        if let Some(var) = p.var.filter(|var| found.has_env(var)) {
            return keyed(
                var,
                format!(
                    "{var} is set, and {} selects the model provider {}, which reads it (an API \
                     key; not checked)",
                    p.shown, p.name
                ),
            );
        }
    }
    // 3. A key variable works whatever the login says, unless a gate keeps
    // it for a fact that was not found.
    let used = |var: &&String| gate(var).is_none_or(|g| found.has_fact(&g.fact));
    if let Some(var) = found.env.iter().find(used) {
        let detail = match gate(var) {
            Some(g) => g.used.clone(),
            None => format!("{var} is set (an API key; not checked)"),
        };
        return keyed(var, detail);
    }
    // 4. Logged out, as the harness said; not logged in to its default
    // provider is not the same as unable to run, so name the one selected.
    if let Some((state, mut detail)) = said {
        if let (LoginState::LoggedOut, Some(p)) = (state, &provider) {
            detail.push_str(&match p.var {
                Some(var) => format!(
                    "; {} selects the model provider {}, which reads {var}, and {var} is not set",
                    p.shown, p.name
                ),
                None => format!(
                    "; {} selects the model provider {}, which names no key variable (this \
                     status command speaks only of {})",
                    p.shown, p.name, p.status_covers
                ),
            });
        }
        return verified(state, detail);
    }
    // 5. A credential file.
    if let Some(file) = found.files.first() {
        return likely(LoginState::LoggedIn, format!("{file} exists"));
    }
    // 6. What a look inside the configuration found; then a key a gate
    // keeps, which is still a key Branchyard can use (isolated runs set
    // what it needs).
    if let Some(login) = entry.probes.iter().find_map(|p| probe_login(p, found)) {
        return login;
    }
    if let Some(g) = entry
        .gates
        .iter()
        .find(|g| found.has_env(&g.var) && !found.has_fact(&g.fact))
    {
        return keyed(&g.var, g.unused.clone());
    }
    // 7. A login store nothing on the machine can read.
    let joined = vars.join(", ");
    for probe in &entry.probes {
        if let Probe::Sqlite { unread, .. } = probe {
            if found.has_fact("login-store unread") {
                return Login {
                    state: LoginState::Unknown,
                    evidence: Evidence::None,
                    detail: fill(unread, &[("vars", &joined)]),
                    ..Login::default()
                };
            }
        }
    }
    // 8. Nothing found.
    if let Some(text) = &entry.logged_out {
        return likely(
            LoginState::LoggedOut,
            fill(text, &[("vars_or", &vars.join(" or ")), ("vars", &joined)]),
        );
    }
    if !(entry.files.is_empty() || entry.keychain_on_macos && os == "Darwin") {
        let files: Vec<&str> = entry.files.iter().map(|f| f.shown.as_str()).collect();
        let mut detail = format!(
            "no {} and no {}",
            files.join(" or "),
            match vars.is_empty() {
                true => "key variable".to_owned(),
                false => vars.join(" or "),
            }
        );
        for g in &entry.gates {
            detail.push_str("; ");
            detail.push_str(&g.absent);
        }
        return likely(LoginState::LoggedOut, detail);
    }
    Login {
        state: LoginState::Unknown,
        evidence: Evidence::None,
        detail: unknown_detail(id),
        ..Login::default()
    }
}

/// What else would tell, for [`unknown_detail`].
pub(super) fn unknown_also(id: &str) -> Option<&'static str> {
    entry(id).unknown_also.as_deref()
}

/// Whether `key` with `detail` is a gated key that is not used: set, but
/// without the fact its gate needs ([`Login::key_unused`]).
pub(super) fn gated_unused(key: &str, detail: &str) -> bool {
    catalog::logins()
        .values()
        .flat_map(|e| &e.gates)
        .any(|g| g.var == key && detail.starts_with(&g.unused))
}

/// The version note of a harness whose executable is a script for a
/// runtime, when its version command failed and the runtime on `PATH` is
/// missing or too old: what it needs and the command that installs it.
pub(super) fn version_note(id: &str, found: &Found, note: Option<String>) -> Option<String> {
    let Some(runtime) = entry(id).runtime.as_ref() else {
        return note;
    };
    let p = &runtime.program;
    if !found.has_fact(&format!("runs-on-{p}")) {
        return note;
    }
    let why = match found.facts_named(p).next() {
        Some("missing") => format!("no {p} is on PATH"),
        Some(v) if version_below(v, &runtime.min) => format!("the {p} on PATH is {v}"),
        _ => return note,
    };
    Some(format!(
        "{}; {} runs on {} {} or newer and {why}: `{}` installs or upgrades it",
        note.unwrap_or_default(),
        program(id),
        runtime.name,
        runtime.min,
        runtime.install
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table goes into the script whole: nothing in it is left out
    /// for being unsafe to put there.
    #[test]
    fn every_login_table_is_safe_to_put_in_the_script() {
        for id in catalog::logins().keys() {
            assert_eq!(problems(id), Vec::<String>::new(), "{id}");
        }
        // And what would not be is refused, not quoted into something else.
        for unsafe_probe_text in [
            r#"kind = "grep"
file = "$(touch x)"
pattern = "p"
fact = "f""#,
            r#"kind = "grep"
file = "f"
pattern = "it's"
fact = "f""#,
            r#"kind = "flag"
var = "A;B"
value = "true"
fact = "f""#,
            r#"kind = "sqlite"
file = "f"
query = "select 'x'"
login = ""
unread = """#,
            r#"kind = "providers"
dir = "d"
shown = "d"
files = ["a b"]
section = "s"
inline_key = "k"
key_env = "e"
configured = ""
unset = """#,
        ] {
            let probe: Probe = toml_edit::de::from_str(unsafe_probe_text).unwrap();
            assert!(unsafe_probe(&probe).is_some(), "{unsafe_probe_text}");
        }
    }

    #[test]
    fn json_fields_are_read_line_by_line_in_either_layout() {
        assert_eq!(json_bool("  \"loggedIn\": true,", "loggedIn"), Some(true));
        assert_eq!(
            json_bool("{\"loggedIn\":false,\"x\":1}", "loggedIn"),
            Some(false)
        );
        assert_eq!(json_bool("\"other\": true", "loggedIn"), None);
        assert_eq!(
            json_string("{\"loggedIn\":false,\"authMethod\":\"none\"}", "authMethod").as_deref(),
            Some("none")
        );
        assert_eq!(json_string("\"authMethod\": \"\"", "authMethod"), None);
    }

    #[test]
    fn templates_fill_every_named_place() {
        assert_eq!(
            fill(
                "{var} in {file}; {var}",
                &[("var", "K"), ("file", "~/.env")]
            ),
            "K in ~/.env; K"
        );
        assert_eq!(fill("no {places}", &[("vars", "x")]), "no {places}");
    }
}
