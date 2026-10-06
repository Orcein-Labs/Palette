#!/usr/bin/env python3
"""Run Palette's Julia, Rust and real operator conformance gates."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).absolute().parents[2]


def run(command, env):
    print("\nverification:", " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), cwd=ROOT, env=env, stdin=subprocess.DEVNULL, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project-dir", required=True, help="Test environment with Palette, JSON, CSV, DataFrames, EzXML and IJulia")
    parser.add_argument("--implementation", choices=("both", "rust", "python"), default="both")
    args = parser.parse_args()
    project = Path(args.project_dir).absolute()
    if not (project / "Project.toml").is_file() or not (project / "Manifest.toml").is_file():
        parser.error("the test project must be instantiated before verification")
    for binary in ("bwrap", "julia", "cargo", "node"):
        if shutil.which(binary) is None:
            parser.error(f"required verification binary missing: {binary}")
    env = {**os.environ, "PALETTE_REPO_DIR": str(ROOT), "PALETTE_TEST_PROJECT_DIR": str(project)}
    env.pop("PALETTE_HOST_BIN", None)
    # This is a test dependency check, not an automatic operator installation.
    run(["julia", "--startup-file=no", f"--project={project}", "-e",
         'using Palette, JSON, CSV, DataFrames, EzXML, IJulia; '
         'realpath(dirname(dirname(pathof(Palette)))) == realpath(ARGS[1]) || '
         'error("Test environment points to a different Palette source tree")', ROOT], env)
    run(["git", "diff", "--check"], env)
    run(["node", ROOT / "runtime/scripts/check-test-policy.mjs"], env)
    run(["julia", "--startup-file=no", f"--project={ROOT}", ROOT / "test/runtests.jl"], env)
    run(["cargo", "test", "--manifest-path", ROOT / "runtime/host/Cargo.toml"], env)
    run(["cargo", "build", "--release", "--manifest-path", ROOT / "runtime/host/Cargo.toml"], env)
    run([sys.executable, ROOT / "runtime/security/test_fs_digest.py", "-v"], env)
    host = ROOT / "runtime/host/target/release/palette-host"
    run([sys.executable, ROOT / "runtime/security/prewarm_depot.py", "--project-dir", project,
         "--repo-dir", ROOT, "--host-bin", host], env)
    for suite in ("test_provisioning.py", "test_patch_broker.py", "test_server_runtime.py", "test_workspace_router.py"):
        run([sys.executable, ROOT / "runtime/security" / suite, "-v"], env)
    run([sys.executable, ROOT / "runtime/integrations/gesso/test_gesso.py", "-v"], env)
    implementations = ("rust", "python") if args.implementation == "both" else (args.implementation,)
    for implementation in implementations:
        print(f"\noperator implementation: {implementation}", flush=True)
        implementation_env = dict(env)
        if implementation == "rust":
            implementation_env["PALETTE_HOST_BIN"] = str(host)
        for suite in ("test_authority.py", "test_session.py", "test_session_cli.py", "test_revival.py", "test_host_bridge.py"):
            run([sys.executable, ROOT / "runtime/security" / suite, "-v"], implementation_env)
    print("\nAll operator verification gates passed.", flush=True)


if __name__ == "__main__":
    main()
