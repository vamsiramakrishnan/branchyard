//! `.devcontainer/devcontainer.json` (or `.devcontainer.json`), the Dev
//! Containers specification's file (containers.dev), read so `by init
//! project` can suggest a `[workspace]` from it. Coder's envbuilder
//! (Apache-2.0, vendored under `vendor/envbuilder`) is the reference for
//! the fields and the variable grammar; nothing of it is copied.
//!
//! What is read:
//!
//! - The lifecycle commands `initializeCommand`, `onCreateCommand`,
//!   `updateContentCommand` and `postCreateCommand`, in that order, as
//!   setup: a string runs in a shell, an array is one command (its words
//!   quoted), and an object's commands, which a container runs in
//!   parallel, run in order. `postStartCommand` becomes the run script.
//! - `containerEnv`, then `remoteEnv`, as `env`: variables setup, the run
//!   script and the harness get.
//! - A file with setup keys a prepared environment (`prepare`, with the
//!   file among the `inputs`), since that is what a container is: an
//!   environment built once per change to its definition.
//!
//! Variables are rewritten to the shell's: `${localWorkspaceFolder}` and
//! `${containerWorkspaceFolder}` are the worktree (`${BRANCHYARD_WORKTREE}`),
//! their `Basename` forms and `${devcontainerId}` the branch's name
//! (`${BRANCHYARD_BRANCH}`, which names the worktree), `${localEnv:VAR}`,
//! `${containerEnv:VAR}` and `${env:VAR}` the variable itself (`${VAR}`,
//! or `${VAR:-default}` with a default). What only a container would
//! provide (`image`, `build`, `dockerFile`, `features`, users, mounts,
//! ports, `customizations`, `hostRequirements`) is a note: setup runs on
//! this host, in the worktree, as every `[workspace]` setup does.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{quote_word, script_commands, Imported, Reader};

/// Where the specification puts the file.
pub const FILE: &str = ".devcontainer/devcontainer.json";
/// The same file at the repository root.
pub const ROOT_FILE: &str = ".devcontainer.json";

/// Setup, in the order a container runs them.
const SETUP_COMMANDS: [&str; 4] = [
    "initializeCommand",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
];

/// What a container would provide and a worktree does not, with why each
/// is left out.
const CONTAINER_ONLY: &[(&str, &str)] = &[
    ("remoteUser", "setup runs as you"),
    ("containerUser", "setup runs as you"),
    (
        "forwardPorts",
        "a branch has one port of its own, $BRANCHYARD_PORT",
    ),
    (
        "appPort",
        "a branch has one port of its own, $BRANCHYARD_PORT",
    ),
    ("portsAttributes", "no ports are forwarded"),
    ("otherPortsAttributes", "no ports are forwarded"),
    ("mounts", "the worktree is the only mount"),
    ("workspaceMount", "the worktree is the only mount"),
    ("workspaceFolder", "the worktree is the workspace folder"),
    ("runArgs", "no container runs"),
    ("init", "no container runs"),
    ("privileged", "no container runs"),
    ("capAdd", "no container runs"),
    ("securityOpt", "no container runs"),
    ("overrideCommand", "no container runs"),
    ("shutdownAction", "no container runs"),
    ("userEnvProbe", "setup gets your environment"),
    ("waitFor", "every setup command runs before the first turn"),
    (
        "customizations",
        "editor and tool settings are the harness's own",
    ),
    ("hostRequirements", "not checked"),
    ("postAttachCommand", "nothing attaches to a worktree"),
];

/// Read [`FILE`]'s text into a `[workspace]` suggestion.
pub fn import(text: &str, _: Reader) -> Result<Imported, String> {
    import_as(FILE, text)
}

/// [`import`] for [`ROOT_FILE`].
pub fn import_root(text: &str, _: Reader) -> Result<Imported, String> {
    import_as(ROOT_FILE, text)
}

