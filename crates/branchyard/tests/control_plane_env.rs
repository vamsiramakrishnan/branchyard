//! A control-plane backend's key read from a variable of the process that
//! runs the turn never reaches the harness, whose environment is otherwise
//! inherited; a plain backend's key variable reaches it as before. A file
//! of its own: it sets variables of this test process, once.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod aiplex_kit;
mod common;

use std::sync::Once;

use aiplex_kit::*;
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
    let (events, read) = run(&f, "env", None, &plan);
    let said = text(&events);
    // Neither the token's variable nor its value is in the harness's
    // environment; the plain backend's variable is, as it always was.
    assert!(
        said.contains("real keys in env: BY_TEST_PLAIN_KEY\n"),
        "{said}"
    );
    assert!(
        said.contains("test variables in env: BY_TEST_PLAIN_KEY,BY_TEST_VISIBLE"),
        "{said}"
    );
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
