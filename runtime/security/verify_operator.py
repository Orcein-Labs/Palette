#!/usr/bin/env python3
"""Run Palette's Julia, Rust and real operator conformance gates."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).absolute().parents[2]


def run(command, env):
    print("\nverification:", " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), cwd=ROOT, env=env, stdin=subprocess.DEVNULL, check=True)


LIST_TESTS = """
import json, sys, unittest
def ids(suite):
    for test in suite:
        yield from ids(test) if isinstance(test, unittest.TestSuite) else [test.id().split(".", 1)[1]]
print(json.dumps(list(ids(unittest.defaultTestLoader.loadTestsFromName(sys.argv[1])))))
"""


class Shard:
    """Splits the test suites' cases round-robin across --shard I/N jobs."""

    def __init__(self, spec):
        index, _, count = spec.partition("/")
        self.index, self.count, self.position = int(index), int(count), 0
        if not 1 <= self.index <= self.count:
            raise ValueError(f"shard {spec} is not of the form I/N with 1 <= I <= N")

    def run_suite(self, suite, env):
        if self.count == 1:
            return run([sys.executable, suite, "-v"], env)
        listed = subprocess.run([sys.executable, "-c", LIST_TESTS, suite.stem], cwd=suite.parent, env=env,
                                stdin=subprocess.DEVNULL, capture_output=True, text=True, check=True)
        tests = json.loads(listed.stdout)
        mine = [test for offset, test in enumerate(tests) if (self.position + offset) % self.count == self.index - 1]
        self.position += len(tests)
        if mine:
            run([sys.executable, suite, "-v", *mine], env)
        else:
            print(f"\nverification: {suite.name} has no cases in shard {self.index}/{self.count}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project-dir", required=True, help="Test environment with Palette, JSON, CSV, DataFrames, EzXML and IJulia")
    parser.add_argument("--implementation", choices=("both", "rust", "python"), default="both")
    parser.add_argument("--shard", default="1/1", help="run shard I of N: the shared gates on shard 1, every Nth test case on each")
    args = parser.parse_args()
    try:
        shard = Shard(args.shard)
    except ValueError as error:
        parser.error(str(error))
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
    if shard.index == 1:
        run(["git", "diff", "--check"], env)
        run(["node", ROOT / "runtime/scripts/check-test-policy.mjs"], env)
        run(["julia", "--startup-file=no", f"--project={ROOT}", ROOT / "test/runtests.jl"], env)
        run(["cargo", "test", "--manifest-path", ROOT / "runtime/host/Cargo.toml"], env)
        run([sys.executable, ROOT / "runtime/security/test_fs_digest.py", "-v"], env)
    run(["cargo", "build", "--release", "--manifest-path", ROOT / "runtime/host/Cargo.toml"], env)
    host = ROOT / "runtime/host/target/release/palette-host"
    run([sys.executable, ROOT / "runtime/security/prewarm_depot.py", "--project-dir", project,
         "--repo-dir", ROOT, "--host-bin", host], env)
    for suite in ("test_provisioning.py", "test_patch_broker.py", "test_server_runtime.py", "test_workspace_router.py"):
        shard.run_suite(ROOT / "runtime/security" / suite, env)
    shard.run_suite(ROOT / "runtime/integrations/gesso/test_gesso.py", env)
    implementations = ("rust", "python") if args.implementation == "both" else (args.implementation,)
    for implementation in implementations:
        print(f"\noperator implementation: {implementation}", flush=True)
        implementation_env = dict(env)
        if implementation == "rust":
            implementation_env["PALETTE_HOST_BIN"] = str(host)
        for suite in ("test_authority.py", "test_session.py", "test_session_cli.py", "test_revival.py", "test_host_bridge.py"):
            shard.run_suite(ROOT / "runtime/security" / suite, implementation_env)
    print(f"\nAll operator verification gates passed (shard {shard.index}/{shard.count}).", flush=True)


if __name__ == "__main__":
    main()
