#!/usr/bin/env julia
#=
Palette persistent session loop -- runs inside the sandboxed worker, kept
alive across many turns instead of one `-e script` per launch.

Protocol: newline-delimited JSON on stdin/stdout, one request per line, one
response per line, in order. The first thing printed is a HELLO line
carrying this process's own `epoch` -- a fresh UUID generated here, inside
this process, not handed in by the orchestrator -- so a caller holding a
handle from a previous process (this one crashed and got relaunched) gets
a real, unambiguous mismatch instead of silently talking to a different
Julia world that happens to share a session_id. See
`runtime/security/session.py`'s `PaletteSession` for the orchestrator side.

Request:  {"request_id": "...", "kind": "EXECUTE" | "EPHEMERAL", "code": "...", "ceiling": {...}?,
           "payload": "..."?, "timeout_s": number?}
Response: {"request_id": "...", "epoch": "...", "success": bool, "data": ..., "display": ..., "output": ...,
           "error": ..., "interrupted": true?, "kernel_exit": true?}
HELLO:    {"kind": "HELLO", "epoch": "..."}

`payload` is bound as `PAYLOAD` for that one EXECUTE turn. At `timeout_s` a
turn still waiting is interrupted (`interrupted`) and the kernel keeps its
state; one that ignores the interrupt for INTERRUPT_GRACE_S ends the process
right after its reply (`kernel_exit`).

`kind == "EXECUTE"` evaluates into this session's own persistent
`eval_module` -- the durable "mind": real state, real bindings, real
compiled methods, kept for the life of this process.

`kind == "EPHEMERAL"` does NOT run in this process at all. An earlier
version of this file ran ephemeral code in a fresh, throwaway `Module`
in-process (see `Palette.EphemeralTool`/`check_ephemeral_source!`, which
still exist and are still real, tested, and honestly documented as a
namespace-HYGIENE measure). That was proven, by direct adversarial
testing, not sufficient as an authority boundary: `Core.eval(Base,
:(function show(io::IO, x::Int) ... end))` -- a value only assembled at
RUNTIME -- extends a foreign module's method table with zero syntactic
trace in the submitted source for any parse-time check to see. No static
checker can fully police a language with reflective `eval`, and
crippling `eval` itself would violate Palette's own "unbounded power
inside the sandbox" thesis. The fix is not a smarter checker; it's
putting the boundary underneath the language instead of inside it:
`EPHEMERAL` spawns a real, disposable, OS-sandboxed CHILD process via the
already-adversarially-proven `spawn_child_worker` capability (see
`runtime/security/broker.py` and `runtime/docs/authority.md`). If the child's
code corrupts its own `Base.show`, that corruption dies with the child
process -- Linux throws the whole thing away, which is a fundamentally
stronger guarantee than trying to detect or reverse the corruption
afterward. `Palette.request_capability("spawn_child_worker", ...)` already
enforces `C_child ⊆ C_caller`; an ephemeral turn's optional `"ceiling"`
field (default `{}`, i.e. full language power, zero broker-mediated
authority) is validated against THIS session's own ceiling exactly like
any other spawn_child_worker request.
=#
using Palette

# Qualified access to Palette's OWN already-declared dependencies, not a
# separate `using JSON`/`using UUIDs` here -- confirmed by direct testing
# that a script run under a project whose Project.toml lists only `Palette`
# (not JSON/UUIDs directly) cannot `using JSON` itself even though Palette
# depends on it internally: a package's dependencies aren't visible by
# name to code outside that package just because the package is loadable.
# `Palette.JSON`/`Palette.UUIDs` work because `using JSON` inside Palette.jl's
# own source creates that exact binding, and this script never needs a
# project of its own beyond whatever already makes `using Palette` work.
const JSON = Palette.JSON
const UUIDs = Palette.UUIDs
using Palette: safe_json, capture_output, run_turn, execute_turn, bounded_data, text_display, scrub, INTERRUPT_GRACE_S

const EPOCH = string(UUIDs.uuid4())
Palette.TASK_EPOCH[] = EPOCH

# The protocol gets its own duplicate of fd 1, and fd 1/fd 2 are then pointed
# at a sink file. Turn code prints, `run` children inherit fd 1, and tasks
# left running after a turn all write somewhere; none of it may reach the
# protocol channel, where one stray `println` used to be parsed as a
# response and kill the session.
const PROTO = let fd = ccall(:dup, Cint, (Cint,), 1)
    fd < 0 && error("dup(1) failed")
    fdio(fd, true)
