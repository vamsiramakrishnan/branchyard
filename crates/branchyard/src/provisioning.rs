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

use branchyard_provision::apply::{self, Applied};
use branchyard_provision::{
    Context, EnvVar, Instructions, Provisioning, Secret, SecretFrom, SecretSource, Session,
};

use crate::placement;
use crate::projection::{Projection, SERVER_NAME};
use crate::state::Record;
use crate::{Activity, Error};
use branchyard_harness::profiles::Protocol;

/// What provisioning gives the harness.
pub(crate) struct Provisioned {
    /// Small discovery notice prepended to task input, including drivers
    /// that cannot accept standing instructions without a private home.
    pub capabilities: String,
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
    /// What to record: [`Activity::Provisioned`], on every turn, since it
    /// names the turn's skills whether or not anything else was provisioned.
    pub activity: Activity,
    /// What to record after it, as [`Activity::Warning`]s: a secret the
    /// harness's provisioner passed over, and why.
    pub warnings: Vec<String>,
    /// The exact values of the turn's secrets, for the scrubber that runs
    /// at the turn's end (`crate::memory::scrub`): this process is the one
    /// that still holds them. Never recorded.
    pub secret_values: Vec<String>,
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
/// Whether this host has what Claude Code's subprocess isolation needs
/// (`bwrap` and `socat` on `PATH`, on Linux) and the user has not chosen
/// for it themselves by setting `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`.
fn subprocess_isolation() -> bool {
    cfg!(target_os = "linux")
        && std::env::var_os(branchyard_provision::CLAUDE_ENV_SCRUB).is_none()
        && ["bwrap", "socat"]
            .iter()
            .all(|tool| crate::harness::find_on_path(tool).is_some())
}

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
    for (harness, provider) in &spec.logins {
        if let Err(why) = branchyard_provision::logins::check(harness, provider) {
            return refuse(format!("logins.{harness}: {why}"));
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
    if let Some(skills) = &spec.skills {
        branchyard_provision::built_in::check(skills).or_else(refuse)?;
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
            if source
                .name
                .eq_ignore_ascii_case(branchyard_support::MANAGER_TOKEN_ENV)
            {
                return Err("the operator manager token cannot be provisioned to a harness".into());
            }
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
    if var.eq_ignore_ascii_case(branchyard_support::MANAGER_TOKEN_ENV) {
        return Err("the operator manager token cannot be used as a harness secret source".into());
    }
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

/// The task's own instructions, then what its parent assigned it (the paths
/// it owns, the shared brief), then the repository knowledge given this
/// turn, then [`WORKSPACE_NOTE`]. The connectors and skills are one line,
/// the tool tree's, given with the prompt (`Provisioned::capabilities`); a
/// turn with connectors still gets the note, as when it had its own line.
/// The note alone is given only where a harness takes instructions apart
/// from the prompt (`note_alone`): an ACP harness gets them prepended to
/// its first prompt, which a note on its own should not change.
fn own_instructions(
    own: Option<&str>,
    assignment: Option<&str>,
    knowledge: Option<&str>,
    has_connectors: bool,
    note_alone: bool,
) -> Option<String> {
    let mut parts: Vec<&str> = [
        own.filter(|t| !t.trim().is_empty()),
        assignment.filter(|t| !t.trim().is_empty()),
        knowledge.filter(|t| !t.trim().is_empty()),
    ]
    .into_iter()
    .flatten()
    .collect();
    if note_alone || has_connectors || !parts.is_empty() {
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
    /// The tokens of the yard's control-plane backends, each with a backend
    /// it is the key of (`models::Gateway::control_plane_tokens`): a secret
    /// whose value holds one is withheld from the harness, whatever the
    /// secret is named.
    pub guarded: &'a [(String, String)],
}

/// What a turn is told beyond its prompt, for its instructions: the
/// repository knowledge given it, and what its parent assigned it (the
/// paths it owns, the shared brief; [`crate::assignment`]); and the
/// repository skills it stages beside the built-in ones
/// ([`crate::skills::for_turn`]) and the person's own
/// ([`crate::skills::user_for_turn`]).
pub(crate) struct Standing<'a> {
    pub knowledge: Option<&'a crate::knowledge::Briefing>,
    pub assignment: Option<&'a str>,
    pub skills: &'a crate::skills::ForTurn,
    /// The person's own skills, read this turn from the folder the branch
    /// was created with; `None` for a branch created without one.
    pub user_skills: Option<&'a crate::skills::UserSet>,
    /// The digest of the turn manifest written into the home
    /// ([`crate::manifest`]); `None` for a branch without a private home.
    pub manifest: Option<&'a str>,
    /// Whether this turn wrote the manifest file (it is rewritten only when
    /// it changed), so `provisioned.files` names it only then.
    pub manifest_written: bool,
    /// The skills the policy allows as of this call
    /// ([`crate::TaskOptions::allowed_skills`]), which narrow the record's
    /// own set for this turn.
    pub allowed_skills: Option<&'a [String]>,
    /// The shared tool store, when the turn's harness sees it at its own
    /// path ([`crate::toolstore::for_turn`]): built-in skills in it are
    /// linked, not copied.
    pub store: Option<&'a crate::toolstore::ToolStore>,
    /// Whether every allowed built-in skill is staged, chosen or not
    /// ([`crate::providers::ProviderKind::mounts_ready_skills`]); else the
    /// chosen ones alone, as in a home that is copied into a sandbox.
    pub mount_ready: bool,
}

/// Plan and apply the turn's provisioning. A failure is the turn's failure
/// reason.
pub(crate) fn prepare(
    record: &Record,
    profile: &branchyard_harness::profiles::Profile,
    projection: Option<&Projection>,
    connectors: Option<&crate::connectors::Prepared>,
    standing: Standing<'_>,
    access: Access<'_>,
    state: &Path,
) -> Result<Provisioned, String> {
    let Access {
        policy,
        model_gateway,
        guarded,
    } = access;
    let Standing {
        knowledge,
        assignment,
        skills,
        user_skills,
        manifest,
        manifest_written,
        allowed_skills,
        store,
        mount_ready,
    } = standing;
    let mut spec = record.provision.clone().unwrap_or_default();
    spec.narrow_allowed_skills(allowed_skills);
    let (workspace, home) = placement::guest_paths(record);
    let manifest_guest = format!(
        "{}/{}",
        home.trim_end_matches('/'),
        crate::manifest::MANIFEST_FILE
    );
    let secrets = resolve(&spec.secrets)?;
    let secret_values: Vec<String> = secrets.iter().map(|s| s.value.clone()).collect();
    // A secret that holds a control plane's token is not the harness's,
    // under whatever name it is given: withheld, and said on the branch.
    let mut withheld = Vec::new();
    let secrets: Vec<Secret> = secrets
        .into_iter()
        .filter(|secret| {
            let plane = guarded
                .iter()
                .find(|(_, token)| crate::models::holds(&secret.value, token));
            if let Some((backend, _)) = plane {
                withheld.push(format!(
                    "the secret {} was not given to the harness: it holds the token of the \
                     control-plane backend {backend}, which no harness is given",
                    secret.name
                ));
            }
            plane.is_none()
        })
        .collect();
    let capability_home = match &record.home {
        Some(home) => home.clone(),
        None => crate::capabilities::local_home(state, &record.info.name)?,
    };
    let capability_guest = if record.home.is_some() {
        Path::new(&home)
    } else {
        &capability_home
    };
    let repository = match skills {
        crate::skills::ForTurn::Staged(set) => Some(set),
        crate::skills::ForTurn::Nothing | crate::skills::ForTurn::Withheld(_) => None,
    };
    let capabilities = crate::capabilities::stage_from(
        &capability_home,
        repository,
        user_skills,
        crate::capabilities::Choice {
            chosen: spec.skills.as_deref(),
            allowed: spec.allowed_skills.as_deref(),
            mount_ready,
        },
        store,
    )?;
    let granted_connectors = connectors.map(|c| c.connectors.clone()).unwrap_or_default();
    let tools = crate::tooltree::mount(
        &capability_home,
        capability_guest,
        &capabilities.chosen,
        &capabilities.mounted,
        &granted_connectors,
        record.grant.as_ref().map(|g| &g.envelope),
        // A provider that controls the guest filesystem also exposes the tree at /tools.
        crate::providers::of(record.provider.as_ref()).tools_root(),
    )?;
    // The harness's own skills directory is context it reads whole: the
    // chosen skills go there, and none of the merely mounted ones.
    let native_links = match record.home.as_deref() {
        Some(home) => crate::tooltree::link_native(home, profile.harness, &capabilities.chosen)?,
        None => Vec::new(),
    };
    let skill_set = crate::skills::skill_set(capabilities.built_in, skills, user_skills);
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
        login: spec.logins.get(profile.harness).cloned(),
        mcp_servers,
        remote_mcp_servers,
        mcp_secrets,
        instructions: instructions(
            own_instructions(
                spec.instructions.as_deref(),
                assignment,
                knowledge.and_then(|k| k.text.as_deref()),
                connectors.is_some(),
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
        subprocess_isolation: !placement::sandboxed(record.provider.as_ref())
            && subprocess_isolation(),
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
    files.extend(capabilities.written);
    files.extend(tools.written);
    files.extend(native_links);
    let mut plan_env = plan.env;
    if let Some(connectors) = connectors {
        files.extend(connectors.files.iter().cloned());
        plan_env.extend(connectors.env.iter().cloned());
    }
    // The turn manifest was written into the home before this; the harness
    // is told where, by the path it sees.
    if manifest.is_some() {
        if manifest_written {
            files.push(crate::manifest::MANIFEST_FILE.to_owned());
        }
        plan_env.push(branchyard_provision::EnvVar::plain(
            crate::manifest::ENV_MANIFEST,
            manifest_guest,
        ));
    }
    let env: Vec<String> = plan_env.iter().map(|e| e.name.clone()).collect();
    let granted = connectors.map(|c| c.connectors.clone()).unwrap_or_default();
    let knowledge_ids = knowledge.map(|k| k.ids.clone()).unwrap_or_default();
    let permissions = plan.permissions.clone().filter(|p| !p.is_empty());
    let activity = Activity::Provisioned {
        auth: plan.auth.clone(),
        files,
        env,
        secrets: plan.secrets.clone(),
        unused_secrets: plan.unused_secrets.clone(),
        connectors: granted,
        knowledge: knowledge_ids,
        skills: skill_set,
        // Recorded by turns before 3 November 2026, which wrote Claude Code's
        // managed settings into a sandbox; nothing writes them now.
        managed: None,
        permissions,
        manifest: manifest.map(str::to_owned),
    };
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
        capabilities: tools.line,
        env: plan_env,
        session: plan.session,
        mcp_config_file,
        turn_file,
        scrub,
        activity,
        warnings: plan.warnings.into_iter().chain(withheld).collect(),
        secret_values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command writes exactly the document, quoted for the shell: a
    /// document holding quotes, expansions and a `;` reaches the file as it
    /// was, and no other file is touched. Run here in a scratch directory.
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
    fn operator_manager_credentials_cannot_be_provisioned_or_aliased() {
        for spec in [
            branchyard_support::MANAGER_TOKEN_ENV.to_owned(),
            "branchyard_manager_token=@a-file-that-must-not-be-read".into(),
            "OPENAI_API_KEY=BRANCHYARD_MANAGER_TOKEN".into(),
            "DOCS_TOKEN=branchyard_manager_token".into(),
        ] {
            let error = resolve(&[SecretSource::parse(&spec).unwrap()]).unwrap_err();
            assert!(error.contains("operator manager token"), "{spec}: {error}");
            assert!(
                !error.contains("not set"),
                "reservation must be checked before reading"
            );
            assert!(
                !error.contains("cannot read"),
                "reservation must be checked before reading"
            );
        }
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
        // A provider login the table has, and one it does not.
        let login = |harness: &str, provider: &str| Provisioning {
            logins: [(harness.to_owned(), provider.to_owned())].into(),
            ..Provisioning::default()
        };
        assert!(check(Some(&login("claude-code", "zai")), true).is_ok());
        let refused = check(Some(&login("claude-code", "gemini")), true)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("logins.claude-code: claude-code has no gemini login; it has zai"),
            "{refused}"
        );
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
