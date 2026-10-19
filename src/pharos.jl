module Pharos

using SHA

export CodeGraph, refresh!, definitions, callers, digest, source_files

const SKIP_DIRS = Set(["vendor", "target", "node_modules", "__pycache__", "build", "dist", "_build", "venv"])
const MAX_FILES = 50_000
const PY_TIMEOUT = 120
const _DOT = Symbol(".")

struct Definition
    name::String
    kind::Symbol
    line::Int
end

struct Call
    caller::String
    callee::String
    line::Int
end

struct FileEntry
    path::String
    language::Symbol
    sha256::String
    stamp::NTuple{3,Float64}
    approximate::Bool
    definitions::Vector{Definition}
    calls::Vector{Call}
    includes::Vector{String}
    imports::Vector{String}
    error::Union{Nothing,String}
end

mutable struct CodeGraph
    root::String
    files::Dict{String,FileEntry}
    last::Dict{String,Any}
end

_language(path::AbstractString) =
    endswith(path, ".jl") ? :julia : endswith(path, ".py") ? :python : endswith(path, ".rs") ? :rust : nothing

function _normroot(root::AbstractString)
    r = rstrip(abspath(root), '/')
    isempty(r) ? "/" : r
end

"""
Relative paths of .jl, .py and .rs files under root, sorted: the files git
tracks or would track when root is in a work tree, else a walk that skips
hidden and build directories. Symlinks are not followed.
"""
function source_files(root::AbstractString)
    listed = try
        Sys.which("git") === nothing ? nothing :
            read(pipeline(`git -C $root ls-files -z --cached --others --exclude-standard`, stderr=devnull), String)
    catch
        nothing
    end
    listed === nothing && return sort!(_walk!(String[], String(root), ""))
    out = [String(p) for p in eachsplit(listed, '\0'; keepempty=false) if _language(p) !== nothing]
    return sort!(unique!(out))[1:min(end, MAX_FILES)]
end

function _walk!(out::Vector{String}, dir::String, rel::String)
    entries = try
        readdir(dir)
    catch
        return out
    end
    for name in sort!(entries)
        length(out) >= MAX_FILES && break
        full = joinpath(dir, name)
        sub = isempty(rel) ? name : rel * "/" * name
        islink(full) && continue
        if isdir(full)
            (startswith(name, ".") || name in SKIP_DIRS) && continue
            _walk!(out, full, sub)
        elseif isfile(full) && _language(name) !== nothing
            push!(out, sub)
        end
    end
    return out
end

_restamp(e::FileEntry, s::NTuple{3,Float64}) =
    FileEntry(e.path, e.language, e.sha256, s, e.approximate, e.definitions, e.calls, e.includes, e.imports, e.error)

"Build a graph over the source files under root (or the given relative `files`)."
function CodeGraph(root::AbstractString; files=nothing)
    g = CodeGraph(_normroot(root), Dict{String,FileEntry}(), Dict{String,Any}())
    return refresh!(g; files=files)
end

"Re-scan changed files. Unchanged stamps are skipped, touched files are re-hashed, and only changed bytes are parsed."
function refresh!(g::CodeGraph; files=nothing)
    t0 = time()
    rels = files === nothing ? source_files(g.root) : sort!(unique(String.(files)))
    old = g.files
    new = Dict{String,FileEntry}()
    checked = 0
    hashed = 0
    reparsed = String[]
    pending = Tuple{String,String,String,NTuple{3,Float64},String}[]
    for rel in rels
        lang = _language(rel)
        lang === nothing && continue
        abs = joinpath(g.root, rel)
        st = lstat(abs)
        isfile(st) || continue
        stamp = (Float64(st.mtime), Float64(st.size), Float64(st.inode))
        e = get(old, rel, nothing)
        if e !== nothing && e.stamp == stamp
            new[rel] = e
            checked += 1
            continue
        end
        text = try
            read(abs, String)
        catch
            continue
        end
        hashed += 1
        h = bytes2hex(sha256(text))
        if e !== nothing && e.sha256 == h
            new[rel] = _restamp(e, stamp)
            continue
        end
        push!(reparsed, rel)
        if lang === :python
            push!(pending, (rel, abs, h, stamp, text))
        elseif lang === :julia
            new[rel] = _julia_entry(rel, text, h, stamp)
        else
            new[rel] = _rust_entry(rel, text, h, stamp)
        end
    end
    if !isempty(pending)
        filter!(p -> !occursin('\n', p[2]), pending)
        res = _python_extract([p[2] for p in pending])
        for (i, (rel, abs, h, stamp, text)) in enumerate(pending)
            new[rel] = res === nothing ? _python_regex_entry(rel, text, h, stamp) : _python_entry(rel, res[i], h, stamp)
        end
    end
    removed = count(k -> !haskey(new, k), keys(old))
    g.files = new
    g.last = Dict{String,Any}(
        "checked" => checked, "hashed" => hashed, "reparsed" => length(reparsed),
        "reparsed_paths" => reparsed, "removed" => removed, "seconds" => time() - t0,
        "files" => length(new))
    return g
