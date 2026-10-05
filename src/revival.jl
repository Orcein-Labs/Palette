# Revival: what a new kernel can truthfully bring back of the one before it.
#
# After each completed call, while the model reads the reply, the kernel
# writes a snapshot of its state to the host's state directory. When a kernel
# stops (a call that never yields, a crash, a harness restart) the next kernel
# revives from the last snapshot, which is the state at the end of the last
# call that completed:
#
# - definitions (functions, methods, types, macros, `using`/`import`) are
#   rebuilt by evaluating their source again, never deserialized, and then
#   checked against what the old kernel had;
# - data bindings are deserialized, in groups that keep shared references
#   shared, only when Julia, the packages and every user type they use are
#   unchanged;
# - closures and anonymous functions are data too: Serialization carries a
#   closure's own type and method, and its captured values are checked like
#   any struct's fields;
# - everything else (tasks, processes, IO, pointers, values built from an
#   earlier version of a type) is reported lost, with the code that made it.
#
# Nothing is reported restored unless it is the same value or the same
# definition. Anything uncertain is reported as such, or as lost.

using Serialization

const STATE_DIR = Ref("")
const REVIVAL_FORMAT = 2
# Per binding (or group of bindings sharing data) and per snapshot; the
# environment can lower them, as the tests do.
max_binding_bytes() = parse(Int, get(ENV, "PALETTE_STATE_MAX_BINDING_BYTES", string(16 * 1024 * 1024)))
max_snapshot_bytes() = parse(Int, get(ENV, "PALETTE_STATE_MAX_BYTES", string(128 * 1024 * 1024)))
const MAX_RECIPE_CHARS = 240

# Definition statements from every call, in order: what a new kernel
# evaluates again. Whole calls are never replayed, only these forms.
const DEFINITION_LOG = Vector{Dict{String, Any}}()
# Files each call named (see note_file!), so a restored value computed from a
# file that has since changed can be reported stale.
const CALL_FILES = Dict{Int, Vector{Tuple{String, Tuple{Float64, Int}}}}()
# ENV as the kernel found it; a snapshot records what calls changed.
const INITIAL_ENV = Ref{Dict{String, String}}(Dict{String, String}())
# What the first call after a revival is told.
const REVIVAL_REPORT = Ref("")
const REVIVAL_OBSERVATION = Ref{Dict{String, Any}}(Dict("state" => "fresh_empty_world", "states" => ["fresh_empty_world"]))

function revival_observation!(; restored=String[], rebuilt=String[], stale=String[], lost=String[],
                               uncertain=String[], saved_call=nothing, text="")
    positive = !isempty(restored) || !isempty(rebuilt)
    attention = !isempty(stale) || !isempty(lost) || !isempty(uncertain)
    state = positive && attention ? "partially_restored" : !isempty(uncertain) ? "uncertain" :
            !isempty(lost) ? "lost" : !isempty(stale) ? "stale" : positive ? "restored" : "fresh_empty_world"
    states = unique([state; positive ? ["restored"] : String[]; isempty(stale) ? String[] : ["stale"];
                     isempty(lost) ? String[] : ["lost"]; isempty(uncertain) ? String[] : ["uncertain"]])
    REVIVAL_OBSERVATION[] = Dict{String, Any}("state" => state, "states" => states, "saved_call" => saved_call,
        "restored" => restored, "rebuilt" => rebuilt, "stale" => stale, "lost" => lost,
        "uncertain" => uncertain, "text" => text)
    return text
end

function revival_summary()
    report = REVIVAL_OBSERVATION[]
    summary = Dict{String, Any}(k => get(report, k, nothing) for k in ("state", "states", "saved_call"))
    summary["counts"] = Dict(k => length(get(report, k, String[])) for k in ("restored", "rebuilt", "stale", "lost", "uncertain"))
    for key in ("stale", "lost", "uncertain")
        summary[key] = [first(s, 240) for s in first(get(report, key, String[]), 6)]
    end
    summary["details"] = "Palette.revival()"
    return summary
end

# --- definitions ---------------------------------------------------------

const DEFINITION_MACROS = ("@kwdef", "Base.@kwdef", "@inline", "@noinline", "@enum", "Base.@enum", "@doc",
                           "Core.@doc", "@__doc__", "@static")

function macro_name(ex::Expr)
    ex.head === :macrocall || return ""
    m = ex.args[1]
    m isa GlobalRef && return "$(m.mod).$(m.name)"
    m isa Expr && m.head === :. && m.args[2] isa QuoteNode && return "$(m.args[1]).$(m.args[2].value)"
    return string(m)
end

is_signature(ex) = ex isa Expr && (ex.head === :call || (ex.head in (:where, :(::)) && is_signature(ex.args[1])))

"""
    definition_kind(ex) -> String or nothing

`"def"` for a function, method, macro or type definition, `"using"` for
`using`/`import`, `"const"` for a constant, `"include"` for `include("…")` of
a literal path, and `nothing` for anything else. Only these forms are ever
evaluated again.
"""
function definition_kind(ex)
    ex isa Expr || return nothing
    h = ex.head
    h in (:function, :macro, :struct, :abstract, :primitive, :module) && return "def"
    h === :(=) && is_signature(ex.args[1]) && return "def"
    h in (:using, :import) && return "using"
    h === :const && return "const"
    if h === :macrocall
        name = macro_name(ex)
        if name == "Core.@doc" || name == "@doc"
            # A docstring: the documented expression is the last argument.
            return definition_kind(ex.args[end]) == "def" ? "def" : nothing
        end
        name in DEFINITION_MACROS || return nothing
        (endswith(name, "@enum") || definition_kind(ex.args[end]) == "def") && return "def"
        return nothing
    end
    if h === :call && ex.args[1] === :include && length(ex.args) == 2 && ex.args[2] isa String
        return "include"
    end
    return nothing
end

function top_statements(ex)
    out = Any[]
    ex isa Expr && ex.head in (:toplevel, :block) || return Any[ex]
    for a in ex.args
        a isa LineNumberNode && continue
        append!(out, a isa Expr && a.head in (:toplevel, :block) ? top_statements(a) : Any[a])
    end
    return out
end

function defined_names(ex)
    ex isa Expr || return String[]
    h = ex.head
    if h === :macrocall
        return endswith(macro_name(ex), "@enum") ? enum_names(ex) : defined_names(ex.args[end])
    elseif h in (:struct,)
        return [string(type_head(ex.args[2]))]
    elseif h === :module
        return [string(ex.args[2])]
    elseif h in (:abstract, :primitive)
        return [string(type_head(ex.args[1]))]
    elseif h in (:function, :macro, :(=))
        sig = ex.args[1]
        while sig isa Expr && sig.head in (:where, :(::))
            sig = sig.args[1]
        end
        sig isa Expr && sig.head === :call || return String[]
        f = sig.args[1]
        f isa Expr && f.head === :(::) && return String[]   # a callable-object method
        name = string(f)
        return [h === :macro ? "@" * name : name]
    elseif h === :const
        a = ex.args[1]
        a isa Expr && a.head === :(=) || return String[]
        return a.args[1] isa Symbol ? [string(a.args[1])] : String[]
    end
    return String[]
