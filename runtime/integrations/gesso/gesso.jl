# Optional Palette adapter over Gesso's existing local inference APIs.
using Gesso
using JSON
using SHA

const _GESSO_ARTIFACT_ROOT = __PALETTE_GESSO_ARTIFACT_ROOT__
const _GESSO_ARTIFACT_ID = "__PALETTE_GESSO_ARTIFACT_ID__"
const _GESSO_SOURCE_REVISION = "__PALETTE_GESSO_SOURCE_REVISION__"
const _GESSO_SOURCE_SHA256 = "__PALETTE_GESSO_SOURCE_SHA256__"
const _GESSO_VERSION = "__PALETTE_GESSO_VERSION__"
const _GESSO_FILES = Set([
    "config.json", "model.safetensors", "merges.txt", "vocab.json",
    "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json",
])
const _GESSO_MODEL = Ref{Any}(nothing)
const _GESSO_LAST_REPORT = Ref{Any}(nothing)
const _GESSO_LOCK = ReentrantLock()

function _source_inventory_sha256()
    root = dirname(dirname(Base.pathof(Gesso)))
    files = String[]
    for relative in ("Project.toml", "src", "ext")
        path = joinpath(root, relative)
        if isdir(path)
            for (directory, _, names) in walkdir(path; follow_symlinks=false), name in names
                file = joinpath(directory, name)
                islink(file) && throw(ArgumentError("installed Gesso package source contains a symlink"))
                isfile(file) && push!(files, file)
            end
        else
            push!(files, path)
        end
    end
    rows = String[]
    for file in sort!(files)
        relative = replace(relpath(file, root), '\\' => '/')
        push!(rows, relative * "\0" * _sha256_file(file))
    end
    return bytes2hex(SHA.sha256(join(rows, "\n")))
end

function _sha256_file(path::String)
    islink(path) && throw(ArgumentError("Gesso artifact contains a symbolic link: $(basename(path))"))
    isfile(path) || throw(ArgumentError("Gesso artifact file is missing: $(basename(path))"))
    open(path, "r") do io
        return bytes2hex(SHA.sha256(io))
    end
end

function _artifact(full_verify::Bool)
    isdir(_GESSO_ARTIFACT_ROOT) && !islink(_GESSO_ARTIFACT_ROOT) || throw(ArgumentError("server-owned Gesso model read root must be a real directory"))
    marker = joinpath(_GESSO_ARTIFACT_ROOT, "artifact.json")
    islink(marker) && throw(ArgumentError("Gesso artifact inventory must not be a symbolic link"))
    isfile(marker) || throw(ArgumentError("Gesso artifact inventory is missing"))
    info = JSON.parsefile(marker)
    get(info, "id", nothing) == _GESSO_ARTIFACT_ID || throw(ArgumentError("Gesso artifact identity differs from the installed capability"))
    files = get(info, "files", nothing)
    files isa AbstractDict || throw(ArgumentError("Gesso artifact inventory is malformed"))
    Set(String.(keys(files))) == _GESSO_FILES || throw(ArgumentError("Gesso artifact file inventory is incomplete or unexpected"))
    rows = String[]
    for name in sort!(collect(_GESSO_FILES))
        path = joinpath(_GESSO_ARTIFACT_ROOT, name)
        isfile(path) && !islink(path) || throw(ArgumentError("Gesso artifact must contain a regular file: $name"))
        expected = get(files, name, nothing)
        expected isa String && occursin(r"^[0-9a-f]{64}$", expected) || throw(ArgumentError("Gesso artifact hash is malformed: $name"))
        actual = full_verify ? _sha256_file(path) : expected
        actual == expected || throw(ArgumentError("Gesso artifact digest mismatch: $name"))
        push!(rows, name * "\0" * actual)
    end
    inventory = bytes2hex(SHA.sha256(join(rows, "\n")))
    inventory == get(info, "inventory_sha256", nothing) || throw(ArgumentError("Gesso artifact inventory digest mismatch"))
    return info
end

