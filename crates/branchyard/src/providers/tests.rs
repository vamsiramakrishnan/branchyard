//! One table over every [`Provider`] variant, exercised only through
//! [`ProviderKind`]: what a new variant must answer before it can ship.

use std::sync::Arc;

use serde_json::json;

use super::microsandbox::tests::Standin;
use super::*;
use crate::placement;
use crate::state::{SandboxKind, SandboxRow};
use crate::{RecipeOptions, SandboxOptions, SubstrateOptions};

/// The names of [`Provider`]'s variants, read from the enum's own source so
/// no count has to be bumped by hand. A variant is a line indented one level
/// inside `pub enum Provider { .. }` that starts with an uppercase letter
/// (doc comments and attributes do not).
fn variants_in_enum() -> Vec<String> {
    let source = include_str!("../lib.rs");
    let body = source
        .split("pub enum Provider {\n")
        .nth(1)
        .expect("`pub enum Provider` in lib.rs")
        .split("\n}\n")
        .next()
        .unwrap();
    body.lines()
        .filter_map(|line| line.strip_prefix("    "))
        .filter(|line| line.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|line| {
            line.split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// The variants `Provider::kind` has an arm for, read from its source.
fn variants_in_kind() -> Vec<String> {
    let source = include_str!("mod.rs");
    let body = source
        .split("fn kind(&self) -> &dyn ProviderKind {\n")
        .nth(1)
        .expect("`Provider::kind` in providers/mod.rs")
        .split("\n    }\n")
        .next()
        .unwrap();
    body.lines()
        .filter_map(|line| line.trim_start().strip_prefix("Provider::"))
        .map(|rest| {
            rest.split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// [`index`] is an exhaustive match, so adding a variant fails to compile
/// until it has an arm here; [`the_table_covers_every_variant`] then fails
/// until [`samples`] has an instance of it.
fn index(provider: &Provider) -> usize {
    match provider {
        Provider::Local => 0,
        Provider::Microsandbox(_) => 1,
        Provider::Substrate(_) => 2,
        Provider::Recipe(_) => 3,
    }
}

/// A repository, and a Substrate bridge key inside it.
struct Fixture {
    _dir: tempfile::TempDir,
    yard: Yard,
    key: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix("by-providers-")
        .tempdir()
        .unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(dir.path())
        .status()
        .unwrap();
    let key = dir.path().join("key");
    branchyard_bridge::Signer::write(&key).unwrap();
    let yard = Yard::open(dir.path()).unwrap();
    Fixture {
        _dir: dir,
        yard,
        key,
    }
}

/// One usable instance of each variant, and one its `check` refuses, in
/// [`index`] order.
fn samples(fixture: &Fixture) -> Vec<(Provider, Provider)> {
    let substrate = SubstrateOptions {
        // Nothing listens on the discard port: recovery and destroy fail
        // fast, which is the error path of those two.
        endpoint: "http://127.0.0.1:9".into(),
        router: "http://127.0.0.1:9/{atespace}/{actor}/".into(),
        template: "by".into(),
        key: fixture.key.clone(),
        ..SubstrateOptions::default()
    };
    let recipe = RecipeOptions {
        name: "lab".into(),
        create: "echo machine".into(),
        ..RecipeOptions::default()
    };
    vec![
        (Provider::Local, Provider::Local),
        (
            Provider::Microsandbox(SandboxOptions {
                image: "alpine:3.20".into(),
                ..SandboxOptions::default()
            }),
            Provider::Microsandbox(SandboxOptions::default()),
        ),
        (
            Provider::Substrate(substrate.clone()),
            Provider::Substrate(SubstrateOptions {
                template: " ".into(),
                ..substrate
            }),
        ),
        (
            Provider::Recipe(recipe.clone()),
            Provider::Recipe(RecipeOptions {
                create: " ".into(),
                ..recipe
            }),
        ),
    ]
}

fn record(provider: &Provider) -> Record {
    serde_json::from_value(json!({
        "info": {
            "name": "b", "git_branch": "by/b", "worktree": "/w",
            "prompt": "p", "harness": "h", "profile": "p", "session": null,
            "parent": null, "base": "b", "candidate": null,
            "status": {"state": "running"}, "turns": 0, "cost_usd": null,
            "created_at": 0
        },
        "created_ms": 0, "check": null, "command": null, "home": null,
        "cost_baseline": null,
        "provider": provider
    }))
    .unwrap()
}

#[test]
fn the_counter_reads_the_enum_and_kind() {
    // If these were empty the coverage test below would compare nothing.
    assert!(variants_in_enum().starts_with(&["Local".to_string()]));
    assert!(variants_in_enum().contains(&"Recipe".to_string()));
    assert_eq!(variants_in_kind(), variants_in_enum());
}

#[test]
fn the_table_covers_every_variant() {
    // The count comes from the enum, not from a constant: a new variant
    // raises it, and `Provider::kind` must have an arm for each.
    let variants = variants_in_enum();
    assert_eq!(
        variants_in_kind(),
        variants,
        "`Provider::kind` must map every variant of the enum, in its order"
    );
    let fixture = fixture();
    let mut seen: Vec<usize> = samples(&fixture)
        .iter()
        .flat_map(|(good, bad)| [index(good), index(bad)])
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen,
        (0..variants.len()).collect::<Vec<_>>(),
        "`Provider` has {} variants ({variants:?}); `samples` must hold a usable and a refused \
         instance of each, and `index` must number them 0..{}",
        variants.len(),
        variants.len()
    );
}

#[test]
fn each_variant_serializes_under_its_own_name() {
    let fixture = fixture();
    for (good, _) in samples(&fixture) {
        let value = serde_json::to_value(&good).unwrap();
        assert_eq!(value["kind"], good.kind().name(), "{good:?}");
        let back: Provider = serde_json::from_value(value).unwrap();
        assert_eq!(back, good);
    }
    // No provider at all is a local harness.
    assert_eq!(of(None).name(), "local");
    assert!(!of(None).sandboxed());
}

#[test]
fn names_keys_and_kinds() {
    let fixture = fixture();
    let expected = [
        ("local", "local", false, "sandbox"),
        ("microsandbox", "microsandbox", true, "sandbox"),
        (
            "substrate",
            "substrate:http://127.0.0.1:9/default/by",
            true,
            "sandbox",
        ),
        ("recipe", "recipe:lab", true, "recipe_machine"),
    ];
    for (good, _) in samples(&fixture) {
        let (name, key, sandboxed, service) = expected[index(&good)];
        let kind = good.kind();
        assert_eq!(kind.name(), name);
        assert_eq!(kind.key(), key);
        assert_eq!(kind.sandboxed(), sandboxed);
        assert_eq!(kind.service_kind(), service);
        assert_eq!(kind.confines_egress(), !sandboxed);
        assert_eq!(placement::sandboxed(Some(&good)), sandboxed);
    }
}

#[test]
fn check_accepts_the_usable_and_refuses_the_rest() {
    let fixture = fixture();
    for (good, bad) in samples(&fixture) {
        let name = good.kind().name();
        let usable = match name {
            // Needs the SDK, or a yard's own sandbox provider.
            "microsandbox" => branchyard_microsandbox::ENABLED,
            _ => true,
        };
        assert_eq!(
            placement::check(&fixture.yard, Some(&good)).is_ok(),
            usable,
            "{name}"
        );
        match name {
            "local" => assert!(placement::check(&fixture.yard, Some(&bad)).is_ok()),
            _ => assert!(
                matches!(
                    placement::check(&fixture.yard, Some(&bad)),
                    Err(Error::Unsupported(_))
                ),
                "{name}"
            ),
        }
    }
    assert!(placement::check(&fixture.yard, None).is_ok());
    // A yard given its own sandbox provider runs Microsandbox without the SDK.
    fixture
        .yard
        .use_sandbox_provider(Arc::new(Standin::default()));
    let image = samples(&fixture).remove(1).0;
    assert!(placement::check(&fixture.yard, Some(&image)).is_ok());
}

#[test]
fn lifecycle_defaults_and_overrides() {
    let fixture = fixture();
    for (good, _) in samples(&fixture) {
        let lifecycle = good.kind().lifecycle();
        match index(&good) {
            0 => assert_eq!(lifecycle, None),
            _ => {
                let lifecycle = lifecycle.expect("a sandbox has a lifecycle");
                assert!(!lifecycle.keep, "{good:?}");
                assert_eq!(lifecycle.max_paused, crate::snapshots::DEFAULT_MAX_PAUSED);
                // A recipe's machine has no checkpoints to keep.
                let snapshots = match index(&good) {
                    3 => 0,
                    _ => crate::snapshots::DEFAULT_SNAPSHOTS,
                };
                assert_eq!(lifecycle.snapshots, snapshots, "{good:?}");
            }
        }
        assert_eq!(crate::snapshots::lifecycle(Some(&good)), lifecycle);
    }
    let kept = Provider::Microsandbox(SandboxOptions {
        keep: crate::SandboxKeep::Pause,
        snapshots: Some(7),
        max_paused: Some(2),
        ..SandboxOptions::default()
    });
    let lifecycle = kept.kind().lifecycle().unwrap();
    assert_eq!(
        (lifecycle.keep, lifecycle.snapshots, lifecycle.max_paused),
        (true, 7, 2)
    );
    // Pausing a recipe's machine needs both of its scripts.
    let paused = |suspend: Option<&str>| {
        Provider::Recipe(RecipeOptions {
            name: "lab".into(),
            create: "echo".into(),
            keep: crate::SandboxKeep::Pause,
            suspend: suspend.map(Into::into),
            resume: suspend.map(Into::into),
            ..RecipeOptions::default()
        })
    };
    assert!(placement::check(&fixture.yard, Some(&paused(Some("s")))).is_ok());
    assert!(matches!(
        placement::check(&fixture.yard, Some(&paused(None))),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn guest_paths_are_where_the_harness_will_see_them() {
    let fixture = fixture();
    for (good, _) in samples(&fixture) {
        let record = record(&good);
        let (workdir, home) = placement::guest_paths(&record);
        match index(&good) {
            0 => assert_eq!(workdir, "/w"),
            1 | 2 => {
                assert_eq!(workdir, placement::WORKSPACE);
                assert_eq!(home, placement::HOME);
            }
            _ => {
                assert!(workdir.starts_with("/tmp/branchyard/b-"), "{workdir}");
                assert!(home.ends_with("/home"), "{home}");
            }
        }
    }
}

/// Recovery of what a stopped engine's turn journaled: nothing for a local
/// harness, a message for each sandbox, whether the sandbox was there or
/// not, and whether its provider could be reached or not.
#[test]
fn recover_reports_every_outcome() {
    let fixture = fixture();
    let yard = &fixture.yard;
    let standin = Standin::default();
    standin.known.lock().unwrap().push("by-b-1".into());
    yard.use_sandbox_provider(Arc::new(standin.clone()));
    for (good, _) in samples(&fixture) {
        let record = record(&good);
        let intent = json!({ "provider": good.kind().name(), "sandbox": "by-b-1" });
        let said = placement::recover(yard, &record, &intent, None);
        match index(&good) {
            0 => assert_eq!(said, None),
            1 => assert_eq!(
                said.as_deref(),
                Some("destroyed its Microsandbox sandbox by-b-1")
            ),
            2 => {
                // Nothing listens at the endpoint: an error path.
                let said = said.expect("said");
                assert!(
                    said.starts_with("could not delete its Substrate actor by-b-1"),
                    "{said}"
                );
            }
            _ => assert_eq!(
                said.as_deref(),
                Some("its machine by-b-1 was already gone"),
                "no machine of that name was ever recorded"
            ),
        }
    }
    // A parked sandbox is left alone, whatever the provider.
    for (good, _) in samples(&fixture) {
        let intent = json!({ "sandbox": "by-b-2" });
        let parked = json!({ "kept": true });
        let said = placement::recover(yard, &record(&good), &intent, Some(&parked));
        assert_eq!(
            said.as_deref(),
            Some("its sandbox by-b-2 was already kept paused for the next turn")
        );
    }
    // Without a journaled sandbox there is nothing to recover.
    let intent = json!({ "provider": "local" });
    assert_eq!(
        placement::recover(yard, &record(&Provider::Local), &intent, None),
        None
    );
    // A kept record for a sandbox destroyed here is removed.
    yard.store()
        .sandboxes()
        .put_sandbox(&SandboxRow {
            branch: "b".into(),
            incarnation: 1,
            kind: SandboxKind::Kept,
            provider: "microsandbox".into(),
            name: "by-b-3".into(),
            turn: None,
            detail: "{}".into(),
            used_ms: 1,
        })
        .unwrap();
    standin.known.lock().unwrap().push("by-b-3".into());
    let micro = samples(&fixture).remove(1).0;
    let intent = json!({ "sandbox": "by-b-3" });
    assert!(placement::recover(yard, &record(&micro), &intent, None).is_some());
    assert!(yard.store().sandboxes().sandboxes("b").unwrap().is_empty());
}

/// Opening a provider and destroying through it, as the reaper does.
#[test]
fn open_and_destroy_through_the_trait() {
    let fixture = fixture();
    let yard = &fixture.yard;
    let standin = Standin::default();
    standin.known.lock().unwrap().push("by-b-1".into());
    yard.use_sandbox_provider(Arc::new(standin.clone()));
    for (good, _) in samples(&fixture) {
        let kind = good.kind();
        let opened = kind.open(yard);
        let destroyed = kind.destroy(yard, "by-b-1");
        match index(&good) {
            0 => {
                assert!(opened.is_err());
                assert_eq!(destroyed.as_deref(), Ok("nothing to destroy"));
            }
            1 => {
                assert!(opened.is_ok());
                assert_eq!(
                    destroyed.as_deref(),
                    Ok("destroyed its Microsandbox sandbox by-b-1")
                );
                assert_eq!(*standin.destroyed.lock().unwrap(), ["by-b-1"]);
            }
            2 => {
                // `open` signs with the key and connects lazily; destroy
                // cannot reach the unreachable endpoint.
                let why = destroyed.expect_err("nothing listens");
                assert!(why.contains("by-b-1") || why.contains("Substrate"), "{why}");
            }
            _ => {
                assert!(opened.is_ok());
                assert_eq!(destroyed.as_deref(), Ok("machine by-b-1 was already gone"));
            }
        }
    }
    // A Microsandbox destroy that fails is an error, not a message.
    let failing = Standin {
        fail: true,
        ..standin
    };
    yard.use_sandbox_provider(Arc::new(failing));
    let micro = samples(&fixture).remove(1).0;
    assert!(micro.kind().destroy(yard, "by-b-1").is_err());
}