end

type_head(x) = x isa Symbol ? x : x isa Expr && x.head in (:curly, :<:, :(::)) ? type_head(x.args[1]) : x
function enum_names(ex)
    args = filter(a -> !(a isa LineNumberNode), ex.args[2:end])
    isempty(args) && return String[]
    names = [string(type_head(args[1]))]
    for a in args[2:end]
        if a isa Symbol
            push!(names, string(a))
        elseif a isa Expr && a.head === :(=)
            push!(names, string(a.args[1]))
        elseif a isa Expr && a.head === :block
            append!(names, [string(b isa Expr ? b.args[1] : b) for b in a.args if !(b isa LineNumberNode)])
        end
    end
    return names
end

"""
    log_definitions!(code, call)

Appends the definition statements of `code` to DEFINITION_LOG. Called for
every call, successful or not: a failed call may have defined things before
it failed, and the check after a revival compares against what the kernel
really had. A failed call's statement that bound nothing is left out.
"""
# The names a `using`/`import` binds: `using A` binds A, `using A: f` binds
# f, `import A.B as C` binds C.
function imported_names(ex)
    out = String[]
    last_name(p) = p isa Expr && p.head === :. ? string(p.args[end]) : string(p)
    for a in ex.args
        if a isa Expr && a.head === :(:)
            for b in a.args[2:end]
                push!(out, b isa Expr && b.head === :as ? string(b.args[2]) : last_name(b))
            end
        elseif a isa Expr && a.head === :as
            push!(out, string(a.args[2]))
        else
            push!(out, last_name(a))
        end
    end
    return out
end

# Dependencies needed while evaluating a definition, excluding lazy function bodies.
function reconstruction_dependencies(ex)
    ex isa Expr || return String[]
    refs = Set{Symbol}()
    function signature(sig)
        sig isa Expr || return
        if sig.head === :call
            target = sig.args[1]
            target isa Expr && target.head === :. && symbol_references(target.args[1], refs)
            for arg in sig.args[2:end]
                if arg isa Expr && arg.head === :(::)
                    symbol_references(arg.args[end], refs)
                elseif arg isa Expr && arg.head === :(=)
                    signature(Expr(:call, :_, arg.args[1]))
                    symbol_references(arg.args[2], refs)
                elseif arg isa Expr && arg.head === :parameters
                    foreach(a -> signature(Expr(:call, :_, a)), arg.args)
                end
            end
        elseif sig.head === :where
            signature(sig.args[1])
            for arg in sig.args[2:end]
                name = arg isa Symbol ? arg : arg isa Expr ? type_head(arg) : nothing
                name isa Symbol && delete!(refs, name)
            end
        elseif sig.head === :(::)
            signature(sig.args[1]); symbol_references(sig.args[2], refs)
        end
    end
    if ex.head in (:function, :macro) || (ex.head === :(=) && is_signature(ex.args[1]))
        signature(ex.args[1])
    elseif ex.head === :module
        local_names = Set{Symbol}()
        for st in top_statements(ex.args[3])
            union!(refs, Symbol.(reconstruction_dependencies(st)))
            union!(local_names, Symbol.(defined_names(st)), assigned_symbols(st))
            st isa Expr && st.head in (:using, :import) && union!(local_names, Symbol.(imported_names(st)))
        end
        setdiff!(refs, local_names)
    elseif ex.head === :struct
        header = ex.args[2]
        header isa Expr && header.head === :<: && symbol_references(header.args[2], refs)
        for st in top_statements(ex.args[3])
            st isa Expr && st.head === :(::) && symbol_references(st.args[end], refs)
        end
        header isa Expr && header.head === :curly && foreach(a -> a isa Symbol && delete!(refs, a), header.args[2:end])
    elseif ex.head === :macrocall
        union!(refs, Symbol.(reconstruction_dependencies(ex.args[end])))
        ex.args[1] isa Symbol && push!(refs, ex.args[1])
    else
        union!(refs, symbol_references(ex))
        setdiff!(refs, assigned_symbols(ex))
    end
    return sort!(string.(setdiff(refs, Set(KERNEL_BINDINGS))))
end

function round_trips(st)
    text = string(st)
    reread = try
        Meta.parse(text)
    catch
        return false
    end
    return Base.remove_linenums!(deepcopy(reread)) == Base.remove_linenums!(deepcopy(st))
end

function expr_hex(st)
    buf = IOBuffer()
    serialize(buf, st)
    return bytes2hex(take!(buf))
end

# A logged definition statement: the Expr itself when it was kept, else its text.
logged_statement(e) = haskey(e, "expr_hex") ? deserialize(IOBuffer(hex2bytes(e["expr_hex"]))) : Meta.parse(e["code"])

function log_definitions!(code::String, call::Int; failed_in::Union{Nothing, Module}=nothing)
    ex = try
        parse_complete(code; filename="call $call")
    catch
        return
    end
    for st in top_statements(ex)
        kind = definition_kind(st)
        kind === nothing && continue
        entry = Dict{String, Any}("call" => call, "kind" => kind, "code" => string(st),
                                  "names" => kind == "using" ? imported_names(st) : defined_names(st), "requires" => reconstruction_dependencies(st))
        # Expr printing does not always read back (a multi-iterator generator
        # with a filter prints as `$(Expr(:filter, ...))`). Such a statement is
        # kept as the Expr itself, so revival rebuilds exactly what ran.
        round_trips(st) || (entry["expr_hex"] = expr_hex(st))
        if kind == "include"
            path = abspath(WORKSPACE_ROOT[], st.args[2])
            any(e -> e["kind"] == "include" && e["call"] == call && e["path"] == path, DEFINITION_LOG) && continue
            entry["path"] = path
            entry["stamp"] = isfile(path) ? collect(file_stamp(path)) : nothing
            entry["names"] = isfile(path) ? unique(reduce(vcat, (defined_names(d) for (_, d) in file_definitions(path)); init=String[])) : String[]
        end
        # In a call that failed, a statement after the error never ran. One
        # that bound nothing is left out, or a revival would report the loss
        # of something the kernel never had.
        if failed_in !== nothing
            # A method of another module's function (`Base.show`) cannot be
            # looked up by name here, so it is kept.
            bound = kind == "using" ? imported_names(st) : entry["names"]
            !isempty(bound) && !any(n -> occursin('.', n) || Base.invokelatest(isdefined, failed_in, Symbol(n)), bound) && continue
        end
        push!(DEFINITION_LOG, entry)
    end
    return
