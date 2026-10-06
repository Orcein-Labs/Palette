# Server-owned Palette

Palette's normal runtime can live on a Linux server. The server owns its Julia
runtime, Rust host, package depot, workspaces, snapshots and capability registry.
A client PC is optional: an operator can separately authorize an edge GPU,
local model, file mount or toolchain. Neither CyanTunnel nor a developer checkout
is needed to start the production adapter. CyanTunnel remains FreeBuff
compatibility/testing machinery.

Install Julia 1.12, Python 3.11 or newer, bubblewrap and Rust on the server. Deploy a Palette
checkout or release SDK at a stable server path. Configure a service account's
directories so the source, capability configuration and registry are outside
worker-writable workspace and state mounts. The server account must be able to
write the data and depot directories and read its deployed source/configuration.
The operating system must support Bubblewrap namespaces and Landlock for patches.

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
Worker networking stays disabled; the broker performs an approved installation.

Generate a profile on the server, then prepare it as an administrator action:

```sh
python3 /opt/palette/runtime/security/server_runtime.py init \
  --profile /etc/palette/runtime.json
python3 /opt/palette/runtime/security/server_runtime.py prepare \
  --profile /etc/palette/runtime.json
```

Defaults are `/opt/palette` for source, `/var/lib/palette` for data,
`/var/cache/palette/depot` for prepared dependencies, and
`/opt/palette-capabilities` for installed capability manifests. `init` accepts
explicit path overrides and refuses to overwrite a profile. `inspect` reports
the selected ownership/layout without starting a worker.

Launch the production MCP stdio adapter with:

```sh
python3 /opt/palette/runtime/security/server_runtime.py launch \
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
`read_roots` is an optional list of narrowly scoped server directories. Ambient
client environment variables do not become configuration or additional grants.

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
