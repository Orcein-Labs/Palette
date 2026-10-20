# Trusted, offline installer for Palette's optional Gesso capability.
#
# This copies an already-present Gesso source tree and SmolLM2 checkpoint into
# server-owned roots. It never downloads model files or edits Gesso itself.
module GessoInstall

using JSON
using SHA
using TOML

const MODEL_FILES = ("config.json", "model.safetensors", "merges.txt", "vocab.json",
                     "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json")
const GESSO_PATHS = ("Project.toml", "src", "ext")
const GESSO_UUID = "05f31162-62af-44f0-af37-0e48a49f1899"
const ARCHITECTURE = ("model_type" => "llama", "hidden_size" => 576, "num_hidden_layers" => 30,
                      "num_attention_heads" => 9, "num_key_value_heads" => 3,
                      "vocab_size" => 49152, "tie_word_embeddings" => true, "eos_token_id" => 0)
const RECORDED_ARCHITECTURE = ("model_type", "hidden_size", "num_hidden_layers",
                               "num_attention_heads", "num_key_value_heads", "vocab_size", "eos_token_id")
const INTEGRATION_DIR = realpath(@__DIR__)
const PALETTE_ROOT = dirname(dirname(dirname(INTEGRATION_DIR)))

sha256_file(path) = bytes2hex(open(sha256, path))

inventory_digest(files) = bytes2hex(sha256(join((name * "\0" * files[name] for name in sort!(collect(keys(files)))), "\n")))

"`path` made absolute with every symbolic link that exists resolved, like Python's `Path.resolve(strict=False)`."
function resolved(path)
    path = abspath(path)
    islink(path) && !ispath(path) && error("installer path is a dangling symbolic link: $path")
    ispath(path) && return realpath(path)
    parent = dirname(path)
    parent == path ? path : joinpath(resolved(parent), basename(path))
end

inside(child, parent) = startswith(child, rstrip(parent, '/') * "/")

function path_disjoint(paths)
    paths = resolved.(paths)
    for (i, left) in enumerate(paths), right in paths[i+1:end]
        (left == right || inside(left, right) || inside(right, left)) &&
            error("installer paths must be disjoint: $left and $right")
    end
end

function regular_file(path)
    metadata = lstat(path)
    isfile(metadata) && !islink(metadata) && metadata.nlink == 1 || error("expected an unlinked regular file: $path")
    return metadata
end

"Gives the sandbox UID traversal/read access without write bits."
function make_tree_read_only_accessible(root)
    for (directory, names, files) in walkdir(root; follow_symlinks=false)
        chmod(directory, 0o750)
        for name in names
            islink(joinpath(directory, name)) && error("installed artifact tree contains a symbolic link: $(joinpath(directory, name))")
        end
        for name in files
            child = joinpath(directory, name)
            islink(child) && error("installed artifact tree contains a symbolic link: $child")
            regular_file(child)
            chmod(child, 0o440)
        end
    end
end

function validate_model_source(source)
    source = realpath(source)
    isdir(source) || throw(ArgumentError("model source must be a directory"))
    hashes = Dict{String,String}()
    raw = Dict{String,Any}()
    for name in MODEL_FILES
        path = joinpath(source, name)
        regular_file(path)
        hashes[name] = sha256_file(path)
        name in ("config.json", "tokenizer_config.json") && (raw[name] = JSON.parsefile(path))
    end
    cfg = raw["config.json"]
    mismatches = [key => (get(cfg, key, nothing), value) for (key, value) in ARCHITECTURE if get(cfg, key, nothing) != value]
    isempty(mismatches) || throw(ArgumentError("checkpoint is not the contracted SmolLM2-135M artifact: $mismatches"))
    get(raw["tokenizer_config.json"], "tokenizer_class", nothing) == "GPT2Tokenizer" ||
        throw(ArgumentError("checkpoint tokenizer is not the contracted GPT2Tokenizer"))
    return hashes, cfg
end

"`value` as Python's `json.dumps(value, sort_keys=True, indent=2)` writes it, plus a newline."
json_text(value) = sprint(write_json, value, 0) * "\n"

const JSON_ESCAPES = Dict('"' => "\\\"", '\\' => "\\\\", '\n' => "\\n", '\r' => "\\r", '\t' => "\\t", '\b' => "\\b", '\f' => "\\f")

