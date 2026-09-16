use branchyard_protocol::*;
use serde_json::{json, Value};

fn example() -> Value {
    serde_json::from_str(include_str!("../../../examples/commands/create-task.json")).unwrap()
}
fn command(value: Value) -> Command {
    serde_json::from_value(value).unwrap()
}

#[test]
fn rejects_unknown_fields_and_protocol_versions() {
    let mut v = example();
    v["schema"] = json!("branchyard/v2");
    assert!(serde_json::from_value::<Command>(v).is_err());
    let mut v = example();
    v["action"]["spec"]["silent_permission_bypass"] = json!(true);
    assert!(serde_json::from_value::<Command>(v).is_err());
    let mut v = example();
    v["action"]["op"] = json!("run_shell");
    assert!(serde_json::from_value::<Command>(v).is_err());
}
#[test]
fn normalization_is_independent_of_input_whitespace() {
    let c = command(example());
    let pretty = serde_json::to_string_pretty(&c).unwrap();
    let decoded: Command = serde_json::from_str(&pretty).unwrap();
    assert_eq!(c.fingerprint(), decoded.fingerprint());
    let mut different = example();
    different["action"]["spec"]["goal"] = json!("different");
    assert_ne!(c.fingerprint(), command(different).fingerprint());
}
#[test]
fn rejects_nil_ids_floating_branches_zero_limits_and_duplicate_bindings() {
    let cases = [
        (
            "/operation_id",
            json!("00000000-0000-0000-0000-000000000000"),
        ),
        ("/action/spec/workspace/commit", json!("main")),
        ("/action/spec/limits/memory_mib", json!(0)),
        (
            "/action/spec/components",
            json!([
                {"component_id":"cache","access":"read_only"},
                {"component_id":"cache","access":"exclusive_write"}
            ]),
        ),
    ];
    for (path, replacement) in cases {
        let mut v = example();
        *v.pointer_mut(path).unwrap() = replacement;
        assert!(command(v).validate().is_err(), "{path}");
    }
}
#[test]
fn graph_delta_is_bounded_and_rejects_duplicate_children() {
    let mut v: Value =
        serde_json::from_str(include_str!("../../../examples/commands/spawn-child.json")).unwrap();
    let edit = v["action"]["edits"][0].clone();
    v["action"]["edits"] = json!([edit.clone(), edit.clone()]);
    assert!(command(v.clone()).validate().is_err());
    v["action"]["edits"] = json!(vec![edit; 65]);
    assert!(command(v.clone()).validate().is_err());
    v["action"]["edits"] = json!([]);
    assert!(command(v).validate().is_err());
}
#[test]
fn examples_roundtrip_and_generated_schema_describes_mutations() {
    for input in [
        include_str!("../../../examples/commands/create-task.json"),
        include_str!("../../../examples/commands/spawn-child.json"),
        include_str!("../../../examples/commands/cancel-task.json"),
    ] {
        let c: Command = serde_json::from_str(input).unwrap();
        let bytes = c.canonical_bytes().unwrap();
        assert_eq!(serde_json::from_slice::<Command>(&bytes).unwrap(), c);
    }
    let schema = schema();
    let action = schema["command"]["$defs"]["Action"].to_string();
    for name in ["create_task", "apply_graph", "cancel_task"] {
        assert!(action.contains(name));
    }
}
