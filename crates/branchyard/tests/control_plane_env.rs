//! A control-plane backend's key read from a variable of the process that
//! runs the turn never reaches the harness, whose environment is otherwise
//! inherited, nor any other child of the engine (the repository's setup
//! commands); a plain backend's key variable reaches them as before. A file of its own: it sets variables of this test process, once.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod aiplex_kit;
mod common;

use std::sync::Once;

use aiplex_kit::*;
use branchyard::WorkspaceSpec;
use common::{text, Fixture};
use serde_json::json;

const ENV_TOKEN: &str = "aiplex-real-env-token-7";
const PLAIN_KEY: &str = "sk-real-plain-env-7";

static VARIABLES: Once = Once::new();

#[test]
fn a_control_planes_key_variable_never_reaches_the_harness() {
    VARIABLES.call_once(|| {
        std::env::set_var("BY_TEST_AIPLEX_TOKEN", ENV_TOKEN);
        std::env::set_var("BY_TEST_PLAIN_KEY", PLAIN_KEY);
    });
    let f = Fixture::new();
    let (access, direct) = (aiplex("one"), aiplex("direct"));
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic", "url": format!("{}/ap/ide", access.url()),
                    "key": "BY_TEST_AIPLEX_TOKEN", "control_plane": "aiplex"},
                "direct": {"api": "anthropic", "url": direct.url(), "key": "BY_TEST_PLAIN_KEY"}
            },
            "routes": [{"model": "claude-*", "backends": ["aiplex-anthropic"]},
                {"model": "direct-*", "backends": ["direct"]}]
        }),
    );
    let plan = json!([
        call(
            "aiplex",
            "anthropic",
            "/v1/messages",
            "claude-ok-json",
            false
        ),
        call("direct", "anthropic", "/v1/messages", "direct-ok", false),
    ]);
    // The repository's own code on this host: a setup command. (Its git
    // hooks do not run: Branchyard runs its own git with hooks off.)
    let setup = f.dir.join("setup.env");
    let (events, read) = run_with(&f, "env", None, &plan, |options| {
        options.workspace = Some(WorkspaceSpec {
            setup: vec![format!("env > '{}'", setup.display())],
            ..WorkspaceSpec::default()
        });
    });
    let env = std::fs::read_to_string(&setup)
        .unwrap_or_else(|e| format!("the setup command wrote nothing: {e}"));
    assert!(
        env.contains("BY_TEST_PLAIN_KEY="),
        "the setup command ran without the environment: {env}"
    );
    assert!(!env.contains("BY_TEST_AIPLEX_TOKEN"), "{env}");
    assert!(!env.contains(ENV_TOKEN), "{env}");
    let said = text(&events);
    // Neither the token's variable nor its value is in the harness's
    // environment; the plain backend's variable is, as it always was.
    // Name by name: the machine may set other variables of its own (CI sets
    // BY_TEST_POSTGRES_URL), which are not this test's to judge.
    let line = |prefix: &str| -> Vec<String> {
        said.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|names| names.split(',').map(str::to_owned).collect())
            .unwrap_or_default()
    };
    for names in [line("real keys in env: "), line("test variables in env: ")] {
        assert!(names.iter().any(|n| n == "BY_TEST_PLAIN_KEY"), "{said}");
        assert!(!names.iter().any(|n| n == "BY_TEST_AIPLEX_TOKEN"), "{said}");
    }
    assert!(!said.contains(ENV_TOKEN), "{said}");
    // And the gateway still read it for AIPlex.
    assert_eq!(read["aiplex"]["status"], 200, "{}", read["aiplex"]);
    let bearer = format!("Bearer {ENV_TOKEN}");
    assert_eq!(
        access.requests()[0].header("authorization"),
        Some(bearer.as_str())
    );
    assert_eq!(direct.requests()[0].header("x-api-key"), Some(PLAIN_KEY));
    access.finish();
    direct.finish();
}
