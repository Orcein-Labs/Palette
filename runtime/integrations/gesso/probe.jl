#!/usr/bin/env julia
# Direct, source-gated proof of the installed Gesso CPU inference path.

# This is an operator test recipe. It accepts only the server-owned artifact
# directory and the expected package digest/revision supplied by the installer;
# runtime callers cannot invoke it or select arbitrary files or commands.
using Gesso
using JSON
using SHA

const MODEL_FILES = ("config.json", "model.safetensors", "merges.txt", "vocab.json",
                     "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json")

function parse_args(args)
    length(args) % 2 == 0 || error("arguments must be --name value pairs")
    parsed = Dict{String,String}()
    allowed = Set(["--model-root", "--source-sha256", "--source-revision", "--backend",
                   "--prompt", "--max-new-tokens"])
    for i in 1:2:length(args)
        key = args[i]
        key in allowed || error("unsupported argument: $key")
        haskey(parsed, key) && error("duplicate argument: $key")
        parsed[key] = args[i + 1]
    end
    required = ("--model-root", "--source-sha256", "--source-revision", "--backend")
    all(key -> haskey(parsed, key), required) || error("missing required probe arguments")
    get!(parsed, "--prompt", "Palette local inference probe.")
    get!(parsed, "--max-new-tokens", "2")
    return parsed
end

function digest_file(path)
    islink(path) && error("refusing symbolic link: $(basename(path))")
    isfile(path) || error("missing file: $(basename(path))")
    return open(path, "r") do io
        bytes2hex(SHA.sha256(io))
    end
end

function package_inventory()
    root = dirname(dirname(Base.pathof(Gesso)))
    paths = String[]
    for relative in ("Project.toml", "src", "ext")
        path = joinpath(root, relative)
        if isdir(path)
            for (directory, _, names) in walkdir(path; follow_symlinks=false), name in names
                file = joinpath(directory, name)
                islink(file) && error("Gesso source contains a symbolic link")
                isfile(file) && push!(paths, file)
            end
        else
            push!(paths, path)
        end
    end
    rows = String[]
    for path in sort!(paths)
        relative = replace(relpath(path, root), '\\' => '/')
        push!(rows, relative * "\0" * digest_file(path))
    end
    return bytes2hex(SHA.sha256(join(rows, "\n")))
end

function check_artifact(root)
    isdir(root) && !islink(root) || error("model root must be a real directory")
    marker = joinpath(root, "artifact.json")
    islink(marker) && error("artifact inventory must not be a symbolic link")
    isfile(marker) || error("artifact inventory is missing")
    info = JSON.parsefile(marker)
    files = get(info, "files", nothing)
    files isa AbstractDict || error("artifact file inventory is missing")
    Set(String.(keys(files))) == Set(MODEL_FILES) || error("artifact file inventory differs")
    rows = String[]
    for name in sort!(collect(MODEL_FILES))
        actual = digest_file(joinpath(root, name))
        actual == get(files, name, nothing) || error("artifact digest mismatch: $name")
        push!(rows, name * "\0" * actual)
    end
    inventory = bytes2hex(SHA.sha256(join(rows, "\n")))
    inventory == get(info, "inventory_sha256", nothing) || error("artifact inventory digest mismatch")
    return info
end

