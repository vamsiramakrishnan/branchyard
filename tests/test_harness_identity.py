#!/usr/bin/env python3
"""A harness is named in several lists, and they must agree: the catalog (catalog/harnesses.toml),
the driver profiles (crates/branchyard-harness/src/profiles.rs), the hook install manifests
(plugins/branchyard/install/*.toml), the `--harness` dialects (crates/branchyard-cli/src/hook_cmd.rs)
and the sections of docs/harness-hooks.md. This test reads each and fails with the harness and the
surface that is missing, so a contributor learns what to add (docs/adding-a-harness.md) from CI, not
from a reviewer. The few places a list may legitimately differ are recorded below, each with its
reason, and a record that no longer applies fails too. No cargo, no network."""

import re
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# A profile whose executable the catalog does not list: an ACP adapter is its own package, not the CLI.
ADAPTER_EXECUTABLES = {
    "claude-code-acp": "claude-agent-acp",  # npm @agentclientprotocol/claude-agent-acp, pinned in docs/deploy.md
    "codex-acp": "codex-acp",  # a separate adapter; Branchyard has no install route for it yet
}

# Drivable harnesses with no hook install, and why. docs/harness-hooks.md "Other harnesses" covers both.
NO_MANIFEST = {
    "goose": "documents plugin hooks but no way for one to add context to a session",
    "kimi-cli": "no hook surface has been checked; it is driven, and the AGENTS.md line covers it",
}

# Harnesses with a hook install and no driver: known, not driven (docs/compatibility.md).
INFORM_ONLY = {"droid", "kiro"}

# Plugins written in code (Pi, Oh My Pi, Amp, OpenCode) call `by hook --harness generic`, so they have
# no dialect of their own and no recorded payloads.
GENERIC_DIALECT = {"pi", "oh-my-pi", "amp", "opencode"}

# The heading in docs/harness-hooks.md when it is not the catalog's name, or (None) the harness whose
# section lives in another document, named with the heading to find there.
SECTION = {
    "cursor": "Cursor",
    "kiro": "Kiro",
    "claude-code": ("docs/harness-integration.md", "Claude Code"),
}


def read(relative):
    return (ROOT / relative).read_text()


def catalog():
    """{id: entry} from catalog/harnesses.toml."""
    return {entry["id"]: entry for entry in tomllib.loads(read("catalog/harnesses.toml"))["harness"]}


def profiles():
    """[(profile id, harness id, executable)] read from profiles.rs: `acp(id, harness, &[command...])`
    calls and `Profile { id, harness, command, .. }` literals in PROFILES."""
    source = read("crates/branchyard-harness/src/profiles.rs")
    body = source[source.index("pub const PROFILES") : source.index("pub const NOT_IMPLEMENTED")]
    found = {}
    for match in re.finditer(r'\bacp\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*&\[([^\]]*)\]', body):
        found[match.group(1)] = (match.group(2), re.findall(r'"([^"]*)"', match.group(3))[0])
    for match in re.finditer(r'id:\s*"([^"]+)",\s*harness:\s*"([^"]+)",[^{}]*?command:\s*&\[([^\]]*)\]', body):
        found[match.group(1)] = (match.group(2), re.findall(r'"([^"]*)"', match.group(3))[0])
    return sorted((profile, harness, command) for profile, (harness, command) in found.items())


def manifests():
    """{harness id: manifest} from plugins/branchyard/install/*.toml."""
    found = {}
    for path in sorted((ROOT / "plugins/branchyard/install").glob("*.toml")):
        manifest = tomllib.loads(path.read_text())
        manifest["_stem"] = path.stem
        found[manifest["harness"]] = manifest
    return found


def drivable():
    return {harness for _, harness, _ in profiles()}


class Profiles(unittest.TestCase):
    def test_the_profiles_read_here_are_the_ones_the_compatibility_table_lists(self):
        # docs/compatibility.md is generated from the registry; if this parse misses a profile (a new
        # way of writing one), the other tests would pass over it without saying so.
        table = read("docs/compatibility.md").split("## Install, sign-in and key")[0]
        listed = set(re.findall(r"^\| [^|]*\(`[a-z0-9-]+`\) \| `([a-z0-9-]+)` \|", table, re.M))
        self.assertEqual(sorted(listed), [profile for profile, _, _ in profiles()])

    def test_every_profile_names_a_catalog_harness(self):
        known = catalog()
        for profile, harness, _ in profiles():
            self.assertIn(harness, known, f"{profile}: harness {harness!r} is not in catalog/harnesses.toml")

    def test_every_profile_launches_an_executable_the_catalog_lists_for_its_harness(self):
        # If it did not, `by harnesses` could report a harness missing that `by run` would start (cursor:
        # the profile ran `agent`, the catalog knew only `cursor-agent`). Fix catalog.rs `curated`, not the TOML.
        known = catalog()
        for profile, harness, executable in profiles():
            if profile in ADAPTER_EXECUTABLES:
                continue
            self.assertIn(
                executable,
                known[harness]["binaries"],
                f"{profile} runs `{executable}`, which is not in the catalog's binaries for {harness} "
                f"({known[harness]['binaries']}); add it in `curated` in crates/branchyard-controls/src/catalog.rs "
                "and regenerate with BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog",
            )

    def test_the_adapter_exceptions_are_still_needed(self):
        known = catalog()
        by_profile = {profile: (harness, executable) for profile, harness, executable in profiles()}
        for profile, executable in ADAPTER_EXECUTABLES.items():
            self.assertIn(profile, by_profile, f"{profile} is no longer a profile: drop it from ADAPTER_EXECUTABLES")
            harness, launched = by_profile[profile]
            self.assertEqual(launched, executable, profile)
            self.assertNotIn(executable, known[harness]["binaries"], f"{profile}: the catalog lists it now")


