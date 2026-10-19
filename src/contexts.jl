# Execution contexts. A call runs in exactly one: the request's `context`, or
# the session default set with `Palette.context!`, or `normal`.
#
#   normal     today's behaviour
#   traced     each top-level statement's outermost call records the method it
#              dispatched to (Palette.xray(n)["dispatch"])
#   reference  registered reference implementations (Palette.reference!)
#              replace the fast ones for the call, inside functions too
#   memoized   each top-level statement's outermost call is served from a small
#              cache; a hit skips the call's side effects
#   budgeted   the call is interrupted past a time or allocation budget

const CONTEXT_NAMES = ("normal", "traced", "reference", "memoized", "budgeted")

# The context of the call in flight, and the session default. A context is
# {"name", "source" => "call" | "session default", "budget" => {"seconds", "bytes"} | nothing};
# nothing means normal.
const CALL_CONTEXT = Ref{Union{Nothing, Dict{String, Any}}}(nothing)
const SESSION_CONTEXT = Ref{Union{Nothing, Dict{String, Any}}}(nothing)

positive_real(x) = x isa Real && !(x isa Bool) && x > 0

function context_spec(name, budget, source::String)
    name isa AbstractString && name in CONTEXT_NAMES ||
        error("unknown context $(repr(name)): expected one of $(join(CONTEXT_NAMES, ", "))")
    if name != "budgeted"
        budget === nothing || error("a budget applies only to the budgeted context, not $(repr(name))")
        return Dict{String, Any}("name" => String(name), "source" => source, "budget" => nothing)
    end
    budget isa AbstractDict || error("the budgeted context needs a budget: {\"seconds\": s, \"bytes\": n}")
    extra = setdiff(keys(budget), ("seconds", "bytes"))
    isempty(extra) || error("unknown budget field(s) $(join(sort!(collect(extra)), ", ")): a budget has seconds and/or bytes")
    seconds = get(budget, "seconds", nothing)
    bytes = get(budget, "bytes", nothing)
    seconds === nothing || positive_real(seconds) || error("budget seconds must be a positive number")
    bytes === nothing || bytes isa Integer && positive_real(bytes) || error("budget bytes must be a positive integer")
    seconds === nothing && bytes === nothing && error("a budget needs seconds and/or bytes")
    return Dict{String, Any}("name" => "budgeted", "source" => source,
                             "budget" => Dict{String, Any}("seconds" => seconds, "bytes" => bytes))
end

"""
The context an EXECUTE request runs in: its own `context` (and `budget`), else
the session default. `normal` in the request overrides a default for one call.
"""
function select_context(req)
    name = get(req, "context", nothing)
    budget = get(req, "budget", nothing)
    name === nothing && budget !== nothing && error("a budget applies only to the budgeted context: send context \"budgeted\" with it")
    name === nothing && return SESSION_CONTEXT[]
    ctx = context_spec(name, budget, "call")
    return ctx["name"] == "normal" && SESSION_CONTEXT[] === nothing ? nothing : ctx
end

context_name(ctx) = ctx === nothing ? "normal" : ctx["name"]

# The reply's `context`: this call's context, and the session default while one
# is set (including one this call set or cleared). Nothing for a plain normal call.
function context_note(ctx)
    default = SESSION_CONTEXT[]
    from_default = ctx !== nothing && ctx["source"] == "session default"
    call = from_default ? "$(ctx["name"]) (session default)" : context_name(ctx)
    if default === nothing
        from_default && return "$call; the session default is cleared from the next call"
        return call == "normal" ? nothing : call
    end
    from_default && default === ctx && return call
    return "$call; session default from the next call: $(default["name"]); Palette.context!(nothing) clears it"
end

"""
    Palette.context!(name; seconds=nothing, bytes=nothing)
    Palette.context!(nothing)

Sets the session's default execution context from the next call on (`traced`,
`reference`, `memoized`, `budgeted` with `seconds` and/or `bytes`, or `normal`);
`nothing` clears it. Every reply reports the default while it is set. A
request's own `context` wins for that call. The default is not saved across a
kernel revival.
"""
function context!(name::Union{Symbol, AbstractString, Nothing}; seconds=nothing, bytes=nothing)
    if name === nothing
        seconds === nothing && bytes === nothing || error("context!(nothing) takes no budget")
        SESSION_CONTEXT[] = nothing
        return nothing
    end
    budget = seconds === nothing && bytes === nothing ? nothing :
        Dict{String, Any}(k => v for (k, v) in ("seconds" => seconds, "bytes" => bytes) if v !== nothing)
    ctx = context_spec(String(name), budget, "session default")
    SESSION_CONTEXT[] = ctx["name"] == "normal" ? nothing : ctx
    return ctx["name"]
end

