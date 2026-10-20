# Tools a session made, kept past it: a helper used in successful calls is
# promoted to a project tool (its source and evidence saved in the workspace),
# a human can approve it into the trusted directory through the patch broker,
# and every later session lists both from a small index and loads a tool's
# code only when it is first called.

const TOOL_NAME = r"^[A-Za-z_][A-Za-z0-9_!]*$"
const MIN_TOOL_USES = 3
const MAX_TOOL_USES = 32
# name => successful calls that used it, each with the hash of the source it had then
const TOOL_USES = Dict{String, Vector{Dict{String, Any}}}()
# name => the demand-load stub bound in the session module
const TOOL_STUBS = Dict{Symbol, Function}()
# name => (stamps of the tool's files, its index entry, its function) of a loaded tool
const LOADED_TOOLS = Dict{String, Tuple{Vector{Any}, Dict{String, Any}, Function}}()
# name => (the definition log entries, the source they give) last computed
const TOOL_SOURCES = Dict{String, Tuple{Vector{Dict{String, Any}}, Any}}()

project_tools_dir() = joinpath(WORKSPACE_ROOT[], ".palette", "tools")
trusted_tools_dir() = get(ENV, "PALETTE_TRUSTED_TOOLS", "")
# Where tools are listed from, in order, and whether the directory is the trusted one.
tool_dirs() = ((project_tools_dir(), false), (trusted_tools_dir(), true))
# A verified trusted tool wins over a project tool of the same name.
prefer(current, e, trusted::Bool) = current === nothing || trusted && e["verified"] ? e : current

# Line comments Expr printing leaves in (`#= none:1 =#` in a macro call).
strip_lines!(ex) = ex isa Expr ? (ex.head === :macrocall && length(ex.args) > 1 && ex.args[2] isa LineNumberNode &&
                                   (ex.args[2] = nothing); foreach(strip_lines!, ex.args); ex) : ex

is_tool_stub(mod::Module, sym::Symbol) = (f = get(TOOL_STUBS, sym, nothing)) !== nothing &&
    latest_isdefined(mod, sym) && latest_global(mod, sym) === f && length(methods(f)) == 1

# The session's own bindings: not the kernel's, not compiler-generated, not a tool stub.
session_binding(mod::Module, sym::Symbol) =
    !(sym === nameof(mod) || sym in KERNEL_BINDINGS || startswith(string(sym), '#') || is_tool_stub(mod, sym))

# A GlobalRef as the path that names it (`Core.@doc`), as source reads back.
unref(x) = x isa GlobalRef ? Expr(:., foldl((a, b) -> Expr(:., a, QuoteNode(b)), fullname(x.mod)), QuoteNode(x.name)) :
           x isa Expr ? Expr(x.head, map(unref, x.args)...) : x
clean_statement(st) = unref(strip_lines!(Base.remove_linenums!(st)))

# Definition statements a call's top-level code made for `name`, in order, as
# source without line comments, and its hash. `nothing` when there are none or
# one does not print back as source. Computed again only when the entries change.
function tool_source(name::AbstractString)
    entries = [e for e in DEFINITION_LOG if e["kind"] == "def" && name in e["names"]]
    isempty(entries) && return nothing
    cached = get(TOOL_SOURCES, name, nothing)
    cached !== nothing && length(cached[1]) == length(entries) && all(map(===, cached[1], entries)) && return cached[2]
    statements = [clean_statement(logged_statement(e)) for e in entries]
    # round_trips does not fit: the reparse has a line node in `Core.@doc` that clean_statement drops.
    src = if all(st -> clean_statement(Meta.parse(string(st))) == st, statements)
        code = join(string.(statements), "\n\n") * "\n"
        (code, bytes2hex(sha256(code)))
    end
    TOOL_SOURCES[name] = (entries, src)
    return src
end