end

_matches(n::AbstractString, name::AbstractString) = n == name || endswith(n, "." * name)

"Definitions whose name equals `name` or ends with \".\" * name, as (path, definition) pairs."
function definitions(g::CodeGraph, name::AbstractString)
    out = Tuple{String,Definition}[]
    for (path, e) in g.files, d in e.definitions
        _matches(d.name, name) && push!(out, (path, d))
    end
    return sort!(out; by=t -> (t[1], t[2].line))
end

"Calls whose callee equals `name` or ends with \".\" * name, as (path, call) pairs."
function callers(g::CodeGraph, name::AbstractString)
    out = Tuple{String,Call}[]
    for (path, e) in g.files, c in e.calls
        _matches(c.callee, name) && push!(out, (path, c))
    end
    return sort!(out; by=t -> (t[1], t[2].line))
end

"sha256 hex over the sorted \"path sha\" lines of every entry."
function digest(g::CodeGraph)
    lines = sort!(["$(path) $(e.sha256)" for (path, e) in g.files])
    return bytes2hex(sha256(join(lines, "\n")))
end

function Base.show(io::IO, ::MIME"text/plain", g::CodeGraph)
    for lang in (:julia, :python, :rust)
        fs = [e for e in values(g.files) if e.language === lang]
        nd = sum(e -> length(e.definitions), fs; init=0)
        println(io, rpad(string(lang), 8), length(fs), " files  ", nd, " definitions")
    end
    println(io, "root    ", g.root)
    if isempty(g.last)
        print(io, "last    (no refresh)")
    else
        l = g.last
        print(io, "last    checked=", l["checked"], " hashed=", l["hashed"], " reparsed=", l["reparsed"],
            " removed=", l["removed"], " seconds=", round(l["seconds"]; digits=3))
    end
end

# Julia: walk the Meta.parseall tree.

mutable struct JState
    defs::Vector{Definition}
    calls::Vector{Call}
    includes::Vector{String}
    imports::Vector{String}
    error::Union{Nothing,String}
end

function _julia_entry(rel::String, text::String, h::String, stamp::NTuple{3,Float64})
    st = JState(Definition[], Call[], String[], String[], nothing)
    try
        _jscan!(st, Meta.parseall(text; filename=rel), 1, "")
    catch e
        st.error = sprint(showerror, e)
    end
    return FileEntry(rel, :julia, h, stamp, false, st.defs, st.calls, st.includes, st.imports, st.error)
end

# Scan x starting at `line`; return the line in effect after x.
function _jscan!(st::JState, x, line::Int, encl::String)::Int
    x isa LineNumberNode && return x.line
    x isa Expr || return line
    a = x.args
    h = x.head
    if h === :error || h === :incomplete
        st.error === nothing && (st.error = isempty(a) ? string(h) : string(a[1]))
        return line
    end
    r = _jspecial!(st, h, a, line, encl)
    r === nothing || return r
    for c in a
        line = _jscan!(st, c, line, encl)
    end
    return line
end