function _load!()
    lock(_GESSO_LOCK) do
        _GESSO_MODEL[] === nothing || return _GESSO_MODEL[]
        info = _artifact(true)
        Base.pkgversion(Gesso) == VersionNumber(_GESSO_VERSION) || throw(ArgumentError("loaded Gesso package version does not match the installed capability"))
        _source_inventory_sha256() == _GESSO_SOURCE_SHA256 || throw(ArgumentError("loaded Gesso package source differs from the installed capability"))
        model, tensors, cfg = Gesso.load_llama(_GESSO_ARTIFACT_ROOT)
        tokenizer = Gesso.load_gpt2_tokenizer(_GESSO_ARTIFACT_ROOT)
        _GESSO_MODEL[] = (model=model, tensors=tensors, config=cfg,
                          tokenizer=tokenizer, artifact=info)
        return _GESSO_MODEL[]
    end
end

function _backend_diagnostic(package::String, backend_name::Symbol)
    source = Base.find_package(package)
    source === nothing && return Dict{String,Any}("status" => "unavailable", "reason" => "optional package $package is not installed")
    try
        Core.eval(@__MODULE__, :(import $(Symbol(package))))
        # Importing an optional package can introduce the extension's backend
        # type and methods after this diagnostic's invocation world.
        return Base.invokelatest() do
            type_name = backend_name === :cuda ? :CUDABackend : :LavaBackend
            isdefined(Gesso, type_name) || return Dict{String,Any}("status" => "unavailable", "reason" => "Gesso extension did not expose $type_name")
            backend = getfield(Gesso, type_name)()
            required = Gesso.required_semantics(Gesso.architecture_spec(joinpath(_GESSO_ARTIFACT_ROOT, "config.json")))
            return Dict{String,Any}("status" => "available", "backend" => String(Gesso.backend_name(backend)),
                "required_semantics" => String.(required),
                "supports" => Dict(String(cap) => Gesso.supports(backend, cap) for cap in required))
        end
    catch err
        return Dict{String,Any}("status" => "unavailable", "reason" => first(sprint(showerror, err), 700),
                                "error_type" => string(nameof(typeof(err))))
    end
end

function _diagnose()
    info = _artifact(false)
    spec = Gesso.architecture_spec(joinpath(_GESSO_ARTIFACT_ROOT, "config.json"))
    backend = Gesso.CPUBackend()
    required = Gesso.required_semantics(spec)
    supported = Dict(String(cap) => Gesso.supports(backend, cap) for cap in required)
    tokenizer = Gesso.load_gpt2_tokenizer(_GESSO_ARTIFACT_ROOT)
    sample_ids = Gesso.encode(tokenizer, "Palette Gesso")
    round_trip = Gesso.decode(tokenizer, sample_ids)
    return Dict{String,Any}(
        "status" => "ready", "gesso_version" => string(Base.pkgversion(Gesso)),
        "gesso_source_revision" => _GESSO_SOURCE_REVISION,
        "artifact_id" => info["id"], "artifact_inventory_sha256" => info["inventory_sha256"],
        "architecture" => Dict("family" => String(spec.family), "hidden_size" => spec.hidden_size,
            "layers" => spec.num_layers, "heads" => spec.n_heads, "kv_heads" => spec.n_kv_heads,
            "vocab_size" => spec.vocab_size, "context_limit" => Int(JSON.parsefile(joinpath(_GESSO_ARTIFACT_ROOT, "config.json"))["max_position_embeddings"])),
        "cpu" => Dict("status" => all(values(supported)) ? "available" : "incompatible",
            "backend" => String(Gesso.backend_name(backend)), "required_semantics" => String.(required), "supports" => supported,
            "import_report" => Gesso.import_report(spec; backend=backend)),
        "tokenizer" => Dict("metadata" => Dict(String(k) => v for (k, v) in pairs(Gesso.tokenizer_metadata(tokenizer))),
            "round_trip" => round_trip == "Palette Gesso", "sample_token_count" => length(sample_ids)),
        "cuda" => _backend_diagnostic("CUDA", :cuda),
        "lava" => _backend_diagnostic("Lava", :lava),
        "known_families" => String.(Gesso.known_families()),
        "compatibility_table" => sprint(io -> Gesso.compatibility_table(io; backend=backend)),
    )
end

function _profile(sink)
    reports = Gesso.Profiling.engine_report(sink)
    return [Dict{String,Any}(String(k) => v for (k, v) in pairs(report)) for report in reports]
end