function write_json(io, value, depth)
    indent(level) = print(io, "\n", "  "^level)
    if value isa AbstractDict || value isa AbstractVector
        opener, closer, items = value isa AbstractDict ? ('{', '}', sort!([string(k) => value[k] for k in keys(value)]; by=first)) :
                                                      ('[', ']', value)
        isempty(items) && return print(io, opener, closer)
        print(io, opener)
        for (i, item) in enumerate(items)
            i > 1 && print(io, ',')
            indent(depth + 1)
            item isa Pair && (write_json(io, first(item), depth + 1); print(io, ": "))
            write_json(io, item isa Pair ? last(item) : item, depth + 1)
        end
        indent(depth)
        print(io, closer)
    elseif value isa AbstractString
        print(io, '"')
        for c in value
            escaped = get(JSON_ESCAPES, c, nothing)
            if escaped !== nothing
                print(io, escaped)
            elseif isascii(c) && c >= ' '
                print(io, c)
            else
                foreach(unit -> print(io, "\\u", string(unit; base=16, pad=4)), transcode(UInt16, string(c)))
            end
        end
        print(io, '"')
    elseif value isa Union{Bool,Integer}
        print(io, value)
    elseif value === nothing
        print(io, "null")
    else
        throw(ArgumentError("installer JSON cannot hold a $(typeof(value))"))
    end
end

function write_atomic(path, text, mode)
    temp = joinpath(dirname(path), "." * basename(path) * "." * string(rand(UInt128); base=16))
    try
        open(temp, "w") do stream
            write(stream, text)
            flush(stream)
            ccall(:fsync, Cint, (Cint,), fd(stream)) == 0 || error("fsync failed: $temp")
        end
        chmod(temp, mode)
        Base.Filesystem.rename(temp, path)
    finally
        rm(temp; force=true)
    end
end

function atomic_json(path, value)
    mkpath(dirname(path); mode=0o750)
    write_atomic(path, json_text(value), 0o640)
end

"Builds a directory in a hidden sibling of `dest`, then renames it into place."
function staged(build, dest, prefix)
    stage = mktempdir(dirname(dest); prefix, cleanup=false)
    try
        build(stage)
        chmod(stage, 0o750)
        mv(stage, dest)
    finally
        rm(stage; recursive=true, force=true)
    end
    return dest
end

copy_file(src, dst) = cp(src, dst; follow_symlinks=false)

function install_model(source, artifact_store)
    hashes, cfg = validate_model_source(source)
    identity = inventory_digest(hashes)
    artifact_id = "smollm2-135m-" * first(identity, 16)
    artifact_store = resolved(artifact_store)
    mkpath(artifact_store; mode=0o750)
    dest = joinpath(artifact_store, artifact_id)
    info = Dict{String,Any}("id" => artifact_id, "model" => "SmolLM2-135M", "inventory_sha256" => identity,
                            "files" => hashes, "architecture" => Dict(key => cfg[key] for key in RECORDED_ARCHITECTURE))
    if ispath(dest) || islink(dest)
        (islink(dest) || !isdir(dest)) && error("existing model artifact destination is not a regular directory")
        regular_file(joinpath(dest, "artifact.json"))
        JSON.parsefile(joinpath(dest, "artifact.json")) == info &&
            all(name -> sha256_file(joinpath(dest, name)) == hashes[name], MODEL_FILES) ||
            error("existing versioned model artifact does not match its inventory")
        make_tree_read_only_accessible(dest)
        return dest, info
    end
    staged(dest, ".gesso-model-") do stage
        for name in MODEL_FILES
            src = joinpath(realpath(source), name)
            regular_file(src)
            target = joinpath(stage, name)
            copy_file(src, target)
            chmod(target, 0o440)
            sha256_file(target) == hashes[name] || error("copied model file failed its source digest: $name")
        end
        atomic_json(joinpath(stage, "artifact.json"), info)
        chmod(joinpath(stage, "artifact.json"), 0o440)
    end
    return dest, info
end

"The committed Gesso package files as (path, relative path), with its revision, version and inventory digest."
function source_frontier(gesso_source)
    root = realpath(gesso_source)
    project = TOML.parsefile(joinpath(root, "Project.toml"))
    get(project, "name", nothing) == "Gesso" && get(project, "uuid", nothing) == GESSO_UUID ||
        throw(ArgumentError("Gesso source root has an unexpected package identity"))
    git(args) = read(Cmd(`git $args`; dir=root), String)
    revision = strip(git(`rev-parse HEAD`))
    status = git(`status --porcelain --untracked-files=all --ignored=matching -- $GESSO_PATHS`)
    clean = success(Cmd(`git diff --quiet HEAD -- $GESSO_PATHS`; dir=root))
    isempty(status) && clean ||
        error("Gesso package source or project files are dirty; install a committed source frontier")
    files = Tuple{String,String}[]
    for relative in GESSO_PATHS
        path = joinpath(root, relative)
        islink(path) && error("Gesso package source contains a symlink: $path")
        if isdir(path)
            for (directory, names, items) in walkdir(path; follow_symlinks=false), name in [names; items]
                item = joinpath(directory, name)
                islink(item) && error("Gesso package source contains a symlink: $item")
                isdir(item) && continue
                regular_file(item)
                push!(files, (item, relpath(item, root)))
            end
        else
            regular_file(path)
            push!(files, (path, relative))
        end
    end
    sort!(files; by=last)
    source_hash = inventory_digest(Dict(relative => sha256_file(path) for (path, relative) in files))
    return revision, string(project["version"]), files, source_hash
