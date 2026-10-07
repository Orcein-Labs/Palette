"""Offline installer and capability deployment contract tests."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from install import (MODEL_FILES, deploy_capability, install_gesso_source,
                     install_model, inventory_digest, sha256_file, validate_model_source)


class GessoInstallerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.model = self.root / "source-model"
        self.model.mkdir()
        cfg = {"model_type": "llama", "hidden_size": 576, "num_hidden_layers": 30,
               "num_attention_heads": 9, "num_key_value_heads": 3,
               "vocab_size": 49152, "tie_word_embeddings": True, "eos_token_id": 0,
               "max_position_embeddings": 8192}
        tokenizer = {"tokenizer_class": "GPT2Tokenizer"}
        for name in MODEL_FILES:
            path = self.model / name
            if name == "config.json":
                path.write_text(json.dumps(cfg))
            elif name == "tokenizer_config.json":
                path.write_text(json.dumps(tokenizer))
            else:
                path.write_bytes((name + "\n").encode())

    def test_G01_fixed_model_identity_and_hash_copy(self):
        hashes, cfg = validate_model_source(self.model)
        self.assertEqual(set(hashes), set(MODEL_FILES))
        artifact_root = self.root / "server-models"
        installed, manifest = install_model(self.model, artifact_root)
        self.assertEqual(manifest["id"], "smollm2-135m-" + inventory_digest(hashes)[:16])
        self.assertEqual(json.loads((installed / "artifact.json").read_text()), manifest)
        for name in MODEL_FILES:
            self.assertEqual((installed / name).read_bytes(), (self.model / name).read_bytes())
            self.assertFalse((installed / name).stat().st_mode & 0o222)
        again, same = install_model(self.model, artifact_root)
        self.assertEqual((again, same), (installed, manifest))
        tampered = installed / "merges.txt"
        tampered.chmod(0o640)
        tampered.write_text("tampered")
        with self.assertRaises(PermissionError):
            install_model(self.model, artifact_root)

    def test_G02_wrong_architecture_and_hardlinks_fail_closed(self):
        cfg_path = self.model / "config.json"
        cfg = json.loads(cfg_path.read_text())
        cfg["model_type"] = "other"
        cfg_path.write_text(json.dumps(cfg))
        with self.assertRaises(ValueError):
            validate_model_source(self.model)
        cfg["model_type"] = "llama"
        cfg_path.write_text(json.dumps(cfg))
        target = self.model / "hardlink.txt"
        os.link(self.model / "merges.txt", target)
        (self.model / "merges.txt").unlink()
        os.link(target, self.model / "merges.txt")
        with self.assertRaises(PermissionError):
            validate_model_source(self.model)

    def test_G03_package_copy_uses_only_clean_tracked_runtime_source(self):
        source = self.root / "Gesso"
        (source / "src").mkdir(parents=True)
        (source / "ext").mkdir()
        (source / "libs" / "Lava").mkdir(parents=True)
        (source / "Project.toml").write_text('name = "Gesso"\nuuid = "05f31162-62af-44f0-af37-0e48a49f1899"\nversion = "0.1.0"\n')
        (source / "src" / "Gesso.jl").write_text("module Gesso\nend\n")
        (source / "ext" / "GessoCUDAExt.jl").write_text("module GessoCUDAExt\nend\n")
        (source / "libs" / "Lava" / "Lava.jl").write_text("module Lava\nend\n")
        subprocess.run(["git", "init", "-q"], cwd=source, check=True)
        subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=source, check=True)
        subprocess.run(["git", "config", "user.name", "Installer Test"], cwd=source, check=True)
        subprocess.run(["git", "add", "Project.toml", "src", "ext"], cwd=source, check=True)
        subprocess.run(["git", "commit", "-qm", "test source"], cwd=source, check=True)
        copied, revision, version, source_hash = install_gesso_source(source, self.root / "packages")
        self.assertEqual(version, "0.1.0")
        self.assertEqual((copied / "src" / "Gesso.jl").read_text(), "module Gesso\nend\n")
        expected_source_hash = inventory_digest({
            "Project.toml": sha256_file(copied / "Project.toml"),
            "src/Gesso.jl": sha256_file(copied / "src" / "Gesso.jl"),
            "ext/GessoCUDAExt.jl": sha256_file(copied / "ext" / "GessoCUDAExt.jl"),
        })
        self.assertEqual(source_hash, expected_source_hash)
        self.assertTrue((copied / "ext" / "GessoCUDAExt.jl").is_file())
        self.assertFalse((copied / "libs").exists())
        self.assertEqual(list((copied / "src").rglob("*.jl")), [copied / "src" / "Gesso.jl"])
        original_copy = shutil.copyfile
        def changing_copy(src, dst, **kwargs):
            if Path(src).name == "Gesso.jl":
                Path(src).write_text("module Gesso; const changed=true; end\n")
            return original_copy(src, dst, **kwargs)
        with patch("install.shutil.copyfile", side_effect=changing_copy):
            with self.assertRaisesRegex(RuntimeError, "committed source inventory"):
                install_gesso_source(source, self.root / "racing-packages")
        self.assertEqual(list((self.root / "racing-packages" / "sources" / "gesso").iterdir()), [])
        subprocess.run(["git", "checkout", "--", "src/Gesso.jl"], cwd=source, check=True)
        (source / ".gitignore").write_text("src/ignored.jl\n")
        (source / "src" / "ignored.jl").write_text("error(\"untracked\")")
        with self.assertRaisesRegex(PermissionError, "committed source frontier"):
            install_gesso_source(source, self.root / "ignored-packages")

    def test_G04_installed_capability_pins_artifact_and_optional_dependency(self):
        artifact_root, info = install_model(self.model, self.root / "models")
        runtime_root = self.root / "runtime"
        capability, version = deploy_capability(runtime_root, artifact_root, info,
            "revision-a", "a" * 64, "0.1.0", Path(__file__).parent)
        manifest = json.loads((capability / "capability.json").read_text())
        code = (capability / "gesso.jl").read_text()
        self.assertEqual(manifest["id"], "gesso")
        self.assertEqual(manifest["dependencies"], ["Gesso", "JSON"])
        self.assertEqual(manifest["requires"], [])
        self.assertIn(str(artifact_root.resolve()), code)
        self.assertNotIn("__PALETTE_GESSO_", code)
        self.assertNotIn("/mnt/d/Code Projects/Gesso", code)
        self.assertIn(info["id"], code)
        self.assertTrue(version.startswith("0.1.0+"))

    def test_G05_deployed_capability_and_probe_parse_as_julia(self):
        julia = shutil.which("julia")
        self.assertIsNotNone(julia)
        artifact_root, info = install_model(self.model, self.root / "models")
        capability, _ = deploy_capability(self.root / "runtime", artifact_root, info,
            "revision-a", "a" * 64, "0.1.0", Path(__file__).parent)
        command = [julia, "--startup-file=no", "-e",
                   'function check(ex); ex isa Expr || return; ex.head in (:error, :incomplete) && error(string(ex)); foreach(check, ex.args); end; foreach(p -> check(Meta.parseall(read(p, String))), ARGS); println("GESSO_SOURCE_PARSE=ok")',
                   str(capability / "gesso.jl"), str(Path(__file__).parent / "probe.jl")]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("GESSO_SOURCE_PARSE=ok", result.stdout)


    def test_G06_optional_backend_import_sees_new_extension_methods(self):
        julia = shutil.which("julia")
        self.assertIsNotNone(julia)
        packages = self.root / "fixture-packages"
        fixtures = {
            "Gesso": "module Gesso; struct GessoError <: Exception; code::Symbol; message::String; end; const ERR_RESOURCE_LIMIT=:ERR_RESOURCE_LIMIT; gesso_error(code, message; kwargs...)=GessoError(code,message); function backend_name end; function supports end; architecture_spec(path)=:fixture; required_semantics(spec)=[:fixture_capability]; end",
            "JSON": "module JSON; end",
            "CUDA": "module CUDA; import Gesso; function __init__(); @eval Gesso begin; struct CUDABackend end; backend_name(::CUDABackend)=:fixture_cuda; supports(::CUDABackend, cap)=cap===:fixture_capability; end; end; end",
        }
        for name, source in fixtures.items():
            target = packages / name / "src"
            target.mkdir(parents=True)
            (target / (name + ".jl")).write_text(source)
        artifact_root, info = install_model(self.model, self.root / "models")
        capability, _ = deploy_capability(self.root / "runtime", artifact_root, info,
            "revision-a", "a" * 64, "0.1.0", Path(__file__).parent)
        script = 'using Test; include(ARGS[1]); r=_backend_diagnostic("CUDA", :cuda); println(r); @test r["status"] == "available"; @test r["backend"] == "fixture_cuda"; @test r["supports"]["fixture_capability"]; println("GESSO_FIXTURE_BACKEND=fixture_cuda")'
        result = subprocess.run([julia, "--startup-file=no", "-e", script, str(capability / "gesso.jl")],
            env={**os.environ, "JULIA_LOAD_PATH": str(packages) + ":@stdlib"}, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("GESSO_FIXTURE_BACKEND=fixture_cuda", result.stdout)
        probe_script = 'using Test; include(ARGS[1]); err=try check_optional_backend("cuda"); nothing catch e; e end; @test err isa Gesso.GessoError; @test err.code === Gesso.ERR_RESOURCE_LIMIT; @test occursin("device construction only", err.message); println("GESSO_PROBE_EXTENSION=constructed")'
        result = subprocess.run([julia, "--startup-file=no", "-e", probe_script, str(Path(__file__).parent / "probe.jl")],
            env={**os.environ, "JULIA_LOAD_PATH": str(packages) + ":@stdlib"}, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("GESSO_PROBE_EXTENSION=constructed", result.stdout)
        validate_script = 'using Test; include(ARGS[1]); @test _artifact(false)["id"] == _GESSO_ARTIFACT_ID; rm(joinpath(_GESSO_ARTIFACT_ROOT,"config.json")); symlink(ARGS[2],joinpath(_GESSO_ARTIFACT_ROOT,"config.json")); @test_throws ArgumentError _artifact(false); println("GESSO_ARTIFACT_LINK=denied")'
        shutil.rmtree(packages / "JSON")
        artifact_root.chmod(0o750)
        result = subprocess.run([julia, "--startup-file=no", "-e", validate_script, str(capability / "gesso.jl"), str(self.model / "config.json")],
            env={**os.environ, "JULIA_LOAD_PATH": str(packages) + ":" + str(Path(__file__).resolve().parents[3]) + ":@stdlib"}, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("GESSO_ARTIFACT_LINK=denied", result.stdout)


if __name__ == "__main__":
    unittest.main()
