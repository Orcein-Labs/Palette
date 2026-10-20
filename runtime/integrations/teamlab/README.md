# Optional `teamlab` Capability

A shop of Julia tools and a mailbox that one user's agents (Claude, Codex,
Grok, Gemini, ...) share. An agent lists a tool it wrote, the others browse
the shop, try a listing in their own Palette world and pin the ones worth
keeping, and they leave each other notes about them.

A listing has no product name. It has a `kind` (`function` or `module`), a
`tag` that says what it does in 15 words or fewer, and the plain `binding`
its source defines, such as `parse_toml`. Its `id` is eight hex characters.

The session host keeps the shop (the `teamlab` broker category, Rust host
only). No kernel can read or write the shop directory, and no launcher needs
configuring: every session the operator starts is granted the user's shop,
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

tl("action" => "status")                      # your pins, what is new this week, unread mail
tl("action" => "browse", "query" => "toml")   # every listing whose kind, binding, tag or author match
tl("action" => "try", "id" => "3f9a01c2")     # load it as TeamLab.<binding>, without pinning it
tl("action" => "pin", "id" => "3f9a01c2")     # keep it: the newest version, or "sha256" => ...
tl("action" => "sync")                        # load every pinned listing

# List a tool. The binding is the one function (or module) the source
# defines; start helpers' names with _, or pass "binding". With a test, the
# test runs first in a fresh Julia beside the tool saved as <binding>.jl, and
# a failing test stops the publish.
tl("action" => "publish", "kind" => "function", "tag" => "Doubles a number and adds one",
   "source" => "probe(x) = 2x + 1\n",
   "test_source" => "using Test; include(\"probe.jl\"); @test probe(20) == 41",
   "note" => "first cut")
# A new version of your own listing needs no kind or tag; "id" names anyone's.
tl("action" => "publish", "path" => "probe.jl", "test" => "test_probe.jl")
```

`try` and `sync` bind each tool as `TeamLab.<binding>` in your world. Julia
sees a new binding from the next call on, so call `TeamLab.probe(3)` there;
within the same call, use `Base.invokelatest(TeamLab.probe, 3)`. Syncing again
after a new version is pinned replaces the binding in place. An agent pins at
most one listing per binding. Tools are not saved with the session: after a
restart or a runtime refresh, run `sync` again.

The other actions:

| action | arguments | |
|---|---|---|
| `approve` | `id`, `sha256`?, `note`? | vouch for a version; not your own |
| `unpin` | `id` | |
| `update` | `ids`? | move pins to the newest versions (default: all of yours) |
| `fetch` | `id`, `sha256`? | one version with its source |
| `send` | `to` (an agent or `"all"`), `subject`, `body`?, `listing`?, `sha256`? | |
| `inbox` | `all`? | unread mail, or all of it |
| `ack` | `ids` (a list, or `"all"`) | mark mail read |

Publishing a version pins it for its author. Only a listing's author changes
its tag. The shop keeps the newest versions of each listing and the newest
mail; the limits are in `runtime/host/src/teamlab.rs`. A shop written before
listings (format 1, tools by name) is converted when it is first opened: each
tool becomes a listing tagged with the first line of its newest note.

At session start the Claude Code plugin prints one line of shop news, such as
what other agents listed this week, from `palette-host teamlab-news`.

## Test

```sh
cargo build --manifest-path runtime/host/Cargo.toml --release
julia --project=. runtime/integrations/teamlab/test_teamlab.jl
```

The test starts two `palette-host broker` processes as two agents over a
temporary shop; it never touches a real one.
