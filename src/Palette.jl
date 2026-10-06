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
using Base.CoreLogging: AbstractLogger, with_logger, current_logger
using Statistics: mean as stdlib_mean

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
        # `Core.eval`'d code lands in this fresh module, not Main -- `using
        # Palette` at Main scope (e.g. in runtime/scripts/session_loop.jl) does not
        # make `Palette` visible here (confirmed by direct testing: a real
        # UndefVarError for `Palette` from inside eval_module otherwise).
        # Bound once here so turn code can call
        # `Palette.request_capability(...)` without re-importing it on every
        # single turn just to reach the one function that's the entire
        # point of the authority fence.
        Core.eval(state.eval_module, :(const Palette = $(Api)))
        Core.eval(state.eval_module, :(const Api = $(Api)))
        Core.eval(state.eval_module, :(const Main = $(state.eval_module)))
        # A bare `Module(...)` never gets `include` for free (confirmed by
        # direct testing across all constructor flag combinations -- this
        # is not a Julia version regression, it never worked the way an
        # earlier comment in this file assumed). Needed so promoted, kit-
        # scope code (written to disk via a real broker-mediated
        # external_fs_write, then loaded back with `include`) can actually
        # be loaded into the persistent mind's own module, not just Main.
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
        Core.eval(state.eval_module, :(const ShellResult = $(ShellResult)))
        # The InteractiveUtils varinfo() lists Main, not this module, and is not
        # loaded here at all; this one lists the session's own bindings.
        Core.eval(state.eval_module, :(varinfo() = $(varinfo)(@__MODULE__)))
        Core.eval(state.eval_module, :(kernelinfo() = $(kernelinfo)()))
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

"""
    reset_kernel_state()

Resets the kernel state (useful for testing or clean restarts).
"""
function reset_kernel_state()
    GLOBAL_STATE[] = nothing
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
function run_bash(script::AbstractString)
    # Files, not pipes: a command that puts a job in the background (`job &`)
    # hands it the pipe, and reading a pipe to its end waited for the job,
    # up to the call's time limit, which then killed it. Only bash is waited
    # for; what the job writes later is reported at the start of a later call.
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
    return ShellResult(p.termsignal > 0 ? 128 + p.termsignal : p.exitcode, o, e)
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
        Base.invokelatest(isdefined, mod, sym) || continue
        value = Base.invokelatest(getglobal, mod, sym)
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

