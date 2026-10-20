# Statement-level observations, not a general dynamic taint engine.
function source_evidence(path::AbstractString; kind="direct")
    full = abspath(path)
    isfile(full) || return nothing
    return Dict{String, Any}("path" => full, "stamp" => collect(file_stamp(full)),
        "sha256" => bytes2hex(open(sha256, full)), "kind" => kind)
end

function named_statement_sources(ex)
    files = Dict{String, Any}()
    for lit in string_literals!(String[], ex), tok in split(lit, (' ', '\t', '\n', '\'', '"', ';', '|', '<', '>', '(', ')', '=', ','))
        isempty(tok) && continue
        length(tok) <= 4096 || continue
        path = abspath(WORKSPACE_ROOT[], tok)
        startswith(path, WORKSPACE_ROOT[] * "/") && isfile(path) || continue
        evidence = source_evidence(path)
        evidence === nothing || (files[path] = evidence)
    end
    return files
end

function symbol_references(ex, out=Set{Symbol}())
    ex isa Symbol && push!(out, ex)
    if ex isa Expr && ex.head !== :quote
        args = ex.head === :(=) && !is_signature(ex.args[1]) ? ex.args[2:end] : ex.args
        foreach(a -> symbol_references(a, out), args)
    end
    ex isa GlobalRef && push!(out, ex.name)
    return out
end

function assigned_symbols(ex)
    ex isa Expr || return Set{Symbol}()
    if ex.head in (:const, :global)
        return reduce(union, (assigned_symbols(a) for a in ex.args); init=Set{Symbol}())
    elseif ex.head === :(=) && !is_signature(ex.args[1])
        lhs = ex.args[1]
        return lhs isa Symbol ? Set([lhs]) : lhs isa Expr && lhs.head === :tuple ? Set(a for a in lhs.args if a isa Symbol) : Set{Symbol}()
    end
    return Set{Symbol}()
end

function binding_ids(mod)
    return Dict(n => objectid(Base.invokelatest(getglobal, mod, n)) for n in Base.invokelatest(names, mod; all=true)
        if session_binding(mod, n) && Base.invokelatest(isdefined, mod, n))
end

# Since Julia 1.12, a struct redefined with a different layout is a new type
# (older versions refuse the redefinition). Values made and methods defined
# before still use the old one, which prints as `@world(Foo, ...)` and fails
# dispatch against the new one, so the turn says which bindings still refer to it.
defined_types(ex) = !(ex isa Expr) ? Symbol[] :
    ex.head in (:toplevel, :block) ? reduce(vcat, map(defined_types, ex.args); init=Symbol[]) :
    ex.head === :macrocall ? defined_types(ex.args[end]) :
    ex.head === :struct ? filter(s -> s isa Symbol, [type_head(ex.args[2])]) :
    ex.head in (:abstract, :primitive) ? filter(s -> s isa Symbol, [type_head(ex.args[1])]) : Symbol[]

function current_types(mod, ex)
    out = Dict{Symbol, Any}()
    for name in defined_types(ex)
        Base.invokelatest(isdefined, mod, name) || continue
        value = Base.invokelatest(getglobal, mod, name)
        value isa Type && (out[name] = value)
    end
    return out
end

function mentions_type(@nospecialize(T), tn::Core.TypeName, depth=0)
    depth > 16 && return false
    T isa TypeVar && return mentions_type(T.ub, tn, depth + 1)
    T isa UnionAll && return mentions_type(Base.unwrap_unionall(T), tn, depth + 1)
    T isa Union && return mentions_type(T.a, tn, depth + 1) || mentions_type(T.b, tn, depth + 1)
    T isa DataType || return false
    T.name === tn && return true
    return any(p -> mentions_type(p, tn, depth + 1), T.parameters)
end

