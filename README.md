# Palette

Palette gives any plugin- or MCP-capable agent a persistent Julia session on your machine, so variables, compiled methods, loaded packages, and workspace files survive across turns.

Developed on CachyOS and tested on Windows 11 via QEMU (Xeon 2699 v3, RTX 5060 8GB, 32GB DDR4).

ChatGPT, Claude, Grok, and Gemini all have the same problem: their chat threads are stateless, so every tool call starts cold or dumps a wall of logs back into context. Palette keeps the live Julia world on disk and in RAM across calls—bindings, compiled modules, isolated workspaces, and a Pharos code graph of the repository. If you kill the process (or a non-yielding call gets reaped), the next process restores from disk and reports exactly what came back `restored`, `rebuilt`, `stale`, or `lost`.

Every call is parsed as a whole before any of it runs. Broker-mediated host effects leave a receipt. The supervised session defaults to `operator` mode (normal user host access), with `--mode sandbox` available for an opt-in Bubblewrap lab.

## Install

Linux, or WSL2 on Windows. One command installs Julia if needed, the Palette host and its packages, and [TeamLab](runtime/integrations/teamlab/README.md). It hooks into any Claude Code, Codex, Grok, and Antigravity installs it finds, adds the Palette plugin to Claude Code, and verifies the setup:

```sh
curl -fsSL https://github.com/Orcein-Labs/Palette/releases/latest/download/install.sh | sh
```

On Windows (PowerShell or cmd), this sets up WSL2 with Ubuntu if needed, installs Palette inside it, and connects Claude Desktop:

```powershell
powershell -c "iex ((New-Object Net.WebClient).DownloadString('https://github.com/Orcein-Labs/Palette/releases/latest/download/install.ps1'))"
```

To install Bubblewrap support and start in the opt-in sandbox, set `PALETTE_MODE=sandbox` or pass `--mode sandbox`. First run takes a few minutes to precompile Julia packages and caches. Everything lives under `~/.local/share/palette`, runs locally, and needs no account. Re-run the installer to update.

Start a session and ask the agent to evaluate `x = 41`, then `x + 1`. Other MCP clients can run `~/.local/share/palette/client/palette-mcp` as a stdio server. See the [installation guide](runtime/docs/install.md) for settings, source checkouts, and removal.

### Claude and ChatGPT apps

**Palette-ChatGPT.zip**, **Palette-Claude.zip**, and **Palette-Claude.mcpb** ship as release assets with SDK source and setup instructions ([install guide](runtime/docs/install.md)). ChatGPT Chat requires a remote runtime; see [server deployment](runtime/docs/server.md).

## Use the Julia package

`using Palette` also works directly from standard Julia:

```sh
julia --project=. -e 'using Pkg; Pkg.instantiate()'
julia --project=. -e 'using Palette; execute(ExecuteCode("x = 41")); println(execute(ExecuteCode("x + 1")).result.data)'
```

The plain Julia API does not create an OS sandbox, and the supervised host defaults to ordinary host access. Use `--mode sandbox` for a bounded Bubblewrap environment. The [installation guide](runtime/docs/install.md) documents running the host directly over newline-delimited JSON on stdio.

## Talking to the kernel

Three shell doors—get `$` right or quoting will bite you:

| door | `$` meaning | use when |
|------|-------------|----------|
| `sh"cmd"` | raw text, bash owns `$` | shell-native commands, bash arithmetic |
| `bash("cmd $var")` | `$var` is Julia interpolation | splicing kernel bindings into a command |
| `bash(PAYLOAD)` | payload text goes to bash verbatim | quoting, `$`, backticks, or user text |

Default to the payload for anything non-trivial, and verify by the reply content. Silence is a bug signature: a healthy kernel answers every request with an event.

Top-level statements in a call see methods defined earlier in the same call. Inside a function body or `let` block, newly `eval`/`include`-defined methods need `Base.invokelatest(f, args...)` until the block returns. Redefining a struct with a new layout creates a new type, and Palette reports which existing values and methods still reference the old one.

Revival labels are `restored`, `rebuilt`, `stale`, `lost`, and `uncertain`. Tasks, IO handles, raw pointers, and values from superseded struct layouts do not revive; files are the durable tier, and capability grants expire with the kernel generation.

## Features

- **Persistent Julia state & revival:** Bindings, functions, types, packages, and workspace files survive restarts with dependency-aware reconstruction, preserved object aliasing/cycles, `Api.bind_mmap` for large read-only disk-backed arrays, `>16 MiB` sidecar spill, and `@ephemeral` for RAM-only bindings (like loaded model weights) that shouldn't bloat snapshots.
- **Hot-reload without restarting or polluting `Main`:** `Api.reinclude_module!` and `Api.reload_methods!` let you edit and reload modules or methods in place inside isolated sandbox modules (`reset_bindings!`), keeping large models resident in RAM while iterating on decoders or training loops.
- **Built-in profiling & type-stability probes:** `Api.@bench` / `Palette.bench` (warmup + allocation + latency), `Api.warntype_summary`, `@alloc_barrier`, `Api.set_blas_threads!`, and `Api.serve_unix_socket` for warm-worker IPC.
- **Pharos code graph:** Workspace AST indexer with incremental SHA-256 refresh, `Pharos.summary` / `Api.summary` compact overviews, `Api.definitions` / `Api.callers`, and persistent index caching (`save_index` / `load_index` / `load_or_build`).
- **Safety, isolation & supervision:** Whole-call parsing before execution, bounded output, deadlines with child-process cleanup, `Api.eval_isolated`, Private Lab single-root `PALETTE_HOME` containment (`check_private_isolation`), and `Joovy` integration.
- **Promotable helpers & receipts:** Helpers used across 3 successful calls can be promoted to `.palette/tools` (`Api.promote`), loaded on first use, and marked trusted after human approval.

