# Optional `teamlab` Capability (Team Lab Hot-Reload Shelf & Cross-Agent Mailbox)

Zero-dependency, pure-Julia capability (`"dependencies": []`) that connects each agent's isolated Palette world (`claude`, `gemini`, `grok`, `codex`) to a shared, content-addressed **Team Lab Tool Shelf** and **Cross-Agent Mailbox** with **zero CLI restarts**.

## One-Line Bootstrap & Hot-Reload Sync (Works Even in Fenced Worlds)

Any agent (`grok`, `claude`, `codex`, `gemini`) can connect its Palette world and hot-load the latest QA-verified `vecindex` and `jrir` capabilities without restarting its CLI:

```julia
include("runtime/integrations/teamlab/teamlab.jl")
teamlab_sync!("grok")
```

- **`teamlab_sync!(agent)`**:
  - Checks the shared Team Lab shelf (`ENV["PALETTE_TEAMLAB_DIR"]`; see *Shared directory* below).
  - Compares each tool's SHA-256 digest (`vecindex`, `jrir`, etc.) against `_TEAMLAB_LOADED_DIGESTS` in the current Julia world.
  - Hot-reloads only new or updated QA-verified capabilities via `Base.include` (`0` CLI restarts required) and returns any unread mailbox messages for `agent`.
- **`teamlab(Dict("action" => "publish", ...))`**:
  - Runs the tool's `test_*.jl` QA gate in an isolated module, content-addresses the `.jl` entrypoint by SHA-256 into `blobs/`, updates the shelf atomically under an advisory lock, and broadcasts a notification to the team.
- **`teamlab(Dict("action" => "send" | "inbox" | "ack", ...))`**:
  - Lightweight, receipted cross-agent mailbox (`claude` ↔ `gemini` ↔ `grok` ↔ `codex`) with per-agent read acknowledgments.

## Shared directory

Set `PALETTE_TEAMLAB_DIR` to the same absolute path in every agent's Palette launcher environment.
`runtime/security/launch_worker.py` then binds it writable into each kernel sandbox and exports it
(the kernel otherwise starts with `--clearenv` and only the workspace writable). The directory is
created mode `0700`, must be owned by the launching user, must not be a symlink, and must not nest
with the workspace, saved state, protected paths, or any `PALETTE_READ_ROOTS` entry. A task env file
cannot override it. When unset, teamlab uses a per-sandbox temp directory and `status` reports
`"shared" => false`.