end
# Requests arrive on a duplicate of fd 0 for the same reason, and fd 0 itself
# becomes /dev/null: a `readline()` or a `run(`cat`)` in turn code used to
# consume the next request and leave the host waiting for a reply until the
# turn timed out.
const PROTO_IN_FD = ccall(:dup, Cint, (Cint,), 0)
PROTO_IN_FD < 0 && error("dup(0) failed")
const PROTO_IN = open(Base.RawFD(PROTO_IN_FD))

# Whether the next request has arrived, without reading it. The stream does
# not read ahead between requests, so bytesavailable alone never saw one that
# arrived meanwhile; the descriptor is asked directly. A closed input
# (POLLHUP alone) is not a request: the loop ends after this snapshot, which
# is then the last chance to save the state.
function request_waiting()
    bytesavailable(PROTO_IN) > 0 && return true
    pfd = Ref((PROTO_IN_FD, Cshort(0x001), Cshort(0)))
    return ccall(:poll, Cint, (Ptr{Cvoid}, Culong, Cint), pfd, 1, 0) > 0 && (pfd[][3] & 0x001) != 0
end
redirect_stdin(open("/dev/null"))
const SINK = open(joinpath(tempdir(), "palette-background-output.log"), "a")
redirect_stdout(SINK)
redirect_stderr(SINK)
# Read back at the start of each call (Palette.report_background).
Palette.SINK[] = SINK
Palette.SINK_PATH[] = joinpath(tempdir(), "palette-background-output.log")
Palette.SINK_READ[] = filesize(Palette.SINK_PATH[])

# `display(x)` goes through the display stack, whose TextDisplay was built at
# startup around the original stdout, which is the protocol pipe: one
# `display` in turn code wrote a matrix into the protocol and killed the
# session. This display writes to whatever stdout is when it is called, which
# during a turn is the turn's captured output.
struct TurnDisplay <: AbstractDisplay end
Base.display(::TurnDisplay, ::MIME"text/plain", @nospecialize(x)) = (show(stdout, MIME"text/plain"(), x); println(stdout))
Base.display(d::TurnDisplay, @nospecialize(x)) = display(d, MIME"text/plain"(), x)
Base.displayable(::TurnDisplay, ::MIME"text/plain") = true
empty!(Base.Multimedia.displays)
pushdisplay(TurnDisplay())

# Without the REPL loaded, `@doc sum` returns a raw DocStr instead of the
# rendered docstring.
import REPL


function respond(resp::Dict{String,Any})
    started = time_ns()
    line = safe_json(resp)
    encoded = time_ns()
    println(PROTO, line)
    flush(PROTO)
    finished = time_ns()
    call = get(resp, "call", nothing)
    if call isa Int && haskey(Palette.TURN_COSTS, call)
        costs = Palette.TURN_COSTS[call]
        costs["serialization_seconds"] = (encoded - started) / 1e9
        costs["worker_pipe_seconds"] = (finished - encoded) / 1e9
        costs["worker_response_bytes"] = sizeof(line) + 1
    end
end


# In trials a model met these errors and retried the same thing several
# times, a model request each; the hint names what works instead.
const PKG_OFFLINE = get(ENV, "JULIA_PKG_OFFLINE", "") == "true"
const LOADABLE = let deps = get(Base.parsed_toml(Base.active_project()), "deps", Dict())
    join(["the Julia standard library"; sort!([String(k) for k in keys(deps) if k != "Palette"])], ", ")
end

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

function with_hint(err, code)
    err isa AbstractString || return err
    missing = match(r"Package ([A-Za-z][A-Za-z0-9_]*) (?:not found in current path|\[[^\]]+\] is required but does not seem to be installed)", err)
    if missing !== nothing
        observed = Palette.package_environment(missing[1])
        message = "Package $(missing[1]): $(observed["status"]). $(observed["action"])"
        isempty(observed["local_paths"]) || (message *= " Local paths: " * join(observed["local_paths"], ", ") * ".")
        PKG_OFFLINE && (message *= " This kernel has no network. Loadable: $LOADABLE.")
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
    elseif PKG_OFFLINE && (
        occursin(r"not found in current path|has no known versions|Could not resolve host|name resolution", err) ||
        (occursin("Pkg", err) &&
            occursin(r"(?i)read-only file system|permission denied", err)))
        # First, not last: the Pkg error that follows runs to many lines.
        return "This kernel has no network, so Pkg.add cannot install packages. Loadable: $LOADABLE.\n" * err
    end
    return err
