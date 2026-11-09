#!/usr/bin/env julia
# Detached depot warmup. Joovy is a Palette Project.toml dependency.

const PROJECT = let
    i = findfirst(==("--project"), ARGS)
    i === nothing ? get(ENV, "PALETTE_PROJECT_DIR", "") : (i < length(ARGS) ? ARGS[i + 1] : "")
end
isempty(PROJECT) && error("joovy-warm: pass --project /path/to/Palette")

using Pkg
Pkg.activate(PROJECT; io=devnull)

using Joovy
packages = String["JSON", "SHA", "Serialization", "PrecompileTools", "Dates", "UUIDs", "Sockets", "Statistics", "Joovy"]
println("joovy-warm: warming $(join(packages, ", ")) in $PROJECT")
ok = joovy_warm(packages; project=PROJECT)
ok || @warn "joovy-warm: some packages failed"

using Palette
println("joovy-warm: Palette loaded")
