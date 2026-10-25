---
name: palette
description: Reach for Palette (the `pal` command or the palette MCP tool) for any computation, data work, scripting or multi-step investigation beyond a one-line shell command; it is a persistent Julia kernel that keeps state between calls.
---

Palette is a Julia kernel that stays up between calls. Variables, loaded data,
packages and compiled code survive from one call to the next, so build up work
in steps instead of rerunning a script from the top. Use it the way you would
use a shell, for anything heavier than a one-liner: arithmetic, parsing,
statistics, file analysis, prototyping, and driving long jobs.

From Bash, `pal` sends code to the user's Palette daemon and prints the value:

    pal 'x = 6 * 7'
    pal 'x + 1'                      # 43: x is still there
    pal -f analysis.jl               # run a file
    pal <<'EOF'
    using Statistics
    mean(rand(1000))
    EOF

Flags: `-w WORKSPACE` and `-c CONTEXT` pick a workspace or context (defaults
from PAL_WORKSPACE and PAL_CONTEXT). `-t SECONDS` sets the turn timeout.
`--full` shows output that would otherwise be truncated. `--ephemeral` runs in a
throwaway child with the session's mode and authority. `--json` prints the raw
tool result. `--tool NAME 'JSON'` calls another Palette tool, for example
`pal --tool palette_workspace '{"action":"list"}'`. An error exits 1 with the
message on stderr. A usage or connection problem exits 2.

The palette MCP tool reaches the same daemon and the same state as `pal`.

The kernel runs in `operator` mode by default: Julia has the user's normal
filesystem, process, network, environment and toolchain access. Work directly
in the configured project. For example, after `cd(repo)`,
`Palette.host_command("git", ["status"])` runs Git from that directory with
literal argv and no shell. `Palette.host_commands()` lists configured aliases;
operator mode also accepts ordinary unregistered executable names or paths.

`Palette.host_command(name, args...)` waits for a synchronous command. Use
`wait=false` for long builds or other work that should outlive one turn; it
returns a job id, `Palette.host_command_poll(job)` returns new output and the
final result, and `Palette.host_jobs()` lists retained jobs. Direct operator
commands start in Julia's current working directory, initially the session
workspace. Configured aliases keep their fixed cwd and timeout.

Use `pal --mode sandbox 'expression'` or set `PALETTE_MODE=sandbox` to enter
the opt-in bubblewrap lab. There, `homedir()` is `/run/palette/home`, filesystem
access follows the configured mounts and raw networking is disabled unless
granted. Sandbox host commands must use an alias listed by
`Palette.host_commands()`.
