//! A control plane's token is hidden by its value, not only by the name the
//! configuration gives it: a variable of the process that holds the same
//! value (AIPlex's own `aiplex dev env` exports the token as the SDKs' key
//! variables too) reaches neither the harness nor a setup command, and a
//! plain backend whose key holds it is never sent it. A file of its own: it
//! sets variables of this test process, once.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod aiplex_kit;
mod common;

use std::sync::Once;

use aiplex_kit::*;
use branchyard::{Activity, SecretSource, WorkspaceSpec};
use common::{text, Fixture};
use serde_json::json;

/// The token a backend reads from `BY_TEST_AIPLEX_TOKEN`.
const ENV_TOKEN: &str = "aiplex-real-copied-token-9";
/// Another secret, in a variable `aiplex dev env` could also have set.
const OTHER: &str = "sk-real-other-value-9";
const HONEST_KEY: &str = "sk-real-honest-key-9";
const AIPLEX_URL: &str = "http://127.0.0.1:1/ap/ide";

static VARIABLES: Once = Once::new();

fn variables() {
    VARIABLES.call_once(|| {
        std::env::set_var("BY_TEST_AIPLEX_TOKEN", ENV_TOKEN);
        // As `eval "$(aiplex dev env)"` leaves a shell: the token again
        // under the SDKs' key variables, and their base URLs.
        for copy in ["GEMINI_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
            std::env::set_var(copy, ENV_TOKEN);
        }
        std::env::set_var("GOOGLE_GEMINI_BASE_URL", AIPLEX_URL);
        // Inside a value, and another value altogether.
        std::env::set_var("BY_TEST_COPY_IN_A_HEADER", format!("Bearer {ENV_TOKEN}"));
        std::env::set_var("GOOGLE_API_KEY", OTHER);
        // A copy of a token that is read from a file (the kit's `aiplex`),
        // which a plain backend names as its key; and a key of its own.
        std::env::set_var("BY_TEST_PLAIN_COPY", AIPLEX_TOKEN);
        std::env::set_var("BY_TEST_HONEST_KEY", HONEST_KEY);
    });
}

/// The names the harness printed after `prefix`.
fn listed(said: &str, prefix: &str) -> Vec<String> {
    said.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .map(|names| names.split(',').map(str::to_owned).collect())
        .unwrap_or_default()
}

