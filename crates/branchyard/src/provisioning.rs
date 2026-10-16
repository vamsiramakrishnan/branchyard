//! Preparing a harness's home and environment before each turn.
//!
//! This is the one path by which a turn's MCP servers and standing
//! instructions reach its harness, the task's own and Branchyard's
//! delegation server and skill alike: they go into a
//! [`branchyard_provision::Context`] with the turn's secrets, model,
//! reasoning effort and telemetry, the harness's provisioner plans them,
//! and the plan decides what the driver passes in the session and what is
//! written into the harness's home. Planning does no I/O; this module
//! resolves the secrets, reads what an earlier turn installed, and applies
//! the plan's files to the branch's private home on this host, before the
//! sandbox exists, so a Microsandbox mount or a Substrate home transfer
//! carries them in.
//!
//! Secret values are read here, at the start of every turn, from this
//! process's environment or files: on a server, the server's. They reach
//! only the harness's environment and files in its private home. Refusals
//! and the recorded [`Activity::Provisioned`] name secrets, never their
//! values; the activity also says how each secret was delivered and
//! whether the harness's tool commands inherit it.
//!
//! Nothing that may hold a secret goes on a command line, which every
//! process on the host can read. The Claude Code stream-json driver would
//! put its MCP servers there; they go in a 0600 file instead: in the
//! private home when there is one (the plan writes it), otherwise in a
//! directory of the state directory, outside the worktree, made for the
//! turn and removed when it ends ([`TurnFile`]).

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use branchyard_provision::apply::{self, Applied};
use branchyard_provision::{
    Context, EnvVar, Instructions, Provisioning, Secret, SecretFrom, SecretSource, Session,
};
use branchyard_provision::{ManagedWritten, MANAGED_SETTINGS};

use crate::placement;
use crate::projection::{Projection, SERVER_NAME};
use crate::state::Record;
use crate::workspace::{self, InSandbox, WorkspacePhase, WorkspaceReport};
use crate::{Activity, Error};
use branchyard_harness::profiles::Protocol;

/// What provisioning gives the harness.
pub(crate) struct Provisioned {
    pub env: Vec<EnvVar>,
    pub session: Session,
    /// The MCP configuration file for a driver that reads one, as the
    /// harness sees it.
    pub mcp_config_file: Option<String>,
    /// That file when it was made for this turn alone; removed on drop, so
    /// it is kept until the harness is gone.
    pub turn_file: Option<TurnFile>,
    /// The variables secrets were read from, which the harness would
    /// otherwise inherit from this process: taken out of its environment
    /// before `env` is set.
    pub scrub: Vec<String>,
    /// What to record, if anything was provisioned.
    pub activity: Option<Activity>,
}

/// Where per-turn files go, under the state directory.
const TURNS: &str = "turns";

/// A 0600 file in a 0700 directory made for one turn under the state
/// directory's `turns/`, removed with its directory when dropped.
pub(crate) struct TurnFile {
    dir: PathBuf,
    path: PathBuf,
}

impl TurnFile {
    fn create(state: &Path, name: &str, content: &str) -> Result<TurnFile, String> {
        let fail = |e: std::io::Error| format!("could not write its {name} for the turn: {e}");
        let turns = state.join(TURNS);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&turns)
            .map_err(fail)?;
        fs::set_permissions(&turns, fs::Permissions::from_mode(0o700)).map_err(fail)?;
        let id = crate::projection::new_token().map_err(|e| e.to_string())?;
        let dir = turns.join(&id[..32]);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(fail)?;
        let turn = TurnFile {
            path: dir.join(name),
            dir,
        };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&turn.path)
            .map_err(fail)?;
        file.write_all(content.as_bytes()).map_err(fail)?;
        Ok(turn)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TurnFile {
    fn drop(&mut self) {
        branchyard_support::cleanup_dir(&self.dir);
    }
}

