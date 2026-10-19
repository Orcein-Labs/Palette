# Palette

Palette gives any harness that accepts plugins, a Julia computer on your machine, and that computer is still running when the next message arrives.

Note: Developed on CachyOS, tested on Windows 11 via QEMU. Xeon 2699 v3, RTX 5060 8GB, 32GB DDR4

ChatGPT, Claude, Grok, and Gemini talk. Palette holds the live world they work in: bindings, loaded packages, files you wrote, journals, isolated workspaces. Kill Julia on purpose and the next process comes up from disk, with a host report of what came back restored, rebuilt, stale, or lost. Trust that report. Re-derive anything it marks lost.

Every call is parsed whole before any of it runs. A call that never yields gets killed, then the kernel revives, and host-side effects go through a capability broker and leave a receipt.

## ChatGPT Chat, October 2026

This is the default ChatGPT chat product. The UI labels it Chat.

<p align="center">
  <img src="runtime/docs/images/chatgpt-chat-mode.png" alt="ChatGPT Chat mode, confirmed in the UI, driving Palette" width="48%" />
  <img src="runtime/docs/images/chatgpt-os-lab.png" alt="ChatGPT Chat installing Metatheory.jl through Palette in real time" width="48%" />
</p>

The right-hand capture is ChatGPT Chat installing Metatheory.jl through Palette while the kernel stays up. Isolated research environment, Gesso's dependencies left alone.

The workspace is a separate research world, kept off the default world, and Palette runs disposable Julia processes against that same tree. Process A writes a World and dies. Process B starts empty and reconstructs identity, authority, provenance, and the journal from disk, independent of Palette's in-memory revival.

We've run long-horizon R&D spirals on this desk. Bugs showed up and they were able to be fixed from the inside. The World was still on disk after the kernel came back.

Clients attach over MCP, or over the host session (newline-delimited JSON on stdio). The kernel is local. Talk to whichever model you already use.

## Install

Linux, or WSL2 on Windows. One command installs Julia if needed, the sandbox
host and its packages, then connects Claude Code and Codex and checks one call.

```sh
curl -fsSL https://github.com/latentcollapse/Palette/releases/latest/download/install.sh | sh
```

On Windows, in PowerShell or a command prompt. It sets up WSL2 with Ubuntu if
you have none, installs Palette inside it, and also connects Claude Desktop:

```powershell
powershell -c "iex ((New-Object Net.WebClient).DownloadString('https://github.com/latentcollapse/Palette/releases/latest/download/install.ps1'))"
```

The installer asks for your password once, to install bubblewrap and Python 3
from your distribution; on Windows it needs none. The first run takes several
minutes while it prepares the sandbox. Everything else lives in
`~/.local/share/palette`, runs locally, and needs no account. Run the command
again to update.

Then start a new Claude Code or Codex session and ask it to evaluate `x = 41`,
then `x + 1`. Other MCP clients can run `~/.local/share/palette/client/palette-mcp`
as a stdio server. The [installation guide](runtime/docs/install.md) covers
settings, a source checkout, servers and removal.

### Claude and ChatGPT apps

**Palette-ChatGPT.zip**, **Palette-Claude.zip** and the Claude Desktop extension
**Palette-Claude.mcpb** are release assets: uploadable plugins with SDK source
and setup instructions ([client guide](runtime/plugins/DISTRIBUTION.md)). An
upload alone does not start a runtime. ChatGPT Chat needs a remote connection
to one; [server deployment](runtime/docs/server.md) keeps compute, packages and
state on that server.

## Use the Julia package

`using Palette` works without the sandbox:

```sh
julia --project=. -e 'using Pkg; Pkg.instantiate()'
julia --project=. -e 'using Palette; execute(ExecuteCode("x = 41")); println(execute(ExecuteCode("x + 1")).result.data)'
```

The plain Julia API does not create an OS sandbox; the supervised host does.
The [installation guide](runtime/docs/install.md) shows how to run the host
session directly, which speaks newline-delimited JSON on stdio.

## Talking to the kernel

Three shell doors. Get `$` wrong and you will have a bad day.

| door | `$` meaning | use when |
|------|-------------|----------|
| `sh"cmd"` | raw text, bash owns `$` | shell-native commands, bash arithmetic |
| `bash("cmd $var")` | `$var` is Julia interpolation | splicing kernel bindings into a command |
| `bash(PAYLOAD)` | payload text goes to bash verbatim | quoting, `$`, backticks, or user text |

Default to the payload for anything non-trivial. Verify by the content of the reply. Silence is a bug signature: a healthy kernel answers every request with an event.

Revival labels are `restored / rebuilt / stale / lost` (and uncertain, when that is the honest answer). Tasks, IO, pointers, and values built from an earlier version of a type do not come back. Files are the durable tier. Capability grants die with the generation: re-grant after a revival.

## Features

- Persistent bindings, functions, types, packages, and workspace files.
- Whole-call parsing before execution, bounded output, deadlines, and process cleanup.
- Snapshots with dependency-aware reconstruction and preserved data aliases/cycles.
- Per-binding source observations, content checks, and opt-in artifact freshness.
- Host-mediated capabilities, workspace routing, and one-shot approved patches.
- A repository-owned MCP adapter for clients that support local stdio servers.

## Documentation

[Plugin installation](runtime/plugins/palette/README.md) · [Operator API](runtime/docs/operator.md) · [Installation and MCP](runtime/docs/install.md) ·
[Workspaces and patches](runtime/docs/workspaces.md) · [Runtime capabilities](runtime/docs/capabilities.md) · [Authority boundaries](runtime/docs/authority.md) · [Server deployment](runtime/docs/server.md) · [Static analysis policy](runtime/docs/static-analysis.md)

## Optional operator briefing

[BRAIN_BLAST.md](BRAIN_BLAST.md) explains how to exploit persistent Julia state,
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

CI runs package, host, Jupyter integration, workspace/patch, and process gates.
Pull requests run the process gates against the Rust host; both hosts run nightly.
The Python host is scheduled for removal ([authority](runtime/docs/authority.md)).
The package manifest declares Julia 1.10+; supervised development and CI are tested on Julia 1.12.

## License

Core: **AGPL-3.0-only**. Identified client/plugin packages and public docs:
**Apache-2.0**. Inherited MIT notices remain preserved. See
[license boundaries](runtime/licenses/README.md) and [LICENSE](LICENSE).

## Provider terms

Palette is a local tool you attach to a hosted model over its plugin or MCP path. Review your provider's current terms yourself before you do; this README is not a reading of them. Whatever usage your plan includes, Palette lets it go into work that survives the thread, persists into a new one, and lets ChatGPT (or whatever you attach) keep the world.