end

# Definition forms of an included file, as they are on disk now, with nested
# includes resolved the same way.
function file_definitions(path::String, depth::Int=0)
    depth > 8 && return Any[]
    ex = Meta.parseall(read(path, String); filename=path)
    out = Any[]
    for st in top_statements(ex)
        kind = definition_kind(st)
        if kind == "include"
            inner = abspath(dirname(path), st.args[2])
            isfile(inner) && append!(out, file_definitions(inner, depth + 1))
        elseif kind in ("def", "using")
            push!(out, (path, st))
        end
    end
    return out
end

# --- data ----------------------------------------------------------------

# Why a value cannot be revived as the same value, or nothing when it can.
# `user` collects the kernel's own types the value uses, and `ids` the
# mutable objects it reaches, so bindings sharing one are saved together.
struct Walk
    mod::Module
    seen::IdDict{Any, Nothing}
    user::Set{String}
    ids::Set{UInt}
    # `check_type` result per type: a value holds few distinct types but may
    # hold millions of objects, and the check scans every loaded module.
    checked::IdDict{Any, Union{Nothing, String}}
    visits::Base.RefValue{Int}
    # Asked every `WALK_YIELD_EVERY` mutable objects; true abandons the walk.
    give_way::Function
end
Walk(mod::Module, seen, user, ids) = Walk(mod, seen, user, ids, IdDict{Any, Union{Nothing, String}}(), Ref(0), () -> false)

# Thrown inside a walk when a waiting request should go first; the snapshot
# that started it stops and the last complete snapshot stays.
struct GaveWay <: Exception end
const WALK_YIELD_EVERY = 4096

const REFUSED_TYPES = (Task, Channel, Condition, Threads.Condition, ReentrantLock, Threads.SpinLock, Timer,
                       Base.Process, Base.ProcessChain, Base.Libc.RawFD, WeakRef, Module, Core.MethodInstance,
                       Method, Core.CodeInfo, Base.Semaphore, Base.Event)

const PTR_FREE = IdDict{Any, Bool}()
function ptr_free(T)
    get!(PTR_FREE, T) do
        T <: Ptr && return false
        isstructtype(T) || return true
        PTR_FREE[T] = true   # recursive types
        all(ptr_free, fieldtypes(T))
    end
end

function current_definition(T::DataType)
    m = parentmodule(T)
    ancestor = m
    while true
        parent = parentmodule(ancestor)
        (parent === ancestor || parent === Main || parent === Core || parent === Base) && break
        Base.invokelatest(isdefined, parent, nameof(ancestor)) || return false
        Base.invokelatest(getglobal, parent, nameof(ancestor)) === ancestor || return false
        ancestor = parent
    end
    isdefined(m, nameof(T)) || return false
    b = Base.invokelatest(getglobal, m, nameof(T))
    b isa Type || return false
    return Base.unwrap_unionall(b) isa DataType && Base.unwrap_unionall(b).name === T.name
end

function workspace_type_key(T::DataType)
    mod = parentmodule(T)
    root = Base.moduleroot(mod)
    any(pair -> first(pair) === root, workspace_packages()) || return nothing
    return "@workspace:" * string(mod) * "." * string(nameof(T))
end

function session_type_key(T::DataType, scope::Module)
    owner = parentmodule(T)
    parts = [string(nameof(T))]
    while owner !== scope
        parent = parentmodule(owner)
        (parent === owner || owner in (Main, Core, Base)) && return nothing
        pushfirst!(parts, string(nameof(owner)))
        owner = parent
    end
    return join(parts, '.')
end

# A function type the kernel created for an anonymous function or closure.
anonymous_function_type(T) = T isa DataType && T <: Function && startswith(string(nameof(T)), '#')

# Serialization sends a closure's whole type (its method included) only for
# closures of the real Main; one of the kernel's module it sends by name, and a
# new kernel would find no such type, or worse, a same-named closure of a
# later definition. This serializer sends the kernel's closures whole, so a
# revived closure runs the code it was made with.
mutable struct SnapshotSerializer{I<:IO} <: Serialization.AbstractSerializer
    io::I
    counter::Int
    table::IdDict{Any, Any}
    pending_refs::Vector{Int}
    known_object_data::Dict{UInt64, Any}
    version::Int
    mod::Module
    SnapshotSerializer(io::I, mod::Module) where {I<:IO} =
        new{I}(io, 0, IdDict{Any, Any}(), Int[], Dict{UInt64, Any}(), Serialization.ser_version, mod)
end

Serialization.should_send_whole_type(s::SnapshotSerializer, t::DataType) =
    (t.name.module === s.mod && anonymous_function_type(t)) ||
    invoke(Serialization.should_send_whole_type, Tuple{Any, DataType}, s, t)

function snapshot_bytes(mod::Module, x)
    buf = IOBuffer()
    s = SnapshotSerializer(buf, mod)
    Serialization.writeheader(s)
    serialize(s, x)
    return take!(buf)
end

# A closure deserializes with its own method, so its code must come from the
# kernel's module or from a package the new kernel loads too.
function closure_module_refusal(w::Walk, T::DataType)
    m = parentmodule(T)
    m === w.mod && return nothing
    Base.moduleroot(m) in (Base, Core) || haskey(Base.loaded_modules, Base.PkgId(Base.moduleroot(m))) ||
        return "it is a closure from module $(m), which a new kernel does not load"
    return nothing
end

function check_type(w::Walk, @nospecialize(T))
    T isa DataType || return nothing
    # Memoised: every side effect below is an idempotent set insertion, so a
    # cached answer is the same answer.
    haskey(w.checked, T) && return w.checked[T]
    r = check_type_uncached(w, T)
    w.checked[T] = r
    return r
end

function check_type_uncached(w::Walk, T::DataType)
    anonymous_function_type(T) && (r = closure_module_refusal(w, T)) !== nothing && return r
    for p in T.parameters
        p isa Type && (r = check_type(w, p)) !== nothing && return r
    end
    # Serialization rebuilds a closure's type in its own module, so there is no
    # definition of it to compare; a new kernel's closures reuse the same names.
    # Every value of one closure type is saved in one group: a group read on its
    # own would make its own copy of the type, and `===` would fail between them.
    anonymous_function_type(T) && (push!(w.ids, objectid(T)); return nothing)
    key = workspace_type_key(T)
    key === nothing && (key = session_type_key(T, w.mod))
    if key !== nothing
        current_definition(T) ||
            return "its type $(nameof(T)) is an earlier definition, since replaced by a new `struct $(nameof(T))`"
        push!(w.user, key)
    end
    return nothing
