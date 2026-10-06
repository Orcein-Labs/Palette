#!/usr/bin/env python3
"""Trusted, offline installer for Palette's optional Gesso capability.

This copies an already-present Gesso source tree and SmolLM2 checkpoint into
server-owned roots. It never downloads model files or edits Gesso itself.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import tomllib
import uuid

MODEL_FILES = (
    "config.json", "model.safetensors", "merges.txt", "vocab.json",
    "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json",
)
GESSO_PATHS = ("Project.toml", "src", "ext")
GESSO_UUID = "05f31162-62af-44f0-af37-0e48a49f1899"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def inventory_digest(files: dict[str, str]) -> str:
    payload = "\n".join(f"{name}\0{files[name]}" for name in sorted(files))
    return hashlib.sha256(payload.encode()).hexdigest()


def path_disjoint(paths: list[Path]) -> None:
    resolved = [path.resolve(strict=False) for path in paths]
    for i, left in enumerate(resolved):
        for right in resolved[i + 1:]:
            if left == right or left in right.parents or right in left.parents:
                raise PermissionError(f"installer paths must be disjoint: {left} and {right}")


def regular_file(path: Path) -> os.stat_result:
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
        raise PermissionError(f"expected an unlinked regular file: {path}")
    return metadata


def make_tree_read_only_accessible(root: Path) -> None:
    """Give the sandbox UID traversal/read access without write bits."""
    for directory, names, files in os.walk(root, topdown=True, followlinks=False):
        current = Path(directory)
        os.chmod(current, 0o750)
        for name in names:
            child = current / name
            if child.is_symlink():
                raise PermissionError(f"installed artifact tree contains a symbolic link: {child}")
        for name in files:
            child = current / name
            regular_file(child)
            os.chmod(child, 0o440)


def validate_model_source(source: Path) -> tuple[dict[str, str], dict]:
    source = source.resolve(strict=True)
    if not source.is_dir():
        raise ValueError("model source must be a directory")
    raw = {}
    hashes = {}
    for name in MODEL_FILES:
        path = source / name
        regular_file(path)
        hashes[name] = sha256_file(path)
        if name in ("config.json", "tokenizer_config.json"):
            raw[name] = json.loads(path.read_text(encoding="utf-8"))
    cfg = raw["config.json"]
    expected = {"model_type": "llama", "hidden_size": 576, "num_hidden_layers": 30,
                "num_attention_heads": 9, "num_key_value_heads": 3,
                "vocab_size": 49152, "tie_word_embeddings": True,
                "eos_token_id": 0}
    mismatches = {key: (cfg.get(key), value) for key, value in expected.items() if cfg.get(key) != value}
    if mismatches:
        raise ValueError(f"checkpoint is not the contracted SmolLM2-135M artifact: {mismatches}")
    if raw["tokenizer_config.json"].get("tokenizer_class") != "GPT2Tokenizer":
        raise ValueError("checkpoint tokenizer is not the contracted GPT2Tokenizer")
    return hashes, cfg


def _atomic_json(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o750)
    temp = path.with_name("." + path.name + "." + uuid.uuid4().hex)
    try:
        with temp.open("x", encoding="utf-8") as stream:
            json.dump(value, stream, sort_keys=True, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temp, 0o640)
        os.replace(temp, path)
    finally:
        temp.unlink(missing_ok=True)


def install_model(source: Path, artifact_store: Path) -> tuple[Path, dict]:
    hashes, cfg = validate_model_source(source)
    identity = inventory_digest(hashes)
    artifact_id = "smollm2-135m-" + identity[:16]
    artifact_store = artifact_store.resolve(strict=False)
    artifact_store.mkdir(parents=True, exist_ok=True, mode=0o750)
    dest = artifact_store / artifact_id
    info = {"id": artifact_id, "model": "SmolLM2-135M", "inventory_sha256": identity,
            "files": hashes,
            "architecture": {key: cfg[key] for key in ("model_type", "hidden_size", "num_hidden_layers",
                "num_attention_heads", "num_key_value_heads", "vocab_size", "eos_token_id")}}
    if dest.exists():
        if dest.is_symlink() or not dest.is_dir():
            raise PermissionError("existing model artifact destination is not a regular directory")
        regular_file(dest / "artifact.json")
        stored = json.loads((dest / "artifact.json").read_text())
        if stored != info or any(sha256_file(dest / name) != hashes[name] for name in MODEL_FILES):
            raise PermissionError("existing versioned model artifact does not match its inventory")
        make_tree_read_only_accessible(dest)
        return dest, info
    with tempfile.TemporaryDirectory(prefix=".gesso-model-", dir=artifact_store) as staging:
        stage = Path(staging)
        for name in MODEL_FILES:
            src = source / name
            regular_file(src)
            target = stage / name
            shutil.copyfile(src, target, follow_symlinks=False)
            os.chmod(target, 0o440)
            if sha256_file(target) != hashes[name]:
                raise RuntimeError(f"copied model file failed its source digest: {name}")
        _atomic_json(stage / "artifact.json", info)
        os.chmod(stage / "artifact.json", 0o440)
        os.chmod(stage, 0o750)
        os.rename(stage, dest)
    return dest, info


def source_frontier(gesso_source: Path) -> tuple[str, str, list[tuple[Path, str]], str]:
    root = gesso_source.resolve(strict=True)
    project = tomllib.loads((root / "Project.toml").read_text(encoding="utf-8"))
    if project.get("name") != "Gesso" or project.get("uuid") != GESSO_UUID:
        raise ValueError("Gesso source root has an unexpected package identity")
    revision = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, check=True,
                              capture_output=True, text=True).stdout.strip()
    status = subprocess.run(["git", "status", "--porcelain", "--untracked-files=all", "--ignored=matching", "--",
                             "Project.toml", "src", "ext"], cwd=root, check=True,
                            capture_output=True, text=True).stdout
    diff = subprocess.run(["git", "diff", "--quiet", "HEAD", "--", "Project.toml", "src", "ext"], cwd=root)
    if status or diff.returncode != 0:
        raise PermissionError("Gesso package source or project files are dirty; install a committed source frontier")
    files = []
    for relative in GESSO_PATHS:
        path = root / relative
        if path.is_symlink():
            raise PermissionError(f"Gesso package source contains a symlink: {path}")
        if path.is_dir():
            for item in sorted(path.rglob("*")):
                if item.is_symlink():
                    raise PermissionError(f"Gesso package source contains a symlink: {item}")
                if item.is_file():
                    regular_file(item)
                    files.append((item, item.relative_to(root).as_posix()))
        else:
            regular_file(path)
            files.append((path, relative))
    hashes = {relative: sha256_file(path) for path, relative in files}
    return revision, str(project["version"]), files, inventory_digest(hashes)


def install_gesso_source(gesso_source: Path, package_store: Path) -> tuple[Path, str, str, str]:
    revision, version, files, source_hash = source_frontier(gesso_source)
    source_id = revision[:12] + "-" + source_hash[:12]
    root = package_store.resolve(strict=False) / "sources" / "gesso" / source_id
    root.parent.mkdir(parents=True, exist_ok=True, mode=0o750)
    if root.exists():
        if root.is_symlink() or not root.is_dir():
            raise PermissionError("existing versioned Gesso source destination is not a regular directory")
        for src, relative in files:
            target = root / relative
            try:
                regular_file(target)
            except (FileNotFoundError, PermissionError):
                raise PermissionError("existing versioned Gesso source copy differs from its inventory")
            if sha256_file(target) != sha256_file(src):
                raise PermissionError("existing versioned Gesso source copy differs from its inventory")
        if inventory_digest({rel: sha256_file(root / rel) for _, rel in files}) != source_hash:
            raise PermissionError("existing versioned Gesso source inventory differs")
        make_tree_read_only_accessible(root)
        return root, revision, version, source_hash
    with tempfile.TemporaryDirectory(prefix=".gesso-source-", dir=root.parent) as staging:
        stage = Path(staging)
        for src, relative in files:
            target = stage / relative
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o750)
            os.chmod(target.parent, 0o750)
            shutil.copyfile(src, target, follow_symlinks=False)
            os.chmod(target, 0o440)
            if sha256_file(target) != sha256_file(src):
                raise RuntimeError(f"copied Gesso source failed its digest: {relative}")
        for directory, _, _ in os.walk(stage, topdown=False):
            os.chmod(directory, 0o750)
        if inventory_digest({rel: sha256_file(stage / rel) for _, rel in files}) != source_hash:
            raise RuntimeError("copied Gesso source differs from the committed source inventory")
        os.rename(stage, root)
    return root, revision, version, source_hash


def _julia_string(value: str) -> str:
    if "\n" in value or "\r" in value or "\x00" in value:
        raise ValueError("installed paths cannot contain control characters")
    return json.dumps(value, ensure_ascii=True).replace("$", "\\$")


def deploy_capability(runtime_root: Path, model_path: Path, artifact_info: dict,
                      source_revision: str, source_sha256: str, gesso_version: str,
                      template_dir: Path) -> tuple[Path, str]:
    target = runtime_root.resolve(strict=False) / "gesso"
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o750)
    template = (template_dir / "gesso.jl").read_text(encoding="utf-8")
    replacements = {
        "__PALETTE_GESSO_ARTIFACT_ROOT__": _julia_string(str(model_path.resolve(strict=True))),
        "__PALETTE_GESSO_ARTIFACT_ID__": artifact_info["id"],
        "__PALETTE_GESSO_SOURCE_REVISION__": source_revision,
        "__PALETTE_GESSO_SOURCE_SHA256__": source_sha256,
        "__PALETTE_GESSO_VERSION__": gesso_version,
    }
    for key, value in replacements.items():
        template = template.replace(key, value)
    if "__PALETTE_GESSO_" in template:
        raise ValueError("Gesso capability source has an unresolved install placeholder")
    manifest = json.loads((template_dir / "capability.json").read_text(encoding="utf-8"))
    manifest["version"] = f"{manifest['version']}+{source_revision[:10]}.{artifact_info['inventory_sha256'][:10]}"
    if target.exists() and target.is_symlink():
        raise PermissionError("capability destination must not be a symlink")
    target.mkdir(parents=True, exist_ok=True, mode=0o750)
    source_path = target / manifest["source"]
    temp_source = source_path.with_name("." + source_path.name + "." + uuid.uuid4().hex)
    temp_source.write_text(template, encoding="utf-8")
    os.chmod(temp_source, 0o440)
    os.replace(temp_source, source_path)
    _atomic_json(target / "capability.json", manifest)
    return target, manifest["version"]


def install(args) -> dict:
    palette_root = Path(__file__).resolve().parents[3]
    gesso_source = Path(args.gesso_source).resolve(strict=True)
    model_source = Path(args.model_source).resolve(strict=True)
    package_store = Path(args.package_store).resolve(strict=False)
    artifact_store = Path(args.artifact_store).resolve(strict=False)
    runtime_root = Path(args.runtime_root).resolve(strict=False)
    integration_source = Path(__file__).resolve().parent
    for destination in (package_store, artifact_store, runtime_root):
        path_disjoint([gesso_source, destination])
        path_disjoint([integration_source, destination])
    path_disjoint([package_store, artifact_store, runtime_root])
    path_disjoint([gesso_source, palette_root])
    path_disjoint([package_store, palette_root])
    path_disjoint([artifact_store, palette_root])
    revision, gesso_version, _, source_sha256 = source_frontier(gesso_source)
    gesso_copy, revision, gesso_version, source_sha256 = install_gesso_source(gesso_source, package_store)
    artifact_path, artifact = install_model(model_source, artifact_store)
    environment = package_store / "environment"
    depot = package_store / "depot"
    if not environment.is_dir() or not depot.is_dir():
        raise ValueError("package_store must already contain its trusted environment/ and depot/ roots")
    julia = args.julia or shutil.which("julia")
    if not julia:
        raise RuntimeError("julia is required to register the optional Gesso package offline")
    package_store.mkdir(parents=True, exist_ok=True, mode=0o750)
    lock_path = package_store / ".package-store.lock"
    with lock_path.open("a+") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        env = {"PATH": f"{Path(julia).resolve().parent}:/usr/bin:/bin", "LANG": "C.UTF-8",
               "JULIA_DEPOT_PATH": str(depot.resolve(strict=True)),
               "JULIA_PROJECT": str(environment.resolve(strict=True)), "JULIA_PKG_OFFLINE": "true"}
        command = [julia, "--startup-file=no", f"--project={environment}", "-e",
                   "using Pkg; Pkg.develop(Pkg.PackageSpec(path=ARGS[1])); Pkg.add(Pkg.PackageSpec(name=\"JSON\")); using Gesso, JSON; println(\"PALETTE_GESSO_VERSION=\", Base.pkgversion(Gesso)); println(\"PALETTE_JSON_VERSION=\", Base.pkgversion(JSON))",
                   str(gesso_copy)]
        result = subprocess.run(command, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, check=False, timeout=300)
        if result.returncode != 0:
            raise RuntimeError("offline Pkg.develop of the copied Gesso source failed: " + result.stderr[-2000:])
        version_lines = [line.removeprefix("PALETTE_GESSO_VERSION=") for line in result.stdout.splitlines()
                         if line.startswith("PALETTE_GESSO_VERSION=")]
        if version_lines != [gesso_version]:
            raise RuntimeError("loaded Gesso package version differs from the copied source version")
        json_lines = [line.removeprefix("PALETTE_JSON_VERSION=") for line in result.stdout.splitlines()
                      if line.startswith("PALETTE_JSON_VERSION=")]
        if len(json_lines) != 1:
            raise RuntimeError("the optional package environment did not resolve Gesso's JSON dependency")
    capability_dir, capability_version = deploy_capability(runtime_root, artifact_path, artifact,
        revision, source_sha256, gesso_version, Path(__file__).parent)
    return {"status": "installed", "capability_id": "gesso", "capability_version": capability_version,
            "capability_dir": str(capability_dir), "gesso_package_dir": str(gesso_copy),
            "package_environment": str(environment), "artifact_id": artifact["id"],
            "artifact_dir": str(artifact_path), "artifact_inventory_sha256": artifact["inventory_sha256"],
            "required_profile_read_root": str(artifact_path),
            "note": "Add required_profile_read_root to the trusted server profile read_roots; no model or Gesso source path is accepted at runtime."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gesso-source", required=True, help="read-only source checkout matching a clean committed Gesso frontier")
    parser.add_argument("--model-source", required=True, help="existing local SmolLM2-135M snapshot; never downloaded")
    parser.add_argument("--package-store", required=True, help="trusted Palette package store with environment/ and depot/")
    parser.add_argument("--artifact-store", required=True, help="server-owned, read-only-mounted model artifact directory")
    parser.add_argument("--runtime-root", required=True, help="trusted PALETTE_RUNTIME_ROOT")
    parser.add_argument("--julia", help="trusted Julia executable; defaults to PATH lookup")
    args = parser.parse_args()
    print(json.dumps(install(args), sort_keys=True, indent=2))


if __name__ == "__main__":
    main()
