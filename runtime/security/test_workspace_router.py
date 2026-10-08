#!/usr/bin/env python3
"""Integration proof for plugin-lab multi-workspace routing.

This is intentionally an adapter-level test: it exercises the real existing
operator_mcp child and therefore the real Rust Palette host.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
ROUTER = REPO / "runtime/security/operator_workspace_router.py"
HOST = REPO / "runtime/host/target/release/palette-host"
sys.path.insert(0, str(REPO / "runtime/security"))
from runtime_registry import RuntimeRegistry, RuntimeRegistryError  # noqa: E402


class WorkspaceRouterIntegration(unittest.TestCase):
    def setUp(self):
        self.assertTrue(HOST.is_file(), "Build release palette-host before running integration tests")

        self.tmp = tempfile.TemporaryDirectory(prefix="palette-workspace-router-")
        root = Path(self.tmp.name)
        self.workspace = root / "project"
        self.workspace.mkdir()
        package_store = root / "package-store"
        package_store.mkdir()
        runtime_root = root / "runtime-capabilities"
        runtime_root.mkdir()

        env = {
            **os.environ,
            "PALETTE_REPO": str(REPO),
            "PALETTE_HOST": str(HOST),
            "OPERATOR_WORKSPACE": str(self.workspace),
            "PALETTE_STATE_DIR": str(root / "legacy-state"),
            "PALETTE_WORKSPACE_STATE_ROOT": str(root / "registry"),
            "PALETTE_SCRATCH_ROOT": str(root / "scratch"),
            "PALETTE_PACKAGE_DEPOT": str(package_store),
            "PALETTE_RUNTIME_ROOT": str(runtime_root),
            "PALETTE_PROJECT_ROOTS": "{}",
            "PALETTE_PATCH_ROOTS": "{}",
            "PALETTE_READ_ROOTS": "",
        }
        for key in ("PALETTE_CAPABILITY_CEILING", "PALETTE_HOST_COMMANDS", "PALETTE_REPAIR_CONFIG",
                    "PALETTE_TASK_TOOLS", "PALETTE_TASK_ENV"):
            env.pop(key, None)
        self.env = env
        self.router_stderr = (root / "router-stderr.log").open("w+")
        self.start_router()

    def start_router(self):
        self.proc = subprocess.Popen(
            [sys.executable, str(ROUTER)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.router_stderr,
            text=True,
            env=self.env,
        )

    def tearDown(self):
        if getattr(self, "proc", None):
            try:
                self.proc.stdin.close()
            except Exception:
                pass
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
            if self.proc.stdout:
                self.proc.stdout.close()
        if getattr(self, "router_stderr", None):
            self.router_stderr.close()
        if getattr(self, "tmp", None):
            self.tmp.cleanup()

    def diagnostics(self):
        self.router_stderr.flush()
        return Path(self.router_stderr.name).read_text(errors="replace")[-12000:]

    def call(self, name, arguments, *, error=False):
        request = {
            "jsonrpc": "2.0",
            "id": "test",
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }
        self.proc.stdin.write(json.dumps(request) + "\n")
        self.proc.stdin.flush()
        with selectors.DefaultSelector() as selector:
            selector.register(self.proc.stdout, selectors.EVENT_READ)
            ready = selector.select(timeout=240)
            self.assertTrue(ready, "MCP reply deadline exceeded\n" + self.diagnostics())
        raw = self.proc.stdout.readline()
        self.assertTrue(raw, "Router exited without reply\n" + self.diagnostics())
        frame = json.loads(raw)
        self.assertIn("result", frame, frame)
        reply = frame["result"]
        self.assertEqual(bool(reply.get("isError")), error, f"{reply}\n{self.diagnostics()}")
        return json.loads(reply["content"][0]["text"])

    def start_without_prepare(self, workspace_id=None, context_id=None):
        arguments = {"action":"restart", "prepare":False}
        if workspace_id:
            arguments["workspace_id"] = workspace_id
        if context_id:
            arguments["context_id"] = context_id
        self.call("palette_control", arguments)

    def test_startup_failure_keeps_its_cause_through_router(self):
        self.proc.stdin.close(); self.proc.wait(timeout=60); self.proc.stdout.close()
        self.env["PALETTE_HOST"] = str(Path(self.tmp.name) / "missing-host")
        self.start_router()
        result = self.call("palette", {"context_id": "failed-startup", "code": "1"}, error=True)
        self.assertIn("FileNotFoundError", result["error"])
        self.assertTrue(result["workspace_id"])

    def test_D01_overlapping_workspace_is_rejected_before_tools(self):
        root = Path(self.tmp.name)
        alias = root / "source-alias"
        alias.symlink_to(REPO, target_is_directory=True)
        for workspace in (REPO, REPO.parent, alias, root, root / "registry", root / "registry/child", root / "legacy-state", root / "legacy-state.receipts"):
            with self.subTest(workspace=workspace):
                result = subprocess.run([sys.executable, str(ROUTER)],
                    input=json.dumps({"jsonrpc":"2.0", "id":1, "method":"tools/list"})+"\n",
                    capture_output=True, text=True, timeout=60,
                    env={**self.env, "OPERATOR_WORKSPACE":str(workspace)})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("overlap", result.stderr)

    def test_D03_registry_cannot_redirect_a_worker_mount(self):
        self.call("palette_workspace", {"action":"create", "workspace_id":"registry-check", "scope":"open"})
        path = Path(self.env["PALETTE_WORKSPACE_STATE_ROOT"]) / "registry.json"
        registry = json.loads(path.read_text())
        registry["workspaces"]["registry-check"].pop("project_root_id")
        path.write_text(json.dumps(registry))
        self.assertEqual(self.call("palette", {"workspace_id":"registry-check", "code":"pwd()"})["data"], str(self.workspace))
        registry = json.loads(path.read_text())
        registry["workspaces"]["registry-check"]["workspace_dir"] = str(Path(self.tmp.name))
        path.write_text(json.dumps(registry))
        result = self.call("palette", {"workspace_id":"registry-check", "code":"1"}, error=True)
        self.assertIn("registry paths", result["error"])

    def test_D04_project_root_allowlist_rejects_protected_paths(self):
        alias = Path(self.tmp.name) / "runtime-alias"
        alias.symlink_to(REPO, target_is_directory=True)
        for path in (REPO, alias, self.workspace, Path.home() / ".julia"):
            with self.subTest(path=path):
                result = subprocess.run(
                    [sys.executable, str(ROUTER)],
                    input=json.dumps({"jsonrpc":"2.0", "id":1, "method":"tools/list"})+"\n",
                    capture_output=True, text=True, timeout=60,
                    env={**self.env, "PALETTE_PROJECT_ROOTS":json.dumps({"unsafe":str(path)})},
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("overlap", result.stderr)

    def test_C04_configured_project_roots_share_files_and_isolate_worlds(self):
        shadow = Path(self.tmp.name) / "shadow-project"
        shadow.mkdir()
        self.proc.stdin.close(); self.proc.wait(timeout=60); self.proc.stdout.close()
        self.env["PALETTE_PROJECT_ROOTS"] = json.dumps({"shadow":str(shadow)})
        self.start_router()

        def create(workspace_id, scope, context_id=None):
            args = {"action":"create", "workspace_id":workspace_id, "scope":scope, "project_root_id":"shadow"}
            if context_id:
                args["context_id"] = context_id
            return self.call("palette_workspace", args)

        def palette(workspace_id, context_id, code):
            return self.call("palette", {"workspace_id":workspace_id, "context_id":context_id, "code":code})

        listing = self.call("palette_workspace", {"action":"list"})
        self.assertIn({"project_root_id":"shadow", "workspace_dir":str(shadow)}, listing["available_project_roots"])
        first = create("shadow-a", "thread", "ctx-shadow-a")
        self.assertEqual(first["workspace_dir"], str(shadow))
        self.assertEqual(first["project_root_id"], "shadow")
        self.start_without_prepare("shadow-a", "ctx-shadow-a")
        self.assertEqual(palette("shadow-a", "ctx-shadow-a", 'write("shared.txt", "from-a"); shadow_value=73; pwd()')["data"], str(shadow))
        self.assertEqual((shadow / "shared.txt").read_text(), "from-a")

        create("shadow-b", "thread", "ctx-shadow-b")
        self.start_without_prepare("shadow-b", "ctx-shadow-b")
        self.assertFalse(palette("shadow-b", "ctx-shadow-b", "isdefined(@__MODULE__, :shadow_value)")["data"])
        self.assertEqual(palette("shadow-b", "ctx-shadow-b", 'read("shared.txt", String)')["data"], "from-a")

        create("shadow-project", "project", "ctx-project-a")
        self.start_without_prepare("shadow-project", "ctx-project-a")
        self.call("palette_workspace", {
            "action":"attach", "workspace_id":"shadow-project", "context_id":"ctx-project-b",
        })
        palette("shadow-project", "ctx-project-a", "project_value=19")
        self.assertEqual(palette("shadow-project", "ctx-project-b", "project_value")["data"], 19)

        self.start_without_prepare()
        default = self.call("palette", {"code":"pwd()"})
        self.assertEqual(default["data"], str(self.workspace))
        self.assertTrue(default["legacy_shared"])
        unknown = self.call("palette_workspace", {
            "action":"create", "workspace_id":"unknown-root", "scope":"open", "project_root_id":"/tmp",
        }, error=True)
        self.assertIn("Unknown project_root_id", unknown["error"])
        raw_path = self.call("palette_workspace", {
            "action":"create", "workspace_id":"raw-path", "scope":"open", "project_dir":str(shadow),
        }, error=True)
        self.assertIn("Invalid tool arguments", raw_path["error"])

    def test_portable_and_compatibility_installation(self):
        for fmt in ("portable", "codex"):
            with self.subTest(format=fmt):
                plugin = Path(self.tmp.name) / fmt
                command = [sys.executable, str(REPO / "runtime/security/install_operator_plugin.py"), "--format", fmt,
                    "--plugin-dir", str(plugin), "--workspace-dir", str(self.workspace), "--bin-dir", str(Path(self.tmp.name) / "bin")]
                install_env = {**os.environ, "XDG_RUNTIME_DIR": str(Path(self.tmp.name) / "runtime")}
                subprocess.run(command, capture_output=True, text=True, check=True, env=install_env)
                config_path = plugin / ("mcp.json" if fmt == "portable" else ".mcp.json")
                config = json.loads(config_path.read_text())
                config["mcpServers"]["palette"]["env"]["INSTALL_TEST_PRESERVED"] = "yes"
                config["INSTALL_TEST_ROOT_PRESERVED"] = "yes"
                config_path.write_text(json.dumps(config))
                subprocess.run(command, capture_output=True, text=True, check=True, env=install_env)
                entry = json.loads(config_path.read_text())["mcpServers"]["palette"]
                self.assertEqual(entry["env"]["INSTALL_TEST_PRESERVED"], "yes")
                self.assertEqual(json.loads(config_path.read_text())["INSTALL_TEST_ROOT_PRESERVED"], "yes")
                self.assertEqual(entry["command"], sys.executable)
                self.assertEqual(Path(entry["args"][0]), REPO / "runtime/security/palette_mcp_client.py")
                self.assertEqual(entry["env"]["PALETTE_SERVICE"], "palette.service")
                self.assertIn("PALETTE_SOCKET", entry["env"])
        wrapper = Path(self.tmp.name) / "bin/palette-mcp"
        self.assertIn(str(REPO / "runtime/security/palette_mcp_client.py"), wrapper.read_text())

    def test_thread_project_open_and_revival(self):
        # Thread A and B are truly separate worlds.
        a = self.call("palette_workspace", {
            "action": "create",
            "workspace_id": "thread-a",
            "scope": "thread",
            "context_id": "ctx-a",
        })
        self.assertTrue(a["active_for_context"])
        self.call("palette", {"context_id": "ctx-a", "code": "x=1"})
        self.assertEqual(
            self.call("palette", {"context_id": "ctx-a", "code": "x"})["data"],
            1,
        )

        self.call("palette_workspace", {
            "action": "create",
            "workspace_id": "thread-b",
            "scope": "thread",
            "context_id": "ctx-b",
        })
        self.assertFalse(
            self.call(
                "palette",
                {"context_id": "ctx-b", "code": "isdefined(@__MODULE__, :x)"},
            )["data"]
        )
        self.call("palette", {"context_id": "ctx-b", "code": "x=2"})
        self.assertEqual(
            self.call("palette", {"context_id": "ctx-a", "code": "x"})["data"],
            1,
        )

        # Thread scope fails closed for the wrong context.
        denied = self.call(
            "palette",
            {
                "workspace_id": "thread-a",
                "context_id": "ctx-b",
                "code": "x",
            },
            error=True,
        )
        self.assertIn("PermissionError", denied["error"])

        # Project scope intentionally shares a world across contexts.
        self.call("palette_workspace", {
            "action": "create",
            "workspace_id": "project-shared",
            "scope": "project",
            "context_id": "ctx-a",
        })
        self.call("palette_workspace", {
            "action": "attach",
            "workspace_id": "project-shared",
            "context_id": "ctx-b",
        })
        self.call("palette", {
            "workspace_id": "project-shared",
            "context_id": "ctx-a",
            "code": "p=7",
        })
        self.assertEqual(
            self.call("palette", {
                "workspace_id": "project-shared",
                "context_id": "ctx-b",
                "code": "p",
            })["data"],
            7,
        )

        # Open scope is deliberately attachable by either context.
        self.call("palette_workspace", {
            "action": "create",
            "workspace_id": "open-lab",
            "scope": "open",
            "context_id": "ctx-a",
        })
        self.call("palette_workspace", {
            "action": "attach",
            "workspace_id": "open-lab",
            "context_id": "ctx-b",
        })
        self.call("palette", {
            "workspace_id": "open-lab",
            "context_id": "ctx-a",
            "code": "o=11",
        })
        self.assertEqual(
            self.call("palette", {
                "workspace_id": "open-lab",
                "context_id": "ctx-b",
                "code": "o",
            })["data"],
            11,
        )

        # Closing only parks the process; explicit routing must select that world.
        closed = self.call("palette_workspace", {
            "action": "close",
            "workspace_id": "thread-a",
            "context_id": "ctx-a",
        })
        self.assertTrue(closed["state_retained"])
        self.assertEqual(
            self.call("palette", {"workspace_id":"thread-a", "context_id": "ctx-a", "code": "x"})["data"],
            1,
        )


        # Routed restart must not restart another world.
        epoch_b = self.call("palette", {"workspace_id":"thread-b", "context_id":"ctx-b", "code":"x"})["epoch"]
        self.call("palette_control", {"action":"restart", "workspace_id":"thread-a", "context_id":"ctx-a"})
        self.assertEqual(self.call("palette", {"workspace_id":"thread-a", "context_id":"ctx-a", "code":"x"})["data"], 1)
        self.assertEqual(self.call("palette", {"workspace_id":"thread-b", "context_id":"ctx-b", "code":"x"})["epoch"], epoch_b)
        scratch_a = self.call("palette", {"workspace_id":"thread-a", "context_id":"ctx-a", "code":"pwd()", "ephemeral":True})["data"]
        scratch_b = self.call("palette", {"workspace_id":"thread-b", "context_id":"ctx-b", "code":"pwd()", "ephemeral":True})["data"]
        self.assertNotEqual(scratch_a, scratch_b)
        self.call("palette_workspace", {"action":"close", "workspace_id":"thread-b", "context_id":"ctx-b"})
        legacy = self.call("palette", {"code":"legacy_value=31"})
        self.assertTrue(legacy["legacy_shared"])
        self.assertEqual(legacy["workspace_id"], "default")
        self.assertEqual(self.call("palette_control", {"action":"status"})["state_dir"], self.env["PALETTE_STATE_DIR"])
        self.call("palette_workspace", {"action":"attach", "workspace_id":"thread-a", "context_id":"ctx-a"})
        self.proc.stdin.close()
        self.assertEqual(self.proc.wait(timeout=60), 0)
        self.proc.stdout.close()
        self.start_router()
        self.assertEqual(self.call("palette", {"context_id":"ctx-a", "code":"x"})["data"], 1)
        self.assertEqual(self.call("palette", {"code":"legacy_value"})["data"], 31)

    def test_two_routers_cannot_own_one_world(self):
        self.call("palette_workspace", {"action":"create", "workspace_id":"shared", "scope":"project", "context_id":"a"})
        self.call("palette", {"context_id":"a", "code":"shared_value=77"})
        first = self.proc
        self.start_router()
        second = self.proc
        try:
            denied = self.call("palette", {"workspace_id":"shared", "context_id":"b", "code":"shared_value"}, error=True)
            self.assertIn("already owned", denied["error"])
            self.proc = first
            self.call("palette_workspace", {"action":"close", "workspace_id":"shared", "context_id":"a"})
            self.proc = second
            self.assertEqual(self.call("palette", {"workspace_id":"shared", "context_id":"b", "code":"shared_value"})["data"], 77)
            # A different canonical project must fail the project scope check.
            self.call("palette_workspace", {"action":"close", "workspace_id":"shared", "context_id":"b"})
            second.stdin.close()
            second.wait(timeout=60)
            second.stdout.close()
            other = Path(self.tmp.name) / "other-project"
            other.mkdir()
            self.env = {**self.env, "OPERATOR_WORKSPACE":str(other)}
            self.start_router()
            denied = self.call("palette_workspace", {"action":"attach", "workspace_id":"shared", "context_id":"a"}, error=True)
            self.assertIn("PermissionError", denied["error"])
        finally:
            first.stdin.close()
            first.wait(timeout=60)
            first.stdout.close()
            if second.poll() is None:
                second.stdin.close()
                second.wait(timeout=60)
                second.stdout.close()

    def test_patch_approval_uses_mcp_client_response(self):
        # Real server/client elicitation, not a model-supplied approve argument.
        target = Path(self.tmp.name) / "target"
        target.mkdir()
        self.proc.stdin.close()
        self.proc.wait(timeout=60)
        self.proc.stdout.close()
        self.env["PALETTE_PATCH_ROOTS"] = json.dumps({"test":str(target)})
        self.start_router()
        def exchange(value):
            self.proc.stdin.write(json.dumps(value)+"\n")
            self.proc.stdin.flush()
            with selectors.DefaultSelector() as selector:
                selector.register(self.proc.stdout, selectors.EVENT_READ)
                self.assertTrue(selector.select(timeout=30))
            return json.loads(self.proc.stdout.readline())
        response = exchange({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"capabilities":{"elicitation":{"form":{}}}}})
        self.assertIn("result",response)
        prepared = self.call("palette_patch", {"action":"prepare", "target":"test", "changes":[{"path":"new.txt", "before_sha256":None, "content":"approved content\n"}]})
        prompt = exchange({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"palette_patch", "arguments":{"action":"apply", "request_id":prepared["request_id"]}}})
        self.assertEqual(prompt["method"], "elicitation/create")
        result = exchange({"jsonrpc":"2.0", "id":prompt["id"], "result":{"action":"accept", "content":{"confirm":True}}})
        receipt = json.loads(result["result"]["content"][0]["text"])
        self.assertEqual(receipt["status"],"applied",receipt)
        self.assertEqual(receipt["granted_by"],"mcp_client_confirmed_user")
        self.assertEqual((target/"new.txt").read_text(),"approved content\n")

    def test_C02_runtime_discovery_invoke_refresh_and_frozen_ceiling(self):
        runtime_root = Path(self.tmp.name) / "runtime-capabilities"
        ceiling_path = Path(self.tmp.name) / "ceiling.json"
        runtime_root.mkdir(exist_ok=True)
        ceiling = {"host_request": {"allowed_types": ["palette.runtime"]},
                   "package_management": {"allowed_packages": ["Parsers"], "offline": True},
                   "network_access": {"allowed": False, "allowed_hosts": []}}
        ceiling_path.write_text(json.dumps(ceiling))

        def install(identifier, version, source, *, dependencies=None, requires=None):
            target = runtime_root / identifier
            target.mkdir(exist_ok=True)
            (target / "main.jl").write_text(source)
            (target / "capability.json").write_text(json.dumps({
                "id": identifier, "version": version, "source": "main.jl", "entrypoint": "runtime_capability",
                "dependencies": dependencies or [], "requires": requires or [], "description": identifier,
            }))

        install("echo", "1.0", 'function runtime_capability(args); get(args, "n", 0) + 2; end\n',
                dependencies=[])
        install("network-tool", "1.0", "function runtime_capability(args); :unused; end\n", requires=["network_access"])
        install("needs-package", "1.0", "function runtime_capability(args); :unused; end\n",
                dependencies=["PaletteMissingDependencyForRuntimeTest"])

        self.proc.stdin.close(); self.proc.wait(timeout=60); self.proc.stdout.close()
        self.env["PALETTE_RUNTIME_ROOT"] = str(runtime_root)
        self.env["PALETTE_CAPABILITY_CEILING"] = str(ceiling_path)
        self.env["PALETTE_PACKAGE_DEPOT"] = str(Path(self.tmp.name) / "package-store")
        self.start_router()

        def schemas():
            request = {"jsonrpc": "2.0", "id": "schemas", "method": "tools/list"}
            self.proc.stdin.write(json.dumps(request) + "\n"); self.proc.stdin.flush()
            with selectors.DefaultSelector() as selector:
                selector.register(self.proc.stdout, selectors.EVENT_READ)
                self.assertTrue(selector.select(timeout=30), "tools/list response deadline exceeded")
            frame = json.loads(self.proc.stdout.readline())
            return {tool["name"]: tool["inputSchema"] for tool in frame["result"]["tools"]}

        before = schemas()
        self.assertEqual(set(before), {"palette", "palette_control", "palette_workspace", "palette_patch"})
        found = self.call("palette", {"code": 'Palette.runtime("discover")'})["data"]
        echo = next(item for item in found["available_capabilities"] if item["id"] == "echo")
        self.assertEqual(echo["state"], "active")
        self.assertIn("PaletteMissingDependencyForRuntimeTest", found["missing_dependencies"])
        self.assertIn("network-tool", found["unauthorized_capabilities"])
        first_generation = found["capability_generation"]
        self.assertFalse(self.call("palette", {"code": 'Base.find_package("Parsers") !== nothing'})["data"])
        self.assertEqual(found["authorized_ceiling"]["network_access"]["allowed"], False)
        initial = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"echo", "arguments"=>Dict("n"=>5)))'})
        self.assertEqual(initial["data"]["result"], 7)
        denied = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"network-tool"))'}, error=True)
        self.assertIn("outside the operator ceiling", denied["error"])

        install("echo", "2.0", 'function runtime_capability(args); get(args, "n", 0) + 20; end\n',
                dependencies=[])
        install("new-cap", "1.0", 'using Parsers; function runtime_capability(args); Parsers.parse(Int, get(args, "text", "42")); end\n',
                dependencies=["Parsers"])
        ceiling["network_access"] = {"allowed": True, "allowed_hosts": ["example.invalid"]}
        ceiling_path.write_text(json.dumps(ceiling))
        available = self.call("palette", {"code": 'Palette.runtime("discover")'})["data"]
        updated = next(item for item in available["available_capabilities"] if item["id"] == "echo")
        self.assertEqual(updated["state"], "available_update")
        self.assertEqual(next(item for item in available["available_capabilities"] if item["id"] == "new-cap")["state"], "available")
        self.assertNotEqual(available["available_generation"], first_generation)
        self.assertEqual(available["authorized_ceiling"]["network_access"]["allowed"], False)
        self.assertIn("network-tool", available["unauthorized_capabilities"])
        still_old = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"echo", "arguments"=>Dict("n"=>5)))'})
        self.assertEqual(still_old["data"]["result"], 7)

        refreshed = self.call("palette", {"code": 'keep_me=44; requested=Palette.runtime("refresh"); '
            'view=Palette.runtime("discover"); (requested["deferred"], view["capability_generation"], view["available_generation"])'})
        observation = refreshed["runtime_refresh"]
        self.assertTrue(observation["success"], observation)
        self.assertNotEqual(observation["epoch"], observation["previous_epoch"])
        self.assertTrue(observation["snapshot"]["covers_completed_turn"], observation)
        self.assertGreaterEqual(observation["revival"]["saved_call"], refreshed["call"])
        self.assertTrue(refreshed["data"][0])
        self.assertEqual(refreshed["data"][1], first_generation)
        self.assertNotEqual(refreshed["data"][2], first_generation)
        self.assertEqual(self.call("palette", {"code": "keep_me"})["data"], 44)
        changed = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"echo", "arguments"=>Dict("n"=>5)))'})
        self.assertEqual(changed["data"]["result"], 25)
        missing = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"new-cap"))'}, error=True)
        self.assertIn("missing Julia dependencies", missing["error"])
        installed = self.call("palette", {"code": 'package_receipt=Api.request_capability("package_management", Dict("name"=>"Parsers")); package_receipt'})
        self.assertTrue(installed["data"]["approved"], installed)
        new_cap = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"new-cap"))'})
        self.assertEqual(new_cap["data"]["result"], 42)
        denied_package = self.call("palette", {"code": 'Api.request_capability("package_management", Dict("name"=>"DefinitelyNotAllowed"))'})
        self.assertFalse(denied_package["data"]["approved"], denied_package)
        self.assertIn("allow", denied_package["data"]["reason"].lower())
        receipt_file = Path(self.env["PALETTE_STATE_DIR"] + ".receipts") / "receipts.jsonl"
        receipts = [json.loads(line) for line in receipt_file.read_text().splitlines()]
        package_receipts = [item for item in receipts if item.get("category") == "package_management"]
        self.assertTrue(any(item["approved"] and item["params"]["name"] == "Parsers" for item in package_receipts), package_receipts)
        self.assertTrue(any(not item["approved"] and item["params"]["name"] == "DefinitelyNotAllowed" for item in package_receipts), package_receipts)
        prior_epoch = observation["epoch"]
        restarted = self.call("palette_control", {"action": "restart", "restore": True})
        self.assertNotEqual(restarted["session"]["epoch"], prior_epoch)
        continued = self.call("palette", {"code": 'keep_me + Palette.runtime("invoke", Dict("id"=>"new-cap"))["result"]'})
        self.assertEqual(continued["data"], 86)
        persisted_receipts = [json.loads(line) for line in receipt_file.read_text().splitlines()]
        self.assertTrue({item["receipt_id"] for item in package_receipts}.issubset(
            {item["receipt_id"] for item in persisted_receipts}))
        installed_discovery = self.call("palette", {"code": 'Palette.runtime("discover")'})["data"]
        self.assertNotIn("Parsers", installed_discovery["missing_dependencies"])
        still_denied = self.call("palette", {"code": 'Palette.runtime("invoke", Dict("id"=>"network-tool"))'}, error=True)
        self.assertIn("outside the operator ceiling", still_denied["error"])
        after = schemas()
        self.assertEqual(after, before)

    def test_C03_runtime_registry_rejects_manifest_authority_and_source_escape(self):
        root = Path(self.tmp.name) / "registry-negative"
        workspace = Path(self.tmp.name) / "registry-negative-workspace"
        state = Path(self.tmp.name) / "registry-negative-state"
        workspace.mkdir(); state.mkdir(); root.mkdir()
        cap = root / "bad-authority"
        cap.mkdir()
        (cap / "main.jl").write_text("function safe_entry(args); 1; end\n")
        (cap / "capability.json").write_text(json.dumps({"id": "bad-authority", "version": "1",
            "source": "main.jl", "entrypoint": "safe_entry", "ceiling": {"network_access": {"allowed": True}}}))
        with self.assertRaisesRegex(RuntimeRegistryError, "forbidden field"):
            RuntimeRegistry(root, {}, workspace=workspace, state_dir=state)

        (cap / "capability.json").write_text(json.dumps({"id": "bad-authority", "version": "1",
            "source": "../outside.jl", "entrypoint": "safe_entry"}))
        (root / "outside.jl").write_text("function safe_entry(args); 1; end\n")
        with self.assertRaisesRegex(RuntimeRegistryError, "inside its directory"):
            RuntimeRegistry(root, {}, workspace=workspace, state_dir=state)

    def test_C01_runtime_registry_toolchain_checks_nested_fixed_artifacts(self):
        root = Path(self.tmp.name) / "registry-toolchains"
        workspace = Path(self.tmp.name) / "registry-toolchains-workspace"
        state = Path(self.tmp.name) / "registry-toolchains-state"
        cwd = Path(self.tmp.name) / "configured-tool"
        root.mkdir(); workspace.mkdir(); state.mkdir(); cwd.mkdir()
        executable = cwd / "runner"
        executable.write_text("#!/bin/sh\nexit 0\n")
        executable.chmod(0o755)
        artifact = cwd / "model.bin"
        argv = [str(executable), "--fixed-arg=--model=" + str(artifact)]
        ceiling = {"host_command": {"commands": {"model": {"argv": argv, "cwd": str(cwd)}}}}
        registry = RuntimeRegistry(root, ceiling, workspace=workspace, state_dir=state)
        self.assertEqual(next(item for item in registry._toolchains() if item["name"] == "model")["status"], "missing")
        artifact.write_bytes(b"model")
        self.assertEqual(next(item for item in registry._toolchains() if item["name"] == "model")["status"], "ready")


    def test_live_limit_and_router_crash_recovery(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=60)
        self.proc.stdout.close()
        self.env["PALETTE_MAX_LIVE_WORKSPACES"] = "1"
        self.start_router()
        a = self.call("palette_workspace", {"action":"create", "workspace_id":"crash-a", "context_id":"a"})
        self.call("palette", {"context_id":"a", "code":"crash_value=89"})
        self.call("palette_workspace", {"action":"create", "workspace_id":"limit-b", "context_id":"b"})
        denied = self.call("palette", {"context_id":"b", "code":"1"}, error=True)
        self.assertIn("Live workspace limit", denied["error"])
        status = self.call("palette_workspace", {"action":"status", "workspace_id":"crash-a", "context_id":"a"})
        adapter = status["local_adapter_pid"]
        os.kill(adapter, signal.SIGSTOP)
        try:
            old = self.proc
            old.kill()
            old.wait(timeout=10)
            old.stdin.close()
            old.stdout.close()
            self.start_router()
            denied = self.call("palette", {"workspace_id":"crash-a", "context_id":"a", "code":"crash_value"}, error=True)
            self.assertIn("already owned", denied["error"])
        finally:
            os.kill(adapter, signal.SIGCONT)
        # Await the concrete kernel lock-release event, not elapsed time.
        lock = Path(a["state_dir"]).with_name("crash-a.owner.lock")
        command = "import fcntl,sys; f=open(sys.argv[1],'a+'); fcntl.flock(f,fcntl.LOCK_EX); print('released',flush=True)"
        waiter = subprocess.Popen([sys.executable,"-c",command,str(lock)],stdout=subprocess.PIPE,text=True)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(waiter.stdout, selectors.EVENT_READ)
                self.assertTrue(selector.select(timeout=60),"Old adapter failed to release ownership")
            self.assertEqual(waiter.stdout.readline().strip(),"released")
            self.assertEqual(waiter.wait(timeout=10),0)
        finally:
            if waiter.poll() is None:
                waiter.kill()
                waiter.wait()
            waiter.stdout.close()
        self.assertEqual(self.call("palette", {"workspace_id":"crash-a", "context_id":"a", "code":"crash_value"})["data"],89)
        self.call("palette_workspace", {"action":"close", "workspace_id":"crash-a", "context_id":"a"})
        self.assertEqual(self.call("palette", {"context_id":"b", "code":"42"})["data"],42)


if __name__ == "__main__":
    unittest.main()