fn import_as(file: &'static str, text: &str) -> Result<Imported, String> {
    let spec: Value = serde_json::from_str(&standardize(text))
        .map_err(|e| format!("not JSON (comments and trailing commas are allowed): {e}"))?;
    let object = spec.as_object().ok_or("the file is not a JSON object")?;
    let mut notes = Vec::new();
    let mut setup = Vec::new();
    for key in SETUP_COMMANDS {
        if let Some(command) = object.get(key) {
            setup.extend(lifecycle(key, command, &mut notes)?);
        }
    }
    let run = match object.get("postStartCommand") {
        Some(command) => {
            let commands = lifecycle("postStartCommand", command, &mut notes)?;
            match commands.is_empty() {
                true => None,
                false => Some(commands.join(" && ")),
            }
        }
        None => None,
    };
    let mut env = BTreeMap::new();
    for key in ["containerEnv", "remoteEnv"] {
        let Some(table) = object.get(key) else {
            continue;
        };
        let table = table
            .as_object()
            .ok_or_else(|| format!("{key} must be an object"))?;
        for (name, value) in table {
            let value = value
                .as_str()
                .ok_or_else(|| format!("{key}.{name} must be a string"))?;
            env.insert(name.clone(), substitute(value));
        }
    }
    let mut container = Vec::new();
    if let Some(image) = object.get("image").and_then(Value::as_str) {
        container.push(format!("the image {image}"));
    }
    let dockerfile = object
        .get("build")
        .and_then(|b| b.get("dockerfile"))
        .or_else(|| object.get("dockerFile"))
        .and_then(Value::as_str);
    if let Some(dockerfile) = dockerfile {
        container.push(format!("a build from {dockerfile}"));
    }
    if let Some(features) = object.get("features").and_then(Value::as_object) {
        if !features.is_empty() {
            let names: Vec<String> = features.keys().map(|k| feature_name(k)).collect();
            container.push(format!("the features {}", names.join(", ")));
        }
    }
    if !container.is_empty() {
        notes.push(format!(
            "needs a container runtime, which Branchyard does not run: {}; setup runs on this \
             host, in the worktree, so what the image provided must be installed there",
            container.join(", ")
        ));
    }
    for (key, why) in CONTAINER_ONLY {
        if object.contains_key(*key) {
            notes.push(format!("{key} is not imported: {why}"));
        }
    }
    let prepare = !setup.is_empty();
    Ok(Imported {
        file,
        copy: Vec::new(),
        setup,
        run,
        teardown: Vec::new(),
        shell_setup: None,
        env,
        prepare,
        inputs: match prepare {
            true => vec![file.to_owned()],
            false => Vec::new(),
        },
        notes,
    })
}

/// A feature's name without its registry and version:
/// `ghcr.io/devcontainers/features/node:1` is `node`.
fn feature_name(reference: &str) -> String {
    let without_version = reference
        .rsplit_once(':')
        .map_or(reference, |(name, version)| {
            match version.chars().all(|c| c.is_ascii_digit() || c == '.') && !version.is_empty() {
                true => name,
                false => reference,
            }
        });
    without_version
        .rsplit('/')
        .next()
        .unwrap_or(without_version)
        .to_owned()
}

