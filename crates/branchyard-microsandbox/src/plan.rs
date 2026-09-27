//! Translating provider-independent specs into what the Microsandbox SDK
//! is asked for. Pure, so it is tested without the SDK or a KVM host.

use std::path::PathBuf;

use branchyard_sandbox::{
    Capabilities, Consistency, ExecSpec, Locality, ProviderError, SandboxSpec, SnapshotGuarantee,
    SnapshotScope,
};

/// Longest sandbox name the runtime accepts (`MAX_SANDBOX_NAME_BYTES`).
pub const MAX_NAME: usize = 128;

/// The one snapshot guarantee this provider declares: a disk snapshot of
/// the guest's root filesystem, taken live without the workload's
/// cooperation, restorable only on the host that holds it. Mounted host
/// directories, including the workspace, are not part of it.
pub const DISK_SNAPSHOT: SnapshotGuarantee = SnapshotGuarantee {
    scope: SnapshotScope::Disk,
    consistency: Consistency::Crash,
    locality: Locality::SameHost,
};

/// What this provider claims: exec, and disk checkpoints that branch into
/// new sandboxes. Not restore in place (the SDK restores into a new
/// sandbox), not full-memory snapshots or live branching (the SDK has
/// them; they are not qualified), not ingress or sharing.
pub fn capabilities() -> Capabilities {
    Capabilities {
        exec: true,
        ingress: false,
        checkpoint: vec![DISK_SNAPSHOT],
        restore: Vec::new(),
        branch: vec![DISK_SNAPSHOT],
        share: false,
    }
}

/// A host directory bound into the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMount {
    pub guest: String,
    pub host: PathBuf,
    pub readonly: bool,
}

/// Arguments for `Sandbox::builder(name).image(..).cpus(..).memory(..)
/// .volume(guest, |m| m.bind(host))`, or for a restore, which takes no image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePlan {
    pub name: String,
    pub image: Option<String>,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub mounts: Vec<PlannedMount>,
}

/// Plan the creation of `spec`, refusing what the runtime would reject or
/// reinterpret.
pub fn create(spec: &SandboxSpec) -> Result<CreatePlan, ProviderError> {
    let image = match &spec.image {
        None => {
            return Err(ProviderError::Invalid(
                "a Microsandbox sandbox needs an OCI image".into(),
            ))
        }
        Some(image) if image.is_empty() || image.starts_with(['/', '.']) => {
            return Err(ProviderError::Invalid(format!(
                "{image:?} is not an OCI image reference"
            )))
        }
        Some(image) => image.clone(),
    };
    Ok(CreatePlan {
        image: Some(image),
        ..branch(spec)?
    })
}

/// Plan a sandbox booted from a checkpoint: `spec` without its image, since
/// the checkpoint fixes the root filesystem.
pub fn branch(spec: &SandboxSpec) -> Result<CreatePlan, ProviderError> {
    spec.validate()?;
    name(&spec.name)?;
    if spec.resources.cpus == Some(0) || spec.resources.memory_mib == Some(0) {
        return Err(ProviderError::Invalid("zero CPUs or memory".into()));
    }
    let mut mounts = Vec::new();
    for mount in &spec.mounts {
        let guest = utf8(&mount.guest, "sandbox path")?;
        if guest == "/" {
            return Err(ProviderError::Invalid(
                "a mount cannot replace the guest's root".into(),
            ));
        }
        mounts.push(PlannedMount {
            guest,
            host: mount.host.clone(),
            readonly: !mount.writable,
        });
    }
    Ok(CreatePlan {
        name: spec.name.clone(),
        image: None,
        cpus: spec.resources.cpus,
        memory_mib: spec.resources.memory_mib,
        mounts,
    })
}

/// The runtime's sandbox-name rule: at most 128 bytes, starting with an
/// ASCII letter or digit, then letters, digits, `.`, `-` or `_`.
pub fn name(name: &str) -> Result<(), ProviderError> {
    let valid = name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    match valid {
        true => Ok(()),
        false => Err(ProviderError::Invalid(format!(
            "{name:?} is not a Microsandbox sandbox name"
        ))),
    }
}

/// Arguments for `sandbox.exec_stream_with(program, |e| e.args(..)
/// .cwd(..).envs(..).stdin_pipe())`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecPlan {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
}

