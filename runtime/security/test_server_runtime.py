"""Server profile and real stdio launcher boundaries."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from server_runtime import environment, load_profile

REPO = Path(__file__).resolve().parents[2]
LAUNCHER = Path(__file__).with_name("server_runtime.py")


class ServerRuntimeTests(unittest.TestCase):
    def test_S01_server_profile_and_real_mcp_entrypoint(self):
        with tempfile.TemporaryDirectory(prefix="palette-server-") as tmp:
            root = Path(tmp)
            profile = root / "config/profile.json"
            ceiling = root / "config/ceiling.json"
            ceiling.parent.mkdir()
            ceiling.write_text(json.dumps({"host_request": {"allowed_types": ["palette.runtime"]}}))
            command = [sys.executable, str(LAUNCHER), "init", "--profile", str(profile),
                       "--repo", str(REPO), "--data", str(root / "data"),
                       "--depot", str(root / "depot"), "--ceiling", str(ceiling),
                       "--runtime-root", str(root / "capabilities")]
            result = subprocess.run(command, text=True, capture_output=True, check=True, timeout=15)
            self.assertFalse(json.loads(result.stdout)["edge_required"])
            paths = load_profile(profile)
            env = environment(paths)
            self.assertEqual(env["PALETTE_PACKAGE_DEPOT"], str(root / "data/packages"))
            self.assertEqual(env["OPERATOR_WORKSPACE"], str(root / "data/workspace"))
            injected = {**os.environ, "OPENAI_API_KEY": "fixture-secret", "PALETTE_PATCH_ROOTS": '{"unsafe":"/"}'}
            listing = subprocess.run([sys.executable, str(LAUNCHER), "launch", "--profile", str(profile)],
                input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}) + "\n",
                text=True, capture_output=True, check=True, timeout=15, env=injected)
            self.assertEqual({t["name"] for t in json.loads(listing.stdout)["result"]["tools"]},
                             {"palette", "palette_control", "palette_workspace", "palette_patch"})
            again = subprocess.run(command, text=True, capture_output=True, timeout=15)
            self.assertNotEqual(again.returncode, 0)
            client = subprocess.run([sys.executable, str(LAUNCHER), "client", "--profile", str(profile),
                                     "--ssh-target", "palette@server"],
                                    text=True, capture_output=True, check=True, timeout=15)
            entry = json.loads(client.stdout)["mcpServers"]["palette"]
            self.assertEqual(entry["args"][:3], ["-T", "--", "palette@server"])
            self.assertEqual(len(entry["args"]), 4)
            self.assertIn(str(profile), entry["args"][3])

    def test_S02_profile_rejects_writable_authority_overlap(self):
        with tempfile.TemporaryDirectory(prefix="palette-server-denial-") as tmp:
            root = Path(tmp)
            profile = root / "profile.json"
            base = {"format": 1, "repo": str(REPO), "data": str(root / "data"),
                    "depot": str(root / "depot"), "ceiling": str(root / "ceiling.json"),
                    "runtime_root": str(root / "capabilities")}
            for field in ("repo", "depot", "ceiling", "runtime_root"):
                with self.subTest(field=field):
                    profile.write_text(json.dumps({**base, field: str(root / "data/hidden")}))
                    with self.assertRaises(PermissionError):
                        load_profile(profile)
            profile.write_text(json.dumps(base))
            previous = os.environ.get("PALETTE_HOST_COMMANDS")
            os.environ["PALETTE_HOST_COMMANDS"] = "fixture-secret"
            try:
                self.assertNotIn("PALETTE_HOST_COMMANDS", environment(load_profile(profile)))
            finally:
                if previous is None:
                    os.environ.pop("PALETTE_HOST_COMMANDS")
                else:
                    os.environ["PALETTE_HOST_COMMANDS"] = previous


if __name__ == "__main__":
    unittest.main()
