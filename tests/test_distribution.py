"""Exercise the distribution archives (`tools/package.py`) as delivered:
extracted outside the checkout, installed with the shipped installer, and
run with the shipped launcher / imported as the shipped module. See
`docs/distribution.md`.
"""

import importlib.metadata
import importlib.util
import json
import os
import re
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
# tools/release_version.py, as tools/package.py loaded it.
release_version = package.release_version


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
        # The workspace version, as Python spells it (0.1.0-rc.1 is 0.1.0rc1).
        self.assertEqual(project["version"], release_version.pep440(workspace["version"]))
        # An SPDX expression: setuptools 77 reads it, and deprecates the table form.
        self.assertEqual(project["license"], workspace["license"])
        self.assertEqual(self.project["build-system"]["requires"], ["setuptools>=77"])
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
        installed = importlib.metadata.version("setuptools")
        if int(installed.split(".")[0]) < 77:
            self.skipTest(f"setuptools {installed} cannot read an SPDX license; pip fetches 77 or later to build it")
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


WORKFLOW = ROOT / ".github/workflows/release.yml"
# The condition every step or job that attests, drafts or pushes runs under.
DRAFTS = "needs.version.outputs.draft == 'true'"


def workflow_jobs(text):
    """{job id: its lines} for the jobs of a workflow, in file order: the keys indented two
    spaces under `jobs:`."""
    jobs: dict = {}
    current = None
    for line in text.split("\njobs:\n", 1)[1].splitlines():
        match = re.match(r"^  ([A-Za-z0-9_-]+):$", line)
        if match:
            current = match.group(1)
            jobs[current] = []
        elif current:
            jobs[current].append(line)
    return {name: "\n".join(lines) for name, lines in jobs.items()}


def step_script(job, name):
    """The `run: |` block of the step named `name` in a job's text, as the shell gets it."""
    lines = job.splitlines()
    start = lines.index(f"      - name: {name}")
    for index in range(start + 1, len(lines)):
        if lines[index].startswith("      - "):
            break
        if lines[index] == "        run: |":
            block = []
            for line in lines[index + 1 :]:
                if line.strip() and not line.startswith(" " * 10):
                    break
                block.append(line[10:])
            return "\n".join(block).strip("\n") + "\n"
    raise AssertionError(f"no `run: |` in step {name!r}")


def run_step(script, cwd, env):
    """A step's script as a runner runs a `run:` without `shell:` (bash -e)."""
    return subprocess.run(["bash", "-e", "-c", script], cwd=cwd, env=env, capture_output=True, text=True)


def fake_by(version):
    """A stand-in `by`: `--version` and the bare status card, as the release's installs run them."""
    return (
        "#!/bin/sh\n"
        'case "${1:-}" in\n'
        f'  --version) echo "by {version}" ;;\n'
        '  "") echo "Branchyard: $PWD is not inside a git repository." ;;\n'
        "  *) exit 2 ;;\n"
        "esac\n"
    )


def host_target():
    """install.sh's choice of archive for this machine, or None where it has none."""
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(os.uname().machine)
    system = {"Linux": "unknown-linux-musl", "Darwin": "apple-darwin"}.get(os.uname().sysname)
    return f"{arch}-{system}" if arch and system else None


