//! [`crate::Provider::Local`]: a local process as your user, with no
//! sandbox.

use std::sync::Arc;

use branchyard_sandbox::SandboxProvider;

use super::ProviderKind;
use crate::placement::{Placement, SandboxPlan};
use crate::snapshots::Lifecycle;
use crate::state::{Fence, Record};
use crate::{Error, Yard};

/// The local provider has no options.
pub(crate) struct Local;

/// The one [`Local`].
pub(crate) static LOCAL: Local = Local;

impl ProviderKind for Local {
    fn name(&self) -> &'static str {
        "local"
    }

    fn lifecycle(&self) -> Option<Lifecycle> {
        None
    }

    fn sandboxed(&self) -> bool {
        false
    }

    fn confines_egress(&self) -> bool {
        true
    }

    fn check(&self, _: &Yard) -> Result<(), Error> {
        Ok(())
    }

    /// A local harness's `HOME` is its private home, or this process's own.
    fn guest_paths(&self, record: &Record) -> (String, String) {
        (
            record.info.worktree.display().to_string(),
            record.home.as_ref().map_or_else(
                || std::env::var("HOME").unwrap_or_default(),
                |home| home.display().to_string(),
            ),
        )
    }

    fn prepare(
        &self,
        _: &Yard,
        record: &Record,
        _: &Fence,
        _: &SandboxPlan,
    ) -> Result<Placement, String> {
        Ok(Placement::local(record))
    }

    fn recover(&self, _: &Yard, _: &Record, _: &str) -> Option<String> {
        None
    }

    fn open(&self, _: &Yard) -> Result<Arc<dyn SandboxProvider>, String> {
        Err("a local harness has no sandbox".into())
    }

    fn destroy(&self, _: &Yard, _: &str) -> Result<String, String> {
        Ok("nothing to destroy".into())
    }
}
