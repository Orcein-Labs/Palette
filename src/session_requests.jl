# The request handling of runtime/scripts/session_loop.jl, which owns the
# process set-up: the protocol descriptors, the output sink and the warm-up.
# It lives in the package so the package image holds its native code. As a
# script loop it compiled in full on the first request, about 1.1 s that the
# first call of every kernel paid.

# `display(x)` goes through the display stack, whose TextDisplay was built at
# startup around the original stdout, which is the protocol pipe: one
# `display` in turn code wrote a matrix into the protocol and killed the
# session. This display writes to whatever stdout is when it is called, which
# during a turn is the turn's captured output.
struct TurnDisplay <: AbstractDisplay end
Base.display(::TurnDisplay, ::MIME"text/plain", @nospecialize(x)) = (show(stdout, MIME"text/plain"(), x); println(stdout))
Base.display(d::TurnDisplay, @nospecialize(x)) = display(d, MIME"text/plain"(), x)
Base.displayable(::TurnDisplay, ::MIME"text/plain") = true

# What the session's own project can load, named when it has no network.
# Set at session start: turn code may activate another project later.
const LOADABLE = Ref("")
loadable_packages() = let deps = get(Base.parsed_toml(Base.active_project()), "deps", Dict())
    join(["the Julia standard library"; sort!([String(k) for k in keys(deps) if k != "Palette"])], ", ")
end
pkg_offline() = get(INITIAL_ENV[], "JULIA_PKG_OFFLINE", "") == "true"

# A reserved word where a name belongs: an argument (`quote::UInt8`), an
# assignment (`end = 3`), a keyword argument or a loop variable. Julia's
# parser reports these as "Expected `end`" several columns away.
const RESERVED_AS_NAME = r"(?:^|[(,;\s])(quote|end|begin|let|local|global|module|baremodule|struct|macro|do|try|catch|finally|export|import|using|const|return|break|continue|function|if|elseif|else|while|for|abstract|primitive|mutable|public)\s*(?:::|=(?!=)|,\s*\w|\s+in\s)"
# Source with a docstring, embedded in a triple-quoted string: the docstring's
# own """ line ends the string, and the lines after it run as code. In the
# endurance runs this failed as `invalid keyword argument name "last::Bool"`
# or an undefined name, far from the cause. Returns the line the string began
# on and the line that ended it, when the error is in the lines that leaked.
function docstring_closed_string(err::AbstractString, code::AbstractString)
    m = match(r"around call \d+:(\d+)|at this call, line (\d+)|# Error @ this call, line (\d+)", err)
    m === nothing && return nothing
    errline = parse(Int, something(m.captures...))
    linenum(i) = count(==('\n'), SubString(code, 1, prevind(code, i))) + 1
    for open in eachmatch(r"(?:=|\(|\*|=>|,)\s*\"\"\"", code)
        start = open.offset + ncodeunits(open.match)
        close = match(r"\n[ \t]*\"\"\"[ \t]*\n(?=[ \t]+\S)", code, start)
        close === nothing && continue
        after = close.offset + ncodeunits(close.match)
        next = findnext("\"\"\"", code, after)
        next === nothing && continue
        linenum(after) <= errline <= linenum(first(next)) && return (linenum(start), linenum(close.offset + 1))
    end
    return nothing
end

# An undefined name that the call's code interpolates into a string. In the
# endurance runs 13 of 18 UndefVarErrors were this: Julia source for a file,
# written as a string literal, whose `$K` and `$new` ran in the kernel.
function interpolated_undefined_name(err::AbstractString, code::AbstractString)
    m = match(r"UndefVarError: `([^`]+)`", err)
    m === nothing && return nothing
    name = Symbol(m[1])
    ex = try
        Meta.parseall(code)
    catch
        return nothing
    end
    found = Ref(false)
    walk(x) = x isa Expr && (x.head === :string && any(a -> a === name || (a isa Expr && name in collect_symbols(a)), x.args) ?
                             (found[] = true) : foreach(walk, x.args))
    walk(ex)
    return found[] ? string(name) : nothing
end
collect_symbols(x) = x isa Symbol ? Symbol[x] : x isa Expr ? reduce(vcat, map(collect_symbols, x.args); init=Symbol[]) : Symbol[]

