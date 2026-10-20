# Authority boundaries

Palette workers run ordinary Julia with reflection, subprocesses, and native
calls. The Linux supervisor places them in bubblewrap namespaces with a bounded
filesystem view. Julia source checks are convenience checks, not an isolation
boundary. The plain in-process package API does not establish this OS boundary.

Workspace effects happen within configured mounts. Effects outside that envelope
must be requested from the host broker, whose capability ceiling is configured
before the session starts. A worker cannot grant itself a capability. Child
ceilings must be subsets of the caller's ceiling. Requests outside the ceiling
are denied, and broker receipts record decisions and outcomes.

A network capability grants broker-mediated access, not unrestricted raw worker
networking. Read-only runtime mounts remain read-only even when Julia can run
arbitrary code. Credentials belong to host services rather than worker bindings.

`package_management.allowed_packages` is an explicit operator allowlist. The
broker installs approved packages into the durable host package store, while
workers receive that store read-only and keep compiled caches in a private
session depot. The optional `offline` flag defaults to `false`; when enabled,
the broker resolves only from its prepared registry and source cache. Package
fetches are broker effects, never worker networking. The store survives adapter
restarts.

Package installation itself runs in a separate filesystem namespace because
Julia executes dependency build scripts. The installer cannot write runtime
source, saved worlds, credentials, or broker configuration. It receives private
scratch space and the managed package directories; prepared dependencies remain
read-only. Offline installation also disables network access at the OS boundary.

Writable workspace and saved-state mounts must be disjoint from protected
runtime, project, toolchain, shared depot, OS and broker-socket paths. Canonical
path checks reject parent, child, equal and symlink overlaps before launch.
The routed workspace must also exclude the host registry, grants, ownership
metadata and scratch area. Keep generated client configuration outside it.

`PALETTE_READ_ROOTS` names host directories (`:`-separated absolute paths)
bound read-only into every worker at their own paths, so a path the kernel
reads is the path on the host. A read root may not contain any other mount
(OS roots, toolchain, depots, runtime, project, broker socket, workspace or
saved state) and may not lie inside a writable one, so `/` and a home
directory holding the depot are refused at launch. Name the narrowest
directories the work needs: everything under a root is readable.

`PALETTE_TRUSTED_TOOLS` names the directory of trusted kernel tools. A
helper becomes a project tool in the workspace's `.palette/tools` once it has
succeeded in 3 calls (`Api.promote`); the kernel can write that. It becomes
trusted only through a `palette_patch` request to the reserved
`trusted_tools` target, which no auto-apply class or tested repair can
approve: a human does. The directory is bound read-only into every worker,
may not overlap the workspace, and holds only `<name>.jl` and `<name>.json`.
Each listing re-hashes the source, so a trusted tool whose source no longer
matches its approved hash is not offered as trusted.

`PALETTE_HOST_COMMANDS` names a JSON file of host commands that becomes the
session's `host_command` ceiling, for work the sandbox cannot do itself (a
GPU, a renderer, a build):

```json
{"render": {"argv": ["/usr/bin/julia", "--project=/abs/proj", "/abs/proj/render.jl"],
            "cwd": "/abs/proj", "timeout_s": 900, "extra_args": true,
            "lock": "/tmp/gpu.lock", "description": "render a frame on the GPU"}}
```

Turn code runs one by name with `Palette.host_command("render", args...)`,
and `Palette.host_commands()` lists the commands the session may run, with
their descriptions.
The program is an absolute path and never a shell; arguments reach only a
command whose entry sets `extra_args`. Each command runs in its own process
group, killed whole at `timeout_s` (at most 4 hours; a call's own
`timeout_s` may shorten it, never extend it). `wait=false` returns a job id:
`Palette.host_command_poll(job)` returns the output written since the last
poll, then the result; `Palette.host_command_cancel(job)` kills the group;
`Palette.host_jobs()` lists the jobs held, and the turn's jobs line names the
ones still running. Jobs still running when the session ends are killed.
A command with a `lock` file runs under an exclusive `flock(1)` on it, the
same lock other GPU work on the machine takes, so two renders never share
the GPU; waiting for the lock counts against `timeout_s`. A command's cwd,
lock and the absolute paths in its argv are protected like the mounts above:
no writable root or package store may lie inside, equal or contain one, nor
equal or contain its parent directory (which could rename it). A sibling in
the same parent is fine, so a store under `~/.local` sits beside a command
that runs in `~/project`.
Commands run with the host process's privilege and environment, outside every
mount rule above: list only commands you would let the model run unattended.

