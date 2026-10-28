"""Exercise the distribution archives (`tools/package.py`) as delivered:
extracted outside the checkout, installed with the shipped installer, and
run with the shipped launcher / imported as the shipped module. See
`docs/distribution.md`.
"""

import importlib.util
import json
import os
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]

spec = importlib.util.spec_from_file_location("package", ROOT / "tools/package.py")
assert spec is not None and spec.loader is not None
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class ArchiveTests(unittest.TestCase):
    """`tools/package.py`'s archives, without touching the checkout."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_archives_are_reproducible_and_complete(self):
        first = package.build(self.root / "one")
        second = package.build(self.root / "two")
        self.assertEqual(len(first), 4)
        for a, b in zip(first, second, strict=True):
            self.assertEqual(a.name, b.name)
            self.assertEqual(a.read_bytes(), b.read_bytes(), f"{a.name} is not reproducible")
            with zipfile.ZipFile(a) as archive:
                root_name = archive.namelist()[0].split("/", 1)[0]
                manifest = json.loads(archive.read(f"{root_name}/MANIFEST.sha256.json"))
                self.assertEqual(
                    set(archive.namelist()),
                    {f"{root_name}/{name}" for name in manifest} | {f"{root_name}/MANIFEST.sha256.json"},
                )
                for name, digest in manifest.items():
                    import hashlib

                    self.assertEqual(hashlib.sha256(archive.read(f"{root_name}/{name}")).hexdigest(), digest)

    def test_plugin_archive_carries_every_harness_file_and_install_manifest(self):
        plugin = package.build(self.root / "dist")[0]
        with zipfile.ZipFile(plugin) as archive:
            names = set(archive.namelist())
            for sub in ["harnesses", "install"]:
                files = [p for p in (package.PLUGIN / sub).rglob("*") if p.is_file()]
                self.assertTrue(files, sub)
                for path in files:
                    name = "branchyard/" + path.relative_to(package.PLUGIN).as_posix()
                    self.assertIn(name, names)
                    self.assertEqual(archive.read(name), path.read_bytes(), name)
            # Every file a manifest installs is in the archive.
            for manifest in sorted((package.PLUGIN / "install").glob("*.toml")):
                data = tomllib.loads(manifest.read_text())
                self.assertEqual(data["harness"], manifest.stem)
                for entry in data.get("file", []):
                    source = "branchyard/" + entry["from"]
                    self.assertTrue(
                        source in names or any(n.startswith(source + "/") for n in names),
                        f"{manifest.name}: {entry['from']}",
                    )

    def test_executable_files_stay_executable(self):
        source = self.root / "src"
        (source / "bin").mkdir(parents=True)
        (source / "bin/hook.sh").write_text("#!/bin/sh\n")
        (source / "bin/hook.sh").chmod(0o755)
        (source / "plain.json").write_text("{}\n")
        files = package.inputs(source)
        package.archive(self.root / "a.zip", files, "x", frozenset(package.executables(source)))
        with zipfile.ZipFile(self.root / "a.zip") as archive:
            modes = {i.filename: i.external_attr >> 16 for i in archive.infolist()}
        self.assertEqual(modes["x/bin/hook.sh"], 0o100755)
        self.assertEqual(modes["x/plain.json"], 0o100644)

    def test_plugin_archive_bundles_the_same_skill_bytes_as_the_standalone_one(self):
        plugin, skill, setup_skill, _sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(plugin) as plugin_zip:
            for archive, source, name in [
                (skill, package.SKILL, "delegate"),
                (setup_skill, package.SETUP_SKILL, "setup"),
            ]:
                with zipfile.ZipFile(archive) as skill_zip:
                    inputs = package.inputs(source)
                    self.assertIn("SKILL.md", inputs)
                    for path, data in inputs.items():
                        self.assertEqual(plugin_zip.read(f"branchyard/skills/{name}/{path}"), data)
                        self.assertEqual(skill_zip.read(f"{name}/{path}"), data)
            # The slash commands and the hooks ship in the plugin.
            for name in ("commands/setup.md", "commands/status.md", "commands/wait.md", "hooks/hooks.json"):
                self.assertIn(f"branchyard/{name}", plugin_zip.namelist())

    def test_sdk_archive_matches_its_source(self):
        *_archives, sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(sdk) as sdk_zip:
            for path, data in package.inputs(package.SDK).items():
                self.assertEqual(sdk_zip.read(f"branchyard-sdk/{path}"), data)

    def test_manifests_must_match_the_workspace_version(self):
        original = (package.PLUGIN / ".codex-plugin/plugin.json").read_text()
        try:
            broken = json.loads(original)
            broken["version"] = "9.9.9"
            (package.PLUGIN / ".codex-plugin/plugin.json").write_text(json.dumps(broken))
            with self.assertRaises(ValueError):
                package.build(self.root / "broken")
        finally:
            (package.PLUGIN / ".codex-plugin/plugin.json").write_text(original)

    def test_the_marketplace_must_list_the_plugin_at_the_workspace_version(self):
        original = package.MARKETPLACE.read_text()
        try:
            for field, value in (("version", "9.9.9"), ("source", "./plugins/elsewhere")):
                broken = json.loads(original)
                broken["plugins"][0][field] = value
                package.MARKETPLACE.write_text(json.dumps(broken))
                with self.assertRaises(ValueError):
                    package.build(self.root / "broken")
        finally:
            package.MARKETPLACE.write_text(original)

    def test_the_sdk_version_must_match_the_workspace_version(self):
        with mock.patch.object(package, "sdk_version", return_value="9.9.9"), self.assertRaises(ValueError) as raised:
            package.build(self.root / "broken")
        self.assertIn("pyproject.toml", str(raised.exception))

    def test_build_leftovers_are_not_packaged(self):
        tree = self.root / "tree"
        for name in ["mod.py", "build/lib/mod.py", "mod.egg-info/PKG-INFO", "__pycache__/mod.cpython-312.pyc"]:
            (tree / name).parent.mkdir(parents=True, exist_ok=True)
            (tree / name).write_text("x")
        self.assertEqual(list(package.inputs(tree)), ["mod.py"])


class SdkProjectTests(unittest.TestCase):
    """`sdk/python/pyproject.toml`: what `pip install ./sdk/python` builds."""

    def setUp(self):
        self.project = tomllib.loads((package.SDK / "pyproject.toml").read_text())

    def test_it_names_every_module_at_the_workspace_version_with_no_dependencies(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]
        project = self.project["project"]
        self.assertEqual(project["name"], "branchyard")
        self.assertEqual(project["version"], workspace["version"])
        self.assertEqual(project["license"], {"text": workspace["license"]})
        self.assertEqual(project["dependencies"], [])
        # ruff.toml holds the modules to 3.9's syntax; the floor says the same.
        self.assertEqual(project["requires-python"], ">=3.9")
        self.assertEqual(self.project["build-system"]["build-backend"], "setuptools.build_meta")
        modules = sorted(path.stem for path in package.SDK.glob("*.py"))
        self.assertEqual(sorted(self.project["tool"]["setuptools"]["py-modules"]), modules)

    def test_it_has_a_readme_urls_and_classifiers_for_a_package_index(self):
        project = self.project["project"]
        self.assertEqual(project["readme"], "README.md")
        readme = (package.SDK / "README.md").read_text()
        # The README is read away from the checkout (an index shows it): its links are absolute.
        self.assertNotRegex(readme, r"\]\((?!https?://|#)")
        for needle in (
            "pip install ./sdk/python",
            "cargo install --locked --path crates/branchyard-cli",
            "Client.local",
        ):
            self.assertIn(needle, readme)
        self.assertEqual(sorted(project["urls"]), ["Documentation", "Homepage", "Issues", "Source"])
        self.assertTrue(all(url.startswith("https://github.com/") for url in project["urls"].values()))
        classifiers = project["classifiers"]
        self.assertIn("Programming Language :: Python :: 3.9", classifiers)
        self.assertNotIn("Typing :: Typed", classifiers, "a single-file module cannot carry py.typed")

    def test_setuptools_builds_it_from_the_extracted_archive(self):
        if importlib.util.find_spec("setuptools") is None:
            self.skipTest("setuptools is not installed; pip install ./sdk/python needs it, or network to fetch it")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            *_archives, sdk = package.build(root / "dist")
            with zipfile.ZipFile(sdk) as archive:
                archive.extractall(root / "extracted")
            # The source distribution, as pip builds it before the wheel:
            # setuptools reads the project and collects both modules. The
            # wheel step is left out: some distributions' patched setuptools
            # cannot build one on every Python they ship.
            built = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "import sys, setuptools.build_meta as b; print(b.build_sdist(sys.argv[1]))",
                    str(root / "sdist"),
                ],
                cwd=root / "extracted/branchyard-sdk",
                capture_output=True,
                text=True,
            )
            self.assertEqual(built.returncode, 0, built.stderr[-2000:])
            version = self.project["project"]["version"]
            name = built.stdout.strip().splitlines()[-1]
            self.assertEqual(name, f"branchyard-{version}.tar.gz")
            with tarfile.open(root / "sdist" / name) as sdist:
                sdist.extractall(root / "unpacked", filter="data")
            unpacked = root / "unpacked" / f"branchyard-{version}"
            for module in ["branchyard.py", "branchyard_client.py"]:
                self.assertEqual((unpacked / module).read_bytes(), (package.SDK / module).read_bytes())
            info = (unpacked / "PKG-INFO").read_text()
            self.assertIn(f"Version: {version}", info)
            self.assertIn("Requires-Python: >=3.9", info)
            # The README is the long description, and the URLs and classifiers are metadata.
            self.assertEqual((unpacked / "README.md").read_bytes(), (package.SDK / "README.md").read_bytes())
            self.assertIn("Description-Content-Type: text/markdown", info)
            self.assertIn("Project-URL: Documentation, https://github.com/", info)
            self.assertIn("Classifier: Programming Language :: Python :: 3.9", info)
            self.assertIn("# Branchyard for Python", info)
            # Both modules import from what was built, nothing from the checkout.
            code = f"import sys; sys.path.insert(0, {str(unpacked)!r}); import branchyard, branchyard_client"
            imported = subprocess.run([sys.executable, "-I", "-c", code], capture_output=True, text=True)
            self.assertEqual(imported.returncode, 0, imported.stderr)


class ClaudeCodePluginTests(unittest.TestCase):
    """The Claude Code marketplace, hooks and commands, as files: no
    Claude Code here, so a live `/plugin install` is the final check."""

    def test_the_marketplace_lists_the_plugin_directory(self):
        marketplace = json.loads(package.MARKETPLACE.read_text())
        self.assertEqual(marketplace["name"], "branchyard")
        self.assertTrue(marketplace["owner"]["name"])
        (entry,) = marketplace["plugins"]
        manifest = json.loads((ROOT / entry["source"] / ".claude-plugin/plugin.json").read_text())
        self.assertEqual(entry["name"], manifest["name"])
        self.assertEqual(entry["version"], manifest["version"])
        self.assertTrue(entry["description"])

    def test_hooks_run_by_hook_only_when_by_is_installed(self):
        hooks = json.loads((package.PLUGIN / "hooks/hooks.json").read_text())["hooks"]
        events = {"SessionStart": "session-start", "UserPromptSubmit": "user-prompt-submit"}
        self.assertEqual(set(hooks), set(events))
        empty = tempfile.TemporaryDirectory()
        self.addCleanup(empty.cleanup)
        for event, groups in hooks.items():
            (group,) = groups
            (hook,) = group["hooks"]
            self.assertEqual(hook["type"], "command")
            self.assertLessEqual(hook["timeout"], 10)
            self.assertEqual(
                hook["command"],
                f"command -v by >/dev/null 2>&1 && by hook {events[event]} || true",
            )
            # Without by on PATH: silent, and exit 0.
            out = subprocess.run(
                ["/bin/sh", "-c", hook["command"]],
                input='{"session_id": "s", "cwd": "/"}',
                capture_output=True,
                text=True,
                env={"PATH": empty.name},
            )
            self.assertEqual((out.returncode, out.stdout, out.stderr), (0, "", ""))

    def test_the_manifest_names_no_hooks_file(self):
        # Claude Code finds hooks/hooks.json itself; the copy Branchyard
        # projects for its own turns has no hooks, so its manifest must not
        # name one (crates/branchyard/src/projection.rs).
        manifest = json.loads((package.PLUGIN / ".claude-plugin/plugin.json").read_text())
        self.assertNotIn("hooks", manifest)

    def test_every_command_has_a_description(self):
        commands = sorted((package.PLUGIN / "commands").glob("*.md"))
        self.assertEqual([c.name for c in commands], ["setup.md", "status.md", "wait.md"])
        for command in commands:
            text = command.read_text()
            self.assertTrue(text.startswith("---\ndescription: "), command.name)
            self.assertIn("\nargument-hint: ", text.split("\n---\n", 1)[0], command.name)


class InstallerTests(unittest.TestCase):
    """The installer, run on a plugin archive extracted outside the checkout."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        plugin, _skill, _setup_skill, _sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(plugin) as archive:
            archive.extractall(self.root / "extracted")
        self.plugin = self.root / "extracted/branchyard"
        self.installer = self.plugin / "scripts/install_skill.py"
        self.host = self.root / "host"

    def run_installer(self, *extra):
        return subprocess.run(
            [sys.executable, str(self.installer), "--destination", str(self.host), *extra],
            capture_output=True,
            text=True,
        )

    def test_preview_by_default_then_applies_then_refuses_a_different_install(self):
        preview = self.run_installer()
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertFalse(json.loads(preview.stdout)["applied"])
        self.assertFalse(self.host.exists())

        applied = self.run_installer("--apply")
        self.assertEqual(applied.returncode, 0, applied.stderr)
        installed = self.host / "delegate"
        self.assertEqual(
            (installed / "SKILL.md").read_bytes(),
            (package.SKILL / "SKILL.md").read_bytes(),
        )

        # An identical re-install is a no-op, not a refusal.
        again = self.run_installer("--apply")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertTrue(json.loads(again.stdout)["unchanged"])

        # A changed destination is refused without --force ...
        (installed / "local-notes.md").write_text("do not delete me")
        refused = self.run_installer("--apply")
        self.assertEqual(refused.returncode, 2)
        self.assertEqual(json.loads(refused.stderr)["error"], "destination_exists")
        self.assertTrue((installed / "local-notes.md").exists())

        # ... and replaced with it.
        forced = self.run_installer("--apply", "--force")
        self.assertEqual(forced.returncode, 0, forced.stderr)
        self.assertTrue(json.loads(forced.stdout)["replaced"])
        self.assertFalse((installed / "local-notes.md").exists())
        self.assertEqual(
            (installed / "SKILL.md").read_bytes(),
            (package.SKILL / "SKILL.md").read_bytes(),
        )

    def test_installs_the_setup_skill_by_name(self):
        applied = self.run_installer("--skill", "setup", "--apply")
        self.assertEqual(applied.returncode, 0, applied.stderr)
        installed = self.host / "setup"
        for path, data in package.inputs(package.SETUP_SKILL).items():
            self.assertEqual((installed / path).read_bytes(), data)
        self.assertFalse((self.host / "delegate").exists())
        unknown = self.run_installer("--skill", "nope")
        self.assertEqual(unknown.returncode, 2)

    def test_refuses_a_non_directory_destination(self):
        self.host.mkdir(parents=True)
        (self.host / "delegate").write_text("not a skill")
        refused = self.run_installer("--apply")
        self.assertEqual(refused.returncode, 2)
        self.assertEqual(json.loads(refused.stderr)["error"], "destination_exists")


