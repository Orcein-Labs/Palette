# Dogfood friction log

Friction found while building and dogfooding the [finish plan](2026-10-18_palette-finish-plan.md).
Each entry is fixed, assigned to a plan item, or deliberately deferred.

Phase A item 1 (long work) was built through Bash, because calls over 60 s
were the blocker it removes. Later items are built through Palette where it can.

| Found in | Friction | Status |
|---|---|---|
| A1 | Session tests in a fresh worktree fail until the Julia project is instantiated by hand. | Fixed in A2: setup instantiates before it prewarms. |
| A1 | The default package store under the home directory is refused by the protected-path ceiling, so tests need `PALETTE_PACKAGE_DEPOT` set elsewhere. | Fixed in A2 for installs: the store defaults to `~/.local/share/palette/data/packages`. A host command whose directory contains that path still blocked it. Fixed in A3: a command path's parent refuses only roots that equal or contain it, so a store beside a command directory is allowed. |
| A1 | A call stopped at its limit reports `Interrupted during completed`: the phase name is wrong. | Plan item A5 (name the execution phase on timeout). |
| A1 | A scratch (`ephemeral`) call or spawned child stops at the session default less 10 s: the broker's child timeout is fixed per session. A scratch call with a shorter `timeout_s` killed the kernel waiting on it. | Hosts refuse `timeout_s` on a scratch call. Per-call child bounds are deferred and documented in `runtime/docs/authority.md`; long scratch work runs as a host command job. |
| A1 | The jobs line prints at the start of a call, before its code runs, so a job started or cancelled in one call shows up in the next call's output. | Deferred: the line reports the state the call starts from. |
| A1 | The server launcher's restricted environment dropped any limit set in the client environment. | Fixed: server profiles take `turn_timeout` and `max_turn_timeout`. |
| A1 | A package store under `/tmp` is refused as overlapping a protected path when a host command's path is under `/tmp`, so the Rust host command tests fail with such a store. | Installs no longer default under `/tmp` (A2). Fixed in A3: a store and a command in sibling directories no longer overlap. |
| A1 | Finishing a job held the jobs lock while it joined output readers, so a job whose grandchild kept a pipe open would stall every later call's jobs line. | Fixed: jobs finish outside the lock; the jobs line waits at most 10 s. |

## First-run friction

Found by reading the public repository the way a new user does, through a
general-purpose assistant's summary of it. Where the README says nothing,
the summary invented an answer: a "Palette Desktop" app, `refs_*` tools and a
browser approval step, none of which exist.

| Friction | Status |
|---|---|
| The README's quick start lists four prerequisites, a Rust build and a minutes-long prewarm as separate commands. The one-command path, `runtime/plugins/setup.py`, is described only inside the release bundles. | Fixed in A2: `install.sh` and `install.ps1`, led by the README. |
| No prebuilt host binary, so every install needs Rust. The only release is a draft, so the ZIPs the README points to are not downloadable. | A2: the release workflow publishes the host, checksums and bundles on each tag. Open until the first full release is tagged. |
| Nothing says how to attach Palette to Claude Code. `setup.py` writes `generic-mcp.json`, but the `claude mcp add` step is left to the reader. | Fixed in A2: the installers register Claude Code, Codex and Claude Desktop, leaving an existing `palette` entry alone. |
| An agent asked to install Palette runs many separate build commands, so it stops for approval at each one. | Fixed in A2: one command. |

## Installer end-to-end runs

Found running the piped installer against a local copy of a release, with an
empty home directory and only Claude Code and Codex on the path.

