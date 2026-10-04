//! The vendored Orca sources still have the shape `src/lib.rs` follows
//! (vendor/orca, pinned in vendor.lock.json; see patches/ports.json). When a
//! re-pin changes one of these, read the upstream change and port it.

#![allow(clippy::panic)] // tests: a panic is the failure report
use std::fs;

fn vendored(path: &str) -> String {
    let full = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/orca/").to_owned() + path;
    fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

#[test]
fn orcas_recipe_contract_is_what_the_port_follows() {
    let runner = vendored("src/shared/ephemeral-vm-recipe-process.ts");
    for mode in ["'create'", "'suspend'", "'resume'", "'destroy'"] {
        assert!(runner.contains(mode), "{mode}");
    }
    assert!(runner.contains("DEFAULT_MAX_CAPTURE_BYTES = 1024 * 1024"));
    assert!(runner.contains("shell: true"));
    assert!(runner.contains("process.kill(-child.pid, signal)"));
    let results = vendored("src/shared/ephemeral-vm-recipes.ts");
    assert!(results.contains("'Recipe stdout must be one JSON object.'"));
    assert!(results.contains("schemaVersion: z.literal(1)"));
    assert!(results.contains("type: z.literal('ssh')"));
    assert!(results.contains("'Recipe result projectRoot must be an absolute runtime path.'"));
    let payload = vendored("src/shared/ephemeral-vm-recipe-lifecycle-payload.ts");
    assert!(payload.contains("recipeResult: args.recipeResult"));
    let runner = vendored("src/shared/ephemeral-vm-recipe-runner.ts");
    assert!(runner.contains("stdin: `${JSON.stringify(payload)}\\n`"));
    let doctor = vendored("src/shared/ephemeral-vm-recipe-doctor.ts");
    for id in [
        "'recipe.create'",
        "'recipe.destroy'",
        "'recipe.suspend_resume_pairing'",
        "fsConstants.X_OK",
    ] {
        assert!(doctor.contains(id), "{id}");
    }
    let yaml = vendored("src/shared/orca-yaml.ts");
    assert!(yaml.contains("ORCA_VM_RECIPE_ID_PATTERN = /^[a-z0-9][a-z0-9._-]{0,63}$/"));
    assert!(yaml.contains("destroyValue === 'none'"));
    let destroy = vendored("src/shared/ephemeral-vm-recipe-destroy-result.ts");
    assert!(destroy.contains("Destroy exited with code"));
}