function check_optional_backend(backend::String)
    package, type_name = backend == "cuda" ? ("CUDA", :CUDABackend) :
                        backend == "lava" ? ("Lava", :LavaBackend) : ("", :none)
    isempty(package) && error("backend must be cpu, cuda, or lava")
    Base.find_package(package) === nothing && throw(Gesso.gesso_error(
        Gesso.ERR_RESOURCE_LIMIT, "$backend backend package is unavailable";
        requested_backend=Symbol(backend), package_available=false))
    try
        Core.eval(@__MODULE__, :(import $(Symbol(package))))
        Base.invokelatest() do
            isdefined(Gesso, type_name) || throw(Gesso.gesso_error(
                Gesso.ERR_RESOURCE_LIMIT, "$backend extension is unavailable";
                requested_backend=Symbol(backend), extension_available=false))
            getfield(Gesso, type_name)()
        end
    catch err
        err isa Gesso.GessoError && rethrow()
        throw(Gesso.gesso_error(Gesso.ERR_RESOURCE_LIMIT,
            "$backend backend could not be loaded";
            requested_backend=Symbol(backend), diagnostic=first(sprint(showerror, err), 700)))
    end
    throw(Gesso.gesso_error(Gesso.ERR_RESOURCE_LIMIT,
        "$backend probe currently validates device construction only";
        requested_backend=Symbol(backend), inference_verified=false))
end

function run_probe(args)
    opts = parse_args(args)
    source_sha = opts["--source-sha256"]
    occursin(r"^[0-9a-f]{64}$", source_sha) || error("expected source digest must be SHA-256")
    package_inventory() == source_sha || error("loaded Gesso source differs from the expected source digest")
    info = check_artifact(opts["--model-root"])
    backend = opts["--backend"]
    backend == "cpu" || check_optional_backend(backend)
    prompt = opts["--prompt"]
    !isempty(prompt) && ncodeunits(prompt) <= 2048 || error("prompt must be 1..2048 UTF-8 bytes")
    max_new = tryparse(Int, opts["--max-new-tokens"])
    max_new !== nothing && 1 <= max_new <= 4 || error("max-new-tokens must be in 1..4 for the direct probe")
    model, tensors, config = Gesso.load_llama(opts["--model-root"])
    tokenizer = Gesso.load_gpt2_tokenizer(opts["--model-root"])
    prompt_ids = Gesso.encode(tokenizer, prompt)
    !isempty(prompt_ids) || error("tokenizer produced an empty prompt")
    context_length = max(64, length(prompt_ids) + max_new + 1)
    context_length <= 8192 || error("prompt exceeds checkpoint context length")
    sink = Gesso.InMemorySink()
    session = Gesso.Session(model, tensors; backend=Gesso.CPUBackend(), page_size=16,
        context_length=context_length, eos_token_id=tokenizer.eos_token_id,
        tokenizer=tokenizer, sink=sink)
    all_ids = Gesso.generate(session, prompt; max_new_tokens=max_new)
    generated = all_ids[(length(prompt_ids) + 1):end]
    receipts = Gesso.Profiling.engine_report(sink)
    return Dict{String,Any}(
        "status" => "passed", "backend" => "cpu", "gesso_version" => string(Base.pkgversion(Gesso)),
        "gesso_source_revision" => opts["--source-revision"], "gesso_source_sha256" => source_sha,
        "artifact_id" => info["id"], "artifact_inventory_sha256" => info["inventory_sha256"],
        "architecture" => Dict("hidden_size" => config.hidden_size,
            "layers" => config.num_hidden_layers, "vocab_size" => config.vocab_size),
        "prompt_token_count" => length(prompt_ids), "new_token_ids" => generated,
        "text" => Gesso.decode(tokenizer, generated), "receipt_count" => length(receipts),
        "receipts" => [Dict{String,Any}(String(k) => v for (k, v) in pairs(r)) for r in receipts],
        "kv_bytes" => Gesso.Profiling.kv_footprint(session.mgr),
        "page_count" => Gesso.Profiling.page_footprint(session.mgr),
        "context_length" => session.context_length)
end

if abspath(PROGRAM_FILE) == @__FILE__
    try
        result = run_probe(ARGS)
        JSON.print(stdout, result)
        println()
    catch err
        failure = Dict{String,Any}("status" => "failed", "error_type" => string(nameof(typeof(err))),
            "error" => first(sprint(showerror, err), 700))
        err isa Gesso.GessoError && (failure["error_code"] = string(err.code))
        JSON.print(stderr, failure)
        println(stderr)
        exit(1)
    end
end
