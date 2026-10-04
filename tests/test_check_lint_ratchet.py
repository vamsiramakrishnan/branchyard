#!/usr/bin/env python3
"""check_lint_ratchet.py fails on each regression it exists to stop."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("check", ROOT / "tools" / "check_lint_ratchet.py")
assert spec is not None and spec.loader is not None
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)

TABLE = (
    '[workspace.package]\nrust-version = "1.94"\n\n[workspace.lints.rust]\n'
    + "".join(f'{lint} = "deny"\n' for lint in check.RUST_DENIED)
    + "\n[workspace.lints.clippy]\n"
    + "".join(f'{lint} = "deny"\n' for lint in check.CLIPPY_DENIED)
)
MANIFEST = '[package]\nname = "a"\nrust-version.workspace = true\n\n[lints]\nworkspace = true\n'


def tree(files):
    root = Path(tempfile.mkdtemp())
    base = {
        "Cargo.toml": TABLE,
        "rust-toolchain.toml": '[toolchain]\nchannel = "1.94.0"\n',
        "crates/a/Cargo.toml": MANIFEST,
    }
    for rel, text in {**base, **files}.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return root


def problems(files, baseline, wide=()):
    root = tree(files)
    counts, found_wide, allow_problems = check.scan(root)
    state = {"allows": baseline, "file_wide": list(wide)}
    return allow_problems + check.manifest_problems(root) + check.check(counts, found_wide, state)


MARKED = "#[allow(clippy::unwrap_used)] // ratchet: a\nfn f() {}\n"


class Ratchet(unittest.TestCase):
    def test_at_baseline_passes(self):
        self.assertEqual(problems({"crates/a/src/lib.rs": MARKED}, {"a": 1}), [])

    def test_a_new_marker_fails(self):
        self.assertTrue(problems({"crates/a/src/lib.rs": MARKED + MARKED}, {"a": 1}))

    def test_a_fall_must_lower_the_baseline(self):
        found = problems({"crates/a/src/lib.rs": "fn f() {}\n"}, {"a": 1})
        self.assertTrue(found and "lower it" in found[0])

    def test_an_allow_without_a_marker_fails(self):
        files = {"crates/a/src/lib.rs": "#![allow(clippy::panic)]\n"}
        self.assertTrue(problems(files, {}))

    def test_a_multi_line_allow_is_found(self):
        files = {"crates/a/src/lib.rs": "#![allow(\n    clippy::expect_used,\n    clippy::panic\n)] // ratchet: a\n"}
        self.assertEqual(problems(files, {"a": 1}, wide=["crates/a/src/lib.rs"]), [])

    def test_tests_markers_are_for_test_code(self):
        line = "#![allow(clippy::unwrap_used)] // tests: a panic is the failure report\n"
        self.assertEqual(problems({"crates/a/tests/t.rs": line}, {}), [])
        self.assertTrue(problems({"crates/a/src/lib.rs": line}, {}))

    def test_an_unrelated_allow_is_left_alone(self):
        self.assertEqual(problems({"crates/a/src/lib.rs": "#![allow(dead_code)]\n"}, {}), [])

    def test_a_crate_without_the_lints_table_fails(self):
        files = {"crates/a/Cargo.toml": '[package]\nname = "a"\nrust-version.workspace = true\n'}
        self.assertTrue(problems(files, {}))

    def test_a_crate_without_the_rust_version_fails(self):
        files = {"crates/a/Cargo.toml": '[package]\nname = "a"\n\n[lints]\nworkspace = true\n'}
        self.assertTrue(problems(files, {}))

    def test_a_lint_left_out_of_the_table_fails(self):
        files = {"Cargo.toml": TABLE.replace('unwrap_used = "deny"\n', "")}
        self.assertTrue(problems(files, {}))

    def test_a_rust_version_that_differs_from_the_toolchain_fails(self):
        files = {"Cargo.toml": TABLE.replace("1.94", "1.80")}
        self.assertTrue(problems(files, {}))

    def bare(self, attribute):
        return problems({"crates/a/src/lib.rs": attribute + "\nfn f() {}\n"}, {})

    def test_expect_is_an_allow_too(self):
        self.assertTrue(self.bare("#[expect(clippy::unwrap_used)]"))

    def test_cfg_attr_allow_is_found(self):
        self.assertTrue(self.bare("#[cfg_attr(not(test), allow(clippy::unwrap_used))]"))
        self.assertEqual(self.bare("#[cfg_attr(not(test), allow(dead_code))]"), [])

    def test_a_group_covers_the_denied_lints(self):
        for group in (
            "clippy::all",
            "clippy::pedantic",
            "clippy::restriction",
            "clippy::complexity",
            "unused",
            "warnings",
        ):
            self.assertTrue(self.bare(f"#[allow({group})]"), group)

    def test_the_generated_module_is_the_one_exemption(self):
        line = "#[allow(clippy::all, missing_docs)]\nmod pb {}\n"
        self.assertEqual(problems({"crates/branchyard-substrate/src/lib.rs": line}, {}), [])
        self.assertTrue(problems({"crates/a/src/lib.rs": line}, {}))

    def test_unused_must_use_is_denied_and_checked(self):
        self.assertTrue(self.bare("#[allow(unused_must_use)]"))
        files = {"Cargo.toml": TABLE.replace('unused_must_use = "deny"\n', "")}
        self.assertTrue(problems(files, {}))

    def test_spacing_in_the_lint_path_does_not_hide_it(self):
        self.assertTrue(self.bare("#[allow(clippy :: panic)]"))
        self.assertTrue(self.bare("#[allow(\n    clippy\n    ::\n    unwrap_used,\n)]"))

    def test_a_bracket_in_a_reason_does_not_end_the_attribute(self):
        attribute = '#[allow(clippy::panic, reason = "see [the note] ]")] // ratchet: a'
        self.assertEqual(problems({"crates/a/src/lib.rs": attribute + "\nfn f() {}\n"}, {"a": 1}), [])
        self.assertTrue(self.bare('#[allow(reason = "x ] y", clippy::panic)]'))

    def test_an_inner_tests_marker_is_refused_in_a_non_test_file(self):
        line = "#![allow(clippy::unwrap_used)] // tests: a panic is the failure report\n#[cfg(test)]\nmod t {}\n"
        self.assertTrue(problems({"crates/a/src/lib.rs": line}, {}))
        outer = "#[allow(clippy::unwrap_used)] // tests: a panic is the failure report\n#[cfg(test)]\nmod t {}\n"
        self.assertEqual(problems({"crates/a/src/lib.rs": outer}, {}), [])

    def test_text_in_comments_and_strings_is_ignored(self):
        source = (
            "// #[allow(clippy::panic)]\n/* #[allow(clippy::panic)] */\n/// #[allow(clippy::panic)]\n"
            'const S: &str = "#[allow(clippy::panic)]";\nconst R: &str = r#"#[allow(clippy::panic)]"#;\n'
        )
        self.assertEqual(problems({"crates/a/src/lib.rs": source}, {}), [])

    def test_the_marker_must_name_the_files_own_crate(self):
        line = "#[allow(clippy::panic)] // ratchet: b\nfn f() {}\n"
        self.assertTrue(problems({"crates/a/src/lib.rs": line}, {"a": 1}))

    def test_a_new_file_wide_marker_fails_and_a_removed_one_must_be_dropped(self):
        wide = "#![allow(clippy::panic)] // ratchet: a\n"
        self.assertTrue(problems({"crates/a/src/lib.rs": wide}, {"a": 1}))
        self.assertEqual(problems({"crates/a/src/lib.rs": wide}, {"a": 1}, wide=["crates/a/src/lib.rs"]), [])
        self.assertTrue(problems({"crates/a/src/lib.rs": "fn f() {}\n"}, {}, wide=["crates/a/src/lib.rs"]))

    def test_the_testkit_may_only_be_a_dev_dependency(self):
        ok = MANIFEST + '\n[dev-dependencies]\nbranchyard-testkit = { path = "../branchyard-testkit" }\n'
        self.assertEqual(problems({"crates/a/Cargo.toml": ok}, {}), [])
        for table in ("dependencies", "build-dependencies", 'target."cfg(unix)".dependencies'):
            bad = MANIFEST + f'\n[{table}]\nbranchyard-testkit = {{ path = "../branchyard-testkit" }}\n'
            self.assertTrue(problems({"crates/a/Cargo.toml": bad}, {}), table)

    def test_the_repository_is_at_baseline(self):
        counts, wide, allow_problems = check.scan(ROOT)
        baseline = check.json.loads(check.BASELINE.read_text())
        self.assertEqual(allow_problems + check.manifest_problems(ROOT) + check.check(counts, wide, baseline), [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
