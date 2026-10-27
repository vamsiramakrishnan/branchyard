//! Artifacts and scratch space a branch shares, locally and on a server.

use super::*;

pub fn artifact(target: &Target, args: &ArtifactArgs) -> Outcome {
    let json = args.json;
    if let Target::Remote(remote) = target {
        return remote_artifact(remote, args);
    }
    let delegate = harness_delegate(json)?;
    let branch = match acting_branch(&delegate, &args.branch, "artifact") {
        Ok(branch) => branch,
        Err(error) => return fail(json, &error),
    };
    let act = match delegate {
        Some(delegate) => delegate,
        None => match as_user(&branch, TaskOptions::default()) {
            Ok(delegate) => delegate,
            Err(error) => return fail(json, &error),
        },
    };
    match args.action.as_str() {
        "publish" => {
            let path = absolute(given(args.arg.as_deref(), "arg")?);
            let labels = args.labels.iter().cloned().collect();
            let result =
                act.publish_artifact(&path, args.name.clone(), args.media_type.clone(), labels);
            emit(json, result, |a| {
                format!(
                    "published {} as {} ({} bytes, {}, blake3 {})\n",
                    a.name, a.id, a.size, a.media_type, a.digest
                )
            })
        }
        "list" => {
            let result = act.artifacts();
            emit(json, result, |list: &Vec<branchyard::ArtifactRef>| {
                if list.is_empty() {
                    return "no readable artifacts\n".into();
                }
                list.iter()
                    .map(|a| {
                        format!(
                            "{} {} {} ({} bytes)\n",
                            a.id, a.name, a.publisher_branch, a.size
                        )
                    })
                    .collect()
            })
        }
        "get" => {
            let id = given(args.arg.clone(), "arg")?;
            let out = absolute(given(args.out.as_deref(), "out")?);
            let result = act.read_artifact(&id, &out);
            emit(json, result, |a| {
                format!("wrote {} bytes of {} to {}\n", a.size, a.id, out.display())
            })
        }
        "share" => {
            let id = given(args.arg.clone(), "arg")?;
            let to = given(args.to.clone(), "to")?;
            let result = act.share_artifact(&id, &to).map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {id} with {to}\n"))
        }
        "export" => {
            let out = absolute(given(args.out.as_deref(), "out")?);
            let result = act.export_artifacts(&args.ids, &out);
            emit(json, result, |entries: &Vec<branchyard::BundleEntry>| {
                format!(
                    "exported {} artifact{} to {}\n",
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" },
                    out.display()
                )
            })
        }
        "import" => {
            let path = absolute(given(args.arg.as_deref(), "arg")?);
            let result = act.import_artifacts(&path);
            emit(json, result, |imported: &Vec<branchyard::ArtifactRef>| {
                let mut out = format!(
                    "imported {} artifact{} from {}\n",
                    imported.len(),
                    if imported.len() == 1 { "" } else { "s" },
                    path.display()
                );
                for a in imported {
                    out.push_str(&format!("  {} {} ({} bytes)\n", a.id, a.name, a.size));
                }
                out
            })
        }
        other => unreachable!("artifact action {other} was validated in args"),
    }
}

/// A trivial success, printed as `{"ok": true}` with `--json` rather than
/// `null`: the Python module treats a `null` result the same as no output,
/// which is how a failed subprocess with nothing on stdout looks too.
#[derive(Serialize)]
pub(crate) struct Ack {
    ok: bool,
}

/// `--branch`, required in remote mode: there is no harness to delegate
/// as (a harness reaching the server acts through the delegation surfaces,
/// not `by --remote`), so a person must always be named explicitly.
pub(crate) fn remote_branch(
    json: bool,
    args_branch: &Option<String>,
    command: &str,
) -> Result<String, Outcome> {
    match args_branch {
        Some(branch) => Ok(branch.clone()),
        None => Err(fail(
            json,
            &branchyard::Error::Denied(format!(
                "--remote {command} needs --branch: there is no harness here to delegate as"
            )),
        )),
    }
}