/// One lifecycle command as setup commands.
fn lifecycle(key: &str, command: &Value, notes: &mut Vec<String>) -> Result<Vec<String>, String> {
    Ok(match command {
        Value::Null => Vec::new(),
        Value::String(text) => script_commands(&substitute(text)),
        Value::Array(words) => vec![argv(key, words)?],
        Value::Object(named) => {
            if named.len() > 1 {
                notes.push(format!(
                    "{key}: a container runs its {} commands in parallel ({}); here they run in \
                     order",
                    named.len(),
                    named.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            let mut out = Vec::new();
            for (name, value) in named {
                match value {
                    Value::String(text) => out.extend(script_commands(&substitute(text))),
                    Value::Array(words) => out.push(argv(&format!("{key}.{name}"), words)?),
                    _ => return Err(format!("{key}.{name} must be a string or an array")),
                }
            }
            out
        }
        _ => {
            return Err(format!(
                "{key} must be a string, an array of words, or an object of them"
            ))
        }
    })
}

/// An array command, which a container runs without a shell, as one
/// shell line: each word quoted, except for the variables substituted
/// into it, which the shell must still expand.
fn argv(key: &str, words: &[Value]) -> Result<String, String> {
    let mut line = Vec::new();
    for word in words {
        let word = word
            .as_str()
            .ok_or_else(|| format!("{key}: an array command's words must be strings"))?;
        line.push(quote_keeping_variables(word));
    }
    if line.is_empty() {
        return Err(format!("{key}: an array command has no words"));
    }
    Ok(line.join(" "))
}

/// `word` quoted for `sh`, its `${...}` expressions substituted and left
/// for the shell to expand.
fn quote_keeping_variables(word: &str) -> String {
    if !word.contains("${") {
        return quote_word(word);
    }
    let mut out = String::from("\"");
    let mut rest = word;
    while let Some(at) = rest.find("${") {
        push_literal(&mut out, &rest[..at]);
        let after = &rest[at + 2..];
        match after.find('}') {
            Some(close) => {
                out.push_str(&expression(&after[..close]));
                rest = &after[close + 1..];
            }
            None => {
                push_literal(&mut out, &rest[at..]);
                rest = "";
            }
        }
    }
    push_literal(&mut out, rest);
    out.push('"');
    out
}

/// `text` inside double quotes, as itself.
fn push_literal(out: &mut String, text: &str) {
    for c in text.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// `text` with the specification's `${...}` expressions rewritten to the
/// shell's; what is not one is left as written.
pub fn substitute(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        match after.find('}') {
            Some(close) => {
                out.push_str(&expression(&after[..close]));
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[at..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The shell form of one `${expression}`.
fn expression(inner: &str) -> String {
    let mut parts = inner.splitn(3, ':');
    let (head, name, default) = (parts.next().unwrap_or(""), parts.next(), parts.next());
    match (head, name) {
        ("localWorkspaceFolder" | "containerWorkspaceFolder", None) => {
            "${BRANCHYARD_WORKTREE}".to_owned()
        }
        (
            "localWorkspaceFolderBasename" | "containerWorkspaceFolderBasename" | "devcontainerId",
            None,
        ) => "${BRANCHYARD_BRANCH}".to_owned(),
        ("localEnv" | "containerEnv" | "env", Some(name)) if is_variable(name) => match default {
            Some(default) => format!("${{{name}:-{default}}}"),
            None => format!("${{{name}}}"),
        },
        _ => format!("${{{inner}}}"),
    }
}

fn is_variable(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// JSON with comments and trailing commas (what the specification allows)
/// as plain JSON: comments blanked, a comma before a closing bracket
/// dropped, strings untouched.
pub fn standardize(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b);
            match b {
                b'\\' if i + 1 < bytes.len() => {
                    out.push(bytes[i + 1]);
                    i += 2;
                    continue;
                }
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                out.push(b);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                out.push(b' ');
            }
            b',' => {
                // A trailing comma: nothing but space and comments before
                // the closing bracket.
                let mut j = i + 1;
                loop {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'/') {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                            j += 1;
                        }
                        j = (j + 2).min(bytes.len());
                        continue;
                    }
                    break;
                }
                if !matches!(bytes.get(j), Some(b'}') | Some(b']')) {
                    out.push(b',');
                }
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_devcontainer_becomes_a_prepared_workspace() {
        let imported = import(
            r#"{
  // The team's environment.
  "name": "app",
  "image": "mcr.microsoft.com/devcontainers/typescript-node:1-20",
  "features": {
    "ghcr.io/devcontainers/features/docker-in-docker:2": {},
    "ghcr.io/devcontainers/features/github-cli": "latest",
  },
  "containerEnv": { "CI": "1", "CACHE": "${containerWorkspaceFolder}/.cache" },
  "remoteEnv": { "CI": "true", "TOKEN_FILE": "${localEnv:HOME}/.token", "LEVEL": "${localEnv:LEVEL:info}" },
  "initializeCommand": "git lfs pull",
  "onCreateCommand": ["pnpm", "install", "--frozen-lockfile", "--dir", "${localWorkspaceFolder}"],
  "updateContentCommand": { "deps": "pnpm run build:deps", "db": ["./scripts/db.sh", "my db"] },
  "postCreateCommand": "pnpm run prepare" /* once */,
  "postStartCommand": "pnpm dev --port ${localEnv:PORT:3000}",
  "postAttachCommand": "echo hi",
  "forwardPorts": [3000],
  "remoteUser": "node",
  "customizations": { "vscode": { "extensions": ["dbaeumer.vscode-eslint"] } },
}"#,
            &none,
        )
        .unwrap();
        assert_eq!(imported.file, FILE);
        assert_eq!(
            imported.setup,
            [
                "git lfs pull",
                "pnpm install --frozen-lockfile --dir \"${BRANCHYARD_WORKTREE}\"",
                "pnpm run build:deps",
                "./scripts/db.sh 'my db'",
                "pnpm run prepare",
            ]
        );
        assert_eq!(
            imported.run.as_deref(),
            Some("pnpm dev --port ${PORT:-3000}")
        );
        assert!(imported.teardown.is_empty() && imported.copy.is_empty());
        assert_eq!(
            imported.env,
            BTreeMap::from([
                (
                    "CACHE".to_owned(),
                    "${BRANCHYARD_WORKTREE}/.cache".to_owned()
                ),
                ("CI".to_owned(), "true".to_owned()),
                ("LEVEL".to_owned(), "${LEVEL:-info}".to_owned()),
                ("TOKEN_FILE".to_owned(), "${HOME}/.token".to_owned()),
            ])
        );
        assert!(imported.prepare);
        assert_eq!(imported.inputs, [FILE]);
        let notes = imported.notes.join("\n");
        assert!(
            notes.contains(
                "updateContentCommand: a container runs its 2 commands in parallel (deps, db)"
            ),
            "{notes}"
        );
        assert!(
            notes.contains(
                "needs a container runtime, which Branchyard does not run: the image \
                 mcr.microsoft.com/devcontainers/typescript-node:1-20, the features \
                 docker-in-docker, github-cli;"
            ),
            "{notes}"
        );
        for key in [
            "forwardPorts",
            "remoteUser",
            "customizations",
            "postAttachCommand",
        ] {
            assert!(
                notes.contains(&format!("{key} is not imported")),
                "{key}: {notes}"
            );
        }
        assert!(!notes.contains("image is not imported"));
    }

    #[test]
    fn an_image_alone_prepares_nothing_and_the_root_file_is_named() {
        let imported = import_root(
            "{ \"build\": { \"dockerfile\": \"Dockerfile\", \"context\": \".\", }, \
             /* the base */ \"image\": \"codercom/code-server:latest\" }",
            &none,
        )
        .unwrap();
        assert_eq!(imported.file, ROOT_FILE);
        assert!(imported.setup.is_empty() && !imported.prepare && imported.inputs.is_empty());
        assert_eq!(imported.notes.len(), 1);
        assert!(
            imported.notes[0]
                .contains("the image codercom/code-server:latest, a build from Dockerfile"),
            "{}",
            imported.notes[0]
        );
        for (text, why) in [
            ("[1, 2]", "not a JSON object"),
            ("{ \"image\": ", "not JSON"),
            (
                "{ \"onCreateCommand\": 7 }",
                "must be a string, an array of words, or an object",
            ),
            (
                "{ \"onCreateCommand\": [\"a\", 1] }",
                "words must be strings",
            ),
            ("{ \"onCreateCommand\": [] }", "has no words"),
            (
                "{ \"postCreateCommand\": { \"x\": 1 } }",
                "postCreateCommand.x must be a string or an array",
            ),
            ("{ \"remoteEnv\": [] }", "remoteEnv must be an object"),
            (
                "{ \"remoteEnv\": { \"A\": 1 } }",
                "remoteEnv.A must be a string",
            ),
        ] {
            let error = import(text, &none).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    #[test]
    fn variables_become_the_shells_and_words_stay_quoted_around_them() {
        for (from, to) in [
            ("${localWorkspaceFolder}/x", "${BRANCHYARD_WORKTREE}/x"),
            ("${containerWorkspaceFolder}", "${BRANCHYARD_WORKTREE}"),
            ("${localWorkspaceFolderBasename}", "${BRANCHYARD_BRANCH}"),
            (
                "${containerWorkspaceFolderBasename}",
                "${BRANCHYARD_BRANCH}",
            ),
            ("${devcontainerId}", "${BRANCHYARD_BRANCH}"),
            ("${localEnv:HOME}", "${HOME}"),
            ("${containerEnv:PATH}:/x", "${PATH}:/x"),
            ("${env:A:b:c}", "${A:-b:c}"),
            ("${localEnv:USER:}", "${USER:-}"),
            ("${localEnv:1BAD}", "${localEnv:1BAD}"),
            ("${unknown}", "${unknown}"),
            ("$HOME and ${", "$HOME and ${"),
            ("plain", "plain"),
        ] {
            assert_eq!(substitute(from), to, "{from}");
        }
        assert_eq!(
            quote_keeping_variables("--dir=${localWorkspaceFolder}/a b\"$c`"),
            "\"--dir=${BRANCHYARD_WORKTREE}/a b\\\"\\$c\\`\""
        );
        assert_eq!(quote_keeping_variables("a b"), "'a b'");
        assert_eq!(quote_keeping_variables("plain"), "plain");
        assert_eq!(
            feature_name("ghcr.io/devcontainers/features/node:1"),
            "node"
        );
        assert_eq!(feature_name("ghcr.io/devcontainers/features/node"), "node");
        assert_eq!(feature_name("./local-feature"), "local-feature");
        assert_eq!(feature_name("registry:5000/x/y:2.1"), "y");
    }

    #[test]
    fn comments_and_trailing_commas_are_standardized_but_strings_are_not() {
        let text = "{\n  // c1\n  \"a\": \"x // not a comment\", /* c2 */\n  \"b\": [1, 2, /* c3 */],\n  \"c\": \"a \\\" b,\" ,\n}\n";
        let value: Value = serde_json::from_str(&standardize(text)).unwrap();
        assert_eq!(value["a"], "x // not a comment");
        assert_eq!(value["b"], serde_json::json!([1, 2]));
        assert_eq!(value["c"], "a \" b,");
        assert_eq!(standardize("[1,2]"), "[1,2]");
        assert_eq!(standardize("/* open"), " ");
    }
}