end

copied_inventory(root, files) = inventory_digest(Dict(relative => sha256_file(joinpath(root, relative)) for (_, relative) in files))

function install_gesso_source(gesso_source, package_store; copy=copy_file)
    revision, version, files, source_hash = source_frontier(gesso_source)
    root = joinpath(resolved(package_store), "sources", "gesso", first(revision, 12) * "-" * first(source_hash, 12))
    mkpath(dirname(root); mode=0o750)
    if ispath(root) || islink(root)
        (islink(root) || !isdir(root)) && error("existing versioned Gesso source destination is not a regular directory")
        for (src, relative) in files
            target = joinpath(root, relative)
            matches = try
                regular_file(target)
                sha256_file(target) == sha256_file(src)
            catch
                false
            end
            matches || error("existing versioned Gesso source copy differs from its inventory")
        end
        copied_inventory(root, files) == source_hash || error("existing versioned Gesso source inventory differs")
        make_tree_read_only_accessible(root)
        return root, revision, version, source_hash
    end
    staged(root, ".gesso-source-") do stage
        for (src, relative) in files
            target = joinpath(stage, relative)
            mkpath(dirname(target); mode=0o750)
            copy(src, target)
            chmod(target, 0o440)
            sha256_file(target) == sha256_file(src) || error("copied Gesso source failed its digest: $relative")
        end
        for (directory, _, _) in walkdir(stage; topdown=false)
            chmod(directory, 0o750)
        end
        copied_inventory(stage, files) == source_hash ||
            error("copied Gesso source differs from the committed source inventory")
    end
    return root, revision, version, source_hash
end

function julia_string(value)
    any(in(('\n', '\r', '\0')), value) && throw(ArgumentError("installed paths cannot contain control characters"))
    return repr(String(value))
end

function deploy_capability(runtime_root, model_path, artifact_info, source_revision, source_sha256,
                           gesso_version, template_dir=@__DIR__)
    target = joinpath(resolved(runtime_root), "gesso")
    mkpath(dirname(target); mode=0o750)
    template = replace(read(joinpath(template_dir, "gesso.jl"), String),
        "__PALETTE_GESSO_ARTIFACT_ROOT__" => julia_string(realpath(model_path)),
        "__PALETTE_GESSO_ARTIFACT_ID__" => artifact_info["id"],
        "__PALETTE_GESSO_SOURCE_REVISION__" => source_revision,
        "__PALETTE_GESSO_SOURCE_SHA256__" => source_sha256,
        "__PALETTE_GESSO_VERSION__" => gesso_version)
    occursin("__PALETTE_GESSO_", template) && throw(ArgumentError("Gesso capability source has an unresolved install placeholder"))
    manifest = JSON.parsefile(joinpath(template_dir, "capability.json"))
    manifest["version"] = "$(manifest["version"])+$(first(source_revision, 10)).$(first(artifact_info["inventory_sha256"], 10))"
    islink(target) && error("capability destination must not be a symlink")
    mkpath(target; mode=0o750)
    write_atomic(joinpath(target, manifest["source"]), template, 0o440)
    atomic_json(joinpath(target, "capability.json"), manifest)
    return target, manifest["version"]
end

"Runs `cmd`, killing it after `seconds`; returns (succeeded, stdout, stderr)."
function capture(cmd, seconds)
    out, err = IOBuffer(), IOBuffer()
    process = run(pipeline(cmd; stdin=devnull, stdout=out, stderr=err); wait=false)
    timer = Timer(_ -> kill(process), seconds)
    wait(process)
    timed_out = !isopen(timer)
    close(timer)
    timed_out && error("$(cmd.exec[1]) timed out after $seconds seconds")
    return success(process), String(take!(out)), String(take!(err))
end

const REGISTER = "using Pkg; Pkg.develop(Pkg.PackageSpec(path=ARGS[1])); Pkg.add(Pkg.PackageSpec(name=\"JSON\")); " *
                 "using Gesso, JSON; println(\"PALETTE_GESSO_VERSION=\", Base.pkgversion(Gesso)); " *
                 "println(\"PALETTE_JSON_VERSION=\", Base.pkgversion(JSON))"

