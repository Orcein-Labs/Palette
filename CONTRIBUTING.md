# Contributing to Palette

Palette owns its Julia operator API, supervised host, portable MCP adapters, and
public documentation. Harness orchestration and deployment-specific language
kernels belong in separate projects.

Start with the [README](README.md), [installation guide](runtime/docs/install.md), and
[development rules](AGENTS.md). Work on a feature branch and describe a concrete
trigger, resulting behavior, and actual verification in the pull request.
Preserve supported behavior and keep changes focused.

Run Julia package tests for Julia changes, Rust tests for host changes, and each
modified test file directly. Adapter and process changes need real integration
checks against the affected host implementations. The full Linux verification
entrypoint is `cargo xtask verify`; its test environment must
use this checkout. Required checks must pass before merging. Whole-package JET
analysis currently follows the [advisory policy](runtime/docs/static-analysis.md).

Keep internal experiments, raw run receipts, exploratory reports, and development
research in the gitignored `.archive/` or a private external archive. Preserve
revisions, environment versions, hashes, attribution, and failed attempts for
future research publication. Public `runtime/docs/` should explain supported behavior,
installation, and limits. Never include credentials or private deployment state.

The optional [operator briefing](BRAIN_BLAST.md) helps users discover how to work
in a persistent Julia world; its examples should stay executable and its claims
should match the current implementation.

## Core and ecosystem scope

Core contributions focus on correctness, recovery, isolation, performance,
portable installation, and stable operator contracts. New language runtimes,
research toolkits, UI extensions, and agent orchestration should be independently
installable plugins. Propose a concrete Core contract change before building a
large feature into the kernel. Maintainers may redirect features to the ecosystem.

Contribute Palette-owned Core changes under AGPL-3.0-only and identified client,
plugin, and documentation changes under Apache-2.0, as specified in
[runtime/licenses/README.md](runtime/licenses/README.md). Retain third-party
attribution and compatible license notices. No copyright assignment is required.
A dedicated Apache-2.0 community-plugin repository is planned; until it is
established, host plugins independently and document their supported contracts.
