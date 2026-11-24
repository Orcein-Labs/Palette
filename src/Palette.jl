"""
Palette — persistent Julia operator core

This module implements a persistent Julia/IJulia kernel operator surface that:
- Maintains state across executions (Phase 1)
- Defines operator types and vocabulary (Phase 2)
- Implements core operations (Phase 4)
- Provides structured semantics (Phase 5)
- Enables discovery and introspection (Phase 6)
- Generates operation receipts (Phase 7)
- Supports shell escape hatch (Phase 8)
- Handles failures safely (Phase 9)

Defined in dependency order, not README phase order -- Julia requires a
struct's field types (and a function's signature types) to already exist at
definition time, so the type graph has to be built leaves-first regardless of
which phase a type belongs to conceptually. Phase-number headers are kept as
comments for navigation.

See the repository README for the full brief.
"""
module Palette

using Dates
using UUIDs
using Sockets
using JSON
using SHA
using Joovy
using PrecompileTools: @compile_workload
using Base.CoreLogging: AbstractLogger, with_logger, current_logger
using Statistics: mean as stdlib_mean, median as stdlib_median

"""
    bind_joovy!(mod)

Put Joovy's TTFX helpers on the session module. `joovy_use` / `joovy_compile_tiered`
must compile into `mod`, not host `Main`.
"""
function bind_joovy!(mod::Module)
    Core.eval(mod, :(const Joovy = $(Joovy)))
    Core.eval(mod, :(joovy_compile_tiered(code::AbstractString; kwargs...) =
        $(Joovy.joovy_compile_tiered)(String(code); mod=@__MODULE__, kwargs...)))
    Core.eval(mod, :(joovy_use(path::AbstractString; kwargs...) =
        $(Joovy.joovy_use)(String(path); mod=@__MODULE__, kwargs...)))
    Core.eval(mod, :(joovy_warm(packages; kwargs...) = $(Joovy.joovy_warm)(packages; kwargs...)))
    Core.eval(mod, :(prepare_cold_load!(args...; kwargs...) = $(Joovy.prepare_cold_load!)(args...; kwargs...)))
    Core.eval(mod, :(finish_cold_load!() = $(Joovy.finish_cold_load!)()))
    Core.eval(mod, :(compile_report() = $(Joovy.compile_report)()))
    return nothing
end

#==============================================================================
PHASE 2: Operator Types & Vocabulary (leaf types first)
==============================================================================#

"""
    OperatorType

Abstract type defining the base for all operator types.
"""
abstract type OperatorType end

#==============================================================================
PHASE 1 (part 1): Execution record -- no forward dependencies
==============================================================================#

"""
    ExecutionRecord

Records details about each code execution for audit and debugging.
"""
struct ExecutionRecord
    id::UUID
    timestamp::DateTime
    code::String
    success::Bool
    error_message::Union{Nothing, String}
    duration_ms::Float64
end

