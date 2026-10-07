"""Case provisioning_001: trusted ceilings and generic fixed toolchains."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from provisioning import OperatorConfigError, operator_ceiling, protected_host_paths

RUNNER = Path(__file__).with_name("provisioning.py")


class ProvisioningTest(unittest.TestCase):
    def test_case_provisioning_001_ceiling_is_host_owned_and_validated(self):
        with tempfile.TemporaryDirectory(prefix="palette-ceiling-") as tmp:
            config = Path(tmp, "ceiling.json")
            legacy = Path(tmp, "commands.json")
            config.write_text(json.dumps({"host_request": {"allowed_types": ["palette.runtime"]},
                                          "package_management": {"allowed_packages": ["Parsers"]}}))
            legacy.write_text(json.dumps({"fixture": {"argv": ["/usr/bin/true"], "cwd": tmp}}))
            old_commands = os.environ.get("PALETTE_HOST_COMMANDS")
            os.environ["PALETTE_HOST_COMMANDS"] = str(legacy)
            try:
                value = operator_ceiling(config)
            finally:
                if old_commands is None:
                    os.environ.pop("PALETTE_HOST_COMMANDS", None)
                else:
                    os.environ["PALETTE_HOST_COMMANDS"] = old_commands
            value["package_management"]["allowed_packages"].append("Injected")
            self.assertEqual(operator_ceiling(config)["package_management"]["allowed_packages"], ["Parsers"])
            config.write_text('{"package_management": {"allowed_packages": null}}')
            with self.assertRaisesRegex(OperatorConfigError, "allowed_packages"):
                operator_ceiling(config)
            config.write_text('{"package_management": {"allowed_packages": ["Parsers"], "offline": "yes"}}')
            with self.assertRaisesRegex(OperatorConfigError, "offline must be boolean"):
                operator_ceiling(config)
            config.write_text('{"made_up": {}}')
            with self.assertRaisesRegex(OperatorConfigError, "unknown capability"):
                operator_ceiling(config)

    def test_case_provisioning_003_host_command_conflicts_fail_closed(self):
        with tempfile.TemporaryDirectory(prefix="palette-command-merge-") as tmp:
            config = Path(tmp, "ceiling.json")
            legacy = Path(tmp, "commands.json")
            config.write_text(json.dumps({"host_command": {"commands": {
                "fixture": {"argv": ["/usr/bin/false"], "cwd": tmp}}}}))
            legacy.write_text(json.dumps({"fixture": {"argv": ["/usr/bin/true"], "cwd": tmp}}))
            old_commands = os.environ.get("PALETTE_HOST_COMMANDS")
            os.environ["PALETTE_HOST_COMMANDS"] = str(legacy)
            try:
                with self.assertRaisesRegex(OperatorConfigError, "conflicts"):
                    operator_ceiling(config)
            finally:
                if old_commands is None:
                    os.environ.pop("PALETTE_HOST_COMMANDS", None)
                else:
                    os.environ["PALETTE_HOST_COMMANDS"] = old_commands
            allowed = operator_ceiling(config)
            protected = protected_host_paths(allowed)
            self.assertIn(Path(tmp).resolve(), protected)
            self.assertIn(Path("/usr/bin/false"), protected)

    def test_case_provisioning_004_protects_nested_operator_artifacts_not_runtime_input(self):
        with tempfile.TemporaryDirectory(prefix="palette-protected-artifacts-") as tmp:
            root = Path(tmp)
            model = root / "models" / "runner.gguf"
            weights = root / "weights" / "fixed.safetensors"
            artifact = root / "assets" / "tokenizer.json"
            tokenizer_config = root / "assets" / "vocab.model"
            generic_config = root / "config" / "runner.toml"
            library = root / "lib"
            executable = root / "bin" / "runner"
            paths = [model, weights, artifact, tokenizer_config, generic_config, library, executable]
            for path in paths:
                path.mkdir(parents=True, exist_ok=True) if path == library else path.parent.mkdir(parents=True, exist_ok=True)
            argv = ["/usr/bin/python3", str(RUNNER), "run",
                    "--executable", str(executable),
                    f"--executable={executable}", f"--model={model}",
                    "--weights", str(weights), f"--weights={weights}",
                    "--artifact", str(artifact), f"--artifact={artifact}",
                    f"--tokenizer={tokenizer_config}", f"--config={generic_config}",
                    "--library-dir", str(library), f"--library-dir={library}",
                    f"--fixed-arg=--model={model}",
                    f"--fixed-arg=--tokenizer={tokenizer_config}",
                    f"--fixed-arg=--config={generic_config}",
                    "--fixed-arg=--weights", f"--fixed-arg={weights}",
                    "--fixed-arg", "--library-dir", "--fixed-arg", str(library)]
            ceiling = {"host_command": {"commands": {
                "inference": {"argv": argv, "cwd": tmp, "extra_args": True}}}}
            protected = set(protected_host_paths(ceiling))
            for selected in paths:
                resolved = selected.resolve()
                self.assertIn(resolved, protected)
                self.assertIn(resolved.parent, protected)
            caller_input_path = root / "caller-selected-model.gguf"
            ordinary_input = f"--model={caller_input_path}"
            self.assertNotIn(ordinary_input, argv)
            self.assertNotIn(caller_input_path.resolve(), protected)

    def test_case_provisioning_002_generic_toolchain_keeps_binary_and_artifact_fixed(self):
        with tempfile.TemporaryDirectory(prefix="palette-toolchain-") as tmp:
            root = Path(tmp)
            artifact = root / "selected-model.bin"
            artifact.write_text("operator-selected")
            executable = root / "fixture-tool"
            executable.write_text(f"#!{sys.executable}\nimport json, os, sys\n"
                                  "print(json.dumps({'argv': sys.argv[1:], 'env': dict(os.environ)}))\n")
            executable.chmod(0o755)
            config = subprocess.run([sys.executable, str(RUNNER), "config", "--name", "fixture",
                "--executable", str(executable), "--cwd", tmp, "--artifact", str(artifact),
                "--fixed-arg=--backend", "--input-mode", "text", "--input-prefix=--prompt"],
                capture_output=True, text=True, check=True, timeout=10)
            entry = json.loads(config.stdout)["fixture"]
            result = subprocess.run(entry["argv"] + ["What is 6 times 7?"], capture_output=True,
                text=True, check=True, timeout=10, env={**os.environ, "OPENAI_API_KEY": "fixture-secret"})
            observed = json.loads(result.stdout)
            argv = observed["argv"]
            self.assertIn(str(artifact), argv)
            self.assertEqual(argv[argv.index("--prompt") + 1], "What is 6 times 7?")
            self.assertNotIn("OPENAI_API_KEY", observed["env"])
            self.assertEqual(entry["extra_args"], True)
            denied = subprocess.run(entry["argv"] + ["--model", str(root / "attacker.bin")],
                capture_output=True, text=True, timeout=10)
            self.assertNotEqual(denied.returncode, 0)


if __name__ == "__main__":
    unittest.main()