function start_xray_record!(call::Int, ctx)
    rec = XRAY[call] = Dict{String, Any}("capability_requests" => Dict{String, Any}[])
    retain!(XRAY, call)
    name = context_name(ctx)
    name == "traced" && (rec["dispatch"] = Dict{String, Any}[])
    name == "memoized" && (rec["memo"] = Dict{String, Any}("hits" => 0, "misses" => 0, "uncached" => 0))
    name == "reference" && (rec["references"] = Dict{String, Any}[])
    return nothing
end

function context_record(key)
    rec = get(XRAY, CURRENT_CALL[], nothing)
    return rec === nothing ? nothing : get(rec, key, nothing)
end

# --- Rewriting a top-level statement's outermost call (traced, memoized) -----

# Calls that must stay syntactic: wrapping them would change what they mean.
const UNWRAPPABLE = (:ccall, :cglobal, :llvmcall, :foreigncall)

plain_target(ex) = ex isa Symbol ||
    ex isa Expr && (ex.head in (:ref, :.) || ex.head === :tuple && all(plain_target, ex.args) ||
                    ex.head === :(::) && length(ex.args) == 2 && ex.args[1] isa Symbol)

function wrap_outer_call(ex, wrapper::GlobalRef)
    ex isa Expr || return ex
    if ex.head === :(=) && length(ex.args) == 2 && plain_target(ex.args[1])
        return Expr(:(=), ex.args[1], wrap_outer_call(ex.args[2], wrapper))
    elseif ex.head in (:const, :global, :local, :return) && length(ex.args) == 1
        return Expr(ex.head, wrap_outer_call(ex.args[1], wrapper))
    end
    ex.head === :call && !isempty(ex.args) || return ex
    f = ex.args[1]
    f in UNWRAPPABLE && return ex
    # a .+ b broadcasts; f.(x) is not a :call at all.
    f isa Symbol && startswith(string(f), '.') && length(string(f)) > 1 && return ex
    if length(ex.args) >= 2 && ex.args[2] isa Expr && ex.args[2].head === :parameters
        return Expr(:call, wrapper, ex.args[2], f, ex.args[3:end]...)
    end
    return Expr(:call, wrapper, f, ex.args[2:end]...)
end

"""
The statement as the call's context evaluates it. In traced and memoized
contexts the outermost call (or an assignment's right-hand side) goes through
`trace_dispatch` / `memo_call`; definitions, macro calls, broadcasts and other
statements run unchanged. Provenance keeps seeing the statement as written.
"""
function context_rewrite(ex)
    name = context_name(CALL_CONTEXT[])
    wrapper = name == "traced" ? :trace_dispatch : name == "memoized" ? :memo_call : nothing
    wrapper === nothing && return ex
    ref = GlobalRef(@__MODULE__, wrapper)
    if ex isa Expr && ex.head in (:toplevel, :block)
        return Expr(ex.head, (context_rewrite(a) for a in ex.args)...)
    end
    return wrap_outer_call(ex, ref)
end

function dispatched_method(f, args)
    try
        return which(f, Base.typesof(args...))
    catch
        return nothing   # no method, or ambiguous: the call itself reports it
    end
end

function trace_dispatch(f, args...; kw...)
    log = context_record("dispatch")
    m = log === nothing ? nothing : dispatched_method(f, args)
    if m !== nothing
        push!(log, Dict{String, Any}("function" => f isa Function ? string(nameof(f)) : string(typeof(f)),
            "signature" => string(Base.typesof(args...)), "method" => sprint(show, m),
            "module" => string(m.module), "file" => string(m.file), "line" => Int(m.line)))
    end
    return f(args...; kw...)
end

# --- memoized ----------------------------------------------------------------

const MEMO_CAPACITY = 64
# key => (value, last-use tick). A hit bumps the tick in place: storing again
# would replace the key's argument copy with the caller's own arguments.
const MEMO = Dict{Any, Tuple{Some, Base.RefValue{Int}}}()
const MEMO_TICK = Ref(0)
const FINGERPRINT = Ref{Tuple{UInt, Tuple{UInt, Int, Int}}}((typemax(UInt), (UInt(0), 0, 0)))

"""
What the method table looks like: the newest definition, the method count and
the number of deleted methods. It changes when any method is defined, replaced
or deleted. Every top-level statement moves the world counter, so the scan
(about 10 ms) is cached per world, not keyed on it.
"""
function definitions_fingerprint()
    world = Base.get_world_counter()
    FINGERPRINT[][1] == world && return FINGERPRINT[][2]
    newest = Ref(UInt(0)); count = Ref(0); deleted = Ref(0)
    Base.visit(Core.methodtable) do m
        count[] += 1
        newest[] = max(newest[], m.primary_world)
        m.dispatch_status == 0 && (deleted[] += 1)
    end
    fp = (newest[], count[], deleted[])
    FINGERPRINT[] = (world, fp)
    return fp
end

kernel_owned(mod::Module) = Base.moduleroot(mod) in (Core, Base, @__MODULE__)