#[test]
fn a_copy_of_the_token_under_another_name_reaches_no_child() {
    variables();
    let f = Fixture::new();
    let access = aiplex("one");
    let at = format!("{}/ap/ide", access.url());
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic", "url": at,
                    "key": "BY_TEST_AIPLEX_TOKEN", "control_plane": "aiplex"},
                "aiplex-openai": {"api": "openai", "url": at,
                    "key": "BY_TEST_AIPLEX_TOKEN", "control_plane": "aiplex"}
            },
            "routes": [{"model": "claude-*", "backends": ["aiplex-anthropic"]}]
        }),
    );
    let plan = json!([
        call(
            "anthropic",
            "anthropic",
            "/v1/messages",
            "claude-ok-json",
            false
        ),
        call("openai", "openai", "/chat/completions", "o3-ok", false),
    ]);
    let setup = f.dir.join("setup.env");
    let shell = f.dir.join("shell.seen");
    let (events, read) = run_with(&f, "copies", None, &plan, |options| {
        options.workspace = Some(WorkspaceSpec {
            setup: vec![format!("env > '{}'", setup.display())],
            shell_setup: Some(format!(
                "printf '%s|%s|%s|%s|%s|%s' \"${{BY_TEST_AIPLEX_TOKEN-unset}}\" \
                 \"${{GEMINI_API_KEY-unset}}\" \"${{OPENAI_API_KEY-unset}}\" \
                 \"${{BY_TEST_COPY_IN_A_HEADER-unset}}\" \"${{GOOGLE_API_KEY-unset}}\" \
                 \"${{GOOGLE_GEMINI_BASE_URL-unset}}\" > '{}'",
                shell.display()
            )),
            ..WorkspaceSpec::default()
        });
    });
    // The snippet whose environment is replayed: no copy, the other
    // value and the URL as they were.
    let seen = std::fs::read_to_string(&shell).unwrap();
    assert_eq!(
        seen,
        format!("unset|unset|unset|unset|{OTHER}|{AIPLEX_URL}"),
        "shell_setup saw: {seen}"
    );
    // A setup command: no variable holds the value, under any name.
    let env = std::fs::read_to_string(&setup)
        .unwrap_or_else(|e| format!("the setup command wrote nothing: {e}"));
    assert!(!env.contains(ENV_TOKEN), "{env}");
    for copy in [
        "BY_TEST_AIPLEX_TOKEN=",
        "GEMINI_API_KEY=",
        "BY_TEST_COPY_IN_A_HEADER=",
    ] {
        assert!(!env.contains(copy), "{copy} in {env}");
    }
    assert!(env.contains(&format!("GOOGLE_API_KEY={OTHER}")), "{env}");
    assert!(
        env.contains(&format!("GOOGLE_GEMINI_BASE_URL={AIPLEX_URL}")),
        "{env}"
    );
    // The harness: the same, and the gateway's own variables are still its
    // turn's token, since both calls were let through.
    let said = text(&events);
    assert!(!said.contains(ENV_TOKEN), "{said}");
    let real = listed(&said, "real keys in env: ");
    assert!(real.iter().any(|n| n == "GOOGLE_API_KEY"), "{said}");
    for copy in [
        "BY_TEST_AIPLEX_TOKEN",
        "GEMINI_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "BY_TEST_COPY_IN_A_HEADER",
    ] {
        assert!(!real.iter().any(|n| n == copy), "{copy}: {said}");
    }
    let tests = listed(&said, "test variables in env: ");
    assert!(!tests.iter().any(|n| n == "BY_TEST_AIPLEX_TOKEN"), "{said}");
    assert!(
        !tests.iter().any(|n| n == "BY_TEST_COPY_IN_A_HEADER"),
        "{said}"
    );
    for label in ["anthropic", "openai"] {
        assert_eq!(read[label]["status"], 200, "{label}: {}", read[label]);
    }
    let bearer = format!("Bearer {ENV_TOKEN}");
    let heard = access.requests();
    assert_eq!(heard.len(), 2, "{heard:?}");
    for request in &heard {
        assert_eq!(request.header_values("authorization"), [bearer.as_str()]);
    }
    access.finish();
}

