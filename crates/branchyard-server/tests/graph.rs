mod common;
use branchyard_protocol::*;
use branchyard_server::{graph::Graph, Error};
use common::*;

#[test]
fn dynamic_children_resolve_independent_of_proposal_order() {
    let (config, principal, command) = setup();
    let policy = &config.tenants[&principal.tenant];
    let (root_id, spec) = root(&command);
    let graph = Graph::create(root_id, spec.clone(), policy).unwrap();
    let child = TaskId::new();
    let grandchild = TaskId::new();
    let mut child_spec = spec.clone();
    child_spec.limits.max_depth = 2;
    let mut grand_spec = spec;
    grand_spec.limits.max_depth = 1;
    let edits = vec![
        GraphEdit::Spawn {
            task_id: grandchild,
            parent_id: child,
            spec: Box::new(grand_spec),
        },
        GraphEdit::Spawn {
            task_id: child,
            parent_id: root_id,
            spec: Box::new(child_spec),
        },
    ];
    let next = graph.apply(&principal, 0, &edits, policy).unwrap();
    assert_eq!(next.nodes.len(), 3);
    assert!(next.within(grandchild, child));
    assert!(!next.within(root_id, child));
    assert_eq!(graph.nodes.len(), 1);
}
#[test]
fn cycles_and_depth_escalation_do_not_mutate_the_input() {
    let (config, principal, command) = setup();
    let policy = &config.tenants[&principal.tenant];
    let (id, spec) = root(&command);
    let graph = Graph::create(id, spec.clone(), policy).unwrap();
    let (delta, child) = spawn(&command, 0);
    let Action::ApplyGraph { edits, .. } = delta.action else {
        unreachable!()
    };
    let next = graph.apply(&principal, 0, &edits, policy).unwrap();
    let cycle = vec![
        GraphEdit::AddDependency {
            task_id: id,
            depends_on: child,
        },
        GraphEdit::AddDependency {
            task_id: child,
            depends_on: id,
        },
    ];
    assert!(matches!(
        next.apply(&principal, 1, &cycle, policy),
        Err(Error::Invalid)
    ));
    let escalation = vec![GraphEdit::Spawn {
        task_id: TaskId::new(),
        parent_id: child,
        spec: Box::new(spec),
    }];
    assert!(matches!(
        next.apply(&principal, 1, &escalation, policy),
        Err(Error::Forbidden)
    ));
    assert_eq!(next.nodes.len(), 2);
    assert!(next.dependencies.is_empty());
}
#[test]
fn scoped_parent_can_delegate_but_cannot_touch_siblings_or_create_authority() {
    let (config, principal, command) = setup();
    let policy = &config.tenants[&principal.tenant];
    let (id, spec) = root(&command);
    let graph = Graph::create(id, spec, policy).unwrap();
    let (delta, child) = spawn(&command, 0);
    let Action::ApplyGraph { edits, .. } = delta.action else {
        unreachable!()
    };
    let graph = graph.apply(&principal, 0, &edits, policy).unwrap();
    let scoped = branchyard_server::config::Principal {
        subtree: Some(child),
        ..principal
    };
    assert!(matches!(
        graph.cancel(&scoped, id, 2, true),
        Err(Error::Forbidden)
    ));
    assert!(graph.cancel(&scoped, child, 1, true).is_ok());
    let mut spec = graph.nodes[&child].spec.clone();
    spec.limits.max_depth -= 1;
    let delta = vec![GraphEdit::Spawn {
        task_id: TaskId::new(),
        parent_id: child,
        spec: Box::new(spec),
    }];
    assert!(graph.apply(&scoped, 1, &delta, policy).is_ok());
}
#[test]
fn unknown_sources_sharing_and_unqualified_profiles_fail_closed() {
    let (mut config, principal, command) = setup();
    let policy = config.tenants.get_mut(&principal.tenant).unwrap();
    let (_, spec) = root(&command);
    let mut shared = spec.clone();
    shared.components.push(ComponentBinding {
        component_id: "cache".into(),
        access: ComponentAccess::ExclusiveWrite,
    });
    assert!(matches!(
        policy.validate_spec(&shared),
        Err(Error::Unsupported)
    ));
    policy
        .harnesses
        .get_mut(&spec.harness_profile)
        .unwrap()
        .qualified = false;
    assert!(matches!(
        policy.validate_spec(&spec),
        Err(Error::Unsupported)
    ));
}
#[test]
fn credentials_require_expiry_scope_and_registered_actions() {
    let (mut config, _, _) = setup();
    config.validate().unwrap();
    assert!(config.authenticate(TOKEN).is_ok());
    assert!(config.authenticate("wrong").is_err());
    config.credentials[0].principal.expires_at_unix = 1;
    assert!(config.authenticate(TOKEN).is_err());
    config.credentials[0]
        .principal
        .actions
        .insert("arbitrary_shell".into());
    assert!(config.validate().is_err());
}