| Friction | Status |
|---|---|
| `cargo` was on the path as a rustup proxy with no toolchain, so setup tried to rebuild the shipped host and failed. | Fixed: a release archive with a host that runs is never rebuilt; a checkout still builds its own. |
| The installer's check that the shipped host runs expected the usage line at the start of the output. The host prefixes it with its name, so a working host was discarded. The release workflow's check had the same pattern, and it also ran under `pipefail`, where the host's nonzero exit fails the step. | Fixed in both. |
| Preparation and plugin installation printed their machine-readable JSON records, several kilobytes, in the middle of the install. | Fixed: shown only when the step fails. |
| The test call checked for `2` in the reply text, but the reply is a JSON record with the value under `data`. | Fixed: the record is parsed. |
| A failed step printed a Python traceback. | Fixed: one line naming the step; the step's own output above it says why. |
| A rerun reported its own earlier registration as a conflicting server. | Fixed: it says the client already uses this installation. |
| Without a terminal, the sudo branch was still taken, because `/dev/tty` is readable even when it cannot be opened. | Fixed: the installer opens it to test. |
| Windows: an npm-installed `claude` resolves to a `.ps1` shim that the default execution policy blocks. An empty Claude Desktop configuration file stopped the merge. | Fixed in `install.ps1`; untested on Windows until a run on a real machine. |

The first package install prints nothing for about 150 s while it fetches the
registry; the elapsed-time line covers it. juliaup's current release, Julia
1.13.2, installed and ran Palette with no changes.

## Windows end-to-end run

A fresh Windows 11 Pro virtual machine with no WSL ran the one-line
`install.ps1` against a local copy of the release. The first run enabled the
WSL platform and asked for a restart. The second installed Ubuntu, created the
Linux user, installed the sandbox packages as root, downloaded and checked the
release, installed Julia 1.13.2 with juliaup, prepared the sandbox and passed
the `1 + 1` call. On this machine's slow virtual disk, the package step took
about 15 min and sandbox preparation about 7 min. No Claude client was
installed in the machine, so the Windows registration path is still untested.

| Friction | Status |
|---|---|
| After creating the Linux user, `wsl --install` leaves the user at an Ubuntu prompt, and the installer waits behind it with nothing saying so. | Fixed: the installer says to type `exit` at that prompt. |
| Installing curl inside an existing distribution assumed apt. | Fixed: apt, dnf, pacman or zypper. |
| `install.sh` printed its Linux closing message, including `rm -rf` removal steps, before the Windows one. | Fixed: under `install.ps1` only the Windows next steps print. |

## A3: host commands

| Friction | Status |
|---|---|
| A turn could not find out which host commands it may run; it had to be told the names. | Fixed: `Palette.host_commands()` lists them, with an optional `description` per command. The tool descriptions name it. |
| GPU work through a host command did not take the machine's GPU lock (the converge3 render script takes none), so it could share the GPU with Codex jobs. | Fixed in the host: a command's `lock` file wraps it in `flock --exclusive`; the wait counts against its timeout. The live config gets the lock when this host is installed for the Phase A exit. |
| A3 was built through Bash: building Palette through Palette needs host commands for `cargo test` and the Python suites in this worktree, and none are configured. | Open: configure dev host commands for the worktree before A4. |

## A4: CI time

Before this round, `security-suite` ran unsharded and took 50 to 90 min. One
run hung for 90 min while preparing packages inside the sandbox, and one failed
because ZMQ did not precompile there. Most of the time went to `test_session_cli`
(11.1 min) and `test_revival` (11.0 min), where each test starts a fresh
session, and to prewarm (4.4 min).

| Friction | Status |
|---|---|
| `TestReadRoots` and `TestHostCommand` in `test_session.py` were defined after `unittest.main()`, so they had not run in CI since #22. | Fixed: the main block moved to the end, and `check-test-policy.mjs` refuses a test class defined after it. |
| One job ran every suite in turn. | Fixed: four shards. Each took 17 to 23 min without a cache. |
| Every shard prepared the package depot from nothing. | Fixed: shard 1 saves the prewarmed depot to the Actions cache, and every shard restores it. Shards then took 7 to 9 min, except shard 1 at 15.5 min. |
| Shard 1 also runs the shared gates (Julia and Rust unit tests, test policy), which took its gates step to 14 min against 6 to 7 min on the others. | Fixed: `verify_operator.py` deals test cases across the shards by their recorded seconds (`test-case-seconds.json`, refreshed from CI logs by `record-test-case-seconds.py`), with shard 1 starting 90 s behind. The recorded test time per Rust shard is now about 500 s, against 740 s on the old shard 1. The gate (under 15 min, green on 10 consecutive runs) is measured after the next push. |
