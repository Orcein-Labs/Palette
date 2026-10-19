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

A session started without a receipts directory (`palette-host session` or
`run-worker` without `--receipts-dir`, or the Python `PaletteSession` without
`receipts_dir`) keeps its receipts in `~/.palette/receipts/<session id>/`,
which is private to the user and survives the session. A closed session's
receipts that have not been written for `PALETTE_RECEIPT_RETENTION_DAYS` days
(default 30) are archived, not deleted. The next session to start appends them to
`~/.palette/docs/receipts/<YYYY-MM>.zip`, where the month is the UTC month of
the directory's last write. Each entry is named `<session id>/<file>`. Then
the session removes the directory. The archive directory is `0700` and each
archive is `0600`.

The archive is a plain ZIP with stored entries, readable by `unzip` or Python
`zipfile`. Each append writes a new archive beside the old one, syncs it and
renames it over the old one, under a lock on `.lock` in the archive directory.
Both hosts write the same format under the same lock. If a directory cannot be
archived, it is kept, and a warning goes to the host's stderr. A later session
tries again. This happens when the receipts hold a symlink or a special file,
when the archive is damaged, or when it would pass 4 GiB, since there is no
ZIP64. The Python host stops at the 2 GiB that `zipfile` allows without ZIP64.
Retrying after a crash is safe: files already in the archive with the same
contents are skipped.

Setting the retention to `0` restores the earlier behavior: receipts live in
the session's scratch root and are deleted when the session closes.

A session's default scratch root, `~/.palette-sessions/<session id>`, and its
session depot in `.palette-depot-clones` each carry a `.owner` record naming the
host process. If that process is killed before its teardown runs, the next
session started in the same pid namespace removes them. A caller-supplied
scratch root is never swept.

A session's depot is built from the newest snapshot of the prepared depot, in
`.palette-depot-snapshots/<depot name>/snap-<sequence>` beside the depot. A
successful preparation publishes the snapshot. The session's depot is an
overlay: the snapshot is mounted read-only as the lower layer and the worker
writes to a private upper layer, so no Julia process writes a snapshot. The
session holds a shared lock on the snapshot for as long as the overlay lives.
When bwrap is older than 0.8 or the kernel refuses unprivileged overlay mounts,
the session copies the snapshot instead and holds the lock only while it copies.
Publishing removes the older snapshots that no session holds, and so does a
later session. When no snapshot exists, the session clones the live depot and
retries up to three times if a precompile changes the tree during the copy.

`external_fs_write.allowed_dirs` are resolved once, when the broker starts.
Each write walks from `/` to its target without following a symlink, so a
directory that is replaced by a symlink later is refused, not followed. This
also covers a directory the worker can write. After such a swap, restart the
session to use the new target. A broker refuses to start when an allowed
directory contains or lies inside a path the host trusts. Those paths are the
receipt log, the Palette checkout, the package store, the operator's
configuration files and the host command paths.

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
