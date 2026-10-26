# Palette

Palette gives any harness that accepts plugins, a Julia computer on your machine, and that computer is still running when the next message arrives.

Note: Developed on CachyOS, tested on Windows 11 via QEMU. Xeon 2699 v3, RTX 5060 8GB, 32GB DDR4

ChatGPT, Claude, Grok, and Gemini talk. Palette holds the live world they work in: bindings, loaded packages, files you wrote, journals, isolated workspaces. You can kill Palette on purpose and the next process comes up from disk, with a host report of what came back restored, rebuilt, stale, or lost. 

Every call is parsed whole before any of it runs. A call that never yields gets killed, then the kernel revives. Broker-mediated host effects leave a receipt; operator mode also has the user's normal host access.

The supervised Julia session defaults to `operator` mode. Choose `--mode sandbox`
to enter the opt-in bubblewrap lab, where filesystem and network access follow
the configured boundaries.

## Install

Linux, or WSL2 on Windows. One command installs Julia if needed, the Palette
host and its packages, and
[TeamLab](runtime/integrations/teamlab/README.md),
the shop of tools and the mailbox your agents share. It connects the Claude
Code, Codex, Grok and Antigravity installs it finds, adds the Palette plugin to
Claude Code, and checks the installation before switching to it.

```sh
curl -fsSL https://github.com/Orcein-Labs/Palette/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell or a command prompt. It sets up WSL2 with Ubuntu if
you have none, installs Palette inside it, and also connects Claude Desktop:

```powershell
powershell -c "iex ((New-Object Net.WebClient).DownloadString('https://github.com/Orcein-Labs/Palette/releases/latest/download/install.ps1'))"
```

To install Bubblewrap support and start in the opt-in sandbox, pipe the installer
to `sh` with `PALETTE_MODE=sandbox`, or pass `--mode sandbox` to a downloaded
installer. Otherwise Palette runs in operator mode. On Linux, the installer asks
for your password to install Python 3 if needed and sandbox packages when you
select sandbox mode; on Windows it needs none.
The first run takes several
minutes while it prepares Julia's packages and caches. Everything else lives in
`~/.local/share/palette`, runs locally, and needs no account. Run the command
again to update, or accept the daily update it offers.

Then start a new agent session and ask it to evaluate `x = 41`,
then `x + 1`. Other MCP clients can run `~/.local/share/palette/client/palette-mcp`
as a stdio server. The [installation guide](runtime/docs/install.md) covers
settings, a source checkout, servers and removal.

### Claude and ChatGPT apps

**Palette-ChatGPT.zip**, **Palette-Claude.zip** and the Claude Desktop extension
**Palette-Claude.mcpb** are release assets: uploadable plugins with SDK source
and setup instructions ([install guide](runtime/docs/install.md)). An
upload alone does not start a runtime. ChatGPT Chat needs a remote connection
to one; [server deployment](runtime/docs/server.md) keeps compute, packages and
state on that server.

## Use the Julia package

`using Palette` works in a normal Julia process:

```sh
julia --project=. -e 'using Pkg; Pkg.instantiate()'
julia --project=. -e 'using Palette; execute(ExecuteCode("x = 41")); println(execute(ExecuteCode("x + 1")).result.data)'
```

The plain Julia API does not create an OS sandbox, and the supervised host also
defaults to ordinary host access. Use `--mode sandbox` for the bounded OS
environment. The [installation guide](runtime/docs/install.md) shows how to run
the host session directly, which speaks newline-delimited JSON on stdio.

## Talking to the kernel

Three shell doors. Get `$` wrong and you will have a bad day.

| door | `$` meaning | use when |
|------|-------------|----------|
| `sh"cmd"` | raw text, bash owns `$` | shell-native commands, bash arithmetic |
| `bash("cmd $var")` | `$var` is Julia interpolation | splicing kernel bindings into a command |
| `bash(PAYLOAD)` | payload text goes to bash verbatim | quoting, `$`, backticks, or user text |

Default to the payload for anything non-trivial. Verify by the content of the reply. Silence is a bug signature: a healthy kernel answers every request with an event.

Each top-level statement in a call sees the methods the statements before it defined. A function body or `let` block that defines methods with `eval` or `include` cannot call them directly until it returns; use `Base.invokelatest(f, args...)`. Redefining a struct with a new layout makes a new type, and the reply names the values and methods still using the old one.

Revival labels are `restored / rebuilt / stale / lost` (and uncertain, when that is the honest answer). Tasks, IO, pointers, and values built from an earlier version of a type do not come back. Files are the durable tier. Capability grants die with the generation: re-grant after a revival.

## Features

- Persistent bindings, functions, types, packages, and workspace files.
- Whole-call parsing before execution, bounded output, deadlines, and process cleanup.
- Snapshots with dependency-aware reconstruction and preserved data aliases/cycles.
- Per-binding source observations, content checks, and opt-in artifact freshness.
- Host-mediated capabilities, workspace routing, and one-shot approved patches.
- Helpers promoted to project tools by their call evidence, listed from an index and loaded on first use; trusted only after a human approves.
- Pharos, a code graph of the workspace that re-parses only changed files and is cited in receipts.
- A repository-owned MCP adapter for clients that support local stdio servers.

## Documentation

[Plugin installation](runtime/plugins/palette/README.md) · [Operator API](runtime/docs/operator.md) · [Installation and MCP](runtime/docs/install.md) ·
[Workspaces and patches](runtime/docs/workspaces.md) · [Runtime capabilities](runtime/docs/capabilities.md) · [Authority boundaries](runtime/docs/authority.md) · [Server deployment](runtime/docs/server.md) · [Static analysis policy](runtime/docs/static-analysis.md)

## Optional operator briefing

The [operator briefing](docs/brain-blast.md) explains how to exploit persistent Julia state,
representations, compiled helpers, provenance, and controlled experiments.
Provide it as context when you want a model to start with those practices, or
withhold it when evaluating unprimed exploration. It is guidance, not a required
runtime dependency.

## Repository layout

| Directory | Contents |
| --- | --- |
| `src/` | Julia package and operator helpers |
| `runtime/` | Host, adapters, installer, worker entrypoint, public docs, and plugin package |
| `test/` | Julia package tests and fixtures |

The tracked root has six folders: the three SDK directories above, `docs/` for
project research, `.github/` for CI, and `.agents/` for the plugin marketplace. Public docs and the plugin
package live under `runtime/docs/` and `runtime/plugins/`.

Rust builds Palette's host; Python supports its adapters. Additional language
kernels and SDKs are provisioned by deployments, rather than bundled with Palette.

Models and additional toolchains use operator-configured provisioning and the
host-command broker. Their artifacts, input format and execution limits belong
to deployment configuration.

The optional [Gesso integration](runtime/integrations/gesso/README.md) exposes its
existing tokenizer, CPU inference, fork and profiling through runtime capabilities.

Two more optional capabilities ship beside it:
[vecindex](runtime/integrations/vecindex/README.md), an in-memory vector index
with exact and IVF search and a code-symbol indexer, and
[jrir](runtime/integrations/jrir/README.md), which normalizes Rust/C LLVM IR for
inlining into Julia through `Base.llvmcall`.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for scope, verification, and research archival.

```sh
julia --project=. -e 'using Pkg; Pkg.test()'
cargo test --manifest-path runtime/host/Cargo.toml --locked
node runtime/scripts/check-test-policy.mjs
```

For the complete Linux process conformance gates, prepare a Julia environment
with this checkout plus JSON, CSV, DataFrames, EzXML, and IJulia, then run:

```sh
cargo xtask verify --project-dir /absolute/path/to/test-env
```

Run it from the checkout root. `--shard I/N` runs the shared gates on shard 1
and deals the test cases across the shards by their recorded seconds
(`runtime/security/test-case-seconds.json`, refreshed from CI job logs with
`cargo xtask record-test-case-seconds`); CI runs the process gates as four shards.

CI runs package, host, Jupyter integration, workspace/patch, and process gates.
Pull requests run the process gates against the Rust host; both hosts run nightly.
The Python host is scheduled for removal ([authority](runtime/docs/authority.md)).
The package manifest declares Julia 1.10+; supervised development and CI are tested on Julia 1.12.

## Explanation for why not just IPython or IJulia or Jupyter

The reason I developed this and didn't simply wrap one of these three, is that Jupyter and IJulia were engineered as chronological presentation layers for human eyes. They treat code execution as a linear notebook, forcing the runtime to store a historical text stream of every cell input and output. If you force an AI agent to use a Jupyter wrapper, it rapidly suffocates its own context window with its own debugging history. Palette.jl is not a notebook UI wrapper; it is a non-linear state architecture built natively for LLM runtimes. Paired with Pharos.jl, it separates code execution logs from semantic working memory. It allows an autonomous agent to spawn disposable Julia sub-scopes, programmatically inspect the project's AST graph, and prune its variable footprint dynamically. These child scopes keep the selected operator or sandbox mode. I didn't build on Jupyter because you can't solve a non-linear, token-constrained machine-learning problem using a linear, human-centric logging protocol. Also, IPython is a scratchpad meant for humans to type lines sequentially. If you give an LLM an IPython cell, it kinda behaves like an automated chatbot trying to navigate a dark room by feeling the walls and there's a rake in the walkway for some reason. Prime-Agent does exceptionally well with IPython, but it showed where the operator surface friction still lives.


## License

**AGPL-3.0-only**. Inherited MIT notices remain preserved. See
[license boundaries](runtime/licenses/README.md) and [LICENSE](LICENSE).

## Provider terms

Palette is a local tool you attach to a hosted model over its plugin or MCP path. Review your provider's current terms yourself before you do; this README is not a reading of them. Whatever usage your plan includes, Palette lets it go into work that survives the thread, persists into a new one, and lets ChatGPT (or whatever you attach) keep the world.