end


Palette.WORKSPACE_ROOT[] = pwd()
Palette.workspace_package_first!(pwd())
# The host's state directory, where each completed call's state is saved for
# a kernel that replaces this one. A revived kernel takes the old eval
# module's name: values of kernel-defined types deserialize only into it.
Palette.STATE_DIR[] = get(ENV, "PALETTE_STATE_DIR", "")
Palette.INITIAL_ENV[] = Dict{String,String}(ENV)
let name = Palette.revival_module_name()
    name === nothing || (Palette.GLOBAL_STATE[] = Palette.KernelState(Module(Symbol(name))))
    mod = Palette.get_kernel_state().eval_module
    Core.eval(Main, Expr(:(=), nameof(mod), mod))
end
revival_pending = true

# Finish runtime compilation and revival before advertising readiness.
# Otherwise the host charges this preparation against the first turn's limit.
# Warm-up calls leave no user history, receipt, binding or phase observations.
let state = Palette.get_kernel_state()
    Core.eval(state.eval_module, Expr(:global, Expr(:(=), :PAYLOAD, nothing)))
    (r, _, _), out = execute_turn("print(\"\"); [1 2]", 60.0)
    # An undefined name is the error models make most, and its message runs
    # the REPL's hint handlers, which the package image cannot hold.
    (e, _, _), _ = execute_turn("warm_up_undefined_name", 60.0)
    with_hint(e.result.error, "")
    text_display(r.result.data); bounded_data(r.result.data); bounded_data(Dict("a" => [1]))
    scrub(out); with_hint("x", "y"); safe_json(Dict{String,Any}("a" => 1))
    empty!(state.execution_history)
    empty!(state.receipt_log)
    empty!(Palette.TURN_COSTS)
    empty!(Palette.DEFINITION_LOG); empty!(Palette.CALL_FILES); empty!(Palette.USED_FILES); empty!(Palette.BINDING_SEEN)
    empty!(Palette.BINDING_ORIGINS); empty!(Palette.BINDING_IDENTITIES)
    Core.eval(state.eval_module, Expr(:global, Expr(:(=), :ans, nothing)))
    Palette.REVIVAL_REPORT[] = try
        Base.invokelatest(Palette.revive_state!)
    catch e
        Palette.revival_observation!(; uncertain=["revival failed after possible partial reconstruction"],
            text="[revival] The previous kernel stopped, and reviving its state failed ($(first(sprint(showerror, e), 200))). " *
                 "Partial reconstructed state may remain; inspect bindings. Files in the workspace remain.")
    end
end

respond(Dict{String,Any}("kind" => "HELLO", "epoch" => EPOCH, "schema_revision" => 3,
    "capabilities" => Dict("revival_observation_v1" => true, "reload_observation_v1" => true, "phase_costs_v1" => true)))

