# Joovy

[Joovy.jl](https://github.com/ilscipio/Joovy.jl) is Palette's tiered-compilation
dependency (Apache-2.0). `using Palette` loads it. Session code can call
`joovy_compile_tiered`, `joovy_use`, `prepare_cold_load!`, `finish_cold_load!`,
and `compile_report` without a separate `using`.

## Why it is a dependency

First-call TTFX is the kernel's cold path. Joovy starts functions at a low
optimization tier and promotes them when they are hot. Lazy `joovy_use` compiles
only the functions that run.

Install is the GitHub URL in `Project.toml` / `Manifest.toml`:

```julia
Pkg.add(url="https://github.com/ilscipio/Joovy.jl.git")
```

## Detached depot warmup

Cyan (and `warm.jl`) still run a detached `using Palette` so the next kernel is
a cache hit. `joovy_warm` precompiles Palette's deps one package at a time.

```sh
julia --startup-file=no --project /path/to/Palette \
  runtime/integrations/joovy/warm.jl --project /path/to/Palette
```

Cyan stamps `~/.cyan/agent/palette-warm.stamp` and skips a re-run for 12h.
Set `CYAN_PALETTE_WARMUP=0` to disable.
