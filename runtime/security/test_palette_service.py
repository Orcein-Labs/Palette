import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import palette_service as service


REPO = Path(__file__).resolve().parents[2]


class PaletteServiceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="palette-service-")
        self.root = Path(self.tmp.name)
        self.runtime = self.root / "runtime with space"
        self.runtime.mkdir()
        self.repo = self.root / "repo with space"
        self.repo.mkdir()
        (self.repo / "runtime/security").mkdir(parents=True)
        (self.repo / "runtime/security/operator_workspace_router.py").write_text("# fixture\n")
        host = self.repo / "runtime/host/target/release/palette-host"
        host.parent.mkdir(parents=True)
        host.write_text("#!/bin/sh\n")
        host.chmod(0o700)
        self.workspace = self.root / "workspace's name"

    def tearDown(self):
        self.tmp.cleanup()

    def test_render_daemon_unit_quotes_paths_and_owns_runtime_socket(self):
        unit = service.render_palette_unit(self.repo, self.workspace, self.runtime / "palette/daemon.sock", "/usr/bin/python with space")
        self.assertIn('ExecStart="/usr/bin/python with space"', unit)
        self.assertIn('"--socket"', unit)
        self.assertIn('workspace\'s name', unit)
        self.assertIn('RuntimeDirectory=palette\nRuntimeDirectoryMode=0700', unit)
        self.assertIn('Restart=on-failure', unit)
        self.assertIn('StandardOutput=journal', unit)
        self.assertIn('UMask=0077', unit)
        self.assertNotIn("$", unit.replace("\\x24", ""))
        self.assertIn("%%", service.unit_quote("100%"))
        with self.assertRaises(ValueError):
            service.unit_quote("bad\nvalue")

    def test_tunnel_unit_requires_existing_operator_client_and_private_env(self):
        client = self.root / "tunnel client"
        client.write_text("#!/bin/sh\n")
        client.chmod(0o700)
        credentials = self.root / "operator credentials.env"
        credentials.write_text("TOKEN=operator-owned\n")
        credentials.chmod(0o600)
        profile_dir = self.root / "profiles with spaces"
        profile_dir.mkdir()
        (profile_dir / "existing profile.yaml").write_text("config_version: 1\n")
        sock = self.runtime / "palette" / "daemon.sock"
        unit = service.render_tunnel_unit(client, "existing profile", credentials,
                                          profile_dir=profile_dir, sock=sock)
        self.assertIn('"run" "--profile" "existing profile" "--profile-dir"', unit)
        self.assertIn(service.unit_quote(profile_dir.resolve()), unit)
        self.assertIn("Environment=PALETTE_SOCKET=" + service.unit_quote(sock.resolve()), unit)
        self.assertIn("UnsetEnvironment=CONTROL_PLANE_API_KEY OPENAI_API_KEY OPENAI_ADMIN_KEY PALETTE_HOST_COMMANDS", service.render_palette_unit(
            self.repo, self.workspace, self.runtime / "palette/daemon.sock"))
        self.assertIn('EnvironmentFile=' + service.unit_path(credentials.resolve()), unit)
        self.assertNotIn('EnvironmentFile="', unit)
        self.assertIn('Restart=always', unit)
        credentials.chmod(0o640)
        with self.assertRaisesRegex(ValueError, "mode 0600"):
            service.render_tunnel_unit(client, "existing", credentials)
        credentials.chmod(0o600)
        credentials.write_text("PALETTE_SOCKET=/tmp/override.sock\n")
        with self.assertRaisesRegex(ValueError, "cannot override service identity"):
            service.render_palette_unit(self.repo, self.workspace, self.runtime / "palette/daemon.sock", env_file=credentials)
        with self.assertRaisesRegex(ValueError, "existing executable"):
            service.render_tunnel_unit(self.root / "missing", "existing")
        with self.assertRaisesRegex(ValueError, "profile must exist"):
            service.render_tunnel_unit(client, "missing", profile_dir=profile_dir)

    def test_install_updates_only_owned_units_and_runs_fake_systemctl(self):
        units = self.root / "config/systemd/user"
        units.mkdir(parents=True)
        other = units / "unrelated.service"
        other.write_text("operator content\n")
        fake = self.root / "systemctl"
        log = self.root / "calls.jsonl"
        fake.write_text("#!/usr/bin/env python3\nimport json,os,sys\n"
                        "open(os.environ['CALL_LOG'],'a').write(json.dumps(sys.argv[1:])+'\\n')\n"
                        "if sys.argv[2:3]==['show']:\n print('ActiveState=active\\nSubState=running\\nMainPID='+str(os.getppid())+'\\nExecMainStartTimestamp=now')\n")
        fake.chmod(0o700)
        env = {"XDG_RUNTIME_DIR": str(self.runtime), "PALETTE_SYSTEMCTL": str(fake), "CALL_LOG": str(log)}
        with patch.object(service, "UNIT_DIR", units), patch.dict(os.environ, env, clear=False):
            service.main(["install", "--repo-dir", str(self.repo), "--workspace-dir", str(self.workspace)])
            before = other.read_text()
            service.main(["install", "--repo-dir", str(self.repo), "--workspace-dir", str(self.workspace)])
            self.assertEqual(other.read_text(), before)
            for action in ("start", "stop", "restart"):
                service.main([action])
            for action in ("start", "stop", "restart"):
                service.main([action, "--unit", "palette-tunnel.service"])
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                service.main(["status"])
            tunnel_output = io.StringIO()
            with contextlib.redirect_stdout(tunnel_output):
                service.main(["status", "--unit", "palette-tunnel.service"])
        status_record = json.loads(output.getvalue())
        self.assertEqual(status_record["MainPID"], str(os.getpid()))
        self.assertTrue(status_record["process_start_ticks"])
        self.assertEqual(status_record["socket"], str(self.runtime / "palette/daemon.sock"))
        self.assertTrue((units / "palette.service").is_file())
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(calls[0], ["--user", "daemon-reload"])
        self.assertEqual(calls[-2], ["--user", "show", "palette.service", "--no-page", "--property=ActiveState,SubState,MainPID,ExecMainStartTimestamp"])
        self.assertEqual(calls[-1], ["--user", "show", "palette-tunnel.service", "--no-page", "--property=ActiveState,SubState,MainPID,ExecMainStartTimestamp"])
        for action in ("start", "stop", "restart"):
            self.assertIn(["--user", action, "palette-tunnel.service"], calls)
        self.assertEqual(json.loads(tunnel_output.getvalue())["unit"], "palette-tunnel.service")
        self.assertEqual((units / "palette.service").stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