# In trials a model met these errors and retried the same thing several
# times, a model request each; the hint names what works instead.
function with_hint(err, code)
    err isa AbstractString || return err
    missing = match(r"Package ([A-Za-z][A-Za-z0-9_]*) (?:not found in current path|\[[^\]]+\] is required but does not seem to be installed)", err)
    if missing !== nothing
        observed = package_environment(missing[1])
        message = "Package $(missing[1]): $(observed["status"]). $(observed["action"])"
        isempty(observed["local_paths"]) || (message *= " Local paths: " * join(observed["local_paths"], ", ") * ".")
        pkg_offline() && (message *= " This kernel has no network. Loadable: $(LOADABLE[]).")
        original = replace(err, r"(?m)^- (?:Otherwise, run|Run) `import Pkg; Pkg\.add\([^\n]+\n?" => "",
            r"(?m)^ - Run `Pkg\.instantiate\(\)`[^\n]+\n?" => "")
        return message * "\n" * original
    elseif (name = interpolated_undefined_name(err, code)) !== nothing
        return err * "\nHint: `$name` is interpolated into a string in this call (`\$$name`). If the string holds text for a file, " *
               "write `\\\$$name`, or pass the text as this call's payload, which is not parsed: write(path, PAYLOAD), or for an edit " *
               "payload {\"old\": ..., \"new\": ...} and replace(text, PAYLOAD[\"old\"] => PAYLOAD[\"new\"])."
    elseif (lines = docstring_closed_string(err, code)) !== nothing
        return err * "\nHint: the string that begins on line $(lines[1]) ends at the docstring's \"\"\" on line $(lines[2]), " *
               "so the following text escaped the string and was parsed as code. Pass the text as this call's payload, which is not parsed: write(path, PAYLOAD), " *
               "or for an edit payload {\"old\": ..., \"new\": ...} and replace(text, PAYLOAD[\"old\"] => PAYLOAD[\"new\"])."
    elseif occursin("must be quoted in commands", err)
        return err * "\nHint: backticks start one program without a shell. " *
               "Use sh\"...\" or bash(\"...\") for pipes, globs, redirects and &&."
    elseif startswith(err, "ParseError") && occursin(r"after \$ in string|interpolat", err)
        return err * "\nHint: in a Julia string \$ interpolates; write \\\$ for a literal dollar sign. sh\"...\" passes \$ to the shell " *
               "unchanged, and a file's text passed as this call's payload needs no escaping: write(path, PAYLOAD)."
    elseif startswith(err, "ParseError") && (m = match(RESERVED_AS_NAME, code)) !== nothing
        return err * "\nHint: `$(m[1])` is a reserved word in Julia and cannot name a variable or argument."
    elseif startswith(err, "ParseError") && occursin("\"\"\"", code)
        return err * "\nHint: to write a file's text without Julia quoting, pass it as this call's payload and use write(path, PAYLOAD)."
    elseif pkg_offline() && (
        occursin(r"not found in current path|has no known versions|Could not resolve host|name resolution", err) ||
        (occursin("Pkg", err) &&
            occursin(r"(?i)read-only file system|permission denied", err)))
        # First, not last: the Pkg error that follows runs to many lines.
        return "This kernel has no network, so Pkg.add cannot install packages. Loadable: $(LOADABLE[]).\n" * err
    end
    return err
end

"""
    serve_requests(respond, requests, request_waiting, epoch)

Answers each request line read from `requests` with one `respond(resp)`,
in order, until the input closes. After a call's reply it saves the state
the call ended with; `request_waiting()` says whether the next request has
arrived meanwhile, so it goes first.
"""
function serve_requests(@nospecialize(respond), requests::IO, @nospecialize(request_waiting), epoch::String)
    revival_pending = true
    for line in eachline(requests)
        isempty(strip(line)) && continue
        resp, snapshot_call = handle_request(respond, line, epoch)
        if revival_pending && resp["kind"] == "RESULT"
            resp["revival"] = revival_summary()
            revival_pending = false
        end
        respond(resp)
        # The model is reading the reply: save the state this call ended with.
        # A request already waiting goes first, until five calls have gone unsaved.
        if snapshot_call !== nothing
            try
                # Latest world: turn code defined methods (show, ==, enum names) since this loop began.
                # A request that arrives meanwhile goes first, while the last saved state is recent.
                Base.invokelatest(snapshot_state!, snapshot_call; waiting=request_waiting)
            catch e
                println(stderr, "[saving the state after call $snapshot_call failed: ", first(sprint(showerror, e), 300), "]")
            end
        end
    end
end

