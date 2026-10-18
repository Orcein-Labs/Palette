# Dogfood friction log

Friction found while building and dogfooding the [finish plan](2026-10-18_palette-finish-plan.md).
Each entry is fixed, assigned to a plan item, or deliberately deferred.

Phase A item 1 (long work) was built through Bash, because calls over 60 s
were the blocker it removes. Later items are built through Palette where it can.

| Found in | Friction | Status |
|---|---|---|
| A1 | Session tests in a fresh worktree fail until the Julia project is instantiated by hand. | Fixed in A2: setup instantiates before it prewarms. |
| A1 | The default package store under the home directory is refused by the protected-path ceiling, so tests need `PALETTE_PACKAGE_DEPOT` set elsewhere. | Fixed in A2 for installs: the store defaults to `~/.local/share/palette/data/packages`. A host command whose directory contains that path still blocks it (plan item A3). |
| A1 | A call stopped at its limit reports `Interrupted during completed`: the phase name is wrong. | Plan item A5 (name the execution phase on timeout). |
| A1 | A scratch (`ephemeral`) call or spawned child stops at the session default less 10 s: the broker's child timeout is fixed per session. A scratch call with a shorter `timeout_s` killed the kernel waiting on it. | Hosts refuse `timeout_s` on a scratch call. Per-call child bounds are deferred and documented in `runtime/docs/authority.md`; long scratch work runs as a host command job. |
| A1 | The jobs line prints at the start of a call, before its code runs, so a job started or cancelled in one call shows up in the next call's output. | Deferred: the line reports the state the call starts from. |
| A1 | The server launcher's restricted environment dropped any limit set in the client environment. | Fixed: server profiles take `turn_timeout` and `max_turn_timeout`. |
| A1 | A package store under `/tmp` is refused as overlapping a protected path when a host command's path is under `/tmp`, so the Rust host command tests fail with such a store. | Installs no longer default under `/tmp` (A2). Test runs still set the store outside it. |
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