/// Refuse a provisioning request before anything is created: servers the
/// drivers would refuse, a malformed telemetry endpoint, repeated secrets,
/// and secrets without a private home.
pub(crate) fn check(spec: Option<&Provisioning>, private_home: bool) -> Result<(), Error> {
    let Some(spec) = spec else {
        return Ok(());
    };
    let refuse = |why: String| Err(Error::Unsupported(why));
    for (index, server) in spec.mcp_servers.iter().enumerate() {
        server.check().or_else(refuse)?;
        if server.name == SERVER_NAME {
            return refuse(format!(
                "the MCP server name {SERVER_NAME} is Branchyard's own; choose another"
            ));
        }
        if spec.mcp_servers[..index]
            .iter()
            .any(|s| s.name == server.name)
        {
            return refuse(format!("MCP server {} is listed twice", server.name));
        }
        if let Some(missing) = server
            .secret_env
            .values()
            .find(|name| !spec.secrets.iter().any(|s| &s.name == *name))
        {
            return refuse(format!(
                "MCP server {} takes the secret {missing}, which the task does not give \
                 (--secret {missing})",
                server.name
            ));
        }
    }
    for (index, server) in spec.remote_mcp_servers.iter().enumerate() {
        server.check().or_else(refuse)?;
        let repeated = server.name == SERVER_NAME
            || spec.mcp_servers.iter().any(|s| s.name == server.name)
            || spec.remote_mcp_servers[..index]
                .iter()
                .any(|s| s.name == server.name);
        if repeated {
            return refuse(format!(
                "MCP server {} is listed twice or is Branchyard's own",
                server.name
            ));
        }
        if let Some(missing) = server
            .headers
            .values()
            .find(|name| !spec.secrets.iter().any(|s| &s.name == *name))
        {
            return refuse(format!(
                "MCP server {} takes the secret {missing}, which the task does not give \
                 (--secret {missing})",
                server.name
            ));
        }
    }
    for (index, secret) in spec.secrets.iter().enumerate() {
        branchyard_provision::check_variable_name(&secret.name)
            .or_else(|why| refuse(format!("secret name {:?} {why}", secret.name)))?;
        if spec.secrets[..index].iter().any(|s| s.name == secret.name) {
            return refuse(format!("secret {} is given twice", secret.name));
        }
    }
    if let Some(telemetry) = &spec.telemetry {
        telemetry.check().or_else(refuse)?;
    }
    for entry in &spec.connectors {
        entry.check().or_else(refuse)?;
    }
    if let Some(approvals) = &spec.approvals {
        approvals.check().or_else(refuse)?;
    }
    if !spec.connectors.is_empty() && !private_home {
        return refuse(
            "connectors are placed only into a home private to the branch; run it --isolated \
             (TaskOptions::isolated) or with a sandbox provider"
                .into(),
        );
    }
    if !spec.secrets.is_empty() && !private_home {
        return refuse(
            "secrets are provisioned only into a home private to the branch; run it \
             --isolated (TaskOptions::isolated) or with a sandbox provider"
                .into(),
        );
    }
    Ok(())
}

/// Read each secret's value from this process's environment or a file.
/// Trailing newlines are dropped, as Scion's `read_secret` does.
pub(crate) fn resolve(sources: &[SecretSource]) -> Result<Vec<Secret>, String> {
    sources
        .iter()
        .map(|source| {
            let value = match &source.from {
                None => variable(&source.name, &source.name)?,
                Some(SecretFrom::Env { var }) => variable(&source.name, var)?,
                Some(SecretFrom::File { path }) => std::fs::read_to_string(path).map_err(|e| {
                    format!(
                        "secret {}: cannot read {}: {e}",
                        source.name,
                        path.display()
                    )
                })?,
            };
            let value = value.trim_end_matches(['\r', '\n']).to_owned();
            if value.is_empty() {
                return Err(format!("secret {} is empty", source.name));
            }
            Ok(Secret::new(&source.name, value))
        })
        .collect()
}

fn variable(secret: &str, var: &str) -> Result<String, String> {
    std::env::var(var).map_err(|_| match secret == var {
        true => format!("secret {secret}: {var} is not set"),
        false => format!("secret {secret}: its variable {var} is not set"),
    })
}

/// The task's instructions and the delegation skill, as one.
fn instructions(own: Option<&str>, delegation: Option<&Instructions>) -> Option<Instructions> {
    match (own.filter(|t| !t.trim().is_empty()), delegation) {
        (None, None) => None,
        (None, Some(delegation)) => Some(delegation.clone()),
        (Some(text), None) => Some(Instructions {
            text: text.to_owned(),
            plugin_dir: None,
        }),
        // A Claude Code plugin would carry only the skill; the text
        // carries both.
        (Some(text), Some(delegation)) => Some(Instructions {
            text: format!("{text}\n\n{}", delegation.text),
            plugin_dir: None,
        }),
    }
}