# One request line's reply, and the call whose state to save after it.
# `respond` is only for the reply of a kernel about to stop.
function handle_request(@nospecialize(respond), line::AbstractString, epoch::String)
    local req
    try
        req = JSON.parse(line; dicttype=Dict{String, Any})
    catch e
        return Dict{String,Any}("kind" => "ERROR", "epoch" => epoch, "error" => sprint(showerror, e)), nothing
    end

    request_id = get(req, "request_id", nothing)
    kind = get(req, "kind", nothing)
    code = get(req, "code", nothing)
    timeout_s = get(req, "timeout_s", nothing)
    resp = Dict{String,Any}("kind" => "RESULT", "epoch" => epoch, "request_id" => request_id)
    snapshot_call = nothing
    previous_reload_sequence = RELOAD_SEQUENCE[]

    try
        code isa String || error("request 'code' must be a string")
        timeout_s === nothing || timeout_s isa Real && timeout_s > 0 || error("request 'timeout_s' must be a positive number")
        payload = get(req, "payload", nothing)
        # Text, or named texts ({"old": ..., "new": ...}) for an edit that needs
        # more than one: PAYLOAD["old"].
        if payload isa AbstractDict
            all(v -> v isa String, values(payload)) || error("request 'payload' parts must be strings")
            payload = Dict{String, String}(String(k) => v for (k, v) in payload)
        end
        payload === nothing || payload isa Union{String, Dict{String, String}} || error("request 'payload' must be a string or an object of strings")
        if kind == "EPHEMERAL"
            ephemeral_reply!(resp, req, code, payload, timeout_s)
        elseif kind == "EXECUTE"
            snapshot_call = execute_reply!(respond, resp, req, code, payload, timeout_s)
        else
            # Fail closed on an unrecognized `kind` -- confirmed by direct
            # testing that an earlier version of this branch ran ANY
            # non-"EPHEMERAL" kind (a typo, a missing field, a future
            # protocol version this process doesn't understand) as durable
            # ExecuteCode against the persistent mind. A malformed or
            # unrecognized request must be rejected, not silently treated
            # as "the durable default."
            error("unrecognized request 'kind': $(repr(kind)) (expected \"EXECUTE\" or \"EPHEMERAL\")")
        end
    catch e
        resp["success"] = false
        resp["data"] = nothing
        resp["error"] = sprint(showerror, e)
    end

    resp["error"] = with_hint(get(resp, "error", nothing), code isa String ? code : "")
    if RELOAD_SEQUENCE[] > previous_reload_sequence
        resp["reloads"] = [Dict(k => r[k] for k in ("module", "status", "identity_risks", "surviving_deleted_definitions",
            "sequence", "source_digest_before", "source_digest_after", "recovery", "error")) for r in RELOAD_REPORTS if r["sequence"] > previous_reload_sequence]
    end
    return resp, snapshot_call
end

# An EPHEMERAL request runs in a disposable child process; see the
# session loop's header for why not in this one.
function ephemeral_reply!(resp, req, code::String, payload, timeout_s)
    request_id = get(req, "request_id", nothing)
    requested_ceiling = get(req, "ceiling", Dict{String,Any}())
    requested_ceiling isa AbstractDict || error("request 'ceiling' must be an object")
    # The child's own script: runs `code` through the SAME
    # ExecuteCode/execute path (real Core.eval, real receipt) as a
    # persistent turn would, but inside its own disposable process
    # -- then prints exactly one JSON line this process parses back.
    # `repr(code)` produces a safely re-escaped Julia string literal
    # (that's what `repr` is for); this is textual generation of
    # trusted host-side code embedding untrusted content as DATA,
    # not string-building a shell command.
    child_script = string("__palette_child_started__=time_ns(); using Palette; Palette.ephemeral_main(", repr(code),
        "; payload=", repr(payload), ", timeout_s=", repr(timeout_s),
        ", startup_seconds=(time_ns()-__palette_child_started__)/1e9)")
    # `child_workspace`/`child_project`/`child_repo` are no longer
    # part of this request: the broker now always allocates the
    # child's workspace itself and uses its own session-established
    # project/repo paths -- a request can no longer choose WHERE a
    # child's filesystem view points, only WHICH capabilities it
    # gets (see broker.py's _handle_spawn_child_worker docstring
    # for the exploit this closed).
    child_started = time_ns()
    spawn_resp = request_capability("spawn_child_worker", Dict(
        "ceiling" => requested_ceiling,
        "script" => child_script,
    ))
    child_roundtrip_seconds = (time_ns() - child_started) / 1e9
    if get(spawn_resp, "approved", false)
        child_stdout = String(get(spawn_resp["result"], "stdout", ""))
        lines = split(child_stdout, '\n')
        idx = findlast(ln -> !isempty(strip(ln)), lines)
        last_line = idx === nothing ? "" : lines[idx]
        child_result = try
            JSON.parse(last_line; dicttype=Dict{String, Any})
        catch
            # The child died before printing its result line.
            stderr_tail = strip(String(get(spawn_resp["result"], "stderr", "")))
            error("ephemeral child exited with code $(get(spawn_resp["result"], "returncode", "?")) and no result" *
                  (isempty(stderr_tail) ? "" : ":\n" * last(stderr_tail, 1500)))
        end
        resp["success"] = get(child_result, "success", false)
        resp["data"] = get(child_result, "data", nothing)
        resp["display"] = get(child_result, "display", nothing)
        resp["output"] = idx === nothing ? "" : rstrip(join(lines[1:idx-1], '\n'))
        resp["error"] = get(child_result, "error", nothing)
        resp["costs"] = get(child_result, "costs", Dict{String, Any}())
        resp["costs"]["child_broker_roundtrip_seconds"] = child_roundtrip_seconds
        get(child_result, "transport_shortened", false) && (resp["transport_shortened"] = true)
        record_tool_capsule(
            string(request_id), code,
            OperationResult(resp["data"], resp["success"], resp["error"]),
            child_roundtrip_seconds * 1000,
        )
    else
        resp["success"] = false
        resp["data"] = nothing
        resp["error"] = "spawn_child_worker denied: " * string(get(spawn_resp, "reason", "unknown"))
    end
