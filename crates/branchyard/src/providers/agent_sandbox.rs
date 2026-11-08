//! Kubernetes SIGs Agent Sandboxes reached through copied/exec placement.

use std::sync::Arc;

use branchyard_sandbox::SandboxProvider;
use serde_json::json;

use super::{native, HarnessLocation, ProviderKind};
use crate::placement::{sandbox_name, Placement, SandboxPlan, HOME, WORKSPACE};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{AgentSandboxOptions, AgentSandboxRuntime, Error, Yard};

/// The tool store to mount into an allocation: the yard's store root, from
/// its bucket, when it has both.
fn store_volume(yard: &Yard) -> Option<branchyard_agent_sandbox::StoreVolume> {
    let store = yard.tool_store()?;
    Some(branchyard_agent_sandbox::StoreVolume {
        bucket: store.bucket.clone()?,
        path: store.root.display().to_string(),
    })
}

/// What an allocation under `deny` may reach besides DNS and Cloud Storage,
/// from the worker's configuration (the branch carries none of it): the
/// `[agent_sandbox]` section's `egress_hosts` (the model endpoint) and
/// `gateway_service`, the package mirrors (`[network] mirrors`) and the
/// gateway's host when it is named by DNS. A FQDN policy matches names on
/// 443, so a mirror on another port, or one named by address, is not listed.
pub(super) fn yard_egress(yard: &Yard) -> branchyard_agent_sandbox::Egress {
    use branchyard_provision::network::Host;
    let section = yard
        .configured()
        .and_then(|config| config.agent_sandbox.clone());
    let gateway = yard
        .connectors()
        .and_then(|gateway| gateway.sandbox_url.clone())
        .and_then(|url| crate::egress::gateway_rule(&url));
    let named = yard
        .mirrors()
        .into_iter()
        .chain(gateway)
        .filter(|rule| rule.port.is_none_or(|port| port == 443) && !rule.names_address())
        .map(|rule| match rule.host {
            Host::Exact(name) => name,
            Host::Subdomains(suffix) => format!("*.{suffix}"),
        });
    let mut hosts: Vec<String> = section
        .as_ref()
        .map(|section| section.egress_hosts.clone())
        .unwrap_or_default();
    hosts.extend(named);
    hosts.sort();
    hosts.dedup();
    branchyard_agent_sandbox::Egress {
        gateway: section
            .and_then(|section| section.gateway_service)
            .map(|gateway| branchyard_agent_sandbox::GatewayService {
                namespace: gateway.namespace,
                name: gateway.name,
                port: gateway.port,
            }),
        hosts,
    }
}

/// How many sandboxes the worker keeps booted ahead of time
/// (`[agent_sandbox] warm_pool`); `None` when it keeps none.
fn warm_pool(yard: &Yard) -> Option<u32> {
    yard.configured()
        .and_then(|config| config.agent_sandbox.as_ref().and_then(|s| s.warm_pool))
        .filter(|size| *size > 0)
}

/// The credential proxy to run beside the harness: when the yard's gateway
/// has a server for sandboxes to fetch tokens from
/// ([`crate::connectors::CredentialProxy::server`]).
fn credential_sidecar(yard: &Yard) -> Option<branchyard_agent_sandbox::CredentialSidecar> {
    let gateway = yard.connectors()?;
    gateway.credential_proxy.server.as_ref()?;
    Some(branchyard_agent_sandbox::CredentialSidecar {
        image: gateway.credential_proxy.sidecar_image.clone(),
    })
}

impl ProviderKind for AgentSandboxOptions {
    fn harness_location(&self) -> HarnessLocation<'_> {
        HarnessLocation::Image
    }
    fn validate_options(&self) -> Result<(), Error> {
        self.validate()
    }
    fn name(&self) -> &'static str {
        "agent_sandbox"
    }
    fn compute(&self) -> super::ProviderCompute {
        super::ProviderCompute {
            network: Some(match self.network {
                crate::AgentSandboxNetwork::Deny => "deny",
                crate::AgentSandboxNetwork::Unrestricted => "unrestricted",
            }),
            namespace: Some(self.namespace.clone()),
            ..super::ProviderCompute::plain(
                super::RunsOn::Kubernetes,
                match self.runtime {
                    AgentSandboxRuntime::Gvisor => super::Isolation::Gvisor,
                    AgentSandboxRuntime::Microvm => super::Isolation::Kata,
                },
            )
        }
    }
    fn image(&self) -> Option<String> {
        Some(self.image.clone()).filter(|image| !image.trim().is_empty())
    }
    fn key(&self) -> String {
        native::key(self.name(), json!(self))
    }
    fn banner(&self, _isolated: bool, _server: bool) -> (&'static str, String) {
        let runtime = match self.runtime {
            AgentSandboxRuntime::Gvisor => "gVisor",
            AgentSandboxRuntime::Microvm => "managed Kata microVM",
        };
        ("sandbox",format!("harnesses run in GKE Agent Sandboxes with {runtime}; workspace and home are copied in and back; each allocation is destroyed after capture"))
    }
    fn lifecycle(&self) -> Option<Lifecycle> {
        native::lifecycle()
    }
    fn copies_worktree(&self) -> bool {
        true
    }
    /// The pod runs the proxy as a container beside the harness
    /// (`branchyard_agent_sandbox::CredentialSidecar`).
    fn credential_site(&self) -> crate::connectors::CredentialSite {
        crate::connectors::CredentialSite::Sidecar
    }
    fn check(&self, _yard: &Yard) -> Result<(), Error> {
        self.validate()
    }
    /// A bucket-backed store is mounted at the store's own path
    /// ([`store_volume`]); without a bucket the home is all it gets.
    fn sees_store(&self, yard: &Yard) -> bool {
        store_volume(yard).is_some()
    }
    fn guest_paths(&self, _record: &Record) -> (String, String) {
        (WORKSPACE.into(), HOME.into())
    }
    fn tools_root(&self) -> Option<&'static str> {
        Some(crate::tooltree::GUEST_ROOT)
    }
    fn prepare(
        &self,
        yard: &Yard,
        record: &Record,
        fence: &Fence,
        plan: &SandboxPlan,
    ) -> Result<Placement, String> {
        let spec = self.spec(&sandbox_name(&record.info.name)?);
        Placement::exec_copied(
            yard,
            record,
            fence,
            plan,
            self.open(yard)?,
            spec,
            &self.pass_env,
        )
    }
    fn recover(&self, yard: &Yard, record: &Record, sandbox: &str) -> Option<String> {
        Some(native::recover(self, yard, record, sandbox))
    }
    fn open_untraced(&self, yard: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        branchyard_agent_sandbox::AgentSandboxProvider::new(
            self.config_with(
                store_volume(yard),
                yard_egress(yard),
                credential_sidecar(yard),
                warm_pool(yard),
            ),
            native::state_dir(yard, &self.key()),
        )
        .map(|provider| Arc::new(provider) as Arc<dyn SandboxProvider>)
        .map_err(|error| error.to_string())
    }
    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String> {
        native::destroy(self, yard, sandbox)
    }
}