function retain_output!(call::Int, text::String)
    isempty(text) || (OUTPUTS[call] = text)
    filter!(kv -> kv.first > call - KEEP_OUTPUTS, OUTPUTS)
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
authority. The whole package used to be bound there, so a model exploring it
found `reset_kernel_state()`, which silently discarded every binding, and
`discover()`, which listed operators that do not exist.
"""
module Api
import ..Palette
host_request(rtype::AbstractString, payload::AbstractDict=Dict{String,Any}()) = Palette.host_request(rtype, payload)
runtime(action::AbstractString, payload::AbstractDict=Dict{String,Any}()) = Palette.runtime(action, payload)
const rlm = Palette.rlm
request_capability(category::String, params::Dict=Dict{String,Any}(); kwargs...) =
    Palette.request_capability(category, params; kwargs...)
host_command(name::AbstractString, args::AbstractString...; kwargs...) = Palette.host_command(name, args...; kwargs...)
host_command_poll(job::AbstractString) = Palette.host_command_poll(job)
fs_digest(paths::Vector{String}) = Palette.fs_digest(paths)
reloads() = deepcopy(Palette.RELOAD_REPORTS)
revival() = deepcopy(Palette.REVIVAL_OBSERVATION[])
jobs() = Palette.task_observations()
taskinfo(t::Task) = Palette.task_observation(t)
environment(name::AbstractString) = Palette.package_environment(name)
provenance(name::Symbol; strong::Bool=false) = Palette.binding_provenance(name; strong)
world() = Palette.get_kernel_state().eval_module
worldinfo() = Dict("module" => string(nameof(world())), "epoch" => Palette.TASK_EPOCH[], "main_is_world" => getglobal(world(), :Main) === world(), "host_main_is_world" => world() === Main)
read_artifact(path::AbstractString; kwargs...) = Palette.read_artifact(path; kwargs...)
changes(; acknowledge::Bool=false) = Palette.changes_since_ack(; acknowledge)
costs() = deepcopy(Palette.TURN_COSTS)
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
const KERNEL_BINDINGS = (:eval, :include, :bash, Symbol("@sh_str"), :Palette, :Api, :ShellResult,
                         :varinfo, :kernelinfo, :ans, :PAYLOAD, :Main)

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

function _summary(v)
    v isa Function && return "function, $(length(methods(v))) method(s)"
    v isa Type && return "type"
    v isa Module && return "module"
    return try
        summary(v)
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
        (sym === nameof(mod) || sym in KERNEL_BINDINGS || startswith(string(sym), '#')) && continue
        Base.invokelatest(isdefined, mod, sym) || continue
        v = Base.invokelatest(getglobal, mod, sym)
        size = try
            Base.format_bytes(Base.summarysize(v))
        catch
            "?"
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
    offline = get(ENV, "JULIA_PKG_OFFLINE", "") == "true"
    nbind = length(varinfo(mod).rows)
    println("""
        Palette kernel: Julia $(VERSION), $(Threads.nthreads()) thread(s), $(nbind) binding(s), $(Base.format_bytes(Base.gc_live_bytes())) live
        workspace  $(pwd()) (read/write; files outlive the kernel)
        persists   bindings, functions, types and loaded packages, until the kernel stops
        time limit $(isempty(timeout) ? "set by the host" : timeout * "s") per call. Waiting work (sleep, run, read, I/O) is interrupted and the kernel keeps its state;
                   compute that never yields cannot be interrupted, so the kernel stops and its bindings are lost
        network    $(offline ? "none; Pkg.add cannot install packages" : "available")
        packages   loadable: Julia stdlib, $(join(deps, ", "))
                   loaded: $(isempty(loaded) ? "none" : join(loaded, ", "))
        helpers    bash("cmd") or sh"cmd" -> ShellResult(exitcode, stdout, stderr); sh"..." keeps \$ for the shell
                   PAYLOAD: this call's payload text, raw; write(path, PAYLOAD) writes a file with no Julia quoting
                   ans: the last call's value.  varinfo(): your bindings.  include("file.jl") loads a workspace file
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
    boundary = findfirst(f -> f.func === :execute && endswith(string(f.file), "Palette.jl"), frames)
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
    call_file = "call $(length(state.execution_history) + 1)"

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
        # World-age note: Core.eval above ran in a compiled function, so it
        # defines eval_module's new bindings in a NEW world age -- plain
        # names/isdefined/getfield here would still see the OLD world and
        # miss bindings that unquestionably exist (confirmed by direct
        # testing, not assumed). Base.invokelatest forces resolution
        # against the current world.
        own_name = nameof(state.eval_module)
        for sym in Base.invokelatest(names, state.eval_module; all=true)
            # :Palette is the constant KernelState's constructor binds here so
            # turn code can reach `Palette.request_capability` without
            # re-importing it every turn -- not a user variable, must not
            # show up in GetState/discovery any more than :eval or :include do.
            (sym === own_name || sym in KERNEL_BINDINGS) && continue
            startswith(string(sym), '#') && continue
            if Base.invokelatest(isdefined, state.eval_module, sym)
                state.variables[string(sym)] = Base.invokelatest(getfield, state.eval_module, sym)
            end
        end
    catch e
        # invokelatest: rendering a frame of a function this call defined
        # reads its binding, and from this function's older world age Julia
        # 1.12 warned "access to binding ... in a world prior to its definition".
        error_msg = Base.invokelatest(describe_error, e, catch_backtrace(), call_file, state.eval_module)
        result = OperationResult(nothing, false, error_msg)
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
        state.id
    )
    # The log keeps the outcome, not the value: holding every call's value
    # kept each unassigned result alive for the whole session (two 400 MB
    # results left 800 MB that GC could never reclaim).
    push!(state.receipt_log, OperationReceipt(receipt.id, receipt.timestamp, receipt.operation_type,
                                              OperationResult(nothing, result.success, result.error),
                                              duration_ms, state.id))

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
the process the instant it's evaluated -- confirmed by direct testing:
`Core.eval` of `function Base.show(io, x::Int) ... end` into a brand new,
otherwise-unrelated module still adds a real, permanent method to
`Base.show`, with no `import` needed at all.
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