end

function refusal(w::Walk, @nospecialize(x))::Union{Nothing, String}
    x isa Union{Nothing, Missing, Bool, Char, String, Symbol, BigInt, BigFloat} && return nothing
    T = typeof(x)
    x isa Number && isprimitivetype(T) && Base.moduleroot(parentmodule(T)) in (Base, Core) && return nothing
    for R in REFUSED_TYPES
        x isa R && return "a $(nameof(R)) cannot be revived"
    end
    # Serialization saves a Regex as its pattern and flags and compiles it
    # again. Two bindings sharing one Regex come back as two equal copies; a
    # Regex cannot be changed, so only `===` can tell.
    x isa Regex && return nothing
    x isa Ptr && return "it holds a pointer"
    x isa IO && !(x isa IOBuffer) && return "an open $(nameof(T)) cannot be revived"
    if x isa Function && isdefined(T, :instance) && !startswith(string(nameof(x)), '#')
        m = parentmodule(x)
        m === w.mod && (push!(w.user, string(nameof(x))); return nothing)
        any(pair -> first(pair) === Base.moduleroot(m), workspace_packages()) &&
            push!(w.user, "@workspace:" * string(m) * "." * string(nameof(x)))
        return Base.moduleroot(m) in (Base, Core) || haskey(Base.loaded_modules, Base.PkgId(Base.moduleroot(m))) ?
               nothing : "it refers to function $(nameof(x)) of an unloaded module"
    end
    # Any other function value is a closure or an anonymous function: checked
    # below like a struct whose fields are the values it captured.
    if x isa Type
        u = Base.unwrap_unionall(x)
        key = u isa DataType ? workspace_type_key(u) : nothing
        key === nothing && u isa DataType && (key = session_type_key(u, w.mod))
        u isa DataType && key !== nothing && !current_definition(u) &&
            return "type $(nameof(u)) was replaced by a later definition"
        key === nothing || push!(w.user, key)
        return nothing
    end
    haskey(w.seen, x) && return nothing
    if ismutable(x)
        w.seen[x] = nothing
        push!(w.ids, objectid(x))
        (w.visits[] += 1) % WALK_YIELD_EVERY == 0 && w.give_way() && throw(GaveWay())
    end
    (r = check_type(w, T)) === nothing || return r
    return refusal_struct(w, x)
end

function refusal_struct(w::Walk, @nospecialize(x))
    T = typeof(x)
    if x isa Union{Array, Memory}
        E = eltype(x)
        if isbitstype(E)
            return ptr_free(E) ? nothing : "its elements hold pointers"
        end
        for i in eachindex(x)
            isassigned(x, i) || continue
            (r = refusal(w, @inbounds x[i])) === nothing || return r
        end
        return nothing
    end
    isbitstype(T) && return ptr_free(T) ? nothing : "it holds a pointer"
    for i in 1:nfields(x)
        isdefined(x, i) || continue
        (r = refusal(w, getfield(x, i))) === nothing || return r
    end
    return nothing
end

# Fields and field types of a kernel-defined type, as text: a type redefined
# with a different layout, even one Serialization would accept, is caught.
function type_fingerprint(mod::Module, name::String)
    if startswith(name, "@workspace:")
        parts = split(name[length("@workspace:") + 1:end], '.')
        loaded = [m for (m, _) in workspace_packages() if string(nameof(m)) == parts[1]]
        length(loaded) == 1 || return nothing
        owner = only(loaded)
        for part in parts[2:end-1]
            isdefined(owner, Symbol(part)) || return nothing
            owner = Base.invokelatest(getglobal, owner, Symbol(part))
            owner isa Module || return nothing
        end
        return type_fingerprint(owner, String(parts[end]))
    end
    parts = split(name, '.')
    for part in parts[1:end-1]
        isdefined(mod, Symbol(part)) || return nothing
        owner = Base.invokelatest(getglobal, mod, Symbol(part))
        owner isa Module || return nothing
        mod = owner
    end
    name = String(parts[end])
    isdefined(mod, Symbol(name)) || return nothing
    t = Base.invokelatest(getglobal, mod, Symbol(name))
    t isa Function && return "function"
    u = Base.unwrap_unionall(t)
    u isa DataType || return nothing
    return string(u.name.name, "{", join(string.(u.parameters), ","), "}:",
                  isstructtype(u) && !isabstracttype(u) ? join(("$f::$(fieldtype(u, f))" for f in fieldnames(u)), ";") : "abstract",
                  ismutabletype(u) ? ":mutable" : "")
end

# Every method of a kernel function, as text, to compare after a rebuild.
function method_set(mod::Module, name::String)
    isdefined(mod, Symbol(name)) || return nothing
    f = Base.invokelatest(getglobal, mod, Symbol(name))
    f isa Function || return nothing
    return sort!([string(m.sig) for m in Base.invokelatest(methods, f) if m.module === mod])
end

# What makes the package environment what it is: data is deserialized only
# into the same one.
function environment_identity()
    manifest = something(Base.project_file_manifest_path(Base.active_project()), "")
    pkgs = sort!([string(k.name, "@", something(pkgversion(m), "stdlib"))
                  for (k, m) in Base.loaded_modules if k.uuid !== nothing])
    return Dict{String, Any}("julia" => string(VERSION),
                             "manifest" => isfile(manifest) ? bytes2hex(sha256(read(manifest))) : "",
                             "packages" => pkgs)
end

# The code that last set `name`, for a binding that cannot be revived.
one_line(ex) = first(replace(string(ex isa Expr ? Base.remove_linenums!(deepcopy(ex)) : ex), r"#=.*?=#" => "", r"\s+" => " "), MAX_RECIPE_CHARS)

function recipe(name::String, call::Int)
    hist = get_kernel_state().execution_history
    1 <= call <= length(hist) || return ""
    code = hist[call].code
    isempty(code) && return ""
    ex = try
        Meta.parseall(code)
    catch
        return first(replace(code, r"\s+" => " "), MAX_RECIPE_CHARS)
    end
    for st in top_statements(ex)
        st isa Expr && st.head in (:(=), :const, :global) || continue
        lhs = st.head === :(=) ? st.args[1] : st.args[1]
        lhs isa Expr && lhs.head === :(=) && (lhs = lhs.args[1])
        (lhs === Symbol(name) || (lhs isa Expr && lhs.head === :tuple && Symbol(name) in lhs.args)) &&
            return one_line(st)
    end
    return first(replace(strip(code), r"\s+" => " "), MAX_RECIPE_CHARS)
end

# --- snapshot ------------------------------------------------------------

mutable struct Group
    names::Vector{String}
    ids::Set{UInt}
    user::Set{String}
end

