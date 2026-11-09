# Joovy

[Joovy.jl](https://github.com/ilscipio/Joovy.jl) is Palette's tiered-compilation
dependency (Apache-2.0). `using Palette` loads it. Session code can call
`joovy_compile_tiered`, `joovy_use`, `prepare_cold_load!`, `finish_cold_load!`,
and `compile_report` without a separate `using`.

## Why it is a dependency

First-call TTFX is the kernel's cold path. Joovy starts functions at a low
optimization tier and promotes them when they are hot. Lazy `joovy_use` compiles
only the functions that run.

## Install

Joovy is not in the General registry. `Project.toml` pins it under `[sources]`
to a Git URL and commit, so `Pkg.instantiate()` fetches it with the rest of
Palette's dependencies. `[sources]` needs Julia 1.11 or newer.

## Detached depot warmup

`warm.jl` runs `using Palette` ahead of time, so the next kernel start is a
cache hit. `joovy_warm` precompiles Palette's dependencies one package at a time.

```sh
julia --startup-file=no --project /path/to/Palette \
  runtime/integrations/joovy/warm.jl --project /path/to/Palette
```
