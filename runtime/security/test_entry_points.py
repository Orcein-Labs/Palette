"""Production entry points run sessions and brokers through the Rust host.

The Python session, broker and their command-line bridge are a reference
implementation that the security suites run against the Rust host. No
production entry point may reach them, so a change made only to the Python
twin can never be what a deployment runs.
"""
import ast
import json
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# What operators and clients start.
ENTRY_POINTS = ("generation_runtime", "install_operator_plugin", "palette_mcp_client", "prewarm_depot")
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
        for entry in ("generation_runtime",):
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


if __name__ == "__main__":
    unittest.main()
