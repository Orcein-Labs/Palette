# Runtime capabilities

Chat keeps the existing `palette`, `palette_control`, `palette_workspace`, and
`palette_patch` schemas. Julia discovers additional capabilities through
`Palette.runtime` (also available as `Api.runtime` in a turn).

The administrator selects a host-owned `PALETTE_CAPABILITY_CEILING` file:

```json
{"host_request":{"allowed_types":["palette.runtime"]},
 "package_management":{"allowed_packages":["Parsers"]}}
```

The adapter freezes this ceiling at startup. Discovery and refresh cannot add a
grant. Worker networking remains disabled; allowed packages are installed by the
broker into its persistent package environment.
The package grant can set `"offline": true` for cached packages only. Otherwise
the broker can fetch allowlisted packages through Julia's package machinery;
this does not grant raw network access to the worker. A child cannot change an
offline parent's package grant to online.
Dependency build scripts run in an isolated installer with managed package
storage and private scratch space writable. Host configuration and credentials
are outside that namespace.

Broker decision and effect receipts are kept in the host-private sibling of the
world's saved-state directory, named `<state directory>.receipts`. They survive
worker refresh and restart and are outside the worker's writable mounts.

```julia
Api.runtime("discover")
receipt = Api.request_capability("package_management", Dict("name" => "Parsers"))
receipt["approved"] === true || error(receipt["reason"])
using Parsers
Parsers.parse(Int, "42")
```

Discovery reports the runtime version and content identity, worker epoch, active
and available capability generations, toolchain readiness, authorized and
unauthorized capabilities, and dependencies missing from the worker's Julia load
path. An available capability is executable only after its generation is active
and its required grants and dependencies are present.

The administrator deploys a directory under `PALETTE_RUNTIME_ROOT`, separate from
the workspace and saved state. Each capability has a `capability.json` and a
Julia source file:

```json
{"id":"example","version":"1.0","source":"main.jl",
 "entrypoint":"example_capability","dependencies":[],
 "requires":[],"description":"Adds two to a supplied integer"}
```

```julia
function example_capability(arguments)
    get(arguments, "value", 0) + 2
end
```

The manifest ID must match its directory. Source stays within that directory.
Entrypoints must use a distinct name. Manifests cannot contain credentials,
ceilings, process arguments, or privileged host callbacks. The host validates
and supplies source; execution occurs inside the Julia sandbox with the existing
ceiling.

```julia
Api.runtime("invoke", Dict("id" => "example",
    "arguments" => Dict("value" => 40)))
Api.runtime("refresh")
```

Refresh requested during a turn is deferred until that turn finishes. The adapter
stops the worker, checks the committed snapshot covers the completed turn, swaps
the capability generation, and starts a new worker epoch with revival enabled.
The full response includes `runtime_refresh` with the observed epoch, snapshot,
and revival evidence. Subsequent calls use the same world and new generation.
Check `success` and the revival report; a restart command alone proves no state
survival. Runtime objects that cannot be revived are still reported explicitly.

Registry source and Julia worker updates use this epoch refresh. Updating Python
adapter code requires a supervised adapter restart; it is a separate deployment
operation. The fixed tool schemas allow a client to reconnect within the same
conversation. A package identity change, such as an older Neura installation,
requires a separately verified state migration before switching its saved worlds.

Tested repair is available through the same runtime interface. See
[the repair workflow](workspaces.md#tested-repair-workflow) for fixed test recipes,
ordinary source classes, and explicit approval of authority changes. See
[server deployment](server.md) for server-owned compute and storage.

Snapshots created before self-provisioning retain their saved project and user
load paths. Revival also keeps the current launcher-selected managed package
environment available; a snapshot cannot replace that host-selected path.
Preparation tracks the managed project and manifest and precompiles its installed
optional packages inside the same sandbox mount layout used by workers.
