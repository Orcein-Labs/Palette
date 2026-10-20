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


WEIGHTS = Path(__file__).absolute().parent / "test-case-seconds.json"
# Shard 1 also runs the shared gates, which took about this long in CI.
SHARED_GATE_SECONDS = 90


def case_key(case):
    suite, _, test = case
    return f"{suite.stem}:{test}"


class Shard:
    """Deals the test suites' cases across --shard I/N jobs, longest first to the least-loaded job.

    The seconds come from test-case-seconds.json (refresh it with
    runtime/scripts/record-test-case-seconds.py); a case with no record costs its suite's mean.
    Both hosts share a case's record."""

    def __init__(self, spec):
        index, _, count = spec.partition("/")
        self.index, self.count = int(index), int(count)
        if not 1 <= self.index <= self.count:
            raise ValueError(f"shard {spec} is not of the form I/N with 1 <= I <= N")

    def mine(self, runs):
        cases = []
        for suite, implementation, env in runs:
            listed = subprocess.run([sys.executable, "-c", LIST_TESTS, suite.stem], cwd=suite.parent, env=env,
                                    stdin=subprocess.DEVNULL, capture_output=True, text=True, check=True)
            cases += [(suite, implementation, test) for test in json.loads(listed.stdout)]
        weights = json.loads(WEIGHTS.read_text())
        means = {}
        for key, seconds in weights.items():
            means.setdefault(key.partition(":")[0], []).append(seconds)
        fallback = sum(weights.values()) / max(len(weights), 1)

        def seconds(case):
            key = case_key(case)
            recorded = means.get(key.partition(":")[0])
            return weights.get(key, sum(recorded) / len(recorded) if recorded else fallback)

        loads = [SHARED_GATE_SECONDS] + [0] * (self.count - 1)
        mine = set()
        for case in sorted(cases, key=lambda case: (-seconds(case), case_key(case))):
            job = min(range(self.count), key=lambda job: (loads[job], job))
            loads[job] += seconds(case)
            if job == self.index - 1:
                mine.add(case)
        return [case for case in cases if case in mine]

    def run(self, runs):
        mine = None if self.count == 1 else self.mine(runs)
        current = None
        for suite, implementation, env in runs:
            if implementation != current:
                print(f"\noperator implementation: {implementation}", flush=True)
                current = implementation
            if mine is None:
                run([sys.executable, suite, "-v"], env)
                continue
            tests = [test for case_suite, case_implementation, test in mine
                     if (case_suite, case_implementation) == (suite, implementation)]
            if tests:
                run([sys.executable, suite, "-v", *tests], env)
            else:
                print(f"\nverification: {suite.name} has no cases in shard {self.index}/{self.count}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project-dir", required=True, help="Test environment with Palette, JSON, CSV, DataFrames, EzXML and IJulia")
    parser.add_argument("--implementation", choices=("both", "rust", "python"), default="both")
    parser.add_argument("--shard", default="1/1", help="run shard I of N: the shared gates on shard 1, and a share of the test cases by recorded seconds")
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
    security = ROOT / "runtime/security"
    runs = [(security / suite, None, env) for suite in
            ("test_provisioning.py", "test_patch_broker.py", "test_server_runtime.py", "test_workspace_router.py",
             "test_receipt_query.py", "test_read_roots.py", "test_palette_daemon.py")]
    runs.append((ROOT / "runtime/integrations/gesso/test_gesso.py", None, env))
    implementations = ("rust", "python") if args.implementation == "both" else (args.implementation,)
    for implementation in implementations:
        implementation_env = {**env, "PALETTE_HOST_BIN": str(host)} if implementation == "rust" else env
        runs += [(security / suite, implementation, implementation_env) for suite in
                 ("test_authority.py", "test_session.py", "test_session_cli.py", "test_revival.py", "test_host_bridge.py",
                  "test_contexts.py", "test_time_machine.py", "test_tools.py")]
        # Receipts are the Rust host's, and test_host_command runs the Python broker itself.
        if implementation == "rust":
            runs += [(security / suite, implementation, implementation_env) for suite in
                     ("test_xray.py", "test_host_command.py")]
    shard.run(runs)
    print(f"\nAll operator verification gates passed (shard {shard.index}/{shard.count}).", flush=True)


if __name__ == "__main__":
    main()