"""
    note_tool_uses!(ex, call, session_request)

After a successful call: each function the session defined in an earlier call
that this call's non-definition statements name counts one use of its current
source. Uses live in the kernel's memory only.
"""
function note_tool_uses!(ex, call::Int, session_request)
    ex === nothing && return
    defined = Set(n for e in DEFINITION_LOG if e["kind"] == "def" && e["call"] != call for n in e["names"])
    isempty(defined) && return
    here = Set(n for e in DEFINITION_LOG if e["call"] == call for n in e["names"])
    refs = Set{Symbol}()
    for st in top_statements(ex)
        definition_kind(st) === nothing && symbol_references(st, refs)
    end
    for sym in refs
        name = string(sym)
        name in defined && !(name in here) || continue
        src = tool_source(name)
        src === nothing && continue
        uses = get!(Vector{Dict{String, Any}}, TOOL_USES, name)
        filter!(u -> u["source_sha256"] == src[2], uses)
        push!(uses, Dict{String, Any}("call" => call, "session_request" => session_request,
                                      "epoch" => TASK_EPOCH[], "source_sha256" => src[2]))
        length(uses) > MAX_TOOL_USES && popfirst!(uses)
    end
    return
end

# Globals the lowered code of `ex` refers to (GlobalRefs on 1.12, plain
# symbols on 1.10; arguments and locals are slots on both).
function free_globals(ex)
    m = Module(:ToolCheck)
    out = Set{Symbol}()
    walk(x) = x isa GlobalRef ? push!(out, x.name) : x isa Symbol ? push!(out, x) :
              x isa Core.CodeInfo ? foreach(walk, x.code) : x isa Expr ? foreach(walk, x.args) : nothing
    foreach(st -> walk(Meta.lower(m, st)), top_statements(ex))
    return out
end

# A fresh module holding a tool's source, with the same `Palette` turn code sees.
function tool_module(name::AbstractString, code::AbstractString, file::AbstractString)
    parsed = parse_complete(code; filename=file)
    check_ephemeral_source!(parsed)
    m = Module(Symbol("Tool_", name))
    Core.eval(m, :(const Palette = $(Api)))
    Core.eval(m, :(const Api = $(Api)))
    Core.eval(m, parsed)
    f = latest_isdefined(m, Symbol(name)) ? latest_global(m, Symbol(name)) : nothing
    f isa Function || error("$file does not define a function $name")
    return f
end

# A tool's docstring and the signatures of its methods, from its parsed source.
function tool_summary(name::AbstractString, ex)
    doc, sigs = "", String[]
    for st in top_statements(ex)
        if isempty(doc) && st isa Expr && st.head === :macrocall && definition_kind(st) == "def" &&
           st.args[3] isa String && name in defined_names(st)
            doc = String(strip(st.args[3]))
        end
        while st isa Expr && st.head === :macrocall
            st = st.args[end]
        end
        st isa Expr && st.head in (:function, :(=)) && push!(sigs, string(st.args[1]))
    end
    return doc, sigs
end

"""
    promote_tool(name; doc) -> Dict

Save session function `name` as a project tool in `.palette/tools/` of the
workspace, with the calls that used it as evidence. It must have been used by
$MIN_TOOL_USES successful calls since its last change and depend on no other
session binding.
"""
function promote_tool(name::Symbol; doc::Union{Nothing, AbstractString}=nothing)
    n = string(name)
    occursin(TOOL_NAME, n) || error("tool name $n must match $(TOOL_NAME.pattern)")
    src = tool_source(n)
    src === nothing && error("$n is not a function defined by a call's top-level code; only those can be promoted (not definitions from included files)")
    code, sha = src
    uses = filter(u -> u["source_sha256"] == sha, get(TOOL_USES, n, Dict{String, Any}[]))
    length(uses) >= MIN_TOOL_USES ||
        error("$n has $(length(uses)) successful call(s) using its current definition; promotion needs $MIN_TOOL_USES")
    mod = get_kernel_state().eval_module
    ex = parse_complete(code; filename=n)
    # Not session_binding: another tool's stub counts here, since a tool that
    # calls one would not load on its own.
    deps = sort!([string(s) for s in free_globals(ex) if s !== name && !(s in KERNEL_BINDINGS) && !startswith(string(s), '#') &&
                  latest_isdefined(mod, s) && Base.binding_module(mod, s) === mod])
    isempty(deps) || error("$n uses session bindings $(join(deps, ", ")); a tool must stand alone (pass them as arguments or define them inside it)")
    tool_module(n, code, n)
    dir = project_tools_dir()
    mkpath(dir)
    found_doc, signatures = tool_summary(n, ex)
    meta = Dict{String, Any}("name" => n, "signatures" => signatures,
        "doc" => something(doc, found_doc), "stage" => "project", "source_sha256" => sha,
        "evidence" => [Dict(k => u[k] for k in ("call", "session_request", "epoch")) for u in uses],
        "promoted" => Dict("epoch" => TASK_EPOCH[], "call" => next_call()))
    write(joinpath(dir, n * ".jl"), code)
    write(joinpath(dir, n * ".json"), JSON.json(meta, 2))
    return Dict{String, Any}("name" => n, "stage" => "project", "file" => relpath(joinpath(dir, n * ".jl"), WORKSPACE_ROOT[]),
                             "source_sha256" => sha, "evidence_count" => length(uses),
                             "next" => "Palette.trust(:$n) gives the palette_patch request a human approves to make it trusted")
