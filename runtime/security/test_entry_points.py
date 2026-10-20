"""Production entry points run sessions and brokers through the Rust host.

The Python session, broker and their command-line bridge are a reference
implementation that the security suites run against the Rust host. No
production entry point may reach them, so a change made only to the Python
twin can never be what a deployment runs.
"""
import ast
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# What operators and clients start.
ENTRY_POINTS = (
    "serve_palette", "operator_mcp", "server_runtime", "generation_runtime",
    "operator_workspace_router", "install_operator_plugin", "palette_mcp_client", "prewarm_depot",
)
REFERENCE_ONLY = {"session", "broker", "session_cli", "host_adapter"}


def local_imports(module: str, local: set[str]) -> set[str]:
    tree = ast.parse((HERE / f"{module}.py").read_text(encoding="utf-8"))
    found = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            found |= {alias.name.split(".")[0] for alias in node.names}
        elif isinstance(node, ast.ImportFrom) and node.module and node.level == 0:
            found.add(node.module.split(".")[0])
        elif (isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
              and node.func.attr == "import_module" and node.args
              and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str)):
            found.add(node.args[0].value.split(".")[0])
    return found & local


class ProductionEntryPointsTest(unittest.TestCase):
    def test_no_entry_point_reaches_the_python_session_or_broker(self):
        local = {path.stem for path in HERE.glob("*.py") if not path.stem.startswith("test_")}
        for entry in ENTRY_POINTS:
            with self.subTest(entry=entry):
                self.assertIn(entry, local, "entry point list is stale")
                seen, stack = set(), [entry]
                while stack:
                    module = stack.pop()
                    if module not in seen:
                        seen.add(module)
                        stack.extend(local_imports(module, local))
                self.assertEqual(seen & REFERENCE_ONLY, set(),
                                 f"{entry} imports the reference-only Python host")

    def test_entry_points_name_the_rust_host(self):
        for entry in ("serve_palette", "operator_mcp", "server_runtime", "generation_runtime"):
            with self.subTest(entry=entry):
                text = (HERE / f"{entry}.py").read_text(encoding="utf-8")
                self.assertIn("runtime/host/target/release/palette-host", text)

    def test_every_part_reports_one_version(self):
        import tomllib
        from provisioning import palette_version
        version = palette_version(HERE.parents[1])
        self.assertEqual(tomllib.loads((HERE.parent / "host/Cargo.toml").read_text())["package"]["version"], version)
        # The release bundles take their version from the plugin manifests.
        for manifest in ("palette/plugin.json", "claude/manifest.json"):
            with self.subTest(manifest=manifest):
                self.assertEqual(json.loads((HERE.parent / "plugins" / manifest).read_text())["version"], version)


class PrivateInstanceTest(unittest.TestCase):
    def test_a_private_data_home_gets_its_own_package_store(self):
        with tempfile.TemporaryDirectory() as root:
            repo, data = Path(root, "repo"), Path(root, "data")
            router = repo / "runtime/security/operator_workspace_router.py"
            host = repo / "runtime/host/target/release/palette-host"
            for path in (router, host):
                path.parent.mkdir(parents=True)
            for name in ("Project.toml", "Manifest.toml"):
                (repo / name).write_text("")
            router.write_text("import json, os\nprint(json.dumps({k: v for k, v in os.environ.items() if k.startswith(('PALETTE_', 'OPERATOR_'))}))\n")
            host.write_text("")
            host.chmod(0o755)
            env = {k: v for k, v in os.environ.items() if not k.startswith(("PALETTE_", "OPERATOR_"))}
            env.update(PALETTE_REPO=str(repo), PALETTE_DATA_HOME=str(data))
            for extra, expected in (({}, str(data.resolve() / "packages")),
                                    ({"PALETTE_PACKAGE_DEPOT": "/srv/shared-packages"}, "/srv/shared-packages")):
                out = subprocess.run([sys.executable, str(HERE / "serve_palette.py")], env={**env, **extra},
                                     stdin=subprocess.DEVNULL, capture_output=True, text=True, check=True).stdout
                self.assertEqual(json.loads(out)["PALETTE_PACKAGE_DEPOT"], expected)


if __name__ == "__main__":
    unittest.main()
