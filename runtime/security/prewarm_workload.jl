using Palette
failed = String[]
stdlibs = filter(n -> isfile(joinpath(Sys.STDLIB, n, "Project.toml")), readdir(Sys.STDLIB))
project_deps = collect(keys(get(Base.parsed_toml(Base.active_project()), "deps", Dict())))
managed_environment = get(ENV, "PALETTE_PACKAGE_ENVIRONMENT", "")
managed_project = joinpath(managed_environment, "Project.toml")
optional_deps = !isempty(managed_environment) && isfile(managed_project) ?
    collect(keys(get(Base.parsed_toml(managed_project), "deps", Dict()))) : String[]
for name in sort(unique([stdlibs; project_deps; optional_deps]))
    try
        Core.eval(Main, :(import $(Symbol(name))))
    catch e
        push!(failed, name * ": " * first(sprint(showerror, e), 200))
    end
end
println("prewarmed ", length(Base.loaded_modules), " modules")
foreach(f -> println("could not load ", f), failed)
cache_files = String[]
for origin in values(Base.pkgorigins)
    path = origin.cachepath
    path === nothing && continue
    push!(cache_files, path)
    image = Base.ocachefile_from_cachefile(path)
    isfile(image) && push!(cache_files, image)
end
println("palette_preparation_caches=", Palette.JSON.json(sort!(unique(cache_files))))
exit(isempty(failed) ? 0 : 1)
