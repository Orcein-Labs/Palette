# Installation and MCP

The Linux supervised runtime requires Julia, Python 3, bubblewrap, and a built
Rust host. Julia dependencies are defined by `Project.toml`; Rust dependencies
are locked in `runtime/host/Cargo.lock`. Instantiate the Julia project and build the host
before installing the adapter. Additional packages must be provisioned by the
host, not silently downloaded inside an offline worker.

See the [portable plugin setup](../plugins/palette/README.md) for Agent Plugins manifests, a repository marketplace, and a generic MCP launcher.

## Prepare before interactive use

Cold preparation compiles standard libraries and project dependencies in the
actual sandbox, with the same paths later sessions use. On a fresh depot this
can take several minutes and exceed a client's interactive request deadline.
Run it before connecting an interactive client, and after runtime or source
updates:

```sh
python3 runtime/security/prewarm_depot.py --project-dir "$PWD" \
  --host-bin "$PWD/runtime/host/target/release/palette-host"
```

A successful command records `state: prepared`; sessions validate its source,
runtime, and loaded-cache identity before reusing it. `--status` inspects the
record without starting preparation. Preparation failures remain failures;
raising a client's call timeout is not a substitute for a prepared installation.

## Local stdio plugin

```sh
python3 runtime/security/install_operator_plugin.py \
  --plugin-dir /absolute/path/to/plugin \
  --workspace-dir /absolute/path/to/workspace \
  --repo-dir /absolute/path/to/Palette
```

The installer creates a Codex-compatible manifest and MCP configuration. It
checks the local build and preserves configuration when updating an existing
Palette entry. The client must separately support and enable that plugin format.
Installing these files does not register a remote ChatGPT connection.

The adapter's entrypoint is `runtime/security/operator_workspace_router.py`. It exposes
`palette`, `palette_control`, `palette_workspace`, and `palette_patch`. See
[workspace routing](workspaces.md) before sharing it between conversations.

| Environment variable | Purpose |
| --- | --- |
| `PALETTE_REPO` | Runtime source checkout |
| `PALETTE_HOST` | Built host executable |
| `OPERATOR_WORKSPACE` | Default legacy workspace |
| `PALETTE_STATE_DIR` | Legacy snapshots, outside the worker workspace |
| `PALETTE_WORKSPACE_STATE_ROOT` | Routed world registry and snapshots |
| `PALETTE_PATCH_ROOTS` | Trusted administrator's JSON mapping of writable targets |
| `PALETTE_READ_ROOTS` | Host directories the kernel may read, `:`-separated; see [authority](authority.md) |
| `PALETTE_HOST_COMMANDS` | JSON file of host commands the kernel may run; see [authority](authority.md) |
| `PALETTE_CAPABILITY_CEILING` | Host-owned JSON capability ceiling loaded once at adapter startup |
| `PALETTE_PACKAGE_DEPOT` | Durable broker-managed package store; defaults to `~/.palette/packages` |
| `JULIA_DEPOT_PATH` | Provisioned Julia depot |
| `JULIA_PKG_OFFLINE` | Julia's offline package policy |

Restart the adapter to load updated runtime code. Refresh client tool discovery
when the tool schema changes. Reusing a connection does not require creating a
second plugin registration.

See [generic toolchain provisioning](toolchain-provisioning.md) for fixed host
commands, explicit package allowlists, and durable package behavior.

## Remote clients

A remote MCP client needs a supported transport or tunnel to this local service.
Palette does not provision a cloud service or bundle client credentials. Keep
transport credentials outside workers. Runtime source updates and client
connection registration are separate operations.

## Prelaunch runtime identity change

The current package is loaded with `using Palette`. Source, package identity,
preparation records, and serialized runtime types belong to a particular
installation. Rebuild and prewarm after updating. Keep an older runtime and its
external saved worlds available until you have exported portable experiment
data and verified the new world; do not assume old binary snapshots migrate
across a package-name change. Active deployment forks can retain their original
runtime until that migration is deliberately tested.
