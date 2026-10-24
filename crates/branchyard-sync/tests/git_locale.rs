//! The git backend under a non-English locale: every git process it starts
//! runs in the C locale, and a lease conflict is still a precondition
//! failure, a missing object still not found. `git` on `PATH` is a wrapper
//! that logs each call's locale and, when the call is not in the C locale,
//! rewrites git's stderr as a localized git would, so a classification
//! that leaned on English text without forcing the locale fails here.
//! Alone in its binary because it sets the process's environment.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use branchyard_sync::store::git::GitStore;
use branchyard_sync::store::ObjectStore;
use branchyard_sync::Kind;

#[test]
fn a_lease_conflict_is_a_precondition_failure_under_any_locale() {
    // The real `git` on `PATH`.
    let real_git: PathBuf = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("locale.log");
    let wrapper = bin.join("git");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             echo \"LC_ALL=${{LC_ALL-unset}} LANGUAGE=${{LANGUAGE-unset}}\" >> '{log}'\n\
             [ \"$LC_ALL\" = C ] && exec '{git}' \"$@\"\n\
             err='{log}.$$'\n\
             '{git}' \"$@\" 2>\"$err\"; code=$?\n\
             sed -e 's/rejected/abgelehnt/g' -e 's/stale info/veraltet/g' \
                 -e 's/fetch first/zuerst holen/g' -e \"s/couldn't find remote ref/Referenz fehlt/g\" \
                 -e 's/unable to delete/kann nicht entfernen/g' \"$err\" >&2\n\
             rm -f \"$err\"\n\
             exit $code\n",
            log = log.display(),
            git = real_git.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    // A German user: every variable a localized git reads.
    std::env::set_var("PATH", path);
    for var in ["LANG", "LC_ALL", "LC_MESSAGES"] {
        std::env::set_var(var, "de_DE.UTF-8");
    }
    std::env::set_var("LANGUAGE", "de");

    branchyard_sync::testing::git(dir.path(), &["init", "--quiet", "--bare", "remote.git"]);
    let store = GitStore::open(
        &format!("file://{}", dir.path().join("remote.git").display()),
        Some(&dir.path().join("cache")),
    )
    .unwrap();

    let first = store.put_if_absent("k", b"one").unwrap();
    // A second create, and a write against a generation that is not the
    // remote's: someone else moved first.
    let again = store.put_if_absent("k", b"two").unwrap_err();
    assert!(again.is(Kind::Precondition), "{again}");
    let stale = store
        .put_if_match("k", b"two", &"0".repeat(40))
        .unwrap_err();
    assert!(stale.is(Kind::Precondition), "{stale}");
    let second = store.put_if_match("k", b"two", &first).unwrap();
    assert_eq!(store.get("k").unwrap().data, b"two");
    // The fallbacks that read git's messages: a missing object, a delete
    // of what is not there.
    let missing = store.get("absent").unwrap_err();
    assert!(missing.is(Kind::NotFound), "{missing}");
    store.delete_if_match("k", &second).unwrap();
    let gone = store.delete_if_match("k", &second).unwrap_err();
    assert!(gone.is(Kind::Precondition), "{gone}");

    // Every git the store started (not the test's own `init`) was in the
    // C locale with no LANGUAGE.
    let calls = std::fs::read_to_string(&log).unwrap();
    let store_calls: Vec<&str> = calls.lines().skip(1).collect();
    assert!(store_calls.len() > 5, "{calls}");
    assert!(
        store_calls
            .iter()
            .all(|line| *line == "LC_ALL=C LANGUAGE=unset"),
        "{calls}"
    );
}