class ReleaseVersionTests(unittest.TestCase):
    """`tools/release_version.py`: one version, everywhere it is written."""

    def run_tool(self, *args, root=ROOT):
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/release_version.py"), "--root", str(root), *args],
            capture_output=True,
            text=True,
        )

    def test_every_field_carries_the_workspace_version(self):
        version = release_version.workspace_version()
        self.assertEqual(release_version.disagreements(), [])
        fields = release_version.fields()
        # Every kind of field is read: crate path dependencies, Cargo.lock, each manifest, the SDK.
        files = {relative for relative, _field, _value in fields}
        for needed in ["Cargo.lock", "crates/branchyard-cli/Cargo.toml", "sdk/python/pyproject.toml"]:
            self.assertIn(needed, files)
        self.assertLessEqual(set(release_version.JSON_FIELDS) | set(release_version.TOML_FIELDS), files)
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["members"]
        locked = [field for relative, field, _value in fields if relative == "Cargo.lock"]
        self.assertEqual(len(locked), len(workspace))
        out = self.run_tool()
        self.assertEqual((out.returncode, out.stdout), (0, version + "\n"), out.stderr)
        out = self.run_tool("--pep440")
        self.assertEqual(out.stdout, release_version.pep440(version) + "\n")

    def test_python_spells_a_pre_release_the_pep_440_way(self):
        for version, python in [
            ("0.1.0", "0.1.0"),
            ("0.1.0-rc.1", "0.1.0rc1"),
            ("1.2.3-alpha.4", "1.2.3a4"),
            ("10.0.0-beta.12", "10.0.0b12"),
        ]:
            self.assertEqual(release_version.pep440(version), python)
        for unusable in ["0.1", "v0.1.0", "0.1.0-rc1", "0.1.0-dev.1", "0.1.0+build.5", "0.1.0-rc.1.2"]:
            with self.assertRaises(ValueError, msg=unusable):
                release_version.pep440(unusable)

    def test_a_tag_must_be_v_and_the_workspace_version(self):
        version = release_version.workspace_version()
        self.assertEqual(self.run_tool("--tag", f"v{version}").stdout, version + "\n")
        for wrong in [version, "v9.9.9", f"v{version}.1", f"release-{version}"]:
            out = self.run_tool("--tag", wrong)
            self.assertEqual(out.returncode, 1, wrong)
            self.assertIn("does not name the workspace version", out.stderr)
            self.assertEqual(out.stdout, "")

    def copy_tree(self, destination):
        """The files the tool reads or writes, copied out of the checkout."""
        names = {"Cargo.toml", "Cargo.lock", *release_version.JSON_FIELDS, *release_version.TOML_FIELDS}
        names |= {path.relative_to(ROOT).as_posix() for path in release_version.members().values()}
        for name in names:
            (destination / name).parent.mkdir(parents=True, exist_ok=True)
            (destination / name).write_bytes((ROOT / name).read_bytes())
        return names

    def test_set_rewrites_every_field_and_nothing_else(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            names = self.copy_tree(root)
            old = release_version.workspace_version(root)
            out = self.run_tool("--set", "v2.0.0-beta.3", root=root)
            self.assertEqual((out.returncode, out.stdout), (0, "2.0.0-beta.3\n"), out.stderr)
            self.assertEqual(release_version.disagreements(root), [])
            self.assertEqual(release_version.workspace_version(root), "2.0.0-beta.3")
            self.assertIn('version = "2.0.0b3"', (root / "sdk/python/pyproject.toml").read_text())
            # Only version lines changed; every other byte of every file is as it was.
            for name in sorted(names):
                before = (ROOT / name).read_text().splitlines()
                after = (root / name).read_text().splitlines()
                self.assertEqual(len(before), len(after), name)
                for was, now in zip(before, after, strict=True):
                    if was != now:
                        self.assertIn("version", was, name)
                        self.assertEqual(
                            now, was.replace(old, "2.0.0-beta.3").replace(release_version.pep440(old), "2.0.0b3")
                        )
            # The registry crates in Cargo.lock keep their versions.
            lock = tomllib.loads((root / "Cargo.lock").read_text())["package"]
            registry = [p for p in lock if "source" in p]
            self.assertTrue(registry)
            original = {(p["name"], p["version"]) for p in tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]}
            self.assertLessEqual({(p["name"], p["version"]) for p in registry}, original)
            # A field left behind is named, with the command that fixes it.
            herdr = root / "plugins/herdr/herdr-plugin.toml"
            herdr.write_text(herdr.read_text().replace('version = "2.0.0-beta.3"', 'version = "1.0.0"'))
            out = self.run_tool(root=root)
            self.assertEqual(out.returncode, 1)
            self.assertIn("plugins/herdr/herdr-plugin.toml version is '1.0.0', not '2.0.0-beta.3'", out.stderr)
            self.assertIn("next: python3 tools/release_version.py --set 2.0.0-beta.3", out.stderr)
            # A version neither Cargo nor Python can say is refused before anything is written.
            out = self.run_tool("--set", "2.0", root=root)
            self.assertEqual(out.returncode, 1)
            self.assertIn("is not a release version", out.stderr)
            self.assertEqual(release_version.workspace_version(root), "2.0.0-beta.3")

    def test_the_docs_name_the_workspace_version(self):
        version = release_version.workspace_version()
        python = release_version.pep440(version)
        patterns = [
            (r"\bv(\d+\.\d+\.\d+(?:-[a-z]+\.\d+)?)\b", version),  # a tag, or its download URL
            (r"--version (\d+\.\d+\.\d+\S*)", version),  # install.sh and the formula
            (r"\bby (\d+\.\d+\.\d+[A-Za-z0-9.+-]*)", version),  # what by --version prints
            (r"branchyard-(?:[a-z-]+-)?(\d+\.\d+\.\d+(?:-[a-z]+\.\d+)?)(?:-|\.zip)", version),  # archives
            (r"branchyard-(\d+\.\d+\.\d+(?:[a-z]+\d+)?)(?:-py3|\.tar\.gz)", python),  # the wheel and sdist
        ]
        for page in ["README.md", "docs/distribution.md", "docs/getting-started.md", "docs/upgrade.md", "docs/sdk.md"]:
            text = (ROOT / page).read_text()
            for pattern, wanted in patterns:
                for found in re.findall(pattern, text):
                    self.assertEqual(found, wanted, f"{page} names {found} ({pattern})")


