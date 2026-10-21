"""Every part of Palette reports one version.

No Python runs in production; what remains in runtime/security is test support.
"""
import json
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))


class ProductionEntryPointsTest(unittest.TestCase):
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
