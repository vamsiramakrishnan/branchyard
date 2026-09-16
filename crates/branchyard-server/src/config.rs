use crate::{Error, Result};
use branchyard_protocol::{sha256, HarnessProfile, TaskId, TaskSpec, WorkspaceSource};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub tenants: BTreeMap<String, TenantPolicy>,
    pub credentials: Vec<Credential>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantPolicy {
    pub max_reserved_tasks: u32,
    pub max_reserved_cpu_millis: u64,
    pub max_reserved_memory_mib: u64,
    pub max_tasks_per_root: u16,
    pub max_root_cpu_millis: u64,
    pub max_root_memory_mib: u64,
    pub max_wall_seconds: u32,
    pub max_depth: u16,
    pub max_children: u16,
    pub harnesses: BTreeMap<String, HarnessProfile>,
    pub environments: BTreeSet<String>,
    pub policies: BTreeSet<String>,
    pub repositories: BTreeSet<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    /// SHA-256 of a high-entropy bearer token. Plaintext tokens never enter config.
    pub token_sha256: String,
    pub principal: Principal,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub tenant: String,
    pub subject: String,
    pub expires_at_unix: u64,
    pub actions: BTreeSet<String>,
    /// None is a tenant controller. A subtree credential cannot create new roots.
    pub subtree: Option<TaskId>,
}
impl Principal {
    pub fn allows(&self, action: &str) -> Result<()> {
        if self.actions.contains(action) {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.tenants.is_empty() || self.credentials.is_empty() {
            return Err(Error::Config);
        }
        for (name, p) in &self.tenants {
            if name.is_empty()
                || name.len() > 128
                || p.max_reserved_tasks == 0
                || p.max_tasks_per_root == 0
                || p.max_tasks_per_root > 256
                || p.max_wall_seconds == 0
                || p.max_reserved_cpu_millis == 0
                || p.max_reserved_memory_mib == 0
                || p.max_root_cpu_millis == 0
                || p.max_root_memory_mib == 0
                || p.max_reserved_cpu_millis > i64::MAX as u64
                || p.max_reserved_memory_mib > i64::MAX as u64
                || p.harnesses.len() > 64
            {
                return Err(Error::Config);
            }
            for (id, profile) in &p.harnesses {
                if id != &profile.id
                    || id.len() > 128
                    || profile.driver.len() > 128
                    || profile.capabilities.len() > 32
                    || profile.capabilities.iter().any(|s| s.len() > 128)
                {
                    return Err(Error::Config);
                }
            }
        }
        let mut keys = BTreeSet::new();
        for c in &self.credentials {
            if c.token_sha256.len() != 64
                || !c
                    .token_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                || !keys.insert(&c.token_sha256)
                || !self.tenants.contains_key(&c.principal.tenant)
                || c.principal.subject.is_empty()
                || c.principal.subject.len() > 128
                || c.principal.subtree.is_some_and(|id| id.0.is_nil())
            {
                return Err(Error::Config);
            }
            if c.principal.actions.iter().any(|a| {
                !["read", "create_task", "apply_graph", "cancel_task"].contains(&a.as_str())
            }) {
                return Err(Error::Config);
            }
        }
        Ok(())
    }
    pub fn authenticate(&self, token: &str) -> Result<Principal> {
        if token.len() < 32 || token.len() > 8192 {
            return Err(Error::Unauthorized);
        }
        let digest = sha256(token.as_bytes());
        let principal = self
            .credentials
            .iter()
            .find(|c| c.token_sha256 == digest)
            .ok_or(Error::Unauthorized)?
            .principal
            .clone();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Unauthorized)?
            .as_secs();
        if principal.expires_at_unix <= now {
            return Err(Error::Unauthorized);
        }
        Ok(principal)
    }
}
impl TenantPolicy {
    pub fn validate_spec(&self, spec: &TaskSpec) -> Result<()> {
        spec.validate().map_err(|_| Error::Invalid)?;
        let profile = self
            .harnesses
            .get(&spec.harness_profile)
            .ok_or(Error::Unsupported)?;
        if !profile.qualified
            || !spec.required_capabilities.is_subset(&profile.capabilities)
            || !self.environments.contains(&spec.environment_profile)
            || !self.policies.contains(&spec.policy_profile)
        {
            return Err(Error::Unsupported);
        }
        // Checkpoints/components require ownership and lifecycle registries; fail closed until supplied.
        match &spec.workspace {
            WorkspaceSource::Repository { repository_id, .. }
                if self.repositories.contains(repository_id) => {}
            _ => return Err(Error::Unsupported),
        }
        if !spec.components.is_empty() {
            return Err(Error::Unsupported);
        }
        if spec.limits.wall_seconds > self.max_wall_seconds
            || spec.limits.max_depth > self.max_depth
            || spec.limits.max_children > self.max_children
        {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}