## Field Results & Dogfooding

Palette was built and hardened directly against multi-agent R&D workloads across the Orcein Stack (`BioForge`, `Somata` / `CNIR`, `Luxel`):

- **Live Model X-Ray (`BioForge` Grail V4.2 $\to$ V4.3):** Instead of eating ~45-second cold-start Julia/ModernBERT reloads on every debugging question, agents kept `ModernBERT-base`, `ModernBERT-base-nli`, and all `K=3` trained heads hot in RAM inside a scoped `palette_workspace`. Probing individual failing sentences took **~23 ms per query**, exposing cross-clause neighbor bleed, poisoned multi-clause labels, and passive ordering gaps, and proving a directional NLI symmetry check on the *existing broken V4.2 weights* (cutting blind errors from `49 -> 3`) before spending ~50 minutes retraining V4.3 (`val_unseen_A` `0.9160` GE at `0.0033` risk; blind `val_unseen_B` `0.8984` GE at `0.0000` risk). See [the Live X-Ray Protocol write-up](docs/research/2026-11-24_grail-xray-protocol.md).
- **Five Dogfooding Waves (`test/test_dogfood_wave1.jl` – `wave5.jl`):** Friction reported by agents during `Luxel`, `Somata`, and `BioForge` runs drove `Pharos` session exports and `summary`, `Api.reload_methods!` / `reinclude_module!`, `@ephemeral` + `bind_mmap`, `Api.@bench` / `warntype_summary` / `@alloc_barrier`, BLAS thread controls, and Private Lab containment.

## Documentation

[Plugin installation](runtime/plugins/palette/README.md) · [Operator API](runtime/docs/operator.md) · [Installation and MCP](runtime/docs/install.md) ·
[Workspaces and patches](runtime/docs/workspaces.md) · [Runtime capabilities](runtime/docs/capabilities.md) · [Authority boundaries](runtime/docs/authority.md) · [Server deployment](runtime/docs/server.md) · [Static analysis policy](runtime/docs/static-analysis.md)

## Optional operator briefing

The [operator briefing](docs/brain-blast.md) covers practical habits for working in a persistent Julia session (keeping hot paths compiled, tracking provenance, and isolating baselines). Load it when you want a model to start with those habits, or leave it out when testing unprimed behavior.

## Repository layout

| Directory | Contents |
| --- | --- |
| `src/` | Julia package, Pharos code graph, revival/snapshots, and operator helpers |
| `runtime/` | Rust host, MCP adapter, installer, worker entrypoint, docs, and plugin package |
| `test/` | Julia package tests, dogfooding wave suites, and fixtures |

The tracked root also includes `docs/` for design notes and field reports, `.github/` for CI, and `.agents/` for the plugin marketplace.

Rust builds Palette's host; Python supports legacy/external adapters. Optional integrations ship alongside it: [Gesso](runtime/integrations/gesso/README.md), [Joovy](runtime/integrations/joovy/), [vecindex](runtime/integrations/vecindex/README.md), and [jrir](runtime/integrations/jrir/README.md).

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for scope, verification, and research archival.

```sh
julia --project=. -e 'using Pkg; Pkg.test()'
cargo test --manifest-path runtime/host/Cargo.toml --locked
node runtime/scripts/check-test-policy.mjs
```

For the full Linux process conformance gates, prepare a Julia environment with this checkout plus `JSON`, `CSV`, `DataFrames`, `EzXML`, and `IJulia`, then run:

```sh
cargo xtask verify --project-dir /absolute/path/to/test-env
```

`--shard I/N` splits the process gates across shards using recorded timings (`runtime/security/test-case-seconds.json`). The package manifest declares Julia 1.10+; CI and supervised runs target Julia 1.12.

## Why not just Jupyter, IJulia, or IPython?

Jupyter and IJulia are linear presentation layers built for humans scrolling through notebooks—they append every cell input and output into a chronological log. Hook an LLM agent up to a notebook wrapper and its context window quickly fills up with its own debugging stdout. IPython has the same scratchpad problem: give an LLM a raw stateful cell with no code graph or scoped resets, and it behaves like a chatbot feeling its way around a dark room with a rake in the walkway.

Palette is built for tool-calling agents instead: bounded outputs, scoped workspaces, hot-reloadable modules that don't wedge `Main` when a struct changes, Pharos AST queries so the agent doesn't grep blind, and explicit revival reports so it knows what survived a restart.

## License

**AGPL-3.0-only**. Inherited MIT notices remain preserved. See
[license boundaries](runtime/licenses/README.md) and [LICENSE](LICENSE).

## Provider terms

Palette is a local tool you attach to a hosted model over its plugin or MCP path. Review your provider's current terms yourself before doing so.
