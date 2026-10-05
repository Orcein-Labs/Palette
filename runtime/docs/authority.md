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

`PALETTE_HOST_COMMANDS` names a JSON file of host commands that becomes the
session's `host_command` ceiling, for work the sandbox cannot do itself (a
GPU, a renderer, a build):

```json
{"render": {"argv": ["/usr/bin/julia", "--project=/abs/proj", "/abs/proj/render.jl"],
            "cwd": "/abs/proj", "timeout_s": 900, "extra_args": true}}
```

Turn code runs one by name with `Palette.host_command("render", args...)`.
The program is an absolute path and never a shell; arguments reach only a
command whose entry sets `extra_args`. Each command runs in its own process
group, killed whole at `timeout_s` (at most 4 hours); `wait=false` returns a
job id for `Palette.host_command_poll`, and jobs still running when the
session ends are killed. Commands run with the host process's privilege and
environment, outside every mount rule above: list only commands you would
let the model run unattended.

The routed MCP adapter can apply exact reviewed patches to trusted configured
source roots. Approval binds a digest and world to one expiring attempt. It
requires client confirmation or separately authorized administrator approval;
model-supplied arguments cannot mint a grant. See [patch contracts](workspaces.md).

Routing context keys prevent accidental conversation collisions. They are not
an authenticated identity system. Project-scoped worlds can share files, and
multiple worlds do not imply filesystem isolation from those shared project files.

The host trusts the operating system's namespace, filesystem, and Landlock
mechanisms. Unmanaged host writers do not participate in Palette's patch lock.
Multi-file patches can complete partially; receipts identify the committed files.
Sandbox-local success alone does not establish that an effect reached the host.
