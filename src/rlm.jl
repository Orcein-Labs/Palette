# Recursive children (RLM) from Julia code. The agent host runs them; this is
# the kernel's side of the same requests the IPython kernel's `rlm` module
# makes: each call is one host request through this session's broker, allowed
# only for the request types its ceiling names. Shapes, checks and messages
# follow prime-agent-runtime/src/rlm/__init__.py so both kernels behave alike
# against one host.

"""
    Palette.host_request(type, payload = Dict()) -> Dict

Ask the agent host to act, and return its result. The request goes through
this session's broker, which allows only the request types in the session's
ceiling (`host_request.allowed_types`) and keeps a receipt of each.
"""
function host_request(rtype::AbstractString, payload::AbstractDict=Dict{String,Any}())
    isempty(rtype) && throw(ArgumentError("request_type must be a non-empty string"))
    resp = request_capability("host_request", Dict{String,Any}("type" => String(rtype), "payload" => payload))
    get(resp, "approved", false) === true || error("host request $rtype was not allowed: $(get(resp, "reason", "no reason given"))")
    reply = resp["result"]
    status = get(reply, "status", nothing)
    status == "ok" && return reply["result"]
    status == "error" && error(something(get(reply, "error", nothing), "host request $rtype failed"))
    error("host request $rtype returned unexpected status: $(repr(status))")
end

"""
    Palette.runtime(action, payload=Dict()) -> Dict

Discover, invoke, refresh, or repair a deployed runtime capability through the
fixed broker request type `palette.runtime`. Capability source is supplied by
the trusted host registry and is evaluated only inside this sandboxed Julia
worker. The host never imports or executes capability code.
"""
function runtime(action::AbstractString, payload::AbstractDict=Dict{String,Any}())
    action in ("discover", "invoke", "refresh", "repair") ||
        throw(ArgumentError("runtime action must be discover, invoke, refresh, or repair"))
    result = host_request("palette.runtime", Dict{String,Any}(
        "action" => String(action), "payload" => Dict{String,Any}(String(k) => v for (k, v) in payload)))
    result isa AbstractDict || error("palette.runtime returned a non-object result")
    response = Dict{String,Any}(String(k) => v for (k, v) in result)
    if action == "discover"
        by_capability = Dict{String,Vector{String}}()
        all_missing = Set{String}()
        for capability in get(response, "available_capabilities", Any[])
            capability isa AbstractDict || continue
            identifier = get(capability, "id", nothing)
            dependencies = get(capability, "dependencies", Any[])
            identifier isa String && dependencies isa AbstractVector || continue
            missing = String[]
            for dependency in dependencies
                dependency isa String || continue
                Base.find_package(dependency) === nothing && push!(missing, dependency)
            end
            isempty(missing) || (by_capability[identifier] = missing)
            union!(all_missing, missing)
        end
        response["missing_dependencies"] = sort!(collect(all_missing))
        response["missing_dependencies_by_capability"] = by_capability
    elseif action == "invoke"
        source = get(response, "source", nothing)
        manifest = get(response, "manifest", nothing)
        source isa String || error("palette.runtime invoke reply has no Julia source")
        manifest isa AbstractDict || error("palette.runtime invoke reply has no manifest")
        identifier = get(manifest, "id", nothing)
        entrypoint = get(manifest, "entrypoint", nothing)
        expected = get(response, "source_sha256", nothing)
        identifier isa String && entrypoint isa String || error("palette.runtime invoke manifest is incomplete")
        expected isa String && bytes2hex(SHA.sha256(source)) == expected || error("palette.runtime capability source identity mismatch")
        dependencies = get(manifest, "dependencies", Any[])
        dependencies isa AbstractVector || error("palette.runtime manifest dependencies are invalid")
        missing = String[dependency for dependency in dependencies if dependency isa String && Base.find_package(dependency) === nothing]
        isempty(missing) || throw(ArgumentError("capability $(identifier) is missing Julia dependencies: $(join(missing, ", "))"))
        occursin(r"^[A-Za-z_][A-Za-z0-9_]*$", entrypoint) || error("palette.runtime entrypoint is invalid")
        generation = get(response, "generation", nothing)
        generation isa String || error("palette.runtime invoke reply has no generation")
        key = (identifier, generation)
        mod = get!(RUNTIME_CAPABILITY_MODULES, key) do
            name = Symbol("PaletteCapability_", replace(identifier, r"[^A-Za-z0-9_]" => "_"), "_", first(generation, 12))
            module_ = Module(name)
            isdefined(module_, Symbol(entrypoint)) &&
                throw(ArgumentError("palette.runtime entrypoint $(entrypoint) shadows a Julia Base/Core binding"))
            Core.eval(module_, :(const Palette = $(@__MODULE__)))
            Base.include_string(module_, source, "capability://$(identifier)/$(generation).jl")
            module_
        end
        isdefined(mod, Symbol(entrypoint)) || error("palette.runtime entrypoint $(entrypoint) was not defined by capability source")
        fn = getfield(mod, Symbol(entrypoint))
        fn isa Function || error("palette.runtime entrypoint $(entrypoint) is not a function")
        arguments = get(payload, "arguments", Dict{String,Any}())
        arguments isa AbstractDict || throw(ArgumentError("invoke payload.arguments must be an object"))
        value = Base.invokelatest(fn, Dict{String,Any}(String(k) => v for (k, v) in arguments))
        return Dict{String,Any}("id" => identifier, "generation" => generation, "result" => value)
    end
    return response