function install(; gesso_source, model_source, package_store, artifact_store, runtime_root,
                 julia=joinpath(Sys.BINDIR, Base.julia_exename()))
    gesso_source, model_source = realpath(gesso_source), realpath(model_source)
    package_store, artifact_store, runtime_root = resolved.((package_store, artifact_store, runtime_root))
    for destination in (package_store, artifact_store, runtime_root)
        path_disjoint([gesso_source, destination])
        path_disjoint([INTEGRATION_DIR, destination])
    end
    path_disjoint([package_store, artifact_store, runtime_root])
    path_disjoint([gesso_source, PALETTE_ROOT])
    path_disjoint([package_store, PALETTE_ROOT])
    path_disjoint([artifact_store, PALETTE_ROOT])
    environment, depot = joinpath(package_store, "environment"), joinpath(package_store, "depot")
    isdir(environment) && isdir(depot) ||
        throw(ArgumentError("package_store must already contain its trusted environment/ and depot/ roots"))
    julia = realpath(julia)
    gesso_copy, revision, gesso_version, source_sha256 = install_gesso_source(gesso_source, package_store)
    artifact_path, artifact = install_model(model_source, artifact_store)
    open(joinpath(package_store, ".package-store.lock"), "a+") do lock
        ccall(:flock, Cint, (Cint, Cint), fd(lock), 2) == 0 || error("could not lock the package store")
        env = Dict("PATH" => "$(dirname(julia)):/usr/bin:/bin", "LANG" => "C.UTF-8",
                   "JULIA_DEPOT_PATH" => realpath(depot), "JULIA_PROJECT" => realpath(environment),
                   "JULIA_PKG_OFFLINE" => "true")
        ok, stdout, stderr = capture(setenv(`$julia --startup-file=no --project=$environment -e $REGISTER $gesso_copy`, env), 300)
        ok || error("offline Pkg.develop of the copied Gesso source failed: " * last(stderr, 2000))
        lines = split(stdout, '\n')
        versions(prefix) = [chopprefix(line, prefix) for line in lines if startswith(line, prefix)]
        versions("PALETTE_GESSO_VERSION=") == [gesso_version] ||
            error("loaded Gesso package version differs from the copied source version")
        length(versions("PALETTE_JSON_VERSION=")) == 1 ||
            error("the optional package environment did not resolve Gesso's JSON dependency")
    end
    capability_dir, capability_version = deploy_capability(runtime_root, artifact_path, artifact,
                                                           revision, source_sha256, gesso_version)
    return Dict("status" => "installed", "capability_id" => "gesso", "capability_version" => capability_version,
                "capability_dir" => capability_dir, "gesso_package_dir" => gesso_copy,
                "package_environment" => environment, "artifact_id" => artifact["id"],
                "artifact_dir" => artifact_path, "artifact_inventory_sha256" => artifact["inventory_sha256"],
                "required_profile_read_root" => artifact_path,
                "note" => "Add required_profile_read_root to the trusted server profile read_roots; no model or Gesso source path is accepted at runtime.")
end

const USAGE = """
usage: julia --project=<Palette checkout> runtime/integrations/gesso/install.jl
         --gesso-source DIR    read-only source checkout matching a clean committed Gesso frontier
         --model-source DIR    existing local SmolLM2-135M snapshot; never downloaded
         --package-store DIR   trusted Palette package store with environment/ and depot/
         --artifact-store DIR  server-owned, read-only-mounted model artifact directory
         --runtime-root DIR    trusted PALETTE_RUNTIME_ROOT
         [--julia PATH]        trusted Julia executable; defaults to the one running this
"""
const FLAGS = ("--gesso-source", "--model-source", "--package-store", "--artifact-store", "--runtime-root", "--julia")

function parse_args(args)
    iseven(length(args)) || throw(ArgumentError("arguments must be --name value pairs"))
    parsed = Dict{Symbol,String}()
    for i in 1:2:length(args)
        args[i] in FLAGS || throw(ArgumentError("unsupported argument: $(args[i])"))
        key = Symbol(replace(chopprefix(args[i], "--"), '-' => '_'))
        haskey(parsed, key) && throw(ArgumentError("duplicate argument: $(args[i])"))
        parsed[key] = args[i+1]
    end
    absent = [flag for flag in FLAGS[1:5] if !haskey(parsed, Symbol(replace(chopprefix(flag, "--"), '-' => '_')))]
    isempty(absent) || throw(ArgumentError("missing required arguments: $(join(absent, ", "))"))
    return parsed
end

function main(args)
    parsed = try
        parse_args(args)
    catch e
        e isa ArgumentError || rethrow()
        print(stderr, e.msg, "\n", USAGE)
        exit(2)
    end
    report = try
        install(; parsed...)
    catch e
        println(stderr, "error: ", sprint(showerror, e))
        exit(1)
    end
    print(json_text(report))
end

end

if abspath(PROGRAM_FILE) == @__FILE__
    GessoInstall.main(ARGS)
end