# The call whose state the last complete snapshot holds, in this kernel, and
# how long that snapshot took.
const LAST_SAVED_CALL = Ref(0)
const LAST_SNAPSHOT_SECONDS = Ref(0.0)
# When saving takes this long, a snapshot gives way to a waiting request, so
# the model does not wait for it, until this many calls have gone unsaved. A
# quick snapshot always completes: the state revived is then the last call's.
const SLOW_SNAPSHOT_SECONDS = 2.0
const MAX_UNSAVED_CALLS = 5

"""
    note_completed_call!(call)

Records in STATE_DIR that `call` completed. A snapshot can give way and fall
behind; a revival compares this with the call its snapshot holds and says
which calls' effects on the kernel it could not bring back.
"""
function note_completed_call!(call::Int)
    dir = STATE_DIR[]
    isempty(dir) && return nothing
    tmp = joinpath(dir, "last_call.tmp")
    write(tmp, string(call))
    mv(tmp, joinpath(dir, "last_call"); force=true)
    return nothing
end

"""
    snapshot_state!(call; waiting = () -> false)

Writes the state at the end of `call` to STATE_DIR, replacing the last
snapshot only once the new one is complete. When the last complete snapshot was
slow and recent, and `waiting()` says a request is waiting, it stops and
returns `nothing`; the last complete snapshot stays.
"""
function snapshot_state!(call::Int; waiting::Function = () -> false)
    dir = STATE_DIR[]
    isempty(dir) && return nothing
    started = time()
    # Gives way when this snapshot or the last one is slow: a first slow
    # snapshot must not hold a waiting request past its deadline either.
    give_way() = (LAST_SNAPSHOT_SECONDS[] >= SLOW_SNAPSHOT_SECONDS || time() - started >= SLOW_SNAPSHOT_SECONDS) &&
                 call - LAST_SAVED_CALL[] < MAX_UNSAVED_CALLS && waiting()
    give_way() && return nothing
    mod = get_kernel_state().eval_module
    consts = Set(n for e in DEFINITION_LOG if e["kind"] == "const" for n in e["names"])
    defnames = Set(n for e in DEFINITION_LOG if e["kind"] in ("def", "include") for n in e["names"])
    bindings = Dict{String, Any}[]
    groups = Group[]
    for (k, sym) in enumerate(sort!(Base.invokelatest(names, mod; all=true)))
        k % 32 == 0 && give_way() && return nothing
        (sym === nameof(mod) || sym in KERNEL_BINDINGS || startswith(string(sym), '#')) && continue
        Base.invokelatest(isdefined, mod, sym) || continue
        v = Base.invokelatest(getglobal, mod, sym)
        name = string(sym)
        setcall = get(BINDING_SEEN, sym, (UInt(0), call))[2]
        b = Dict{String, Any}("name" => name, "type" => short_type(v), "call" => setcall,
                              "const" => name in consts)
        b["origin"] = get(BINDING_ORIGINS, sym, Dict{String, Any}())
        own = v isa Function ? isdefined(typeof(v), :instance) && parentmodule(v) === mod && nameof(v) === sym :
              v isa Type && Base.unwrap_unionall(v) isa DataType && parentmodule(Base.unwrap_unionall(v)) === mod &&
              nameof(Base.unwrap_unionall(v)) === sym
        if own && name in defnames
            b["class"] = "definition"
            b["check"] = v isa Function ? method_set(mod, name) : type_fingerprint(mod, name)
        elseif name in defnames && !(name in consts)
            # Made by a definition (an @enum member): rebuilt with it, checked by value.
            b["class"] = "definition"
            b["check"] = "value:" * repr(v)
        elseif own
            b["class"] = "runtime"
            b["reason"] = "it was not defined by top-level code of a call (inside `let`, `@eval` or a block), so it cannot be rebuilt"
        elseif v isa Module
            b["class"] = haskey(Base.loaded_modules, Base.PkgId(v)) ? "import" : "runtime"
            b["class"] == "runtime" && (b["reason"] = "a module defined in the session cannot be revived")
        else
            w = Walk(mod, IdDict{Any, Nothing}(), Set{String}(), Set{UInt}(), IdDict{Any, Union{Nothing, String}}(), Ref(0), give_way)
            why = try
                refusal(w, v)
            catch e
                e isa GaveWay && return nothing
                "it could not be inspected ($(typeof(e)))"
            end
            if why === nothing
                b["class"] = "data"
                b["user_types"] = sort!(collect(w.user))
                g = Group([name], w.ids, w.user)
                for other in filter(o -> !isdisjoint(o.ids, w.ids), groups)
                    append!(g.names, other.names); union!(g.ids, other.ids); union!(g.user, other.user)
                    filter!(o -> o !== other, groups)
                end
                push!(groups, g)
            else
                b["class"] = "runtime"
                b["reason"] = why
            end
        end
        b["class"] == "runtime" && (b["recipe"] = recipe(name, setcall))
        b["files"] = [[p, evidence["stamp"]] for (p, evidence) in binding_sources(b["origin"], setcall)]
        push!(bindings, b)
    end
    byname = Dict(b["name"] => b for b in bindings)
    seq = string(call, "-", round(Int, time() * 1000) % 10^9)
    written = String[]
    stored = Dict{String, Any}[]
    total = 0
    for (i, g) in enumerate(groups)
        if give_way()
            foreach(f -> rm(joinpath(dir, f * ".tmp"); force=true), written)
            return nothing
        end
        bytes = UInt8[]
        ok = try
            bytes = snapshot_bytes(mod, Dict(n => Base.invokelatest(getglobal, mod, Symbol(n)) for n in g.names))
            true
        catch e
            for n in g.names
                byname[n]["class"] = "runtime"
                byname[n]["reason"] = "serializing it failed: $(first(sprint(showerror, e), 160))"
                byname[n]["recipe"] = recipe(n, byname[n]["call"])
            end
            false
        end
        ok || continue
        why = length(bytes) > max_binding_bytes() * length(g.names) ?
                  "$(Base.format_bytes(length(bytes))) is over the $(Base.format_bytes(max_binding_bytes())) limit per binding" :
              total + length(bytes) > max_snapshot_bytes() ?
                  "the snapshot reached its $(Base.format_bytes(max_snapshot_bytes())) limit" : nothing
        if why !== nothing
            for n in g.names
                byname[n]["class"] = "runtime"
                byname[n]["reason"] = "not saved: " * why
                byname[n]["recipe"] = recipe(n, byname[n]["call"])
            end
            continue
        end
        total += length(bytes)
        file = "data-$seq-$i.jls"
        write(joinpath(dir, file * ".tmp"), bytes)
        push!(written, file)
        push!(stored, Dict{String, Any}("file" => file, "names" => sort(g.names), "sha256" => bytes2hex(sha256(bytes)),
                                        "user_types" => sort!(collect(g.user))))
    end
    usertypes = Set(t for g in stored for t in g["user_types"])
    manifest = Dict{String, Any}(
        "format" => REVIVAL_FORMAT, "call" => call, "module" => string(nameof(mod)), "workspace" => WORKSPACE_ROOT[],
        "environment" => environment_identity(),
        "load_path" => copy(LOAD_PATH), "active_project" => something(Base.active_project(), ""),
        "cwd" => pwd(), "env" => Dict(k => v for (k, v) in ENV if get(INITIAL_ENV[], k, nothing) != v),
        "env_removed" => [k for k in keys(INITIAL_ENV[]) if !haskey(ENV, k)],
        "definitions" => DEFINITION_LOG, "bindings" => bindings, "data" => stored,
        "type_fingerprints" => Dict(t => type_fingerprint(mod, t) for t in usertypes),
        "used_files" => [[p, collect(s), c] for (p, (s, c)) in USED_FILES],
        "workspace_packages" => Dict(string(nameof(mod_)) => Dict(k => collect(v) for (k, v) in snap) for (mod_, snap) in WORKSPACE_PACKAGES),
        "processes" => [first(cmd, 200) for (_, cmd, _) in background_processes()],
        "bytes" => total, "seconds" => round(time() - started, digits=3))
    for f in written
        mv(joinpath(dir, f * ".tmp"), joinpath(dir, f); force=true)
    end
    tmp = joinpath(dir, "manifest.json.tmp")
    write(tmp, JSON.json(manifest))
    mv(tmp, joinpath(dir, "manifest.json"); force=true)   # the commit point
    LAST_SAVED_CALL[] = call
    LAST_SNAPSHOT_SECONDS[] = time() - started
    keep = Set(written)
    for f in readdir(dir)
        startswith(f, "data-") && !(f in keep) && rm(joinpath(dir, f); force=true)
    end
    return manifest