/// `by --remote artifact …`: the same JSON as local mode, over
/// `branchyard_client`, acting with the server's authority as the named
/// `--branch`. A path here (`publish`'s file, `get`'s `--out`) is always
/// the caller's own local file: the bytes cross the wire, unlike a
/// `--substrate-key`-style path, which names a file on the server.
pub(crate) fn remote_artifact(server: &remote::Remote, args: &ArtifactArgs) -> Outcome {
    let json = args.json;
    let branch = match remote_branch(json, &args.branch, "artifact") {
        Ok(branch) => branch,
        Err(outcome) => return outcome,
    };
    match args.action.as_str() {
        "publish" => {
            let path = absolute(given(args.arg.as_deref(), "arg")?);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return fail(
                        json,
                        &branchyard::Error::State(format!("read {}: {error}", path.display())),
                    )
                }
            };
            let result = server
                .repo
                .publish_artifact(
                    &branch,
                    &bytes,
                    args.name.as_deref(),
                    args.media_type.as_deref(),
                    &args.labels,
                    &branchyard_client::new_key(),
                )
                .map_err(remote::sdk_error);
            emit(json, result, |a| {
                format!(
                    "published {} as {} ({} bytes, {})\n",
                    a.name, a.id, a.size, a.digest
                )
            })
        }
        "list" => {
            let result = server.repo.artifacts(&branch).map_err(remote::sdk_error);
            emit(json, result, |list: &Vec<branchyard::ArtifactRef>| {
                if list.is_empty() {
                    return "no readable artifacts\n".into();
                }
                list.iter()
                    .map(|a| {
                        format!(
                            "{} {} {} ({} bytes)\n",
                            a.id, a.name, a.publisher_branch, a.size
                        )
                    })
                    .collect()
            })
        }
        "get" => {
            let id = given(args.arg.clone(), "arg")?;
            let out = absolute(given(args.out.as_deref(), "out")?);
            let result = server
                .repo
                .read_artifact(&branch, &id)
                .map_err(remote::sdk_error)
                .and_then(|(a, bytes)| {
                    if let Some(dir) = out.parent() {
                        if !dir.as_os_str().is_empty() {
                            std::fs::create_dir_all(dir).map_err(|e| {
                                branchyard::Error::State(format!("create {}: {e}", dir.display()))
                            })?;
                        }
                    }
                    std::fs::write(&out, &bytes).map_err(|e| {
                        branchyard::Error::State(format!("write {}: {e}", out.display()))
                    })?;
                    Ok(a)
                });
            emit(json, result, |a| {
                format!("wrote {} bytes of {} to {}\n", a.size, a.id, out.display())
            })
        }
        "share" => {
            let id = given(args.arg.clone(), "arg")?;
            let to = given(args.to.clone(), "to")?;
            let result = server
                .repo
                .share_artifact(&branch, &id, &to, &branchyard_client::new_key())
                .map_err(remote::sdk_error)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {id} with {to}\n"))
        }
        "export" | "import" => fail(
            json,
            &branchyard::Error::Unsupported(format!(
                "`by --remote artifact {action}` is not supported: the server has no bundle \
                 endpoint. Run `by artifact {action}` locally instead (see docs/storage.md \
                 \"Portable bundles\")",
                action = args.action
            )),
        ),
        other => unreachable!("artifact action {other} was validated in args"),
    }
}