# Returns the new line, or nothing when the node is not handled here.
function _jspecial!(st::JState, h::Symbol, a::Vector{Any}, line::Int, encl::String)
    if h === :function && !isempty(a)
        core = _sig_core(a[1])
        name = _def_name(core)
        if name === nothing
            line = _jscan!(st, a[1], line, encl)
            for r in a[2:end]
                line = _jscan!(st, r, line, encl)
            end
            return line
        end
        return _jdef!(st, :function, name, core, a[2:end], line, encl)
    elseif h === :(=) && length(a) == 2
        core = _sig_core(a[1])
        if core isa Expr && core.head === :call
            name = _def_name(core)
            name === nothing || return _jdef!(st, :function, name, core, [a[2]], line, encl)
        end
        return nothing
    elseif h === :macro && !isempty(a)
        core = _sig_core(a[1])
        name = _def_name(core)
        name === nothing && return nothing
        return _jdef!(st, :macro, "@" * name, core, a[2:end], line, encl)
    elseif h === :struct && length(a) == 3
        name = _type_name(a[2])
        name === nothing && return nothing
        push!(st.defs, Definition(name, :struct, line))
        return _jscan!(st, a[3], line, name)
    elseif (h === :abstract || h === :primitive) && !isempty(a)
        name = _type_name(a[1])
        name === nothing && return nothing
        push!(st.defs, Definition(name, h, line))
        return line
    elseif h === :const && length(a) == 1 && a[1] isa Expr && a[1].head === :(=) && length(a[1].args) == 2
        for n in _const_names(a[1].args[1])
            push!(st.defs, Definition(n, :const, line))
        end
        return _jscan!(st, a[1].args[2], line, encl)
    elseif h === :module && length(a) == 3 && a[2] isa Symbol
        name = string(a[2])
        push!(st.defs, Definition(name, :module, line))
        return _jscan!(st, a[3], line, name)
    elseif h === :call && !isempty(a)
        name = _callee_name(a[1])
        if name === nothing
            line = _jscan!(st, a[1], line, encl)
        else
            push!(st.calls, Call(encl, name, line))
            name == "include" && _jinclude!(st, a)
        end
        for c in a[2:end]
            line = _jscan!(st, c, line, encl)
        end
        return line
    elseif h === _DOT && length(a) == 2 && a[2] isa Expr && a[2].head === :tuple
        name = _callee_name(a[1])
        name === nothing && return nothing
        push!(st.calls, Call(encl, name, line))
        for c in a[2].args
            line = _jscan!(st, c, line, encl)
        end
        return line
    elseif h === :using || h === :import
        for c in a
            p = c isa Expr && c.head === :(:) && !isempty(c.args) ? c.args[1] : c
            p isa Expr && p.head === _DOT && push!(st.imports, _modpath(p))
        end
        return line
    end
    return nothing
end

function _jdef!(st::JState, kind::Symbol, name::String, core, rest, line::Int, encl::String)::Int
    push!(st.defs, Definition(name, kind, line))
    if core isa Expr && core.head === :call
        for c in core.args[2:end]
            line = _jscan!(st, c, line, name)
        end
    end
    for r in rest
        line = _jscan!(st, r, line, name)
    end
    return line
end

function _jinclude!(st::JState, a::Vector{Any})
    length(a) in (2, 3) && a[end] isa String && push!(st.includes, a[end])
end

# Strip where clauses and return annotations down to the signature core.
function _sig_core(x)
    while x isa Expr && (x.head === :where || x.head === :(::)) && !isempty(x.args)
        x = x.args[1]
    end
    return x
end

function _def_name(c)
    c isa Expr && c.head === :call && !isempty(c.args) && return _callee_name(c.args[1])
    return _dotted(c)
end

function _callee_name(x)
    if x isa Expr && x.head === :curly && !isempty(x.args)
        return _callee_name(x.args[1])
    end
    return _dotted(x)
end

function _dotted(x)
    x isa Symbol && return string(x)
    if x isa Expr && x.head === _DOT && length(x.args) == 2
        l = _dotted(x.args[1])
        r = x.args[2] isa QuoteNode ? x.args[2].value : x.args[2]
        return l !== nothing && r isa Symbol ? l * "." * string(r) : nothing
    end
    return nothing
end

function _type_name(x)
    while x isa Expr && (x.head === :<: || x.head === :curly) && !isempty(x.args)
        x = x.args[1]
    end
    return x isa Symbol ? string(x) : nothing
end

function _const_names(x)
    x isa Symbol && return String[string(x)]
    x isa Expr && x.head === :(::) && !isempty(x.args) && return _const_names(x.args[1])
    if x isa Expr && x.head === :tuple
        return reduce(vcat, (_const_names(c) for c in x.args); init=String[])
    end
    return String[]
end

function _modpath(p::Expr)
    dots = count(x -> x === _DOT, p.args)
    names = String[string(x isa QuoteNode ? x.value : x) for x in p.args if x !== _DOT && (x isa Symbol || x isa QuoteNode)]
    return repeat(".", dots) * join(names, ".")
end

# Python: one python3 -I subprocess per refresh, falling back to a regex scan.

# stdin: one absolute path per line. stdout: "<index>\tD\tname\tkind\tline",
# "<index>\tI\tmodule" or "<index>\tE\tmessage" lines.
const _PY_SCRIPT = """
import ast, sys
paths = sys.stdin.read().split('\\n')
w = sys.stdout.write
for i, p in enumerate(paths):
    try:
        with open(p, 'rb') as f:
            tree = ast.parse(f.read(), filename=p)
    except Exception as e:
        w('%d\\tE\\t%s\\n' % (i, ' '.join((type(e).__name__ + ': ' + str(e)).split())))
        continue
    def visit(node, prefix):
        for ch in ast.iter_child_nodes(node):
            if isinstance(ch, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                kind = 'class' if isinstance(ch, ast.ClassDef) else 'function'
                w('%d\\tD\\t%s\\t%s\\t%d\\n' % (i, prefix + ch.name, kind, ch.lineno))
                visit(ch, prefix + ch.name + '.')
            else:
                visit(ch, prefix)
    visit(tree, '')
    for n in ast.walk(tree):
        if isinstance(n, ast.Import):
            for a in n.names:
                w('%d\\tI\\t%s\\n' % (i, a.name))
        elif isinstance(n, ast.ImportFrom):
            w('%d\\tI\\t%s\\n' % (i, '.' * n.level + (n.module or '')))
"""

