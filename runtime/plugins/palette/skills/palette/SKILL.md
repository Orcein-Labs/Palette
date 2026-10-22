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
throwaway kernel. `--json` prints the raw tool result. `--tool NAME 'JSON'`
calls another Palette tool, for example `pal --tool palette_workspace
'{"action":"list"}'`. An error exits 1 with the message on stderr. A usage or
connection problem exits 2.

The palette MCP tool reaches the same daemon and the same state as `pal`.

The kernel runs in a sandbox. Inside it, `homedir()` is `/run/palette/home`,
so do not write `~`. Use absolute paths under the read roots the user
configured. Host programs run through the commands the operator allows:
`Palette.host_commands()` lists them. `Palette.host_command(name, args...)`
runs one and waits. With `wait=false` it returns a job id at once, for work
longer than a turn. Then `Palette.host_command_poll(job)` returns the output
since the previous poll and finally the result, and `Palette.host_jobs()`
lists the jobs. The daemon has a private `/tmp`, so host commands do not see
yours: have long jobs write their logs and outputs under a project directory.