function stale_type_notice(mod, types_before)
    lines = String[]
    for (name, old) in types_before
        Base.invokelatest(getglobal, mod, name) === old && continue
        body = Base.unwrap_unionall(old)
        body isa DataType || continue
        tn = body.name
        values, methods_, fields = String[], String[], String[]
        for sym in Base.invokelatest(names, mod; all=true)
            (sym in (name, :ans) || !session_binding(mod, sym)) && continue
            Base.invokelatest(isdefined, mod, sym) || continue
            v = Base.invokelatest(getglobal, mod, sym)
            if v isa Type
                t = Base.unwrap_unionall(v)
                t isa DataType && isconcretetype(t) || continue
                any(f -> mentions_type(f, tn), fieldtypes(t)) && push!(fields, string(sym))
            elseif v isa Function
                any(m -> mentions_type(m.sig, tn), methods(v)) && push!(methods_, string(sym))
            elseif mentions_type(typeof(v), tn)
                push!(values, string(sym))
            end
        end
        stale = String[]
        isempty(values) || push!(stale, "values " * join(values, ", "))
        isempty(methods_) || push!(stale, "methods of " * join(methods_, ", "))
        isempty(fields) || push!(stale, "fields of " * join(fields, ", "))
        isempty(stale) && continue
        push!(lines, "[Palette: $name changed layout, so it is a new type. Still using the old $name: " *
              join(stale, "; ") * ". Re-run those definitions, and rebuild those values, to use the new $name.]")
    end
    isempty(lines) || println(join(lines, "\n"))
end

function observed_statement(mod, ex, run; source=nothing)
    call = length(get_kernel_state().execution_history) + 1
    types_before = current_types(mod, ex)
    before = binding_ids(mod)
    inherited = Dict{String, Any}()
    artifacts = Dict{String, Any}[]
    for name in symbol_references(ex)
        origin = get(BINDING_ORIGINS, name, Dict())
        for (path, evidence) in get(origin, "sources", Dict())
            inherited[path] = merge(evidence, Dict("kind" => "derived"))
        end
        append!(artifacts, get(origin, "artifacts", Dict{String, Any}[]))
    end
    direct = source === nothing ? named_statement_sources(ex) : Dict(source["path"] => source)
    merge!(inherited, direct)
    outer = get(task_local_storage(), :palette_artifact_reads, nothing)
    reads = Dict{String, Any}[]
    outer_sources = get(task_local_storage(), :palette_source_reads, nothing)
    source_reads = Dict{String, Any}[]
    task_local_storage(:palette_source_reads, source_reads)
    task_local_storage(:palette_artifact_reads, reads)
    try
        value = run()
        isempty(types_before) || try stale_type_notice(mod, types_before) catch end
        return value
    finally
        task_local_storage(:palette_source_reads, outer_sources)
        outer_sources === nothing || append!(outer_sources, source_reads)
        foreach(e -> inherited[e["path"]] = e, source_reads)
        task_local_storage(:palette_artifact_reads, outer)
        outer === nothing || append!(outer, reads)
        append!(artifacts, reads)
        after = Base.invokelatest(binding_ids, mod)
        assigned = assigned_symbols(ex)
        for (name, id) in after
            (get(before, name, nothing) != id || name in assigned) || continue
            old = get(BINDING_ORIGINS, name, Dict())
            # A nested include already observed this binding in its source context.
            get(old, "observed_call", nothing) == call && !(name in assigned) && source === nothing && continue
            BINDING_SEEN[name] = (id, call)
            BINDING_ORIGINS[name] = Dict{String, Any}("epoch" => TASK_EPOCH[], "source_digest" => bytes2hex(sha256(string(ex))),
                "observed_call" => call, "sources" => deepcopy(inherited), "artifacts" => unique(deepcopy(artifacts)))
        end
    end
end

function eval_observed(mod, parsed)
    value = nothing
    line = LineNumberNode(1, :none)
    statements = Any[]
    function flatten(ex)
        if ex isa Expr && ex.head in (:toplevel, :block)
            foreach(flatten, ex.args)
        else
            push!(statements, ex)
        end
    end
    scoped(ex) = ex isa Expr && (ex.head in (:local, :return) || ex.head in (:toplevel, :block) && any(scoped, ex.args))
    # Local declarations and returns belong to one evaluation scope.
    scoped(parsed) && return observed_statement(mod, parsed, () -> Core.eval(mod, softscope(context_rewrite(parsed))))
    flatten(parsed)
    for st in statements
        if st isa LineNumberNode
            line = st
            continue
        end
        value = observed_statement(mod, st, () -> Core.eval(mod, Expr(:toplevel, line, softscope(context_rewrite(st)))))
    end
    return value
