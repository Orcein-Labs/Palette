"""Exercise the local model runner's CLI boundary without inference downloads."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

RUNNER = Path(__file__).with_name("vibethinker.py")


class VibeThinkerTest(unittest.TestCase):
    def test_fixed_configuration_prompt_and_environment(self):
        with tempfile.TemporaryDirectory(prefix="palette-vibe-") as tmp:
            root = Path(tmp)
            model = root / "model.gguf"
            model.write_bytes(b"fixture")
            executable = root / "llama-cli"
            executable.write_text(f"#!{sys.executable}\nimport json, os, sys\n"
                                  "print(json.dumps({'argv': sys.argv, 'env': dict(os.environ)}))\n")
            executable.chmod(0o755)
            config = subprocess.run([sys.executable, str(RUNNER), "config",
                "--llama-cli", str(executable), "--model", str(model), "--tokens", "32"],
                capture_output=True, text=True, check=True, timeout=10)
            entry = json.loads(config.stdout)["vibethinker"]
            prompt = "--model /other.gguf\n$(touch forbidden)"
            result = subprocess.run(entry["argv"] + [prompt], capture_output=True,
                text=True, check=True, timeout=10, env={**os.environ, "OPENAI_API_KEY": "fixture-secret"})
            observed = json.loads(result.stdout)
            argv = observed["argv"]
            self.assertEqual(argv[argv.index("--model") + 1], str(model))
            self.assertEqual(argv[argv.index("--n-predict") + 1], "32")
            self.assertIn("--single-turn", argv)
            self.assertIn("\n" + prompt + "<|im_end|>", argv[argv.index("--prompt") + 1])
            self.assertNotIn("OPENAI_API_KEY", observed["env"])
            self.assertFalse((root / "forbidden").exists())
            rejected = subprocess.run(entry["argv"] + ["one", "--model", str(model)],
                capture_output=True, text=True, timeout=10)
            self.assertNotEqual(rejected.returncode, 0)


if __name__ == "__main__":
    unittest.main()