end

# --- revival -------------------------------------------------------------

"""
    revival_module_name() -> Union{Nothing, String}

The eval-module name the last kernel used, so this one can take it: values
of kernel-defined types deserialize only into a module of the same name.
"""
function revival_module_name()
    path = joinpath(STATE_DIR[], "manifest.json")
    isempty(STATE_DIR[]) || !isfile(path) ? nothing : try
        m = JSON.parse(read(path, String); dicttype=Dict{String, Any})
        get(m, "workspace", nothing) == WORKSPACE_ROOT[] ? String(m["module"]) : nothing
    catch
        nothing
    end
end

fmt_binding(b) = "$(b["name"]) ($(b["type"]), call $(b["call"]))"

# A list line that stays readable at any size: in full while short, then as
# names only, then as the first names and a count; varinfo() has the rest.
const REPORT_LIST_CHARS = 1500
function short_list(heading::String, items::Vector{String})
    line = "  $heading: " * join(items, ", ")
    length(line) <= REPORT_LIST_CHARS && return line
    names = [first(split(i, " ("; limit=2)) for i in items]
    shown = String[]; n = 0
    for nm in names
        n + length(nm) + 2 > REPORT_LIST_CHARS && break
        push!(shown, nm); n += length(nm) + 2
    end
    rest = length(names) - length(shown)
    return "  $heading ($(length(items))): " * join(shown, ", ") *
           (rest > 0 ? ", and $rest more (varinfo() lists every binding)" : "")
end

"""
    revive_state!() -> String

Rebuilds the last snapshot's state in this kernel, and returns the report the
first call shows.
"""
read_or_empty(path) = isfile(path) ? read(path, String) : ""

function revive_state!()
    dir = STATE_DIR[]
    path = joinpath(dir, "manifest.json")
    if isempty(dir) || !isfile(path)
        previous = !isempty(dir) && isfile(joinpath(dir, "last_call"))
        return revival_observation!(; lost=previous ? ["previous live state: no complete snapshot remains"] : String[],
            text=previous ? "[revival] No complete snapshot remains. Prior live state is lost; workspace files remain." : "")
    end
    m = try
        JSON.parse(read(path, String); dicttype=Dict{String, Any})
    catch e
        return revival_observation!(; lost=["previous bindings (snapshot inventory unreadable)"],
            uncertain=["previous binding inventory could not be established"],
            text="[revival] The previous kernel stopped, and its snapshot could not be read " *
                 "($(first(sprint(showerror, e), 160))). Nothing was revived; files in the workspace remain.")
    end
    get(m, "format", 0) == REVIVAL_FORMAT || return revival_observation!(; lost=["previous snapshot: incompatible format"],
        text="[revival] The previous kernel's snapshot is in an unknown format ($(get(m, "format", 0)); current $REVIVAL_FORMAT). Nothing was revived.")
    # A state saved for another workspace belongs to another task.
    get(m, "workspace", nothing) == WORKSPACE_ROOT[] || return revival_observation!()
    state = get_kernel_state()
    mod = state.eval_module
    call = Int(m["call"])

    # The environment turn code set up: where packages load from, and where it works.
    empty!(LOAD_PATH); append!(LOAD_PATH, String.(m["load_path"]))
    isempty(m["active_project"]) || (Base.ACTIVE_PROJECT[] = m["active_project"])
    isdir(m["cwd"]) && cd(m["cwd"])
    for (k, v) in m["env"]; ENV[k] = v; end
    for k in m["env_removed"]; delete!(ENV, k); end

    # Definitions, in call order. Package code loads first from `using`.
    errors = Dict{String, String}()
    changed_files = Set{String}()
    pending = Any[]
    for e in m["definitions"]
        e["kind"] == "const" && continue
        if e["kind"] == "include"
            p = e["path"]
            if !isfile(p)
                foreach(n -> errors[n] = "$(relpath(p, WORKSPACE_ROOT[])) is gone", e["names"])
                continue
            end
            ((e["stamp"] !== nothing && Tuple(e["stamp"]) != file_stamp(p)) || (get(e, "sha256", nothing) !== nothing && bytes2hex(sha256(read(p))) != e["sha256"])) && push!(changed_files, p)
            for (file, st) in file_definitions(p)
                push!(pending, (file, st, e))
            end
        else
            # One unreadable definition costs only the names it binds, not the
            # whole revival.
            st = try
                logged_statement(e)
            catch err
                foreach(n -> errors[n] = "its definition could not be read back ($(typeof(err)))", e["names"])
                continue
            end
            push!(pending, (nothing, st, e))
        end
    end
    # Keep original order among replacements of the same binding, but build
    # foundational providers before eager consumers. Only deferred nodes run later.
    providers = Dict{String, Vector{Int}}()
    names_by_item = [definition_kind(st) == "using" ? imported_names(st) : defined_names(st) for (_, st, _) in pending]
    for (i, names_) in enumerate(names_by_item), name in names_
        push!(get!(providers, name, Int[]), i)
    end
    dependencies = [Set{Int}() for _ in pending]
    value_dependencies = [String[] for _ in pending]
    for (i, (_, st, e)) in enumerate(pending)
        e["requires"] = reconstruction_dependencies(st)
        for name in e["requires"]
            options = [j for j in get(providers, name, Int[]) if j != i && definition_kind(pending[j][2]) != "using"]
            if !isempty(options)
                earlier = filter(j -> j < i, options)
                push!(dependencies[i], isempty(earlier) ? first(options) : last(earlier))
            elseif !(name in names_by_item[i]) && any(b -> b["name"] == name, m["bindings"])
                push!(value_dependencies[i], name)
            end
        end
        for name in names_by_item[i]
            earlier = filter(j -> j < i, get(providers, name, Int[]))
            isempty(earlier) || push!(dependencies[i], last(earlier))
        end
    end
    done = Set{Int}(); attempted = Set{Int}()
    function replay(items)
        waiting = Set{Int}()
        for item in items
            index = findfirst(p -> p === item, pending)
            index === nothing && error("Reconstruction item is absent from its dependency graph")
            push!(waiting, index)
        end
        failures = Any[]
        while !isempty(waiting)
            ready = sort!([i for i in waiting if issubset(dependencies[i], done) && all(n -> Base.invokelatest(isdefined, mod, Symbol(n)), value_dependencies[i])])
            isempty(ready) && break
            for i in ready
                delete!(waiting, i); push!(attempted, i)
                file, st, e = pending[i]
                try
                    file === nothing ? Core.eval(mod, st) : task_local_storage(:SOURCE_PATH, file) do
                        Core.eval(mod, st)
                    end
                    push!(done, i)
                catch err
                    push!(failures, (pending[i], first(sprint(showerror, err isa LoadError ? err.error : err), 160)))
                end
            end
        end
        for i in sort!(collect(waiting))
            roots = [join(names_by_item[j], ",") for j in sort!(collect(setdiff(dependencies[i], done)))]
            append!(roots, [n for n in value_dependencies[i] if !Base.invokelatest(isdefined, mod, Symbol(n))])
            push!(failures, (pending[i], "unavailable reconstruction prerequisites: " * join(roots, "; ")))
        end
        return failures
    end
    failed = with_logger(DocReplacementFilter(current_logger())) do
        replay(pending)
    end

    # Everything after the replay runs in the world the replay made: checks
    # call methods it defined (an @enum's names, a type's `show`).
    return Base.invokelatest(finish_revival, m, mod, state, call, pending, failed, errors, changed_files, replay, attempted)