# One record per path, or nothing when python3 is missing, fails or times out.
function _python_extract(abs_paths::Vector{String})
    Sys.which("python3") === nothing && return nothing
    proc = try
        open(pipeline(`python3 -I -c $(_PY_SCRIPT)`, stderr=devnull), "r+")
    catch
        return nothing
    end
    timer = Timer(_ -> kill(proc), PY_TIMEOUT)
    out = try
        write(proc.in, join(abs_paths, '\n'))
        close(proc.in)
        read(proc.out, String)
    catch
        ""
    end
    wait(proc)
    close(timer)
    success(proc) || return nothing
    recs = [(Definition[], String[], Ref{Union{Nothing,String}}(nothing)) for _ in abs_paths]
    for line in eachsplit(out, '\n'; keepempty=false)
        f = split(line, '\t')
        defs, imports, err = recs[parse(Int, f[1]) + 1]
        f[2] == "D" ? push!(defs, Definition(f[3], Symbol(f[4]), parse(Int, f[5]))) :
        f[2] == "I" ? push!(imports, f[3]) : (err[] = f[3])
    end
    return recs
end

_python_entry(rel::String, (defs, imports, err), h::String, stamp::NTuple{3,Float64}) =
    FileEntry(rel, :python, h, stamp, false, defs, Call[], String[], imports, err[])

const _PY_DEF = r"^\s*(async\s+)?def\s+(\w+)"
const _PY_CLASS = r"^\s*class\s+(\w+)"
const _PY_IMPORT = r"^\s*(?:from|import)\s+([\w.]+)"

function _python_regex_entry(rel::String, text::String, h::String, stamp::NTuple{3,Float64})
    defs = Definition[]
    imports = String[]
    for (i, line) in enumerate(split(text, '\n'))
        if (m = match(_PY_DEF, line)) !== nothing
            push!(defs, Definition(String(m.captures[2]), :function, i))
        elseif (m = match(_PY_CLASS, line)) !== nothing
            push!(defs, Definition(String(m.captures[1]), :class, i))
        end
        (m = match(_PY_IMPORT, line)) !== nothing && push!(imports, String(m.captures[1]))
    end
    return FileEntry(rel, :python, h, stamp, true, defs, Call[], String[], imports, nothing)
end

# Rust: line-based approximation.

const _RS_ITEM = r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const|extern(?:\s+\"[^\"]*\")?)\s+)*(fn|struct|enum|trait|mod|type|const|static|union)\s+([A-Za-z_][A-Za-z0-9_]*)"
const _RS_IMPL = r"^\s*(?:unsafe\s+)?impl\b(?:<[^>]*>)?\s+([^{;]+?)\s*(?:\{.*)?$"
const _RS_MACRO = r"^\s*macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)"
const _RS_USE = r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+([^;]+);"
const _RS_KIND = Dict("fn" => :fn, "struct" => :struct, "union" => :struct, "enum" => :enum,
    "trait" => :trait, "mod" => :mod, "type" => :type, "const" => :const, "static" => :static)

function _rust_entry(rel::String, text::String, h::String, stamp::NTuple{3,Float64})
    defs = Definition[]
    imports = String[]
    for (i, line) in enumerate(split(text, '\n'))
        startswith(lstrip(line), "//") && continue
        if (m = match(_RS_ITEM, line)) !== nothing
            push!(defs, Definition(String(m.captures[2]), _RS_KIND[String(m.captures[1])], i))
        elseif (m = match(_RS_IMPL, line)) !== nothing
            push!(defs, Definition(strip(String(m.captures[1])), :impl, i))
        elseif (m = match(_RS_MACRO, line)) !== nothing
            push!(defs, Definition(String(m.captures[1]), :macro, i))
        end
        (m = match(_RS_USE, line)) !== nothing && push!(imports, strip(String(m.captures[1])))
    end
    return FileEntry(rel, :rust, h, stamp, true, defs, Call[], String[], imports, nothing)
end

end # module Pharos
