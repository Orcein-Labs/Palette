# TeamLab client: a kernel's view of the shop of Julia tools and the mailbox
# that one user's agents share. The shop itself is kept by the session host
# (the `teamlab` broker category) in a directory no kernel can write, under the
# name the host was given for the calling agent; this file only asks for it.
#
# Three actions do work here as well. `publish` checks a tool loads (and runs
# its test, when given one) before listing it, `try` loads one listing into
# this world without pinning it, and `sync` loads every pinned listing, each
# bound as `TeamLab.<binding>`.
#
# A listing's author keeps merge authority over it: a version another agent
# publishes to it is a proposal, which the author accepts or declines.

using SHA

const _SYNC_ARGS = ("action",)
const _TRY_ARGS = ("action", "id", "sha256")
const _PUBLISH_ARGS = ("action", "id", "kind", "binding", "tag", "source", "path", "test", "test_source",
                       "test_timeout_s", "note")
# Passed to the host as they are; the host checks them.
const _HOST_ACTIONS = ("status", "browse", "approve", "pin", "unpin", "update", "fetch", "send", "inbox", "ack",
                       "accept", "decline")
# Kept in `TeamLab`: the hash each binding was loaded from. A binding cannot
# start with `#`, so this cannot collide with one.
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

_kind_of(value) = value isa Module ? "module" : value isa Function ? "function" : nothing

# The names a tool's module defines that could be the tool: its own functions
# and submodules, not `_helpers`, imports or the module's own plumbing.
function _candidates(mod::Module)
    skip = (:eval, :include, :Palette, nameof(mod))
    found = Pair{String,Any}[]
    for sym in Base.invokelatest(names, mod; all=true)
        name = String(sym)
        (sym in skip || startswith(name, '_') || startswith(name, '#')) && continue
        Base.invokelatest(isdefined, mod, sym) || continue
        value = Base.invokelatest(getglobal, mod, sym)
        _kind_of(value) !== nothing && value !== mod && parentmodule(value) === mod && push!(found, name => value)
    end
    return found
end

# Loads `source` into a fresh module and returns the tool's binding and value:
# `binding` when given, else the one function (or module, for kind "module")
# the source defines.
function _load(kind, binding, source::AbstractString, label::AbstractString)
    mod = Module(Symbol("#teamlab_tool"))  # a name no source can define
    world = Palette.get_kernel_state().eval_module
    Core.eval(mod, :(const Palette = $(Base.invokelatest(getglobal, world, :Palette))))
    Base.include_string(mod, source, label)
    if binding === nothing
        found = [c for c in _candidates(mod) if kind === nothing || _kind_of(c.second) == kind]
        what = kind === nothing ? "function or module" : kind
        isempty(found) && throw(ArgumentError("the source defines no $what to list; name it with 'binding'"))
        length(found) > 1 && throw(ArgumentError(
            "the source defines several: $(join(first.(found), ", ")). Pass 'binding' to say which is the tool, or start the helpers' names with _"))
        binding = only(found).first
    end
    sym = Symbol(binding)
    Base.invokelatest(isdefined, mod, sym) || throw(ArgumentError("the source defines no `$binding`"))
    value = Base.invokelatest(getglobal, mod, sym)
    kind === nothing || _kind_of(value) == kind ||
        throw(ArgumentError("`$binding` is a $(something(_kind_of(value), typeof(value))), not a $kind"))
    return String(binding), value
end

# Runs the tool's test in a fresh Julia process, beside a copy of the tool
# saved as `<binding>.jl`, so the test can `include("<binding>.jl")`.
function _run_test(binding::AbstractString, source::AbstractString, test::AbstractString, timeout_s::Real)
    mktempdir() do dir
        write(joinpath(dir, "$binding.jl"), source)
        script = joinpath(dir, "test_$binding.jl")
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
    id, kind, binding = _text(args, "id"), _text(args, "kind"), _text(args, "binding")
    if id !== nothing
        listing = _host(Dict{String,Any}("action" => "fetch", "id" => id))
        kind, binding = something(kind, listing["kind"]), something(binding, listing["binding"])
    end
    kind === nothing || kind in ("function", "module") || throw(ArgumentError("teamlab 'kind' must be function or module"))
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
    binding, value = _load(kind, binding, source, "teamlab-check://$(something(binding, "tool")).jl")
    params = Dict{String,Any}("action" => "publish", "kind" => _kind_of(value), "binding" => binding, "source" => source)
    for key in ("id", "tag", "note")
        haskey(args, key) && (params[key] = args[key])
    end
    if test_source !== nothing
        passed, summary = _run_test(binding, source, test_source, timeout_s)
        passed || return Dict{String,Any}("published" => false, "binding" => binding,
                                         "reason" => "its test failed, so it was not published", "test_output" => summary)
        params["test"] = Dict("passed" => true, "summary" => summary)
    end
    result = _host(params)
    result["published"] = true
    get(result, "proposed", false) === true && (result["next"] =
        "$(result["author"]) listed this tool, so your version is a proposal: it is not pinned or loaded by sync until $(result["author"]) accepts it. Withdraw it with action \"decline\", \"id\" and \"sha256\"")
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