"""
Serves `f(args...; kw...)` from the memo cache when the same function, equal
arguments and the same method definitions were seen before. Not cached: calls
to Core, Base or Palette methods (printing, mutation and kernel state must
happen), `nothing` results, and arguments or values that cannot be deep-copied.
Redefined constants and non-constant globals are not tracked.
"""
function memo_call(f, args...; kw...)
    stats = context_record("memo")
    m = dispatched_method(f, args)
    key = hit = nothing
    if m !== nothing && !kernel_owned(m.module)
        # Look up with the arguments as given; copy them only to store a miss,
        # and before the call, which may mutate them.
        key, hit = try
            k = (f, args, values(kw), definitions_fingerprint(), m)
            h = get(MEMO, k, nothing)
            h === nothing ? ((f, deepcopy(args), deepcopy(values(kw)), k[4], m), nothing) : (k, h)
        catch
            nothing, nothing
        end
    end
    if key === nothing
        stats === nothing || (stats["uncached"] += 1)
        return f(args...; kw...)
    end
    MEMO_TICK[] += 1
    if hit !== nothing
        stats === nothing || (stats["hits"] += 1)
        hit[2][] = MEMO_TICK[]
        return deepcopy(something(hit[1]))
    end
    stats === nothing || (stats["misses"] += 1)
    value = f(args...; kw...)
    value === nothing && return value
    stored = try Some(deepcopy(value)) catch; nothing end
    stored === nothing && return value
    if length(MEMO) >= MEMO_CAPACITY
        delete!(MEMO, argmin(k -> MEMO[k][2][], keys(MEMO)))
    end
    MEMO[key] = (stored, Ref(MEMO_TICK[]))
    return value
end

# --- reference ---------------------------------------------------------------

# (f, signature) => implementation. Kernel state: not saved across a revival.
const REFERENCES = Dict{Tuple{Any, Type}, Any}()

"""
    Palette.reference!(f, Tuple{T1, ...}, impl)

Registers `impl` as the reference implementation of `f` for that signature.
In the `reference` context it replaces `f`'s method for the call's duration,
for calls made inside other functions too, and the fast method is back
afterwards. Functions owned by Core, Base or Palette are refused: the kernel
itself runs on them during the call. Registrations are not saved across a
kernel revival.
"""
function reference!(f, sig::Type{<:Tuple}, impl)
    f isa Function || error("reference! takes a function, not a $(typeof(f))")
    kernel_owned(parentmodule(f)) && error("$(f) belongs to $(Base.moduleroot(parentmodule(f))): the kernel runs on it, so it cannot be replaced")
    sig isa DataType && !any(T -> T isa Core.TypeofVararg, sig.parameters) ||
        error("reference! needs a fixed-arity signature such as Tuple{Vector{Float64}}, not $sig")
    hasmethod(impl, sig) || error("the reference implementation has no method for $sig")
    REFERENCES[(f, sig)] = impl
    return references()
end

"""
    Palette.references() -> Vector{Dict}

The registered reference implementations (`reference!`).
"""
references() = [Dict{String, Any}("function" => string(f), "signature" => string(sig), "implementation" => string(impl))
                for ((f, sig), impl) in sort!(collect(REFERENCES); by=kv -> string(kv.first))]

"""
    Palette.forget_reference!(f, Tuple{T1, ...}) -> Bool

Removes a registration; false when there was none.
"""
forget_reference!(f, sig::Type{<:Tuple}) = pop!(REFERENCES, (f, sig), nothing) !== nothing

function install_reference(f, sig, impl)
    names = [Symbol("a", i) for i in 1:length(sig.parameters)]
    head = Expr(:call, Expr(:(::), typeof(f)), (Expr(:(::), n, T) for (n, T) in zip(names, sig.parameters))...)
    replaced = hasmethod(f, sig) && which(f, sig).sig == Tuple{typeof(f), sig.parameters...}
    Core.eval(parentmodule(f), Expr(:(=), head, Expr(:call, impl, names...)))
    return Base.invokelatest(which, f, sig), replaced
end

"""
Runs `body()` with the reference implementations in place when `ctx` is the
reference context, and takes them out again however `body` ends. Deleting a
method brings back the one it replaced.
"""
function with_context(body, ctx)
    context_name(ctx) == "reference" || return body()
    log = context_record("references")
    installed = Tuple{Any, Type, Method}[]
    try
        for ((f, sig), impl) in REFERENCES
            m, replaced = install_reference(f, sig, impl)
            push!(installed, (f, sig, m))
            log === nothing || push!(log, Dict{String, Any}("function" => string(f), "signature" => string(sig),
                "implementation" => string(impl), "replaced_method" => replaced))
        end
        return body()
    finally
        for (f, sig, m) in installed
            # The call redefined that method itself: it is the call's now, leave it.
            Base.invokelatest(which, f, sig) === m && Base.delete_method(m)
        end
    end
end