class SdkArchiveRuntimeTests(unittest.TestCase):
    """The Python SDK archive, extracted outside the checkout and run
    against a stub `by`."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        *_archives, sdk = package.build(self.root / "dist")
        with zipfile.ZipFile(sdk) as archive:
            archive.extractall(self.root / "extracted")
        self.module_path = self.root / "extracted/branchyard-sdk/branchyard.py"

    def _stub_by(self, reply: dict) -> Path:
        stub = self.root / "by"
        stub.write_text(f"#!/usr/bin/env python3\nimport json, sys\nprint(json.dumps({reply!r}))\n")
        stub.chmod(stub.stat().st_mode | stat.S_IEXEC)
        return stub

    def test_extracted_module_is_importable_and_matches_its_source(self):
        self.assertEqual(self.module_path.read_bytes(), (package.SDK / "branchyard.py").read_bytes())
        spec = importlib.util.spec_from_file_location("branchyard_extracted", self.module_path)
        module = importlib.util.module_from_spec(spec)
        # Its error classes are its sibling `branchyard_client`'s, which
        # it imports from beside it, as from the archive.
        sys.modules.pop("branchyard_client", None)
        sys.path.insert(0, str(self.module_path.parent))
        try:
            spec.loader.exec_module(module)
        finally:
            sys.path.remove(str(self.module_path.parent))
        self.assertEqual(
            Path(sys.modules.pop("branchyard_client").__file__).parent, self.module_path.parent, "the extracted sibling"
        )
        stub = self._stub_by(
            {
                "name": "demo",
                "status": {"state": "running"},
                "harness": "codex",
                "profile": "codex-app-server",
                "parent": None,
                "children": [],
                "depth": 0,
                "turns": 1,
                "candidate": None,
                "cost_usd": None,
                "subtree_cost_usd": 0.0,
                "max_usd": None,
                "remaining_usd": None,
                "envelope": None,
                "last_message": "",
            }
        )
        env = dict(os.environ)
        env["BRANCHYARD_BY"] = str(stub)
        code = (
            "import os, sys\n"
            f"sys.path.insert(0, {str(self.module_path.parent)!r})\n"
            "import branchyard\n"
            "result = branchyard.inspect()\n"
            "print(result.name, result.running)\n"
        )
        result = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "demo True")


class ReleaseWorkflowTests(unittest.TestCase):
    """Dist owns packaging; the generated workflow gates publication on native smoke."""

    def setUp(self):
        self.text = (ROOT / ".github/workflows/release.yml").read_text()
        self.config = tomllib.loads((ROOT / "dist-workspace.toml").read_text())

    def test_releases_are_tagged_and_smoke_tests_gate_publication(self):
        self.assertIn("  push:\n    tags:", self.text)
        self.assertNotIn("  pull_request:", self.text)
        host = self.text.split("\n  host:\n", 1)[1].split("\n  custom-release-smoke:\n", 1)[0]
        self.assertIn("      - custom-release-smoke", host)
        self.assertIn("needs.custom-release-smoke.result == 'success'", host)
        self.assertIn("uses: ./.github/workflows/release-smoke.yml", self.text)
        self.assertIn("uses: ./.github/workflows/agy-image.yml", self.text)

    def test_actions_are_pinned_and_generated_configuration_is_reproducible(self):
        for path in ["release.yml", "release-smoke.yml", "release-extras.yml"]:
            text = (ROOT / ".github/workflows" / path).read_text()
            for use in re.findall(r"uses:\s*(\S+)", text):
                if not use.startswith("./"):
                    self.assertRegex(use, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}$")
        spec = importlib.util.spec_from_file_location("release_workflow", ROOT / "tools/release_workflow.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertEqual(module.require_smoke(self.text), self.text)
        checked = subprocess.run(
            [sys.executable, str(ROOT / "tools/release_workflow.py"), "--check"], capture_output=True, text=True
        )
        self.assertEqual(checked.returncode, 0, checked.stderr)
        unguarded = self.text.replace(" && needs.custom-release-smoke.result == 'success'", "")
        self.assertEqual(module.require_smoke(unguarded), self.text)
        with self.assertRaises(ValueError):
            module.require_smoke("no generated host job")

    def test_portable_and_native_artifacts_have_dist_owned_targets_and_resources(self):
        self.assertEqual(self.config["dist"]["cargo-dist-version"], "0.33.1")
        self.assertEqual(
            set(self.config["dist"]["targets"]),
            {
                "x86_64-unknown-linux-musl",
                "aarch64-unknown-linux-musl",
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
            },
        )
        runtime = tomllib.loads((ROOT / "packaging/runtime/dist.toml").read_text())
        self.assertEqual(set(runtime["package"]["binaries"]), {"by", "branchyard-server", "branchyard-bridge"})
        self.assertEqual(
            runtime["package"]["version"],
            tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"],
        )
        self.assertEqual(set(runtime["dist"]["targets"]), {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"})
        for source, packaged in [
            ("vendor/orca/LICENSE", "orca-LICENSE"),
            ("vendor/emdash/LICENSE.md", "emdash-LICENSE.md"),
        ]:
            self.assertEqual((ROOT / source).read_bytes(), (ROOT / "packaging/licenses" / packaged).read_bytes())
        cli = tomllib.loads((ROOT / "crates/branchyard-cli/Cargo.toml").read_text())["package"]["metadata"]["dist"]
        for name in [
            "cloud_setup.py",
            "compute_gce.py",
            "substrate_gke.py",
            "Dockerfile.substrate",
            "substrate-entrypoint.sh",
        ]:
            self.assertTrue(any(path.endswith("/" + name) for path in cli["include"]), name)


def _fake_release(directory, version, target, by_text="#!/bin/sh\necho 'by 9.9.9'\n"):
    """Create dist-shaped archives and checksum-checking generated installer fixtures."""
    import hashlib

    release = directory / "release"
    release.mkdir(exist_ok=True)
    packages = {
        "branchyard-cli": {"by": by_text},
        "branchyard-server": {"branchyard-server": "#!/bin/sh\nexit 0\n"},
        "branchyard-bridge": {"branchyard-bridge": "#!/bin/sh\nexit 0\n"},
        "branchyard-runtime": {
            "by": by_text,
            "branchyard-server": "#!/bin/sh\nexit 0\n",
            "branchyard-bridge": "#!/bin/sh\nexit 0\n",
        },
    }
    for package_name, binaries in packages.items():
        name = f"{package_name}-{target}"
        stage = directory / "stage" / name
        stage.mkdir(parents=True, exist_ok=True)
        for binary, text in binaries.items():
            (stage / binary).write_text(text)
            (stage / binary).chmod(0o755)
        archive = release / f"{name}.tar.gz"
        subprocess.run(["tar", "-C", str(stage.parent), "-czf", str(archive), name], check=True)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        installer = release / f"{package_name}-installer.sh"
        installer.write_text(
            "#!/bin/sh\nset -eu\n"
            f'APP_VERSION="{version}"\n'
            "work=$(mktemp -d)\ntrap 'rm -rf \"$work\"' EXIT\n"
            'case "$INSTALLER_DOWNLOAD_URL" in\n'
            f'file://*) cp "${{INSTALLER_DOWNLOAD_URL#file://}}/{archive.name}" "$work/archive" ;;\n'
            f'*) curl -fsSL "$INSTALLER_DOWNLOAD_URL/{archive.name}" -o "$work/archive" ;;\n'
            "esac\n"
            "actual=$(sha256sum \"$work/archive\" | awk '{print $1}')\n"
            f'[ "$actual" = "{digest}" ] || {{ echo "checksum mismatch" >&2; exit 1; }}\n'
            'tar -xzf "$work/archive" -C "$work"\n'
            'mkdir -p "$CARGO_DIST_FORCE_INSTALL_DIR"\n'
            + "".join(f'cp "$work/{name}/{binary}" "$CARGO_DIST_FORCE_INSTALL_DIR/{binary}"\n' for binary in binaries)
        )
    return release


class InstallScriptTests(unittest.TestCase):
    """The bootstrap delegates to dist installers, including mirrors and terminal handoff."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.target = "x86_64-unknown-linux-musl"
        self.release = _fake_release(self.root, "1.2.3", self.target)
        self.prefix = self.root / "prefix with spaces"

    def run_install(self, *args):
        return subprocess.run(
            ["sh", str(ROOT / "install.sh"), *args],
            capture_output=True,
            text=True,
            env={"PATH": os.environ["PATH"], "HOME": str(self.root)},
            timeout=30,
        )

    def test_it_is_posix_sh(self):
        self.assertEqual(subprocess.run(["sh", "-n", str(ROOT / "install.sh")]).returncode, 0)
        self.assertNotIn("sudo ", (ROOT / "install.sh").read_text())
        self.assertNotIn("[[", (ROOT / "install.sh").read_text())

    def test_installs_three_binaries_and_reinstall_replaces(self):
        args = ["--version", "v1.2.3", "--prefix", str(self.prefix), "--base-url", self.release.as_uri(), "--no-setup"]
        out = self.run_install(*args)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("Installed by 9.9.9", out.stdout)
        for binary in ("by", "branchyard-server", "branchyard-bridge"):
            self.assertEqual(stat.S_IMODE((self.prefix / "bin" / binary).stat().st_mode), 0o755)
        self.assertEqual(self.run_install(*args).returncode, 0)
        relative = os.path.relpath(self.release, ROOT)
        mirrored = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", relative, "--no-setup"
        )
        self.assertEqual(mirrored.returncode, 0, mirrored.stderr)

    def test_a_tampered_archive_installs_nothing(self):
        archive = self.release / f"branchyard-cli-{self.target}.tar.gz"
        with archive.open("ab") as stream:
            stream.write(b"tampered")
        out = self.run_install("--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", self.release.as_uri())
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("checksum mismatch", out.stderr)
        self.assertFalse((self.prefix / "bin/by").exists())

    def test_an_installed_binary_that_cannot_start_is_reported(self):
        _fake_release(self.root, "1.2.3", self.target, "#!/bin/sh\nexit 13\n")
        out = self.run_install("--prefix", str(self.prefix), "--base-url", self.release.as_uri(), "--no-setup")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("installed by could not execute", out.stderr)

    def test_latest_uses_installer_version_and_pins_remaining_downloads(self):
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        curl = fake_bin / "curl"
        curl.write_text(
            "#!/bin/sh\nset -eu\nurl=\nout=\n"
            'while [ "$#" -gt 0 ]; do\n'
            'case "$1" in -o) out=$2; shift 2 ;; https://*) url=$1; shift ;; *) shift ;; esac\ndone\n'
            'printf "%s\\n" "$url" >> "$HOME/downloads"\n'
            f'cp "{self.release}/' + '${url##*/}" "$out"\n'
        )
        curl.chmod(0o755)
        out = subprocess.run(
            ["sh", str(ROOT / "install.sh"), "--prefix", str(self.prefix), "--no-setup"],
            capture_output=True,
            text=True,
            timeout=30,
            env={"PATH": f"{fake_bin}:{os.environ['PATH']}", "HOME": str(self.root)},
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        downloads = (self.root / "downloads").read_text().splitlines()
        self.assertIn("/releases/latest/download/branchyard-cli-installer.sh", downloads[0])
        self.assertTrue(all("/download/v1.2.3/" in url for url in downloads[1:]), downloads)
        self.assertNotIn("different release", out.stderr)

    def test_explicit_headless_setup_and_runtime_flavor(self):
        by = (
            '#!/bin/sh\nif [ "$1" = --version ]; then echo "by 9.9.9"; '
            'else printf "%s\\n" "$*" > "$HOME/setup-arguments"; fi\n'
        )
        _fake_release(self.root, "1.2.3", self.target, by)
        out = self.run_install(
            "--prefix", str(self.prefix), "--base-url", str(self.release), "--setup", "--yes", "--runtime"
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual((self.root / "setup-arguments").read_text().strip(), "setup --yes --up")

    def test_setup_failure_is_returned_and_no_terminal_has_a_remedy(self):
        by = '#!/bin/sh\nif [ "$1" = --version ]; then echo "by 9.9.9"; else exit 17; fi\n'
        _fake_release(self.root, "1.2.3", self.target, by)
        out = self.run_install("--prefix", str(self.prefix), "--base-url", self.release.as_uri(), "--setup", "--yes")
        self.assertEqual(out.returncode, 17)
        out = self.run_install("--prefix", str(self.prefix), "--base-url", self.release.as_uri(), "--setup")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("guided setup needs a terminal", out.stderr)
        self.assertIn("installed successfully", out.stderr)

    @unittest.skipUnless(
        sys.platform.startswith("linux") and shutil.which("script"),
        "util-linux script provides the controlling terminal",
    )
    def test_piped_installer_hands_setup_a_real_terminal(self):
        by = (
            '#!/bin/sh\nif [ "$1" = --version ]; then echo "by 9.9.9"; exit 0; fi\n'
            '[ -t 0 ] || exit 41\nprintf "Continue? "\n'
            'IFS= read -r answer\n[ "$answer" = ready ] || exit 42\n'
            'printf "%s\\n" "$*" > "$HOME/setup-arguments"\n'
        )
        _fake_release(self.root, "1.2.3", self.target, by)
        command = (
            shlex.join(["cat", str(ROOT / "install.sh")])
            + " | "
            + shlex.join(
                [
                    "sh",
                    "-s",
                    "--",
                    "--setup",
                    "--prefix",
                    str(self.prefix),
                    "--base-url",
                    self.release.as_uri(),
                ]
            )
        )
        result = subprocess.run(
            ["script", "-qec", command, "/dev/null"],
            input="ready\n",
            capture_output=True,
            text=True,
            env={"PATH": os.environ["PATH"], "HOME": str(self.root)},
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Continue?", result.stdout)
        self.assertEqual((self.root / "setup-arguments").read_text().strip(), "setup --guided")

    def test_it_refuses_invalid_sources_versions_and_dry_run_writes_nothing(self):
        for args, message in [
            (["--base-url", "http://example.invalid/r"], "refusing plain http"),
            (["--version", "bad/version"], "version must"),
            (["--prefix", "relative"], "absolute path"),
            (["--base-url", self.release.as_uri(), "--version", "2.0.0"], "requested 2.0.0"),
        ]:
            self.assertIn(message, self.run_install(*args).stderr)
        out = self.run_install("--dry-run", "--prefix", str(self.prefix))
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("latest/download", out.stdout)
        self.assertFalse(self.prefix.exists())
        out = self.run_install("--version", "1.2.3", "--dry-run")
        self.assertIn("/download/v1.2.3", out.stdout)


class ReleaseAssetTests(unittest.TestCase):
    def test_assets_use_actual_archive_hashes_and_keep_the_bootstrap(self):
        import hashlib

        spec = importlib.util.spec_from_file_location("release_assets", ROOT / "tools/release_assets.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for target in [
                "aarch64-apple-darwin",
                "x86_64-apple-darwin",
                "aarch64-unknown-linux-musl",
                "x86_64-unknown-linux-musl",
            ]:
                release = _fake_release(root, "0.0.1", target)
            bootstrap, sums, formula = module.build_assets(release, root / "output")
            self.assertEqual(bootstrap.read_bytes(), (ROOT / "install.sh").read_bytes())
            self.assertIn(hashlib.sha256(bootstrap.read_bytes()).hexdigest() + "  install.sh", sums.read_text())
            self.assertIn("resource binary", formula.read_text())
        for tool in ["release_assets.py", "release_smoke.py"]:
            result = subprocess.run(
                [sys.executable, str(ROOT / "tools" / tool), "--help"], capture_output=True, text=True
            )
            self.assertEqual(result.returncode, 0, result.stderr)


class HomebrewFormulaTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("homebrew_formula", ROOT / "tools/homebrew_formula.py")
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)

    def test_the_template_fills_from_sha256sums(self):
        sums = "".join(
            f"{str(i) * 64}  {package}-{target}.tar.gz\n"
            for i, target in enumerate(self.module.TARGETS)
            for package in self.module.PACKAGES
        )
        formula = self.module.render("1.2.3", sums)
        self.assertNotRegex(formula, r"@[A-Z0-9_]+@")
        self.assertIn('version "1.2.3"', formula)
        self.assertIn("releases/download/v1.2.3", formula)
        self.assertIn('"branchyard-cli" => "' + "0" * 64 + '"', formula)
        with self.assertRaisesRegex(ValueError, "lists no"):
            self.module.render("1.2.3", sums.splitlines()[0])


class HarnessImageTests(unittest.TestCase):
    """`deploy/Dockerfile.harnesses`, statically: no container runtime here."""

    def setUp(self):
        self.text = (ROOT / "deploy/Dockerfile.harnesses").read_text()

    def test_the_build_stage_is_deploy_dockerfiles(self):
        def stage(text):
            start = text.index("FROM rust:")
            return text[start : text.index("\nFROM ", start + 1)]

        self.assertEqual(stage(self.text), stage((ROOT / "deploy/Dockerfile").read_text()))

    def test_runtime_images_are_pinned_by_digest_and_run_as_a_user(self):
        froms = re.findall(r"^FROM (\S+)", self.text, re.M)
        self.assertEqual(len(froms), 3)
        for image in froms[1:]:
            self.assertRegex(image, r"^node:22[\w.-]*@sha256:[0-9a-f]{64}$")
        self.assertRegex(self.text, r"\nUSER branchyard\n")
        self.assertIn("openssh-client", self.text)
        self.assertIn(" git ", self.text)
        self.assertIn("npm ci --omit=dev", self.text)
        self.assertNotIn("npm install", self.text)
        self.assertNotRegex(self.text, r"(?i)ENV [^\n]*(KEY|TOKEN|SECRET)")

    def test_harnesses_are_the_qualified_pins_with_integrity(self):
        manifest = json.loads((ROOT / "deploy/harnesses/package.json").read_text())
        lock = json.loads((ROOT / "deploy/harnesses/package-lock.json").read_text())
        qualify = (ROOT / ".github/workflows/qualify.yml").read_text()
        for package, version in manifest["dependencies"].items():
            self.assertRegex(version, r"^\d+\.\d+\.\d+$", "exact pins only")
            self.assertIn(f"{package}@{version}", qualify)
            self.assertEqual(lock["packages"][f"node_modules/{package}"]["version"], version)
        self.assertEqual(lock["packages"][""]["dependencies"], manifest["dependencies"])
        for path, entry in lock["packages"].items():
            if not path:
                continue
            self.assertRegex(entry.get("integrity", ""), r"^sha512-", path)
            self.assertTrue(entry["resolved"].startswith("https://registry.npmjs.org/"), path)


if __name__ == "__main__":
    unittest.main()