class ReleaseWorkflowTests(unittest.TestCase):
    """`.github/workflows/release.yml`, read: when it runs and what a dry run may not do. Parsed
    as YAML too where PyYAML is installed."""

    def setUp(self):
        self.text = WORKFLOW.read_text()
        self.jobs = workflow_jobs(self.text)
        try:
            import yaml
        except ImportError:
            self.parsed = None
        else:
            self.parsed = yaml.safe_load(self.text)

    def triggers(self):
        head = self.text.split("\npermissions:", 1)[0]
        return head[head.index("\non:\n") + 1 :]

    def test_a_tag_drafts_and_a_pull_request_or_run_workflow_rehearses(self):
        on = self.triggers()
        self.assertIn("  push:\n    tags: ['v*']\n", on)
        self.assertIn("  pull_request:\n    paths:\n", on)
        self.assertIn("  workflow_dispatch:\n    inputs:\n      draft:\n", on)
        for absent in ("schedule", "branches"):
            self.assertNotIn(absent, on)
        paths = re.findall(r"^      - (\S+)$", on, re.M)
        for needed in [".github/workflows/release.yml", "install.sh", "packaging/**", "sdk/python/pyproject.toml"]:
            self.assertIn(needed, paths)
        for path in paths:
            self.assertTrue(list(ROOT.glob(path.replace("/**", ""))), f"{path} names nothing")
        # Every script of the repository the workflow runs is a path that starts a rehearsal.
        for script in set(re.findall(r"\btools/[\w-]+\.py", self.text)):
            self.assertIn(script, paths)
        if self.parsed is not None:
            on = self.parsed[True]  # PyYAML reads the key `on` as True.
            self.assertEqual(on["push"], {"tags": ["v*"]})
            self.assertEqual(on["pull_request"], {"paths": paths})
            self.assertEqual(
                on["workflow_dispatch"]["inputs"]["draft"],
                {
                    "description": on["workflow_dispatch"]["inputs"]["draft"]["description"],
                    "type": "boolean",
                    "default": False,
                },
            )
            self.assertEqual(list(self.parsed["jobs"]), list(self.jobs))

    def test_jobs_run_in_order_and_only_draft_writes(self):
        self.assertEqual(list(self.jobs), ["version", "build", "python", "assemble", "draft", "image"])
        self.assertIn("    needs: [version, build, python]\n", self.jobs["assemble"])
        self.assertIn("    needs: [version, assemble]\n", self.jobs["draft"])
        self.assertIn(f"    if: {DRAFTS}\n", self.jobs["draft"])
        self.assertIn("    if: github.event_name != 'pull_request'\n", self.jobs["image"])
        for name, job in self.jobs.items():
            self.assertEqual("contents: write" in job, name == "draft", name)
            self.assertEqual("packages: write" in job, name == "image", name)
            self.assertEqual("gh release create" in job, name == "draft", name)
            self.assertEqual("docker push" in job, name == "image", name)
        if self.parsed is not None:
            jobs = self.parsed["jobs"]
            self.assertEqual(jobs["draft"]["if"], DRAFTS)
            self.assertEqual(jobs["draft"]["permissions"]["contents"], "write")
            self.assertEqual(self.parsed["permissions"], {"contents": "read"})
            for name in ("version", "build", "python", "assemble"):
                self.assertNotIn("if", jobs[name], f"a dry run runs {name}")

    def test_a_dry_run_attests_drafts_and_pushes_nothing(self):
        for name, job in self.jobs.items():
            steps = job.split("\n      - ")
            for step in steps:
                if "actions/attest-build-provenance@" in step and name != "draft":
                    self.assertIn(f"if: {DRAFTS}", step, name)
                if "docker push" in step:
                    self.assertIn(f"if: vars.PUBLISH_IMAGES == 'true' && {DRAFTS}", step)
        self.assertEqual(self.text.count("actions/attest-build-provenance@"), 3)
        self.assertIn("--draft --verify-tag $prerelease", self.jobs["draft"])

    def test_every_action_is_pinned_to_a_commit(self):
        uses = re.findall(r"uses:\s*(\S+)", self.text)
        self.assertTrue(uses)
        for use in uses:
            self.assertRegex(use, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}$")

    def test_it_builds_four_targets_and_checks_them(self):
        for target in (
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
        ):
            self.assertIn(f"target: {target}", self.text)
        # The archives the Homebrew formula (and so the assemble job) needs are the ones built.
        self.assertEqual(sorted(re.findall(r"target: (\S+)", self.text)), sorted(_homebrew().TARGETS))
        self.assertIn("-p branchyard-cli -p branchyard-server", self.text)
        self.assertIn("rust-toolchain.toml", self.jobs["build"])
        self.assertIn("python3 -m build --outdir dist sdk/python", self.jobs["python"])
        self.assertIn("if-no-files-found: error", self.jobs["build"])
        self.assertIn("docker build -f deploy/Dockerfile.harnesses", self.jobs["image"])

    def test_a_static_binary_passes_whichever_way_file_says_it(self):
        check = step_script(self.jobs["build"], "Check the binaries")
        (pattern,) = re.findall(r"grep -Eq '([^']+)'", check)
        # file 5.45 (Ubuntu 24.04) on Rust's musl executables, which are static-pie, and on a
        # classic static one; never a dynamic one.
        self.assertRegex("ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped", pattern)
        self.assertRegex("ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped", pattern)
        self.assertNotRegex("ELF 64-bit LSB pie executable, dynamically linked, interpreter /lib64", pattern)

    def test_the_scripts_are_what_yaml_reads(self):
        if self.parsed is None:
            self.skipTest("PyYAML is not installed")
        for job_name, job in self.parsed["jobs"].items():
            for step in job["steps"]:
                if "name" in step and "run" in step and "\n" in step["run"]:
                    self.assertEqual(step_script(self.jobs[job_name], step["name"]), step["run"], step["name"])


