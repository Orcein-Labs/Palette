"""Real host file operations and one-shot administrator approval entry point."""
import hashlib
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import patch_broker as patch_broker_module
from patch_broker import PatchBroker, request_digest
from repair_broker import RepairBroker, normalize_recipe, run_isolated_test
from prewarm_depot import resolve_real_julia_binary


def python_test_recipe(recipe_id, *arguments, executable=None, toolchain_root=None,
                       additional_read_roots=()):
    """Declare only the interpreter's trusted toolchain to the test worker."""
    executable = str(Path(executable).absolute() if executable else Path(sys.executable).resolve())
    root = Path(toolchain_root or sys.base_prefix).resolve(strict=True)
    if not Path(executable).is_relative_to(root):
        raise ValueError("Python executable must be contained by its declared toolchain root")
    roots = [root, *(Path(path).resolve(strict=True) for path in additional_read_roots)]
    return {"id": recipe_id, "argv": [executable, *arguments],
            "read_roots": list(dict.fromkeys(str(path) for path in roots))}


class PatchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "project"
        self.root.mkdir()
        self.store = Path(self.tmp.name) / "grants"
        self.broker = PatchBroker(self.store, {"project": str(self.root)})
        (self.root / "a.txt").write_text("before\n")

    def prepare(self, path="a.txt", before="before\n", content="after\n"):
        return self.broker.prepare({"target": "project", "changes": [{"path": path, "before_sha256": hashlib.sha256(before.encode()).hexdigest() if before is not None else None, "content": content}]}, "world-a")

    def approve(self, record):
        result = subprocess.run([sys.executable, str(Path(__file__).with_name("patch_broker.py")), "--storage", str(self.store), "approve", record["request_id"], "--digest", record["patch_sha256"]], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_real_apply_and_replay(self):
        read = self.broker.call({"action":"read", "target":"project", "path":"a.txt"}, "world-a", {}, None, None)
        self.assertEqual(read["content"], "before\n")
        self.assertEqual(read["sha256"], hashlib.sha256(b"before\n").hexdigest())
        request = self.prepare()
        with self.assertRaises(PermissionError):
            self.broker.apply(request["request_id"], "world-a")
        self.approve(request)
        with self.assertRaises(PermissionError):
            self.broker.apply(request["request_id"], "world-b")
        result = self.broker.apply(request["request_id"], "world-a")
        self.assertEqual(result["status"], "applied", result)
        self.assertEqual((self.root / "a.txt").read_text(), "after\n")
        self.assertEqual(result["files_touched"][0]["after_sha256"], hashlib.sha256(b"after\n").hexdigest())
        with self.assertRaises(PermissionError):
            self.broker.apply(request["request_id"], "world-a")
        self.assertIn('"status": "applied"', (self.store / "receipts.jsonl").read_text())

    def test_target_lock_is_shared_across_registries(self):
        other = PatchBroker(Path(self.tmp.name) / "other-grants", {"project": str(self.root)})
        lock_path = self.root.parent / ("." + self.root.name + ".palette-patch.lock")
        for broker in [self.broker, other]:
            with broker.target_locked(str(self.root)):
                fd = os.open(lock_path, os.O_RDWR)
                try:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                finally:
                    os.close(fd)
        fd = os.open(lock_path, os.O_RDWR)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            os.close(fd)

    def test_D02_patch_roots_cannot_cover_private_grants(self):
        alias = Path(self.tmp.name) / "grants-alias"
        alias.symlink_to(self.store, target_is_directory=True)
        for root in (self.store, self.store.parent, alias):
            with self.subTest(root=root), self.assertRaises(PermissionError):
                PatchBroker(self.store, {"unsafe":str(root)})

    def test_conflict_consumes_without_overwrite(self):
        request = self.prepare()
        self.approve(request)
        (self.root / "a.txt").write_text("someone else's work\n")
        result = self.broker.apply(request["request_id"], "world-a")
        self.assertEqual(result["status"], "failed", result)
        self.assertEqual((self.root / "a.txt").read_text(), "someone else's work\n")
        with self.assertRaises(PermissionError):
            self.broker.apply(request["request_id"], "world-a")

    def test_create_and_unsafe_paths(self):
        request = self.prepare("new.txt", None, "new\n")
        self.approve(request)
        self.assertEqual(self.broker.apply(request["request_id"], "world-a")["status"], "applied")
        self.assertEqual((self.root / "new.txt").read_text(), "new\n")
        outside = Path(self.tmp.name) / "outside"
        outside.mkdir()
        (outside / "external.txt").write_text("before\n")
        (self.root / "link").symlink_to(outside, target_is_directory=True)
        (self.root / "alias.txt").symlink_to(outside / "external.txt")
        os.link(outside / "external.txt", self.root / "hard.txt")
        for path in ["../outside/external.txt", "link/external.txt", "alias.txt", "hard.txt", ".git/config", "/tmp/nope"]:
            with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                self.prepare(path)
        self.assertEqual((outside / "external.txt").read_text(), "before\n")

    def test_R08_credentials_are_forbidden_even_under_protected_or_self_apply_roots(self):
        paths = ("credentials.json", "secrets/token", ".ssh/id_rsa", ".aws/credentials", ".gnupg/private-keys-v1.d/key")
        for relative in paths:
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("credential-before\n")
            with self.subTest(path=relative), self.assertRaises(PermissionError):
                self.prepare(relative, "credential-before\n", "credential-after\n")
            with self.subTest(read_path=relative), self.assertRaises(PermissionError):
                self.broker.call({"action": "read", "target": "project", "path": relative}, "world-a", {}, None, None)

        self.broker.protected_paths = (self.root.resolve(),)
        self.assertEqual(self.broker.classify("project", ".ssh/id_rsa")["class"], "forbidden")
        self.broker.auto_apply_prefixes["project"] = (("adapters", ".ssh"),)
        self.assertEqual(self.broker.classify("project", ".ssh/id_rsa")["class"], "forbidden")

    def test_R09_old_approved_credential_request_is_reclassified_at_apply(self):
        credential = self.root / "credentials.json"
        credential.write_text("before\n")
        # Model a pending request created by an older broker before the
        # immutable credential denylist was added.
        with patch.object(PatchBroker, "classify", return_value={"class": "runtime", "self_apply": True, "reason": "old policy"}):
            request = self.prepare("credentials.json", "before\n", "after\n")
        # Approval does not mutate files; the current apply-time policy must
        # stop a grant created under the earlier classification.
        self.broker.approve(request["request_id"], request["patch_sha256"], "local_administrator")
        with self.assertRaises(PermissionError):
            self.broker.apply(request["request_id"], "world-a")
        self.assertEqual(credential.read_text(), "before\n")

    def test_symlink_swap_after_approval(self):
        request = self.prepare()
        self.approve(request)
        outside = Path(self.tmp.name) / "external.txt"
        outside.write_text("before\n")
        (self.root / "a.txt").unlink()
        (self.root / "a.txt").symlink_to(outside)
        result = self.broker.apply(request["request_id"], "world-a")
        self.assertEqual(result["status"], "failed")
        self.assertEqual(outside.read_text(), "before\n")

    def test_client_without_confirmation_cannot_apply(self):
        request = self.prepare()
        result = self.broker.call({"action": "apply", "request_id": request["request_id"]}, "world-a", {}, None, None)
        self.assertEqual(result["status"], "approval_required")
        self.assertEqual((self.root / "a.txt").read_text(), "before\n")
        with self.assertRaises(PermissionError):
            self.broker.approve(request["request_id"], "wrong digest", "local_administrator")


    def test_expired_and_tampered_requests(self):
        request = self.prepare()
        path = self.broker.path(request["request_id"])
        request["expiry"] = 0
        request["patch_sha256"] = request_digest(request)
        path.write_text(json.dumps(request))
        with self.assertRaises(PermissionError):
            self.broker.approve(request["request_id"], request["patch_sha256"], "local_administrator")
        request = self.prepare()
        altered = dict(request, changes=[{"path":"a.txt", "before_sha256":request["changes"][0]["before_sha256"], "content":"not approved"}])
        self.broker.path(request["request_id"]).write_text(json.dumps(altered))
        with self.assertRaises(PermissionError):
            self.broker.approve(request["request_id"], request["patch_sha256"], "local_administrator")
        self.assertEqual((self.root/"a.txt").read_text(), "before\n")

    def test_partial_failure_is_reported_without_replay(self):
        protected = self.root/"protected"
        protected.mkdir()
        (protected/"b.txt").write_text("before\n")
        before = hashlib.sha256(b"before\n").hexdigest()
        request = self.broker.prepare({"target":"project", "changes":[{"path":name, "before_sha256":before, "content":"after\n"} for name in ["a.txt", "protected/b.txt"]]}, "world-a")
        self.approve(request)
        protected.chmod(0o555)
        try:
            result = self.broker.apply(request["request_id"], "world-a")
            self.assertEqual(result["status"], "partial", result)
            self.assertEqual([row["path"] for row in result["files_touched"]], ["a.txt"])
            self.assertEqual((self.root/"a.txt").read_text(), "after\n")
            self.assertEqual((protected/"b.txt").read_text(), "before\n")
            with self.assertRaises(PermissionError):
                self.broker.apply(request["request_id"], "world-a")
        finally:
            protected.chmod(0o755)

    def test_R01_tested_ordinary_repair_applies_exact_digest(self):
        (self.root / "recipes").mkdir()
        (self.root / "recipes" / "value.py").write_text("VALUE = 'before'\n")
        (self.root / "test_recipe.py").write_text(
            "import unittest\nfrom recipes.value import VALUE\n"
            "class RepairTests(unittest.TestCase):\n"
            "    def test_value(self): self.assertEqual(VALUE, 'after')\n"
        )
        broker = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest", "-v", "test_recipe")},
            {"project": [("recipes", "recipes")]})
        before = hashlib.sha256(b"VALUE = 'before'\n").hexdigest()
        proposal = broker.call({"action": "propose", "target": "project", "changes": [
            {"path": "recipes/value.py", "before_sha256": before, "content": "VALUE = 'after'\n"}],
            "test_command": ["this-is-ignored"]}, "world-a")
        self.assertEqual(proposal["classifications"][0]["class"], "recipes")
        tested = broker.call({"action": "test", "repair_id": proposal["repair_id"]}, "world-a")
        self.assertEqual(tested["status"], "testing", tested)
        tested = broker.wait_test(proposal["repair_id"], "world-a")
        self.assertEqual(tested["status"], "tested", tested)
        self.assertIn("Ran 1 test", tested["test"]["output"])
        applied = broker.call({"action": "apply", "repair_id": proposal["repair_id"]}, "world-a")
        self.assertEqual(applied["status"], "applied", applied)
        self.assertEqual((self.root / "recipes" / "value.py").read_text(), "VALUE = 'after'\n")

    def test_R02_failing_disposable_test_cannot_apply(self):
        (self.root / "recipes").mkdir()
        (self.root / "recipes" / "value.py").write_text("VALUE = 'before'\n")
        (self.root / "test_recipe.py").write_text(
            "import unittest\nfrom recipes.value import VALUE\n"
            "class RepairTests(unittest.TestCase):\n"
            "    def test_value(self): self.assertEqual(VALUE, 'after')\n"
        )
        broker = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest", "-v", "test_recipe")},
            {"project": [("recipes", "recipes")]})
        before = hashlib.sha256(b"VALUE = 'before'\n").hexdigest()
        proposal = broker.propose({"target": "project", "changes": [
            {"path": "recipes/value.py", "before_sha256": before, "content": "VALUE = 'still-wrong'\n"}]}, "world-a")
        tested = broker.test(proposal["repair_id"], "world-a")
        self.assertEqual(tested["status"], "test_failed", tested)
        self.assertIsNotNone(tested["test"]["exit_code"])
        self.assertNotEqual(tested["test"]["exit_code"], 0)
        self.assertIn("FAILED (failures=1)", tested["test"]["output"])
        with self.assertRaises(PermissionError):
            broker.apply(proposal["repair_id"], "world-a")
        self.assertEqual((self.root / "recipes" / "value.py").read_text(), "VALUE = 'before'\n")

    def test_R03_protected_unknown_and_credentials_never_self_apply(self):
        classes = {"project": [("runtime", "runtime"), ("tools", "tools"),
                               ("recipes", "recipes"), ("adapters", "adapters")]}
        broker = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest")}, classes)
        self.assertFalse(broker.patches.classify("project", "runtime/security/patch_broker.py")["self_apply"])
        self.assertFalse(broker.patches.classify("project", "runtime/capabilities/manifest.json")["self_apply"])
        self.assertEqual(broker.patches.classify("project", "settings/credentials.json")["class"], "forbidden")
        self.assertFalse(broker.patches.classify("project", "unclassified/app.py")["self_apply"])
        nested = PatchBroker(Path(self.tmp.name) / "nested-grants",
            {"security-tree": str(Path(__file__).resolve().parent)},
            {"security-tree": [("runtime", "new_helpers")]})
        self.assertFalse(nested.classify("security-tree", "new_helpers/module.py")["self_apply"])
        self.assertFalse(broker.patches.classify("project", "app/config.py")["self_apply"])

    def test_R04_repair_record_tampering_cannot_launder_tests(self):
        (self.root / "recipes").mkdir()
        original = "VALUE = 'before'\n"
        (self.root / "recipes" / "value.py").write_text(original)
        broker = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest")},
            {"project": [("recipes", "recipes")]})
        proposal = broker.propose({"target": "project", "changes": [{
            "path": "recipes/value.py", "before_sha256": hashlib.sha256(original.encode()).hexdigest(),
            "content": "VALUE = 'tested'\n"}]}, "world-a")
        path = broker.record_path(proposal["repair_id"])
        record = json.loads(path.read_text())
        record["changes"][0]["content"] = "VALUE = 'laundered'\n"
        path.write_text(json.dumps(record))
        with self.assertRaises(PermissionError):
            broker.test(proposal["repair_id"], "world-a")
        with self.assertRaises(PermissionError):
            broker.apply(proposal["repair_id"], "world-a")
        self.assertEqual((self.root / "recipes" / "value.py").read_text(), original)

    def test_R05_host_approval_retry_keeps_test_evidence(self):
        authority = self.root / "runtime" / "security"
        authority.mkdir(parents=True)
        source = authority / "logic.py"
        source.write_text("VALUE = 'before'\n")
        (self.root / "test_recipe.py").write_text(
            "import unittest\nfrom pathlib import Path\n"
            "class RepairTests(unittest.TestCase):\n"
            "    def test_candidate(self): self.assertIn(\"after\", Path('runtime/security/logic.py').read_text())\n"
        )
        broker = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest", "-v", "test_recipe")})
        proposal = broker.propose({"target": "project", "changes": [{
            "path": "runtime/security/logic.py", "before_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "content": "VALUE = 'after'\n"}]}, "world-a")
        self.assertEqual(broker.test(proposal["repair_id"], "world-a")["status"], "tested")
        pending = broker.apply(proposal["repair_id"], "world-a")
        self.assertEqual(pending["status"], "tested")
        self.assertEqual(pending["apply"]["status"], "approval_required")
        self.broker.approve(proposal["patch_request_id"], proposal["patch_sha256"], "trusted_test_admin")
        applied = broker.apply(proposal["repair_id"], "world-a")
        self.assertEqual(applied["status"], "applied", applied)
        self.assertEqual(source.read_text(), "VALUE = 'after'\n")

    def test_R06_test_recipe_arguments_are_bound_to_result(self):
        (self.root / "recipes").mkdir()
        source = self.root / "recipes" / "value.py"
        source.write_text("VALUE = 'before'\n")
        (self.root / "test_recipe.py").write_text(
            "import unittest\nfrom recipes.value import VALUE\n"
            "class RepairTests(unittest.TestCase):\n"
            "    def test_value(self): self.assertEqual(VALUE, 'after')\n"
        )
        recipe = python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest", "test_recipe")
        broker = RepairBroker(self.broker, {"project": recipe}, {"project": [("recipes", "recipes")]})
        proposal = broker.propose({"target": "project", "changes": [{
            "path": "recipes/value.py", "before_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "content": "VALUE = 'after'\n"}]}, "world-a")
        self.assertEqual(broker.test(proposal["repair_id"], "world-a")["status"], "tested")
        broker.test_recipes["project"]["argv"] = ["/usr/bin/false"]
        with self.assertRaises(PermissionError):
            broker.apply(proposal["repair_id"], "world-a")
        self.assertEqual(source.read_text(), "VALUE = 'before'\n")

    def test_R07_julia_candidate_runs_in_disposable_worker(self):
        executable = Path(resolve_real_julia_binary()).resolve()
        toolchain = executable.parent.parent
        (self.root / "tools").mkdir()
        source = self.root / "tools/value.jl"
        source.write_text("answer() = 0\n")
        (self.root / "test_value.jl").write_text('using Test; include("tools/value.jl"); @test answer() == 42\n')
        recipe = {"id": "julia-value-v1", "argv": [str(executable), "--startup-file=no", "test_value.jl"],
                  "read_roots": [str(toolchain)]}
        broker = RepairBroker(self.broker, {"project": recipe}, {"project": [("tools", "tools")]})
        proposal = broker.propose({"target": "project", "changes": [{"path": "tools/value.jl",
            "before_sha256": hashlib.sha256(source.read_bytes()).hexdigest(), "content": "answer() = 42\n"}]}, "world-a")
        broker.start_test(proposal["repair_id"], "world-a")
        tested = broker.wait_test(proposal["repair_id"], "world-a")
        self.assertEqual(tested["status"], "tested", tested)
        self.assertEqual(broker.apply(proposal["repair_id"], "world-a")["status"], "applied")
        self.assertEqual(source.read_text(), "answer() = 42\n")

    def test_R10_disposable_candidate_cannot_read_staged_credentials(self):
        (self.root / "recipes").mkdir()
        source = self.root / "recipes" / "value.py"
        source.write_text("VALUE = 'before'\n")
        synthetic_credentials = {
            ".env": "ENV_SENTINEL_PRIVATE_FIXTURE\n",
            "credentials/token": "CREDENTIALS_SENTINEL_PRIVATE_FIXTURE\n",
            ".ssh/id_rsa": "SSH_SENTINEL_PRIVATE_FIXTURE\n",
        }
        for relative, content in synthetic_credentials.items():
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        (self.root / "test_recipe.py").write_text(
            "import unittest\nfrom recipes.value import VALUE\n"
            "class RepairTests(unittest.TestCase):\n"
            "    def test_ordinary_candidate(self): self.assertEqual(VALUE, 'after')\n"
        )
        candidate = (
            "from pathlib import Path\n"
            "for candidate in ('.env', 'credentials/token', '.ssh/id_rsa'):\n"
            "    try: print(Path(candidate).read_text(), end='')\n"
            "    except FileNotFoundError: pass\n"
            "VALUE = 'after'\n"
        )
        broker = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest", "-v", "test_recipe")},
            {"project": [("recipes", "recipes")]})
        proposal = broker.propose({"target": "project", "changes": [{
            "path": "recipes/value.py", "before_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "content": candidate,
        }]}, "world-a")
        tested = broker.test(proposal["repair_id"], "world-a")
        self.assertEqual(tested["status"], "tested", tested)
        output = tested["test"]["output"]
        for marker in synthetic_credentials.values():
            self.assertNotIn(marker.strip(), output)
        self.assertIn("Ran 1 test", output)
        self.assertEqual(source.read_text(), "VALUE = 'before'\n")

    def test_R11_whitelisted_readonly_capability_source_is_tested_before_apply(self):
        executable = Path(resolve_real_julia_binary()).resolve()
        toolchain = executable.parent.parent
        registry = Path(self.tmp.name) / "runtime-capabilities"
        capability = registry / "gesso-fixture"
        capability.mkdir(parents=True)
        source = capability / "gesso.jl"
        source.write_text("gesso() = :before\n")
        source.chmod(0o440)
        manifest = capability / "capability.json"
        manifest.write_text(json.dumps({"id": "gesso-fixture", "version": "1", "source": "gesso.jl",
                                       "entrypoint": "gesso", "dependencies": [], "requires": []}))
        manifest.chmod(0o640)
        (capability / "verify_gesso.jl").write_text(
            'using Test; include("gesso.jl"); @test gesso() === :after\n')
        broker = PatchBroker(Path(self.tmp.name) / "gesso-grants", {"gesso": str(capability)},
            {"gesso": [("adapters", "gesso.jl")]}, registry_root=registry,
            sandbox_source_paths={"gesso": ["gesso.jl"]})
        repair = RepairBroker(broker,
            {"gesso": {"id": "fixed-julia-gesso-v1",
                       "argv": [str(executable), "--startup-file=no", "verify_gesso.jl"],
                       "read_roots": [str(toolchain)]}},
            {"gesso": [("adapters", "gesso.jl")]})
        before = source.read_bytes()
        change = {"path": "gesso.jl", "before_sha256": hashlib.sha256(before).hexdigest(),
                  "content": "gesso() = :after\n"}
        self.assertEqual(broker.classify("gesso", "gesso.jl")["class"], "adapters")
        manual_request = broker.prepare({"target": "gesso", "changes": [change]}, "world-a")
        self.assertEqual(broker.call({"action": "apply", "request_id": manual_request["request_id"]},
                                     "world-a", {}, None, None)["status"], "approval_required")
        self.assertEqual(source.read_bytes(), before)
        proposal = repair.propose({"target": "gesso", "changes": [change]}, "world-a")
        self.assertEqual(proposal["classifications"][0]["class"], "adapters")
        repair.start_test(proposal["repair_id"], "world-a")
        tested = repair.wait_test(proposal["repair_id"], "world-a")
        self.assertEqual(tested["status"], "tested", tested)
        applied = repair.apply(proposal["repair_id"], "world-a")
        self.assertEqual(applied["status"], "applied", applied)
        self.assertEqual(source.read_text(), "gesso() = :after\n")
        self.assertEqual(source.stat().st_mode & 0o777, 0o440)
        self.assertEqual(json.loads(manifest.read_text())["source"], "gesso.jl")

    def test_R12_registry_exceptions_never_cover_manifest_security_config_host_or_credentials(self):
        registry = Path(self.tmp.name) / "runtime-capabilities"
        capability = registry / "adapter"
        capability.mkdir(parents=True)
        source = capability / "adapter.jl"
        source.write_text("answer() = 1\n")
        manifest = capability / "capability.json"
        manifest.write_text(json.dumps({"id": "adapter", "version": "1", "source": "adapter.jl",
                                       "entrypoint": "answer", "dependencies": [], "requires": []}))
        broker = PatchBroker(Path(self.tmp.name) / "registry-grants", {"adapter": str(capability)},
            {"adapter": [("adapters", "adapter.jl"), ("adapters", "capability.json"),
                          ("adapters", "config.json"), ("adapters", "security")]},
            registry_root=registry, sandbox_source_paths={"adapter": ["adapter.jl"]})
        self.assertEqual(broker.classify("adapter", "capability.json")["class"], "protected")
        self.assertEqual(broker.classify("adapter", "config.json")["class"], "protected")
        self.assertEqual(broker.classify("adapter", "security/logic.jl")["class"], "protected")
        for relative in ("capability.json", "config.json", "security/logic.jl"):
            with self.subTest(path=relative):
                self.assertFalse(broker.classify("adapter", relative)["self_apply"])
        for relative in (".env", ".ssh/id_rsa", "credentials/token"):
            with self.subTest(path=relative):
                self.assertEqual(broker.classify("adapter", relative)["class"], "forbidden")
        protected_host = PatchBroker(Path(self.tmp.name) / "hostcwd-grants", {"adapter": str(capability)},
            {"adapter": [("adapters", "adapter.jl")]}, protected_paths=[capability],
            registry_root=registry, sandbox_source_paths={"adapter": ["adapter.jl"]})
        classified = protected_host.classify("adapter", "adapter.jl")
        self.assertEqual(classified["class"], "protected")
        self.assertFalse(classified["self_apply"])
        executable = Path(resolve_real_julia_binary()).resolve()
        (capability / "verify_adapter.jl").write_text(
            'using Test; include("adapter.jl"); @test answer() == 2\n')
        protected_repair = RepairBroker(protected_host,
            {"adapter": {"id": "fixed-julia-protected-adapter-v1",
                          "argv": [str(executable), "--startup-file=no", "verify_adapter.jl"],
                          "read_roots": [str(executable.parent.parent)]}},
            {"adapter": [("adapters", "adapter.jl")]})
        change = {"path": "adapter.jl", "before_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
                  "content": "answer() = 2\n"}
        protected_proposal = protected_repair.propose({"target": "adapter", "changes": [change]}, "world-a")
        self.assertEqual(protected_proposal["classifications"][0]["class"], "protected")
        protected_repair.start_test(protected_proposal["repair_id"], "world-a")
        self.assertEqual(protected_repair.wait_test(protected_proposal["repair_id"], "world-a")["status"], "tested")
        pending = protected_repair.apply(protected_proposal["repair_id"], "world-a")
        self.assertEqual(pending["status"], "tested")
        self.assertEqual(pending["apply"]["status"], "approval_required")
        self.assertEqual(source.read_text(), "answer() = 1\n")
        protected_host.approve(protected_proposal["patch_request_id"], protected_proposal["patch_sha256"],
                               "local_administrator")
        self.assertEqual(protected_repair.apply(protected_proposal["repair_id"], "world-a")["status"], "applied")
        self.assertEqual(source.read_text(), "answer() = 2\n")
        with self.assertRaises(ValueError):
            PatchBroker(Path(self.tmp.name) / "manifest-whitelist-grants", {"adapter": str(capability)},
                {"adapter": [("adapters", "capability.json")]}, registry_root=registry,
                sandbox_source_paths={"adapter": ["capability.json"]})
        ssh_root = Path(self.tmp.name) / ".ssh"
        ssh_root.mkdir()
        private_key = ssh_root / "id_rsa"
        private_key.write_text("SYNTHETIC_PRIVATE_KEY_FIXTURE\n")
        narrowed = PatchBroker(Path(self.tmp.name) / "ssh-grants", {"private": str(ssh_root)})
        self.assertEqual(narrowed.classify("private", "id_rsa")["class"], "forbidden")
        with self.assertRaises(PermissionError):
            narrowed.call({"action": "read", "target": "private", "path": "id_rsa"},
                          "world-a", {}, None, None)
        with self.assertRaises(PermissionError):
            narrowed.prepare({"target": "private", "changes": [{
                "path": "id_rsa", "before_sha256": hashlib.sha256(private_key.read_bytes()).hexdigest(),
                "content": "REPLACED\n"}]}, "world-a")

    def test_R13_default_deployed_registry_stays_immutable_without_explicit_exception(self):
        fake_runtime = Path(self.tmp.name) / "runtime"
        fake_security = fake_runtime / "security"
        capability = fake_runtime / "capabilities" / "gesso"
        fake_security.mkdir(parents=True)
        capability.mkdir(parents=True)
        (capability / "gesso.jl").write_text("answer() = 1\n")
        (capability / "capability.json").write_text(json.dumps({"source": "gesso.jl"}))
        with patch.object(patch_broker_module, "__file__", str(fake_security / "patch_broker.py")):
            broker = PatchBroker(Path(self.tmp.name) / "default-registry-grants",
                                 {"deployed": str(capability)},
                                 {"deployed": [("adapters", "gesso.jl")]})
            result = broker.classify("deployed", "gesso.jl")
        self.assertEqual(result["class"], "protected")
        self.assertFalse(result["self_apply"])

    def test_R14_rechecks_neutral_classification_after_repair_autoapproval(self):
        (self.root / "tools").mkdir()
        source = self.root / "tools" / "value.py"
        source.write_text("VALUE = 'before'\n")
        (self.root / "test_value.py").write_text(
            "import unittest\nfrom tools.value import VALUE\n"
            "class CandidateTests(unittest.TestCase):\n"
            "    def test_value(self): self.assertEqual(VALUE, 'after')\n"
        )
        repair = RepairBroker(self.broker,
            {"project": python_test_recipe("fixed-unittest-v1", "-B", "-m", "unittest", "-v", "test_value")},
            {"project": [("tools", "tools")]})
        proposal = repair.propose({"target": "project", "changes": [{
            "path": "tools/value.py", "before_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "content": "VALUE = 'after'\n"}]}, "world-a")
        tested = repair.test(proposal["repair_id"], "world-a")
        self.assertEqual(tested["status"], "tested", tested)
        original_approve = self.broker.approve
        def approve_then_protect(*args, **kwargs):
            original_approve(*args, **kwargs)
            self.broker.protected_paths = (*self.broker.protected_paths, source.parent.resolve())
        with patch.object(self.broker, "approve", side_effect=approve_then_protect):
            with self.assertRaises(PermissionError):
                repair.apply(proposal["repair_id"], "world-a")
        self.assertEqual(source.read_text(), "VALUE = 'before'\n")
        self.broker.protected_paths = tuple(path for path in self.broker.protected_paths
                                            if path != source.parent.resolve())
        self.assertEqual(repair.apply(proposal["repair_id"], "world-a")["status"], "applied")
        self.assertEqual(source.read_text(), "VALUE = 'after'\n")

    def test_R15_json_list_prefixes_start_real_adapter_and_preserve_four_router_schemas(self):
        with tempfile.TemporaryDirectory(prefix="palette-json-repair-config-") as tmp:
            root = Path(tmp)
            workspace, state, router_state = root / "workspace", root / "state", root / "router-state"
            registry, target, home = root / "capabilities", root / "fixture", root / "home"
            for path in (workspace, state, router_state, registry, target, home):
                path.mkdir()
            (target / "main.jl").write_text("fixture() = 1\n")
            config_path = root / "repair-config.json"
            config_path.write_text(json.dumps({
                "test_recipes": {"fixture": python_test_recipe("fixed-fixture-v1", "-c", "pass")},
                "auto_apply_prefixes": {"fixture": [["adapters", "main.jl"]]},
            }))
            ceiling = root / "ceiling.json"
            ceiling.write_text("{}")
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "LANG": "C.UTF-8",
                   "PYTHONPATH": str(Path(__file__).resolve().parent), "PALETTE_REPO": str(Path(__file__).resolve().parents[2]),
                   "PALETTE_HOST": str(Path(__file__).resolve().parents[1] / "host/target/release/palette-host"),
                   "OPERATOR_WORKSPACE": str(workspace), "PALETTE_STATE_DIR": str(state),
                   "PALETTE_WORKSPACE_STATE_ROOT": str(router_state), "PALETTE_RUNTIME_ROOT": str(registry),
                   "PALETTE_PATCH_ROOTS": json.dumps({"fixture": str(target)}),
                   "PALETTE_CAPABILITY_CEILING": str(ceiling)}
            request = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}) + "\n"

            def list_tools(script, child_env):
                result = subprocess.run([sys.executable, str(Path(__file__).with_name(script))],
                    input=request, text=True, capture_output=True, timeout=30, env=child_env)
                self.assertEqual(result.returncode, 0, result.stderr)
                replies = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
                self.assertEqual(len(replies), 1, result.stdout)
                return replies[0]["result"]["tools"]

            adapter_tools = list_tools("operator_mcp.py", {**env, "PALETTE_REPAIR_CONFIG": str(config_path)})
            self.assertEqual({tool["name"] for tool in adapter_tools}, {"palette", "palette_control"})
            router_before = list_tools("operator_workspace_router.py", env)
            router_after = list_tools("operator_workspace_router.py", {**env, "PALETTE_REPAIR_CONFIG": str(config_path)})
            self.assertEqual({tool["name"] for tool in router_after},
                             {"palette", "palette_control", "palette_workspace", "palette_patch"})
            self.assertEqual(router_after, router_before)

    def test_R16_python_recipe_mounts_only_its_declared_toolchain(self):
        with tempfile.TemporaryDirectory(prefix="palette-python-toolchain-", dir=Path.home()) as tmp:
            toolchain = Path(tmp)
            binary = toolchain / "bin" / "python3"
            binary.parent.mkdir()
            binary.symlink_to(Path(sys.executable).resolve())
            code = "import sys; print('PYTHON_TOOLCHAIN_OK:' + str(sys.version_info.major))"

            declared = normalize_recipe(python_test_recipe(
                "relocated-python-v1", "-B", "-c", code,
                executable=str(binary), toolchain_root=str(toolchain),
                additional_read_roots=(sys.base_prefix,)))
            passed = run_isolated_test(self.root, declared)
            self.assertEqual(passed["status"], "passed", passed)
            self.assertIn("PYTHON_TOOLCHAIN_OK:3", passed["output"])

            omitted = normalize_recipe({
                "id": "relocated-python-without-root-v1",
                "argv": [str(binary), "-B", "-c", code],
                "read_roots": [str(Path(sys.base_prefix).resolve())],
            })
            failed = run_isolated_test(self.root, omitted)
            self.assertEqual(failed["status"], "failed", failed)
            self.assertNotEqual(failed["exit_code"], 0)
            self.assertIn("No such file or directory", failed["output"])


if __name__ == "__main__":
    unittest.main()
