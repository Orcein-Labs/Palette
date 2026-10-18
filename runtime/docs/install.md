# Installation and MCP

## One-command install

On Linux, or inside WSL2:

```sh
curl -fsSL https://github.com/latentcollapse/Palette/releases/latest/download/install.sh | sh
```

On Windows (PowerShell or a command prompt):

```powershell
powershell -c "iex ((New-Object Net.WebClient).DownloadString('https://github.com/latentcollapse/Palette/releases/latest/download/install.ps1'))"
```

`install.sh` does the following, and running it again updates the install:

1. Downloads the release archive, which holds the source and a prebuilt
   x86_64 host (glibc 2.35 or newer), and checks it against `SHA256SUMS.txt`.
   Where the prebuilt host does not run, setup builds it with Rust.
2. Installs Python 3 and bubblewrap if they are missing and, where AppArmor
   restricts user namespaces (Ubuntu 23.10 and newer), Ubuntu's recommended
   upstream bubblewrap profile, pinned by revision and checksum. This is the
   only step that needs root (`runtime/scripts/setup-sandbox-host.sh`); global
   AppArmor and user-namespace policy is left as it is.
3. Installs Julia with [juliaup](https://github.com/JuliaLang/juliaup) unless
   Julia 1.10 or newer is on `PATH`.
4. Runs `runtime/plugins/setup.py`, which instantiates the Julia project,
   prepares the sandbox depot, writes the client configuration and the
   `palette-mcp` stdio launcher, registers the server with Claude Code and Codex
   when they are installed and have no server named `palette`, and makes one
   test call, `1 + 1`, through the launcher.

`install.ps1` installs WSL2 with Ubuntu when there is no WSL distribution
(restart Windows if asked, create the Linux user, then run it again), installs
step 2 as root inside WSL without a password, runs `install.sh` inside the
distribution, and registers Claude Desktop, Claude Code and Codex on Windows.
Each runs `wsl.exe --distribution <name> --exec <launcher>`. A WSL1
distribution cannot run the sandbox; convert it with `wsl --set-version`.

Settings, all optional environment variables:

| Variable | Effect |
|---|---|
| `PALETTE_HOME` | Install location (default `~/.local/share/palette`): `sdk/`, `client/`, `data/` and `depot/` |
| `PALETTE_VERSION` | Release tag to install. `releases/latest` skips prereleases, so a tag such as `v1.0.0-rc.1` needs this |
| `PALETTE_RELEASE_URL` | Download from this base URL instead: a mirror, or `file:///path` for a local copy of the release assets |
| `PALETTE_WSL_DISTRIBUTION` | On Windows, the WSL distribution to use instead of the default one |

From a source checkout, `sh runtime/scripts/install.sh` uses the checkout as the SDK and
builds the host with `cargo`. `python3 runtime/plugins/setup.py --help` lists
the setup options, such as `--no-register` and `--no-check`, for running that
step alone.

To remove Palette: `claude mcp remove palette`, `codex mcp remove palette`,
delete the `palette` entry from Claude Desktop's configuration on Windows, and
delete `PALETTE_HOME`. Julia stays installed (`juliaup self uninstall` removes it).

## Manual installation

The Linux supervised runtime requires Julia, Python 3, bubblewrap, and a built
Rust host. Julia dependencies are defined by `Project.toml`; Rust dependencies
are locked in `runtime/host/Cargo.lock`. Instantiate the Julia project and build the host
before installing the adapter. Additional packages must be provisioned by the
host, not silently downloaded inside an offline worker.

See the [portable plugin setup](../plugins/palette/README.md) for Agent Plugins manifests, a repository marketplace, and a generic MCP launcher.

To run a host session directly after preparing it (below):

```sh
runtime/host/target/release/palette-host session --repo-dir "$PWD" --project-dir "$PWD" --workspace-dir /absolute/path/to/workspace --state-dir /absolute/path/to/state --ceiling '{}'
```

The session speaks newline-delimited JSON on stdin/stdout. After its startup
message, send `{"request_id":"1","code":"x = 41"}` and then
`{"request_id":"2","code":"x + 1"}`. Close stdin to stop it. Keep state outside
the workspace.

### Prepare before interactive use

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

A successful preparation also publishes a snapshot of the depot in
`<depot parent>/.palette-depot-snapshots/<depot name>/`. Each session mounts an
overlay of it: the snapshot is the read-only lower layer and the session writes
to a private upper layer, so starting a session copies nothing and a Julia
process that shares the live depot cannot break a session's depot. A reused
preparation publishes a snapshot when none exists. Sessions copy the snapshot
instead when bwrap is older than 0.8 or the kernel refuses unprivileged overlay
mounts, and copy the live depot until a snapshot exists. On a filesystem
without reflinks the snapshot is a full copy of the depot.

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

### User service lifecycle

The daemon is owned by the user's systemd manager, so closing a shell or Codex
client only disconnects that client. Install the unit with the same runtime
checkout and workspace used by the plugin:

```sh
python3 runtime/security/palette_service.py install \
  --repo-dir "$PWD" --workspace-dir /absolute/path/to/workspace
python3 runtime/security/palette_service.py start
python3 runtime/security/palette_service.py status
```

`palette.service` owns the Julia daemon and its private socket at
`$XDG_RUNTIME_DIR/palette/daemon.sock` (override with `PALETTE_SOCKET`). Its
identity output includes the systemd main PID and process start ticks. The
stdio MCP client connects to this socket and exits independently. Restarting
the tunnel alone leaves the daemon and its workspace state in place; use
`systemctl --user restart palette-tunnel.service` for that operation. The
installer writes units but does not enable them for login or boot. Start the
daemon with `palette_service.py start`; manage the optional tunnel with
`palette_service.py restart --unit palette-tunnel.service` (the same `--unit`
selector works with `start`, `stop`, and `status`) and enable
it with `systemctl --user enable palette-tunnel.service` only if the operator
wants it started with the user manager. The daemon can be enabled separately
with `systemctl --user enable palette.service`. Restart
`palette.service` intentionally to exercise daemon revival and workspace
recovery. State remains in the configured durable state root.

Pass `--env-file /absolute/path/to/operator.env` to `install` when the daemon
needs additional operator policy such as `PALETTE_CAPABILITY_CEILING` or host
command/read-root configuration. The file must already be operator-owned with
mode `0600`; Palette never creates or copies it. The service's checkout,
workspace, and socket values remain authoritative.

The Chat tunnel is a separately owned optional service. Palette does not
install the tunnel client, choose a profile, or create credentials. When the
operator already has a tunnel client and profile, generate its unit with:

```sh
python3 runtime/security/palette_service.py install \
  --repo-dir "$PWD" --workspace-dir /absolute/path/to/workspace \
  --tunnel-client /absolute/path/to/tunnel-client --tunnel-profile existing-profile \
  --tunnel-profile-dir /absolute/path/to/profile-directory
```

Add `--tunnel-env-file /absolute/path/to/operator.env` only when the existing
client requires credentials; the file must already belong to the operator and
have mode `0600`. The generated `palette-tunnel.service` reconnects to the
daemon through its configured profile and can be restarted without restarting
Palette. The tunnel restarts after both successful and failed process exits,
because a closed daemon connection can make the tunnel exit successfully. Use
one supervisor for a profile: stop a managed `tunnel-client runtimes connect`
process before starting its systemd unit. An explicit service stop stays stopped.
Both services log to the user journal (`journalctl --user -u
palette.service` or `palette-tunnel.service`).

By default, user services run while the user manager exists and stop after the
last login session. To have them start after machine boot before login, an
operator may choose `loginctl enable-linger`; this is an explicit machine
policy choice. Palette setup does not enable lingering or change the user
manager.

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
| `PALETTE_PATCH_APPROVAL` | Who may approve a patch: `client` (default, MCP form confirmation) or `administrator` (local administrator command only); see [workspace routing](workspaces.md) |
| `PALETTE_READ_ROOTS` | Host directories the kernel may read, `:`-separated; see [authority](authority.md) |
| `PALETTE_HOST_COMMANDS` | JSON file of host commands the kernel may run; see [authority](authority.md) |
| `PALETTE_CAPABILITY_CEILING` | Host-owned JSON capability ceiling loaded once at adapter startup |
| `PALETTE_PACKAGE_DEPOT` | Durable broker-managed package store. The installers and `serve_palette.py` set it to `packages` under the instance's data directory (`PALETTE_DATA_HOME`), so a private instance never shares another's store; a host started without it uses `~/.palette/packages` |
| `PALETTE_RECEIPT_RETENTION_DAYS` | Days a session's default receipts directory is kept after its last write, before a later session archives it into `~/.palette/docs/receipts/<YYYY-MM>.zip`. Defaults to 30. `0` deletes it when the session closes. See [capabilities](capabilities.md) |
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
