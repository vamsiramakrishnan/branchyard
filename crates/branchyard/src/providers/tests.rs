//! One table over every [`Provider`] variant, exercised only through
//! [`ProviderKind`]: what a new variant must answer before it can ship.

use std::sync::Arc;

use serde_json::json;

use super::microsandbox::tests::Standin;
use super::*;
use crate::placement;
use crate::state::{SandboxKind, SandboxRow};
use crate::{
    AgentSandboxOptions, ContainerOptions, RecipeOptions, SandboxOptions, SubstrateOptions,
};

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
        Provider::Container(_) => 4,
        Provider::AgentSandbox(_) => 5,
    }
}

/// A repository, and a Substrate bridge key inside it.
struct Fixture {
    _dir: tempfile::TempDir,
    yard: Yard,
    key: std::path::PathBuf,
}

fn fixture() -> Fixture {
    native_binaries();
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

fn native_binaries() {
    static BIN: std::sync::OnceLock<branchyard_testkit::Scratch> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let scratch = branchyard_testkit::Scratch::new("provider-binaries");
        let source =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native-docker");
        std::os::unix::fs::symlink(source, scratch.join("docker")).unwrap();
        let mut paths = vec![scratch.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        scratch
    });
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
        (
            Provider::container(ContainerOptions {
                image: format!("example.test/runtime@sha256:{}", "a".repeat(64)),
                ..ContainerOptions::default()
            }),
            Provider::container(ContainerOptions::default()),
        ),
        (
            Provider::agent_sandbox(AgentSandboxOptions {
                context: "test-context".into(),
                namespace: "branchyard-gvisor".into(),
                image: format!("example.test/runtime@sha256:{}", "a".repeat(64)),
                ..AgentSandboxOptions::default()
            }),
            Provider::agent_sandbox(AgentSandboxOptions::default()),
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
        assert_eq!(value["kind"], good.name(), "{good:?}");
        let back: Provider = serde_json::from_value(value).unwrap();
        assert_eq!(back, good);
    }
    // No provider at all is a local harness.
    assert_eq!(of(None).name(), "local");
    assert!(!of(None).sandboxed());
    assert!(!of(None).copies_worktree());
}

#[test]
fn public_harness_locations_cover_the_inventory_and_borrow_the_selected_target() {
    let fixture = fixture();
    for (provider, _) in samples(&fixture) {
        match (&provider, provider.harness_location()) {
            (Provider::Local, HarnessLocation::Local)
            | (Provider::Microsandbox(_), HarnessLocation::Microsandbox)
            | (Provider::Container(_) | Provider::AgentSandbox(_), HarnessLocation::Image) => {}
            (Provider::Substrate(expected), HarnessLocation::Substrate(actual)) => {
                assert!(std::ptr::eq(expected, actual));
            }
            (Provider::Recipe(expected), HarnessLocation::Recipe(actual)) => {
                assert!(std::ptr::eq(expected, actual));
            }
            (_, location) => panic!(
                "{} has the wrong harness location: {location:?}",
                provider.name()
            ),
        }
        assert_eq!(
            provider.is_native_image(),
            matches!(provider.harness_location(), HarnessLocation::Image),
            "{}",
            provider.name()
        );
    }
}

#[test]
fn public_option_validation_checks_native_specs_without_opening_a_runtime() {
    let fixture = fixture();
    for (good, bad) in samples(&fixture) {
        assert!(good.validate_options().is_ok(), "{}", good.name());
        let native = matches!(&good, Provider::Container(_) | Provider::AgentSandbox(_));
        assert_eq!(good.is_native_image(), native, "{}", good.name());
        if native {
            assert!(matches!(bad.validate_options(), Err(Error::Unsupported(_))));
            for (field, value) in [
                ("image", json!("example.test/runtime:unpinned")),
                ("cpus", json!(0)),
                ("memory_mib", json!(0)),
                ("pass_env", json!(["KUBECONFIG"])),
            ] {
                let mut invalid = serde_json::to_value(&good).unwrap();
                invalid[field] = value;
                let invalid: Provider = serde_json::from_value(invalid).unwrap();
                assert!(
                    matches!(invalid.validate_options(), Err(Error::Unsupported(_))),
                    "{} must reject invalid {field}",
                    good.name()
                );
            }
        } else {
            // Legacy providers defer runtime-dependent checks to preparation.
            assert!(bad.validate_options().is_ok(), "{}", bad.name());
        }
    }
    assert!(
        !fixture.yard.root().join(".branchyard/native").exists(),
        "pure option validation created native state"
    );
}

