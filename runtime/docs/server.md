# Server-owned Palette

Palette's normal runtime can live on a Linux server. The server owns its Julia
runtime, Rust host, package depot, workspaces, snapshots and capability registry.
A client PC is optional: an operator can separately authorize an edge GPU,
local model, file mount or toolchain. Neither CyanTunnel nor a developer checkout
is needed to start the production adapter. CyanTunnel remains FreeBuff
compatibility/testing machinery.

Both local and server profiles default to operator mode, which runs Julia
directly and does not require bubblewrap. Set `mode` to `sandbox` in a server
profile (or pass `--mode sandbox` to `serve init`) to make sandbox the default;
that mode requires bubblewrap. For local calls, `pal --mode sandbox 'code'`
selects the separate sandbox world for one call. From `pal`, `--status` reports
effective mode, the running session's config revision, and the pending daemon
config. `--reload` validates and publishes configuration for new sessions. A
config reload leaves an already-running session on its captured mode and config
revision.

Install Julia 1.12, Python 3.11 or newer, and Rust on the server. Install
bubblewrap only if the profile selects sandbox mode. Deploy a Palette
checkout or release SDK at a stable server path. Configure a service account's
directories so the source, capability configuration and registry are outside
worker-writable workspace and state mounts. The server account must be able to
write the data and depot directories and read its deployed source/configuration.
Sandbox mode needs Bubblewrap namespaces; repair patches also need Landlock.

Create a host-owned ceiling file, for example `/etc/palette/ceiling.json`:

```json
{
  "host_request": {"allowed_types": ["palette.runtime"]},
  "package_management": {"allowed_packages": ["Crayons"]}
}
```

The runtime request exposes discovery, sandboxed capabilities, refresh and the
configured repair workflow. It does not authorize network access, arbitrary
host commands or file writes. Package requests remain subject to the named
allowlist. Leave `package_management` out when installations are not authorized.
Sandbox worker networking stays disabled; the broker performs an approved installation.

Build the host once, generate a profile on the server, then prepare it as an
administrator action:

```sh
cargo build --release --locked --manifest-path /opt/palette/runtime/host/Cargo.toml
/opt/palette/runtime/host/target/release/palette-host serve init \
  --profile /etc/palette/runtime.json
/opt/palette/runtime/host/target/release/palette-host serve prepare \
  --profile /etc/palette/runtime.json
```

`prepare` needs `julia` and `cargo` on `PATH`; it also needs `bwrap` when the
profile's default mode is `sandbox`. Older profiles without a `mode` field
default to `operator`.

Defaults are `/opt/palette` for source, `/var/lib/palette` for data,
`/var/cache/palette/depot` for prepared dependencies, and
`/opt/palette-capabilities` for installed capability manifests. `init` accepts
explicit path overrides and refuses to overwrite a profile. `inspect` reports
the selected ownership/layout without starting a worker.

Launch the production MCP stdio adapter with:

```sh
/opt/palette/runtime/host/target/release/palette-host serve launch \
  --profile /etc/palette/runtime.json
```

An SSH-capable MCP client can obtain a client entry with the `client` action and
`--ssh-target palette@your-server`. Compute and state remain on the server.
ChatGPT Chat requires a supported authenticated remote MCP transport or tunnel;
run that transport on the server and use the command above as its stdio backend.
The profile does not manufacture an HTTPS endpoint, register a Chat connection,
or supply credentials. Transport credentials remain outside the worker runtime.

The launcher uses a restricted environment. To authorize additional named host
commands, put an absolute `host_commands` configuration path in the profile.
`read_roots` is an optional list of narrowly scoped server directories.
`turn_timeout` (default 60) and `max_turn_timeout` (default 1800) set a call's
default time limit and the longest one it may ask for in seconds. Ambient
client environment variables do not become configuration or additional grants.

For the local daemon operator desk, `~/.config/palette/read-roots` is the live
default roots file; `PALETTE_READ_ROOTS` explicitly overrides it.
`PALETTE_HOST_COMMANDS` likewise overrides the canonical host-commands file.
Older installs may have `PALETTE_READ_ROOTS` in `config/daemon.env` because
setup copied the roots file into that variable. It now acts as an explicit
override. Review and remove the line only when returning to the canonical live
file; setup preserves it because its original intent cannot be recovered.

For staged repair, add `patch_roots` (target names mapped to source directories)
and a `repair_config` file path to the profile. The host configuration contains
`test_recipes` with a fixed `id`/`argv` for each target, and
`auto_apply_prefixes` listing ordinary path classes and relative prefixes.
Only sandbox-executed ordinary source qualifies for unattended repair. Broker,
sandbox, credential, verification and host-executed command paths remain
protected even if a prefix would otherwise allow them. Keep the repair
configuration outside every patch target. See [repair contracts](workspaces.md).

An installed, sandbox-only Julia capability may be maintained in place only
when its target is an immediate operator-owned directory under
`PALETTE_RUNTIME_ROOT`, its immutable `capability.json` names the source, and
`sandbox_source_paths` explicitly lists that exact `.jl` filename for the
target. The same target must have an `adapters` or `tools` prefix and a fixed
passing test recipe. The source and manifest must be regular, single-link,
operator-owned files; registry directories cannot be group/world writable.
The manifest, configuration, credentials, host-command roots, and all runtime
authority modules remain approval-protected. Direct patch calls cannot use this
exception or bypass the tested-repair workflow.

Keep the server data and package directories across runtime upgrades. Refresh
within the current MCP connection and inspect actual revival evidence. A runtime
upgrade that keeps the external tool schemas fixed does not require a new Chat
conversation. An incompatible external schema change still requires the client
to rediscover tools. Package-name or serialized-type migrations require their
own compatibility proof; retain the old runtime until that proof is complete.

Local launcher/process tests establish portable execution, path protection and
the unchanged tool surface. Independence from a particular PC requires a live
server deployment test with that PC disconnected. That evidence is distinct from
the local test and must be recorded by the deployment operator.