end

function finish_revival(m, mod, state, call, pending, failed, errors, changed_files, replay, attempted)
    dir = STATE_DIR[]
    lines = String[]
    exact = String[]; rebuilt = String[]; approx = String[]; stale = String[]; lost = String[]
    # Data, when this is the environment it was saved in.
    envnow = environment_identity()
    envthen = m["environment"]
    bindings = Dict(b["name"] => b for b in m["bindings"])
    data_block = if envthen["julia"] != envnow["julia"]
        "it was saved by Julia $(envthen["julia"]) and this is $(envnow["julia"])"
    elseif envthen["manifest"] != envnow["manifest"]
        "the package environment changed since it was saved"
    else
        nothing
    end
    values = Dict{String, Any}()
    bound = Set{String}()
    groups = collect(m["data"])
    while true
        progress_before = (length(groups), length(attempted), length(bound))
        remaining = Any[]
        for g in groups
            if data_block === nothing && any(t -> type_fingerprint(mod, t) === nothing, g["user_types"])
                push!(remaining, g)
                continue
            end
            names = String.(g["names"])
            why = data_block
            if why === nothing
                bad = [t for t in g["user_types"] if get(m["type_fingerprints"], t, nothing) != type_fingerprint(mod, t)]
                isempty(bad) || (why = "type $(join(bad, ", ")) is defined differently now")
            end
            file = joinpath(dir, g["file"])
            if why === nothing
                bytes = isfile(file) ? read(file) : nothing
                why = bytes === nothing ? "its data file is missing" :
                      bytes2hex(sha256(bytes)) != g["sha256"] ? "its data file is damaged" : nothing
                if why === nothing
                    try
                        d = Base.invokelatest(deserialize, IOBuffer(bytes))
                        for n in names
                            values[n] = d[n]
                        end
                    catch err
                        why = "reading it failed: $(first(sprint(showerror, err), 160))"
                    end
                end
            end
            if why !== nothing
                for n in names
                    bindings[n]["class"] = "runtime"
                    bindings[n]["reason"] = why
                end
            end
        end
        groups = remaining
        for (n, v) in collect(values)
            n in bound && continue
        b = bindings[n]
        try
            Core.eval(mod, Expr(b["const"] ? :const : :global, Expr(:(=), Symbol(n), QuoteNode(v))))
        catch err
            delete!(values, n)
            b["class"] = "runtime"
            b["reason"] = "binding it again failed: $(first(sprint(showerror, err), 160))"
        end
            push!(bound, n)
        end
        blocked = [item for (item, _) in failed if !(findfirst(p -> p === item, pending) in attempted)]
        failed = with_logger(DocReplacementFilter(current_logger())) do
            [filter(pair -> findfirst(p -> p === pair[1], pending) in attempted, failed); replay(blocked)]
        end
        (length(groups), length(attempted), length(bound)) == progress_before && break
    end
    for g in groups, n in g["names"]
        bindings[n]["class"] = "runtime"
        bindings[n]["reason"] = "unavailable type reconstruction prerequisites: " * join([t for t in g["user_types"] if type_fingerprint(mod, t) === nothing], ", ")
    end
    for (item, err) in failed
        for n in item[3]["names"]
            errors[n] = "rebuilding it failed: $err"
        end
        # A method of a function that is not the session's own (`Base.show(…) = …`)
        # has no binding to report it under.
        any(n -> haskey(bindings, n), item[3]["names"]) ||
            push!(lost, "the definition from call $(item[3]["call"]) `$(one_line(item[2]))`: rebuilding it failed: $err")
    end

    # Workspace packages (`using` makes no binding for them): loaded again from
    # the workspace, and said to differ when their sources changed.
    changed_packages = Set{String}()
    for (name, saved) in sort!(collect(get(m, "workspace_packages", Dict())); by=first)
        loaded = [mm for (k, mm) in Base.loaded_modules if k.name == name && pathof(mm) !== nothing &&
                  startswith(pathof(mm), WORKSPACE_ROOT[] * "/")]
        if isempty(loaded)
            push!(changed_packages, name)
            push!(lost, "$name (workspace package): it did not load again")
        elseif Dict(k => Tuple(v) for (k, v) in saved) != source_snapshot(dirname(pathof(loaded[1])))
            push!(changed_packages, name)
            push!(approx, "$name (workspace package): loaded again from sources that changed since")
        else
            push!(rebuilt, "$name (workspace package)")
        end
    end
    # Check each binding against what the old kernel had: definitions first,
    # because data that refers to a definition is only as exact as it is.
    inexact = changed_packages
    ordered = sort!(collect(bindings); by=nb -> (nb[2]["class"] in ("definition", "import") ? 0 : 1, nb[1]))
    for (n, b) in ordered
        cls = b["class"]
        if cls == "definition"
            now = b["check"] isa String && startswith(b["check"], "value:") ?
                  (isdefined(mod, Symbol(n)) ? "value:" * repr(getglobal(mod, Symbol(n))) : nothing) :
                  startswith(b["type"], "function") ? method_set(mod, n) : type_fingerprint(mod, n)
            if haskey(errors, n) || now === nothing
                push!(inexact, n)
                push!(lost, "$(fmt_binding(b)): $(get(errors, n, "its definition was not found in any call's top-level code"))")
            elseif now != b["check"]
                push!(inexact, n)
                push!(approx, "$(fmt_binding(b)): rebuilt, but its methods or fields differ from before")
            elseif any(e -> e["kind"] == "include" && n in e["names"] && e["path"] in changed_files, m["definitions"])
                push!(inexact, n)
                push!(approx, "$(fmt_binding(b)): rebuilt from a file that changed since")
            else
                push!(rebuilt, fmt_binding(b))
            end
        elseif cls == "import"
            changed = get(get(m, "workspace_packages", Dict()), n, nothing)
            if !isdefined(mod, Symbol(n))
                push!(lost, "$n (module): its `using`/`import` was not found")
            elseif changed !== nothing && Dict(k => Tuple(v) for (k, v) in changed) != source_snapshot(dirname(something(pathof(getglobal(mod, Symbol(n))), "/x/x")))
                push!(approx, "$n (module): loaded again from workspace sources that changed since")
            else
                push!(rebuilt, "$n (module)")
            end
        elseif cls == "data" && haskey(values, n)
            origin = get(b, "origin", Dict{String, Any}())
            sources = binding_sources(origin, b["call"])
            changed_paths = [p for (p, evidence) in sources if source_changed(evidence; strong=true)]
            origin["strong_stale_sources"] = changed_paths
            changed = [relpath(p, WORKSPACE_ROOT[]) for p in changed_paths]
            superseded = any(artifact_changed, get(origin, "artifacts", Dict{String, Any}[]))
            superseded && append!(changed, ["tracked artifact authority/content superseded"])
            types = String.(get(b, "user_types", String[]))
            owners = Set(startswith(t, "@workspace:") ? first(split(t[length("@workspace:") + 1:end], '.')) : t for t in types)
            refers = sort!(collect(intersect(owners, inexact)))
            if !isempty(refers)
                push!(approx, "$(fmt_binding(b)): restored, but it refers to $(join(refers, ", ")), which is not the same as before")
            elseif isempty(changed)
                push!(exact, fmt_binding(b))
            else
                push!(stale, "$(fmt_binding(b)): $(join(changed, ", ")) changed since")
            end
        else
            r = get(b, "recipe", "")
            push!(lost, "$(fmt_binding(b)): $(get(b, "reason", "not revived"))" * (isempty(r) ? "" : "; it came from `$r`"))
        end
    end

    # This kernel continues the old one's call numbers and file tracking.
    hist = state.execution_history
    while length(hist) < call
        push!(hist, ExecutionRecord(uuid4(), Dates.now(), "", true, nothing, 0.0))
    end
    empty!(DEFINITION_LOG)
    append!(DEFINITION_LOG, m["definitions"])
    for (p, s, c) in m["used_files"]
        USED_FILES[p] = (Tuple(s), c)
    end
    for (n, b) in bindings
        sym = Symbol(n)
        if (haskey(values, n) || b["class"] == "definition") && Base.invokelatest(isdefined, mod, sym)
            value = Base.invokelatest(getglobal, mod, sym)
            BINDING_SEEN[sym] = (objectid(value), b["call"])
            BINDING_ORIGINS[sym] = Dict{String, Any}(get(b, "origin", Dict()))
            origin = BINDING_ORIGINS[sym]
            get(origin, "definition", nothing) isa AbstractDict &&
                (origin["definition"] = Dict{String, Any}(origin["definition"]))
            value isa Union{Type, Module} && (BINDING_IDENTITIES[sym] = value)
            CALL_FILES[b["call"]] = [(String(p), (Float64(s[1]), Int(s[2]))) for (p, s) in b["files"]]
        end
    end

    push!(lines, "[revival] The previous kernel stopped. This kernel revived the state saved at the end of call $call" *
                 (data_block === nothing ? "." : "; its data could not be restored because $data_block."))
    last = tryparse(Int, strip(read_or_empty(joinpath(dir, "last_call"))))
    last !== nothing && last > call &&
        push!(lines, "  Call$(last == call + 1 ? " $last" : "s $(call + 1)–$last") completed after this state was saved: " *
                     "what $(last == call + 1 ? "it" : "they") changed in the kernel is lost; files written to the workspace remain.")
    procs = String.(get(m, "processes", String[]))
    isempty(procs) || push!(lines, "  Background processes stopped with the previous kernel: " *
                                   join(("`$(first(c, 100))`" for c in procs), ", ") * ". Start them again if you need them.")
    # What needs attention first: a long report is cut in the middle for the
    # model (in the 12h run it reached 23 KB), so the lists that only confirm
    # come last and are shortened.
    isempty(lost) || push!(lines, "  not revived: " * join(lost, "; "))
    isempty(approx) || push!(lines, "  not the same as before: " * join(approx, "; "))
    isempty(stale) || push!(lines, "  restored but stale (a file it was computed from changed): " * join(stale, "; "))
    isempty(rebuilt) || push!(lines, short_list("rebuilt from source", rebuilt))
    isempty(exact) || push!(lines, short_list("restored exactly", exact))
    isempty(bindings) && push!(lines, "  (the previous kernel had no bindings)")
    push!(lines, "  Packages were loaded again, and LOAD_PATH, the active project, ENV and the working directory were restored. " *
                 "Other runtime state starts fresh: random number streams, settings changed inside packages, open handles. " *
                 "Files in the workspace are unchanged.")
    uncertain = copy(approx)
    last !== nothing && last > call && push!(uncertain, "calls $(call + 1)–$last completed after the saved snapshot")
    return revival_observation!(; restored=exact, rebuilt, stale, lost, uncertain, saved_call=call, text=join(lines, "\n"))
end