The `teamlab` category is the shop of Julia tools and the mailbox that one
user's agents share (`runtime/integrations/teamlab`). The ceiling grants it
as `{"teamlab": {"dir": "/abs/shop"}}`; the operator adds that grant itself
unless the ceiling names one, using `PALETTE_TEAMLAB_DIR`, else
`$XDG_DATA_HOME/palette/teamlab` (`~/.local/share/palette/teamlab`), and
`PALETTE_TEAMLAB_DIR=off` leaves it out. The directory is protected like the
mounts above, and no read root may contain it or lie inside it: no kernel can
read or write it, and the host does every read
and write there, under a lock, with the size limits in
`runtime/host/src/teamlab.rs`. Each request acts as the agent the session
was started for: the MCP client's `clientInfo.name` (`claude-code` is
`claude`), else `PALETTE_AGENT`, else `local`. That name is cooperative. It
records who published, approved or sent something; it does not authenticate
anyone, since every agent of the user runs as the same OS user. A listing's
author keeps merge authority over it: a version another agent publishes to it
is held as a proposal until the author accepts it, and only the author (or
the proposer, withdrawing it) can decline it. Like the name, that rule is
cooperative. Only the Rust host serves `teamlab`; the Python host denies it.

Each call runs under a time limit. The host's `--turn-timeout` is the default
and `--max-turn-timeout` (default: the same, at most 7 days) is the longest a
call may name with its own `timeout_s`; a value outside that range is refused
before the call runs. Past its limit a call is interrupted and the kernel keeps every
binding; if it does not stop within a further 12 s, the kernel is killed and
revived. The MCP adapters pass `PALETTE_TURN_TIMEOUT` (default 60) and
`PALETTE_MAX_TURN_TIMEOUT` (default 1800) to the host, and a server profile
sets them with `turn_timeout` and `max_turn_timeout`. The limit covers one
call; work longer than any call runs as a host command job. A scratch
(`ephemeral`) call runs in a broker child bounded by the session's default
limit less 10 s, so it takes no `timeout_s`: the host refuses one. A spawned
child has the same bound, whatever limit its parent call runs under.

The routed MCP adapter can apply exact reviewed patches to trusted configured
source roots. Approval binds a digest and world to one expiring attempt. It
requires client confirmation or separately authorized administrator approval;
model-supplied arguments cannot mint a grant. See [patch contracts](workspaces.md).

Routing context keys prevent accidental conversation collisions. They are not
an authenticated identity system. Project-scoped worlds can share files, and
multiple worlds do not imply filesystem isolation from those shared project files.

The broker's socket accepts connections from any local user, because the
worker reaches it through a user namespace, and the broker does not check its
peer. What keeps other users out is the directory around it: the session root,
`~/.palette-sessions` and the socket's `broker` directory are created or
tightened to mode 0700, and a session refuses to start if `broker` is a symlink
or belongs to another user. The sandbox's passwd and group files are kept in
`palette-etc-<uid>` under `$XDG_RUNTIME_DIR` when that directory is private to
the user, and under `$TMPDIR` or `/tmp` otherwise, where another user who
creates that name first can stop sessions from starting but cannot change the
files.

Production entry points start sessions and brokers through the Rust host,
`palette-host`. The Python session, broker and `session_cli.py` are a reference
implementation for the conformance suites (`cargo xtask verify --implementation
python`); no production entry point imports them.

## Python host: scheduled for removal

> **The Python host is kept for one more release, only as the conformance
> oracle. It is deleted once the Rust host has its own conformance tests for
> every category the oracle covers.** Do not add features to it.

The Python host is `session.py`, `broker.py`, `launch_worker.py`,
`session_cli.py` and the Python branch of `host_adapter.py`. Today it earns its
place by running the same assertions as the Rust host, so a divergence between
the two shows up as a failure on one side. That is how A3 and the D11 control
were caught.

The oracle covers these categories, one suite each, as run by
`cargo xtask verify --implementation python`:

| Category | Suite |
|---|---|
| Authority: bind layout, read and write roots, network, credentials, broker ceilings | `test_authority.py` |
| Session: persistent and ephemeral turns, kill detection, session roots, session depots, receipts, broker socket privacy, approved and denied writes | `test_session.py` |
| Session bridge: the stdio protocol of `session_cli` / `palette-host session` | `test_session_cli.py` |
| Revival: what a replacement kernel restores, rebuilds or reports lost | `test_revival.py` |
| Host bridge: mid-call host requests from the kernel, limited to the types its ceiling names | `test_host_bridge.py` |

Delete the Python host when each row has Rust-side coverage that does not need
the Python run to tell right from wrong. Then drop the `python` leg of the
nightly CI matrix and the `--implementation` option.

Pull-request and push CI run the Rust host only. The `[rust, python]` matrix
runs nightly and on manual dispatch.

The host trusts the operating system's namespace, filesystem, and Landlock
mechanisms. Unmanaged host writers do not participate in Palette's patch lock.
Multi-file patches can complete partially; receipts identify the committed files.
Sandbox-local success alone does not establish that an effect reached the host.
