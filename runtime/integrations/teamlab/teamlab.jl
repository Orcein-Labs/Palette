# TeamLab client: a kernel's view of the shelf of Julia tools and the mailbox
# that one user's agents share. The shelf itself is kept by the session host
# (the `teamlab` broker category) in a directory no kernel can write, under the
# name the host was given for the calling agent; this file only asks for it.
#
# Two actions do work here as well. `publish` checks a tool loads (and runs
# its test, when given one) before sending it, and `sync` loads each pinned
# tool into this world, bound as `TeamLab.<name>`.

using SHA

const _SYNC_ARGS = ("action",)
const _PUBLISH_ARGS = ("action", "name", "source", "path", "test", "test_source", "test_timeout_s", "note")
# Passed to the host as they are; the host checks them.
const _HOST_ACTIONS = ("status", "approve", "pin", "unpin", "update", "fetch", "send", "inbox", "ack")
# Kept in `TeamLab`: the hash each bound tool was loaded from. A tool name
# cannot start with `#`, so this cannot collide with one.
const _LOADED = Symbol("#teamlab_loaded")

function _host(params::Dict{String,Any})
    resp = Palette.request_capability("teamlab", params)
    get(resp, "approved", false) === true ||
        error("teamlab $(get(params, "action", "")): $(get(resp, "reason", "the host gave no reason"))")
    return resp["result"]
end

function _check_keys(args, allowed)
    extra = sort!([k for k in keys(args) if !(k in allowed)])
    isempty(extra) || throw(ArgumentError("teamlab $(args["action"]) does not take $(join(extra, ", ")); it takes $(join(allowed[2:end], ", "))"))
end

_text(args, key) = (v = get(args, key, nothing); v === nothing || v isa AbstractString ? v :
                    throw(ArgumentError("teamlab '$key' must be a string")))

# Loads `source` into a fresh module and returns the
# tool: the module's binding of that name.
function _load(name::AbstractString, source::AbstractString, label::AbstractString)
    mod = Module(Symbol("TeamLab_", name))  # not `name`: a module binds its own name
    world = Palette.get_kernel_state().eval_module
    Core.eval(mod, :(const Palette = $(Base.invokelatest(getglobal, world, :Palette))))
    Base.include_string(mod, source, label)
    sym = Symbol(name)
    Base.invokelatest(isdefined, mod, sym) ||
        throw(ArgumentError("a TeamLab tool named $name must define `$name`; the source defines no such binding"))
    return Base.invokelatest(getglobal, mod, sym)
end

# Runs the tool's test in a fresh Julia process, beside a copy of the tool
# saved as `<name>.jl`, so the test can `include("<name>.jl")`.
function _run_test(name::AbstractString, source::AbstractString, test::AbstractString, timeout_s::Real)
    mktempdir() do dir
        write(joinpath(dir, "$name.jl"), source)
        script = joinpath(dir, "test_$name.jl")
        write(script, test)
        project = Base.active_project()
        flags = project === nothing ? String[] : ["--project=$project"]
        cmd = `$(Base.julia_cmd()) --startup-file=no --history-file=no $flags $script`
        log = joinpath(dir, "output.txt")
        timed_out = Ref(false)
        proc = open(log, "w") do out
            p = run(pipeline(Cmd(cmd; dir=dir, ignorestatus=true); stdout=out, stderr=out); wait=false)
            timer = Timer(timeout_s) do _
                process_running(p) && (timed_out[] = true; kill(p))
            end
            try
                wait(p)
            finally
                close(timer)
            end
            p
        end
        text = last(read(log, String), 4000)
        timed_out[] && (text = "the test did not finish within $(timeout_s) s\n" * text)
        return success(proc) && !timed_out[], text
    end
end