1. Any `import` statement. `using Mod: f` alone is already blocked by
   Julia itself for bare-name extension (confirmed by direct testing --
   "function Base.show must be explicitly imported to be extended") --
   but `import Mod: f` followed by a bare `function f(...)` succeeds and
   is exactly the pattern that needs to be stopped before eval, since
   Julia's own runtime won't stop it once `import` has run.
2. Any qualified function definition head (`function Mod.name(...)`),
   which extends `Mod`'s generic function directly, no `import` required
   at all -- also confirmed by direct testing.
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
    # reachable path to request_capability.
    Core.eval(tool_module, :(const Palette = $(@__MODULE__)))
    # Deliberately NOT given an `include` binding, unlike the persistent
    # eval_module: check_ephemeral_source! only inspects the source string
    # passed directly to EphemeralTool -- it cannot see inside a file that
    # code would dynamically `include` at runtime, so granting `include`
    # here would be a silent bypass of the whole namespace-pollution guard
    # (load a file containing `import Base: show; function show(...)`, and
    # the guard never looked at it). Kit code loads into the durable mind
    # deliberately, not into disposable ephemeral tool code.

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
    push!(kernel_state.receipt_log, receipt)
    record_tool_capsule(op.tool_id, op.code, result, duration_ms)

    return receipt
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
    dir = get(ENV, "PALETTE_PROVENANCE_DIR", joinpath(pwd(), ".palette"))
    mkpath(dir)
    capsule = Dict{String,Any}(
        "schema" => "palette.tool_capsule.v1",
        "tool_id" => tool_id,
        "source_hash" => bytes2hex(sha256(code)),
        "source" => code,
        "success" => result.success,
        "error" => result.error,
        "duration_ms" => duration_ms,
        "retired_at" => string(Dates.now()),
        # Reflects the real outcome -- confirmed by direct testing that
        # this was previously hardcoded to "RETIRED" regardless of
        # result.success, so a failed ephemeral tool's capsule looked
        # indistinguishable from a successful one to anything reading the
        # provenance log later.
        "disposition" => result.success ? "RETIRED" : "FAILED",
    )
    open(joinpath(dir, "provenance.jsonl"), "a") do io
        println(io, JSON.json(capsule))
    end
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
    push!(state.receipt_log, receipt)

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
            # signal-killed command (found by FM-RQ-A1 probe P13; run_bash
            # already mapped this correctly).
            exit_code = proc.termsignal > 0 ? 128 + proc.termsignal : proc.exitcode
            error_class = exit_code == 0 ? nothing :
                          proc.termsignal > 0 ? "signal_death" : "clean_failure"
            data = op.capture_output ? begin
                out = String(take!(out_buf)); err = String(take!(err_buf))
                # Output digests (FM-SLICE-A1): observer is the kernel itself
                # (self-attested — the host-side independent eye is A2). They
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
        # class instead of prose-only errors (FM-SLICE-A1).
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
    push!(state.receipt_log, receipt)

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

