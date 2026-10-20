#!/usr/bin/env julia
#=
Palette persistent session loop. It runs inside the sandboxed worker and
stays alive across many calls.

Protocol: newline-delimited JSON on stdin/stdout, one request per line and
one response per line, in order. The first line printed is HELLO. Its
`epoch` is a fresh UUID made in this process, so a caller still holding a
handle to a previous process gets a clear mismatch. The orchestrator side
is the Rust host (`runtime/host/src/session.rs`).

HELLO:    {"kind": "HELLO", "epoch", "schema_revision": 3,
           "capabilities": {"revival_observation_v1", "reload_observation_v1", "phase_costs_v1"}}
Request:  {"request_id", "kind": "EXECUTE" | "EPHEMERAL", "code",
           "payload"?, "timeout_s"?, "digest"?, "map"?,
           "session_request"?, "context"?, "budget"?    (EXECUTE only)
           "ceiling"?                                    (EPHEMERAL only)}
Response: {"kind": "RESULT" | "ERROR", "epoch", "request_id", "success", "data", "display",
           "output", "error", "call", "context", "bindings", "costs",
           "interrupted"?, "kernel_exit"?, "reloads"?, "revival"?, "transport_shortened"?}

`payload` (a string, or an object of strings) is bound as `PAYLOAD` for that
one call. `digest: false` drops the output digest; `map: true` puts the
workspace map before the output. `budget` needs `context: "budgeted"`.
`revival` appears on the first RESULT after a revival. At `timeout_s` a call
still running is interrupted (`interrupted`) and the kernel keeps its state;
one that ignores the interrupt for INTERRUPT_GRACE_S ends the process right
after its reply (`kernel_exit`).

EXECUTE evaluates into this session's persistent `eval_module`, kept for the
life of the process.

EPHEMERAL does not run in this process. It runs in a disposable,
OS-sandboxed child started through `spawn_child_worker`. Code evaluated at
runtime can change a foreign module's method table (for example
`Core.eval(Base, ...)`), which no parse-time check can see, so the boundary
sits under the language: whatever the child changes dies with it. The
call's optional `ceiling` (default `{}`) must lie within this session's own
ceiling, as for any other `spawn_child_worker` request.
=#
using Palette

# Palette's own UUIDs binding: a project that lists only Palette cannot
# `using UUIDs` itself.
const UUIDs = Palette.UUIDs
using Palette: safe_json, execute_turn, bounded_data, text_display, scrub, with_hint

const EPOCH = string(UUIDs.uuid4())
Palette.TASK_EPOCH[] = EPOCH

# The protocol gets its own duplicate of fd 1, and fd 1/fd 2 are then pointed
# at a sink file. Turn code prints, `run` children inherit fd 1, and tasks
# left running after a turn all write somewhere; none of it may reach the
# protocol channel, where one stray `println` would be parsed as a response
# and kill the session.
const PROTO = let fd = ccall(:dup, Cint, (Cint,), 1)
    fd < 0 && error("dup(1) failed")
    fdio(fd, true)
end
# Requests arrive on a duplicate of fd 0 for the same reason, and fd 0 itself
# becomes /dev/null, so a `readline()` or a `run(`cat`)` in turn code cannot
# consume the next request and leave the host waiting until the turn times
# out.
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

# Turn code's `display` writes to the turn's output, not the protocol pipe
# (see Palette.TurnDisplay).
empty!(Base.Multimedia.displays)
pushdisplay(Palette.TurnDisplay())

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


Palette.WORKSPACE_ROOT[] = pwd()
Palette.workspace_package_first!(pwd())
# The host's state directory, where each completed call's state is saved for
# a kernel that replaces this one. A revived kernel takes the old eval
# module's name: values of kernel-defined types deserialize only into it.
Palette.STATE_DIR[] = get(ENV, "PALETTE_STATE_DIR", "")
Palette.INITIAL_ENV[] = Dict{String,String}(ENV)
Palette.LOADABLE[] = Palette.loadable_packages()
# A restore the host asked for swaps the checkpoint's snapshot in first.
Palette.apply_pending_restore!()
let name = Palette.revival_module_name()
    name === nothing || (Palette.GLOBAL_STATE[] = Palette.KernelState(Module(Symbol(name))))
    mod = Palette.get_kernel_state().eval_module
    Core.eval(Main, Expr(:(=), nameof(mod), mod))
end

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
    Palette.REVIVAL_REPORT[] = Palette.restore_report(Palette.REVIVAL_REPORT[])
    # Project and trusted tools: a stub per tool, so the first call loads it.
    empty!(Palette.TOOL_USES)
    try
        Base.invokelatest(Palette.install_tool_stubs!, state.eval_module)
    catch e
        Palette.REVIVAL_REPORT[] = join(filter(!isempty, [Palette.REVIVAL_REPORT[],
            "[tools] Listing project and trusted tools failed ($(first(sprint(showerror, e), 200))); none are bound."]), "\n")
    end
end

respond(Dict{String,Any}("kind" => "HELLO", "epoch" => EPOCH, "schema_revision" => 3,
    "capabilities" => Dict("revival_observation_v1" => true, "reload_observation_v1" => true, "phase_costs_v1" => true)))

# Request handling lives in the package (Palette.serve_requests), so its
# image holds the loop's native code and the first call does not compile it.
Palette.serve_requests(respond, PROTO_IN, request_waiting, EPOCH)
