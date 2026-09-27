//! The actor template a Branchyard provider needs: one container that runs
//! `branchyard-bridge` next to the harness executables, with the host's
//! verifying key and the actor's identity projected into it.

use crate::pb;

/// Where the template projects the actor's identity.
pub const IDENTITY_DIR: &str = "/run/branchyard/identity";
/// Where the bridge keeps its attempt state: on the actor's root
/// filesystem, so a suspend keeps it.
pub const STATE_FILE: &str = "/var/lib/branchyard-bridge/attempts";
/// The port the bridge listens on.
pub const BRIDGE_PORT: u16 = 8080;

/// What a bridge template is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeTemplate {
    /// Template name, unique in the atespace.
    pub name: String,
    /// An image pinned by digest (`name@sha256:...`) that holds the bridge,
    /// git and every harness executable the provider will start.
    pub image: String,
    /// Absolute path of `branchyard-bridge` in the image.
    pub bridge: String,
    /// The host's verifying key, from `branchyard-bridge keygen`.
    pub public_key: String,
    pub sandbox_class: pb::SandboxClass,
    /// The cluster-scoped `SandboxConfig` object for `sandbox_class`.
    pub sandbox_config: String,
    /// Object-storage URI that snapshots are kept under.
    pub storage_location: String,
}

/// How the bridge runs, beyond what every template needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeOptions {
    /// Serve TLS with this PEM certificate chain and key, paths in the
    /// image, for a router that passes TLS through to the actor. The key
    /// is in every actor made from the image; give it to the bridge's user
    /// only (mode 0600) and run the harness as another user (`run_as`).
    /// The wakeup probe's plain `GET /healthz` is still answered.
    pub tls: Option<(String, String)>,
    /// Run execs, and move files and trees, as this user and group, which
    /// the image must provide. The bridge itself must run as root.
    pub run_as: Option<(u32, u32)>,
}

/// An `ActorTemplate` for `CreateActorTemplate` in `atespace`, with the
/// bridge's default options.
pub fn bridge_template(atespace: &str, spec: &BridgeTemplate) -> pb::ActorTemplate {
    bridge_template_with(atespace, spec, &BridgeOptions::default())
}