#==============================================================================
PHASE 5: Structured Semantics (needed by Phase 7's OperationReceipt)
==============================================================================#

"""
    OperationResult

Structured result from an operation execution.
"""
struct OperationResult
    data::Any
    success::Bool
    error::Union{Nothing, Any}

    function OperationResult(data::Any, success::Bool, error::Union{Nothing, Any}=nothing)
        new(data, success, error)
    end
end

"""
    StructuredResponse

A complete structured response including result, metadata, and status.
"""
struct StructuredResponse
    result::OperationResult
    metadata::Dict{String, Any}
    timestamp::DateTime

    function StructuredResponse(data::Any, success::Bool,
                                 metadata::Dict{String, Any}=Dict{String,Any}();
                                 error::Union{Nothing, Any}=nothing)
        result = OperationResult(data, success, error)
        new(result, metadata, Dates.now())
    end
end

#==============================================================================
PHASE 7: Operation Receipts (needed by KernelState's receipt_log field)
==============================================================================#

"""
    OperationReceipt

Audit trail receipt for an operation execution.
"""
struct OperationReceipt
    id::UUID
    timestamp::DateTime
    operation_type::String
    result::OperationResult
    duration_ms::Float64
    kernel_id::UUID
    metadata::Dict{String, Any}

    function OperationReceipt(id::UUID, timestamp::DateTime, op_type::String,
                              result::OperationResult, duration_ms::Float64,
                              kernel_id::UUID,
                              metadata::Dict{String, Any}=Dict{String,Any}())
        new(id, timestamp, op_type, result, duration_ms, kernel_id, metadata)
    end
end

# The receipt log keeps the outcome, not the value: holding every call's value
# kept each unassigned result alive for the whole session (two 400 MB results
# left 800 MB that GC could never reclaim).
_logged(r::OperationReceipt) = OperationReceipt(r.id, r.timestamp, r.operation_type,
    OperationResult(nothing, r.result.success, r.result.error), r.duration_ms, r.kernel_id, r.metadata)

"""
    ReceiptLog

Collection of operation receipts with query capabilities.
"""
struct ReceiptLog
    receipts::Vector{OperationReceipt}
    max_size::Int

    function ReceiptLog(max_size::Int=10000)
        new(OperationReceipt[], max_size)
    end
end

"""
    get_receipts(log::ReceiptLog;
                 operation_type::Union{String, Nothing}=nothing,
                 since::Union{DateTime, Nothing}=nothing,
                 limit::Int=100)::Vector{OperationReceipt}

Retrieves receipts with optional filtering.
"""
function get_receipts(log::ReceiptLog;
                      operation_type::Union{String, Nothing}=nothing,
                      since::Union{DateTime, Nothing}=nothing,
                      limit::Int=100)::Vector{OperationReceipt}
    filtered = log.receipts

    if !isnothing(operation_type)
        filtered = filter(r -> r.operation_type == operation_type, filtered)
    end

    if !isnothing(since)
        filtered = filter(r -> r.timestamp >= since, filtered)
    end

    return filtered[1:min(limit, length(filtered))]
end

#==============================================================================
PHASE 1 (part 2): State Persistence Infrastructure
==============================================================================#

"""
    KernelState

Mutable struct that maintains persistent state across IJulia executions.
This is the core of Phase 1 - proving state persistence.

`eval_module` is a fresh, empty Julia `Module` created once per kernel state.
`ExecuteCode` evaluates real code into this module with `Core.eval`, so
variable bindings genuinely persist for the lifetime of this `KernelState` --
this is not a simulation or a regex-based approximation of assignment.
"""
mutable struct KernelState
    id::UUID
    created_at::DateTime
    last_accessed::DateTime
    eval_module::Module
    variables::Dict{String, Any}
    execution_history::Vector{ExecutionRecord}
    operator_registry::Dict{String, Function}
    receipt_log::Vector{OperationReceipt}

    function KernelState(eval_module::Union{Module, Nothing}=nothing)
        now = Dates.now()
        state = new()
        state.id = uuid4()
        state.created_at = now
        state.last_accessed = now
        # A fresh anonymous module per kernel state: real, isolated eval scope,
        # not shared global mutable state across sessions. The precompile
        # workload passes a module of this package's own instead, because
        # precompilation refuses evaluation into any other module.
        state.eval_module = something(eval_module, Module(Symbol("KernelScope_$(replace(string(state.id), '-' => '_'))")))
        # Turn code runs in this module, not Main, so `using Palette` at Main
        # scope (as in runtime/scripts/session_loop.jl) does not reach it.
        # Binding `Palette` here lets turn code call
        # `Palette.request_capability(...)` without importing it each turn.
        Core.eval(state.eval_module, :(const Palette = $(Api)))
        Core.eval(state.eval_module, :(const Api = $(Api)))
        Core.eval(state.eval_module, :(const Pharos = $(Pharos)))
        Core.eval(state.eval_module, :(const Main = $(state.eval_module)))
        # A bare `Module(...)` has no `include` of its own. This one loads
        # files into the eval module, not Main, so code written to disk
        # through the broker can be loaded back into the session.
        # It first reloads edited workspace packages, so a call that edits a
        # package and then includes its tests runs the edited code.
        # Every ordinary module has `eval`; this one did not, and turn code calling it failed.
        Core.eval(state.eval_module, :(eval(x) = Core.eval(@__MODULE__, x)))
        Core.eval(state.eval_module, :(include(path) = $(include_observed)(@__MODULE__, path)))
        # Backtick commands start one program with no shell, so a pipe, glob
        # or `2>&1` inside them is a parse error; this is the explicit way
        # to reach a shell from turn code.
        Core.eval(state.eval_module, :(bash(script::AbstractString) = $(run_bash)(script)))
        # A non-standard string literal does no interpolation, so `$?`, `$HOME`
        # and `$(...)` reach bash. In `bash("...")` they are Julia syntax and a
        # parse error.
        Core.eval(state.eval_module, :(macro sh_str(script) :($$(run_bash)($script)) end))
        # A `"` inside sh"..." ends the literal early, and the word right after
        # it becomes a string-macro suffix, which Julia reported as a
        # MethodError for @sh_str that did not say what happened.
        Core.eval(state.eval_module, :(macro sh_str(script, suffix) $(sh_suffix_error)(suffix) end))
        Core.eval(state.eval_module, :(macro ephemeral(ex) $(_ephemeral_macro_expand)(ex) end))
        Core.eval(state.eval_module, :(macro alloc_barrier(ex) $(_alloc_barrier_macro_expand)(ex) end))
        Core.eval(state.eval_module, :(reinclude_module!(path::AbstractString; mod_name::Union{Nothing, Symbol}=nothing, into::Union{Nothing, Module}=nothing) = $(reinclude_module!)(path; mod_name, into)))
        Core.eval(state.eval_module, :(reinclude_module!(mod_name::Symbol, path::AbstractString; into::Union{Nothing, Module}=nothing) = $(reinclude_module!)(mod_name, path; into)))
        Core.eval(state.eval_module, :(reset_bindings!(syms::Symbol...; mod::Union{Nothing, Module}=nothing) = $(reset_bindings!)(syms...; mod)))
        Core.eval(state.eval_module, :(reset_bindings!(syms::AbstractVector{Symbol}; mod::Union{Nothing, Module}=nothing) = $(reset_bindings!)(syms; mod)))
        Core.eval(state.eval_module, :(const ShellResult = $(ShellResult)))
        # The InteractiveUtils varinfo() lists Main, not this module, and is not
        # loaded here at all; this one lists the session's own bindings.
        Core.eval(state.eval_module, :(varinfo() = $(varinfo)(@__MODULE__)))
        Core.eval(state.eval_module, :(kernelinfo() = $(kernelinfo)()))
        # Models type `ls`; Julia Base only has readdir. Bind both names.
        Core.eval(state.eval_module, :(ls(path::AbstractString=".") = readdir(path)))
        bind_joovy!(state.eval_module)
        if ccall(:jl_generating_output, Cint, ()) == 0
            scope_sym = nameof(state.eval_module)
            if startswith(string(scope_sym), "KernelScope_") &&
               (!latest_isdefined(Core.Main, scope_sym) || !Base.invokelatest(isconst, Core.Main, scope_sym))
                try
                    Core.eval(Core.Main, Expr(:(=), scope_sym, state.eval_module))
                catch
                end
            end
        end
        state.variables = Dict{String, Any}()
        state.execution_history = ExecutionRecord[]
        state.operator_registry = Dict{String, Function}()
        state.receipt_log = OperationReceipt[]
        return state
    end
end

"""
    get_kernel_state()

Returns the global kernel state singleton.
"""
const GLOBAL_STATE = Ref{Union{Nothing, KernelState}}(nothing)

function get_kernel_state()::KernelState
    state = GLOBAL_STATE[]
    if state === nothing
        state = KernelState()
        GLOBAL_STATE[] = state
    end
    state.last_accessed = Dates.now()
    return state
end

# The number of the call in flight, or of the next one between calls.
next_call() = length(get_kernel_state().execution_history) + 1

# Session code defines globals after the kernel's own world was fixed, so
# reads of them go through the latest world.
latest_isdefined(m::Module, s::Symbol) = Base.invokelatest(isdefined, m, s)
latest_global(m::Module, s::Symbol) = Base.invokelatest(getglobal, m, s)

"""
    reset_kernel_state()

Resets the kernel state (useful for testing or clean restarts).
"""
function reset_kernel_state()
    GLOBAL_STATE[] = nothing
    empty!(MMAP_BINDINGS)
    empty!(PIN_REGISTRY)
    empty!(EPHEMERAL_BINDINGS)
    empty!(SANDBOX_MODULES)
    empty!(SANDBOX_MODULE_GENERATIONS)
    ACTIVE_CODEGRAPH_ROOT[] = nothing
    return nothing
end

"""
    reset_kernel_state!(; process=false)

Forgets what earlier calls left behind: call history and receipts, costs,
definitions, used files, binding records, outputs, task and host-job reports,
and tool uses, sources and stubs. The session loop runs it after its warm-up
calls so the first real call is call 1.

`process=true` also forgets what the process was set up with: the state
directory (removed from disk), the revival report, the kernel state, workspace
packages and reloads, kept late-output files, the task epoch, restore and
checkpoint notices, the session context and code graphs. The precompile
workload runs it so none of its state is baked into the package image.
"""
function reset_kernel_state!(; process::Bool=false)
    state = GLOBAL_STATE[]
    if state !== nothing
        empty!(state.execution_history)
        empty!(state.receipt_log)
    end
    empty!(TURN_COSTS)
    empty!(DEFINITION_LOG); clear_definition_index!()
    empty!(CALL_FILES); empty!(USED_FILES)
    empty!(BINDING_SEEN); empty!(BINDING_ORIGINS); empty!(BINDING_IDENTITIES)
    empty!(MMAP_BINDINGS)
    empty!(PIN_REGISTRY)
    empty!(EPHEMERAL_BINDINGS)
    empty!(SANDBOX_MODULES); empty!(SANDBOX_MODULE_GENERATIONS)
    empty!(OUTPUTS); empty!(XRAY)
    OBSERVATION_ACK[] = nothing
    empty!(LAST_RUNNING[])
    empty!(REPORTED_TASKS); empty!(FINISH_REPORTED); empty!(TASK_OBSERVATIONS)
    TASK_SEQUENCE[] = 0
    empty!(HOST_JOBS); empty!(HOST_JOB_RESULTS); empty!(HOST_JOB_RESULT_ORDER)
    empty!(TOOL_USES); empty!(TOOL_SOURCES); empty!(TOOL_STUBS); empty!(LOADED_TOOLS)
    process || return nothing
    isempty(STATE_DIR[]) || rm(STATE_DIR[]; recursive=true, force=true)
    STATE_DIR[] = ""
    REVIVAL_REPORT[] = ""
    revival_observation!()
    LAST_SAVED_CALL[] = 0
    LAST_SNAPSHOT_SECONDS[] = 0.0
    GLOBAL_STATE[] = nothing
    empty!(WORKSPACE_PACKAGES); empty!(WORKSPACE_DECLARATIONS)
    empty!(RELOAD_REPORTS)
    RELOAD_SEQUENCE[] = 0
    foreach(f -> rm(f[1]; force=true), LATE_FILES)
    empty!(LATE_FILES)
    TASK_EPOCH[] = ""
    RESTORE_INFO[] = nothing
    PENDING_CHECKPOINT[] = nothing
    TIME_MACHINE_NOTICE[] = ""
    SESSION_CONTEXT[] = nothing
    ACTIVE_CODEGRAPH_ROOT[] = nothing
    empty!(CODE_GRAPHS)
    return nothing
end

"""
    OperatorVocabulary

Defines the vocabulary/lexicon available to operators.
"""
struct OperatorVocabulary <: OperatorType
    name::String
    description::String
    supported_operations::Vector{String}
    input_schema::Dict{String, Any}
    output_schema::Dict{String, Any}

    function OperatorVocabulary(name::String, description::String,
                                ops::Vector{String},
                                input_schema::Dict{String, Any}=Dict{String, Any}(),
                                output_schema::Dict{String, Any}=Dict{String, Any}())
        new(name, description, ops, input_schema, output_schema)
    end
end

"""
    Core vocabulary instances
"""
const CODE_EXECUTION_VOCAB = OperatorVocabulary(
    "CodeExecution",
    "Execute Julia code in the persistent kernel",
    ["execute", "evaluate", "run"],
    Dict{String,Any}("code" => "String", "timeout" => "Int"),
    Dict{String,Any}("result" => "Any", "success" => "Bool")
)

const OPERATOR_INVOCATION_VOCAB = OperatorVocabulary(
    "OperatorInvocation",
    "Invoke a registered operator by name",
    ["invoke", "call", "execute_operator"],
    Dict{String,Any}("operator_name" => "String", "arguments" => "Dict"),
    Dict{String,Any}("result" => "Any", "receipt" => "OperationReceipt")
)

const STATE_QUERY_VOCAB = OperatorVocabulary(
    "StateQuery",
    "Query the current kernel state",
    ["get_state", "inspect", "query"],
    Dict{String,Any}("key" => "String"),
    Dict{String,Any}("value" => "Any", "exists" => "Bool")
)

#==============================================================================
PHASE 4: Core Operations
==============================================================================#

"""
    ExecuteCode

Operation type for executing code in the kernel.
"""
struct ExecuteCode <: OperatorType
    code::String
    timeout_ms::Int
    context::Dict{String, Any}

    function ExecuteCode(code::String; timeout_ms::Int=30000, context::Dict=Dict())
        new(code, timeout_ms, context)
    end
end

"""
    EphemeralTool

Operation type for disposable, task-scoped code: generated helper logic
that should not become a permanent member of the persistent session's
`eval_module`. See `execute(op::EphemeralTool)` for why this needs to be a
genuinely different code path from `ExecuteCode`, not just a naming
convention -- Julia has no operation to un-define a method once it has
been evaluated anywhere in the process, so "ephemeral" has to be enforced
before eval, not cleaned up after.
"""
struct EphemeralTool <: OperatorType
    code::String
    tool_id::String

    function EphemeralTool(code::String; tool_id::String=string(uuid4()))
        new(code, tool_id)
    end
end

"""
    InvokeOperator

Operation type for invoking a registered operator.
"""
struct InvokeOperator <: OperatorType
    operator_name::String
    arguments::Dict{String, Any}

    function InvokeOperator(name::String; args::Dict=Dict())
        new(name, args)
    end
end

"""
    GetState

Operation type for querying kernel state.
"""
struct GetState <: OperatorType
    key::Union{String, Nothing}
    include_metadata::Bool

    function GetState(key::Union{String, Nothing}=nothing; include_metadata::Bool=true)
        new(key, include_metadata)
    end
end

"""
    ShellEscape

Operation type for executing shell commands.
"""
struct ShellEscape <: OperatorType
    command::String
    working_dir::String
    timeout_ms::Int
    capture_output::Bool

    function ShellEscape(command::String;
                         working_dir::String=pwd(),
                         timeout_ms::Int=60000,
                         capture_output::Bool=true)
        new(command, working_dir, timeout_ms, capture_output)
    end
end

"""
    ShellResult

What `bash(script)` returns: the exit status and the complete stdout and
stderr text. A process killed by a signal reports `128 + signal`, as a shell
does. `success(r)` is true for status 0.
"""
struct ShellResult
    exitcode::Int
    stdout::String
    stderr::String
end
Base.success(r::ShellResult) = r.exitcode == 0
# The text was already printed when the command ran, so repeating it here would
# return it to the model twice.
Base.show(io::IO, r::ShellResult) =
    print(io, "ShellResult(exitcode=", r.exitcode, ", stdout=", ncodeunits(r.stdout), " bytes, stderr=",
          ncodeunits(r.stderr), " bytes)")

"""
    run_bash(script) -> ShellResult

Runs `script` with `bash -c`, so pipes, globs, redirects and `&&` work. Prints
the command's stdout and then its stderr, and returns them with the exit code
instead of throwing on a nonzero one. Turn code reaches it as `bash(script)`,
or as `sh"..."`, which passes `\$` to the shell instead of interpolating it.
"""
const TURN_TASK = Ref{Union{Task, Nothing}}(nothing)
const TURN_DONE = Ref{Union{Base.Event, Nothing}}(nothing)

function run_bash(script::AbstractString)
    # Files, not pipes: a command that puts a job in the background (`job &`)
    # hands it the pipe, and reading a pipe to its end waited for the job,
    # up to the call's time limit, which then killed it. Only bash is waited
    # for; what the job writes later is reported at the start of a later call.
    done = TURN_DONE[]
    # Background `@async bash(...)` (the turn task is someone else, or the turn
    # already returned). Synchronous bash on the turn task keeps the ShellResult
    # in that call and does not ring the host doorbell.
    notify_when_done = current_task() !== TURN_TASK[]
    out, err = tempname(), tempname()
    # Append mode, so the files can be truncated safely once reported (turn.jl).
    p = open(out, "a") do o
        open(err, "a") do e
            run(pipeline(ignorestatus(`bash -c $script`); stdout=o, stderr=e))
        end
    end
    o, e = read(out, String), read(err, String)
    keep_for_late_output!(out, ncodeunits(o))
    keep_for_late_output!(err, ncodeunits(e))
    print(stdout, o)
    print(stderr, e)
    exitcode = p.termsignal > 0 ? 128 + p.termsignal : p.exitcode
    notify_when_done && maybe_notify_bash_completed(bash_completion_id(p), script, exitcode, done)
    return ShellResult(exitcode, o, e)
end

const BASH_COMPLETED_COMMAND_CAP = 2000
const BASH_COMPLETION_SEQ = Threads.Atomic{Int}(0)

# Julia 1.12 Process drops its OS pid once reaped (`getpid` → ESRCH). Long
# jobs can still report the real pid; instant ones get a positive job id.
function bash_completion_id(p)
    try
        pid = Int(getpid(p))
        pid > 0 && return pid
    catch
    end
    return Threads.atomic_add!(BASH_COMPLETION_SEQ, 1) + 1
end

# Independent `@async bash(...)` work: after the creating turn returns, ask the
# agent host to start a follow-up, the same class as IPython `[bash-done …]`.
# Synchronous bash during a turn already returns the ShellResult in that call.
function maybe_notify_bash_completed(pid, command::AbstractString, exitcode::Integer, done)
    isempty(get(ENV, "PALETTE_BROKER_SOCKET", "")) && return nothing
    try
        pid = Int(pid)
        pid > 0 || return nothing
        cmd = String(command)
        if ncodeunits(cmd) > BASH_COMPLETED_COMMAND_CAP
            cmd = cmd[1:prevind(cmd, BASH_COMPLETED_COMMAND_CAP + 1)] * "\n... [command truncated]"
        end
        payload = Dict{String, Any}("pid" => pid, "command" => cmd, "exitCode" => Int(exitcode))
        @async begin
            try
                done !== nothing && wait(done)
            catch e
                e isa InterruptException && rethrow()
            end
            try
                host_request("bash.completed", payload)
            catch e
                e isa InterruptException && rethrow()
            end
        end
    catch e
        e isa InterruptException && rethrow()
    end
    return nothing
end

function sh_suffix_error(suffix::AbstractString)
    throw(ArgumentError("a quote inside sh\"...\" ended the command early, and the text right after it " *
        "(`$(first(suffix, 40))`) was read as a suffix. Write \\\" for a quote inside sh\"...\", or put the whole " *
        "script in this call's payload and run bash(PAYLOAD): the payload needs no escaping."))
end

"""
    workspace_package_first!(root)

Puts the workspace ahead of the kernel's own environment in `LOAD_PATH` when
the workspace is itself a package. The kernel's environment holds packages of
its own (DataFrames brings OrderedCollections, for one), and a model that
appended the workspace to `LOAD_PATH` ran the package's whole test suite
against the kernel's copy: every test passed, on code it had not written.
"""
function workspace_package_first!(root::AbstractString)
    project = joinpath(root, "Project.toml")
    isfile(project) || return nothing
    toml = try
        Base.parsed_toml(project)
    catch
        return nothing
    end
    haskey(toml, "name") && haskey(toml, "uuid") || return nothing
    root in LOAD_PATH || pushfirst!(LOAD_PATH, root)
    return toml["name"]
end

"""
    WORKSPACE_PACKAGES

Packages loaded from the workspace, with the modification time and size of
every source file under their `src` directory as of their last load. `using`
a package that is already loaded does nothing, so after the model edited a
workspace package the kernel kept running the old code and in-kernel tests
passed or failed against it. Every model we watched then moved its tests to a
`julia` subprocess that recompiles the whole suite on each run.
"""
const WORKSPACE_PACKAGES = Dict{Module, Dict{String, Tuple{Float64, Int, String}}}()
const WORKSPACE_ROOT = Ref{String}("")
const WORKSPACE_DECLARATIONS = Dict{Module, Set{String}}()
const RELOAD_REPORTS = Dict{String, Any}[]
const RELOAD_SEQUENCE = Ref(0)

function source_snapshot(dir::AbstractString)
    snap = Dict{String, Tuple{Float64, Int, String}}()
    for (d, _, files) in walkdir(dir), f in files
        endswith(f, ".jl") || continue
        path = joinpath(d, f)
        st = stat(path)
        snap[path] = (st.mtime, Int(st.size), bytes2hex(sha256(read(path))))
    end
    return snap
end

function workspace_packages()
    root = WORKSPACE_ROOT[]
    isempty(root) && return Pair{Module, String}[]
    mods = Pair{Module, String}[]
    for mod in collect(values(Base.loaded_modules))
        path = pathof(mod)
        path !== nothing && startswith(path, root * "/") && push!(mods, mod => path)
    end
    return mods
end

# Evaluates the root file's module body again inside the loaded module, so
# every file it includes is re-read and each method is redefined in place.
# Definitions deleted from the source stay defined, and `__init__` does not run.
function reload_package!(mod::Module, rootfile::String)
    ex = parse_complete(read(rootfile, String); filename=rootfile)
    i = findfirst(a -> a isa Expr && a.head === :module, ex.args)
    i === nothing && error("$rootfile has no module block")
    body = ex.args[i].args[3]
    task_local_storage(:SOURCE_PATH, rootfile) do
        with_logger(DocReplacementFilter(current_logger())) do
            Core.eval(mod, Expr(:toplevel, body.args...))
        end
    end
    return nothing
end

# These are observations, not an effect log: mutable values and foreign
# modules can change without replacing a named binding or a Method object.
function reload_observation(mod::Module)
    bindings = Dict{String, Any}()
    observed_methods = Dict{Any, Method}()
    for sym in Base.invokelatest(names, mod; all=true)
        latest_isdefined(mod, sym) || continue
        value = latest_global(mod, sym)
        bindings[string(sym)] = (id=objectid(value), kind=value isa Type ? "type" : value isa Module ? "module" : "value",
                                identity=value isa Union{Type, Module} ? value : nothing)
        value isa Union{Function, Type} || continue
        for method in Base.invokelatest(methods, value)
            method.module === mod || continue
            observed_methods[method.sig] = method
        end
    end
    return (bindings=bindings, methods=observed_methods)
end

function source_declarations(snap)
    declared = Set{String}()
    function visit(ex)
        ex isa Expr || return
        union!(declared, defined_names(ex))
        ex.head in (:toplevel, :block, :module) || return
        foreach(visit, ex.args)
    end
    for path in keys(snap)
        visit(parse_complete(read(path, String); filename=path))
    end
    return declared
end

source_digest(snap) = bytes2hex(sha256(join((path * ":" * snap[path][3] for path in sort!(collect(keys(snap)))), "\n")))

function observed_reload!(mod::Module, rootfile::String, old, snap, prior_declarations)
    before = try reload_observation(mod) catch; nothing end
    parse_checked = false
    completed = false
    failure = nothing
    try
        parse_complete(read(rootfile, String); filename=rootfile)
        parse_checked = true
        Base.invokelatest(reload_package!, mod, rootfile)
        completed = true
    catch e
        failure = first(sprint(showerror, e isa LoadError ? e.error : e), 2000)
    end
    after = try Base.invokelatest(reload_observation, mod) catch; nothing end
    observed = before !== nothing && after !== nothing
    methods_changed = observed ? sort!(unique([string(sig) for sig in union(keys(before.methods), keys(after.methods))
        if get(before.methods, sig, nothing) !== get(after.methods, sig, nothing)])) : String[]
    bindings_changed = observed ? sort!([name for name in union(keys(before.bindings), keys(after.bindings))
        if !haskey(before.bindings, name) || !haskey(after.bindings, name) ||
           before.bindings[name].id != after.bindings[name].id ||
           before.bindings[name].identity !== after.bindings[name].identity]) : String[]
    identity_risks = observed ? [name for name in bindings_changed
        if haskey(before.bindings, name) && before.bindings[name].kind in ("type", "module")] : String[]
    declarations = try source_declarations(snap) catch; nothing end
    surviving = declarations === nothing || after === nothing ? String[] :
        sort!([name for name in setdiff(prior_declarations, declarations) if haskey(after.bindings, name)])
    status = !isempty(identity_risks) ? "reload_requires_clean_process" :
        completed ? "reload_succeeded" : !parse_checked ? "reload_failed_no_observed_change" :
        observed && (!isempty(methods_changed) || !isempty(bindings_changed)) ? "reload_failed_partial_change" : "state_unknown"
    report = Dict{String, Any}("status" => status, "module" => string(mod), "reload_completed" => completed,
        "source_digest_before" => source_digest(old), "source_digest_after" => source_digest(snap),
        "methods_observed_changed" => methods_changed, "bindings_observed_changed" => bindings_changed,
        "surviving_deleted_definitions" => surviving, "identity_risks" => identity_risks, "error" => failure,
        "recovery" => isempty(identity_risks) ? "Repair sources and recheck live behavior; deleted definitions require a clean process." :
            "Start a clean process without reviving incompatible objects; files remain available.",
        "observer_limits" => "Named binding and local Method identity only; in-place mutation, foreign effects, and deleted overloads of still-declared names are not fully observed. No rollback.")
    RELOAD_SEQUENCE[] += 1
    report["sequence"] = RELOAD_SEQUENCE[]
    push!(RELOAD_REPORTS, report)
    length(RELOAD_REPORTS) > KEEP_OUTPUTS && popfirst!(RELOAD_REPORTS)
    declarations === nothing || (WORKSPACE_DECLARATIONS[mod] = declarations)
    return report
end

# A reload defines every docstring a second time, and Base.Docs warns once per
# docstring ("Replacing docs for ..."): a dozen lines of noise per reload.
struct DocReplacementFilter <: AbstractLogger
    parent::AbstractLogger
end
Base.CoreLogging.min_enabled_level(f::DocReplacementFilter) = Base.CoreLogging.min_enabled_level(f.parent)
Base.CoreLogging.shouldlog(f::DocReplacementFilter, level, _module, group, id) =
    _module !== Base.Docs && Base.CoreLogging.shouldlog(f.parent, level, _module, group, id)
Base.CoreLogging.handle_message(f::DocReplacementFilter, args...; kwargs...) = Base.CoreLogging.handle_message(f.parent, args...; kwargs...)

"""
    refresh_workspace_packages!(; reload=true)

Reloads each workspace package whose sources changed since it was loaded or
last reloaded, and prints one line saying so. With `reload=false` it only
starts tracking packages loaded since the last call.
"""
function refresh_workspace_packages!(; reload::Bool=true)
    for (mod, rootfile) in workspace_packages()
        old = get(WORKSPACE_PACKAGES, mod, nothing)
        old === nothing || reload || continue
        snap = source_snapshot(dirname(rootfile))
        WORKSPACE_PACKAGES[mod] = snap
        if old === nothing
            WORKSPACE_DECLARATIONS[mod] = try source_declarations(snap) catch; Set{String}() end
            continue
        end
        old == snap && continue
        changed = sort!([relpath(f, WORKSPACE_ROOT[]) for f in union(keys(old), keys(snap))
                         if get(old, f, nothing) != get(snap, f, nothing)])
        report = observed_reload!(mod, rootfile, old, snap, get(WORKSPACE_DECLARATIONS, mod, Set{String}()))
        if report["status"] == "reload_succeeded"
            println("[reloaded $(nameof(mod)) from the workspace: $(join(changed, ", ")) changed]")
        else
            println("[reload $(nameof(mod)): $(report["status"]). No rollback; verify live state. ",
                    something(report["error"], "Type/module identity changed."), "]")
        end
        isempty(report["surviving_deleted_definitions"]) || println("[reload: deleted definitions still live: ", join(report["surviving_deleted_definitions"], ", "), "]")
    end
    return nothing
end

"""
    OUTPUTS

Everything each recent call printed, by call number. The host shows the model
a bounded slice of a result and elides the middle of anything longer, and that
text was then gone: models re-ran a 50-second test suite only to read the
failures its first run had printed.
"""
const OUTPUTS = Dict{Int, String}()
const KEEP_OUTPUTS = 64

# Execution X-Ray: the call whose request is in flight (0 between calls), and
# per call what Palette.xray reports beyond its costs: "capability_requests"
# (joined to the host's receipts by id), and the context's "dispatch", "memo"
# or "references".
const CURRENT_CALL = Ref(0)
const XRAY = Dict{Int, Dict{String, Any}}()

# Keeps a per-call table to the last KEEP_OUTPUTS calls.
retain!(d::AbstractDict{Int}, call::Int) = filter!(kv -> kv.first > call - KEEP_OUTPUTS, d)

function retain_output!(call::Int, text::String)
    isempty(text) || (OUTPUTS[call] = text)
    retain!(OUTPUTS, call)
    return nothing
end

include("rlm.jl")

function package_environment(name::AbstractString)
    occursin(r"^[A-Za-z][A-Za-z0-9_]*$", name) || throw(ArgumentError("expected a package name"))
    project = Base.active_project()
    deps = project === nothing ? Dict() : get(Base.parsed_toml(project), "deps", Dict())
    visible = try Base.find_package(String(name)) catch; nothing end
    root = isempty(WORKSPACE_ROOT[]) ? pwd() : WORKSPACE_ROOT[]
    candidates = [root, joinpath(root, name), joinpath(root, "packages", name)]
    for entry in first(sort!(readdir(root)), 64)
        isdir(joinpath(root, entry)) && push!(candidates, joinpath(root, entry))
    end
    local_paths = String[]
    for path in unique(candidates)
        file = joinpath(path, "Project.toml")
        isfile(file) || continue
        toml = try Base.parsed_toml(file) catch; continue end
        get(toml, "name", nothing) == name && push!(local_paths, path)
    end
    cached = any(depot -> isdir(joinpath(depot, "packages", name)), Base.DEPOT_PATH)
    status = visible !== nothing ? "visible" : !isempty(local_paths) ? "local_not_visible" :
             haskey(deps, name) ? "declared_dependency_not_visible" : cached ? "cached_not_in_project" : "not_found_in_inspected_locations"
    action = status == "local_not_visible" ? "Expose the local package with an explicit project or LOAD_PATH path; do not fetch a duplicate from the network." :
             status == "declared_dependency_not_visible" ? "Check the active manifest, development path and local installation; a declared dependency does not prove its files are accessible." :
             status == "cached_not_in_project" ? "Package files exist in the depot but are not visible from this project. Select the intended prepared environment or explicitly configure it." :
             status == "visible" ? "Package source is visible; an import failure needs its original diagnostic." :
             "Select an environment containing the package or provision it separately. Ecosystem availability and whether installation is needed have not been established."
    return Dict{String, Any}("name" => String(name), "status" => status, "project" => project,
        "visible_source" => visible, "local_paths" => local_paths, "declared_dependency" => haskey(deps, name),
        "cached_files" => cached, "offline" => get(ENV, "JULIA_PKG_OFFLINE", "") == "true",
        "action" => action, "limits" => "Checks active project visibility, named workspace paths, first 64 top-level workspace entries and depot package directories. No network lookup or environment mutation.")
end

"""
    Api

What turn code sees as `Palette`: the one entry point to broker-mediated
authority. It is not the whole package: a model exploring that would find
`reset_kernel_state()`, which silently discards every binding, and
`discover()`, which lists operators that do not exist.
"""
module Api
import ..Palette
host_request(rtype::AbstractString, payload::AbstractDict=Dict{String,Any}()) = Palette.host_request(rtype, payload)
runtime(action::AbstractString, payload::AbstractDict=Dict{String,Any}()) = Palette.runtime(action, payload)
const rlm = Palette.rlm
request_capability(category::String, params::Dict=Dict{String,Any}(); kwargs...) =
    Palette.request_capability(category, params; kwargs...)
host_command(name::AbstractString, args::AbstractString...; kwargs...) = Palette.host_command(name, args...; kwargs...)
host_command(name::AbstractString, args::AbstractVector{<:AbstractString}; kwargs...) =
    Palette.host_command(name, args; kwargs...)
host_command_poll(job::AbstractString) = Palette.host_command_poll(job)
host_command_wait(job::AbstractString; kw...) = Palette.host_command_wait(job; kw...)
host_command_cancel(job::AbstractString) = Palette.host_command_cancel(job)
host_jobs(; kwargs...) = Palette.host_jobs(; kwargs...)
host_commands(; kwargs...) = Palette.host_commands(; kwargs...)
fs_digest(paths::Vector{String}) = Palette.fs_digest(paths)
reloads() = deepcopy(Palette.RELOAD_REPORTS)
revival() = deepcopy(Palette.REVIVAL_OBSERVATION[])
checkpoint(name::AbstractString; note::AbstractString="") = Palette.checkpoint(name; note)
checkpoints() = Palette.checkpoints()
forget_checkpoint(name::AbstractString) = Palette.forget_checkpoint(name)
jobs() = Palette.task_observations()
taskinfo(t::Task) = Palette.task_observation(t)
environment(name::AbstractString) = Palette.package_environment(name)
provenance(name::Symbol; strong::Bool=false) = Palette.binding_provenance(name; strong)
world() = Palette.get_kernel_state().eval_module
worldinfo() = Dict("module" => string(nameof(world())), "epoch" => Palette.TASK_EPOCH[], "main_is_world" => getglobal(world(), :Main) === world(), "host_main_is_world" => world() === Main)
read_artifact(path::AbstractString; kwargs...) = Palette.read_artifact(path; kwargs...)
changes(; acknowledge::Bool=false) = Palette.changes_since_ack(; acknowledge)
costs() = deepcopy(Palette.TURN_COSTS)
xray(n::Integer) = Palette.xray(n)
context!(name; kw...) = Palette.context!(name; kw...)
reference!(f, sig::Type{<:Tuple}, impl) = Palette.reference!(f, sig, impl)
references() = Palette.references()
forget_reference!(f, sig::Type{<:Tuple}) = Palette.forget_reference!(f, sig)
promote(name::Symbol; doc::Union{Nothing, AbstractString}=nothing) = Palette.promote_tool(name; doc)
tools() = Palette.tool_index()
trust(name::Symbol) = Palette.trust_request(name)
codegraph(root::AbstractString=Palette.default_codegraph_root()) = Palette.codegraph(root)
codegraph!(root::Union{Nothing, AbstractString}) = Palette.codegraph!(root)
definitions(name::Union{AbstractString, Symbol}; graph=nothing, root::Union{Nothing, AbstractString}=nothing, match::Symbol=:exact, kwargs...) =
    Palette.definitions(name; graph, root, match, kwargs...)
callers(name::Union{AbstractString, Symbol}; graph=nothing, root::Union{Nothing, AbstractString}=nothing, match::Symbol=:exact, kwargs...) =
    Palette.callers(name; graph, root, match, kwargs...)
grep_defs(pattern::Union{AbstractString, Regex}; graph=nothing, path_prefix::AbstractString="", root::Union{Nothing, AbstractString}=nothing, exts=nothing, kwargs...) =
    Palette.grep_defs(pattern; graph, path_prefix, root, exts, kwargs...)
bind_mmap(name::Symbol, path::AbstractString, ::Type{T}, dims::Tuple{Vararg{Integer}}; offset::Integer=0) where {T} =
    Palette.bind_mmap(name, path, T, dims; offset)
bind_mmap(name::Symbol, path::AbstractString, ::Type{T}, len::Integer; offset::Integer=0) where {T} =
    Palette.bind_mmap(name, path, T, len; offset)
pin(fn::Function, name::Symbol; lazy::Bool=false) = Palette.pin(fn, name; lazy)
pin(name::Symbol, fn::Function; lazy::Bool=false) = Palette.pin(fn, name; lazy)
pin(name::Symbol) = Palette.pin(name)
unpin(name::Symbol) = Palette.unpin(name)
pinned() = Palette.pinned()
map_budgeted(f, xs; budget_s::Real=20.0, chunk::Integer=16) =
    Palette.map_budgeted(f, xs; budget_s, chunk)
ephemeral!(sym::Symbol) = Palette.ephemeral!(sym)
ephemeral!(syms::Symbol...) = Palette.ephemeral!(syms...)
unephemeral!(sym::Symbol) = Palette.unephemeral!(sym)
ephemerals() = Palette.ephemerals()
safe_summarysize(x; kwargs...) = Palette.safe_summarysize(x; kwargs...)
macro ephemeral(ex)
    return Palette._ephemeral_macro_expand(ex)
end
isolate_module(name::Symbol, path::AbstractString; reset::Bool=true) =
    Palette.isolate_module(name, path; reset)
reset_sandbox_module!(name::Symbol) = Palette.reset_sandbox_module!(name)
sandbox_modules() = Palette.sandbox_modules()
reinclude_module!(path::AbstractString; mod_name::Union{Nothing, Symbol}=nothing, into::Union{Nothing, Module}=nothing) =
    Palette.reinclude_module!(path; mod_name, into)
reinclude_module!(mod_name::Symbol, path::AbstractString; into::Union{Nothing, Module}=nothing) =
    Palette.reinclude_module!(mod_name, path; into)
reset_bindings!(syms::Symbol...; mod::Union{Nothing, Module}=nothing) =
    Palette.reset_bindings!(syms...; mod)
reset_bindings!(syms::AbstractVector{Symbol}; mod::Union{Nothing, Module}=nothing) =
    Palette.reset_bindings!(syms; mod)
eval_isolated(code::AbstractString; entry::Union{Nothing, Symbol}=nothing, args=(), kwargs=NamedTuple(), tests::Union{Nothing, AbstractString}=nothing) =
    Palette.eval_isolated(code; entry, args, kwargs, tests)
depot_mode() = Palette.depot_mode()
enable_shared_depot!(path::AbstractString=joinpath(homedir(), ".julia")) = Palette.enable_shared_depot!(path)
reload_methods!(path::AbstractString; mod::Module=Palette.get_kernel_state().eval_module) =
    Palette.reload_methods!(path; mod)
pin_cache!(key::Union{Symbol, AbstractString}, val; kwargs...) = Palette.pin_cache!(key, val; kwargs...)
get_cache(key::Union{Symbol, AbstractString}; kwargs...) = Palette.get_cache(key; kwargs...)
cache_get!(builder::Function, key::Union{Symbol, AbstractString}; kwargs...) =
    Palette.cache_get!(builder, key; kwargs...)
clear_cache!(; kwargs...) = Palette.clear_cache!(; kwargs...)
clear_cache!(key::Union{Symbol, AbstractString}; kwargs...) = Palette.clear_cache!(key; kwargs...)
summary(target::Union{Nothing, AbstractString}=nothing; max_files::Int=50) =
    Palette.summary(target; max_files)
bench(f::F, args::Vararg{Any, N}; warmup::Int=2, samples::Int=11) where {F, N} =
    Palette.bench(f, args...; warmup, samples)
macro bench(ex, opts...)
    return Palette._bench_macro_expand(ex, opts)
end
macro alloc_barrier(ex)
    return Palette._alloc_barrier_macro_expand(ex)
end
warntype_summary(f, args...) = Palette.warntype_summary(f, args...)
warntype_summary_call(f, args...) = Palette.warntype_summary_call(f, args...)
set_blas_threads!(n::Integer) = Palette.set_blas_threads!(n)
get_blas_threads() = Palette.get_blas_threads()
serve_unix_socket(handler::Function, path::AbstractString; kwargs...) =
    Palette.serve_unix_socket(handler, path; kwargs...)
stop!(handle) = Palette.stop!(handle)
check_private_isolation(; kwargs...) = Palette.check_private_isolation(; kwargs...)
private_lab_root() = Palette.private_lab_root()
configure_private_lab!(; kwargs...) = Palette.configure_private_lab!(; kwargs...)
function costs(n::Integer)
    haskey(Palette.TURN_COSTS, n) || error("no phase observations retained for call $n")
    return deepcopy(Palette.TURN_COSTS[n])
end

"""
    Palette.output(n) -> String

Everything call `n` printed, in full, for the last $(Palette.KEEP_OUTPUTS) calls.
"""
function output(n::Integer)
    haskey(Palette.OUTPUTS, n) && return Palette.OUTPUTS[n]
    error("call $n printed nothing, or is older than the last $(Palette.KEEP_OUTPUTS) calls")
end

"""
    Palette.stress(cmd; n = 200, jobs = 8, seconds = 45)

Run the shell command `cmd` `n` times, `jobs` at a time, and report the exit
codes and the first failing run; run `i` sees `STRESS_RUN=i`.
"""
stress(cmd::AbstractString; kwargs...) = Palette.stress(cmd; kwargs...)
end

# Bindings the kernel creates in every session module. They are not the
# model's own variables, so GetState and varinfo() leave them out.
const KERNEL_BINDINGS = (:eval, :include, :bash, Symbol("@sh_str"), Symbol("@ephemeral"), Symbol("@alloc_barrier"),
                         :reinclude_module!, :reset_bindings!,
                         :Palette, :Api, :Pharos, :ShellResult,
                         :varinfo, :kernelinfo, :ls, :Joovy, :joovy_compile_tiered, :joovy_use,
                         :joovy_warm, :prepare_cold_load!, :finish_cold_load!, :compile_report,
                         :ans, :PAYLOAD, :Main)

"""
    VarInfo

A table of the session's own bindings: name, size and summary.
"""
struct VarInfo
    rows::Vector{NTuple{3,String}}
end
function Base.show(io::IO, ::MIME"text/plain", v::VarInfo)
    isempty(v.rows) && return print(io, "(no bindings yet)")
    w1 = max(4, maximum(r -> textwidth(r[1]), v.rows))
    w2 = max(4, maximum(r -> textwidth(r[2]), v.rows))
    println(io, rpad("name", w1), "  ", rpad("size", w2), "  summary")
    for (i, r) in enumerate(v.rows)
        print(io, rpad(r[1], w1), "  ", rpad(r[2], w2), "  ", r[3])
        i < length(v.rows) && println(io)
    end
end

struct PinnedRef
    name::Symbol
end

function _summary(v)
    v isa PinnedRef && return "pinned ref :$(v.name)"
    v isa Function && return "function, $(length(methods(v))) method(s)"
    v isa Type && return "type"
    v isa Module && return "module"
    return try
        Base.summary(v)
    catch
        string(typeof(v))
    end
end

"""
    varinfo() -> VarInfo

Every binding this session has created, with its size and summary.
"""
function varinfo(mod::Module=get_kernel_state().eval_module)
    rows = NTuple{3,String}[]
    for sym in sort!(Base.invokelatest(names, mod; all=true))
        session_binding(mod, sym) || continue
        latest_isdefined(mod, sym) || continue
        v = latest_global(mod, sym)
        size = if sym in EPHEMERAL_BINDINGS
            "ephemeral"
        else
            nbytes = try
                safe_summarysize(v)
            catch
                nothing
            end
            nbytes === nothing ? "?" : Base.format_bytes(nbytes)
        end
        push!(rows, (string(sym), size, _summary(v)))
    end
    return VarInfo(rows)
end

"""
    kernelinfo()

Prints what this kernel is: what persists, the call time limit, the network,
the loadable and loaded packages, and the helpers turn code can use.
"""
function kernelinfo()
    mod = get_kernel_state().eval_module
    deps = try
        sort!([String(k) for k in keys(get(Base.parsed_toml(Base.active_project()), "deps", Dict())) if k != "Palette"])
    catch
        String[]
    end
    loaded = sort!([String(nameof(m)) for m in values(Base.loaded_modules) if String(nameof(m)) in deps])
    timeout = get(ENV, "PALETTE_TURN_TIMEOUT", "")
    max_timeout = get(ENV, "PALETTE_MAX_TURN_TIMEOUT", timeout)
    limit = isempty(timeout) ? "set by the host" :
        max_timeout == timeout ? timeout * "s per call" : timeout * "s per call; a call may ask for up to " * max_timeout * "s (timeout_s)"
    offline = get(ENV, "JULIA_PKG_OFFLINE", "") == "true"
    nbind = length(varinfo(mod).rows)
    println("""
        Palette kernel: Julia $(VERSION), $(Threads.nthreads()) thread(s), $(nbind) binding(s), $(Base.format_bytes(Base.gc_live_bytes())) live
        workspace  $(pwd()) (read/write; files outlive the kernel)
        persists   bindings, functions, types and loaded packages, until the kernel stops
        time limit $(limit). Waiting work (sleep, run, read, I/O) is interrupted and the kernel keeps its state;
                   compute that never yields cannot be interrupted, so the kernel stops and its bindings are lost
        network    $(offline ? "none; Pkg.add cannot install packages" : "available")
        packages   loadable: Julia stdlib, $(join(deps, ", "))
                   loaded: $(isempty(loaded) ? "none" : join(loaded, ", "))
        helpers    bash("cmd") or sh"cmd" -> ShellResult(exitcode, stdout, stderr); sh"..." keeps \$ for the shell
                   PAYLOAD: this call's payload text, raw; write(path, PAYLOAD) writes a file with no Julia quoting
                   ans: the last call's value.  varinfo(): your bindings.  ls(path="."): directory listing (readdir)
                   include("file.jl") loads a workspace file.  joovy_use("file.jl") lazy-loads it (tier 1, promotes when hot)
                   joovy_compile_tiered("f(x)=..."); prepare_cold_load!(0); finish_cold_load!(); compile_report()
                   Palette.output(n): everything call n printed, in full, when the result you saw was elided
        reloading  a package loaded from the workspace is reloaded when its source changes, before the next
                   call or include runs, so `using` it again is not needed. When the workspace is itself a
                   package, it comes first in LOAD_PATH, ahead of the kernel's own packages""")
    return nothing
end

"""
    register_operator!(state::KernelState, name::String, f::Function)

Registers a real, callable operator under `name`. `InvokeOperator` calls the
registered function directly -- there is no separate "simulated invocation"
path.
"""
function register_operator!(state::KernelState, name::String, f::Function)
    state.operator_registry[name] = f
    return nothing
end

const MAX_ERROR_FRAMES = 12

function check_parse_errors(@nospecialize ex)
    ex isa Expr || return nothing
    if ex.head in (:error, :incomplete)
        # parseall can return an error node after otherwise valid statements.
        # Eval would run those statements before encountering the error.
        problem = first(ex.args)
        throw(problem isa Exception ? problem : ErrorException(string(problem)))
    end
    foreach(check_parse_errors, ex.args)
    return nothing
end

function parse_complete(code::AbstractString; filename="none")
    parsed = Meta.parseall(code; filename)
    check_parse_errors(parsed)
    return parsed
end

"""
    softscope(ex)

Marks each top-level statement for soft scope, as `REPL.softscope` does for
the REPL and IJulia: a top-level `for` or `while` loop assigns to an
existing global instead of warning and making a new local. Without it, turn
code behaved like a script file, not an interactive session. The same
transform, kept here so Palette does not depend on the REPL stdlib.
"""
function softscope(@nospecialize ex)
    ex isa Expr || return ex
    h = ex.head
    h === :toplevel && return Expr(h, map(softscope, ex.args)...)
    h in (:meta, :import, :using, :export, :module, :error, :incomplete, :thunk) && return ex
    h === :global && all(x -> x isa Symbol, ex.args) && return ex
    return Expr(:block, Expr(:softscope, true), ex)
end

"""
    describe_error(e, bt, call_file, scope) -> String

The error message plus the stack frames above the kernel's own `execute`,
most recent first. Code from the failing call is labelled "this call";
functions defined by earlier calls are labelled "an earlier call".
"""
function describe_error(e, bt, call_file::AbstractString, scope::Module)
    # "./call 3" and "./boot.jl" are how stack frames spell these files.
    clean(file) = replace(string(file), r"^\./" => "", r"^.*/packages/([^/]+)/[^/]+/" => s"\1/",
                          r"^.*/share/julia/stdlib/v[0-9.]+/" => "")
    location(file) = file == call_file ? "this call" : startswith(file, "call ") ? "an earlier call" : file
    msg = replace(sprint(showerror, e), "@ $call_file:" => "@ this call, line ", string(scope) * "." => "",
                  string(scope) => "Main")
    e isa Meta.ParseError && return msg
    frames = try
        stacktrace(bt)
    catch
        return msg
    end
    boundary = findfirst(f -> (f.func === :execute && endswith(string(f.file), "Palette.jl")) ||
                              (f.func in (:eval_observed, :observed_statement) && endswith(string(f.file), "provenance.jl")) ||
                              (startswith(string(f.func), "#eval_observed") && endswith(string(f.file), "provenance.jl")),
                         frames)
    frames = boundary === nothing ? frames : frames[1:boundary-1]
    filter!(f -> !(f.func === :eval && clean(f.file) == "boot.jl"), frames)
    # No frame from the model's own code (a failed `using`, say): the rest is
    # Julia internals and says nothing about where the code went wrong.
    any(f -> startswith(clean(f.file), "call "), frames) || return msg
    lines = String[]
    # A run of library frames says where inside a package the error surfaced
    # and which package call led there; the frames between those two are that
    # package's internals (a failed Pkg.add listed ten of them).
    library_run = String[]
    function flush_run!()
        if length(library_run) > 3
            append!(lines, library_run[1:2])
            push!(lines, "  ... $(length(library_run) - 3) library frames omitted ...")
            push!(lines, library_run[end])
        else
            append!(lines, library_run)
        end
        empty!(library_run)
    end
    for f in frames
        name = f.func === Symbol("top-level scope") ? "top-level code" : try
            replace(sprint(Base.StackTraces.show_spec_linfo, f), string(scope) * "." => "")
        catch
            string(f.func)
        end
        # Resolver and loader signatures run to several hundred characters.
        length(name) > 160 && (name = first(name, 160) * "…)")
        where_ = location(clean(f.file))
        line = "  $name at $where_, line $(f.line)"
        if where_ in ("this call", "an earlier call")
            flush_run!()
            push!(lines, line)
        else
            push!(library_run, line)
        end
    end
    flush_run!()
    if length(lines) > MAX_ERROR_FRAMES
        half = MAX_ERROR_FRAMES ÷ 2
        lines = [lines[1:half]; "  ... $(length(lines) - 2half) frames omitted ..."; lines[end-half+1:end]]
    end
    return msg * "\nStacktrace, most recent call first:\n" * join(lines, "\n")
end

"""
    execute(op::ExecuteCode)::OperationReceipt

Executes code in the kernel's real, persistent eval module and returns a
structured receipt. This performs a genuine `Core.eval` -- the result is the
actual value Julia computed, not a placeholder.
"""
function execute(op::ExecuteCode)::OperationReceipt
    state = get_kernel_state()
    start_time = time_ns()

    result = OperationResult(nothing, true, nothing)
    error_msg = nothing
    was_interrupted = false
    call_file = "call $(next_call())"

    try
        # Meta.parse only parses a single top-level statement and throws on
        # anything after it; real code blocks are usually multi-statement, so
        # this needs parseall (a :toplevel Expr, evaluated statement-by-
        # statement, yielding the last statement's value -- REPL semantics).
        # A per-call file name gives every error a location the model can use.
        parsed = parse_complete(op.code; filename=call_file)
        value = eval_observed(state.eval_module, parsed)
        result = OperationResult(value, true, nothing)
        # REPL semantics: the last call's value stays reachable without having
        # been assigned.
        Core.eval(state.eval_module, Expr(:global, Expr(:(=), :ans, QuoteNode(value))))

        # Variable index for GetState/discovery convenience, built from
        # real module reflection (`names`) rather than regex-guessing
        # assignment targets out of the source text -- the previous
        # approach matched one simple `name =` per line and silently
        # missed destructuring (`a, b = 1, 2`), multi-statement lines
        # (`x = 1; y = 2`), and anything else that isn't that one shape.
        # `names(module; all=true)` reports exactly what Julia itself
        # considers a top-level binding in this module, which is the
        # actual question being asked here.
        # World age: Core.eval above defined eval_module's new bindings in a
        # newer world than this function runs in, so plain
        # names/isdefined/getfield here would miss them. Base.invokelatest
        # reads the current world.
        for sym in Base.invokelatest(names, state.eval_module; all=true)
            # :Palette is the constant KernelState's constructor binds here so
            # turn code can reach `Palette.request_capability` without
            # re-importing it every turn -- not a user variable, must not
            # show up in GetState/discovery any more than :eval or :include do.
            session_binding(state.eval_module, sym) || continue
            if latest_isdefined(state.eval_module, sym)
                state.variables[string(sym)] = latest_global(state.eval_module, sym)
            end
        end
        sync_kernel_modules_to_main!(state.eval_module)
    catch e
        # invokelatest: rendering a frame of a function this call defined
        # reads its binding, and from this function's older world age Julia
        # 1.12 warned "access to binding ... in a world prior to its definition".
        error_msg = Base.invokelatest(describe_error, e, catch_backtrace(), call_file, state.eval_module)
        result = OperationResult(nothing, false, error_msg)
        e isa InterruptException && (was_interrupted = true)
    end

    duration_ms = (time_ns() - start_time) / 1_000_000.0

    record = ExecutionRecord(
        uuid4(),
        Dates.now(),
        op.code,
        result.success,
        error_msg,
        duration_ms
    )
    push!(state.execution_history, record)

    receipt = OperationReceipt(
        uuid4(),
        Dates.now(),
        "ExecuteCode",
        result,
        duration_ms,
        state.id,
        was_interrupted ? Dict{String, Any}("interrupted" => true) : Dict{String, Any}()
    )
    push!(state.receipt_log, _logged(receipt))

    return receipt
end

struct EphemeralToolViolation <: Exception
    reason::String
end
Base.showerror(io::IO, e::EphemeralToolViolation) = print(io, "EphemeralToolViolation: ", e.reason)

"""
    _qualified_def_target(head)::Bool

True if a function-definition call head is a dotted/qualified name
(`function Mod.name(...)`), i.e. defines a method on a generic function
that belongs to another module, reachable and permanent from anywhere in
the process the instant it's evaluated: `Core.eval` of
`function Base.show(io, x::Int) ... end` into a brand new, unrelated module
adds a permanent method to `Base.show`, with no `import` needed.
"""
_qualified_def_target(head) = false
_qualified_def_target(head::Expr) = head.head === :(.)

"""
    _call_head(expr)

Pulls the name/target expression out of a function-definition's call
signature, for both `function f(...) ... end` and `f(...) = ...` forms.
`where`-clauses (`function f(x::T) where T`) wrap the call one level
deeper and are unwrapped first.
"""
function _call_head(expr::Expr)
    sig = expr.args[1]  # the signature slot for both `function f(...) ... end` and `f(...) = ...`
    sig = sig isa Expr && sig.head === :where ? sig.args[1] : sig
    sig isa Expr && sig.head === :call ? sig.args[1] : nothing
end

"""
    _qualified_include_call(expr)::Bool

True for a `:call` expression whose target is a qualified `.include`
access (`Base.include(...)`, `Core.include(...)`, or any `Mod.include`),
regardless of what module it targets.
"""
_qualified_include_call(expr) = false
function _qualified_include_call(expr::Expr)
    expr.head === :call || return false
    target = expr.args[1]
    target isa Expr && target.head === :(.) && length(target.args) == 2 || return false
    name = target.args[2]
    name isa QuoteNode && name.value === :include
end

"""
    check_ephemeral_source!(parsed::Expr)

Walks the ENTIRE parsed tree (not just top-level statements -- a
definition nested inside a `let`/`if`/`for`/another function body would
otherwise sail through unchecked) and throws `EphemeralToolViolation`
before any of it is ever evaluated, for the real ways this process
confirmed can extend a foreign generic function's method table
permanently:

1. Any `import` statement. Julia itself blocks bare-name extension after
   `using Mod: f` ("function Base.show must be explicitly imported to be
   extended"), but `import Mod: f` followed by a bare `function f(...)` succeeds and
   is exactly the pattern that needs to be stopped before eval, since
   Julia's own runtime won't stop it once `import` has run.
2. Any qualified function definition head (`function Mod.name(...)`),
   which extends `Mod`'s generic function directly, no `import` required.
3. Any qualified call to `.include` (`Base.include(@__MODULE__, path)`),
   confirmed to load and run a file's contents with zero syntactic trace
   in the calling code itself -- the whole point of this check is to
   examine the submitted source, and an `include`d file's contents were
   never submitted at all, so they can't be inspected here. `include`
   itself is never bound in an ephemeral tool's own module (see
   `execute(op::EphemeralTool)`) specifically so that only the fully
   qualified form needs blocking here, not a bare name too.

**This is a syntactic hygiene check, not a security boundary, and cannot
be one.** Confirmed by direct adversarial testing: `Core.eval(Base,
:(function show(io::IO, x::Int) ... end))` -- a *quoted* expression
constructed and evaluated entirely at runtime -- extends `Base.show`
exactly as effectively as writing the definition directly, and no
parse-time check on the OUTER submitted source can see inside a value
that is only ever assembled once the code is already running (Julia's
`eval`/`Core.eval` remain fully callable, and must, for "unbounded power"
to mean anything). This check stops accidental/careless pollution and the
two simplest deliberate bypass shapes; it does not stop a determined
adversarial payload built to route around exactly this check.
**Genuinely untrusted generated code that must be hard-isolated belongs
in a disposable child worker (`spawn_child_worker`, already OS-level
sandboxed and adversarially tested -- see runtime/docs/authority.md), not
same-process ephemeral tool execution.** See the repository README's
Authority philosophy note for how this relates to (and is separate from)
`SafetyGuard`, a different, unrelated policy layer.
"""
function check_ephemeral_source!(parsed::Expr)
    parsed.head === :toplevel || throw(EphemeralToolViolation("expected a top-level block"))
    _check_ephemeral_node!(parsed)
    nothing
end

function _check_ephemeral_node!(stmt)
    stmt isa Expr || return nothing

    if stmt.head === :import
        throw(EphemeralToolViolation(
            "ephemeral tool code may not use `import` -- it can permanently extend a foreign " *
            "module's function (confirmed: `import Mod: f` then a bare `function f(...)` adds a " *
            "real, permanent method). Use `using` to call existing functionality; a real need to " *
            "extend one belongs in a durable kit, not disposable code."
        ))
    elseif _qualified_include_call(stmt)
        throw(EphemeralToolViolation(
            "ephemeral tool code may not call a qualified `.include` (e.g. `Base.include(...)`) -- " *
            "this loads and runs a file whose contents were never part of the submitted source and " *
            "so cannot be checked here at all. Kit code should be loaded into the persistent " *
            "session's own eval_module, not ephemeral tool code."
        ))
    elseif stmt.head in (:function, :(=))
        head = _call_head(stmt)
        if head !== nothing && _qualified_def_target(head)
            throw(EphemeralToolViolation(
                "ephemeral tool code may not define a qualified method (`function Mod.name(...)`) -- " *
                "confirmed: this extends the foreign module's generic function permanently, with no " *
                "import needed at all. A real need to extend one belongs in a durable kit."
            ))
        end
    end

    # Recurse into every child regardless of which (if any) check matched
    # above -- a dangerous definition or qualified include call nested
    # inside a `let`/`if`/`for`/function body is exactly as real as one at
    # the top level once this code actually runs.
    for child in stmt.args
        _check_ephemeral_node!(child)
    end
    nothing
end

"""
    execute(op::EphemeralTool)::OperationReceipt

Evaluates disposable code into a genuinely fresh, throwaway `Module` --
never the persistent session's own `state.eval_module` -- so that once
every reference to the returned receipt/result is dropped, the tool's
bindings are ordinary garbage, not permanent state next to the session's
real cognitive world. `check_ephemeral_source!` runs first and throws
before any eval happens at all if the source would otherwise leave a
permanent mark on the process outside this throwaway module (see its
docstring for the two confirmed ways that can happen).

This does not, and cannot, stop a name collision from being merely
confusing (two ephemeral tools both defining a local `helper` function is
fine -- they're in different modules) -- it stops permanent, cross-cutting
pollution of shared, foreign method tables.
"""
function execute(op::EphemeralTool)::OperationReceipt
    kernel_state = get_kernel_state()
    start_time = time_ns()

    result = OperationResult(nothing, true, nothing)
    tool_module = Module(Symbol("Ephemeral_$(replace(op.tool_id, '-' => '_'))"))
    # Same reasoning as KernelState's own eval_module: ephemeral code has
    # the SAME authority ceiling as durable code (broker-mediated, not
    # granted by which module happens to run it), so it needs the same
    # reachable path to request_capability -- and, like it, only the turn
    # API, never the whole package.
    Core.eval(tool_module, :(const Palette = $(Api)))
    Core.eval(tool_module, :(const Api = $(Api)))
    Core.eval(tool_module, :(const Pharos = $(Pharos)))
    # Deliberately NOT given an `include` binding, unlike the persistent
    # eval_module: check_ephemeral_source! only inspects the source string
    # passed directly to EphemeralTool -- it cannot see inside a file that
    # code would dynamically `include` at runtime, so granting `include`
    # here would be a silent bypass of the whole namespace-pollution guard
    # (load a file containing `import Base: show; function show(...)`, and
    # the guard never looked at it). Files load into the persistent eval
    # module, not into disposable ephemeral tool code.

    try
        parsed = parse_complete(op.code)
        check_ephemeral_source!(parsed)
        value = Core.eval(tool_module, parsed)
        result = OperationResult(value, true, nothing)
    catch e
        result = OperationResult(nothing, false, sprint(showerror, e))
    end

    duration_ms = (time_ns() - start_time) / 1_000_000.0

    receipt = OperationReceipt(
        uuid4(),
        Dates.now(),
        "EphemeralTool",
        result,
        duration_ms,
        kernel_state.id,
        Dict{String,Any}("tool_id" => op.tool_id)
    )
    push!(kernel_state.receipt_log, _logged(receipt))
    record_tool_capsule(op.tool_id, op.code, result, duration_ms)

    return receipt
end

"""
    eval_isolated(code::AbstractString;
                  entry::Union{Nothing, Symbol}=nothing,
                  args=(),
                  kwargs=NamedTuple(),
                  tests::Union{Nothing, AbstractString}=nothing)
        -> NamedTuple{(:ok, :module_name, :value, :elapsed_s, :error)}

Evaluate `code` inside a fresh anonymous `Module`, optionally run `tests` inside
that same isolated module, and optionally invoke `entry` via `Base.invokelatest`
so newly defined methods work immediately without Julia 1.12 world-age errors.
"""
function eval_isolated(code::AbstractString;
                       entry::Union{Nothing, Symbol}=nothing,
                       args=(),
                       kwargs=NamedTuple(),
                       tests::Union{Nothing, AbstractString}=nothing)
    t0 = time_ns()
    mod_sym = Symbol("Isolated_", replace(string(uuid4()), '-' => '_'))
    m = Module(mod_sym)
    Core.eval(m, :(eval(x) = Core.eval(@__MODULE__, x)))
    Core.eval(m, :(include(p) = Base.include(@__MODULE__, p)))
    try
        val = Base.include_string(m, String(code), "isolated_code")
        if tests !== nothing
            val = Base.include_string(m, String(tests), "isolated_tests")
        end
        if entry !== nothing
            Base.invokelatest(isdefined, m, entry) || error("entry symbol :$entry is not defined in isolated module")
            fn = Base.invokelatest(getglobal, m, entry)
            kw = kwargs isa NamedTuple ? kwargs : NamedTuple(kwargs)
            val = Base.invokelatest(fn, args...; kw...)
        end
        elapsed_s = (time_ns() - t0) / 1e9
        return (ok=true, module_name=mod_sym, value=val, elapsed_s=elapsed_s, error=nothing)
    catch e
        bt = catch_backtrace()
        inner = e isa LoadError ? e.error : e
        err_msg = Base.invokelatest(describe_error, inner, bt, "isolated_code", m)
        elapsed_s = (time_ns() - t0) / 1e9
        return (ok=false, module_name=mod_sym, value=nothing, elapsed_s=elapsed_s, error=err_msg)
    end
end

"""
    depot_mode() -> Symbol

Return the active depot mode for this session (`:isolated` by default, or
`:shared_readonly` when the user's shared Julia depot is appended read-only).
"""
function depot_mode()
    raw = lowercase(strip(get(ENV, "PALETTE_DEPOT_MODE", "isolated")))
    return raw == "shared_readonly" ? :shared_readonly : :isolated
end

"""
    enable_shared_depot!(path::AbstractString = joinpath(homedir(), ".julia")) -> Vector{String}

Append `path` to the end of `Base.DEPOT_PATH` as a read-only fallback cache
entry (behind the workspace's primary writable depot) so precompiled `.ji`/`.so`
caches for heavy local packages are reused instead of re-precompiled from scratch.
"""
function enable_shared_depot!(path::AbstractString=joinpath(homedir(), ".julia"))
    dir = abspath(expanduser(String(path)))
    isdir(dir) || error("shared depot directory does not exist: $dir")
    if !(dir in Base.DEPOT_PATH)
        push!(Base.DEPOT_PATH, dir)
    end
    ENV["PALETTE_DEPOT_MODE"] = "shared_readonly"
    return copy(Base.DEPOT_PATH)
end

const _MAIN_SYNC_EXCLUDED_NAMES = Set{Symbol}((:Base, :Core, :Main, :Palette, :Api, :Pharos, :Joovy))

"""
    sync_kernel_modules_to_main!(scope_mod::Module) -> Vector{Symbol}

Mirror user-defined `Module` bindings from `scope_mod` (such as Palette's
isolated `KernelScope_*` module) into `Core.Main` using safe non-const bindings
so `Serialization.deserialize` can resolve `Main.<Mod>.<Type>` transparently.
Skips built-in/kernel modules and any name already bound as a `const` in `Core.Main`.
"""
function sync_kernel_modules_to_main!(scope_mod::Module)
    mirrored = Symbol[]
    ccall(:jl_generating_output, Cint, ()) != 0 && return mirrored
    scope_mod === Core.Main && return mirrored
    scope_name = nameof(scope_mod)
    for sym in Base.invokelatest(names, scope_mod; all=true, imported=false)
        (sym === scope_name || sym in _MAIN_SYNC_EXCLUDED_NAMES || sym in KERNEL_BINDINGS) && continue
        startswith(string(sym), '#') && continue
        latest_isdefined(scope_mod, sym) || continue
        val = latest_global(scope_mod, sym)
        val isa Module || continue
        _is_sandbox_module(val) && continue
        (val === scope_mod || val === Base || val === Core || val === Core.Main ||
         val === Palette || val === Api || val === Pharos || val === Joovy) && continue
        if latest_isdefined(Core.Main, sym)
            if Base.invokelatest(isconst, Core.Main, sym)
                continue
            end
            if latest_global(Core.Main, sym) === val
                push!(mirrored, sym)
                continue
            end
        end
        try
            Core.eval(Core.Main, Expr(:(=), sym, val))
            push!(mirrored, sym)
        catch
        end
    end
    return mirrored
end

function _const_lhs_symbols(lhs)
    lhs isa Symbol && return Symbol[lhs]
    lhs isa Expr && lhs.head === :(::) && !isempty(lhs.args) && return _const_lhs_symbols(lhs.args[1])
    if lhs isa Expr && lhs.head === :tuple
        out = Symbol[]
        for a in lhs.args
            append!(out, _const_lhs_symbols(a))
        end
        return out
    end
    return Symbol[]
end

function _unwrap_reload_macros(ex)
    cur = ex
    is_enum = false
    while cur isa Expr && cur.head === :macrocall && !isempty(cur.args)
        mname = macro_name(cur)
        if endswith(mname, "@enum")
            is_enum = true
            break
        end
        cur = cur.args[end]
    end
    return cur, is_enum
end

function _reload_eval_stmt!(target_mod::Module, line_node::LineNumberNode, stmt)
    Core.eval(target_mod, Expr(:toplevel, line_node, stmt))
    return nothing
end

function _reload_methods_walk!(ex, target_mod::Module, cur_file::String, line_ref::Base.RefValue{LineNumberNode},
                               counts::Dict{Symbol, Int}, visited_files::Set{String})
    if ex isa LineNumberNode
        line_ref[] = ex
        return nothing
    end
    ex isa Expr || return nothing
    if ex.head in (:toplevel, :block)
        for arg in ex.args
            _reload_methods_walk!(arg, target_mod, cur_file, line_ref, counts, visited_files)
        end
        return nothing
    end

    inner, is_enum = _unwrap_reload_macros(ex)
    if is_enum
        enames = enum_names(inner)
        if !isempty(enames) && latest_isdefined(target_mod, Symbol(enames[1]))
            counts[:skipped_types] += 1
        else
            _reload_eval_stmt!(target_mod, line_ref[], ex)
        end
        return nothing
    end
    inner isa Expr || return nothing

    h = inner.head
    if h === :module && length(inner.args) >= 3 && inner.args[2] isa Symbol
        mod_sym = inner.args[2]
        if latest_isdefined(target_mod, mod_sym)
            existing = latest_global(target_mod, mod_sym)
            if existing isa Module
                _reload_methods_walk!(inner.args[3], existing, cur_file, line_ref, counts, visited_files)
                return nothing
            end
        end
        _reload_eval_stmt!(target_mod, line_ref[], ex)
        return nothing
    elseif h === :struct && length(inner.args) >= 2
        tname = type_head(inner.args[2])
        if tname isa Symbol && latest_isdefined(target_mod, tname)
            counts[:skipped_types] += 1
        else
            _reload_eval_stmt!(target_mod, line_ref[], ex)
        end
        return nothing
    elseif (h === :abstract || h === :primitive) && !isempty(inner.args)
        tname = type_head(inner.args[1])
        if tname isa Symbol && latest_isdefined(target_mod, tname)
            counts[:skipped_types] += 1
        else
            _reload_eval_stmt!(target_mod, line_ref[], ex)
        end
        return nothing
    elseif h === :const && !isempty(inner.args)
        a = inner.args[1]
        syms = (a isa Expr && a.head === :(=) && !isempty(a.args)) ? _const_lhs_symbols(a.args[1]) : Symbol[]
        if !isempty(syms) && all(s -> latest_isdefined(target_mod, s) && Base.invokelatest(isconst, target_mod, s), syms)
            return nothing
        end
        _reload_eval_stmt!(target_mod, line_ref[], ex)
        return nothing
    elseif h === :function
        if length(inner.args) == 1 && inner.args[1] isa Symbol && latest_isdefined(target_mod, inner.args[1])
            return nothing
        end
        _reload_eval_stmt!(target_mod, line_ref[], ex)
        length(inner.args) >= 2 && (counts[:updated_methods] += 1)
        return nothing
    elseif h === :macro
        _reload_eval_stmt!(target_mod, line_ref[], ex)
        counts[:updated_methods] += 1
        return nothing
    elseif h === :(=) && length(inner.args) == 2 && is_signature(inner.args[1])
        _reload_eval_stmt!(target_mod, line_ref[], ex)
        counts[:updated_methods] += 1
        return nothing
    elseif h in (:using, :import, :export)
        _reload_eval_stmt!(target_mod, line_ref[], ex)
        return nothing
    elseif h === :call && length(inner.args) == 2 && inner.args[1] === :include && inner.args[2] isa AbstractString
        sub_path = abspath(dirname(cur_file), String(inner.args[2]))
        if isfile(sub_path) && !(sub_path in visited_files)
            _reload_methods_file!(sub_path, target_mod, counts, visited_files)
        end
        return nothing
    end
    return nothing
end

function _reload_methods_file!(abs_path::String, target_mod::Module, counts::Dict{Symbol, Int}, visited_files::Set{String})
    push!(visited_files, abs_path)
    parsed = parse_complete(read(abs_path, String); filename=abs_path)
    line_ref = Ref(LineNumberNode(1, Symbol(abs_path)))
    task_local_storage(:SOURCE_PATH, abs_path) do
        with_logger(DocReplacementFilter(current_logger())) do
            _reload_methods_walk!(parsed, target_mod, abs_path, line_ref, counts, visited_files)
        end
    end
    return nothing
end

"""
    reload_methods!(path::AbstractString; mod::Module=get_kernel_state().eval_module)
        -> NamedTuple{(:path, :updated_methods, :skipped_types, :target_module)}

Revise-style hot-reload for a Julia source file: updates `function`, short-form
method, macro-decorated method, and `macro` definitions in place while skipping
already-defined `struct`/`abstract`/`primitive` types and `const` bindings, and
descending into already-defined submodules without replacing their module identity.
"""
function reload_methods!(path::AbstractString; mod::Module=get_kernel_state().eval_module)
    abs_path = abspath(String(path))
    isfile(abs_path) || throw(ArgumentError("file not found: $abs_path"))
    counts = Dict{Symbol, Int}(:updated_methods => 0, :skipped_types => 0)
    visited = Set{String}()
    _reload_methods_file!(abs_path, mod, counts, visited)
    sync_kernel_modules_to_main!(mod)
    return (
        path=abs_path,
        updated_methods=counts[:updated_methods],
        skipped_types=counts[:skipped_types],
        target_module=nameof(mod),
    )
end

@noinline function _bench_sample(f::F, args::Args) where {F, Args<:Tuple}
    gc0 = Base.gc_num()
    t0 = time_ns()
    val = f(args...)
    dt_ns = time_ns() - t0
    gc1 = Base.gc_num()
    diff = Base.GC_Diff(gc1, gc0)
    return val, dt_ns, Int(diff.allocd)
end

"""
    bench(f, args...; warmup=2, samples=11)
        -> NamedTuple{(:value, :median_ms, :p95_ms, :min_ms, :mean_ms, :allocated_bytes, :alloc_bytes, :samples)}

Run `warmup` unmeasured calls followed by `samples` timed and allocation-measured
calls of `f(args...)` inside a fully typed function barrier (eliminating non-const
global dispatch overhead), returning the last value, median/p95/min/mean latency
in milliseconds, minimum allocated bytes (`allocated_bytes` and Wave-2 alias
`alloc_bytes`), and sample count.
"""
function bench(f::F, args::Vararg{Any, N}; warmup::Int=2, samples::Int=11) where {F, N}
    warmup >= 0 || throw(ArgumentError("warmup must be non-negative, got $warmup"))
    samples >= 1 || throw(ArgumentError("samples must be at least 1, got $samples"))
    local val
    for _ in 1:warmup
        val, _, _ = _bench_sample(f, args)
    end
    times_ms = Vector{Float64}(undef, samples)
    min_alloc = typemax(Int)
    for i in 1:samples
        val, dt_ns, alloc = _bench_sample(f, args)
        times_ms[i] = dt_ns / 1e6
        alloc < min_alloc && (min_alloc = alloc)
    end
    sorted = sort(times_ms)
    idx = 1.0 + 0.95 * (samples - 1)
    lo = floor(Int, idx)
    hi = ceil(Int, idx)
    p95 = lo == hi ? sorted[lo] : sorted[lo] + (idx - lo) * (sorted[hi] - sorted[lo])
    return (
        value=val,
        median_ms=Float64(stdlib_median(sorted)),
        p95_ms=Float64(p95),
        min_ms=sorted[1],
        mean_ms=Float64(stdlib_mean(sorted)),
        allocated_bytes=min_alloc,
        alloc_bytes=min_alloc,
        samples=samples,
    )
end

_is_pure_bench_arg(x) = x isa Union{Symbol, Number, Bool, Char, String, Nothing, Missing, QuoteNode}

function _bench_macro_expand(ex, opts)
    kws = Any[]
    has_warmup = false
    has_samples = false
    for opt in opts
        if opt isa Expr && opt.head === :(=) && opt.args[1] isa Symbol
            opt.args[1] === :warmup && (has_warmup = true)
            opt.args[1] === :samples && (has_samples = true)
            push!(kws, Expr(:kw, opt.args[1], esc(opt.args[2])))
        else
            throw(ArgumentError("@bench options must be keyword assignments like `warmup=1` or `samples=5`, got: $opt"))
        end
    end
    has_warmup || push!(kws, Expr(:kw, :warmup, 1))
    has_samples || push!(kws, Expr(:kw, :samples, 5))
    bench_ref = GlobalRef(Palette, :bench)
    if ex isa Expr && ex.head === :call && !isempty(ex.args)
        fn = ex.args[1]
        cargs = ex.args[2:end]
        if !(fn isa Expr && fn.head === :...) && all(_is_pure_bench_arg, cargs)
            return Expr(:call, bench_ref, Expr(:parameters, kws...), esc(fn), map(esc, cargs)...)
        end
    end
    thunk = Expr(:(->), Expr(:tuple), esc(ex))
    return Expr(:call, bench_ref, Expr(:parameters, kws...), thunk)
end

"""
    @bench expr [warmup=1] [samples=5]

One-call warmup, latency, and allocation probe wrapping `Palette.bench`.
Simple calls with symbol/literal arguments hoist argument evaluation so non-const
top-level globals do not cause spurious allocation noise.
"""
macro bench(ex, opts...)
    return _bench_macro_expand(ex, opts)
end

"""
    map_budgeted(f, xs; budget_s=20.0, chunk=16)
        -> NamedTuple{(:results, :completed, :total, :timed_out, :elapsed_s)}

Evaluate `f(x)` across `xs` in chunks of `chunk` items, calling `GC.safepoint()`
and `yield()` between chunks and stopping early once wall-clock time reaches
`budget_s`. Returns `(results=..., completed=..., total=..., timed_out=..., elapsed_s=...)`
so long sweeps return partial progress cleanly instead of hitting hard worker
timeouts.
"""
function map_budgeted(f, xs; budget_s::Real=20.0, chunk::Integer=16)
    (budget_s isa Bool || isnan(Float64(budget_s)) || Float64(budget_s) <= 0.0) &&
        throw(ArgumentError("budget_s must be a positive real number, got $budget_s"))
    (chunk isa Bool || chunk < 1) &&
        throw(ArgumentError("chunk must be >= 1, got $chunk"))

    budget = Float64(budget_s)
    chunk_size = Int(chunk)
    items = Base.IteratorSize(xs) isa Union{Base.HasLength, Base.HasShape} ? xs : collect(xs)
    total = length(items)

    if total == 0
        return (results=Any[], completed=0, total=0, timed_out=false, elapsed_s=0.0)
    end

    t0 = time_ns()
    raw = Vector{Any}()
    sizehint!(raw, min(total, 1024))
    completed = 0
    in_chunk = 0
    timed_out = false

    for x in items
        if (time_ns() - t0) / 1e9 >= budget
            timed_out = true
            break
        end
        push!(raw, f(x))
        completed += 1
        in_chunk += 1
        if in_chunk >= chunk_size && completed < total
            in_chunk = 0
            GC.safepoint()
            yield()
        end
    end

    elapsed_s = (time_ns() - t0) / 1e9
    results = isempty(raw) ? Any[] : [r for r in raw]
    return (
        results=results,
        completed=completed,
        total=total,
        timed_out=timed_out,
        elapsed_s=elapsed_s,
    )
end

#==============================================================================
Wave 4: @ephemeral RAM-Only Guard, Safe summarysize, Sandbox Modules & Warntype Summary
==============================================================================#

const EPHEMERAL_BINDINGS = Set{Symbol}()

"""
    ephemeral!(sym::Symbol...) -> Symbol

Mark one or more session bindings as RAM-only ephemeral: they remain resident in
memory across turns, but are completely skipped by snapshot serialization,
`Walk` graph scans, and `summarysize` scans.
"""
function ephemeral!(sym::Symbol)
    push!(EPHEMERAL_BINDINGS, sym)
    return sym
end
function ephemeral!(syms::Symbol...)
    for s in syms
        push!(EPHEMERAL_BINDINGS, s)
    end
    return collect(syms)
end

"""
    unephemeral!(sym::Symbol) -> Symbol

Remove `sym` from the ephemeral RAM-only binding set so future snapshots may
serialize it normally.
"""
function unephemeral!(sym::Symbol)
    delete!(EPHEMERAL_BINDINGS, sym)
    return sym
end

"""
    ephemerals() -> Vector{Symbol}

Return the sorted list of binding names currently marked `@ephemeral`.
"""
ephemerals() = sort!(collect(EPHEMERAL_BINDINGS))

function _ephemeral_lhs_symbols(lhs)
    lhs isa Symbol && return Symbol[lhs]
    if lhs isa Expr
        if lhs.head === :(::) && !isempty(lhs.args)
            return _ephemeral_lhs_symbols(lhs.args[1])
        elseif lhs.head === :tuple
            out = Symbol[]
            for a in lhs.args
                append!(out, _ephemeral_lhs_symbols(a))
            end
            return out
        elseif lhs.head === :const && !isempty(lhs.args)
            return _ephemeral_lhs_symbols(lhs.args[1])
        end
    end
    return Symbol[]
end

function _ephemeral_macro_expand(ex)
    reg_fn = GlobalRef(Palette, :ephemeral!)
    if ex isa Symbol
        return Expr(:call, reg_fn, QuoteNode(ex))
    elseif ex isa Expr && ex.head === :(=) && length(ex.args) == 2
        syms = _ephemeral_lhs_symbols(ex.args[1])
        isempty(syms) && throw(ArgumentError("@ephemeral expects a variable assignment or symbol, got: $ex"))
        reg_calls = [Expr(:call, reg_fn, QuoteNode(s)) for s in syms]
        return Expr(:block, reg_calls..., esc(ex))
    elseif ex isa Expr && ex.head === :const && length(ex.args) == 1 && ex.args[1] isa Expr && ex.args[1].head === :(=)
        syms = _ephemeral_lhs_symbols(ex.args[1].args[1])
        isempty(syms) && throw(ArgumentError("@ephemeral const expects a variable assignment, got: $ex"))
        reg_calls = [Expr(:call, reg_fn, QuoteNode(s)) for s in syms]
        return Expr(:block, reg_calls..., esc(ex))
    else
        throw(ArgumentError("@ephemeral expects `x = expr`, `a, b = expr`, `const X = expr`, or a symbol, got: $ex"))
    end
end

"""
    @ephemeral x = expr
    @ephemeral x

Mark `x` as a RAM-only ephemeral binding (skipped by snapshot serialization and
`summarysize` scans) and evaluate the assignment in the caller's scope.
"""
macro ephemeral(ex)
    return _ephemeral_macro_expand(ex)
end

"""
    safe_summarysize(x; max_depth::Int=32, max_nodes::Int=20_000) -> Union{Int, Nothing}

Iterative, cycle-guarded, depth-bounded byte-size estimator that never overflows
the call stack on deep or cyclic graphs and immediately short-circuits rebuildable
graph caches such as `Pharos.CodeGraph`. Returns `nothing` if the object exceeds
`max_depth` or `max_nodes`, or is an excluded graph type.
"""
function safe_summarysize(@nospecialize(x); max_depth::Int=32, max_nodes::Int=20_000)::Union{Int, Nothing}
    max_depth >= 0 || throw(ArgumentError("max_depth must be non-negative, got $max_depth"))
    max_nodes >= 1 || throw(ArgumentError("max_nodes must be positive, got $max_nodes"))
    x isa PinnedRef && return 0
    isdefined(@__MODULE__, :Pharos) && x isa Pharos.CodeGraph && return nothing

    seen = IdDict{Any, Nothing}()
    stack = Tuple{Any, Int}[(x, 0)]
    nodes = 0
    total = 0

    while !isempty(stack)
        cur, depth = pop!(stack)
        depth > max_depth && return nothing
        nodes += 1
        nodes > max_nodes && return nothing

        cur isa PinnedRef && continue
        isdefined(@__MODULE__, :Pharos) && cur isa Pharos.CodeGraph && return nothing
        T = typeof(cur)
        if cur === nothing || cur === missing || cur isa Module || cur isa Type || cur isa Task ||
           cur isa Method || cur isa Core.CodeInfo || cur isa Core.MethodInstance
            continue
        elseif cur isa Function && isdefined(T, :instance) && nfields(cur) == 0
            continue
        elseif cur isa AbstractString
            total += sizeof(cur) + sizeof(Int)
            continue
        elseif cur isa Symbol
            total += sizeof(cur)
            continue
        elseif isbitstype(T)
            total += sizeof(T)
            continue
        end

        if ismutable(cur)
            haskey(seen, cur) && continue
            seen[cur] = nothing
        end

        if cur isa Array
            E = eltype(cur)
            if isbitstype(E)
                total += sizeof(cur) + 40
                continue
            end
            n = length(cur)
            if n > max_nodes - nodes
                return nothing
            end
            total += n * sizeof(Ptr{Cvoid}) + 40
            next_depth = depth + 1
            for i in eachindex(cur)
                isassigned(cur, i) || continue
                push!(stack, (@inbounds(cur[i]), next_depth))
            end
        elseif isdefined(Core, :Memory) && cur isa getfield(Core, :Memory)
            E = eltype(cur)
            if isbitstype(E)
                total += sizeof(cur) + 16
                continue
            end
            n = length(cur)
            if n > max_nodes - nodes
                return nothing
            end
            total += n * sizeof(Ptr{Cvoid}) + 16
            next_depth = depth + 1
            for i in eachindex(cur)
                isassigned(cur, i) || continue
                push!(stack, (@inbounds(cur[i]), next_depth))
            end
        elseif cur isa Tuple
            next_depth = depth + 1
            for item in cur
                push!(stack, (item, next_depth))
            end
        else
            nf = nfields(cur)
            total += nf * sizeof(Ptr{Cvoid})
            next_depth = depth + 1
            for i in 1:nf
                isdefined(cur, i) || continue
                push!(stack, (getfield(cur, i), next_depth))
            end
        end
    end
    return total
end

# --- W4-4: Sandbox Module Isolation ---

const SANDBOX_MODULES = Dict{Symbol, Module}()
const SANDBOX_MODULE_GENERATIONS = Dict{Symbol, Int}()

function _is_sandbox_module(m::Module)
    cur = m
    while true
        startswith(string(nameof(cur)), "PaletteSandbox_") && return true
        parent = parentmodule(cur)
        (parent === cur || parent === Main || parent === Core || parent === Base) && return false
        cur = parent
    end
end

"""
    isolate_module(name::Symbol, path::AbstractString; reset::Bool=true) -> Module

Load (or cleanly reload when `reset=true`) the Julia source file at `path` inside
a fresh, versioned isolated container module tracked under `name`, without
polluting `Main`'s export namespace or triggering `UndefVarError` ambiguity or
struct redefinition conflicts on cached session data.

If `path` defines a submodule with the same name as `name`, returns that inner
module handle; otherwise returns the isolated container module directly.
"""
function isolate_module(name::Symbol, path::AbstractString; reset::Bool=true)::Module
    abs_path = abspath(String(path))
    isfile(abs_path) || throw(ArgumentError("file not found: $abs_path"))
    if !reset && haskey(SANDBOX_MODULES, name)
        return SANDBOX_MODULES[name]
    end
    gen = get(SANDBOX_MODULE_GENERATIONS, name, 0) + 1
    SANDBOX_MODULE_GENERATIONS[name] = gen
    container_sym = Symbol("PaletteSandbox_", name, "_v", gen, "_", first(replace(string(uuid4()), '-' => ""), 8))
    container = Module(container_sym)
    Core.eval(container, :(eval(x) = Core.eval(@__MODULE__, x)))
    Core.eval(container, :(include(p) = Base.include(@__MODULE__, p)))
    Core.eval(container, :(const Palette = $(Api)))
    Core.eval(container, :(const Api = $(Api)))
    Core.eval(container, :(const Pharos = $(Pharos)))
    with_logger(DocReplacementFilter(current_logger())) do
        Base.include(container, abs_path)
    end
    handle = if latest_isdefined(container, name)
        v = latest_global(container, name)
        v isa Module && v !== container ? v : container
    else
        container
    end
    SANDBOX_MODULES[name] = handle
    return handle
end

"""
    reset_sandbox_module!(name::Symbol) -> Bool

Remove `name` from the isolated sandbox module registry so the next
`isolate_module(name, path; reset=false)` loads a fresh module. Returns `true`
if an entry existed and was removed.
"""
function reset_sandbox_module!(name::Symbol)::Bool
    existed = haskey(SANDBOX_MODULES, name)
    delete!(SANDBOX_MODULES, name)
    return existed
end

"""
    sandbox_modules() -> Vector{Symbol}

Return the sorted list of currently registered isolated sandbox module names.
"""
sandbox_modules() = sort!(collect(keys(SANDBOX_MODULES)))

# --- W5-1: Clean In-Place Module Reloading & Binding Reset ---

function _top_level_module_names(ex)
    out = Symbol[]
    function visit(node)
        node isa Expr || return
        if node.head === :module && length(node.args) >= 2 && node.args[2] isa Symbol
            push!(out, node.args[2])
        elseif node.head in (:toplevel, :block)
            for a in node.args
                visit(a)
            end
        end
    end
    visit(ex)
    return out
end

function _sandbox_key_from_path(abs_path::String)::Symbol
    stem = first(splitext(basename(abs_path)))
    clean = replace(stem, r"[^A-Za-z0-9_]" => "_")
    (isempty(clean) || isdigit(first(clean))) && (clean = "Mod_" * clean)
    return Symbol(clean)
end

const _SANDBOX_BUILTINS = Set{Symbol}((:Palette, :Api, :Pharos, :Main, :Base, :Core, :eval, :include))

function _detect_sandbox_submodule(container::Module)
    found = Module[]
    c_name = nameof(container)
    for sym in Base.invokelatest(names, container; all=true, imported=false)
        (sym === c_name || sym in _SANDBOX_BUILTINS || startswith(string(sym), '#')) && continue
        latest_isdefined(container, sym) || continue
        val = latest_global(container, sym)
        if val isa Module && val !== container && parentmodule(val) === container
            push!(found, val)
        end
    end
    isempty(found) && return nothing
    return last(found)
end

"""
    reinclude_module!(path::AbstractString; mod_name=nothing, into=nothing) -> Module
    reinclude_module!(mod_name::Symbol, path::AbstractString; into=nothing) -> Module

Cleanly reload a Julia source file containing a `module` (or flat definitions)
inside a fresh `PaletteSandbox_*` container module via `isolate_module`,
detecting the inner module automatically (or using `mod_name`) and returning
the freshly loaded module handle without polluting `Core.Main` or producing
`using` ambiguities or stale-struct binding conflicts.
"""
function reinclude_module!(path::AbstractString;
                           mod_name::Union{Nothing, Symbol}=nothing,
                           into::Union{Nothing, Module}=nothing)::Module
    abs_path = abspath(String(path))
    isfile(abs_path) || throw(ArgumentError("file not found: $abs_path"))
    parsed = parse_complete(read(abs_path, String); filename=abs_path)
    ast_mods = _top_level_module_names(parsed)
    key_sym = if mod_name !== nothing
        mod_name
    elseif !isempty(ast_mods)
        last(ast_mods)
    else
        _sandbox_key_from_path(abs_path)
    end

    reset_sandbox_module!(key_sym)
    handle = isolate_module(key_sym, abs_path; reset=true)
    if mod_name === nothing && isempty(ast_mods) && startswith(string(nameof(handle)), "PaletteSandbox_")
        detected = _detect_sandbox_submodule(handle)
        if detected !== nothing
            det_sym = nameof(detected)
            det_sym !== key_sym && reset_sandbox_module!(key_sym)
            SANDBOX_MODULES[det_sym] = detected
            handle = detected
        end
    end

    if into !== nothing
        bind_sym = mod_name !== nothing ? mod_name :
                   (!startswith(string(nameof(handle)), "PaletteSandbox_") ? nameof(handle) : nothing)
        if bind_sym !== nothing && (!latest_isdefined(into, bind_sym) || !Base.invokelatest(isconst, into, bind_sym))
            try
                Core.eval(into, Expr(:global, Expr(:(=), bind_sym, QuoteNode(handle))))
            catch
            end
        end
    end
    return handle
end

reinclude_module!(mod_name::Symbol, path::AbstractString; into::Union{Nothing, Module}=nothing)::Module =
    reinclude_module!(path; mod_name, into)

function _reset_bindings!(syms, mod::Union{Nothing, Module})
    target_mod = mod === nothing ? get_kernel_state().eval_module : mod
    state = GLOBAL_STATE[]
    is_session_mod = state !== nothing && target_mod === state.eval_module
    target_name = nameof(target_mod)

    reset_syms = Symbol[]
    skipped_syms = Symbol[]
    for sym in syms
        if sym === target_name || sym in KERNEL_BINDINGS || !latest_isdefined(target_mod, sym)
            push!(skipped_syms, sym)
            continue
        end
        if Base.invokelatest(isconst, target_mod, sym)
            push!(skipped_syms, sym)
            continue
        end
        ok = try
            Core.eval(target_mod, Expr(:global, Expr(:(=), sym, QuoteNode(nothing))))
            true
        catch
            false
        end
        if ok
            push!(reset_syms, sym)
            if is_session_mod
                delete!(EPHEMERAL_BINDINGS, sym)
                delete!(MMAP_BINDINGS, sym)
                delete!(BINDING_SEEN, sym)
                delete!(BINDING_ORIGINS, sym)
                delete!(BINDING_IDENTITIES, sym)
                delete!(state.variables, string(sym))
            end
        else
            push!(skipped_syms, sym)
        end
    end
    return (reset=reset_syms, skipped=skipped_syms)
end

"""
    reset_bindings!(syms::Symbol...; mod::Union{Nothing, Module}=nothing)
        -> NamedTuple{(:reset, :skipped), Tuple{Vector{Symbol}, Vector{Symbol}}}
    reset_bindings!(syms::AbstractVector{Symbol}; mod::Union{Nothing, Module}=nothing)

Safely reset mutable user bindings in `mod` (defaulting to the active session
`eval_module`) to `nothing` and clear associated ephemeral/tracking caches.
Never throws on `const`, kernel, or undefined symbols; reports them in `skipped`.
"""
reset_bindings!(syms::Symbol...; mod::Union{Nothing, Module}=nothing) = _reset_bindings!(syms, mod)
reset_bindings!(syms::AbstractVector{Symbol}; mod::Union{Nothing, Module}=nothing) = _reset_bindings!(syms, mod)

# --- W5-2: @alloc_barrier Macro ---

@noinline function _alloc_barrier_call(f::F, args::Args) where {F, Args<:Tuple}
    f(args...)
    gc0 = Base.gc_num()
    val = f(args...)
    diff = Base.GC_Diff(Base.gc_num(), gc0)
    bytes = Int(diff.allocd)
    return (value=val, alloc_bytes=bytes, bytes=bytes)
end

@noinline function _alloc_barrier_call(::Type{T}, args::Args) where {T, Args<:Tuple}
    T(args...)
    gc0 = Base.gc_num()
    val = T(args...)
    diff = Base.GC_Diff(Base.gc_num(), gc0)
    bytes = Int(diff.allocd)
    return (value=val, alloc_bytes=bytes, bytes=bytes)
end

@noinline function _alloc_barrier_kwcall(f::F, args::Args, kwargs::KW) where {F, Args<:Tuple, KW<:NamedTuple}
    f(args...; kwargs...)
    gc0 = Base.gc_num()
    val = f(args...; kwargs...)
    diff = Base.GC_Diff(Base.gc_num(), gc0)
    bytes = Int(diff.allocd)
    return (value=val, alloc_bytes=bytes, bytes=bytes)
end

@noinline function _alloc_barrier_kwcall(::Type{T}, args::Args, kwargs::KW) where {T, Args<:Tuple, KW<:NamedTuple}
    T(args...; kwargs...)
    gc0 = Base.gc_num()
    val = T(args...; kwargs...)
    diff = Base.GC_Diff(Base.gc_num(), gc0)
    bytes = Int(diff.allocd)
    return (value=val, alloc_bytes=bytes, bytes=bytes)
end

function _is_dotted_broadcast_op(fn)::Bool
    fn isa Symbol || return false
    s = string(fn)
    return length(s) >= 2 && s[1] == '.' && fn !== :.. && fn !== :...
end

function _escape_alloc_kw(item)
    if item isa Expr && item.head === :kw && length(item.args) == 2
        return Expr(:kw, item.args[1], esc(item.args[2]))
    elseif item isa Expr && item.head === :... && length(item.args) == 1
        return Expr(:..., esc(item.args[1]))
    elseif item isa Symbol
        return Expr(:kw, item, esc(item))
    end
    return esc(item)
end

function _alloc_barrier_macro_expand(ex)
    call_ref = GlobalRef(Palette, :_alloc_barrier_call)
    kwcall_ref = GlobalRef(Palette, :_alloc_barrier_kwcall)
    if ex isa Expr && ex.head === :call && !isempty(ex.args) && !_is_dotted_broadcast_op(ex.args[1])
        fn = ex.args[1]
        pos = Any[]
        kws = Any[]
        for arg in ex.args[2:end]
            if arg isa Expr && arg.head === :parameters
                for p in arg.args
                    push!(kws, _escape_alloc_kw(p))
                end
            elseif arg isa Expr && arg.head === :kw && length(arg.args) == 2
                push!(kws, _escape_alloc_kw(arg))
            elseif arg isa Expr && arg.head === :... && length(arg.args) == 1
                push!(pos, Expr(:..., esc(arg.args[1])))
            else
                push!(pos, esc(arg))
            end
        end
        args_tuple = Expr(:tuple, pos...)
        if isempty(kws)
            return quote
                let _f = $(esc(fn)), _args = $args_tuple
                    $call_ref(_f, _args)
                end
            end
        else
            kw_nt = Expr(:tuple, Expr(:parameters, kws...))
            return quote
                let _f = $(esc(fn)), _args = $args_tuple, _kw = $kw_nt
                    $kwcall_ref(_f, _args, _kw)
                end
            end
        end
    end
    return quote
        let _thunk = () -> $(esc(ex))
            $call_ref(_thunk, ())
        end
    end
end

"""
    @alloc_barrier expr

Evaluate call arguments once, hoist them through a typed function barrier
(with 1 warmup invocation so JIT compilation does not pollute the measurement),
and measure heap allocations via `Base.gc_num()` / `Base.GC_Diff`.
Returns `(value=val, alloc_bytes=bytes, bytes=bytes)` — reporting `0` even
when called on non-const global workspace bindings.
"""
macro alloc_barrier(ex)
    return _alloc_barrier_macro_expand(ex)
end

# --- W4-6 & W5-2: Compact Warntype Summary ---

function _unwrap_ir_type(@nospecialize(t))
    if t isa Core.Const
        return typeof(t.val)
    elseif isdefined(Core, :PartialStruct) && t isa Core.PartialStruct
        return t.typ
    elseif isdefined(Core, :Conditional) && t isa Core.Conditional
        return Bool
    elseif isdefined(Core, :InterConditional) && t isa Core.InterConditional
        return Bool
    end
    return t
end

function _is_stable_type(@nospecialize(raw_t))::Bool
    t = _unwrap_ir_type(raw_t)
    t === Union{} && return true
    t === Any && return false
    t isa Union && return false
    t isa UnionAll && return false
    if t isa DataType
        t.name === Type.body.name && return true
        return Base.isconcretetype(t)
    end
    return false
end

function _is_control_stmt(@nospecialize(stmt))::Bool
    stmt isa Union{Core.GotoNode, Core.GotoIfNot, Core.ReturnNode, Core.NewvarNode, LineNumberNode, Nothing} && return true
    if stmt isa Expr && stmt.head in (:meta, :loopinfo, :boundscheck, :inbounds, :enter, :leave, :pop_exception)
        return true
    end
    return false
end

function _normalize_argtypes(argtypes)
    if argtypes isa Type && argtypes <: Tuple
        return argtypes
    elseif argtypes isa Tuple
        return Tuple{argtypes...}
    elseif argtypes isa AbstractVector
        return Tuple{argtypes...}
    end
    throw(ArgumentError("argtypes must be a Tuple of types or Tuple{...} DataType, got $(typeof(argtypes))"))
end

_is_argtype_spec(x) =
    (x isa Type && x <: Tuple) ||
    (x isa Tuple && all(a -> a isa Type, x)) ||
    (x isa AbstractVector && (eltype(x) <: Type || (!isempty(x) && all(a -> a isa Type, x))))

function _warntype_summary_tuple(f, tt::Type{<:Tuple})
    unopt = Base.invokelatest(Base.code_typed, f, tt; optimize=false)
    isempty(unopt) && error("no matching method found for $(f) with argtypes $(tt)")
    opt = Base.invokelatest(Base.code_typed, f, tt; optimize=true)

    ci_unopt, rt_unopt = unopt[1]
    ci_opt, rt_opt = isempty(opt) ? (ci_unopt, rt_unopt) : opt[1]
    rettype = _unwrap_ir_type(rt_opt)

    unstable_vars = NamedTuple[]
    if ci_unopt.slottypes !== nothing
        seen_vars = Set{Symbol}()
        for (i, sname) in enumerate(ci_unopt.slotnames)
            i == 1 && continue
            (sname === Symbol("") || startswith(string(sname), '#')) && continue
            i <= length(ci_unopt.slottypes) || continue
            stype = _unwrap_ir_type(ci_unopt.slottypes[i])
            if !_is_stable_type(stype) && !(sname in seen_vars)
                push!(seen_vars, sname)
                push!(unstable_vars, (name=sname, type=stype))
            end
        end
    end

    unstable_exprs = 0
    if ci_opt.ssavaluetypes isa Vector
        for (stmt, st) in zip(ci_opt.code, ci_opt.ssavaluetypes)
            _is_control_stmt(stmt) && continue
            _is_stable_type(st) || (unstable_exprs += 1)
        end
    end

    sig_str = try
        string(Base.invokelatest(which, f, tt).sig)
    catch
        string("Tuple{", typeof(f), ", ", join(string.(tt.parameters), ", "), "}")
    end

    concrete = _is_stable_type(rettype) && isempty(unstable_vars) && unstable_exprs == 0
    return (
        is_concretely_typed=concrete,
        return_type=rettype,
        unstable_vars=unstable_vars,
        unstable_exprs=unstable_exprs,
        method_signature=sig_str,
    )
end

"""
    warntype_summary(f, argtypes)
    warntype_summary(f, args...)
    warntype_summary_call(f, args...)
        -> NamedTuple{(:is_concretely_typed, :return_type, :unstable_vars, :unstable_exprs, :method_signature)}

Inspect `code_typed(f, argtypes)` and return a compact type-stability report
listing ONLY non-concrete (`Any`, `Union`, abstract, or `UnionAll`) user slots
and SSA expressions instead of dumping a full `@code_warntype` IR wall.
Accepts a `Tuple{...}` type, a tuple/vector of types, or runtime argument
values (`warntype_summary(f, x, y)`).
"""
function warntype_summary(f, argtypes)
    tt = _is_argtype_spec(argtypes) ? _normalize_argtypes(argtypes) : Tuple{Core.Typeof(argtypes)}
    return _warntype_summary_tuple(f, tt)
end
warntype_summary(f) = _warntype_summary_tuple(f, Tuple{})
warntype_summary(f, arg1, arg2, args...) =
    _warntype_summary_tuple(f, Base.typesof(arg1, arg2, args...))
warntype_summary_call(f, args...) = _warntype_summary_tuple(f, Base.typesof(args...))

#==============================================================================
Wave 3: Private Lab Isolation, BLAS Thread Control, and AF_UNIX Warm Worker
==============================================================================#

const DEFAULT_BUILDER_COMMS = ("grok", "gemini", "agy", "antigravity")
const LINEAR_ALGEBRA_PKG = Base.PkgId(Base.UUID("37e2e46d-f89d-539d-b4ee-838fcccc9c8e"), "LinearAlgebra")

function _env_truthy(key::AbstractString)
    return lowercase(strip(get(ENV, String(key), ""))) in ("1", "true", "yes", "on")
end

"""
    private_lab_root() -> Union{Nothing, String}

Returns the active Private Lab containment root (`PALETTE_PRIVATE_DIR` or
`PALETTE_HOME`, or `PALETTE_DATA_HOME` when `PALETTE_PRIVATE_MODE=1` is set),
or `nothing` when Private Lab isolation is not enabled.
"""
function private_lab_root()
    for key in ("PALETTE_PRIVATE_DIR", "PALETTE_HOME")
        raw = strip(get(ENV, key, ""))
        if !isempty(raw)
            return abspath(expanduser(raw))
        end
    end
    if _env_truthy("PALETTE_PRIVATE_MODE")
        raw = strip(get(ENV, "PALETTE_DATA_HOME", ""))
        isempty(raw) && error("PALETTE_PRIVATE_MODE=1 requires PALETTE_HOME or PALETTE_PRIVATE_DIR")
        return abspath(expanduser(raw))
    end
    return nothing
end

is_private_mode() = private_lab_root() !== nothing

function _assert_within_private_root(path::AbstractString, label::AbstractString="path")
    root = private_lab_root()
    p = abspath(expanduser(String(path)))
    root === nothing && return p
    r = rstrip(abspath(root), '/')
    if !(p == r || startswith(p, r * "/"))
        error("private lab containment violation: $label ($p) escapes PALETTE_HOME ($r)")
    end
    return p
end

function _ensure_private_dir!(path::AbstractString)
    p = abspath(expanduser(String(path)))
    root = private_lab_root()
    if root !== nothing
        _assert_within_private_root(p, "private directory")
    end
    mkpath(p)
    if Sys.isunix() && root !== nothing
        r = rstrip(abspath(root), '/')
        cur = p
        while true
            if isdir(cur) && !islink(cur)
                chmod(cur, 0o700)
            end
            cur == r && break
            parent = dirname(cur)
            parent == cur && break
            cur = parent
        end
        isdir(r) && !islink(r) && chmod(r, 0o700)
    end
    return p
end

function _enforce_private_file_perms!(path::AbstractString; mode::Integer=0o600)
    if Sys.isunix() && is_private_mode() && ispath(path) && !islink(path)
        chmod(path, UInt32(mode))
    end
    return path
end

function _matches_builder_comm(comm::AbstractString, builder_comms=DEFAULT_BUILDER_COMMS)
    c = lowercase(strip(String(comm)))
    isempty(c) && return false
    for known in builder_comms
        k = lowercase(String(known))
        if c == k || startswith(c, k * "-") || startswith(c, k * "_") || startswith(c, k * ".") ||
           (k == "antigravity" && startswith(c, "antigravity"))
            return true
        end
    end
    return false
end

function _parse_proc_status_uids(status_text::AbstractString)
    for line in eachsplit(status_text, '\n')
        if startswith(line, "Uid:")
            parts = split(strip(line[5:end]))
            length(parts) >= 1 || return nothing
            ruid = tryparse(Int, parts[1])
            ruid === nothing && return nothing
            euid = length(parts) >= 2 ? something(tryparse(Int, parts[2]), ruid) : ruid
            return (ruid, euid)
        end
    end
    return nothing
end

"""
    check_private_isolation(; allow_unboxed=false, proc_root="/proc", ...)

Audits `/proc` for other processes belonging to the same Linux UID and sharing
the same mount namespace (`/proc/<pid>/ns/mnt`) whose `comm` matches known
builder binaries (`grok`, `gemini`, `agy`, `antigravity`).

- A builder running in `builder-box` has a different mount namespace and is
  classified as `boxed_builders` (isolated).
- A builder running under a different Linux user (when the private lab runs as
  e.g. `inquisitor`) has a different UID and is classified as `other_uid_builders`
  (isolated).
- Any same-UID, same-mount-namespace builder is classified as `unboxed_builders`.
  When `!allow_unboxed` and `unboxed_builders` is non-empty, this throws an
  `ErrorException` naming the offending PID(s)/`comm`(s) and both real fixes.
"""
function check_private_isolation(;
    allow_unboxed::Bool=false,
    proc_root::AbstractString="/proc",
    own_pid::Integer=getpid(),
    own_uid::Union{Nothing, Integer}=nothing,
    own_mnt_ns::Union{Nothing, AbstractString}=nothing,
    builder_comms=DEFAULT_BUILDER_COMMS,
)
    uid_val = own_uid === nothing ? Int(Sys.isunix() ? ccall(:getuid, Cuint, ()) : 0) : Int(own_uid)
    ns_val = if own_mnt_ns !== nothing
        String(own_mnt_ns)
    else
        self_ns = joinpath(String(proc_root), "self", "ns", "mnt")
        try
            islink(self_ns) ? String(readlink(self_ns)) : nothing
        catch
            nothing
        end
    end

    EntryT = NamedTuple{(:pid, :comm, :uid, :mnt_ns), Tuple{Int, String, Int, Union{Nothing, String}}}
    unboxed = EntryT[]
    boxed = EntryT[]
    other_uid = EntryT[]

    if isdir(proc_root)
        for name in readdir(proc_root)
            pid = tryparse(Int, name)
            (pid === nothing || pid == Int(own_pid)) && continue
            pdir = joinpath(proc_root, name)
            comm_path = joinpath(pdir, "comm")
            comm = try
                strip(read(comm_path, String))
            catch
                continue
            end
            _matches_builder_comm(comm, builder_comms) || continue
            status_text = try
                read(joinpath(pdir, "status"), String)
            catch
                continue
            end
            uids = _parse_proc_status_uids(status_text)
            uids === nothing && continue
            ruid, euid = uids
            mnt_ns = try
                ns_path = joinpath(pdir, "ns", "mnt")
                islink(ns_path) ? String(readlink(ns_path)) : nothing
            catch
                nothing
            end
            rec = (pid=pid, comm=String(comm), uid=ruid, mnt_ns=mnt_ns)
            if ruid != uid_val && euid != uid_val
                push!(other_uid, rec)
            elseif ns_val !== nothing && mnt_ns !== nothing && mnt_ns != ns_val
                push!(boxed, rec)
            else
                push!(unboxed, rec)
            end
        end
    end

    sort!(unboxed; by=r -> r.pid)
    sort!(boxed; by=r -> r.pid)
    sort!(other_uid; by=r -> r.pid)

    isolated = isempty(unboxed)
    message = if isolated
        "private lab isolation OK (uid=$uid_val, mnt_ns=$(something(ns_val, "unknown")), boxed_builders=$(length(boxed)), other_uid_builders=$(length(other_uid)))"
    else
        offenders = join(["pid=$(r.pid) comm=$(r.comm) uid=$(r.uid) mnt_ns=$(something(r.mnt_ns, "unreadable"))" for r in unboxed], ", ")
        "PALETTE_PRIVATE_STRICT isolation failure: unboxed same-user builder process(es) detected in the same mount namespace " *
        "(uid=$uid_val, mnt_ns=$(something(ns_val, "unknown"))): [$offenders]. " *
        "A same-UID process in the same mount namespace can read 0o700 files under PALETTE_HOME. " *
        "Real fixes: (1) run builders inside `builder-box` (separate mount namespace hiding PALETTE_HOME), " *
        "or (2) run this Private Lab under a dedicated Linux user (e.g. `inquisitor`) owning 0o700 PALETTE_HOME."
    end

    report = (
        isolated=isolated,
        own_pid=Int(own_pid),
        own_uid=uid_val,
        own_mnt_ns=ns_val,
        unboxed_builders=unboxed,
        boxed_builders=boxed,
        other_uid_builders=other_uid,
        message=message,
    )

    if !isolated
        if !allow_unboxed
            error(message)
        end
        @warn message
    end
    return report
end

"""
    configure_private_lab!(; strict=nothing)

Initializes Private Lab single-root containment when `PALETTE_HOME`,
`PALETTE_PRIVATE_DIR`, or `PALETTE_PRIVATE_MODE=1` is active: creates `0o700`
directories under `PALETTE_HOME`, binds `STATE_DIR` strictly inside it, and
runs `check_private_isolation`.
"""
function configure_private_lab!(; strict::Union{Nothing, Bool}=nothing)
    root = private_lab_root()
    root === nothing && return nothing
    _ensure_private_dir!(root)
    for sub in ("workspace", "state", "worlds", "scratch", "packages", "logs", "run", "provenance", "pharos", "pinned_cache")
        _ensure_private_dir!(joinpath(root, sub))
    end
    if isempty(STATE_DIR[])
        STATE_DIR[] = joinpath(root, "state")
    else
        _assert_within_private_root(STATE_DIR[], "STATE_DIR")
        _ensure_private_dir!(STATE_DIR[])
    end
    is_strict = strict === nothing ? _env_truthy("PALETTE_PRIVATE_STRICT") : strict
    audit = check_private_isolation(; allow_unboxed=!is_strict)
    return (
        root=root,
        state_dir=STATE_DIR[],
        pinned_cache=joinpath(root, "pinned_cache"),
        logs=joinpath(root, "logs"),
        provenance=joinpath(root, "provenance"),
        pharos=joinpath(root, "pharos"),
        audit=audit,
    )
end

"""
    set_blas_threads!(n::Integer) -> Int

Sets `LinearAlgebra.BLAS.set_num_threads(Int(n))` and records
`ENV["PALETTE_BLAS_THREADS"]` so the setting survives kernel revival.
"""
function set_blas_threads!(n::Integer)
    n >= 1 || throw(ArgumentError("blas_threads must be >= 1, got $n"))
    la = Base.require(LINEAR_ALGEBRA_PKG)
    Base.invokelatest(la.BLAS.set_num_threads, Int(n))
    ENV["PALETTE_BLAS_THREADS"] = string(Int(n))
    return get_blas_threads()
end

"""
    get_blas_threads() -> Int

Returns the active `LinearAlgebra.BLAS.get_num_threads()` thread count.
"""
function get_blas_threads()
    la = Base.require(LINEAR_ALGEBRA_PKG)
    return Int(Base.invokelatest(la.BLAS.get_num_threads))
end

"""
    apply_configured_blas_threads!() -> Union{Nothing, Int}

If `ENV["PALETTE_BLAS_THREADS"]` is set to a positive integer, applies it via
`set_blas_threads!` and returns the active thread count; otherwise returns `nothing`.
"""
function apply_configured_blas_threads!()
    raw = strip(get(ENV, "PALETTE_BLAS_THREADS", ""))
    isempty(raw) && return nothing
    n = tryparse(Int, raw)
    (n === nothing || n < 1) && return nothing
    return set_blas_threads!(n)
end

mutable struct UnixSocketServerHandle
    path::String
    server::Sockets.PipeServer
    task::Union{Nothing, Task}
    running::Threads.Atomic{Bool}
    requests_handled::Threads.Atomic{Int}
    max_requests::Int
end

Base.isopen(h::UnixSocketServerHandle) = h.running[] && isopen(h.server)
Base.close(h::UnixSocketServerHandle) = stop!(h)

"""
    stop!(handle::UnixSocketServerHandle) -> UnixSocketServerHandle

Cleanly stops a Unix socket server created by `serve_unix_socket`, closes its
listening socket, waits briefly for its background task, and unlinks the socket
file if it is still a Unix socket.
"""
function stop!(h::UnixSocketServerHandle)
    h.running[] = false
    try
        isopen(h.server) && close(h.server)
    catch
    end
    t = h.task
    if t !== nothing && t !== current_task() && !istaskdone(t)
        deadline = time() + 2.0
        while !istaskdone(t) && time() < deadline
            sleep(0.01)
        end
    end
    try
        st = lstat(h.path)
        if Base.Filesystem.issocket(st)
            rm(h.path; force=true)
        end
    catch
    end
    return h
end

function _format_socket_reply(val)
    if val isa AbstractString
        return replace(chomp(String(val)), '\n' => "\\n")
    end
    return safe_json(val)
end

function _serve_unix_socket_loop!(h::UnixSocketServerHandle, handler::Function)
    try
        while h.running[] && isopen(h.server)
            conn = try
                Sockets.accept(h.server)
            catch
                break
            end
            try
                while h.running[] && isopen(conn) && !eof(conn)
                    line = try
                        readline(conn; keep=false)
                    catch
                        break
                    end
                    (isempty(line) && eof(conn)) && break
                    reply = try
                        _format_socket_reply(Base.invokelatest(handler, line))
                    catch e
                        JSON.json(Dict{String, Any}(
                            "ok" => false,
                            "error" => sprint(showerror, e),
                            "error_type" => string(typeof(e)),
                        ))
                    end
                    write(conn, reply, "\n")
                    flush(conn)
                    handled = Threads.atomic_add!(h.requests_handled, 1) + 1
                    if h.max_requests > 0 && handled >= h.max_requests
                        h.running[] = false
                        try
                            close(h.server)
                        catch
                        end
                        break
                    end
                end
            catch
            finally
                try
                    close(conn)
                catch
                end
            end
        end
    finally
        h.running[] = false
        try
            isopen(h.server) && close(h.server)
        catch
        end
    end
    return h
end

"""
    serve_unix_socket(handler::Function, path::AbstractString;
                      max_requests::Int=0, permissions::UInt32=0o600, async::Bool=true) -> UnixSocketServerHandle

Serves newline-delimited requests over a Unix domain socket (`AF_UNIX`) at `path`
using stdlib `Sockets.listen`. Safely removes stale socket files (refusing
non-sockets or live active sockets), enforces `chmod(path, permissions)` (default
`0o600`), isolates `handler(line)` inside `try/catch` (returning a structured JSON
error object instead of crashing the worker), and returns a handle that can be
cleanly shut down with `stop!(handle)`.
"""
function serve_unix_socket(
    handler::Function,
    path::AbstractString;
    max_requests::Int=0,
    permissions::Integer=0o600,
    async::Bool=true,
)
    max_requests >= 0 || throw(ArgumentError("max_requests must be >= 0, got $max_requests"))
    perms = UInt32(permissions)
    sock_path = abspath(expanduser(String(path)))
    parent = dirname(sock_path)
    if is_private_mode()
        _ensure_private_dir!(parent)
    else
        mkpath(parent)
    end
    if ispath(sock_path) || islink(sock_path)
        st = lstat(sock_path)
        if !Base.Filesystem.issocket(st)
            error("refusing to overwrite non-socket path: $sock_path")
        end
        live = try
            c = Sockets.connect(sock_path)
            close(c)
            true
        catch
            false
        end
        live && error("Unix socket is already active: $sock_path")
        rm(sock_path; force=true)
    end
    server = Sockets.listen(sock_path)
    if Sys.isunix()
        chmod(sock_path, perms)
    end
    handle = UnixSocketServerHandle(
        sock_path,
        server,
        nothing,
        Threads.Atomic{Bool}(true),
        Threads.Atomic{Int}(0),
        max_requests,
    )
    if async
        handle.task = @async _serve_unix_socket_loop!(handle, handler)
        yield()
        return handle
    end
    _serve_unix_socket_loop!(handle, handler)
    return handle
end

"""
    record_tool_capsule(tool_id, code, result, duration_ms)

Appends one mechanical, non-authoritative provenance record for a retired
ephemeral tool to `provenance.jsonl` under `ENV["PALETTE_PROVENANCE_DIR"]`,
or `.palette/` relative to `pwd()` when that is unset. A persistent session
sets it to the sandbox's private home, because there `pwd()` is the agent's
task workspace and the log would show up among the task's own files.

Deliberately mechanical, not model-authored: a content hash (same
convention -- `bytes2hex(sha256(...))`),
success/failure, and the source itself, so a human or agent can
reconstruct what ran without the now-garbage-collected ephemeral module
still existing. Written unconditionally at zero LLM cost regardless of
whether anything ever reads it -- reading is opt-in (grep/jq against a
JSONL file), never preloaded into any prompt. This is provenance, not
authorization: nothing in this codebase reads this file to decide whether
to permit anything, and it is never treated as such.
"""
function record_tool_capsule(tool_id::String, code::String, result::OperationResult, duration_ms::Float64)
    dir = if haskey(ENV, "PALETTE_PROVENANCE_DIR") && !isempty(strip(ENV["PALETTE_PROVENANCE_DIR"]))
        ENV["PALETTE_PROVENANCE_DIR"]
    elseif is_private_mode()
        joinpath(private_lab_root(), "provenance")
    else
        joinpath(pwd(), ".palette")
    end
    if is_private_mode()
        _ensure_private_dir!(dir)
    else
        mkpath(dir)
    end
    capsule = Dict{String,Any}(
        "schema" => "palette.tool_capsule.v1",
        "tool_id" => tool_id,
        "source_hash" => bytes2hex(sha256(code)),
        "source" => code,
        "success" => result.success,
        "error" => result.error,
        "duration_ms" => duration_ms,
        "retired_at" => string(Dates.now()),
        # The real outcome, so a failed ephemeral tool's capsule is told
        # apart from a successful one in the provenance log.
        "disposition" => result.success ? "RETIRED" : "FAILED",
    )
    prov_file = joinpath(dir, "provenance.jsonl")
    open(prov_file, "a") do io
        println(io, JSON.json(capsule))
    end
    _enforce_private_file_perms!(prov_file)
    nothing
end

"""
    execute(op::InvokeOperator)::OperationReceipt

Invokes a really-registered operator function (see `register_operator!`) and
returns a receipt. There is no "would invoke in a real implementation"
placeholder path -- if the operator is registered, it is actually called
with `op.arguments`.
"""
function execute(op::InvokeOperator)::OperationReceipt
    state = get_kernel_state()
    start_time = time_ns()

    result = OperationResult(nothing, false, "Operator not found: $(op.operator_name)")

    if haskey(state.operator_registry, op.operator_name)
        f = state.operator_registry[op.operator_name]
        try
            value = f(op.arguments)
            result = OperationResult(value, true, nothing)
        catch e
            result = OperationResult(nothing, false, sprint(showerror, e))
        end
    end

    duration_ms = (time_ns() - start_time) / 1_000_000.0

    receipt = OperationReceipt(
        uuid4(),
        Dates.now(),
        "InvokeOperator",
        result,
        duration_ms,
        state.id,
        Dict{String,Any}("operator" => op.operator_name)
    )
    push!(state.receipt_log, _logged(receipt))

    return receipt
end

"""
    execute(op::GetState)::StructuredResponse

Queries kernel state and returns a structured response.
"""
function execute(op::GetState)::StructuredResponse
    state = get_kernel_state()

    if !isnothing(op.key)
        exists = haskey(state.variables, op.key)
        value = exists ? state.variables[op.key] : nothing

        return StructuredResponse(
            Dict("key" => op.key, "value" => value, "exists" => exists),
            true,
            Dict{String,Any}();
        )
    else
        metadata = op.include_metadata ? Dict{String,Any}(
            "kernel_id" => string(state.id),
            "created_at" => string(state.created_at),
            "last_accessed" => string(state.last_accessed),
            "variable_count" => length(state.variables),
            "execution_count" => length(state.execution_history)
        ) : Dict{String,Any}()

        return StructuredResponse(
            merge(Dict("variables" => collect(keys(state.variables))), metadata),
            true,
            metadata;
        )
    end
end

"""
    execute(op::ShellEscape)::OperationReceipt

Executes a real shell command as a real subprocess and returns a receipt
carrying its actual stdout, stderr, and exit code. This is a genuine
`run`/pipe, not a static-analysis placeholder -- and it has no safety
envelope of its own (see SafetyGuard / safe_execute, and the repo-level
authority-philosophy note: this experiment intentionally does not implement
privilege enforcement).
"""
function execute(op::ShellEscape)::OperationReceipt
    state = get_kernel_state()
    start_time = time_ns()

    out_buf = IOBuffer()
    err_buf = IOBuffer()
    result = OperationResult(nothing, false, nothing)

    try
        cmd = Cmd(`sh -c $(op.command)`; dir=op.working_dir)
        proc = run(pipeline(cmd; stdout=out_buf, stderr=err_buf); wait=false)

        # Event-driven timeout, not a 10ms sleep-poll loop: a `Timer` fires
        # once at the deadline and kills the process if it's still
        # running, while `wait(proc)` blocks cooperatively on the task
        # scheduler in the meantime (no busy-waking every 10ms for the
        # entire duration of every shell command this runs).
        timed_out = Ref(false)
        timer = Timer(op.timeout_ms / 1000) do _
            if process_running(proc)
                timed_out[] = true
                kill(proc)
            end
        end
        wait(proc)
        close(timer)

        if timed_out[]
            out = String(take!(out_buf)); err = String(take!(err_buf))
            result = OperationResult(
                Dict("stdout" => out, "stderr" => err,
                     "stdout_sha256" => bytes2hex(sha256(out)),
                     "stderr_sha256" => bytes2hex(sha256(err)),
                     "error_class" => "timeout"),
                false,
                "timed out after $(op.timeout_ms)ms",
            )
        else
            # A process killed by a signal reports 128 + signal, as a shell
            # does. Julia's Process.exitcode is 0 alongside a nonzero
            # termsignal, so reading it raw reported success for a
            # signal-killed command (run_bash already mapped this correctly).
            exit_code = proc.termsignal > 0 ? 128 + proc.termsignal : proc.exitcode
            error_class = exit_code == 0 ? nothing :
                          proc.termsignal > 0 ? "signal_death" : "clean_failure"
            data = op.capture_output ? begin
                out = String(take!(out_buf)); err = String(take!(err_buf))
                # Output digests: the observer is the kernel itself
                # (self-attested; the host-side check is Palette.fs_digest). They
                # make captured output recomputable and detect tunnel corruption.
                # Contract: error_class is ABSENT on success, not null.
                d = Dict{String,Any}("stdout" => out, "stderr" => err,
                                     "exit_code" => exit_code,
                                     "stdout_sha256" => bytes2hex(sha256(out)),
                                     "stderr_sha256" => bytes2hex(sha256(err)))
                error_class === nothing || (d["error_class"] = error_class)
                d
            end : begin
                d = Dict{String,Any}("exit_code" => exit_code)
                error_class === nothing || (d["error_class"] = error_class)
                d
            end
            result = OperationResult(data, exit_code == 0, exit_code == 0 ? nothing : "exit code $(exit_code)")
        end
    catch e
        # Spawn-stage failures (vanished working_dir, ENOENT, …) get a typed
        # class instead of prose-only errors.
        result = OperationResult(Dict("error_class" => "spawn_error"), false, sprint(showerror, e))
    end

    duration_ms = (time_ns() - start_time) / 1_000_000.0

    receipt = OperationReceipt(
        uuid4(),
        Dates.now(),
        "ShellEscape",
        result,
        duration_ms,
        state.id,
        Dict{String,Any}("command" => op.command, "working_dir" => op.working_dir,
                         "error_class" => result.data isa AbstractDict ? get(result.data, "error_class", nothing) : nothing)
    )
    push!(state.receipt_log, _logged(receipt))

    return receipt
end

#==============================================================================
PHASE 6: Discovery & Introspection
==============================================================================#

"""
    DiscoveryService

Service for discovering available operators and capabilities.
"""
struct DiscoveryService
    state::KernelState

    function DiscoveryService()
        new(get_kernel_state())
    end
end

"""
    IntrospectionResult

Result of introspecting the operator surface.
"""
struct IntrospectionResult
    available_operators::Vector{String}
    vocabularies::Vector{String}
    registered_variables::Vector{String}
    execution_stats::Dict{String, Any}
    kernel_info::Dict{String, Any}
end

"""
    mean(iter)::Float64

`Statistics.mean` (a stdlib, not the StatsBase dependency this was
originally hand-rolled to avoid) with this package's own empty-input
convention (`0.0`, not `NaN`) for `execution_stats`.
"""
function mean(iter)
    isempty(iter) && return 0.0
    return Float64(stdlib_mean(iter))
end

"""
    discover(service::DiscoveryService)::IntrospectionResult

Discovers all available operators and capabilities.
"""
function discover(service::DiscoveryService)::IntrospectionResult
    state = service.state

    operators = collect(keys(state.operator_registry))
    builtin_ops = ["ExecuteCode", "InvokeOperator", "GetState",
                   "ShellEscape", "Discover", "Introspect"]
    operators = unique(vcat(operators, builtin_ops))

    vocabularies = ["CodeExecution", "OperatorInvocation", "StateQuery", "ShellEscape"]

    total_executions = length(state.execution_history)
    successful = sum(r.success for r in state.execution_history; init=0)
    failed = total_executions - successful
    avg_duration = total_executions > 0 ?
        mean(r.duration_ms for r in state.execution_history) : 0.0

    execution_stats = Dict(
        "total" => total_executions,
        "successful" => successful,
        "failed" => failed,
        "average_duration_ms" => avg_duration
    )

    kernel_info = Dict(
        "id" => string(state.id),
        "created_at" => string(state.created_at),
        "uptime_seconds" => Dates.value(Dates.now() - state.created_at) / 1000.0,
        "variable_count" => length(state.variables),
        "receipt_count" => length(state.receipt_log)
    )

    return IntrospectionResult(
        operators,
        vocabularies,
        collect(keys(state.variables)),
        execution_stats,
        kernel_info
    )
end

#==============================================================================
PHASE 9: Failure Handling & Safety
==============================================================================#

"""
    ErrorHandler

Handles errors with configurable strategies.
"""
struct ErrorHandler
    strategy::Symbol  # :propagate, :log, :recover, :fail_safe
    max_retries::Int
    timeout_ms::Int

    function ErrorHandler(strategy::Symbol=:log;
                          max_retries::Int=3,
                          timeout_ms::Int=30000)
        @assert strategy in [:propagate, :log, :recover, :fail_safe]
        new(strategy, max_retries, timeout_ms)
    end
end

"""
    SafetyGuard

Enforces safety constraints on operations. It is an allowlist and pattern
blocklist, not a security boundary: authority is enforced by the broker (see
the repository README's Authority philosophy section). It demonstrates
failure containment.
"""
struct SafetyGuard
    max_execution_time_ms::Int
    max_memory_mb::Int
    allowed_operations::Set{DataType}
    blocked_patterns::Vector{Regex}

    function SafetyGuard(max_time::Int=60000,
                         max_mem::Int=1024,
                         allowed::Set{DataType}=Set{DataType}([ExecuteCode, InvokeOperator, GetState]),
                         blocked::Vector{Regex}=Regex[])
        new(max_time, max_mem, allowed, blocked)
    end
end

"""
    validate(op, guard::SafetyGuard)::Bool

Validates that an operation passes safety checks. Dispatches on the
operation's actual type against a `Set{DataType}`, so two unrelated types
that share a short name are never confused.
"""
function validate(op, guard::SafetyGuard)::Bool
    return typeof(op) in guard.allowed_operations
end

"""
    check_patterns(code::String, guard::SafetyGuard)::Vector{String}

Checks code against blocked patterns and returns matches.
"""
function check_patterns(code::String, guard::SafetyGuard)::Vector{String}
    matches = String[]
    for pattern in guard.blocked_patterns
        if occursin(pattern, code)
            push!(matches, string(pattern))
        end
    end
    return matches
end

"""
    safe_execute(op, guard::SafetyGuard)::Union{OperationReceipt, StructuredResponse}

Executes an operation with the toy safety guard applied.
"""
function safe_execute(op, guard::SafetyGuard)
    if !validate(op, guard)
        return StructuredResponse(
            nothing,
            false,
            Dict{String,Any}("reason" => "Operation not allowed");
            error = "Safety violation: operation type not permitted"
        )
    end

    if op isa ExecuteCode
        violations = check_patterns(op.code, guard)
        if !isempty(violations)
            return StructuredResponse(
                nothing,
                false,
                Dict{String,Any}("violations" => violations);
                error = "Safety violation: blocked patterns detected"
            )
        end
    end

    return execute(op)
end

#==============================================================================
EXPERIMENT 002: broker-mediated capability requests
==============================================================================#

"""
    request_capability(category::String, params::Dict=Dict(); timeout_s::Real=320.0) -> Dict

Request a broker-mediated (Effect Class C) capability -- see
docs/CAPABILITY_MODEL.md and docs/EXPERIMENT_002_AUTHORITY.md.

This is the ONLY code path in this package that can reach outside the
sandbox's own bounded filesystem/network envelope. It does not perform any
external effect itself: it connects to the host capability broker over the
Unix socket named by the `PALETTE_BROKER_SOCKET` environment variable (set
by `palette-host`'s sandbox launcher, reachable only because that one socket path
was explicitly bind-mounted into this sandbox), sends one JSON request, and
returns the broker's JSON response. The broker -- a separate process
running outside this sandbox's OS namespace entirely -- decides whether to
approve the request against this session's fixed capability ceiling, and if
approved, performs the actual effect itself, with host privilege this
process never has.

This function is a convenience for well-behaved callers. It is not, and is
not meant to be, an enforcement mechanism: a caller that wants to skip it
and try the effect directly (raw `write`, raw `Sockets.connect`, `ccall`,
`run`) is exactly the adversarial case Experiment 002's tests exercise --
and what stops that is the OS-level sandbox boundary this process runs
inside, not this function's cooperation.

Throws if `PALETTE_BROKER_SOCKET` is unset (no broker configured for this
session -- e.g. running unsandboxed) or if the connection fails.
"""
function request_capability(category::String, params::Dict=Dict{String,Any}(); timeout_s::Real=320.0)
    sock_path = get(ENV, "PALETTE_BROKER_SOCKET", "")
    isempty(sock_path) && error("PALETTE_BROKER_SOCKET is not set -- no capability broker is available in this session")
    req = Dict("id" => string(uuid4()), "category" => category, "params" => params)
    rec = get(XRAY, CURRENT_CALL[], nothing)
    asked = nothing
    if rec !== nothing
        asked = Dict{String, Any}("id" => req["id"], "category" => category, "approved" => nothing)
        push!(rec["capability_requests"], asked)
    end
    conn = connect(sock_path)
    # A broker that hangs mid-effect (a stuck Pkg.add, a network call that
    # never returns) must not hang the call forever. At `timeout_s` a Timer
    # closes the socket, which makes the blocked `readline` fail, as in
    # ShellEscape's timeout.
    timed_out = Ref(false)
    timer = Timer(timeout_s) do _
        if isopen(conn)
            timed_out[] = true
            close(conn)
        end
    end
    try
        write(conn, JSON.json(req) * "\n")
        line = readline(conn)
        timed_out[] && error("broker did not respond within timeout_s=$(timeout_s)s")
        isempty(line) && error("broker closed the connection without a response")
        resp = JSON.parse(line; dicttype=Dict{String, Any})
        asked === nothing || (asked["approved"] = get(resp, "approved", nothing))
        return resp
    catch e
        timed_out[] && error("broker did not respond within timeout_s=$(timeout_s)s")
        rethrow()
    finally
        close(timer)
        close(conn)
    end
end

"""
    Palette.fs_digest(paths::Vector{String}) -> Dict

Digest files with host privilege, as a check the sandbox cannot forge. The
request goes through the capability broker like any other: the broker reads
the paths OUTSIDE this sandbox, computes sha256 itself, and writes a receipt
on the host regardless of outcome. A digest returned here is therefore not
the kernel grading its own writes (kernel receipts die with the kernel;
host receipts do not). Broker-side ceiling shape:
`{"fs_digest": {"allowed_paths": ["/abs/dir", ...]}}`; a path outside every
allowed path denies the whole request (policy), while per-path filesystem
conditions (missing, directory, unreadable) come back as per-path
`error_class` entries.
"""
function fs_digest(paths::Vector{String})
    isempty(paths) && throw(ArgumentError("fs_digest requires a non-empty path list"))
    resp = request_capability("fs_digest", Dict{String,Any}("paths" => paths))
    get(resp, "approved", false) === true ||
        error("fs_digest was not allowed: $(get(resp, "reason", "no reason given"))")
    return resp["result"]
end

"""
    Palette.host_command(name, args...; wait=true, timeout_s=nothing) -> Dict

Run a command on the host without a shell. A configured alias uses its fixed
argv, working directory, timeout and `extra_args` policy. In operator mode,
an unregistered name resolves through host `PATH` or as a path and runs from
the current Julia working directory. Sandbox mode allows only configured
aliases. Synchronous direct operator commands default to 60 seconds; async
jobs default to 14,400 seconds. Direct commands accept `timeout_s` up to
14,400; an alias's configured timeout can only be shortened. `host_commands()`
lists configured aliases. Broker-side alias ceiling shape:
`{"host_command": {"commands": {"render": {"argv": ["/abs/prog", ...],
"cwd": "/abs/dir", "timeout_s": 600, "extra_args": false}}}}`. A command
with `"lock": "/abs/file"` runs under an exclusive flock on that file, shared
with work outside Palette (a GPU lock); the wait counts against its timeout.
`host_commands()` lists the commands this session may run.

`args` are read on the host. The kernel's `homedir()` (`/run/palette/home`)
and `tempdir()` exist only inside the sandbox, so a path built from them
names nothing the command can see; a path under a read root is the same on
both sides.

With `wait=true` returns `returncode`, `stdout`, `stderr` (their last 16
KiB; `stdout_skipped_bytes` counts what was dropped), `timed_out`,
`cancelled` and `seconds`. With `wait=false` returns at once with a `job`
id, for work longer than a turn: `host_command_poll(job)` returns the
output since the last poll while it runs, then the result;
`host_command_cancel(job)` kills it; `host_jobs()` lists the jobs held.

Pass argv as separate strings or an `AbstractVector{<:AbstractString}`;
vector elements, including an empty vector, are forwarded individually.
Completed results also include `stdout_tail` and `stderr_tail`, each with the
last 16 KiB of its full stream. Their `*_tail_skipped_bytes` fields report
bytes dropped before the retained tails. Async `stdout` and `stderr` remain
per-poll deltas.
"""
function host_command(name::AbstractString, args::AbstractString...; wait::Bool=true,
                      timeout_s::Union{Nothing,Real}=nothing)
    params = Dict{String,Any}("name" => String(name), "args" => collect(String, args),
                              "action" => wait ? "run" : "start", "cwd" => pwd())
    timeout_s === nothing || (params["timeout_s"] = Float64(timeout_s))
    # The broker enforces the command's deadline; this wait only has to
    # outlast it (14400 s is the most a ceiling may set).
    result = host_command_request(params, "host_command $(repr(name))";
                                  timeout_s=(timeout_s === nothing ? 14400.0 : Float64(timeout_s)) + 60.0)
    wait || (HOST_JOBS[result["job"]] = String(name))
    return result
end

host_command(name::AbstractString, args::AbstractVector{<:AbstractString}; kwargs...) =
    host_command(name, args...; kwargs...)

# Jobs this kernel started and has not seen finish, id => command name, for
# the turn's jobs line. The broker holds the jobs themselves; a revived
# kernel starts with this empty, and host_jobs() still lists them.
const HOST_JOBS = Dict{String, String}()

function host_command_request(params::Dict{String,Any}, what::String; timeout_s::Real=320.0)
    resp = request_capability("host_command", params; timeout_s)
    get(resp, "approved", false) === true ||
        error("$what was not allowed: $(get(resp, "reason", "no reason given"))")
    return resp["result"]
end

# The broker forgets a job once a poll returns its result, so the kernel keeps
# the last few results: a second poll of a finished job (a wait loop's own
# poll took the result) returns the same result instead of an error.
const HOST_JOB_RESULTS = Dict{String, Dict{String,Any}}()
const HOST_JOB_RESULT_ORDER = String[]
const HOST_JOB_RESULTS_KEPT = 16

function host_job_request(action::String, job::AbstractString)
    id = String(job)
    action == "poll" && haskey(HOST_JOB_RESULTS, id) && return HOST_JOB_RESULTS[id]
    result = host_command_request(Dict{String,Any}("action" => action, "job" => id), "host_command $action")
    if get(result, "running", false) !== true
        delete!(HOST_JOBS, id)
        HOST_JOB_RESULTS[id] = result
        push!(HOST_JOB_RESULT_ORDER, id)
        length(HOST_JOB_RESULT_ORDER) > HOST_JOB_RESULTS_KEPT && delete!(HOST_JOB_RESULTS, popfirst!(HOST_JOB_RESULT_ORDER))
    end
    return result
end

"""
    Palette.host_command_poll(job) -> Dict

For a `host_command(...; wait=false)` job: while it runs, `running => true`
with the `stdout` and `stderr` it wrote since the last poll; once it has
finished, its final delta and the bounded `stdout_tail` and `stderr_tail` of
the full streams. Tail skipped-byte fields report any output older than the
retained tails.
Polling a finished job again returns the same result (the kernel keeps the
last 16). `host_command_wait` blocks until the job finishes.
"""
host_command_poll(job::AbstractString) = host_job_request("poll", job)

"""
    Palette.host_command_wait(job; timeout_s=nothing, interval_s=5) -> Dict

Block until a `host_command(...; wait=false)` job finishes and return its
final delta and bounded full-stream tails. Returns the running
poll (`running => true`) if `timeout_s` passes first; the job keeps running.
Waiting counts against the call's own deadline, so give the call one longer
than the wait.
"""
function host_command_wait(job::AbstractString; timeout_s::Union{Nothing,Real}=nothing, interval_s::Real=5)
    id = String(job)
    haskey(HOST_JOB_RESULTS, id) && return HOST_JOB_RESULTS[id]
    t0 = time()
    while true
        # Listing does not consume output, so the result keeps all of it.
        held = [j for j in host_jobs(; timeout_s=60) if string(j["job"]) == id]
        isempty(held) && return host_job_request("poll", id)  # error names the missing job
        held[1]["running"] === true || return host_job_request("poll", id)
        timeout_s !== nothing && time() - t0 >= timeout_s && return host_job_request("poll", id)
        sleep(interval_s)
    end
end

"""
    Palette.host_command_cancel(job) -> Dict

Kill a `host_command(...; wait=false)` job's whole process group and return
its result. `cancelled` is false when the job had already finished.
"""
host_command_cancel(job::AbstractString) = host_job_request("cancel", job)

"""
    Palette.host_jobs(; timeout_s=320) -> Vector

The host_command jobs this session holds, longest-running first, each with
`job`, `name`, `running` and `seconds`. A finished job stays listed until
`host_command_poll` returns its result.
"""
host_jobs(; timeout_s::Real=320.0) =
    host_command_request(Dict{String,Any}("action" => "list"), "host_command list"; timeout_s)["jobs"]

"""
    Palette.host_commands(; timeout_s=320) -> Vector

The commands this session's ceiling lets `host_command` run, each with
`name`, `argv`, `cwd`, `timeout_s`, `extra_args` and, when set, `lock` and
`description`.
"""
host_commands(; timeout_s::Real=320.0) =
    host_command_request(Dict{String,Any}("action" => "commands"), "host_command commands"; timeout_s)["commands"]

#==============================================================================
PHASE 10: Integration Demo Helpers
==============================================================================#

"""
    demo_setup()

Sets up a demonstration environment.
"""
function demo_setup()
    reset_kernel_state()
    state = get_kernel_state()
    register_operator!(state, "demo_operator", args -> get(args, "echo", nothing))
    return state
end

"""
    run_demo()

Runs a simple, real demonstration of the operator surface end to end:
real eval-based execution, real state query, real discovery, and (if `sh`
is available) a real shell escape.
"""
function run_demo()
    println("=== Palette Demo ===\n")

    state = demo_setup()
    println("1. Kernel initialized with ID: $(state.id)\n")

    code_op = ExecuteCode("x = 42\ny = x * 2")
    receipt = execute(code_op)
    println("2. Executed code operation")
    println("   Receipt ID: $(receipt.id)")
    println("   Real result of last expression (y): $(receipt.result.data)")
    println("   Duration: $(receipt.duration_ms) ms\n")

    state_op = GetState()
    response = execute(state_op)
    println("3. Queried kernel state")
    println("   Variables: $(response.result.data["variables"])\n")

    invoke_op = InvokeOperator("demo_operator"; args=Dict("echo" => "hello from a real call"))
    invoke_receipt = execute(invoke_op)
    println("4. Invoked a real registered operator")
    println("   Result: $(invoke_receipt.result.data)\n")

    shell_op = ShellEscape("echo shell-escape-is-real")
    shell_receipt = execute(shell_op)
    println("5. Real shell escape")
    println("   stdout: $(get(shell_receipt.result.data, "stdout", nothing))\n")

    service = DiscoveryService()
    introspection = discover(service)
    println("6. Discovery results:")
    println("   Available operators: $(introspection.available_operators)")
    println("   Execution count: $(introspection.execution_stats["total"])\n")

    println("=== Demo Complete ===")

    return introspection
end

#==============================================================================
Exports
==============================================================================#

include("contexts.jl")
include("turn.jl")
include("revival.jl")
include("time_machine.jl")
include("provenance.jl")
include("digest.jl")
include("workspace_map.jl")
include("stress.jl")
include("session_requests.jl")
include("pharos.jl")
# The world binds `Palette` to `Api`, not this package, so the lighthouse has
# to be copied onto the facade. Function bodies already call Palette.Pharos;
# without this const, `isdefined(Palette, :Pharos)` is false in turn code.
Core.eval(Api, :(const Pharos = $(Pharos)))
include("tools.jl")
include("precompile_workload.jl")

export KernelState, ExecutionRecord
export get_kernel_state, reset_kernel_state
export OperatorType, OperatorVocabulary
export CODE_EXECUTION_VOCAB, OPERATOR_INVOCATION_VOCAB, STATE_QUERY_VOCAB
export ExecuteCode, InvokeOperator, GetState, ShellEscape, fs_digest, host_command, host_command_poll, host_command_wait, host_command_cancel, host_jobs, host_commands
export EphemeralTool, EphemeralToolViolation, check_ephemeral_source!, record_tool_capsule
export register_operator!
export OperationResult, StructuredResponse
export DiscoveryService, IntrospectionResult, discover
export OperationReceipt, ReceiptLog, get_receipts
export ErrorHandler, SafetyGuard, validate, check_patterns, safe_execute
export demo_setup, run_demo
export execute, mean
export request_capability
export host_request, runtime
export Api, Pharos, codegraph, codegraph!, definitions, callers, grep_defs
export bind_mmap, eval_isolated, depot_mode, enable_shared_depot!
export reload_methods!, sync_kernel_modules_to_main!, pin_cache!, get_cache, cache_get!, clear_cache!, bench, @bench
export pin, unpin, pinned, map_budgeted
export set_blas_threads!, get_blas_threads, serve_unix_socket, stop!, UnixSocketServerHandle
export private_lab_root, is_private_mode, check_private_isolation, configure_private_lab!
export @ephemeral, ephemeral!, unephemeral!, ephemerals, safe_summarysize
export isolate_module, reset_sandbox_module!, sandbox_modules, warntype_summary
export reinclude_module!, reset_bindings!, @alloc_barrier, warntype_summary_call

"""
    ephemeral_main(code)

Entry point of an ephemeral child worker. Runs `code` as a persistent turn
would and prints one JSON line last: success, data, display and error. The
display is the same `text/plain` rendering a persistent turn returns, and
`data` falls back to it when the value has no JSON form (NaN, a function),
so such a value does not crash the child after the code succeeded.
"""
function ephemeral_main(code::AbstractString; payload=nothing, timeout_s=nothing, startup_seconds=nothing)
    redirect_stderr(stdout)
    mod = get_kernel_state().eval_module
    Core.eval(mod, Expr(:global, Expr(:(=), :PAYLOAD, QuoteNode(payload))))
    (receipt, interrupted, stuck), output = execute_turn(String(code), timeout_s === nothing ? nothing : Float64(timeout_s))
    print(task_notice_hint(output))
    projection = receipt isa OperationReceipt ? get(receipt.metadata, "transport", Dict()) : Dict()
    success = receipt isa OperationReceipt && receipt.result.success && !interrupted && !stuck
    error = receipt isa OperationReceipt ? receipt.result.error : "child interrupted during $(get(TURN_COSTS[1], "phase", "unknown"))"
    result = Dict{String, Any}("success" => success, "data" => get(projection, "data", nothing),
        "display" => get(projection, "display", nothing), "error" => error, "costs" => deepcopy(TURN_COSTS[1]))
    result["costs"]["child_import_seconds"] = startup_seconds
    # The existing capability broker retains only the final 4000 characters.
    # Keep the final frame within that contract instead of losing its JSON start.
    writer = DisplayWriter(UInt8[], 3500)
    line = try
        JSON.print(writer, result)
        valid_utf8(String(writer.bytes))
    catch e
        e isa DisplayLimit || rethrow()
        display = get(projection, "display", nothing)
        text = display === nothing ? "<scratch result exceeds the existing broker transport limit>" :
            first(display, 200) * "\n[scratch response shortened to the existing broker transport limit]"
        JSON.json(Dict("success" => success, "data" => text, "display" => nothing,
            "error" => error === nothing ? nothing : first(error, 150), "transport_shortened" => true))
    end
    println()
    println(line)
end

end # module
