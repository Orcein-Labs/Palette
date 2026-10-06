# Palette

A persistent Julia operator surface (conceptually similar to Bash or IPython, or could also be equated to DaemonMode.jl or Pluto.jl) for interactive work and agent harnesses.
Palette keeps bindings across calls, captures results and process output, and
revives saved state after a kernel restart with an explicit report of what
survived, changed, or was lost. It's Jupyter on steroids, basically.

Palette supplies the execution surface. Harnesses such as Prime-Agent or Hermes provide the
agent loop, model inference, and orchestration.

## Use Palette in your chatbot

Get **Palette-ChatGPT.zip** or **Palette-Claude.zip** from the release assets
when published. Each is one uploadable plugin with SDK source and setup
instructions. Claude Desktop extensions use the separate `.mcpb` option. Read the [client installation guide](runtime/plugins/DISTRIBUTION.md).
Linux/WSL2 preparation is required; ChatGPT Chat additionally needs your own
tunnel or hosted connection. ZIP upload alone does not start a runtime.

## Quick start

Import the Julia package with `using Palette`.

```sh
julia --project=. -e 'using Pkg; Pkg.instantiate()'
julia --project=. -e 'using Palette; execute(ExecuteCode("x = 41")); println(execute(ExecuteCode("x + 1")).result.data)'
```

For supervised, sandboxed sessions on Linux, install Julia 1.12, Python 3,
bubblewrap, and a Rust build toolchain, then build the host:

```sh
cargo build --manifest-path runtime/host/Cargo.toml --release --locked
python3 runtime/security/prewarm_depot.py --project-dir "$PWD" --host-bin "$PWD/runtime/host/target/release/palette-host"
runtime/host/target/release/palette-host session --repo-dir "$PWD" --project-dir "$PWD" --workspace-dir /absolute/path/to/workspace --state-dir /absolute/path/to/state --ceiling '{}'
```

The session speaks newline-delimited JSON on stdin/stdout. After its startup
message, send `{"request_id":"1","code":"x = 41"}` and then
`{"request_id":"2","code":"x + 1"}`. Close stdin to stop it.
Keep state outside the workspace. The plain Julia API above does not create an
OS sandbox; the supervised host does.

## Features

- Persistent bindings, functions, types, packages, and workspace files.
- Whole-call parsing before execution, bounded output, deadlines, and process cleanup.
- Snapshots with dependency-aware reconstruction and preserved data aliases/cycles.
- Per-binding source observations, content checks, and opt-in artifact freshness.
- Host-mediated capabilities, workspace routing, and one-shot approved patches.
- A repository-owned MCP adapter for clients that support local stdio servers.

## Documentation

[Plugin installation](runtime/plugins/palette/README.md) · [Operator API](runtime/docs/operator.md) · [Installation and MCP](runtime/docs/install.md) ·
[Workspaces and patches](runtime/docs/workspaces.md) · [Authority boundaries](runtime/docs/authority.md) · [Static analysis policy](runtime/docs/static-analysis.md)

## Optional operator briefing

[BRAIN_BLAST.md](BRAIN_BLAST.md) explains how to exploit persistent Julia state,
representations, compiled helpers, provenance, and controlled experiments.
Provide it as context when you want a model to start with those practices, or
withhold it when evaluating unprimed exploration. It is guidance, not a required
runtime dependency. You can use it as a seed for curated training examples;
validate those examples and evaluate a tuned model on held-out tasks rather than
assuming that fine-tuning on this prose improves performance.

## Repository layout

| Directory | Contents |
| --- | --- |
| `src/` | Julia package and operator helpers |
| `runtime/` | Host, adapters, installer, worker entrypoint, public docs, and plugin package |
| `test/` | Julia package tests and fixtures |

The tracked root has five folders: the three SDK directories above, `.github/`
for CI, and `.agents/` for the plugin marketplace. Public docs and the plugin
package live under `runtime/docs/` and `runtime/plugins/`.

Rust builds Palette's host; Python supports its adapters. Additional language
kernels and SDKs are provisioned by deployments, rather than bundled with Palette.

An operator-provisioned [local VibeThinker-3B](runtime/docs/vibethinker.md) can
answer prompts through the host-command broker using llama.cpp and a GGUF model.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for scope, verification, and research archival.

```sh
julia --project=. -e 'using Pkg; Pkg.test()'
cargo test --manifest-path runtime/host/Cargo.toml --locked
python3 runtime/scripts/test_julia_analyzer.py
node runtime/scripts/check-test-policy.mjs
```

For the complete Linux process conformance gates, prepare a Julia environment
with this checkout plus JSON, CSV, DataFrames, EzXML, and IJulia, then run:

```sh
python3 runtime/security/verify_operator.py --project-dir /absolute/path/to/test-env --implementation both
```

Internal development research and run receipts belong in the gitignored
`.archive/` directory or a private external archive. Keep revisions, hashes,
attribution, and failed attempts for future research publication. Public `runtime/docs/`
contains supported product documentation.

CI runs package, host, Jupyter integration, workspace/patch, and process gates.
The package manifest declares Julia 1.10+; supervised development and CI are tested on Julia 1.12.

## License

Core: **AGPL-3.0-only**. Identified client/plugin packages and public docs:
**Apache-2.0**. Inherited MIT notices remain preserved. See
[license boundaries](runtime/licenses/README.md) and [LICENSE](LICENSE).