function _publish(args)
    _check_keys(args, _PUBLISH_ARGS)
    name = _text(args, "name")
    name === nothing && throw(ArgumentError("teamlab publish requires the tool's 'name'"))
    source, path = _text(args, "source"), _text(args, "path")
    (source === nothing) == (path === nothing) &&
        throw(ArgumentError("teamlab publish takes the tool as 'source' (its code) or 'path' (a file this kernel can read), one of them"))
    source = something(source, path === nothing ? nothing : read(path, String))
    test, test_source = _text(args, "test"), _text(args, "test_source")
    test !== nothing && test_source !== nothing &&
        throw(ArgumentError("teamlab publish takes 'test' (a file) or 'test_source' (its code), not both"))
    test_source = something(test_source, test === nothing ? nothing : read(test, String), Some(nothing))
    timeout_s = get(args, "test_timeout_s", 240)
    timeout_s isa Real && 0 < timeout_s <= 300 || throw(ArgumentError("teamlab 'test_timeout_s' must be a number of seconds, at most 300"))

    # A tool that does not load in its author's world would not load in anyone's.
    _load(name, source, "teamlab-check://$name.jl")
    params = Dict{String,Any}("action" => "publish", "name" => name, "source" => source)
    haskey(args, "note") && (params["note"] = args["note"])
    if test_source !== nothing
        passed, summary = _run_test(name, source, test_source, timeout_s)
        passed || return Dict{String,Any}("published" => false, "name" => name,
                                         "reason" => "its test failed, so it was not published", "test_output" => summary)
        params["test"] = Dict("passed" => true, "summary" => summary)
    end
    result = _host(params)
    result["published"] = true
    return result
end

function _shelf()
    world = Palette.get_kernel_state().eval_module
    if Base.invokelatest(isdefined, world, :TeamLab)
        shelf = Base.invokelatest(getglobal, world, :TeamLab)
        shelf isa Module && Base.invokelatest(isdefined, shelf, _LOADED) && return shelf
        error("`TeamLab` in this world is bound to something else; TeamLab binds its tools there")
    end
    shelf = Module(:TeamLab)
    Core.eval(shelf, :(const $_LOADED = Dict{String,String}()))
    Core.eval(world, :(const TeamLab = $shelf))
    return shelf
end

function _sync(args)
    _check_keys(args, _SYNC_ARGS)
    reply = _host(Dict{String,Any}("action" => "sync"))
    shelf = _shelf()
    loaded = Base.invokelatest(getglobal, shelf, _LOADED)
    done, current, failed = Dict{String,Any}[], String[], Dict{String,Any}[]
    for t in reply["tools"]
        name, sha, source = t["name"], t["sha256"], t["source"]
        if bytes2hex(SHA.sha256(source)) != sha
            push!(failed, Dict("name" => name, "error" => "the host sent source that does not match its hash"))
            continue
        end
        if get(loaded, name, nothing) == sha
            push!(current, name)
            continue
        end
        try
            tool = _load(name, source, "teamlab://$name/$(first(sha, 12)).jl")
            Core.eval(shelf, :(global $(Symbol(name)) = $tool))
            loaded[name] = sha
            push!(done, Dict{String,Any}("name" => name, "version" => t["version"], "author" => t["author"], "sha256" => sha))
        catch e
            push!(failed, Dict("name" => name, "error" => sprint(showerror, e)))
        end
    end
    return Dict{String,Any}(
        "agent" => reply["agent"], "loaded" => done, "current" => current, "failed" => failed,
        "missing" => reply["missing"], "behind" => reply["behind"], "unread" => reply["unread"],
        "use" => "call a tool as TeamLab.<name>(...) from your next call; in this one, Base.invokelatest(TeamLab.<name>, ...)",
    )
end

"""
    teamlab(arguments::Dict{String,Any}) -> Dict{String,Any}

The `teamlab` capability. `arguments["action"]` is one of:

- `status` (the default): the shelf's tools, which version this agent has
  pinned and whether it is behind, unread mail.
- `publish`: `name` and `source` or `path`; optional `test` (a file) or
  `test_source`, run first in a fresh Julia beside the tool saved as
  `<name>.jl`; optional `note`. Pins the new version for its author.
- `approve`, `pin`: `name`, optional `sha256` (default: the newest version).
- `unpin`: `name`. `update`: optional `names` (default: every pinned tool),
  moved to the newest version.
- `sync`: loads every pinned tool into this world as `TeamLab.<name>`.
- `fetch`: `name`, optional `sha256`: one version with its source.
- `send`: `to` (an agent or "all"), `subject`, optional `body`, `tool`, `sha256`.
- `inbox`: optional `all` (include read mail). `ack`: `ids`, a list or "all".
"""
function teamlab(arguments::Dict{String,Any})
    action = get(arguments, "action", "status")
    action isa AbstractString || throw(ArgumentError("teamlab 'action' must be a string"))
    action == "publish" && return _publish(arguments)
    action == "sync" && return _sync(arguments)
    action in _HOST_ACTIONS || throw(ArgumentError("teamlab 'action' must be one of publish, sync, $(join(_HOST_ACTIONS, ", "))"))
    return _host(Dict{String,Any}(arguments..., "action" => action))
end
