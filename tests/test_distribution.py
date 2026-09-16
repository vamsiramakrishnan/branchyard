"""Exercise extracted delivery artifacts and the actual CLI without source-tree imports."""
import hashlib
import importlib.util
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("BRANCHYARD_TEST_BINARY", ROOT / "target/debug/branchyard")).resolve()
spec = importlib.util.spec_from_file_location("package", ROOT / "tools/package.py")
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class DistributionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("BRANCHYARD_")}
        self.env["BRANCHYARD_BIN"] = str(BINARY)

    def run_cli(self, *args, code=0):
        result = subprocess.run([str(BINARY), *args], cwd=self.root, env=self.env, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, code, result.stderr)
        return result

    def test_archives_are_reproducible_complete_and_identical_skill_sources(self):
        first = package.build(self.root / "one")
        second = package.build(self.root / "two")
        for a, b in zip(first, second):
            self.assertEqual(a.read_bytes(), b.read_bytes())
            with zipfile.ZipFile(a) as archive:
                manifest = json.loads(archive.read("branchyard/MANIFEST.sha256.json"))
                self.assertEqual(set(archive.namelist()), {"branchyard/" + name for name in manifest} | {"branchyard/MANIFEST.sha256.json"})
                for name, digest in manifest.items():
                    self.assertEqual(hashlib.sha256(archive.read("branchyard/" + name)).hexdigest(), digest)
        with zipfile.ZipFile(first[0]) as plugin, zipfile.ZipFile(first[1]) as skill:
            for path, data in package.inputs(package.SKILL).items():
                self.assertEqual(plugin.read("branchyard/skills/branchyard/" + path), data)
                self.assertEqual(skill.read("branchyard/" + path), data)

    def test_extracted_plugin_installs_and_launches_without_checkout(self):
        plugin = package.build(self.root / "dist")[0]
        with zipfile.ZipFile(plugin) as archive:
            archive.extractall(self.root / "extracted")
        root = self.root / "extracted/branchyard"
        installer = root / "scripts/install_skill.py"
        command = [sys.executable, str(installer), "--destination", str(self.root / "host")]
        preview = subprocess.run(command, cwd=self.root, env=self.env, capture_output=True, text=True, check=True)
        self.assertFalse(json.loads(preview.stdout)["applied"])
        self.assertFalse((self.root / "host").exists())
        subprocess.run([*command, "--apply"], cwd=self.root, env=self.env, capture_output=True, check=True)
        refusal = subprocess.run([*command, "--apply"], cwd=self.root, env=self.env, capture_output=True)
        self.assertEqual(refusal.returncode, 2)
        installed = self.root / "host/branchyard"
        self.assertEqual((installed / "SKILL.md").read_bytes(), (package.SKILL / "SKILL.md").read_bytes())
        launch = [sys.executable, str(installed / "scripts/branchyard.py")]
        result = subprocess.run([*launch, "describe"], cwd=self.root, env=self.env, capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(result.stdout)["schema"], "branchyard/contract/v1alpha1")
        self.env["BRANCHYARD_BIN"] = "/does/not/exist"
        result = subprocess.run([*launch, "describe"], cwd=self.root, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(json.loads(result.stderr)["error"], "unavailable_cli")

    def test_schema_matches_checked_in_contract(self):
        self.assertEqual(json.loads(self.run_cli("describe").stdout), json.loads((ROOT / "schema/contract.json").read_text()))

    def test_validation_is_offline_and_remote_calls_require_configuration(self):
        result = self.run_cli("validate", "--file", str(ROOT / "examples/commands/create-task.json"))
        self.assertTrue(json.loads(result.stdout)["valid"])
        error = self.run_cli("doctor", code=2)
        self.assertEqual(error.stdout, "")
        self.assertEqual(json.loads(error.stderr)["error"], "invalid_request")

    def test_oversized_and_unknown_inputs_fail_without_echoing_content(self):
        request = self.root / "bad.json"
        request.write_text('{"private-secret": "' + "a" * (256 * 1024) + '"}')
        result = self.run_cli("validate", "--file", str(request), code=2)
        self.assertNotIn("private-secret", result.stderr)
        self.assertLess(len(result.stderr), 512)
        request.write_text('{"private-secret": true}')
        result = self.run_cli("validate", "--file", str(request), code=2)
        self.assertNotIn("private-secret", result.stderr)

    def test_cli_unknown_submission_reconciles_without_another_post(self):
        operations, calls = {}, []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self):
                calls.append(self.path)
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                command = json.loads(raw)
                operation_id = command["operation_id"]
                if self.headers["Authorization"] != "Bearer fixture-only-token":
                    self.reply(401, {})
                    return
                operations[operation_id] = {
                    "schema": "branchyard/v1alpha1", "operation_id": operation_id,
                    "request_sha256": hashlib.sha256(raw).hexdigest(),
                    "state": "pending", "task_ids": [],
                }
                self.reply(503, {"message": "private server details"})

            def do_GET(self):
                self.reply(200, operations[self.path.rsplit("/", 1)[-1]])

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
        worker.start()
        try:
            self.env["BRANCHYARD_ENDPOINT"] = f"http://127.0.0.1:{server.server_port}"
            self.env["BRANCHYARD_TOKEN"] = "fixture-only-token"
            request = str(ROOT / "examples/commands/create-task.json")
            uncertain = self.run_cli("--allow-loopback-http", "call", "--file", request, code=4)
            error = json.loads(uncertain.stderr)
            self.assertEqual(error["error"], "submission_unknown")
            self.assertNotIn("private server", uncertain.stderr)
            recovered = self.run_cli("--allow-loopback-http", "reconcile", "--file", request)
            self.assertEqual(json.loads(recovered.stdout)["request_sha256"], error["request_sha256"])
            self.assertEqual(calls, ["/v1alpha1/commands"])
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