end

function include_observed(mod, path::AbstractString)
    refresh_workspace_packages!()
    full = abspath(path)
    call = length(get_kernel_state().execution_history) + 1
    note_file!(full, call)
    evidence = source_evidence(full)
    source_reads = get(task_local_storage(), :palette_source_reads, nothing)
    evidence === nothing || source_reads === nothing || push!(source_reads, evidence)
    before = binding_ids(mod)
    try
        return Base.include(mod, full)
    finally
        if evidence !== nothing
            after = Base.invokelatest(binding_ids, mod)
            defs = try file_definitions(full) catch; Tuple{String, Any}[] end
            statements = try top_statements(Meta.parseall(read(full, String))) catch; Any[] end
            declared = Set(Symbol(n) for (_, st) in defs for n in defined_names(st))
            union!(declared, reduce(union, (assigned_symbols(st) for st in statements); init=Set{Symbol}()))
            for (name, id) in after
                get(before, name, nothing) == id && !(name in declared) && continue
                BINDING_SEEN[name] = (id, call)
                BINDING_ORIGINS[name] = Dict{String, Any}("epoch" => TASK_EPOCH[], "source_digest" => evidence["sha256"],
                    "observed_call" => call, "sources" => Dict(full => evidence), "artifacts" => Dict{String, Any}[])
            end
            names_ = unique(reduce(vcat, (defined_names(st) for (_, st) in defs); init=String[]))
            push!(DEFINITION_LOG, Dict{String, Any}("call" => call, "kind" => "include", "code" => "include(" * repr(full) * ")",
                "path" => full, "stamp" => collect(file_stamp(full)), "sha256" => evidence["sha256"], "names" => names_,
                "requires" => unique(reduce(vcat, (reconstruction_dependencies(st) for (_, st) in defs); init=String[]))))
        end
    end
end

struct TrackedArtifact
    content::String
    observation::Dict{String, Any}
end

function read_artifact(path::AbstractString; authority_path=nothing)
    full = abspath(path)
    content = read(full, String)
    observation = Dict{String, Any}("path" => full, "digest" => bytes2hex(sha256(content)),
        "authority_path" => authority_path === nothing ? nothing : abspath(authority_path))
    if authority_path !== nothing
        record = JSON.parse(read(observation["authority_path"], String))
        all(k -> haskey(record, k), ("generation", "commit", "digest")) || error("artifact authority needs generation, commit and digest")
        record["digest"] == observation["digest"] || error("artifact does not match committed authority digest")
        observation["generation"] = record["generation"]
        observation["commit"] = record["commit"]
    end
    reads = get(task_local_storage(), :palette_artifact_reads, nothing)
    reads === nothing || push!(reads, observation)
    return TrackedArtifact(content, observation)
end

function artifact_changed(observation)
    path = observation["path"]
    isfile(path) && bytes2hex(open(sha256, path)) == observation["digest"] || return true
    authority = get(observation, "authority_path", nothing)
    authority === nothing && return false
    isfile(authority) || return true
    try
        record = JSON.parse(read(authority, String))
        return any(k -> get(record, k, nothing) != get(observation, k, nothing), ("generation", "commit", "digest"))
    catch
        return true
    end
end

function source_changed(evidence; strong=false)
    path = evidence["path"]
    isfile(path) || return true
    file_stamp(path) == Tuple(evidence["stamp"]) || return true
    return strong && get(evidence, "sha256", nothing) !== nothing && bytes2hex(open(sha256, path)) != evidence["sha256"]
end

function binding_sources(origin, call)
    haskey(origin, "sources") && return origin["sources"]
    return Dict{String, Any}(p => Dict{String, Any}("path" => p, "stamp" => collect(stamp), "sha256" => nothing, "kind" => "legacy_call")
        for (p, stamp) in get(CALL_FILES, call, Tuple{String, Tuple{Float64, Int}}[]))
end