end

# One index entry from a sidecar: what it claims, checked against the source's hash now.
function tool_entry(dir::AbstractString, file::AbstractString, trusted_dir::Bool)
    n = chopsuffix(file, ".json")
    src = joinpath(dir, n * ".jl")
    entry = Dict{String, Any}("name" => n, "stage" => "project", "verified" => false, "file" => src)
    meta = try
        read_json(joinpath(dir, file))
    catch e
        entry["reason"] = "unreadable sidecar: $(first(sprint(showerror, e), 200))"
        return entry
    end
    for k in ("signatures", "doc", "source_sha256")
        entry[k] = get(meta, k, nothing)
    end
    entry["evidence_count"] = length(get(meta, "evidence", ()))
    claimed = get(meta, "stage", "project")
    entry["reason"] = if !occursin(TOOL_NAME, n) || get(meta, "name", nothing) != n
        "the sidecar's name does not match its file"
    elseif !isfile(src)
        "$(n).jl is missing"
    elseif file_sha(src) != get(meta, "source_sha256", nothing)
        "$(n).jl changed since it was $(claimed == "trusted" ? "approved" : "promoted"); promote it again"
    else
        nothing
    end
    entry["verified"] = entry["reason"] === nothing
    if claimed == "trusted" && entry["verified"]
        if trusted_dir
            entry["stage"] = "trusted"
        else
            entry["reason"] = "claims trusted outside the trusted directory; listed as a project tool"
        end
    end
    return entry
end

"""
    tool_index() -> Dict{String, Dict}

Every project and trusted tool, by name, without loading any: name,
signatures, doc, stage and whether its source still matches its recorded
hash. A trusted tool wins over a project tool of the same name.
"""
function tool_index()
    out = Dict{String, Dict{String, Any}}()
    for (dir, trusted) in tool_dirs()
        (isempty(dir) || !isdir(dir)) && continue
        for f in sort!(readdir(dir))
            endswith(f, ".json") || continue
            e = tool_entry(dir, f, trusted)
            out[e["name"]] = prefer(get(out, e["name"], nothing), e, trusted)
        end
    end
    return out
end

"""
    install_tool_stubs!(mod)

Bind a small stub for each verified tool whose name `mod` does not already
see, so the first call of it loads the tool. Revived bindings win.
"""
function install_tool_stubs!(mod::Module)
    empty!(TOOL_STUBS)
    empty!(LOADED_TOOLS)
    for (n, e) in tool_index()
        e["verified"] || continue
        sym = Symbol(n)
        latest_isdefined(mod, sym) && continue
        TOOL_STUBS[sym] = Core.eval(mod, :(function $sym(args...; kw...)
            $(GlobalRef(Palette, :call_tool))($(QuoteNode(sym)), args, kw)
        end))
    end
    return
end

# This tool's index entry, read and hash-checked now.
function tool_lookup(n::AbstractString)
    found = nothing
    for (dir, trusted) in tool_dirs()
        !isempty(dir) && isfile(joinpath(dir, n * ".json")) || continue
        found = prefer(found, tool_entry(dir, n * ".json", trusted), trusted)
    end
    return found
end

# The stamps of every file that decides which copy of tool `n` loads.
tool_stamps(n::AbstractString) = Any[(p = joinpath(dir, n * ext); isfile(p) ? file_stamp(p) : nothing)
                                     for (dir, _) in tool_dirs() if !isempty(dir) for ext in (".json", ".jl")]

