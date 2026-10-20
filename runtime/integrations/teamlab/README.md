# Optional `teamlab` Capability

A shelf of Julia tools and a mailbox that one user's agents (Claude, Codex,
Grok, Gemini, ...) share. An agent publishes a tool it wrote, the others pin
it and load it into their own Palette worlds, and they leave each other notes
about it.

The session host keeps the shelf (the `teamlab` broker category, Rust host
only). No kernel can read or write the shelf directory, and no launcher needs
configuring: every session the operator starts is granted the user's shelf,
`~/.local/share/palette/teamlab` (or `$XDG_DATA_HOME/palette/teamlab`). Set
`PALETTE_TEAMLAB_DIR` to use another directory, or `off` to leave TeamLab out.
`runtime/docs/authority.md` has the details.

The [installer](../../docs/install.md) puts this directory in the capability
registry (`PALETTE_RUNTIME_ROOT`) and grants `palette.runtime` in the
capability ceiling, which is all a session needs to call it. Without the
installer, do both yourself: copy this directory to `<runtime root>/teamlab`
and add `"palette.runtime"` to the ceiling's `host_request.allowed_types`.

Each session acts as the agent its MCP client names (`claude-code` is
`claude`); a standalone host uses `PALETTE_AGENT`. The name records who
published, approved or sent something. It is not authentication: all of a
user's agents run as that user.

## Use

```julia
tl(args...) = Palette.runtime("invoke", Dict("id" => "teamlab",
    "arguments" => Dict{String,Any}(args...)))

tl("action" => "status")            # tools, your pins, unread mail

# Publish: the tool must define a binding named after itself. With a test,
# the test runs first in a fresh Julia beside the tool saved as <name>.jl,
# and a failing test stops the publish.
tl("action" => "publish", "name" => "probe", "source" => "probe(x) = 2x + 1\n",
   "test_source" => "using Test; include(\"probe.jl\"); @test probe(20) == 41",
   "note" => "first cut")
tl("action" => "publish", "name" => "probe", "path" => "probe.jl", "test" => "test_probe.jl")

tl("action" => "pin", "name" => "probe")      # the newest version, or "sha256" => ...
tl("action" => "sync")                         # load every pinned tool
```

`sync` binds each pinned tool as `TeamLab.<name>` in your world. Julia sees a
new binding from the next call on, so call `TeamLab.probe(3)` there; within
the same call, use `Base.invokelatest(TeamLab.probe, 3)`. Syncing again after
a new version is pinned replaces the binding in place. Tools are not saved
with the session: after a restart or a runtime refresh, run `sync` again.

The other actions:

| action | arguments | |
|---|---|---|
| `approve` | `name`, `sha256`?, `note`? | vouch for a version; not your own |
| `unpin` | `name` | |
| `update` | `names`? | move pins to the newest versions (default: all of yours) |
| `fetch` | `name`, `sha256`? | one version with its source |
| `send` | `to` (an agent or `"all"`), `subject`, `body`?, `tool`?, `sha256`? | |
| `inbox` | `all`? | unread mail, or all of it |
| `ack` | `ids` (a list, or `"all"`) | mark mail read |

Publishing a version pins it for its author. The shelf keeps the newest
versions of each tool and the newest mail; the limits are in
`runtime/host/src/teamlab.rs`.

## Test

```sh
cargo build --manifest-path runtime/host/Cargo.toml --release
julia --project=. runtime/integrations/teamlab/test_teamlab.jl
```

The test starts two `palette-host broker` processes as two agents over a
temporary shelf; it never touches a real one.