/// Plan an exec. The guest protocol carries UTF-8 strings only.
pub fn exec(spec: &ExecSpec) -> Result<ExecPlan, ProviderError> {
    let Some((program, args)) = spec.argv.split_first() else {
        return Err(ProviderError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argument vector",
        )));
    };
    if !spec.cwd.is_absolute() {
        return Err(ProviderError::Invalid(format!(
            "working directory {} is not an absolute sandbox path",
            spec.cwd.display()
        )));
    }
    let env = spec
        .env
        .iter()
        .map(|(name, value)| {
            let name = name.to_str().filter(|n| !n.is_empty() && !n.contains('='));
            match (name, value.to_str()) {
                (Some(name), Some(value)) => Ok((name.to_owned(), value.to_owned())),
                _ => Err(ProviderError::Invalid(format!(
                    "variable {name:?} is not a UTF-8 name and value"
                ))),
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(ExecPlan {
        program: program.clone(),
        args: args.to_vec(),
        cwd: utf8(&spec.cwd, "working directory")?,
        env,
    })
}

fn utf8(path: &std::path::Path, what: &str) -> Result<String, ProviderError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| ProviderError::Invalid(format!("{what} {} is not UTF-8", path.display())))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use branchyard_sandbox::{admit, Mount, Requirements, Resources};

    use super::*;

    fn spec() -> SandboxSpec {
        SandboxSpec::new("by-fix-parser-1")
            .image("ghcr.io/acme/claude-code:2.1")
            .resources(Resources {
                cpus: Some(2),
                memory_mib: Some(2048),
            })
            .mount(Mount::writable(
                "/repo/.branchyard/worktrees/a",
                "/workspace",
            ))
            .mount(Mount::read_only("/repo/.git", "/repo/.git"))
    }

    #[test]
    fn a_spec_maps_to_image_limits_and_bind_mounts() {
        assert_eq!(
            create(&spec()).unwrap(),
            CreatePlan {
                name: "by-fix-parser-1".into(),
                image: Some("ghcr.io/acme/claude-code:2.1".into()),
                cpus: Some(2),
                memory_mib: Some(2048),
                mounts: vec![
                    PlannedMount {
                        guest: "/workspace".into(),
                        host: "/repo/.branchyard/worktrees/a".into(),
                        readonly: false,
                    },
                    PlannedMount {
                        guest: "/repo/.git".into(),
                        host: "/repo/.git".into(),
                        readonly: true,
                    },
                ],
            }
        );
    }

    #[test]
    fn specs_the_runtime_would_reject_or_reinterpret_are_refused() {
        let cases = [
            SandboxSpec {
                image: None,
                ..spec()
            },
            SandboxSpec {
                image: Some("./rootfs".into()),
                ..spec()
            },
            SandboxSpec {
                name: "-leading-dash".into(),
                ..spec()
            },
            SandboxSpec {
                name: "has/slash".into(),
                ..spec()
            },
            SandboxSpec {
                name: "x".repeat(MAX_NAME + 1),
                ..spec()
            },
            SandboxSpec {
                resources: Resources {
                    cpus: Some(0),
                    memory_mib: None,
                },
                ..spec()
            },
            spec().mount(Mount::writable("/tmp", "/")),
        ];
        for case in cases {
            assert!(
                matches!(create(&case), Err(ProviderError::Invalid(_))),
                "{case:?}"
            );
        }
        assert!(name(&"x".repeat(MAX_NAME)).is_ok());
        let branched = branch(&SandboxSpec {
            image: None,
            ..spec()
        })
        .unwrap();
        assert_eq!(branched.image, None);
        assert_eq!(branched.mounts.len(), 2);
    }

    #[test]
    fn an_exec_maps_to_program_args_cwd_and_env() {
        let spec = ExecSpec {
            argv: vec!["claude".into(), "-p".into(), "--verbose".into()],
            cwd: "/workspace".into(),
            env: BTreeMap::from([
                ("HOME".into(), "/branchyard/home".into()),
                ("ANTHROPIC_API_KEY".into(), "k".into()),
            ]),
        };
        assert_eq!(
            exec(&spec).unwrap(),
            ExecPlan {
                program: "claude".into(),
                args: vec!["-p".into(), "--verbose".into()],
                cwd: "/workspace".into(),
                env: vec![
                    ("ANTHROPIC_API_KEY".into(), "k".into()),
                    ("HOME".into(), "/branchyard/home".into()),
                ],
            }
        );
    }

    #[test]
    fn execs_the_guest_protocol_cannot_carry_are_refused() {
        let base = ExecSpec {
            argv: vec!["sh".into()],
            cwd: "/workspace".into(),
            env: BTreeMap::new(),
        };
        assert!(matches!(
            exec(&ExecSpec {
                argv: vec![],
                ..base.clone()
            }),
            Err(ProviderError::Io(_))
        ));
        assert!(exec(&ExecSpec {
            cwd: "workspace".into(),
            ..base.clone()
        })
        .is_err());
        let bad = OsString::from_vec(vec![0xff]);
        let env = BTreeMap::from([("X".into(), bad)]);
        assert!(exec(&ExecSpec {
            env,
            ..base.clone()
        })
        .is_err());
        let env = BTreeMap::from([("A=B".into(), "1".into())]);
        assert!(exec(&ExecSpec { env, ..base }).is_err());
    }

    #[test]
    fn capabilities_are_exec_and_disk_checkpoints_only() {
        let offered = capabilities();
        let full = SnapshotGuarantee {
            scope: SnapshotScope::Full,
            ..DISK_SNAPSHOT
        };
        let portable = SnapshotGuarantee {
            locality: Locality::Portable,
            ..DISK_SNAPSHOT
        };
        let admitted = Requirements {
            exec: true,
            checkpoint: Some(DISK_SNAPSHOT),
            branch: Some(DISK_SNAPSHOT),
            ..Requirements::default()
        };
        assert_eq!(admit(&admitted, &offered), Ok(()));
        for refused in [
            Requirements {
                branch: Some(full),
                ..Requirements::default()
            },
            Requirements {
                checkpoint: Some(portable),
                ..Requirements::default()
            },
            Requirements {
                restore: Some(DISK_SNAPSHOT),
                ..Requirements::default()
            },
            Requirements {
                ingress: true,
                ..Requirements::default()
            },
            Requirements {
                share: true,
                ..Requirements::default()
            },
        ] {
            assert!(admit(&refused, &offered).is_err(), "{refused:?}");
        }
    }
}