/// What every harness should know about the branch it runs on: the
/// commits it finds there that it did not make, and where its scratch
/// files go. The delegation skill says the same to a branch that has it.
pub(crate) const WORKSPACE_NOTE: &str = "Branchyard runs you on your own git branch and \
     worktree. At the end of each turn it commits everything in the worktree for you, as a \
     commit named `by/<branch>: turn <n>`: you need not commit, and commits by that name are \
     Branchyard's, not someone else's. Put scratch files in $TMPDIR, a directory private to \
     this branch, not in /tmp, which other branches share.";

/// The task's own instructions, then the repository knowledge given this
/// turn, then the connectors' line, then [`WORKSPACE_NOTE`]. The note
/// alone is given only where a harness takes instructions apart from the
/// prompt (`note_alone`): an ACP harness gets them prepended to its first
/// prompt, which a note on its own should not change.
fn own_instructions(
    own: Option<&str>,
    knowledge: Option<&str>,
    connectors: Option<&crate::connectors::Prepared>,
    note_alone: bool,
) -> Option<String> {
    let mut parts: Vec<&str> = [
        own.filter(|t| !t.trim().is_empty()),
        knowledge.filter(|t| !t.trim().is_empty()),
        connectors.map(|c| c.instruction.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if note_alone || !parts.is_empty() {
        parts.push(WORKSPACE_NOTE);
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// What the turn grants its harness, for the provisioners that write it
/// into the home: the permission policy, which a harness that enforces its
/// own rules (Antigravity) is given as rules; and the turn's model gateway,
/// for a harness whose provider endpoint is a file (DeepSeek Harness).
pub(crate) struct Access<'a> {
    pub policy: &'a crate::Policy,
    pub model_gateway: Option<branchyard_provision::models::ModelGateway>,
}

/// Plan and apply the turn's provisioning. A failure is the turn's failure
/// reason.
pub(crate) fn prepare(
    record: &Record,
    profile: &branchyard_harness::profiles::Profile,
    projection: Option<&Projection>,
    connectors: Option<&crate::connectors::Prepared>,
    knowledge: Option<&crate::knowledge::Briefing>,
    access: Access<'_>,
    state: &Path,
) -> Result<Provisioned, String> {
    let Access {
        policy,
        model_gateway,
    } = access;
    let spec = record.provision.clone().unwrap_or_default();
    let (workspace, home) = placement::guest_paths(record);
    let secrets = resolve(&spec.secrets)?;
    // Branchyard's own server first, then the task's, with the variables
    // they take from secrets. A sandboxed harness gets Branchyard's over
    // HTTP, from the turn's endpoint on this host.
    let mut mcp_servers: Vec<_> = projection
        .filter(|p| p.remote.is_none())
        .map(|p| p.server.clone())
        .into_iter()
        .collect();
    let mut mcp_secrets = Vec::new();
    for spec in &spec.mcp_servers {
        let (server, used) = spec.resolve(&secrets)?;
        mcp_servers.push(server);
        mcp_secrets.extend(used);
    }
    let mut remote_mcp_servers: Vec<_> = projection
        .and_then(|p| p.remote.clone())
        .into_iter()
        .collect();
    for spec in &spec.remote_mcp_servers {
        let (server, used) = spec.resolve(&secrets)?;
        remote_mcp_servers.push(server);
        mcp_secrets.extend(used);
    }
    let private = record.home.as_deref();
    // The `by` a delegating harness runs; a sandboxed one has none.
    let by = projection.and_then(|p| {
        p.env
            .iter()
            .find(|(name, _)| name == crate::projection::ENV_BY)
            .map(|(_, value)| value.clone())
    });
    let context = Context {
        harness: profile.harness.to_owned(),
        protocol: profile.protocol,
        home,
        workspace,
        private_home: private.is_some(),
        sandbox: placement::sandboxed(record.provider.as_ref()),
        secrets,
        auth: spec.auth.clone(),
        mcp_servers,
        remote_mcp_servers,
        mcp_secrets,
        instructions: instructions(
            own_instructions(
                spec.instructions.as_deref(),
                knowledge.and_then(|k| k.text.as_deref()),
                connectors,
                // A delegating branch has it in its skill.
                projection.is_none()
                    && matches!(
                        profile.protocol,
                        Protocol::ClaudeStreamJson | Protocol::CodexAppServer
                    ),
            )
            .as_deref(),
            projection.map(|p| &p.instructions),
        ),
        model: spec.model.clone(),
        effort: spec.effort,
        telemetry: spec.telemetry.clone(),
        permissions: Some(crate::policy::permissions(policy, by)),
        installed: private.map(apply::installed).unwrap_or_default(),
        model_gateway,
    };
    let plan = branchyard_provision::plan(&context).map_err(|r| r.to_string())?;
    let applied = match (private, plan.files.is_empty()) {
        (_, true) => Applied::default(),
        (Some(home), false) => apply::apply(&plan, Path::new(home))
            .map_err(|e| format!("could not prepare its home: {e}"))?,
        (None, false) => return Err("its provisioning writes files but it has no home".into()),
    };
    let (mcp_config_file, turn_file) = match &plan.session.mcp_config {
        None => (None, None),
        Some(file) => match (&file.path, private) {
            (Some(path), _) => (Some(path.clone()), None),
            (None, None) if !context.sandbox => {
                let turn = TurnFile::create(Path::new(state), "mcp-config.json", &file.content)?;
                (Some(turn.path().display().to_string()), Some(turn))
            }
            (None, _) => return Err("its MCP configuration has nowhere to go".into()),
        },
    };
    let mut files: Vec<String> = applied.written.into_iter().chain(applied.removed).collect();
    let mut plan_env = plan.env;
    if let Some(connectors) = connectors {
        files.extend(connectors.files.iter().cloned());
        plan_env.extend(connectors.env.iter().cloned());
    }
    let env: Vec<String> = plan_env.iter().map(|e| e.name.clone()).collect();
    let granted = connectors.map(|c| c.connectors.clone()).unwrap_or_default();
    let knowledge_ids = knowledge.map(|k| k.ids.clone()).unwrap_or_default();
    let permissions = plan.permissions.clone().filter(|p| !p.is_empty());
    let activity = (plan.auth.is_some()
        || !files.is_empty()
        || !env.is_empty()
        || !plan.unused_secrets.is_empty()
        || !knowledge_ids.is_empty()
        || permissions.is_some())
    .then(|| Activity::Provisioned {
        auth: plan.auth.clone(),
        files,
        env,
        secrets: plan.secrets.clone(),
        unused_secrets: plan.unused_secrets.clone(),
        connectors: granted,
        knowledge: knowledge_ids,
        // Set once the sandbox has taken the managed settings; see `with_managed`.
        managed: None,
        permissions,
    });
    let scrub = spec
        .secrets
        .iter()
        .filter_map(|source| match &source.from {
            None => Some(source.name.clone()),
            Some(SecretFrom::Env { var }) => Some(var.clone()),
            Some(SecretFrom::File { .. }) => None,
        })
        .collect();
    Ok(Provisioned {
        env: plan_env,
        session: plan.session,
        mcp_config_file,
        turn_file,
        scrub,
        activity,
    })
}

/// How long the write of a sandbox's managed settings may take.
const MANAGED_TIMEOUT: Duration = Duration::from_secs(60);

/// The shell command that writes `json` to `path`, creating the file's
/// directory. The JSON is one quoted word and nothing else is interpolated:
/// `run_in_sandbox` drops a command's standard input, so the document cannot
/// come in on it.
pub(crate) fn managed_command(path: &str, json: &str) -> String {
    let dir = match Path::new(path).parent() {
        Some(dir) => dir.display().to_string(),
        None => String::new(),
    };
    format!(
        "mkdir -p {} && printf '%s\\n' {} > {}",
        branchyard_recipe::quote(&dir),
        branchyard_recipe::quote(json),
        branchyard_recipe::quote(path),
    )
}

/// Write Claude Code's managed settings into a sandboxed turn's sandbox,
/// before the harness starts. Only a Claude Code turn in a sandbox gets
/// them: a local harness is not Branchyard's to configure in the host's
/// `/etc`, and a turn's home cannot override a managed setting. `Ok(None)`
/// when none are due; `Err` says why the write failed, and the turn goes on
/// with the settings in its home only.
pub(crate) fn write_managed(
    harness: &str,
    record: &Record,
    placement: &placement::Placement,
    denied: &[String],
    cancel: &dyn Fn() -> Option<String>,
) -> Result<Option<ManagedWritten>, String> {
    if harness != "claude-code" {
        return Ok(None);
    }
    let Some((provider, sandbox)) = placement.sandbox() else {
        return Ok(None);
    };
    let telemetry_off = record
        .provision
        .as_ref()
        .and_then(|spec| spec.telemetry.as_ref())
        .is_some_and(|telemetry| !telemetry.enabled);
    let denied: Vec<&str> = denied.iter().map(String::as_str).collect();
    let settings = branchyard_provision::managed_settings(&denied, telemetry_off);
    let cwd = placement.cwd();
    // `run_in_sandbox` reports into a workspace report. Only its outcome is
    // used here: this is not workspace setup, and nothing is recorded as it.
    let mut report = WorkspaceReport::new(WorkspacePhase::Setup, None);
    workspace::run_in_sandbox(
        &mut report,
        &[managed_command(MANAGED_SETTINGS, &settings.json)],
        InSandbox {
            provider,
            sandbox,
            cwd: &cwd,
            env: &[],
            timeout: MANAGED_TIMEOUT,
            cancel,
        },
    );
    if report.ok {
        return Ok(Some(ManagedWritten {
            path: MANAGED_SETTINGS.to_owned(),
            deny: settings.deny,
            bypass_disabled: true,
            telemetry_off: settings.telemetry_off,
        }));
    }
    let mut why = report
        .error
        .clone()
        .or_else(|| report.exit_code.map(|code| format!("exit status {code}")))
        .unwrap_or_else(|| "it did not finish".to_owned());
    if let Some(line) = report.output.lines().rev().find(|l| !l.trim().is_empty()) {
        why = format!("{why}: {}", line.trim());
    }
    Err(format!(
        "managed settings could not be written to {MANAGED_SETTINGS}, so the harness runs with \
         the settings in its home only: {why}"
    ))
}

/// The turn's `Provisioned` activity with the managed settings the sandbox
/// took added to it, so one activity records everything provisioning did.
/// A turn that provisioned nothing else gets one for them alone.
pub(crate) fn with_managed(
    activity: Option<Activity>,
    managed: Option<ManagedWritten>,
) -> Option<Activity> {
    let Some(managed) = managed else {
        return activity;
    };
    let mut activity = activity.unwrap_or_else(|| Activity::Provisioned {
        auth: None,
        files: Vec::new(),
        env: Vec::new(),
        secrets: Vec::new(),
        unused_secrets: Vec::new(),
        connectors: Vec::new(),
        knowledge: Vec::new(),
        managed: None,
        permissions: None,
    });
    if let Activity::Provisioned { managed: slot, .. } = &mut activity {
        *slot = Some(managed);
    }
    Some(activity)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command writes exactly the document, quoted for the shell: a
    /// document holding quotes, expansions and a `;` reaches the file as it
    /// was, and no other file is touched. Run here in a scratch directory.
    #[test]
    fn the_managed_command_writes_the_document_exactly() {
        let dir = tempfile::Builder::new()
            .prefix("by-managed-")
            .tempdir()
            .unwrap();
        let target = dir.path().join("etc").join("managed-settings.json");
        let json = "{\"a\":\"it's $HOME `id` \\\\ \\\"q\\\" ;rm -rf x\"}";
        let command = managed_command(&target.display().to_string(), json);
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .status()
            .unwrap();
        assert!(status.success(), "{command}");
        let written = fs::read_to_string(&target).unwrap();
        assert_eq!(written, format!("{json}\n"));
        assert_eq!(
            fs::read_dir(dir.path().join("etc")).unwrap().count(),
            1,
            "only the managed file is in its directory"
        );
    }

    /// The turn's `Provisioned` activity takes the managed settings, keeps
    /// what it had, and a turn with nothing else gets one for them alone.
    #[test]
    fn managed_settings_join_the_provisioned_activity() {
        let managed = ManagedWritten {
            path: MANAGED_SETTINGS.to_owned(),
            deny: vec!["Bash".to_owned()],
            bypass_disabled: true,
            telemetry_off: false,
        };
        assert_eq!(with_managed(None, None), None);
        let alone = with_managed(None, Some(managed.clone()));
        assert!(matches!(
            alone,
            Some(Activity::Provisioned { ref files, managed: Some(ref m), .. })
                if files.is_empty() && *m == managed
        ));
        let before = Activity::Provisioned {
            auth: Some("api-key".into()),
            files: vec![".claude/settings.json".into()],
            env: vec!["ANTHROPIC_MODEL".into()],
            secrets: Vec::new(),
            unused_secrets: Vec::new(),
            connectors: Vec::new(),
            knowledge: Vec::new(),
            managed: None,
            permissions: None,
        };
        match with_managed(Some(before.clone()), Some(managed.clone())) {
            Some(Activity::Provisioned {
                auth,
                files,
                env,
                managed: Some(m),
                ..
            }) => {
                assert_eq!(auth.as_deref(), Some("api-key"));
                assert_eq!(files, [".claude/settings.json"]);
                assert_eq!(env, ["ANTHROPIC_MODEL"]);
                assert_eq!(m, managed);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            with_managed(Some(before.clone()), None),
            Some(Activity::Provisioned { managed: None, .. })
        ));
    }

    #[test]
    fn secrets_resolve_from_variables_and_files_without_trailing_newlines() {
        let dir = tempfile::Builder::new()
            .prefix("by-secret-")
            .tempdir()
            .unwrap();
        let file = dir.path().join("key");
        std::fs::write(&file, "from-a-file\r\n").unwrap();
        std::env::set_var("BY_TEST_SECRET_VALUE", "from-a-variable\n");
        let resolved = resolve(&[
            SecretSource::parse("A=BY_TEST_SECRET_VALUE").unwrap(),
            SecretSource::parse(&format!("B=@{}", file.display())).unwrap(),
        ])
        .unwrap();
        assert_eq!(resolved[0].value, "from-a-variable");
        assert_eq!(resolved[1].value, "from-a-file");
        let missing = resolve(&[SecretSource::parse("BY_TEST_SECRET_UNSET").unwrap()]);
        assert!(missing.unwrap_err().contains("is not set"));
        std::fs::write(&file, "\n").unwrap();
        let empty = resolve(&[SecretSource::parse(&format!("B=@{}", file.display())).unwrap()]);
        assert!(empty.unwrap_err().contains("empty"));
    }

    #[test]
    fn requests_are_checked_before_a_branch_exists() {
        let spec = |f: fn(&mut Provisioning)| {
            let mut spec = Provisioning::default();
            f(&mut spec);
            spec
        };
        let secret = spec(|s| s.secrets = vec![SecretSource::parse("ANTHROPIC_API_KEY").unwrap()]);
        assert!(check(Some(&secret), false).is_err());
        assert!(check(Some(&secret), true).is_ok());
        let ours = spec(|s| {
            s.mcp_servers =
                vec![branchyard_provision::McpServerSpec::parse("branchyard=/bin/x").unwrap()]
        });
        assert!(check(Some(&ours), false).is_err());
        let twice = spec(|s| {
            s.secrets = vec![
                SecretSource::parse("A").unwrap(),
                SecretSource::parse("A=B").unwrap(),
            ]
        });
        assert!(check(Some(&twice), true).is_err());
        // An MCP server's variable from a secret the task does not give.
        let mut docs = branchyard_provision::McpServerSpec::parse("docs=/bin/x").unwrap();
        docs.secret_env.insert("TOKEN".into(), "DOCS".into());
        let missing = Provisioning {
            mcp_servers: vec![docs],
            ..Provisioning::default()
        };
        let refused = check(Some(&missing), true).unwrap_err().to_string();
        assert!(refused.contains("DOCS"), "{refused}");
        let given = Provisioning {
            secrets: vec![SecretSource::parse("DOCS").unwrap()],
            ..missing
        };
        assert!(check(Some(&given), true).is_ok());
    }

    #[test]
    fn task_instructions_and_the_skill_combine() {
        let skill = Instructions {
            text: "skill".into(),
            plugin_dir: Some("/p".into()),
        };
        assert_eq!(instructions(None, Some(&skill)), Some(skill.clone()));
        let both = instructions(Some("mine"), Some(&skill)).unwrap();
        assert_eq!(both.text, "mine\n\nskill");
        assert!(both.plugin_dir.is_none());
        assert_eq!(instructions(Some(" "), None), None);
    }
}