/// An `ActorTemplate` for `CreateActorTemplate` in `atespace`.
///
/// Suspends commit full snapshots, so a stopped actor resumes its processes
/// as they were. The wakeup probe polls the bridge's health check, so
/// `ResumeActor` returns only once the bridge listens. The template's
/// environment is shared by every actor made from it, which is why it holds
/// only the public half of the key: per-attempt credentials are minted by
/// the host and never stored in the actor.
///
/// The bridge is the container's entry point and may be its process 1: it
/// reaps orphans itself and stops cleanly on the runtime's SIGTERM.
pub fn bridge_template_with(
    atespace: &str,
    spec: &BridgeTemplate,
    options: &BridgeOptions,
) -> pb::ActorTemplate {
    let mut command = vec![
        spec.bridge.clone(),
        "serve".into(),
        "--listen".into(),
        format!("0.0.0.0:{BRIDGE_PORT}"),
        "--identity".into(),
        IDENTITY_DIR.into(),
        "--state".into(),
        STATE_FILE.into(),
    ];
    if let Some((cert, key)) = &options.tls {
        command.extend([
            "--tls-cert".into(),
            cert.clone(),
            "--tls-key".into(),
            key.clone(),
        ]);
    }
    if let Some((uid, gid)) = options.run_as {
        command.extend(["--run-as".into(), format!("{uid}:{gid}")]);
    }
    let identity = |field: pb::ActorMetadataField, path: &str| pb::ActorMetadataItem {
        field: field as i32,
        path: path.into(),
    };
    pb::ActorTemplate {
        metadata: Some(pb::ResourceMetadata {
            atespace: atespace.into(),
            name: spec.name.clone(),
            ..Default::default()
        }),
        containers: vec![pb::Container {
            name: "bridge".into(),
            image: spec.image.clone(),
            command,
            env: vec![pb::EnvVar {
                name: branchyard_bridge::KEY_ENV.into(),
                value: spec.public_key.clone(),
            }],
            wakeup_probe: Some(pb::ContainerWakeupProbe {
                http_get: Some(pb::HttpGetAction {
                    path: "/healthz".into(),
                    port: i32::from(BRIDGE_PORT),
                }),
                timeout_seconds: 60,
            }),
            volume_mounts: vec![pb::VolumeMount {
                name: "identity".into(),
                mount_path: IDENTITY_DIR.into(),
            }],
            ..Default::default()
        }],
        volumes: vec![pb::Volume {
            name: "identity".into(),
            system_info: Some(pb::SystemInfoVolumeSource {
                data_sources: vec![pb::SystemInfoDataSource {
                    actor_metadata: Some(pb::ActorMetadataDataSource {
                        items: vec![
                            identity(pb::ActorMetadataField::Atespace, "atespace"),
                            identity(pb::ActorMetadataField::Name, "name"),
                            identity(pb::ActorMetadataField::Uid, "uid"),
                        ],
                    }),
                    trust_bundle: None,
                }],
            }),
            ..Default::default()
        }],
        snapshot_config: Some(pb::SnapshotConfig {
            on_pause: pb::SnapshotContentScope::Full as i32,
            on_commit: pb::SnapshotContentScope::Full as i32,
            on_resume: Some(pb::OnResumeConfig {
                from_data: pb::ResumeSource::ColdBoot as i32,
            }),
            storage_location: spec.storage_location.clone(),
        }),
        sandbox_config: Some(pb::SandboxConfig {
            sandbox_class: spec.sandbox_class as i32,
            config_name: spec.sandbox_config.clone(),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_runs_the_bridge_and_projects_identity() {
        let template = bridge_template(
            "tenant",
            &BridgeTemplate {
                name: "by-harness".into(),
                image: "registry.example/by@sha256:00".into(),
                bridge: "/usr/local/bin/branchyard-bridge".into(),
                public_key: "ab".repeat(32),
                sandbox_class: pb::SandboxClass::Gvisor,
                sandbox_config: "gvisor".into(),
                storage_location: "gs://bucket/by".into(),
            },
        );
        assert!(crate::runs_bridge(&template));
        let caps = crate::capabilities(&template).unwrap();
        assert!(caps.exec);
        let container = &template.containers[0];
        assert_eq!(container.command[0], "/usr/local/bin/branchyard-bridge");
        assert_eq!(container.volume_mounts[0].mount_path, IDENTITY_DIR);
        let items = &template.volumes[0]
            .system_info
            .as_ref()
            .unwrap()
            .data_sources[0]
            .actor_metadata
            .as_ref()
            .unwrap()
            .items;
        let paths: Vec<_> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, branchyard_bridge::server::IDENTITY_FILES);
        assert!(!container.command.iter().any(|a| a.starts_with("--tls")));
    }

    #[test]
    fn options_add_tls_and_another_user_to_the_command() {
        let spec = BridgeTemplate {
            name: "by-harness".into(),
            image: "registry.example/by@sha256:00".into(),
            bridge: "/usr/local/bin/branchyard-bridge".into(),
            public_key: "ab".repeat(32),
            sandbox_class: pb::SandboxClass::Gvisor,
            sandbox_config: "gvisor".into(),
            storage_location: "gs://bucket/by".into(),
        };
        let template = bridge_template_with(
            "tenant",
            &spec,
            &BridgeOptions {
                tls: Some(("/etc/by/tls.crt".into(), "/etc/by/tls.key".into())),
                run_as: Some((1000, 1000)),
            },
        );
        let command = template.containers[0].command.join(" ");
        assert!(
            command.ends_with(
                "--tls-cert /etc/by/tls.crt --tls-key /etc/by/tls.key --run-as 1000:1000"
            ),
            "{command}"
        );
    }
}