#[test]
fn every_variant_describes_its_compute() {
    let fixture = fixture();
    let expected = [
        (RunsOn::Worker, Isolation::Process, None, None),
        (RunsOn::Worker, Isolation::Microsandbox, None, None),
        (RunsOn::Substrate, Isolation::Substrate, None, None),
        (RunsOn::Worker, Isolation::Recipe, None, None),
        (RunsOn::Worker, Isolation::Gvisor, Some("deny"), None),
        (
            RunsOn::Kubernetes,
            Isolation::Gvisor,
            Some("deny"),
            Some("branchyard-gvisor"),
        ),
    ];
    for (good, _) in samples(&fixture) {
        let (runs_on, isolation, network, namespace) = expected[index(&good)];
        let compute = good.compute();
        assert_eq!(compute.runs_on, runs_on, "{}", good.name());
        assert_eq!(compute.isolation, isolation, "{}", good.name());
        assert_eq!(compute.network, network, "{}", good.name());
        assert_eq!(compute.namespace.as_deref(), namespace, "{}", good.name());
    }
    let runc = Provider::container(ContainerOptions {
        runtime: crate::ContainerRuntime::Runc,
        network: crate::ContainerNetwork::Unrestricted,
        ..ContainerOptions::default()
    });
    assert_eq!(runc.compute().isolation, Isolation::Runc);
    assert_eq!(runc.compute().network, Some("unrestricted"));
    let kata = Provider::agent_sandbox(AgentSandboxOptions {
        runtime: crate::AgentSandboxRuntime::Microvm,
        ..AgentSandboxOptions::default()
    });
    assert_eq!(kata.compute().isolation, Isolation::Kata);
    assert_eq!(
        serde_json::to_value(kata.compute()).unwrap(),
        json!({"runs_on": "kubernetes", "isolation": "kata", "network": "deny", "namespace": ""})
    );
}

#[test]
fn names_keys_and_kinds() {
    let fixture = fixture();
    let expected = [
        ("local", "local", false, false, "sandbox"),
        ("microsandbox", "microsandbox", true, false, "sandbox"),
        (
            "substrate",
            "substrate:http://127.0.0.1:9/default/by",
            true,
            true,
            "sandbox",
        ),
        ("recipe", "recipe:lab", true, true, "recipe_machine"),
        ("container", "container:", true, true, "sandbox"),
        ("agent_sandbox", "agent_sandbox:", true, true, "sandbox"),
    ];
    for (good, _) in samples(&fixture) {
        let (name, key, sandboxed, copied, service) = expected[index(&good)];
        let kind = good.kind();
        assert_eq!(kind.name(), name);
        if index(&good) >= 4 {
            assert!(kind.key().starts_with(key));
        } else {
            assert_eq!(kind.key(), key);
        }
        assert_eq!(kind.sandboxed(), sandboxed);
        assert_eq!(kind.copies_worktree(), copied);
        assert_eq!(kind.service_kind(), service);
        assert_eq!(kind.confines_egress(), !sandboxed);
        assert_eq!(placement::sandboxed(Some(&good)), sandboxed);
        // Only a Microsandbox guest is served the delegation tools from
        // this host; a local harness runs them itself.
        assert_eq!(kind.reaches_host(), name == "microsandbox", "{name}");
    }
}