pub fn scratch(target: &Target, args: &ScratchArgs) -> Outcome {
    let json = args.json;
    if let Target::Remote(remote) = target {
        return remote_scratch(remote, args);
    }
    let delegate = harness_delegate(json)?;
    let branch = match acting_branch(&delegate, &args.branch, "scratch") {
        Ok(branch) => branch,
        Err(error) => return fail(json, &error),
    };
    let act = match delegate {
        Some(delegate) => delegate,
        None => match as_user(&branch, TaskOptions::default()) {
            Ok(delegate) => delegate,
            Err(error) => return fail(json, &error),
        },
    };
    match args.action.as_str() {
        "create" => {
            let result = act.create_scratch(given(args.name.as_deref(), "name")?);
            emit(json, result, |a| {
                format!("created scratch area {}\n", a.name)
            })
        }
        "list" => {
            let result = act.scratch_areas();
            emit(json, result, |list: &Vec<branchyard::ScratchArea>| {
                if list.is_empty() {
                    return "no reachable scratch areas\n".into();
                }
                list.iter()
                    .map(|a| format!("{} {}\n", a.name, a.owner_branch))
                    .collect()
            })
        }
        "lock" => {
            let result = act.lock_scratch(given(args.name.as_deref(), "name")?);
            emit(json, result, |l| {
                format!("{} holds {}\n", l.holder_branch, l.name)
            })
        }
        "unlock" => {
            let result = act
                .unlock_scratch(given(args.name.as_deref(), "name")?)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| "unlocked\n".to_owned())
        }
        "share" => {
            let name = given(args.name.clone(), "name")?;
            let to = given(args.to.clone(), "to")?;
            let result = act.share_scratch(&name, &to).map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {name} with {to}\n"))
        }
        other => unreachable!("scratch action {other} was validated in args"),
    }
}

/// `by --remote scratch …`: the same JSON as local mode, acting with the
/// server's authority as the named `--branch`. The scratch directory
/// lives on the server host; a remote caller reaches it only through
/// `lock`/`unlock` (which fence honest callers going through the server,
/// same as local mode) and whatever harnesses the server runs, never by
/// reading or writing the directory itself.
pub(crate) fn remote_scratch(server: &remote::Remote, args: &ScratchArgs) -> Outcome {
    let json = args.json;
    let branch = match remote_branch(json, &args.branch, "scratch") {
        Ok(branch) => branch,
        Err(outcome) => return outcome,
    };
    match args.action.as_str() {
        "create" => {
            let name = given(args.name.as_deref(), "name")?;
            let result = server
                .repo
                .create_scratch(&branch, name, &branchyard_client::new_key())
                .map_err(remote::sdk_error);
            emit(json, result, |a| {
                format!("created scratch area {}\n", a.name)
            })
        }
        "list" => {
            let result = server
                .repo
                .scratch_areas(&branch)
                .map_err(remote::sdk_error);
            emit(json, result, |list: &Vec<branchyard::ScratchArea>| {
                if list.is_empty() {
                    return "no reachable scratch areas\n".into();
                }
                list.iter()
                    .map(|a| format!("{} {}\n", a.name, a.owner_branch))
                    .collect()
            })
        }
        "lock" => {
            let name = given(args.name.as_deref(), "name")?;
            let result = server
                .repo
                .lock_scratch(&branch, name, &branchyard_client::new_key())
                .map_err(remote::sdk_error);
            emit(json, result, |l| {
                format!("{} holds {}\n", l.holder_branch, l.name)
            })
        }
        "unlock" => {
            let name = given(args.name.as_deref(), "name")?;
            let result = server
                .repo
                .unlock_scratch(&branch, name, &branchyard_client::new_key())
                .map_err(remote::sdk_error)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| "unlocked\n".to_owned())
        }
        "share" => {
            let name = given(args.name.clone(), "name")?;
            let to = given(args.to.clone(), "to")?;
            let result = server
                .repo
                .share_scratch(&branch, &name, &to, &branchyard_client::new_key())
                .map_err(remote::sdk_error)
                .map(|()| Ack { ok: true });
            emit(json, result, |_| format!("shared {name} with {to}\n"))
        }
        other => unreachable!("scratch action {other} was validated in args"),
    }
}
