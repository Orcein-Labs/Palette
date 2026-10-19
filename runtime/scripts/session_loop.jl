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

# Palette's own UUIDs binding: a project that lists only Palette cannot
# `using UUIDs` itself.
const UUIDs = Palette.UUIDs
using Palette: safe_json, execute_turn, bounded_data, text_display, scrub, with_hint

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