#[test]
fn native_keys_bind_every_allocation_and_target_option() {
    let fixture = fixture();
    for (good, _) in samples(&fixture)
        .into_iter()
        .filter(|(good, _)| index(good) >= 4)
    {
        let original = good.kind().key();
        let mut changes = vec![
            (
                "image",
                json!(format!("example.test/other@sha256:{}", "b".repeat(64))),
            ),
            ("cpus", json!(2)),
            ("memory_mib", json!(2048)),
            ("network", json!("unrestricted")),
            ("pass_env", json!(["OPENAI_API_KEY"])),
        ];
        if index(&good) == 4 {
            changes.push(("runtime", json!("runc")));
        } else {
            changes.extend([
                ("runtime", json!("microvm")),
                ("context", json!("other-context")),
                ("namespace", json!("other-namespace")),
            ]);
        }
        for (field, value) in changes {
            let mut changed = serde_json::to_value(&good).unwrap();
            changed[field] = value;
            let changed: Provider = serde_json::from_value(changed).unwrap();
            assert_ne!(
                changed.kind().key(),
                original,
                "{} {field}",
                good.kind().name()
            );
        }
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
                assert_eq!(
                    lifecycle.max_paused,
                    if index(&good) >= 4 {
                        0
                    } else {
                        crate::snapshots::DEFAULT_MAX_PAUSED
                    }
                );
                assert_eq!(lifecycle.max_kept_secs, None, "{good:?}");
                // A recipe's machine has no checkpoints to keep.
                let snapshots = match index(&good) {
                    3..=5 => 0,
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
        max_kept_secs: Some(600),
        ..SandboxOptions::default()
    });
    let lifecycle = kept.kind().lifecycle().unwrap();
    assert_eq!(
        (
            lifecycle.keep,
            lifecycle.snapshots,
            lifecycle.max_paused,
            lifecycle.max_kept_secs
        ),
        (true, 7, 2, Some(600))
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
            1 | 2 | 4 | 5 => {
                assert_eq!(workdir, placement::WORKSPACE);
                assert_eq!(home, placement::HOME);
            }
            _ => {
                // hygiene-allow: tmp the recipe machine's default workdir is a product contract
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
            3 => assert_eq!(
                said.as_deref(),
                Some("its machine by-b-1 was already gone"),
                "no machine of that name was ever recorded"
            ),
            _ => assert!(said.expect("native recovery").contains("destroyed its")),
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
            3 => {
                assert!(opened.is_ok());
                assert_eq!(destroyed.as_deref(), Ok("machine by-b-1 was already gone"));
            }
            _ => {
                assert!(opened.is_ok());
                assert!(destroyed.unwrap().contains("sandbox by-b-1"));
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

/// Every variant says where the harness runs and what that does not give;
/// only a sandbox provider says "sandbox", and a private home changes the
/// words of a local run alone.
#[test]
fn each_variant_has_a_banner_of_its_own() {
    let (mode, local) = Provider::banner_of(None, false, false);
    assert_eq!(mode, "local");
    assert_eq!(
        local,
        "harnesses run as your user, in your own HOME; no sandbox"
    );
    assert_eq!(
        Provider::banner_of(Some(&Provider::Local), false, false).1,
        local
    );
    let (mode, isolated) = Provider::banner_of(None, true, false);
    assert_eq!(mode, "isolated");
    assert!(
        isolated.contains("private HOME") && isolated.contains("not a sandbox"),
        "{isolated}"
    );
    let (mode, served) = Provider::banner_of(None, false, true);
    assert_eq!(mode, "local");
    assert!(served.contains("the server's user"), "{served}");
    assert!(Provider::banner_of(None, true, true)
        .1
        .contains("the server's user with a private HOME"));

    let microsandbox = Provider::Microsandbox(SandboxOptions {
        image: "alpine:3.20".into(),
        ..SandboxOptions::default()
    });
    let (mode, what) = Provider::banner_of(Some(&microsandbox), false, false);
    assert_eq!(mode, "sandbox");
    assert!(what.contains("microVMs from alpine:3.20"), "{what}");
    assert_eq!(
        Provider::banner_of(Some(&microsandbox), true, false).1,
        what
    );
    assert!(Provider::banner_of(Some(&microsandbox), false, true)
        .1
        .contains("on the server, from alpine:3.20"));

    let substrate = Provider::Substrate(SubstrateOptions {
        template: "by".into(),
        ..SubstrateOptions::default()
    });
    let (mode, what) = Provider::banner_of(Some(&substrate), false, false);
    assert_eq!(mode, "sandbox");
    assert!(
        what.contains("Agent Substrate actors from template by"),
        "{what}"
    );
    assert!(Provider::banner_of(Some(&substrate), false, true)
        .1
        .contains("reached from the server"));

    let recipe = Provider::Recipe(RecipeOptions {
        name: "devbox".into(),
        ..RecipeOptions::default()
    });
    let (mode, what) = Provider::banner_of(Some(&recipe), false, false);
    assert_eq!(mode, "sandbox");
    assert!(what.contains("recipe devbox"), "{what}");
    assert!(Provider::banner_of(Some(&recipe), false, true)
        .1
        .contains("is not run by a server"));

    assert!(Provider::is_local(None) && Provider::is_local(Some(&Provider::Local)));
    assert!(!Provider::is_local(Some(&microsandbox)));
    assert!(!Provider::is_local(Some(&recipe)));
}

/// Links into the tool store need the store at the same path in the
/// harness's filesystem: a local harness has it, Microsandbox is given a
/// read-only mount of it, an Agent Sandbox mounts its bucket, and a
/// provider that copies the home in has none.
#[test]
fn only_a_provider_that_sees_the_store_at_its_path_is_given_links_to_it() {
    use crate::toolstore::ToolStore;
    let f = fixture();
    let store = branchyard_testkit::Scratch::new("provider-store");
    let sees = |provider: &Provider| of(Some(provider)).sees_store(&f.yard);
    let usable = samples(&f);
    let sandbox = |name: &str| {
        usable
            .iter()
            .map(|(ok, _)| ok)
            .find(|p| p.name() == name)
            .unwrap()
            .clone()
    };
    assert!(of(None).sees_store(&f.yard));
    for name in [
        "microsandbox",
        "substrate",
        "recipe",
        "container",
        "agent_sandbox",
    ] {
        assert!(!sees(&sandbox(name)), "{name} before a store is set");
    }

    // Where every ready skill is mounted (R2): a local harness and
    // Microsandbox read the tree in place whether or not there is a store;
    // the providers that copy the home in stay chosen-only until they link
    // one (an Agent Sandbox with a bucket), or until a fetch exists.
    let mounts = |provider: &Provider| of(Some(provider)).mounts_ready_skills(&f.yard);
    assert!(of(None).mounts_ready_skills(&f.yard));
    assert!(mounts(&sandbox("microsandbox")), "the home is a live mount");
    for name in ["substrate", "recipe", "container", "agent_sandbox"] {
        assert!(!mounts(&sandbox(name)), "{name} copies the home in");
    }

    f.yard.use_tool_store(ToolStore::new(store.path()));
    assert!(sees(&sandbox("microsandbox")));
    for name in ["substrate", "recipe", "container", "agent_sandbox"] {
        assert!(!sees(&sandbox(name)), "{name} without a bucket or a mount");
        assert!(!mounts(&sandbox(name)), "{name} with a store it cannot see");
    }

    // The mount is read-only, at the same path, and only for a store that
    // exists.
    let mut record = record(&sandbox("microsandbox"));
    let home = branchyard_testkit::Scratch::new("provider-home");
    record.home = Some(home.path().to_path_buf());
    let options = SandboxOptions {
        image: "i".into(),
        ..SandboxOptions::default()
    };
    let (spec, env) = super::microsandbox::spec(&f.yard, &record, &options).unwrap();
    // The tool tree is also at /tools, read-only, and the guest is told so.
    let tree = spec
        .mounts
        .iter()
        .find(|m| m.guest == std::path::Path::new("/tools"))
        .expect("the tool tree is mounted at /tools");
    assert_eq!(tree.host, home.path().join(".branchyard/tools"));
    assert!(!tree.writable);
    assert!(
        tree.host.is_dir(),
        "made now, for the mount to have a source"
    );
    assert_eq!(
        env.get(std::ffi::OsStr::new("BRANCHYARD_TOOLS")).unwrap(),
        "/tools"
    );
    let mount = spec
        .mounts
        .iter()
        .find(|m| m.guest == store.path())
        .expect("the store is mounted");
    assert_eq!(mount.host, store.path());
    assert!(!mount.writable);
    f.yard.use_tool_store(ToolStore::new(store.join("absent")));
    assert!(!sees(&sandbox("microsandbox")));
    let (spec, _) = super::microsandbox::spec(&f.yard, &record, &options).unwrap();
    assert!(spec.mounts.iter().all(|m| m.guest != store.join("absent")));

    // A bucket makes the Agent Sandbox's pod mount it at the store's path.
    f.yard.use_tool_store(ToolStore {
        root: store.path().to_path_buf(),
        bucket: Some("acme-tools/store".into()),
    });
    assert!(sees(&sandbox("agent_sandbox")));
    assert!(!sees(&sandbox("container")));
    assert!(!sees(&sandbox("substrate")));
}

/// A provider that controls the guest filesystem exposes the tool tree at
/// `/tools`; a local harness and the copies that cannot (a Substrate actor,
/// a recipe's machine) keep it under the home.
#[test]
fn the_providers_that_control_the_guest_filesystem_expose_the_tool_tree_at_a_root() {
    let f = fixture();
    let usable = samples(&f);
    let root = |name: &str| {
        let provider = usable
            .iter()
            .map(|(ok, _)| ok)
            .find(|p| p.name() == name)
            .unwrap();
        of(Some(provider)).tools_root()
    };
    for name in ["microsandbox", "container", "agent_sandbox"] {
        assert_eq!(root(name), Some("/tools"), "{name}");
    }
    for name in ["substrate", "recipe"] {
        assert_eq!(root(name), None, "{name}");
    }
    assert_eq!(of(None).tools_root(), None);
}

/// A `deny` Agent Sandbox may reach, by name, the worker's `egress_hosts`
/// (`[agent_sandbox]`) and the package mirrors a name policy can hold (a name
/// or `*.suffix` on 443), once each and sorted, and the gateway Service it
/// names; a mirror on another port or at an address is left out. The branch
/// carries none of it.
#[test]
fn an_agent_sandbox_may_reach_the_workers_hosts_and_mirrors_by_name() {
    let f = fixture();
    let text = format!(
        "version = 1\n[network]\nallow = []\nmirrors = [\"pypi.org\", \"*.pythonhosted.org\", \
         \"registry.npmjs.org:443\", \"mirror.internal:8080\", \"api.anthropic.com\"]\n\
         [agent_sandbox]\ncontext = \"c\"\nnamespace = \"n\"\nimage = \"example.test/r@sha256:{}\"\n\
         egress_hosts = [\"api.anthropic.com\", \"gateway.example.com\"]\n\
         gateway_service = {{ namespace = \"branchyard-system\", name = \"anvil-gateway\", port = 8931 }}\n",
        "a".repeat(64)
    );
    // No configuration, nothing to reach.
    assert!(super::agent_sandbox::yard_egress(&f.yard).is_empty());
    f.yard
        .configure(&crate::config::parse(&text).unwrap())
        .unwrap();
    let egress = super::agent_sandbox::yard_egress(&f.yard);
    assert_eq!(
        egress.hosts,
        [
            "*.pythonhosted.org",
            "api.anthropic.com",
            "gateway.example.com",
            "pypi.org",
            "registry.npmjs.org"
        ]
    );
    assert_eq!(egress.gateway.as_ref().unwrap().port, 8931);
    let options = AgentSandboxOptions {
        context: "c".into(),
        namespace: "n".into(),
        image: format!("example.test/r@sha256:{}", "a".repeat(64)),
        ..AgentSandboxOptions::default()
    };
    let config = options.config_with(None, egress, None, None);
    config.validate().unwrap();
    assert_eq!(config.fqdn_hosts().len(), 5);
}

/// Serves one bundle, `github`, and packages it as a `SKILL.md`.
#[derive(Debug)]
struct OneBundle;

impl crate::connectors::Packager for OneBundle {
    fn served(&self) -> Result<Vec<crate::connectors::Bundle>, String> {
        Ok(vec![crate::connectors::Bundle {
            id: "github".into(),
            path: "/bundles/github".into(),
            hash: "hash-github".into(),
        }])
    }

    fn package(&self, _: &crate::connectors::Bundle, out: &std::path::Path) -> Result<(), String> {
        std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
        std::fs::write(out.join("SKILL.md"), "# github\n").map_err(|e| e.to_string())
    }

    fn index(
        &self,
        _: &std::path::Path,
        _: &[crate::connectors::Bundle],
        out: &std::path::Path,
    ) -> Result<(), String> {
        std::fs::write(out, "# Connectors\n").map_err(|e| e.to_string())
    }
}

#[test]
fn each_provider_declares_where_it_can_run_the_credential_proxy() {
    use crate::connectors::CredentialSite::{Guest, Host, Sidecar};
    let fixture = fixture();
    let sites: Vec<_> = samples(&fixture)
        .iter()
        .map(|(good, _)| (good.name(), of(Some(good)).credential_site()))
        .collect();
    assert_eq!(
        sites,
        [
            ("local", Host),
            ("microsandbox", Guest),
            ("substrate", Sidecar),
            ("recipe", Host),
            ("container", Host),
            ("agent_sandbox", Sidecar),
        ]
    );
    assert_eq!(of(None).credential_site(), Host);
}

#[test]
fn a_sandbox_that_holds_its_own_credentials_gives_the_harness_loopback_and_no_token() {
    use crate::connectors::{
        credential, sandbox_proxy::IdentityKind, CredentialSite, GrantEntry, ENV_GATEWAY_AUTH,
        ENV_GATEWAY_TOKEN_FILE, ENV_GATEWAY_URL, TOKEN_FILE,
    };
    const UPSTREAM: &str = "http://192.168.127.1:8931/mcp";
    let fixture = fixture();
    for (provider, _) in samples(&fixture) {
        let site = of(Some(&provider)).credential_site();
        for server in [None, Some("https://by.example")] {
            let home = tempfile::Builder::new()
                .prefix("by-credential-home-")
                .tempdir()
                .unwrap();
            let mut gateway = crate::connectors::Gateway::local(
                &fixture.yard,
                "http://127.0.0.1:9/mcp",
                Arc::new(OneBundle),
            )
            .unwrap();
            gateway.sandbox_url = Some(UPSTREAM.into());
            gateway.credential_proxy = crate::connectors::CredentialProxy {
                server: server.map(str::to_owned),
                audience: "https://by.example".into(),
                ..crate::connectors::CredentialProxy::default()
            };
            fixture.yard.use_connectors(gateway);
            let mut record = record(&provider);
            record.home = Some(home.path().to_path_buf());
            record.provision = Some(crate::Provisioning {
                connectors: vec![GrantEntry::parse("github:read").unwrap()],
                ..crate::Provisioning::default()
            });
            let scopes = crate::access::TokenScopes::default();
            let sources = crate::manifest::Sources {
                repository: &crate::skills::ForTurn::Nothing,
                user: None,
            };
            let manifest =
                crate::manifest::settle(&fixture.yard, &record, &scopes, Some(0), false, sources);
            let prepared =
                crate::connectors::prepare(&fixture.yard, &record, None, &scopes, Some(&manifest))
                    .unwrap()
                    .expect("a granted turn");
            let named = |name: &str| prepared.env.iter().find(|v| v.name == name);
            let held_here = server.is_none() || site == CredentialSite::Host;
            let what = format!("{} with server {server:?}", provider.name());
            if held_here {
                // The host's proxy holds the token, as before.
                assert!(prepared.sandbox.is_none(), "{what}");
                assert!(named(ENV_GATEWAY_TOKEN_FILE).is_some(), "{what}");
                assert!(
                    named(ENV_GATEWAY_AUTH).is_none(),
                    "{what}: token-file mode, Anvil's default"
                );
                assert!(home.path().join(TOKEN_FILE).is_file(), "{what}");
                continue;
            }
            let sandbox = prepared.sandbox.as_ref().expect(&what);
            assert_eq!(sandbox.site, site, "{what}");
            // The harness is given the proxy on its own loopback and no
            // token, and no file with one is in its home.
            let url = named(ENV_GATEWAY_URL).expect(&what);
            assert_eq!(url.value, "http://127.0.0.1:18900/mcp", "{what}");
            // Anvil's clients are told to send no token (contract v2 item 6).
            let auth = named(ENV_GATEWAY_AUTH).expect(&what);
            assert_eq!(auth.value, "proxy", "{what}");
            assert_eq!(prepared.env.len(), 2, "{what}: {:?}", prepared.env);
            assert!(!home.path().join(TOKEN_FILE).exists(), "{what}");
            assert!(!prepared.files.iter().any(|f| f == TOKEN_FILE), "{what}");
            assert_eq!(prepared.shared_token(), None, "{what}");
            // The proxy reaches the gateway as the sandbox does, and the
            // server it fetches from is allowed through the sandbox's
            // network policy.
            assert_eq!(prepared.gateway_url, UPSTREAM, "{what}");
            assert_eq!(
                prepared.reach,
                ["https://by.example", "http://127.0.0.1:18900/mcp"],
                "{what}"
            );
            assert_eq!(sandbox.config.upstream, UPSTREAM, "{what}");
            assert_eq!(sandbox.config.manifest, manifest.digest(), "{what}");
            let settings = home.path().join(credential::PROXY_CONFIG_FILE);
            match site {
                CredentialSite::Guest => {
                    // A secret for the guest's process, never in the home.
                    let (secret, kept) = &sandbox.secret;
                    assert_eq!(sandbox.config.identity, IdentityKind::Secret, "{what}");
                    assert!(!settings.exists(), "{what}");
                    assert_eq!(
                        (kept.manifest.as_str(), kept.turn),
                        (manifest.digest().as_str(), 1)
                    );
                    assert!(!format!("{kept:?}").contains(secret.as_str()), "{what}");
                    let mut files = String::new();
                    for entry in walk(home.path()) {
                        // Binary files cannot hold a text secret.
                        if let Ok(text) = std::fs::read_to_string(entry) {
                            files.push_str(&text);
                        }
                    }
                    assert!(
                        !files.contains(secret.as_str()),
                        "{what}: the home holds it"
                    );
                }
                CredentialSite::Sidecar => {
                    // A sidecar proves the turn with the turn's secret, read
                    // from settings in the home, and as its workload.
                    let (secret, kept) = &sandbox.secret;
                    assert_eq!(
                        (kept.manifest.as_str(), kept.turn),
                        (manifest.digest().as_str(), 1)
                    );
                    assert!(!format!("{kept:?}").contains(secret.as_str()), "{what}");
                    assert_eq!(
                        sandbox.settings.as_deref(),
                        Some(settings.as_path()),
                        "{what}"
                    );
                    assert_eq!(
                        sandbox.config.identity,
                        IdentityKind::Workload {
                            audience: "https://by.example".into()
                        },
                        "{what}"
                    );
                    let text = std::fs::read_to_string(&settings).expect(&what);
                    let on_disk = credential::parse_config(&text).unwrap();
                    assert_eq!(on_disk.secret.as_deref(), Some(secret.as_str()), "{what}");
                    assert_eq!(
                        credential::parse_config(&text).unwrap().secret,
                        Some(secret.clone())
                    );
                    assert_eq!(
                        crate::connectors::sandbox_proxy::ProxyConfig {
                            secret: None,
                            ..on_disk
                        },
                        sandbox.config
                    );
                    // The file is the only place, and goes when the turn ends.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        let mode = std::fs::metadata(&settings).unwrap().permissions().mode();
                        assert_eq!(mode & 0o077, 0, "{what}: readable beyond its owner");
                    }

                    assert!(
                        prepared
                            .files
                            .iter()
                            .any(|f| f == credential::PROXY_CONFIG_FILE),
                        "{what}"
                    );
                    let mut prepared = prepared;
                    prepared.retire();
                    assert!(!settings.exists(), "{what}");
                }
                CredentialSite::Host => unreachable!("held on the host above"),
            }
        }
    }
}

/// Every file under `dir`, links not followed.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match std::fs::symlink_metadata(&path).unwrap().file_type() {
            t if t.is_dir() => found.extend(walk(&path)),
            t if t.is_file() => found.push(path),
            _ => {}
        }
    }
    found
}