end

const RUNTIME_CAPABILITY_MODULES = Dict{Tuple{String,String},Module}()

module rlm

using ..Palette: host_request

export spawn, create_session, find_models, list_subagents, collect, progress_note, delete_subagent

const PROGRESS_NOTE_MAX_LENGTH = 512
# A host request must be answered within the bridge's limit (the call timeout
# less 5 s); a longer wait belongs in a later call.
const COLLECT_MAX_TIMEOUT_MS = 50_000

struct SpawnHandle
    rlm_child_id::String
    name::String
    session_dir::String
    model::String
end

struct CreateSessionHandle
    active_session_id::String
    session_id::String
    name::String
    session_file::String
    model::String
end

struct Model
    provider::String
    id::String
    name::String
    selector::String
end

struct SubagentActivity
    kind::String
    tool_name::Union{Nothing,String}
end

struct Subagent
    rlm_child_id::String
    active_session_id::Union{Nothing,String}
    session_id::Union{Nothing,String}
    session_name::String
    session_dir::String
    status::String
    activity::Union{Nothing,SubagentActivity}
    tool_use_count::Union{Nothing,Int}
    duration_ms::Union{Nothing,Int}
    answer_preview::Union{Nothing,String}
    replied_since_task::Union{Nothing,Bool}
    progress_note::Union{Nothing,String}
    label::Union{Nothing,String}
    last_activity_at::Union{Nothing,Int}
    activity_stale_ms::Union{Nothing,Int}
end

struct ProgressNoteResult
    accepted::Bool
    retry_after_ms::Union{Nothing,Int}
end

struct ChildResult
    rlm_child_id::String
    session_name::Union{Nothing,String}
    session_dir::Union{Nothing,String}
    status::String
    settled::Bool
    answer_preview::Union{Nothing,String}
    error::Union{Nothing,String}
    duration_ms::Union{Nothing,Int}
    tool_use_count::Union{Nothing,Int}
    replied_since_task::Union{Nothing,Bool}
end

nonempty(v) = v isa AbstractString && !isempty(v)
isint(v) = v isa Integer && !(v isa Bool)

function opt(p, field, op, ok)
    v = get(p, field, nothing)
    v === nothing && return nothing
    ok(v) || error("$op entry has invalid $field")
    return v
end
optstr(p, f, op) = (v = opt(p, f, op, x -> x isa AbstractString); v === nothing ? nothing : String(v))
optint(p, f, op) = (v = opt(p, f, op, isint); v === nothing ? nothing : Int(v))
optbool(p, f, op) = opt(p, f, op, x -> x isa Bool)

function spawn_handle(p)
    p isa AbstractDict && all(nonempty(get(p, k, nothing)) for k in ("rlm_child_id", "name", "session_dir", "model")) ||
        error("rlm.spawn returned an invalid spawn handle")
    SpawnHandle(p["rlm_child_id"], p["name"], p["session_dir"], p["model"])
end

function model_entry(p)
    p isa AbstractDict && all(nonempty(get(p, k, nothing)) for k in ("provider", "id", "name", "selector")) ||
        error("rlm.find_models returned an invalid model entry")
    Model(p["provider"], p["id"], p["name"], p["selector"])
end

function activity(p, op)
    v = get(p, "activity", nothing)
    v === nothing && return nothing
    v isa AbstractDict || error("$op entry has invalid activity")
    kind = get(v, "kind", nothing)
    kind in ("waiting", "writing", "executing") || error("$op entry has invalid activity kind")
    tool = get(v, "tool_name", nothing)
    tool === nothing || tool isa AbstractString || error("$op entry has invalid activity tool_name")
    SubagentActivity(kind, tool === nothing ? nothing : String(tool))
