use super::*;
use branchyard::SandboxOptions;

/// Each way a harness can run says so, and says what that does not
/// give: the lines differ, and only a sandbox provider says "sandbox".
#[test]
fn each_mode_has_its_own_banner() {
    let local = banner(None, false, None);
    assert_eq!(
        local,
        "by: local mode: harnesses run as your user, in your own HOME; no sandbox"
    );

    let isolated = banner(None, true, None);
    assert!(isolated.starts_with("by: isolated mode:"), "{isolated}");
    assert!(isolated.contains("private HOME"), "{isolated}");
    assert!(isolated.contains("not a sandbox"), "{isolated}");
    assert!(isolated.contains("read your files"), "{isolated}");
    assert_ne!(isolated, local);

    let microsandbox = Provider::microsandbox(SandboxOptions {
        image: "alpine:3.20".into(),
        ..SandboxOptions::default()
    });
    let sandbox = banner(Some(&microsandbox), false, None);
    assert!(sandbox.starts_with("by: sandbox mode:"), "{sandbox}");
    assert!(sandbox.contains("microVMs from alpine:3.20"), "{sandbox}");
    // A private home asked for beside a sandbox changes nothing: the
    // sandbox is what the harness runs in.
    assert_eq!(banner(Some(&microsandbox), true, None), sandbox);
    // Substrate and recipe words: the providers' own tests.
}

/// Against a server the line names it, whose user the harness runs as,
/// and whether the server gives the branch a private home.
#[test]
fn a_server_banner_names_the_server_and_its_user() {
    let label = "http://127.0.0.1:8421 (app)";
    let local = banner(None, false, Some(label));
    assert_eq!(
        local,
        "by: remote mode on http://127.0.0.1:8421 (app): harnesses run as the server's user, \
         in the server's own HOME; no sandbox"
    );
    let isolated = banner(None, true, Some(label));
    assert!(
        isolated.contains("the server's user with a private HOME"),
        "{isolated}"
    );
    assert!(isolated.contains("not a sandbox"), "{isolated}");
    let microsandbox = Provider::microsandbox(SandboxOptions {
        image: "alpine:3.20".into(),
        ..SandboxOptions::default()
    });
    assert_eq!(
        banner(Some(&microsandbox), false, Some(label)),
        "by: remote mode on http://127.0.0.1:8421 (app): harnesses run in Microsandbox \
         microVMs on the server, from alpine:3.20"
    );
}

#[test]
fn steps_the_run_survived_get_one_line_in_show() {
    assert_eq!(survived_line(&[], "b"), None);
    assert_eq!(
        survived_line(&["destroy the sandbox: gone"], "hello").as_deref(),
        Some(
            "1 step failed and the run carried on, the first: destroy the sandbox: gone. \
             See: by log hello"
        )
    );
    let two = survived_line(&["a: x", "b: y"], "hello").unwrap();
    assert!(two.starts_with("2 steps failed and the run carried on, the first: a: x."));
    assert!(two.ends_with("See: by log hello"), "{two}");
}
