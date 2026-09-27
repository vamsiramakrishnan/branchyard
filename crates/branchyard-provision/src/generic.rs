//! Harnesses without a translated provisioner: session items pass through
//! to the driver, which accepts or refuses them; nothing is written.

use crate::{pass_session, unsupported, Context, Plan, Provisioner, Refused};

pub(crate) struct Generic;

impl Provisioner for Generic {
    fn harness(&self) -> &'static str {
        "*"
    }

    fn scion(&self) -> Option<&'static str> {
        None
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Branchyard has no provisioner for it";
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(context, "telemetry", why));
        }
        if context.auth.is_some() {
            return Err(unsupported(context, "an auth method", why));
        }
        let mut plan = Plan {
            unused_secrets: context.secrets.iter().map(|s| s.name.clone()).collect(),
            ..Plan::default()
        };
        // The drivers pass these on or refuse them with their own reason.
        pass_session(context, &mut plan);
        plan.session.model = context.model.clone();
        Ok(plan)
    }
}