end

function subagent(p, op="rlm.list_subagents")
    p isa AbstractDict || error("$op returned an invalid subagent entry")
    nonempty(get(p, "rlm_child_id", nothing)) || error("$op entry is missing rlm_child_id")
    for k in ("active_session_id", "session_id")
        v = get(p, k, nothing)
        v === nothing || v isa AbstractString || error("$op entry has invalid $k")
    end
    nonempty(get(p, "session_name", nothing)) || error("$op entry is missing session_name")
    nonempty(get(p, "session_dir", nothing)) || error("$op entry is missing session_dir")
    get(p, "status", nothing) in ("running", "completed", "error") || error("$op entry has invalid status")
    Subagent(p["rlm_child_id"], get(p, "active_session_id", nothing), get(p, "session_id", nothing),
             p["session_name"], p["session_dir"], p["status"], activity(p, op),
             optint(p, "tool_use_count", op), optint(p, "duration_ms", op), optstr(p, "answer_preview", op),
             optbool(p, "replied_since_task", op), optstr(p, "progress_note", op), optstr(p, "label", op),
             optint(p, "last_activity_at", op), optint(p, "activity_stale_ms", op))
end

function child_result(p)
    op = "rlm.collect"
    p isa AbstractDict || error("rlm.collect returned an invalid result entry")
    nonempty(get(p, "rlm_child_id", nothing)) || error("rlm.collect entry is missing rlm_child_id")
    get(p, "status", nothing) in ("queued", "running", "done", "error", "cancelled") || error("rlm.collect entry has invalid status")
    get(p, "settled", nothing) isa Bool || error("rlm.collect entry has invalid settled flag")
    replied = get(p, "replied_since_task", nothing)
    replied === nothing || replied isa Bool || error("rlm.collect entry has invalid replied_since_task")
    dir = optstr(p, "session_dir", op)
    ChildResult(p["rlm_child_id"], optstr(p, "session_name", op), (dir === nothing || isempty(dir)) ? nothing : dir,
                p["status"], p["settled"], optstr(p, "answer_preview", op), optstr(p, "error", op),
                optint(p, "duration_ms", op), optint(p, "tool_use_count", op), replied)
end

"""
    Palette.rlm.spawn(prompt; name, model = nothing, thinking = nothing) -> SpawnHandle

Spawn a recursive agent child and return once its task is admitted; it runs
on in the host while this code continues. `name` is required and unique among
siblings; `model` is an exact `provider/model` selector; `thinking` sets the
child's reasoning level (default: the parent's). Collect its result with
`Palette.rlm.collect`, in this call or a later one.
"""
function spawn(prompt::AbstractString; name::AbstractString, model=nothing, thinking=nothing)
    kwargs = Dict{String,Any}("name" => name)
    model === nothing || (kwargs["model"] = model)
    thinking === nothing || (kwargs["thinking"] = thinking)
    # The wire type stays "rlm.run", as in the Python kernel.
    spawn_handle(host_request("rlm.run", Dict{String,Any}("prompt" => prompt, "kwargs" => kwargs)))
end

"""
    Palette.rlm.create_session(prompt; name, model, thinking, cwd) -> CreateSessionHandle

Create and prompt a resident depth-0 daemon session (daemon-backed sessions only).
"""
function create_session(prompt::AbstractString; name=nothing, model=nothing, thinking=nothing, cwd=nothing)
    kwargs = Dict{String,Any}()
    for (k, v) in (("name", name), ("model", model), ("thinking", thinking), ("cwd", cwd))
        v === nothing || (kwargs[k] = v)
    end
    p = host_request("rlm.create_session", Dict{String,Any}("prompt" => prompt, "kwargs" => kwargs))
    p isa AbstractDict && all(nonempty(get(p, k, nothing)) for k in ("active_session_id", "session_id", "name", "session_file", "model")) ||
        error("rlm.create_session returned an invalid payload structure")
    CreateSessionHandle(p["active_session_id"], p["session_id"], p["name"], p["session_file"], p["model"])
end

"""
    Palette.rlm.find_models(query = ""; limit = 8) -> Vector{Model}

Search a bounded list of models backed by active user credentials.
"""
function find_models(query::AbstractString=""; limit::Integer=8)
    p = host_request("rlm.find_models", Dict{String,Any}("query" => query, "limit" => limit))
    models = get(p, "models", nothing)
    models isa AbstractVector || error("rlm.find_models returned an invalid models list")
    Model[model_entry(m) for m in models]
