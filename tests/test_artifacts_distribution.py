"""Installed-wheel assets and the optional runtime's dependency contract."""

import ast
import importlib.metadata
import importlib.util
import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

tomllib = importlib.import_module("tomllib" if sys.version_info >= (3, 11) else "tomli")

ROOT = Path(__file__).resolve().parents[1]
SDK = ROOT / "sdk/python"
PACKAGE = SDK / "branchyard_artifacts"


class PackagingTests(unittest.TestCase):
    def test_matrix_installs_explicit_build_dependencies_before_distribution_tests(self):
        build_input = (ROOT / "deploy/artifacts-test-requirements.in").read_text()
        build_lock = (ROOT / "deploy/artifacts-test-requirements.lock").read_text()
        for name in ["setuptools", "wheel", "tomli"]:
            self.assertRegex(build_input, rf"(?m)^{name}==")
            self.assertRegex(build_lock, rf"(?m)^{name}==[^\n]+\\\n\s+--hash=sha256:")
        self.assertNotRegex((ROOT / "deploy/artifacts-requirements.in").read_text(), r"(?m)^setuptools==")
        workflow = (ROOT / ".github/workflows/artifacts.yml").read_text()
        install = "python -m pip install --require-hashes -r deploy/artifacts-test-requirements.lock"
        preflight = "Require wheel build prerequisites"
        run = "python tests/test_artifacts_distribution.py -v"
        self.assertIn(install, workflow)
        self.assertLess(workflow.index(install), workflow.index(preflight))
        self.assertLess(workflow.index(preflight), workflow.index(run))

    def test_base_has_no_dependencies_and_artifact_pins_match_the_hashed_lock(self):
        project = tomllib.loads((SDK / "pyproject.toml").read_text())
        self.assertEqual(project["project"]["dependencies"], [])
        self.assertEqual(project["project"]["requires-python"], ">=3.9")
        self.assertIn("branchyard_artifacts", project["tool"]["setuptools"]["packages"])
        lock = (ROOT / "deploy/artifacts-requirements.lock").read_text()

        def normalize(name):
            return name.casefold().replace("_", "-")

        locked = {normalize(name): version for name, version in re.findall(r"^([\w-]+)==([^\s]+)", lock, re.M)}
        for pin in project["project"]["optional-dependencies"]["artifacts"]:
            # These are deliberately exact pins; wheel building validates marker syntax.
            name, version = pin.partition(";")[0].strip().split("==")
            self.assertEqual(locked[normalize(name)], version)
        self.assertGreater(lock.count("--hash=sha256:"), len(locked))
        image = (ROOT / "deploy/Dockerfile.artifacts").read_text()
        self.assertIn("--require-hashes", image)
        self.assertIn("python3-uno", image)
        self.assertIn("fonts-dejavu-core", image)
        self.assertIn("poppler-utils", image)

    @unittest.skipUnless(sys.version_info >= (3, 11), "bare Python needs the stdlib TOML reader")
    def test_pin_checks_need_no_optional_site_packages(self):
        result = subprocess.run(
            [
                sys.executable,
                "-S",
                __file__,
                "PackagingTests.test_base_has_no_dependencies_and_artifact_pins_match_the_hashed_lock",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_every_package_module_parses_as_python39_and_every_example_is_versioned(self):
        for path in PACKAGE.glob("*.py"):
            with self.subTest(path=path.name):
                ast.parse(path.read_text(), filename=path.name, feature_version=(3, 9))
        for path in (PACKAGE / "examples").glob("*.json"):
            with self.subTest(path=path.name):
                self.assertEqual(json.loads(path.read_text())["schema_version"], 1)
        for path in (PACKAGE / "schemas").glob("*.json"):
            value = json.loads(path.read_text())
            self.assertFalse(value["additionalProperties"])
            self.assertEqual(value["properties"]["schema_version"], {"const": 1})

    def test_installed_wheel_has_executable_module_schemas_and_examples_away_from_source(self):
        if importlib.util.find_spec("setuptools") is None or importlib.util.find_spec("pip") is None:
            self.skipTest("wheel installation needs setuptools and pip")
        if int(importlib.metadata.version("setuptools").split(".")[0]) < 77:
            self.skipTest("setuptools 77 or later reads the SDK SPDX license")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            shutil.copytree(SDK, source, ignore=shutil.ignore_patterns("build", "*.egg-info", "__pycache__"))
            built = subprocess.run(
                [sys.executable, "-c", "import setuptools.build_meta as b; print(b.build_wheel('dist'))"],
                cwd=source,
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(built.returncode, 0, built.stderr)
            wheel = next((source / "dist").glob("*.whl"))
            with zipfile.ZipFile(wheel) as archive:
                for path in PACKAGE.rglob("*"):
                    if path.is_file() and path.suffix in {".py", ".json"}:
                        relative = "branchyard_artifacts/" + path.relative_to(PACKAGE).as_posix()
                        self.assertEqual(archive.read(relative), path.read_bytes())
            installed = root / "installed"
            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "pip",
                    "install",
                    "--no-index",
                    "--no-deps",
                    "--target",
                    str(installed),
                    str(wheel),
                ],
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            shutil.rmtree(source)
            code = """
import json,sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import branchyard, branchyard_client, branchyard_artifacts
from branchyard_artifacts.__main__ import main
package=Path(branchyard_artifacts.__file__).parent
assert (package/'schemas/section-v1.json').is_file()
assert json.loads((package/'examples/deck.json').read_text())['kind']=='deck'
raise SystemExit(main(['deck','schema','section']))
"""
            result = subprocess.run(
                [sys.executable, "-I", "-c", code, str(installed)], cwd=root, capture_output=True, text=True, timeout=30
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["properties"]["schema_version"], {"const": 1})


if __name__ == "__main__":
    unittest.main()