Enforces safety constraints on operations. NOTE: this is an
allowlist/pattern-blocklist toy, not the authority fence -- see the
repository README's Authority philosophy section. It exists to demonstrate
failure containment (Phase 9), not to be relied on as a security boundary.
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
operation's actual type against a `Set{DataType}` -- this used to stringify
`typeof(op)` and look it up in a `Set{String}`, which is duck-typing
grafted onto a language whose type system already answers "is this op one
of the allowed kinds" directly, without an allocation or a name-collision
risk between unrelated types that happen to share a short name.
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
by `runtime/security/launch_worker.py`, reachable only because that one socket path
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
    conn = connect(sock_path)
    # `timeout_s` was accepted but never referenced anywhere in this
    # function -- confirmed by reading it, not assumed: a broker that
    # hung mid-effect (a stuck Pkg.add, a network call that never returns)
    # would hang this call, and therefore this whole worker/turn, forever,
    # with no way for the documented parameter to prevent it. Same
    # Timer-based cancellation pattern already used for ShellEscape's
    # timeout: closing the socket while `readline` blocks on it forces
    # that read to error out instead of hanging indefinitely.
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
        return JSON.parse(line; dicttype=Dict{String, Any})
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

Digest files with host privilege — the independent eye (FM-SLICE-A2). The
request goes through the capability broker like any other: the broker reads
the paths OUTSIDE this sandbox, computes sha256 itself, and writes a receipt
on the host regardless of outcome. A digest returned here is therefore not
the kernel grading its own writes (probe P15: kernel receipts die with the
kernel; host receipts do not). Broker-side ceiling shape:
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
    Palette.host_command(name, args...; wait=true, timeout_s=14460) -> Dict

Run a command the operator named in this session's ceiling, on the host,
outside the sandbox: the way a kernel reaches a GPU, a renderer or a build
it cannot run itself. The ceiling fixes the command's program, working
directory and timeout; `args` are passed only to a command whose ceiling
entry sets `extra_args`. Never a shell. Broker-side ceiling shape:
`{"host_command": {"commands": {"render": {"argv": ["/abs/prog", ...],
"cwd": "/abs/dir", "timeout_s": 600, "extra_args": false}}}}`.

With `wait=true` returns `returncode`, `stdout`, `stderr` (their last 16
KiB), `timed_out` and `seconds`. With `wait=false` returns at once with a
`job` id, for work longer than a turn; `host_command_poll(job)` returns
`running => true` until the result.
"""
function host_command(name::AbstractString, args::AbstractString...; wait::Bool=true, timeout_s::Real=14460.0)
    params = Dict{String,Any}("name" => String(name), "args" => collect(String, args),
                              "action" => wait ? "run" : "start")
    resp = request_capability("host_command", params; timeout_s)
    get(resp, "approved", false) === true ||
        error("host_command $(repr(name)) was not allowed: $(get(resp, "reason", "no reason given"))")
    return resp["result"]
end

"""
    Palette.host_command_poll(job) -> Dict

The result of a `host_command(...; wait=false)` job once it has finished,
or `running => true` while it runs.
"""
function host_command_poll(job::AbstractString)
    resp = request_capability("host_command", Dict{String,Any}("action" => "poll", "job" => String(job)))
    get(resp, "approved", false) === true ||
        error("host_command poll was not allowed: $(get(resp, "reason", "no reason given"))")
    return resp["result"]
end

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

include("turn.jl")
include("revival.jl")
include("provenance.jl")
include("digest.jl")
include("workspace_map.jl")
include("stress.jl")
include("precompile_workload.jl")

export KernelState, ExecutionRecord
export get_kernel_state, reset_kernel_state
export OperatorType, OperatorVocabulary
export CODE_EXECUTION_VOCAB, OPERATOR_INVOCATION_VOCAB, STATE_QUERY_VOCAB
export ExecuteCode, InvokeOperator, GetState, ShellEscape, fs_digest, host_command, host_command_poll
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

"""
    ephemeral_main(code)

Entry point of an ephemeral child worker. Runs `code` as a persistent turn
would and prints one JSON line last: success, data, display and error. The
display is the same `text/plain` rendering a persistent turn returns, and
`data` falls back to it when the value has no JSON form (NaN, a function),
which used to crash the child after the code had already succeeded.
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
