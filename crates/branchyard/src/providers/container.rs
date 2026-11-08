//! Owned containers reached through the shared copied/exec placement.

use std::sync::Arc;

use branchyard_sandbox::SandboxProvider;
use serde_json::json;

use super::{native, HarnessLocation, ProviderKind};
use crate::placement::{sandbox_name, Placement, SandboxPlan, HOME, WORKSPACE};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{ContainerOptions, ContainerRuntime, Error, Yard};

impl ProviderKind for ContainerOptions {
    fn harness_location(&self) -> HarnessLocation<'_> {
        HarnessLocation::Image
    }
    fn validate_options(&self) -> Result<(), Error> {
        self.validate()
    }
    fn name(&self) -> &'static str {
        "container"
    }
    fn compute(&self) -> super::ProviderCompute {
        super::ProviderCompute {
            network: Some(match self.network {
                crate::ContainerNetwork::Deny => "deny",
                crate::ContainerNetwork::Unrestricted => "unrestricted",
            }),
            ..super::ProviderCompute::plain(
                super::RunsOn::Worker,
                match self.runtime {
                    ContainerRuntime::Gvisor => super::Isolation::Gvisor,
                    ContainerRuntime::Runc => super::Isolation::Runc,
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
            ContainerRuntime::Gvisor => "gVisor",
            ContainerRuntime::Runc => "Linux namespace",
        };
        ("sandbox",format!("harnesses run in owned {runtime} containers; workspace and home are copied in and back; each allocation is destroyed after capture"))
    }
    fn lifecycle(&self) -> Option<Lifecycle> {
        native::lifecycle()
    }
    fn copies_worktree(&self) -> bool {
        true
    }
    fn check(&self, _yard: &Yard) -> Result<(), Error> {
        self.validate()
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
        let config = branchyard_container::Config {
            runtime: self.runtime,
            network: self.network,
        };
        branchyard_container::ContainerProvider::new(config, native::state_dir(yard, &self.key()))
            .map(|provider| Arc::new(provider) as Arc<dyn SandboxProvider>)
            .map_err(|error| error.to_string())
    }
    fn destroy(&self, yard: &Yard, sandbox: &str) -> Result<String, String> {
        native::destroy(self, yard, sandbox)
    }
}