end

"""
    Palette.rlm.list_subagents() -> Vector{Subagent}

List the direct children this session retains.
"""
function list_subagents()
    entries = get(host_request("rlm.list_subagents"), "subagents", nothing)
    entries isa AbstractVector || error("rlm.list_subagents returned an invalid subagents registry")
    Subagent[subagent(e) for e in entries]
end

selector(t::Union{SpawnHandle,Subagent}) = t.rlm_child_id
function selector(t::AbstractString)
    s = strip(t)
    isempty(s) && throw(ArgumentError("collect target must be SpawnHandle, Subagent, or non-empty string, got String"))
    String(s)
end
selector(t) = throw(ArgumentError("collect target must be SpawnHandle, Subagent, or non-empty string, got $(typeof(t))"))

"""
    Palette.rlm.collect(targets = nothing; timeout_ms = 0) -> Vector{ChildResult}

Results of direct children: a spawn handle, a subagent row, a name or id, or a
vector of them; `nothing` selects every child. `timeout_ms = 0` returns a
snapshot at once; a positive value waits up to that long (at most 50 000) for
the selected children to settle, and a timeout returns snapshots, never an
error. A finished child keeps its result, so a later `collect` re-reads it.
For children that run longer than a call, spawn now and collect in a later
call, or wait in a background task (`job = @async Palette.rlm.collect(h; timeout_ms = 50_000)`).
"""
function collect(targets=nothing; timeout_ms::Integer=0)
    (isint(timeout_ms) && timeout_ms >= 0) || throw(ArgumentError("timeout_ms must be a non-negative int"))
    timeout_ms <= COLLECT_MAX_TIMEOUT_MS || throw(ArgumentError(
        "timeout_ms must be at most $COLLECT_MAX_TIMEOUT_MS: a host request has to finish within one call; " *
        "collect again in a later call, or wait in a background task"))
    sels = targets === nothing ? String[] :
           targets isa Union{SpawnHandle,Subagent,AbstractString} ? [selector(targets)] :
           targets isa Union{AbstractVector,Tuple} ? String[selector(t) for t in targets] :
           throw(ArgumentError("targets must be nothing, a target, or a vector of targets, got $(typeof(targets))"))
    results = get(host_request("rlm.collect", Dict{String,Any}("targets" => sels, "timeout_ms" => timeout_ms)), "results", nothing)
    results isa AbstractVector || error("rlm.collect returned an invalid results list")
    ChildResult[child_result(r) for r in results]
end

"""
    Palette.rlm.progress_note(message) -> ProgressNoteResult

Report brief progress to the parent orchestrator (at most 512 UTF-16 code
units, about one per 10 seconds). A throttled note returns `accepted = false`
with a `retry_after_ms` hint instead of throwing.
"""
function progress_note(message::AbstractString)
    s = strip(message)
    isempty(s) && throw(ArgumentError("message must not be empty"))
    # The host measures length in UTF-16 code units.
    length(transcode(UInt16, String(s))) > PROGRESS_NOTE_MAX_LENGTH &&
        throw(ArgumentError("message must be at most $PROGRESS_NOTE_MAX_LENGTH characters"))
    p = host_request("rlm.progress.note", Dict{String,Any}("message" => String(s)))
    accepted = get(p, "accepted", nothing)
    accepted isa Bool || error("rlm.progress.note returned an invalid accepted flag")
    retry = get(p, "retry_after_ms", nothing)
    retry === nothing || isint(retry) || error("rlm.progress.note returned an invalid retry_after_ms")
    ProgressNoteResult(accepted, retry === nothing ? nothing : Int(retry))
end

"""
    Palette.rlm.delete_subagent(target) -> Subagent

Delete one running or retained direct child: its spawn handle, its subagent
row, or its id or session name.
"""
function delete_subagent(target)
    sel = if target isa Union{SpawnHandle,Subagent}
        target.rlm_child_id
    elseif target isa AbstractString
        s = strip(target)
        isempty(s) && throw(ArgumentError("target must not be empty"))
        String(s)
    else
        throw(ArgumentError("target must be SpawnHandle, Subagent, or string, got $(typeof(target))"))
    end
    subagent(get(host_request("rlm.delete_subagent", Dict{String,Any}("target" => sel)), "subagent", nothing), "rlm.delete_subagent")
end

end # module rlm