class Manifests(unittest.TestCase):
    def test_every_manifest_is_named_for_a_catalog_harness(self):
        known = catalog()
        for harness, manifest in manifests().items():
            self.assertEqual(manifest["_stem"], harness, f"plugins/branchyard/install/{manifest['_stem']}.toml")
            self.assertIn(harness, known, f"install manifest {harness}: not in catalog/harnesses.toml")

    def test_a_manifest_detects_every_executable_the_catalog_and_the_profile_know(self):
        known = catalog()
        launched = {}
        for profile, harness, executable in profiles():
            if profile not in ADAPTER_EXECUTABLES:
                launched.setdefault(harness, set()).add(executable)
        for harness, manifest in manifests().items():
            wanted = set(known[harness]["binaries"]) | launched.get(harness, set())
            missing = wanted - set(manifest["binaries"])
            self.assertFalse(missing, f"install manifest {harness} does not detect {sorted(missing)}")

    def test_every_drivable_harness_has_a_manifest_or_a_recorded_reason(self):
        have = set(manifests())
        for harness in sorted(drivable() - have):
            self.assertIn(
                harness,
                NO_MANIFEST,
                f"{harness} is drivable and has no plugins/branchyard/install/{harness}.toml: add one, or record "
                "why not in NO_MANIFEST",
            )
        for harness, reason in NO_MANIFEST.items():
            self.assertIn(harness, drivable(), f"{harness} is not drivable: drop it from NO_MANIFEST ({reason})")
            self.assertNotIn(harness, have, f"{harness} has a manifest now: drop it from NO_MANIFEST")

    def test_a_manifest_without_a_driver_is_a_recorded_one(self):
        self.assertEqual(set(manifests()) - drivable(), INFORM_ONLY)

    def test_every_manifest_has_its_plugin_files(self):
        # Under plugins/branchyard/harnesses/<id>/, or shared: Oh My Pi installs Pi's extension.
        plugin = ROOT / "plugins/branchyard"
        for harness, manifest in manifests().items():
            self.assertTrue(manifest.get("file"), f"install manifest {harness} installs no file")
            for entry in manifest["file"]:
                self.assertTrue((plugin / entry["from"]).exists(), f"{harness}: {entry['from']} does not exist")


class Documents(unittest.TestCase):
    def test_every_manifest_has_a_section_in_the_hooks_document(self):
        known = catalog()
        sections = set(re.findall(r"^## (.+)$", read("docs/harness-hooks.md"), re.M))
        for harness in manifests():
            wanted = SECTION.get(harness, known[harness]["name"])
            if isinstance(wanted, tuple):
                document, heading = wanted
                headings = set(re.findall(r"^#+ (.+)$", read(document), re.M))
                self.assertIn(heading, headings, f"{harness}: no `{heading}` heading in {document}")
                continue
            self.assertIn(
                wanted, sections, f"{harness}: docs/harness-hooks.md has no `## {wanted}` section (see SECTION)"
            )

    def test_every_manifest_has_a_hook_dialect_with_recorded_payloads_or_uses_the_generic_one(self):
        dialects = set(re.findall(r'Dialect\s*\{\s*id:\s*"([^"]+)"', read("crates/branchyard-cli/src/hook_cmd.rs")))
        fixtures = {p.name for p in (ROOT / "crates/branchyard-cli/tests/fixtures/hooks").iterdir() if p.is_dir()}
        self.assertIn("generic", dialects)
        for harness in manifests():
            if harness in GENERIC_DIALECT:
                self.assertNotIn(harness, dialects, f"{harness} has a dialect now: drop it from GENERIC_DIALECT")
                continue
            self.assertIn(harness, dialects, f"{harness}: no `Dialect {{ id: {harness!r} }}` row in hook_cmd.rs")
            self.assertIn(harness, fixtures, f"{harness}: no crates/branchyard-cli/tests/fixtures/hooks/{harness}/")


if __name__ == "__main__":
    unittest.main()