#[test]
fn a_plain_backend_whose_key_holds_a_control_planes_token_is_never_sent_it() {
    variables();
    let f = Fixture::new();
    let (access, direct, honest) = (aiplex("one"), aiplex("direct"), aiplex("honest"));
    // The control plane's token is read from a file here (the kit's
    // `aiplex`), and a variable of the process holds a copy of it, which a
    // plain backend names as its key.
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic",
                    "url": format!("{}/ap/ide", access.url()),
                    "key": "aiplex", "control_plane": "aiplex"},
                "direct": {"api": "anthropic", "url": direct.url(),
                    "key": "BY_TEST_PLAIN_COPY"},
                "honest": {"api": "anthropic", "url": honest.url(),
                    "key": "BY_TEST_HONEST_KEY"}
            },
            "routes": [
                {"model": "claude-*", "backends": ["aiplex-anthropic"]},
                {"model": "direct-only-*", "backends": ["direct"]},
                {"model": "direct-then-*", "backends": ["direct"], "fallbacks": ["honest"]}
            ]
        }),
    );
    let plan = json!([
        call("only", "anthropic", "/v1/messages", "direct-only-ok", false),
        call("then", "anthropic", "/v1/messages", "direct-then-ok", false),
        call(
            "aiplex",
            "anthropic",
            "/v1/messages",
            "claude-ok-json",
            false
        ),
    ]);
    // The workspace's own snippet reads the token from its file and
    // exports it under a name of its own, beside another variable.
    let (events, read) = run_with(&f, "plain", None, &plan, |options| {
        options.workspace = Some(WorkspaceSpec {
            shell_setup: Some(format!(
                "export BY_TEST_REEXPORTED=\"$(cat '{}')\"; export BY_TEST_REEXPORTED_OTHER=kept",
                f.dir.join("aiplex").display()
            )),
            ..WorkspaceSpec::default()
        });
    });
    let why = "direct: its key holds the token of the control-plane backend aiplex-anthropic, \
               which is sent only to that backend";
    // The provider the plain backend names heard nothing, whichever route.
    assert!(direct.requests().is_empty(), "{:?}", direct.requests());
    let calls = calls(&events);
    assert_eq!(read["only"]["status"], 502, "{}", read["only"]);
    let only = &calls["direct-only-ok"];
    assert_eq!((only.decision.as_str(), only.status), ("failed", 502));
    assert!(
        only.failed_over.iter().any(|tried| tried == why),
        "{:?} {:?}",
        only.failed_over,
        only.reason
    );
    // A route's next plain backend, with a key of its own, takes the call.
    assert_eq!(read["then"]["status"], 200, "{}", read["then"]);
    let then = &calls["direct-then-ok"];
    assert_eq!(then.backend.as_deref(), Some("honest"));
    assert!(
        then.failed_over.iter().any(|tried| tried == why),
        "{:?}",
        then.failed_over
    );
    assert_eq!(honest.requests().len(), 1);
    assert_eq!(
        honest.requests()[0].header_values("x-api-key"),
        [HONEST_KEY]
    );
    // AIPlex is still sent its token, and nothing shows its value.
    assert_eq!(read["aiplex"]["status"], 200, "{}", read["aiplex"]);
    let said = format!("{}{:?}{:?}", text(&events), read["only"], read["then"]);
    assert!(!said.contains(AIPLEX_TOKEN), "{said}");
    // The copy is a variable of the process too: hidden from the harness,
    // as a token read from a file is wherever a variable repeats it.
    let tests = listed(&text(&events), "test variables in env: ");
    assert!(
        !tests.iter().any(|n| n == "BY_TEST_PLAIN_COPY"),
        "{tests:?}"
    );
    assert!(tests.iter().any(|n| n == "BY_TEST_HONEST_KEY"), "{tests:?}");
    // Nor does the harness get what the snippet exported with the token in
    // it; the snippet's other variable is its own.
    assert!(
        !tests.iter().any(|n| n == "BY_TEST_REEXPORTED"),
        "{tests:?}"
    );
    assert!(
        tests.iter().any(|n| n == "BY_TEST_REEXPORTED_OTHER"),
        "{tests:?}"
    );
    access.finish();
    direct.finish();
    honest.finish();
}

#[test]
fn a_secret_that_holds_the_token_is_withheld_from_the_harness() {
    variables();
    let f = Fixture::new();
    let access = aiplex("one");
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic",
                    "url": format!("{}/ap/ide", access.url()),
                    "key": "BY_TEST_AIPLEX_TOKEN", "control_plane": "aiplex"}
            },
            "routes": [{"model": "claude-*", "backends": ["aiplex-anthropic"]}]
        }),
    );
    let plan = json!([call(
        "anthropic",
        "anthropic",
        "/v1/messages",
        "claude-ok-json",
        false
    )]);
    // A private home, whose harness is given the branch's secrets: one read
    // from a variable that holds the token (as GEMINI_API_KEY does after
    // `aiplex dev env`), one that holds another value.
    let (events, read) = run_with(&f, "secret", None, &plan, |options| {
        options.isolated = true;
        if let Some(provision) = options.provision.as_mut() {
            provision.secrets = vec![
                SecretSource::parse("GEMINI_API_KEY").unwrap(),
                SecretSource::parse("GOOGLE_API_KEY").unwrap(),
            ];
        }
    });
    let warnings: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.activity {
            Activity::Warning(warning) => Some(warning.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings,
        [
            "the secret GEMINI_API_KEY was not given to the harness: it holds the token of the \
          control-plane backend aiplex-anthropic, which no harness is given"
        ]
    );
    let said = text(&events);
    assert!(!said.contains(ENV_TOKEN), "{said}");
    // The other secret is the harness's, as it always was.
    let real = listed(&said, "real keys in env: ");
    assert!(real.iter().any(|n| n == "GOOGLE_API_KEY"), "{said}");
    assert!(!real.iter().any(|n| n == "GEMINI_API_KEY"), "{said}");
    // The turn ran, on the gateway.
    assert_eq!(read["anthropic"]["status"], 200, "{}", read["anthropic"]);
    access.finish();
}