for line in eachline(PROTO_IN)
    isempty(strip(line)) && continue

    local req
    try
        req = JSON.parse(line; dicttype=Dict{String, Any})
    catch e
        respond(Dict{String,Any}("kind" => "ERROR", "epoch" => EPOCH, "error" => sprint(showerror, e)))
        continue
    end

    request_id = get(req, "request_id", nothing)
    kind = get(req, "kind", nothing)
    code = get(req, "code", nothing)
    timeout_s = get(req, "timeout_s", nothing)
    resp = Dict{String,Any}("kind" => "RESULT", "epoch" => EPOCH, "request_id" => request_id)
    snapshot_call = nothing
    previous_reload_sequence = Palette.RELOAD_SEQUENCE[]

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
            spawn_resp = Palette.request_capability("spawn_child_worker", Dict(
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
        elseif kind == "EXECUTE"
            mod = Palette.get_kernel_state().eval_module
            # A file's text arrives as a JSON string and is bound as it is,
            # so it needs no Julia quoting: `write("x.py", PAYLOAD)`.
            Core.eval(mod, Expr(:global, Expr(:(=), :PAYLOAD, payload)))
            call = length(Palette.get_kernel_state().execution_history) + 1
            Palette.note_started_call!(call, code)
            (receipt, interrupted, stuck), output = execute_turn(code, timeout_s === nothing ? nothing : Float64(timeout_s))
            printed = Palette.task_notice_hint(scrub(output))
            resp["call"] = call
            resp["bindings"] = Palette.binding_list!(Palette.get_kernel_state().eval_module, call)
            # Palette.output(call) gives back what the call printed, without what
            # the result adds in front of it below.
            Palette.retain_output!(call, printed)
            # The host can turn the digest off for an A/B of its effect ("digest": false).
            resp["output"] = get(req, "digest", true) === false ? Palette.strip_ansi(printed) : Palette.with_digest(printed)
            # The host asks for the workspace map at the session's first call and
            # after a compaction; it opens the result, ahead of the call's output.
            if get(req, "map", false) === true
                wmap = try Palette.workspace_map() catch e; "[workspace map unavailable: $(first(sprint(showerror, e), 160))]" end
                resp["output"] = wmap * "\n\n" * resp["output"]
            end
            if stuck
                resp["costs"] = deepcopy(get(Palette.TURN_COSTS, call, Dict{String, Any}()))
                resp["success"] = false
                resp["data"] = nothing
                resp["error"] = "During $(get(resp["costs"], "failed_phase", "execution")), the call exceeded its $(timeout_s)s limit and kept running after being interrupted " *
                                "for $(Int(INTERRUPT_GRACE_S))s, so the kernel stopped. " *
                                (isempty(Palette.STATE_DIR[]) ? "Every binding is gone; files written to the workspace remain." :
                                 "The next call starts a new kernel, which revives what it can of the last saved state (its report names the call) and says what it could not.")
                resp["kernel_exit"] = true
                respond(resp)
                ccall(:_exit, Cvoid, (Cint,), 3)
            end
            if !(receipt isa Palette.OperationReceipt)
                receipt = Palette.OperationReceipt(UUIDs.uuid4(), Palette.Dates.now(), "ExecuteCode",
                                                 Palette.OperationResult(nothing, false, sprint(showerror, receipt)),
                                                 0.0, Palette.get_kernel_state().id)
            end
            resp["success"] = receipt.result.success && !interrupted
            projection = get(receipt.metadata, "transport", Dict{String, Any}())
            resp["data"] = resp["success"] ? get(projection, "data", nothing) : nothing
            resp["display"] = resp["success"] ? get(projection, "display", nothing) : nothing
            resp["costs"] = deepcopy(get(Palette.TURN_COSTS, call, Dict{String, Any}()))
            resp["error"] = scrub(something(receipt.result.error, ""))
            if interrupted
                resp["interrupted"] = true
                resp["error"] = "Interrupted during $(get(resp["costs"], "failed_phase", "execution")): the call exceeded its $(timeout_s)s limit. The kernel and every binding are intact; " *
                                "processes this call started were stopped. Output printed before the interrupt is above.\n" * resp["error"]
            end
            isempty(resp["error"]) && (resp["error"] = nothing)
            Palette.note_completed_call!(call)
            interrupted || (snapshot_call = call)
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
    if revival_pending
        resp["revival"] = Palette.revival_summary()
        global revival_pending = false
    end
    if Palette.RELOAD_SEQUENCE[] > previous_reload_sequence
        resp["reloads"] = [Dict(k => r[k] for k in ("module", "status", "identity_risks", "surviving_deleted_definitions",
            "sequence", "source_digest_before", "source_digest_after", "recovery", "error")) for r in Palette.RELOAD_REPORTS if r["sequence"] > previous_reload_sequence]
    end
    respond(resp)
    # The model is reading the reply: save the state this call ended with.
    # A request already waiting goes first, until five calls have gone unsaved.
    if snapshot_call !== nothing
        try
            # Latest world: turn code defined methods (show, ==, enum names) since this loop began.
            # A request that arrives meanwhile goes first, while the last saved state is recent.
            Base.invokelatest(Palette.snapshot_state!, snapshot_call;
                              waiting=request_waiting)
        catch e
            println(stderr, "[saving the state after call $snapshot_call failed: ", first(sprint(showerror, e), 300), "]")
        end
    end
end