def _homebrew():
    spec = importlib.util.spec_from_file_location("homebrew_formula", ROOT / "tools/homebrew_formula.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WorkflowStepTests(unittest.TestCase):
    """The workflow's own step scripts, run here with bash against stand-in binaries: the
    version decision, packing and installing one archive, and assembling a release."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.jobs = workflow_jobs(WORKFLOW.read_text())
        self.version = release_version.workspace_version()
        self.python = release_version.pep440(self.version)
        self.env = {
            "PATH": os.pathsep.join([str(Path(sys.executable).parent), os.environ["PATH"]]),
            "HOME": str(self.root / "home"),
            "RUNNER_TEMP": str(self.root / "runner"),
            "VERSION": self.version,
            "PYTHON_VERSION": self.python,
            "GITHUB_OUTPUT": str(self.root / "output"),
            "GITHUB_STEP_SUMMARY": str(self.root / "summary.md"),
        }
        (self.root / "runner").mkdir()

    def decide(self, event, ref_type, ref_name, draft_input=""):
        (self.root / "output").write_text("")
        env = dict(
            self.env,
            GITHUB_EVENT_NAME=event,
            GITHUB_REF_TYPE=ref_type,
            GITHUB_REF_NAME=ref_name,
            DRAFT_INPUT=draft_input,
        )
        out = run_step(step_script(self.jobs["version"], "Read and check the version"), ROOT, env)
        values = dict(line.split("=", 1) for line in (self.root / "output").read_text().splitlines())
        return out, values

    def test_only_a_tag_drafts(self):
        tag = f"v{self.version}"
        for event, ref_type, ref_name, draft_input, drafts in [
            ("push", "tag", tag, "", "true"),
            ("workflow_dispatch", "tag", tag, "true", "true"),
            ("workflow_dispatch", "tag", tag, "false", "false"),
            ("workflow_dispatch", "branch", "main", "true", "false"),
            ("pull_request", "branch", "27/merge", "", "false"),
        ]:
            out, values = self.decide(event, ref_type, ref_name, draft_input)
            self.assertEqual(out.returncode, 0, out.stderr)
            self.assertEqual(values, {"version": self.version, "python": self.python, "draft": drafts}, event)
        out, values = self.decide("push", "tag", "v9.9.9")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("does not name the workspace version", out.stderr)
        self.assertEqual(values, {})

    def stage_build(self, target, by_version):
        """A checkout with a built `by` and `branchyard-server` for `target` (stand-ins)."""
        for name in [
            "LICENSE",
            "THIRD_PARTY.md",
            "README.md",
            "install.sh",
            "vendor/orca/LICENSE",
            "vendor/emdash/LICENSE.md",
        ]:
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / name).write_bytes((ROOT / name).read_bytes())
        release = self.root / "target" / target / "release"
        release.mkdir(parents=True, exist_ok=True)
        for binary, text in (("by", fake_by(by_version)), ("branchyard-server", "#!/bin/sh\nexit 0\n")):
            (release / binary).write_text(text)
            (release / binary).chmod(0o755)

    def test_package_then_install_the_archive(self):
        if shutil.which("shasum") is None:
            self.skipTest("no shasum (the Package step's checksum tool)")
        target = "x86_64-unknown-linux-musl"
        self.stage_build(target, self.version)
        env = dict(self.env, TARGET=target)
        out = run_step(step_script(self.jobs["build"], "Package"), self.root, env)
        self.assertEqual(out.returncode, 0, out.stderr)
        name = f"branchyard-{self.version}-{target}"
        archive = self.root / "dist" / f"{name}.tar.gz"
        with tarfile.open(archive) as tar:
            members = {m.name: m for m in tar.getmembers() if m.isfile()}
        self.assertEqual(
            set(members),
            {
                f"{name}/{f}"
                for f in [
                    "by",
                    "branchyard-server",
                    "LICENSE",
                    "THIRD_PARTY.md",
                    "README.md",
                    "licenses/orca-LICENSE",
                    "licenses/emdash-LICENSE.md",
                ]
            },
        )
        self.assertEqual(stat.S_IMODE(members[f"{name}/by"].mode), 0o755)
        import hashlib

        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual((self.root / "dist" / f"{name}.tar.gz.sha256").read_text(), f"{digest}  {name}.tar.gz\n")

        out = run_step(step_script(self.jobs["build"], "Install the archive with install.sh"), self.root, env)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn(f"installed by {self.version} into {self.root}/runner/by/bin", out.stdout)
        self.assertIn("is not inside a git repository", out.stdout)

        # A binary that names another version fails the install check.
        shutil.rmtree(self.root / "dist")
        shutil.rmtree(self.root / "runner" / "by")
        self.stage_build(target, "9.9.9")
        self.assertEqual(run_step(step_script(self.jobs["build"], "Package"), self.root, env).returncode, 0)
        out = run_step(step_script(self.jobs["build"], "Install the archive with install.sh"), self.root, env)
        self.assertNotEqual(out.returncode, 0)

    def stage_assemble(self):
        """What the assemble job has once the artifacts are downloaded: every archive with its
        .sha256, the Python files, and the checkout's install.sh, tools and packaging."""
        import hashlib

        dist = self.root / "dist"
        dist.mkdir()
        for target in _homebrew().TARGETS:
            name = f"branchyard-{self.version}-{target}"
            stage = self.root / "stage" / name
            stage.mkdir(parents=True)
            (stage / "by").write_text(fake_by(self.version))
            (stage / "by").chmod(0o755)
            archive = dist / f"{name}.tar.gz"
            subprocess.run(["tar", "-C", str(stage.parent), "-czf", str(archive), name], check=True)
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            (dist / f"{name}.tar.gz.sha256").write_text(f"{digest}  {archive.name}\n")
        python_files = [p.name for p in package.build(self.root / "zips")]
        python_files += [f"branchyard-{self.python}-py3-none-any.whl", f"branchyard-{self.python}.tar.gz"]
        for name in python_files:
            (dist / name).write_bytes(name.encode())
        (self.root / "install.sh").write_bytes((ROOT / "install.sh").read_bytes())
        for linked in ("tools", "packaging"):
            (self.root / linked).symlink_to(ROOT / linked)
        return python_files

    def test_assemble_writes_sums_and_formula_installs_and_names_only_real_files(self):
        if shutil.which("sha256sum") is None or host_target() not in _homebrew().TARGETS:
            self.skipTest("needs sha256sum and a machine install.sh has an archive for (the job runs on Linux)")
        python_files = self.stage_assemble()
        job = self.jobs["assemble"]
        for step in ("Write SHA256SUMS and the Homebrew formula", "Install from the directory, as a person would"):
            out = run_step(step_script(job, step), self.root, self.env)
            self.assertEqual(out.returncode, 0, f"{step}: {out.stderr}")
        dist = self.root / "dist"
        archives = [f"branchyard-{self.version}-{t}.tar.gz" for t in _homebrew().TARGETS]
        files = sorted([*archives, *python_files, "install.sh", "branchyard.rb"])
        self.assertEqual(sorted(p.name for p in dist.iterdir()), sorted([*files, "SHA256SUMS"]))
        sums = (dist / "SHA256SUMS").read_text().splitlines()
        self.assertEqual([line.split("  ", 1)[1] for line in sums], files)
        formula = (dist / "branchyard.rb").read_text()
        self.assertNotRegex(formula, r"@[A-Z0-9_]+@")
        self.assertIn(f'version "{self.version}"', formula)

        out = run_step(step_script(job, "Write the release notes"), self.root, self.env)
        self.assertEqual(out.returncode, 0, out.stderr)
        notes = (self.root / "release-notes.md").read_text()
        self.assertNotRegex(notes, r"@[A-Z_]+@")
        named = set(re.findall(r"branchyard[\w.+-]*\.(?:zip|whl|tar\.gz|rb)\b", notes)) | set(
            re.findall(r"\b(?:install\.sh|SHA256SUMS)\b", notes)
        )
        self.assertTrue(named)
        self.assertLessEqual(named, {*files, "SHA256SUMS"}, "the notes name a file the release lacks")
        summary = (self.root / "summary.md").read_text()
        self.assertIn(notes, summary)
        self.assertIn((dist / "SHA256SUMS").read_text(), summary)

    def test_an_archive_that_changed_after_its_build_stops_the_release(self):
        if shutil.which("sha256sum") is None:
            self.skipTest("needs sha256sum (the job runs on Linux)")
        self.stage_assemble()
        archive = self.root / "dist" / f"branchyard-{self.version}-x86_64-unknown-linux-musl.tar.gz"
        archive.write_bytes(archive.read_bytes() + b"\0")
        out = run_step(
            step_script(self.jobs["assemble"], "Write SHA256SUMS and the Homebrew formula"), self.root, self.env
        )
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("FAILED", out.stdout + out.stderr)
        self.assertFalse((self.root / "dist" / "SHA256SUMS").exists())


def _fake_release(directory, version, target, by_text="#!/bin/sh\necho 'by 9.9.9'\n"):
    name = f"branchyard-{version}-{target}"
    stage = directory / "stage" / name
    (stage / "licenses").mkdir(parents=True)
    for binary, text in (("by", by_text), ("branchyard-server", "#!/bin/sh\nexit 0\n")):
        (stage / binary).write_text(text)
        (stage / binary).chmod(0o755)
    for extra in ("LICENSE", "THIRD_PARTY.md", "README.md"):
        (stage / extra).write_text(extra)
    (stage / "licenses" / "orca-LICENSE").write_text("MIT")
    release = directory / "release"
    release.mkdir(exist_ok=True)
    archive = release / f"{name}.tar.gz"
    subprocess.run(["tar", "-C", str(stage.parent), "-czf", str(archive), name], check=True)
    import hashlib

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (release / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")
    return release


class InstallScriptTests(unittest.TestCase):
    """`install.sh` against a local file:// "release", in a temporary prefix."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.target = "x86_64-unknown-linux-musl"
        self.release = _fake_release(self.root, "1.2.3", self.target)
        self.prefix = self.root / "prefix"

    def run_install(self, *args, cwd=None, **env):
        return subprocess.run(
            ["sh", str(ROOT / "install.sh"), "--target", self.target, *args],
            capture_output=True,
            text=True,
            cwd=cwd,
            env={"PATH": os.environ["PATH"], "HOME": str(self.root), **env},
        )

    def test_it_is_posix_sh(self):
        self.assertEqual(subprocess.run(["sh", "-n", str(ROOT / "install.sh")]).returncode, 0)
        text = (ROOT / "install.sh").read_text()
        self.assertTrue(text.startswith("#!/bin/sh\n"))
        self.assertNotIn("sudo ", text.replace("never calls sudo", ""))
        self.assertNotIn("[[", text)
        if shutil.which("shellcheck"):
            out = subprocess.run(["shellcheck", "-s", "sh", str(ROOT / "install.sh")], capture_output=True, text=True)
            self.assertEqual(out.returncode, 0, out.stdout)

    def test_installs_after_verifying_the_checksum(self):
        out = self.run_install(
            "--version", "v1.2.3", "--prefix", str(self.prefix), "--base-url", f"file://{self.release}"
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("installed by 9.9.9 into", out.stdout)
        for binary in ("by", "branchyard-server"):
            path = self.prefix / "bin" / binary
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o755)
        self.assertTrue((self.prefix / "share/branchyard/licenses/orca-LICENSE").is_file())
        # A plain directory works too, and installing again replaces.
        out = self.run_install("--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", str(self.release))
        self.assertEqual(out.returncode, 0, out.stderr)

    def test_a_tampered_archive_installs_nothing(self):
        sums = self.release / "SHA256SUMS"
        sums.write_text("0" * 64 + sums.read_text()[64:])
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", f"file://{self.release}"
        )
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("checksum mismatch", out.stderr)
        self.assertFalse((self.prefix / "bin" / "by").exists())

    def test_it_refuses_what_it_cannot_do_safely(self):
        out = self.run_install("--prefix", str(self.prefix))
        self.assertIn("--version", out.stderr)
        out = self.run_install("--version", "1.2.3", "--base-url", "http://example.invalid/r")
        self.assertIn("refusing plain http", out.stderr)
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", f"file://{self.root}/nowhere"
        )
        self.assertNotEqual(out.returncode, 0)
        self.assertFalse(self.prefix.exists())
        out = self.run_install("--version", "1.2.3", "--dry-run")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("would install branchyard-1.2.3-x86_64-unknown-linux-musl.tar.gz", out.stdout)

    def test_installs_from_the_base_in_branchyard_install_base(self):
        # How the release workflow installs what it just built, and how a draft is checked.
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), BRANCHYARD_INSTALL_BASE=str(self.release)
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertTrue((self.prefix / "bin" / "by").is_file())
        out = self.run_install("--version", "1.2.3", "--dry-run", BRANCHYARD_INSTALL_BASE=str(self.release))
        self.assertIn(f"from {self.release} into", out.stdout)
        # --base-url wins over the variable.
        out = self.run_install(
            "--version",
            "1.2.3",
            "--prefix",
            str(self.prefix),
            "--base-url",
            str(self.release),
            BRANCHYARD_INSTALL_BASE=f"{self.root}/nowhere",
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), BRANCHYARD_INSTALL_BASE="http://x.invalid"
        )
        self.assertIn("refusing plain http", out.stderr)

    def test_says_how_to_put_the_prefix_on_path(self):
        # A relative prefix with a trailing slash is advised as the absolute directory PATH needs.
        out = self.run_install(
            "--version", "1.2.3", "--prefix", "prefix/", "--base-url", str(self.release), cwd=self.root
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn(f"installed by 9.9.9 into {self.prefix}/bin", out.stdout)
        self.assertIn(f"{self.prefix}/bin is not on your PATH", out.stdout)
        self.assertIn(f'  export PATH="{self.prefix}/bin:$PATH"\n', out.stdout)

    def test_names_a_by_that_comes_first_on_path(self):
        earlier = self.root / "cargo-bin"
        earlier.mkdir()
        (earlier / "by").write_text("#!/bin/sh\necho 'by 0.0.1'\n")
        (earlier / "by").chmod(0o755)
        path = os.pathsep.join([str(earlier), str(self.prefix / "bin"), os.environ["PATH"]])
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", str(self.release), PATH=path
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn(
            f"note: {earlier}/by comes first on your PATH; remove it or put {self.prefix}/bin before it", out.stdout
        )
        self.assertNotIn("export PATH", out.stdout)
        # First on PATH: nothing to say.
        path = os.pathsep.join([str(self.prefix / "bin"), str(earlier), os.environ["PATH"]])
        out = self.run_install(
            "--version", "1.2.3", "--prefix", str(self.prefix), "--base-url", str(self.release), PATH=path
        )
        self.assertEqual(out.stdout.splitlines()[1:], [])


class HomebrewFormulaTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("homebrew_formula", ROOT / "tools/homebrew_formula.py")
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)

    def test_the_template_fills_from_sha256sums(self):
        sums = "".join(f"{str(i) * 64}  branchyard-1.2.3-{t}.tar.gz\n" for i, t in enumerate(self.module.TARGETS))
        formula = self.module.render("1.2.3", sums)
        self.assertNotRegex(formula, r"@[A-Z0-9_]+@")
        self.assertIn('version "1.2.3"', formula)
        self.assertIn("releases/download/v1.2.3", formula)
        self.assertIn('sha256 "' + "0" * 64 + '"', formula)
        with self.assertRaisesRegex(ValueError, "lists no"):
            self.module.render("1.2.3", sums.splitlines()[0])

    def test_the_formula_test_matches_what_by_prints(self):
        template = self.module.TEMPLATE.read_text()
        # `by recipe list` in a new repository says "No recipes: add [recipes.NAME] ..." (27
        # October 2026); the source spells it lower case, so the formula matches either.
        (pattern,) = re.findall(r'assert_match\(/([^/]+)/i, shell_output\("#\{bin\}/by recipe list"\)\)', template)
        source = (ROOT / "crates/branchyard-cli/src/recipe_cmd.rs").read_text()
        self.assertIn(f'"{pattern}: add [recipes.NAME]', source)
        self.assertIn('assert_match version.to_s, shell_output("#{bin}/by --version")', template)


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
