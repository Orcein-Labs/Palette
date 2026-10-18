# Scripts

- `install.sh`: one-command install on Linux or inside WSL2 (`runtime/docs/install.md`).
- `install.ps1`: one-command install on Windows through WSL2.
- `setup-sandbox-host.sh`: `check` that the sandbox starts here; `install`, as root, what it needs.

- `session_loop.jl`: persistent worker entrypoint used by the host and launcher.
- `julia_analyzer.py`: approximate static preflight; real Julia tests are authoritative.
- `test_julia_analyzer.py`: preflight regression controls.
- `check-test-policy.mjs`: incremental test hygiene gate.
- `setup-ci-bubblewrap.sh`: `setup-sandbox-host.sh` on disposable GitHub Ubuntu runners.
- `real_ijulia_proof.py`: integration with a live IJulia kernel over Jupyter/ZMQ.

Run process conformance through `runtime/security/verify_operator.py`.