end

# An EXECUTE request runs in this kernel's persistent eval module. Returns
# the call to save the state of, or nothing for an interrupted call.
function execute_reply!(@nospecialize(respond), resp, req, code::String, payload, timeout_s)
    mod = get_kernel_state().eval_module
    # A file's text arrives as a JSON string and is bound as it is,
    # so it needs no Julia quoting: `write("x.py", PAYLOAD)`.
    Core.eval(mod, Expr(:global, Expr(:(=), :PAYLOAD, payload)))
    call = length(get_kernel_state().execution_history) + 1
    note_started_call!(call, code)
    (receipt, interrupted, stuck), output = execute_turn(code, timeout_s === nothing ? nothing : Float64(timeout_s))
    printed = task_notice_hint(scrub(output))
    resp["call"] = call
    resp["bindings"] = binding_list!(get_kernel_state().eval_module, call)
    # Palette.output(call) gives back what the call printed, without what
    # the result adds in front of it below.
    retain_output!(call, printed)
    # The host can turn the digest off for an A/B of its effect ("digest": false).
    resp["output"] = get(req, "digest", true) === false ? strip_ansi(printed) : with_digest(printed)
    # The host asks for the workspace map at the session's first call and
    # after a compaction; it opens the result, ahead of the call's output.
    if get(req, "map", false) === true
        wmap = try workspace_map() catch e; "[workspace map unavailable: $(first(sprint(showerror, e), 160))]" end
        resp["output"] = wmap * "\n\n" * resp["output"]
    end
    if stuck
        resp["costs"] = deepcopy(get(TURN_COSTS, call, Dict{String, Any}()))
        resp["success"] = false
        resp["data"] = nothing
        resp["error"] = "During $(get(resp["costs"], "failed_phase", "execution")), the call exceeded its $(timeout_s)s limit and kept running after being interrupted " *
                        "for $(Int(INTERRUPT_GRACE_S))s, so the kernel stopped. " *
                        (isempty(STATE_DIR[]) ? "Every binding is gone; files written to the workspace remain." :
                         "The next call starts a new kernel, which revives what it can of the last saved state (its report names the call) and says what it could not.")
        resp["kernel_exit"] = true
        respond(resp)
        ccall(:_exit, Cvoid, (Cint,), 3)
    end
    if !(receipt isa OperationReceipt)
        receipt = OperationReceipt(UUIDs.uuid4(), Dates.now(), "ExecuteCode",
                                   OperationResult(nothing, false, sprint(showerror, receipt)),
                                   0.0, get_kernel_state().id)
    end
    resp["success"] = receipt.result.success && !interrupted
    projection = get(receipt.metadata, "transport", Dict{String, Any}())
    resp["data"] = resp["success"] ? get(projection, "data", nothing) : nothing
    resp["display"] = resp["success"] ? get(projection, "display", nothing) : nothing
    resp["costs"] = deepcopy(get(TURN_COSTS, call, Dict{String, Any}()))
    resp["error"] = scrub(something(receipt.result.error, ""))
    if interrupted
        resp["interrupted"] = true
        resp["error"] = "Interrupted during $(get(resp["costs"], "failed_phase", "execution")): the call exceeded its $(timeout_s)s limit. The kernel and every binding are intact; " *
                        "processes this call started were stopped. Output printed before the interrupt is above.\n" * resp["error"]
    end
    isempty(resp["error"]) && (resp["error"] = nothing)
    note_completed_call!(call)
    return interrupted ? nothing : call
end
