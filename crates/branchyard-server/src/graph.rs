//! Pure proposed-state validation. The store commits the result with reservations and queue writes.
use crate::{
    config::{Principal, TenantPolicy},
    Error, Result,
};
use branchyard_protocol::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub task: Task,
    pub spec: TaskSpec,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graph {
    pub root: TaskId,
    pub revision: u64,
    pub nodes: BTreeMap<TaskId, Node>,
    /// (dependent, prerequisite). Ownership is stored separately on each node.
    pub dependencies: BTreeSet<(TaskId, TaskId)>,
}
impl Graph {
    pub fn create(id: TaskId, spec: TaskSpec, policy: &TenantPolicy) -> Result<Self> {
        policy.validate_spec(&spec)?;
        let mut graph = Self {
            root: id,
            revision: 0,
            nodes: BTreeMap::new(),
            dependencies: BTreeSet::new(),
        };
        graph.insert(id, None, spec);
        Ok(graph)
    }
    fn insert(&mut self, id: TaskId, parent_id: Option<TaskId>, spec: TaskSpec) {
        self.nodes.insert(
            id,
            Node {
                task: Task {
                    schema: Version::V1Alpha1,
                    task_id: id,
                    root_id: self.root,
                    parent_id,
                    revision: 1,
                    graph_revision: self.revision,
                    state: TaskState::Queued,
                    effective_capabilities: BTreeSet::new(),
                    artifacts: vec![],
                },
                spec,
            },
        );
    }
    pub fn within(&self, id: TaskId, scope: TaskId) -> bool {
        let mut at = Some(id);
        for _ in 0..=self.nodes.len() {
            let Some(current) = at else {
                return false;
            };
            if current == scope {
                return self.nodes.contains_key(&scope);
            }
            at = self.nodes.get(&current).and_then(|n| n.task.parent_id);
        }
        false
    }
    pub fn authorize(&self, principal: &Principal, id: TaskId) -> Result<()> {
        if !self.nodes.contains_key(&id) {
            return Err(Error::NotFound);
        }
        if principal
            .subtree
            .is_some_and(|scope| !self.within(id, scope))
        {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub fn apply(
        &self,
        principal: &Principal,
        expected: u64,
        edits: &[GraphEdit],
        policy: &TenantPolicy,
    ) -> Result<Self> {
        if expected != self.revision {
            return Err(Error::Conflict);
        }
        let mut next = self.clone();
        // Insert all proposed identities first so edit ordering has no semantic effect.
        for edit in edits {
            if let GraphEdit::Spawn {
                task_id,
                parent_id,
                spec,
            } = edit
            {
                if next.nodes.contains_key(task_id) {
                    return Err(Error::Conflict);
                }
                policy.validate_spec(spec)?;
                next.insert(*task_id, Some(*parent_id), *spec.clone());
            }
        }
        if next.nodes.len() > policy.max_tasks_per_root as usize {
            return Err(Error::Capacity);
        }
        for edit in edits {
            match edit {
                GraphEdit::Spawn {
                    parent_id, spec, ..
                } => {
                    next.authorize(principal, *parent_id)?;
                    let parent = next.nodes.get(parent_id).ok_or(Error::Invalid)?;
                    if !matches!(
                        parent.task.state,
                        TaskState::Queued | TaskState::Blocked | TaskState::Running
                    ) {
                        return Err(Error::Conflict);
                    }
                    if spec.policy_profile != parent.spec.policy_profile
                        || parent.spec.limits.max_depth == 0
                        || spec.limits.max_depth >= parent.spec.limits.max_depth
                        || spec.limits.wall_seconds > parent.spec.limits.wall_seconds
                        || spec.limits.cpu_millis > parent.spec.limits.cpu_millis
                        || spec.limits.memory_mib > parent.spec.limits.memory_mib
                        || spec.limits.max_children > parent.spec.limits.max_children
                    {
                        return Err(Error::Forbidden);
                    }
                }
                GraphEdit::AddDependency {
                    task_id,
                    depends_on,
                }
                | GraphEdit::RemoveDependency {
                    task_id,
                    depends_on,
                } => {
                    next.authorize(principal, *task_id)?;
                    next.authorize(principal, *depends_on)?;
                    if !matches!(
                        next.nodes[task_id].task.state,
                        TaskState::Queued | TaskState::Blocked
                    ) {
                        return Err(Error::Conflict);
                    }
                    let edge = (*task_id, *depends_on);
                    match edit {
                        GraphEdit::AddDependency { .. } if !next.dependencies.insert(edge) => {
                            return Err(Error::Conflict)
                        }
                        GraphEdit::RemoveDependency { .. } if !next.dependencies.remove(&edge) => {
                            return Err(Error::Conflict)
                        }
                        _ => {}
                    }
                }
            }
        }
        next.validate_topology()?;
        next.revision = next.revision.checked_add(1).ok_or(Error::Capacity)?;
        for (id, node) in &mut next.nodes {
            node.task.graph_revision = next.revision;
            if matches!(node.task.state, TaskState::Queued | TaskState::Blocked) {
                node.task.state = if next.dependencies.iter().any(|(dependent, prerequisite)| {
                    dependent == id
                        && self
                            .nodes
                            .get(prerequisite)
                            .is_none_or(|n| n.task.state != TaskState::Completed)
                }) {
                    TaskState::Blocked
                } else {
                    TaskState::Queued
                };
            }
            if let Some(old) = self.nodes.get(id) {
                // Every successful topology delta invalidates observed task revisions consistently.
                node.task.revision = old.task.revision.checked_add(1).ok_or(Error::Capacity)?;
            }
        }
        Ok(next)
    }
    fn validate_topology(&self) -> Result<()> {
        let mut children: BTreeMap<TaskId, usize> = BTreeMap::new();
        for (id, node) in &self.nodes {
            let mut seen = BTreeSet::new();
            let mut at = Some(*id);
            while let Some(current) = at {
                if !seen.insert(current) {
                    return Err(Error::Invalid);
                }
                let n = self.nodes.get(&current).ok_or(Error::Invalid)?;
                at = n.task.parent_id;
                if at.is_none() && current != self.root {
                    return Err(Error::Invalid);
                }
            }
            if let Some(parent) = node.task.parent_id {
                *children.entry(parent).or_default() += 1;
            }
        }
        for (parent, count) in children {
            if count > self.nodes[&parent].spec.limits.max_children as usize {
                return Err(Error::Capacity);
            }
        }
        let mut degree: BTreeMap<TaskId, usize> = self.nodes.keys().map(|id| (*id, 0)).collect();
        for (dependent, _) in &self.dependencies {
            *degree.get_mut(dependent).ok_or(Error::Invalid)? += 1;
        }
        let mut ready: VecDeque<_> = degree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut visited = 0;
        while let Some(id) = ready.pop_front() {
            visited += 1;
            for (dependent, prerequisite) in &self.dependencies {
                if *prerequisite == id {
                    let d = degree.get_mut(dependent).ok_or(Error::Invalid)?;
                    *d -= 1;
                    if *d == 0 {
                        ready.push_back(*dependent);
                    }
                }
            }
        }
        if visited != self.nodes.len() {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn cancel(
        &self,
        principal: &Principal,
        id: TaskId,
        expected: u64,
        cascade: bool,
    ) -> Result<Self> {
        self.authorize(principal, id)?;
        if self.nodes[&id].task.revision != expected {
            return Err(Error::Conflict);
        }
        let mut next = self.clone();
        for (task_id, node) in &mut next.nodes {
            if (*task_id == id || (cascade && self.within(*task_id, id)))
                && !matches!(
                    node.task.state,
                    TaskState::Completed | TaskState::Failed | TaskState::Cancelled
                )
            {
                node.task.state = TaskState::CancelRequested;
                node.task.revision = node.task.revision.checked_add(1).ok_or(Error::Capacity)?;
            }
        }
        Ok(next)
    }
    pub fn reservation(&self) -> (u64, u64, u64) {
        // Retain reservations through cancel_requested/unknown. Only verified teardown may release them.
        self.nodes
            .values()
            .filter(|n| {
                !matches!(
                    n.task.state,
                    TaskState::Completed | TaskState::Failed | TaskState::Cancelled
                )
            })
            .fold((0, 0, 0), |(tasks, cpu, memory), node| {
                (
                    tasks + 1,
                    cpu + node.spec.limits.cpu_millis as u64,
                    memory + node.spec.limits.memory_mib as u64,
                )
            })
    }
}