function _bounded_prompt(args)
    prompt = get(args, "prompt", nothing)
    prompt isa String && !isempty(prompt) || throw(ArgumentError("prompt must be a non-empty string"))
    ncodeunits(prompt) <= 2048 || throw(ArgumentError("prompt exceeds the 2048-byte limit"))
    max_new = get(args, "max_new_tokens", 4)
    max_new isa Integer && !(max_new isa Bool) && 1 <= max_new <= 16 || throw(ArgumentError("max_new_tokens must be an integer in 1..16"))
    return prompt, Int(max_new)
end

function _tokenize(args)
    Set(String.(keys(args))) == Set(["action", "text"]) || throw(ArgumentError("tokenize accepts only text"))
    text = get(args, "text", nothing)
    text isa String && !isempty(text) || throw(ArgumentError("text must be a non-empty string"))
    ncodeunits(text) <= 2048 || throw(ArgumentError("text exceeds the 2048-byte limit"))
    _artifact(false)
    tokenizer = Gesso.load_gpt2_tokenizer(_GESSO_ARTIFACT_ROOT)
    ids = Gesso.encode(tokenizer, text)
    length(ids) <= 128 || throw(ArgumentError("tokenize output exceeds the 128-token limit"))
    return Dict{String,Any}("status" => "tokenized", "artifact_id" => _GESSO_ARTIFACT_ID,
        "tokenizer" => String(Gesso.tokenizer_metadata(tokenizer).kind),
        "token_count" => length(ids), "token_ids" => ids)
end

function _decode(args)
    Set(String.(keys(args))) == Set(["action", "token_ids"]) || throw(ArgumentError("decode accepts only token_ids"))
    raw = get(args, "token_ids", nothing)
    raw isa AbstractVector && length(raw) <= 128 || throw(ArgumentError("token_ids must be an array with at most 128 entries"))
    ids = Int[]
    for value in raw
        value isa Integer && !(value isa Bool) && 0 <= value < 49152 ||
            throw(ArgumentError("each token id must be an integer in 0..49151"))
        push!(ids, Int(value))
    end
    _artifact(false)
    tokenizer = Gesso.load_gpt2_tokenizer(_GESSO_ARTIFACT_ROOT)
    return Dict{String,Any}("status" => "decoded", "artifact_id" => _GESSO_ARTIFACT_ID,
        "text" => Gesso.decode(tokenizer, ids), "token_count" => length(ids))
end

function _generate(args)
    allowed = Set(["action", "prompt", "max_new_tokens"])
    isempty(setdiff(Set(String.(keys(args))), allowed)) || throw(ArgumentError("generate accepts only prompt and max_new_tokens"))
    prompt, max_new = _bounded_prompt(args)
    loaded = _load!()
    prompt_ids = Gesso.encode(loaded.tokenizer, prompt)
    isempty(prompt_ids) && throw(ArgumentError("tokenizer produced an empty prompt"))
    context_length = max(64, length(prompt_ids) + max_new + 1)
    context_length <= 8192 || throw(ArgumentError("prompt exceeds the checkpoint context limit"))
    sink = Gesso.InMemorySink()
    session = Gesso.Session(loaded.model, loaded.tensors; backend=Gesso.CPUBackend(),
        page_size=16, context_length=context_length, eos_token_id=loaded.tokenizer.eos_token_id,
        tokenizer=loaded.tokenizer, sink=sink)
    ids = Gesso.generate(session, prompt; max_new_tokens=max_new)
    generated = ids[(length(prompt_ids)+1):end]
    report = _profile(sink)
    _GESSO_LAST_REPORT[] = report
    return Dict{String,Any}("status" => "generated", "artifact_id" => loaded.artifact["id"],
        "backend" => "cpu", "prompt_token_count" => length(prompt_ids), "new_token_ids" => generated,
        "text" => Gesso.decode(loaded.tokenizer, generated), "receipt" => last(report),
        "profile" => report, "kv_bytes" => Gesso.Profiling.kv_footprint(session.mgr),
        "page_count" => Gesso.Profiling.page_footprint(session.mgr),
        "context_length" => session.context_length)
end