# Loads one version the host sent into `TeamLab`, checking its hash first.
# Returns the hash the binding held before, or nothing.
function _bind!(shelf::Module, t)
    binding, sha, source = t["binding"], t["sha256"], t["source"]
    bytes2hex(SHA.sha256(source)) == sha || error("the host sent source that does not match its hash")
    _, tool = _load(t["kind"], binding, source, "teamlab://$(t["id"])/$(first(sha, 12)).jl")
    loaded = Base.invokelatest(getglobal, shelf, _LOADED)
    before = get(loaded, binding, nothing)
    Core.eval(shelf, :(global $(Symbol(binding)) = $tool))
    loaded[binding] = sha
    return before
end

_described(t) = Dict{String,Any}(k => t[k] for k in ("id", "kind", "binding", "tag", "version", "author", "sha256"))

const _USE = "call it as TeamLab.<binding>(...) from your next call; in this one, Base.invokelatest(TeamLab.<binding>, ...)"

function _sync(args)
    _check_keys(args, _SYNC_ARGS)
    reply = _host(Dict{String,Any}("action" => "sync"))
    shelf = _shelf()
    loaded = Base.invokelatest(getglobal, shelf, _LOADED)
    done, current, failed = Dict{String,Any}[], String[], Dict{String,Any}[]
    for t in reply["tools"]
        if get(loaded, t["binding"], nothing) == t["sha256"]
            push!(current, t["binding"])
            continue
        end
        try
            _bind!(shelf, t)
            push!(done, _described(t))
        catch e
            push!(failed, Dict("id" => t["id"], "binding" => t["binding"], "error" => sprint(showerror, e)))
        end
    end
    return Dict{String,Any}(
        "agent" => reply["agent"], "loaded" => done, "current" => current, "failed" => failed,
        "missing" => reply["missing"], "damaged" => get(reply, "damaged", Any[]),
        "behind" => reply["behind"], "clashes" => reply["clashes"],
        "unread" => reply["unread"], "use" => _USE,
    )
end

# Loads one listing to try it, without pinning it.
function _try(args)
    _check_keys(args, _TRY_ARGS)
    params = Dict{String,Any}("action" => "fetch")
    for key in ("id", "sha256")
        haskey(args, key) && (params[key] = args[key])
    end
    t = _host(params)
    before = _bind!(_shelf(), t)
    keep = t["pending"] === true ?
        "this is a proposal: the listing's author accepts it (action \"accept\") or declines it (action \"decline\"), and only then can it be pinned" :
        "pin it (action \"pin\", \"id\") to keep it: sync loads only what you pin, and a pin on another listing with this binding replaces it"
    return merge(_described(t), Dict{String,Any}(
        "pending" => t["pending"], "replaced" => before, "pinned" => false, "use" => _USE, "keep" => keep,
    ))
end

"""
    teamlab(arguments::Dict{String,Any}) -> Dict{String,Any}

The `teamlab` capability: a shop of Julia tools this user's agents share. A
listing has an `id`, a `kind` ("function" or "module"), a `tag` (what it
does, in 15 words or fewer) and the `binding` its source defines.
`arguments["action"]` is one of:

- `status` (the default): your agent name (`agent`), what you pin (and whether
  a newer version is out), listings new from other agents this week, proposed
  versions of your listings awaiting your review, unread mail.
- `browse`: every listing, newest first; optional `query` keeps those whose
  kind, binding, tag, author or id hold every word.
- `try`: `id`, optional `sha256` (a version's or a proposal's): loads that
  listing as `TeamLab.<binding>` without pinning it.
- `publish`: `source` or `path`, and a `tag` for a new listing. The `binding`
  is the one function or module the source defines unless named (start
  helpers' names with `_`), and `kind` defaults to what the binding is.
  `id` adds a version to that listing; without it, a version goes to your own
  listing of the same binding. Optional `test` (a file) or `test_source`, run
  first in a fresh Julia beside the tool saved as `<binding>.jl`; optional
  `note`. Pins the new version for you. On another agent's listing the version
  is a proposal instead: its author is mailed, and it waits for them.
- `approve`, `pin`: `id`, optional `sha256` (default: the newest version).
- `unpin`: `id`. `update`: optional `ids` (default: every pinned listing),
  moved to the newest version.
- `sync`: loads every pinned listing into this world as `TeamLab.<binding>`.
- `accept`: `id`, `sha256`, optional `note`: the listing's author makes a
  proposal its newest version. `decline`: `id`, `sha256`, optional `note`: the
  author declines a proposal, or its proposer withdraws it.
- `fetch`: `id`, optional `sha256`: one version (or proposal) with its source;
  `pending` is true for a proposal.
- `send`: `to` (an agent or "all"), `subject`, optional `body`, `listing`, `sha256`.
- `inbox`: optional `all` (include read mail). `ack`: `ids`, a list or "all".

Your name in the shop is `agent` in `status`. Check it before you publish or
update a tool: if it is `local` or another agent's name, do not publish, and
tell the user, because your work would be credited to the wrong agent.
"""
function teamlab(arguments::Dict{String,Any})
    action = get(arguments, "action", "status")
    action isa AbstractString || throw(ArgumentError("teamlab 'action' must be a string"))
    action == "publish" && return _publish(arguments)
    action == "sync" && return _sync(arguments)
    action == "try" && return _try(arguments)
    action in _HOST_ACTIONS || throw(ArgumentError("teamlab 'action' must be one of publish, try, sync, $(join(_HOST_ACTIONS, ", "))"))
    return _host(Dict{String,Any}(arguments..., "action" => action))
end