# The tool's entry and function. Its files are read and hashed again only when
# one of their stamps changed since it was loaded.
function loaded_tool(name::Symbol)
    n = string(name)
    stamps = tool_stamps(n)
    t = get(LOADED_TOOLS, n, nothing)
    t !== nothing && t[1] == stamps && return t[2], t[3], false
    e = tool_lookup(n)
    e === nothing && error("tool $n is no longer in the index")
    e["verified"] || error("tool $n cannot be loaded: $(e["reason"])")
    f = tool_module(n, read(e["file"], String), e["file"])
    LOADED_TOOLS[n] = (stamps, e, f)
    return e, f, true
end

"""
    call_tool(name, args, kw)

What a tool stub runs: check the tool against its recorded hash, load it the
first time, note it on this call's costs and X-Ray, and call it.
"""
function call_tool(name::Symbol, args, kw)
    e, f, loaded = loaded_tool(name)
    call = next_call()
    costs = get(TURN_COSTS, call, nothing)
    if costs !== nothing
        used = get!(Vector{Dict{String, Any}}, costs, "tools")
        any(u -> u["name"] == e["name"], used) ||
            push!(used, Dict{String, Any}("name" => e["name"], "stage" => e["stage"], "source_sha256" => e["source_sha256"],
                                          "evidence_count" => e["evidence_count"], "loaded" => loaded))
    end
    return Base.invokelatest(f, args...; kw...)
end

"""
    trust_request(name) -> Dict

The `palette_patch` prepare request that copies project tool `name` into the
trusted directory. The kernel cannot write there; a human approves the patch.
"""
function trust_request(name::Symbol)
    n = string(name)
    dir = trusted_tools_dir()
    isempty(dir) && error("no trusted tool directory: the operator sets PALETTE_TRUSTED_TOOLS")
    e = tool_entry(project_tools_dir(), n * ".json", false)
    e["verified"] || error("project tool $n is not verified: $(e["reason"])")
    meta = read_json(joinpath(project_tools_dir(), n * ".json"))
    meta["stage"] = "trusted"
    before(f) = (p = joinpath(dir, f); isfile(p) ? file_sha(p) : nothing)
    changes = [Dict{String, Any}("path" => n * ".jl", "before_sha256" => before(n * ".jl"), "content" => read(e["file"], String)),
               Dict{String, Any}("path" => n * ".json", "before_sha256" => before(n * ".json"), "content" => JSON.json(meta, 2))]
    return Dict{String, Any}("tool" => "palette_patch",
                             "arguments" => Dict{String, Any}("action" => "prepare", "target" => "trusted_tools", "changes" => changes),
                             "next" => "send these arguments to palette_patch; a human approves the patch, then the tool is trusted from the next session")
end

# Code graphs by root, refreshed on each use: only files whose stamp and hash
# changed are parsed again.
const CODE_GRAPHS = Dict{String, Pharos.CodeGraph}()
const MAX_CODE_GRAPHS = 4

"""
    codegraph(root = workspace) -> Pharos.CodeGraph

The definitions, calls, includes and imports of the Julia, Python and Rust
files under `root`, brought up to date first. The call's X-Ray cites the
graph's digest and which files were parsed again.
"""
function codegraph(root::AbstractString=WORKSPACE_ROOT[])
    key = Pharos._normroot(root)
    isdir(key) || error("$key is not a directory")
    g = get(CODE_GRAPHS, key, nothing)
    if g === nothing
        length(CODE_GRAPHS) >= MAX_CODE_GRAPHS && empty!(CODE_GRAPHS)
        g = CODE_GRAPHS[key] = Pharos.CodeGraph(key)
    else
        Pharos.refresh!(g)
    end
    costs = get(TURN_COSTS, next_call(), nothing)
    if costs !== nothing
        cited = get!(Vector{Dict{String, Any}}, costs, "codegraph")
        filter!(c -> c["root"] != key, cited)
        push!(cited, Dict{String, Any}("root" => key, "digest" => Pharos.digest(g), "files" => length(g.files),
                                       "reparsed_paths" => copy(g.last["reparsed_paths"]), "seconds" => g.last["seconds"]))
    end
    return g
end