function _fork(args)
    allowed = Set(["action", "prompt"])
    isempty(setdiff(Set(String.(keys(args))), allowed)) || throw(ArgumentError("fork accepts only prompt"))
    prompt, _ = _bounded_prompt(merge(Dict{String,Any}(args), Dict("max_new_tokens" => 1)))
    loaded = _load!()
    ids = Gesso.encode(loaded.tokenizer, prompt)
    isempty(ids) && throw(ArgumentError("tokenizer produced an empty prompt"))
    context_length = max(64, length(ids) + 4)
    parent_sink = Gesso.InMemorySink()
    parent = Gesso.Session(loaded.model, loaded.tensors; backend=Gesso.CPUBackend(),
        page_size=16, context_length=context_length, eos_token_id=loaded.tokenizer.eos_token_id,
        tokenizer=loaded.tokenizer, sink=parent_sink)
    Gesso.prefill!(parent, ids)
    left_sink, right_sink = Gesso.InMemorySink(), Gesso.InMemorySink()
    left = Gesso.fork(parent; sink=left_sink)
    right = Gesso.fork(parent; sink=right_sink)
    shared_bytes = Gesso.Profiling.unique_kv_bytes(parent.mgr, left.mgr, right.mgr)
    parent_bytes = Gesso.Profiling.kv_footprint(parent.mgr)
    left_id, right_id = Gesso.decode!(left), Gesso.decode!(right)
    reports = vcat(_profile(parent_sink), _profile(left_sink), _profile(right_sink))
    _GESSO_LAST_REPORT[] = reports
    return Dict{String,Any}("status" => "forked", "artifact_id" => loaded.artifact["id"],
        "backend" => "cpu", "prompt_token_count" => length(ids), "left_token_id" => left_id,
        "right_token_id" => right_id, "parent_kv_bytes" => parent_bytes,
        "left_text" => Gesso.decode(loaded.tokenizer, [left_id]),
        "right_text" => Gesso.decode(loaded.tokenizer, [right_id]),
        "unique_shared_kv_bytes_before_branch_decode" => shared_bytes,
        "left_kv_bytes" => Gesso.Profiling.kv_footprint(left.mgr),
        "right_kv_bytes" => Gesso.Profiling.kv_footprint(right.mgr),
        "profile" => reports)
end

"""Palette runtime capability entrypoint. Paths and package choices are fixed by install."""
function gesso(arguments::Dict{String,Any})
    action = get(arguments, "action", "diagnose")
    action isa String || throw(ArgumentError("action must be a string"))
    if action == "diagnose"
        Set(String.(keys(arguments))) == Set(["action"]) || throw(ArgumentError("diagnose accepts only action"))
        return _diagnose()
    elseif action == "load"
        Set(String.(keys(arguments))) == Set(["action"]) || throw(ArgumentError("load accepts only action"))
        loaded = _load!()
        return Dict{String,Any}("status" => "loaded", "artifact_id" => loaded.artifact["id"],
            "config" => Dict("hidden_size" => loaded.config.hidden_size,
                "layers" => loaded.config.num_hidden_layers, "vocab_size" => loaded.config.vocab_size),
            "tokenizer" => Dict(String(k) => v for (k, v) in pairs(Gesso.tokenizer_metadata(loaded.tokenizer))))
    elseif action == "generate"
        return _generate(arguments)
    elseif action == "fork"
        return _fork(arguments)
    elseif action == "profile"
        Set(String.(keys(arguments))) == Set(["action"]) || throw(ArgumentError("profile accepts only action"))
        return Dict{String,Any}("status" => "profiled", "reports" => something(_GESSO_LAST_REPORT[], Any[]))
    elseif action == "tokenize"
        return _tokenize(arguments)
    elseif action == "decode"
        return _decode(arguments)
    elseif action == "cuda" || action == "lava"
        Set(String.(keys(arguments))) == Set(["action"]) || throw(ArgumentError("backend diagnostics accept only action"))
        package = action == "cuda" ? "CUDA" : "Lava"
        result = _backend_diagnostic(package, Symbol(action))
        if result["status"] != "available"
            throw(Gesso.gesso_error(Gesso.ERR_RESOURCE_LIMIT,
                "requested $package backend is unavailable: $(result["reason"])";
                requested_backend=Symbol(action), diagnostic=result))
        end
        return Dict{String,Any}("backend" => action, "result" => result)
    end
    throw(ArgumentError("action must be diagnose, load, tokenize, decode, generate, fork, profile, cuda, or lava"))
end
