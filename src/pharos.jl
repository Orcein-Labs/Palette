module Pharos

using SHA
using Serialization

export CodeGraph, refresh!, definitions, callers, grep_defs, digest, source_files,
       save_index, load_index, load_or_build

const INDEX_CACHE_FORMAT = 1

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
    dirs::Dict{String,Float64}
    git_head::String
    untracked::Set{String}
    fs_type::Symbol
end

CodeGraph(root::String, files::Dict{String,FileEntry}, last::Dict{String,Any}) =
    CodeGraph(root, files, last, Dict{String,Float64}(), "", Set{String}(), :unknown)

_language(path::AbstractString) =
    endswith(path, ".jl") ? :julia :
    endswith(path, ".py") ? :python :
    endswith(path, ".rs") ? :rust :
    endswith(path, ".wgsl") ? :wgsl : nothing

function _normroot(root::AbstractString)
    r = rstrip(abspath(root), '/')
    isempty(r) ? "/" : r
end

# Fast pointer-wrapped SHA-256: avoids Julia SHA.jl scalar fallback on Base.CodeUnits{UInt8, String}.
# Memory safety: GC.@preserve keeps `s` alive; own=false ensures Julia never frees the pointer.
@inline function _sha256_hex(s::String)
    n = sizeof(s)
    n == 0 && return bytes2hex(sha256(UInt8[]))
    GC.@preserve s begin
        arr = unsafe_wrap(Array, pointer(s), n; own=false)
        return bytes2hex(sha256(arr))
    end
end
@inline _sha256_hex(s::AbstractString) = _sha256_hex(String(s))
@inline _sha256_hex(b::AbstractVector{UInt8}) = bytes2hex(sha256(b))

function _detect_fs_type(path::AbstractString)
    try
        target = abspath(path)
        best_mount = ""
        best_fs = :generic
        if isfile("/proc/mounts")
            for line in eachline("/proc/mounts")
                parts = split(line)
                length(parts) >= 3 || continue
                mountpoint = parts[2]
                fstype = parts[3]
                if (target == mountpoint || startswith(target, mountpoint * "/")) && length(mountpoint) > length(best_mount)
                    best_mount = mountpoint
                    best_fs = Symbol(fstype)
                end
            end
        end
        return best_fs
    catch
        return :generic
    end
end

function _git_head(root::AbstractString)
    Sys.which("git") === nothing && return ""
    try
        s = read(pipeline(`git -C $root rev-parse HEAD`, stderr=devnull), String)
        return strip(s)
    catch
        return ""
    end
end

function _git_status_uall(root::AbstractString)
    Sys.which("git") === nothing && return nothing
    try
        return read(pipeline(`git -C $root status --porcelain -uall`, stderr=devnull), String)
    catch
        return nothing
    end
end

function _record_dirs(root::AbstractString, rel_files)
    dirs = Dict{String,Float64}()
    for f in rel_files
        d = dirname(f)
        if !haskey(dirs, d)
            abs_d = isempty(d) ? root : joinpath(root, d)
            st = lstat(abs_d)
            (isfile(st) || isdir(st)) && (dirs[d] = Float64(st.mtime))
        end
    end
    return dirs
end

"""
Relative paths of .jl, .py, .rs and .wgsl files under root, sorted: the files git
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
    norm_root = String(_normroot(root))
    g = CodeGraph(norm_root, Dict{String,FileEntry}(), Dict{String,Any}(), Dict{String,Float64}(), "", Set{String}(), _detect_fs_type(norm_root))
    return refresh!(g; files=files)
end

"Re-scan changed files. Unchanged stamps are skipped, touched files are re-hashed, and only changed bytes are parsed."
function refresh!(g::CodeGraph; files=nothing, full::Bool=false)
    t0 = time()

    # -------------------------------------------------------------
    # Incremental Fast-Path (when no explicit files list requested)
    # -------------------------------------------------------------
    if files === nothing && !full && !isempty(g.files)
        head = _git_head(g.root)
        if !isempty(head) && head == g.git_head
            status_out = _git_status_uall(g.root)
            if status_out !== nothing
                if isempty(status_out) && isempty(g.untracked)
                    # Repository is 100% clean! Zero changes to tracked or untracked source files.
                    g.last = Dict{String,Any}(
                        "checked" => length(g.files), "hashed" => 0, "reparsed" => 0,
                        "reparsed_paths" => String[], "removed" => 0, "seconds" => time() - t0,
                        "files" => length(g.files), "strategy" => :git_clean)
                    return g
                end

                # Check if .gitignore or exclude rules changed
                status_lines = split(status_out, '\n'; keepempty=false)
                ignore_changed = any(l -> occursin(".gitignore", l) || occursin(".git/info/exclude", l), status_lines)
                if !ignore_changed
                    current_untracked = Set{String}()
                    dirty_paths = String[]
                    removed_paths = String[]
                    for line in status_lines
                        length(line) >= 4 || continue
                        code = line[1:2]
                        raw_path = strip(line[3:end])
                        if startswith(code, "??")
                            if _language(raw_path) !== nothing
                                push!(current_untracked, raw_path)
                                push!(dirty_paths, raw_path)
                            end
                        elseif startswith(code, "R") && occursin(" -> ", raw_path)
                            parts = split(raw_path, " -> ")
                            old_p, new_p = strip(parts[1]), strip(parts[2])
                            _language(old_p) !== nothing && push!(removed_paths, old_p)
                            _language(new_p) !== nothing && push!(dirty_paths, new_p)
                        elseif occursin('D', code)
                            _language(raw_path) !== nothing && push!(removed_paths, raw_path)
                        else
                            _language(raw_path) !== nothing && push!(dirty_paths, raw_path)
                        end
                    end

                    # Check for untracked files previously indexed that were deleted
                    for old_u in g.untracked
                        if !(old_u in current_untracked)
                            push!(removed_paths, old_u)
                        end
                    end
                    g.untracked = current_untracked

                    if isempty(dirty_paths) && isempty(removed_paths)
                        # Modified files exist, but none are source files!
                        g.last = Dict{String,Any}(
                            "checked" => length(g.files), "hashed" => 0, "reparsed" => 0,
                            "reparsed_paths" => String[], "removed" => 0, "seconds" => time() - t0,
                            "files" => length(g.files), "strategy" => :git_non_source)
                        return g
                    end

                    # Apply removals
                    removed_count = 0
                    for rp in removed_paths
                        if haskey(g.files, rp)
                            delete!(g.files, rp)
                            removed_count += 1
                        end
                    end

                    # Reparse dirty paths
                    reparsed_paths = String[]
                    py_pending = Tuple{String,String,String,NTuple{3,Float64},String}[]
                    hashed_count = 0

                    for rel in unique(dirty_paths)
                        lang = _language(rel)
                        lang === nothing && continue
                        abs = joinpath(g.root, rel)
                        st = lstat(abs)
                        isfile(st) || continue
                        stamp = (Float64(st.mtime), Float64(st.size), Float64(st.inode))
                        text = try read(abs, String) catch; continue end
                        hashed_count += 1
                        h = _sha256_hex(text)

                        e = get(g.files, rel, nothing)
                        if e !== nothing && e.sha256 == h
                            g.files[rel] = _restamp(e, stamp)
                            continue
                        end
                        push!(reparsed_paths, rel)
                        if lang === :python
                            push!(py_pending, (rel, abs, h, stamp, text))
                        elseif lang === :julia
                            g.files[rel] = _julia_entry(rel, text, h, stamp)
                        elseif lang === :rust
                            g.files[rel] = _rust_entry(rel, text, h, stamp)
                        else
                            g.files[rel] = _wgsl_entry(rel, text, h, stamp)
                        end
                    end

                    if !isempty(py_pending)
                        filter!(p -> !occursin('\n', p[2]), py_pending)
                        res = _python_extract([p[2] for p in py_pending])
                        for (i, (rel, abs, h, stamp, text)) in enumerate(py_pending)
                            g.files[rel] = res === nothing ? _python_regex_entry(rel, text, h, stamp) : _python_entry(rel, res[i], h, stamp)
                        end
                    end

                    g.last = Dict{String,Any}(
                        "checked" => length(g.files), "hashed" => hashed_count, "reparsed" => length(reparsed_paths),
                        "reparsed_paths" => reparsed_paths, "removed" => removed_count, "seconds" => time() - t0,
                        "files" => length(g.files), "strategy" => :git_incremental)
                    return g
                end
            end
        end
    end

    # -------------------------------------------------------------
    # Full Refresh / Trusted Fallback Path
    # -------------------------------------------------------------
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
        h = _sha256_hex(text)
        if e !== nothing && e.sha256 == h
            new[rel] = _restamp(e, stamp)
            continue
        end
        push!(reparsed, rel)
        if lang === :python
            push!(pending, (rel, abs, h, stamp, text))
        elseif lang === :julia
            new[rel] = _julia_entry(rel, text, h, stamp)
        elseif lang === :rust
            new[rel] = _rust_entry(rel, text, h, stamp)
        else
            new[rel] = _wgsl_entry(rel, text, h, stamp)
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
    g.git_head = _git_head(g.root)
    g.dirs = _record_dirs(g.root, keys(new))
    st_out = _git_status_uall(g.root)
    if st_out !== nothing
        g.untracked = Set{String}(strip(line[3:end]) for line in split(st_out, '\n'; keepempty=false) if startswith(line, "??") && _language(strip(line[3:end])) !== nothing)
    else
        g.untracked = Set{String}()
    end
    g.last = Dict{String,Any}(
        "checked" => checked, "hashed" => hashed, "reparsed" => length(reparsed),
        "reparsed_paths" => reparsed, "removed" => removed, "seconds" => time() - t0,
        "files" => length(new), "strategy" => :full)
    return g
end

function _unqualified_tail(n::AbstractString)
    idx = findlast('.', n)
    return idx === nothing ? n : SubString(n, nextind(n, idx))
end

# Matches a symbol name according to `match`:
#   :exact     — equals `name` or ends with "." * name (default)
#   :prefix    — starts with `name`, or its unqualified tail after "." starts with `name`
#   :substring — contains `name` anywhere
#   :regex     — matches Regex(name)
function _matcher(name::AbstractString, match::Symbol=:exact)
    s = String(name)
    if match === :exact
        suffix = "." * s
        return n -> n == s || endswith(n, suffix)
    elseif match === :prefix
        return n -> startswith(n, s) || startswith(_unqualified_tail(n), s)
    elseif match === :substring
        return n -> occursin(s, n)
    elseif match === :regex
        rx = Regex(s)
        return n -> occursin(rx, n)
    else
        throw(ArgumentError("unsupported match mode :$match (expected :exact, :prefix, :substring, or :regex)"))
    end
end

function _norm_relpath(p::AbstractString)
    s = replace(String(p), '\\' => '/')
    while startswith(s, "./")
        s = s[3:end]
    end
    return s == "." ? "" : s
end

function _prefix_match(path::AbstractString, norm_prefix::String)
    isempty(norm_prefix) && return true
    p = _norm_relpath(path)
    return startswith(p, norm_prefix)
end

"Definitions matching `name` (`match` in `:exact`, `:prefix`, `:substring`, `:regex`), as (path, definition) pairs."
function definitions(g::CodeGraph, name::Union{AbstractString, Symbol}; path_prefix::AbstractString="", match::Symbol=:exact)
    out = Tuple{String,Definition}[]
    matches = _matcher(String(name), match)
    prefix = _norm_relpath(path_prefix)
    for (path, e) in g.files
        _prefix_match(path, prefix) || continue
        for d in e.definitions
            matches(d.name) && push!(out, (path, d))
        end
    end
    return sort!(out; by=t -> (t[1], t[2].line))
end

"Calls whose callee matches `name` (`match` in `:exact`, `:prefix`, `:substring`, `:regex`), as (path, call) pairs."
function callers(g::CodeGraph, name::Union{AbstractString, Symbol}; path_prefix::AbstractString="", match::Symbol=:exact)
    out = Tuple{String,Call}[]
    matches = _matcher(String(name), match)
    prefix = _norm_relpath(path_prefix)
    for (path, e) in g.files
        _prefix_match(path, prefix) || continue
        for c in e.calls
            matches(c.callee) && push!(out, (path, c))
        end
    end
    return sort!(out; by=t -> (t[1], t[2].line))
end

function _norm_ext_token(x)
    s = lowercase(strip(String(x)))
    isempty(s) && return ""
    s in ("julia", ".julia") && return ".jl"
    s in ("python", ".python") && return ".py"
    s in ("rust", ".rust") && return ".rs"
    s == "wgsl" && return ".wgsl"
    return startswith(s, '.') ? s : "." * s
end

function _normalize_exts(exts)
    exts === nothing && return nothing
    out = Set{String}()
    if exts isa Union{AbstractString, Symbol}
        t = _norm_ext_token(exts)
        isempty(t) || push!(out, t)
    else
        for item in exts
            t = _norm_ext_token(item)
            isempty(t) || push!(out, t)
        end
    end
    return out
end

"""
    grep_defs(g::CodeGraph, pattern; path_prefix="", exts=nothing, ignorecase=false, match=:substring)
    grep_defs(pattern; root=nothing, path_prefix="", exts=nothing, ignorecase=false, match=:substring)

Concise structured matches (`(file, line, kind, name)` NamedTuples) for definitions
matching `pattern` (a substring `AbstractString` or `Regex`), optionally filtered
to relative paths matching `path_prefix` (with leading `./` normalized away) and
file extensions/languages `exts` (e.g. `(".rs", ".wgsl")` or `["jl", "wgsl"]`).
"""
function grep_defs(g::CodeGraph, pattern::Union{AbstractString, Regex};
                   path_prefix::AbstractString="", exts=nothing,
                   ignorecase::Bool=false, match::Symbol=:substring)
    out = NamedTuple{(:file, :line, :kind, :name), Tuple{String, Int, Symbol, String}}[]
    prefix = _norm_relpath(path_prefix)
    ext_set = _normalize_exts(exts)
    pred = if pattern isa Regex
        rx = ignorecase ? Regex(pattern.pattern, "i") : pattern
        name -> occursin(rx, name)
    elseif match === :substring
        needle = ignorecase ? lowercase(String(pattern)) : String(pattern)
        name -> occursin(needle, ignorecase ? lowercase(name) : name)
    elseif match === :regex
        rx = ignorecase ? Regex(String(pattern), "i") : Regex(String(pattern))
        name -> occursin(rx, name)
    elseif match in (:exact, :prefix)
        if ignorecase
            p = lowercase(String(pattern))
            if match === :exact
                suf = "." * p
                name -> (ln = lowercase(name); ln == p || endswith(ln, suf))
            else
                name -> (ln = lowercase(name); startswith(ln, p) || startswith(_unqualified_tail(ln), p))
            end
        else
            _matcher(String(pattern), match)
        end
    else
        throw(ArgumentError("unsupported match mode :$match (expected :substring, :exact, :prefix, or :regex)"))
    end
    for (path, e) in g.files
        _prefix_match(path, prefix) || continue
        norm_path = _norm_relpath(path)
        if ext_set !== nothing
            ext = lowercase(last(splitext(norm_path)))
            ext in ext_set || continue
        end
        for d in e.definitions
            pred(d.name) && push!(out, (file=norm_path, line=d.line, kind=d.kind, name=d.name))
        end
    end
    return sort!(out; by=t -> (t.file, t.line, t.name))
end

const _SUMMARY_MODULE_KINDS = (:module, :mod)
const _SUMMARY_TYPE_KINDS = (:struct, :abstract, :primitive, :class, :enum, :trait, :type, :alias)
const _SUMMARY_FUNCTION_KINDS = (:function, :fn, :macro)

function _default_summary_root()
    m = @__MODULE__
    p = parentmodule(m)
    if p !== m && isdefined(p, :default_codegraph_root)
        r = try
            p.default_codegraph_root()
        catch
            ""
        end
        !isempty(r) && isdir(r) && return r
    end
    return pwd()
end

function _summarize_graph(g::CodeGraph; max_files::Int=50)
    max_files >= 0 || throw(ArgumentError("max_files must be non-negative, got $max_files"))
    sorted_paths = sort!(collect(keys(g.files)))
    symbol_count = sum(e -> length(e.definitions), values(g.files); init=0)
    limit = min(max_files, length(sorted_paths))
    files = NamedTuple{(:path, :modules, :types, :functions, :includes)}[]
    sizehint!(files, limit)
    for i in 1:limit
        path = sorted_paths[i]
        e = g.files[path]
        mods = [(name=d.name, line=d.line) for d in e.definitions if d.kind in _SUMMARY_MODULE_KINDS]
        typs = [(name=d.name, kind=d.kind, line=d.line) for d in e.definitions if d.kind in _SUMMARY_TYPE_KINDS]
        fns = [(name=d.name, line=d.line) for d in e.definitions if d.kind in _SUMMARY_FUNCTION_KINDS]
        sort!(mods; by=t -> (t.line, t.name))
        sort!(typs; by=t -> (t.line, t.name))
        sort!(fns; by=t -> (t.line, t.name))
        push!(files, (
            path=_norm_relpath(path),
            modules=mods,
            types=typs,
            functions=fns,
            includes=copy(e.includes),
        ))
    end
    return (
        root=g.root,
        file_count=length(g.files),
        symbol_count=symbol_count,
        files=files,
    )
end

"""
    Pharos.summary(target=nothing; max_files=50)

Refresh or load a `CodeGraph` for `target` (a `CodeGraph`, directory path, or
`nothing`/`""` for the active workspace root) and return a compact structured
overview `(root, file_count, symbol_count, files)` where each file entry lists
`(path, modules, types, functions, includes)` with line numbers.
"""
function summary(g::CodeGraph; max_files::Int=50)
    refresh!(g)
    return _summarize_graph(g; max_files)
end

function summary(target::Union{Nothing, AbstractString}=nothing; max_files::Int=50)
    if target === nothing || isempty(strip(String(target)))
        m = @__MODULE__
        p = parentmodule(m)
        if p !== m && isdefined(p, :codegraph)
            r = _default_summary_root()
            g = try
                p.codegraph(r)
            catch
                CodeGraph(r)
            end
            return _summarize_graph(g; max_files)
        end
        return _summarize_graph(CodeGraph(_default_summary_root()); max_files)
    end
    return _summarize_graph(CodeGraph(String(target)); max_files)
end

"sha256 hex over the sorted \"path sha\" lines of every entry."
function digest(g::CodeGraph)
    lines = sort!(["$(path) $(e.sha256)" for (path, e) in g.files])
    return _sha256_hex(join(lines, "\n"))
end

function _pharos_private_root()
    m = @__MODULE__
    p = parentmodule(m)
    if p !== m && isdefined(p, :private_lab_root)
        return try
            p.private_lab_root()
        catch
            nothing
        end
    end
    for key in ("PALETTE_PRIVATE_DIR", "PALETTE_HOME")
        raw = strip(get(ENV, key, ""))
        isempty(raw) || return abspath(expanduser(raw))
    end
    return nothing
end

function _ensure_cache_dir!(dir::AbstractString)
    d = abspath(expanduser(String(dir)))
    priv = _pharos_private_root()
    if priv !== nothing
        m = @__MODULE__
        p = parentmodule(m)
        if p !== m && isdefined(p, :_ensure_private_dir!)
            p._ensure_private_dir!(d)
            return d
        end
        mkpath(d)
        Sys.isunix() && chmod(d, 0o700)
        return d
    end
    mkpath(d)
    return d
end

"""
    default_index_cache_path(root::AbstractString) -> String

Returns the default on-disk cache path for `Pharos` index persistence for `root`.
When Private Lab isolation (`PALETTE_HOME` / `PALETTE_PRIVATE_DIR`) is active,
strictly stores caches under `<PALETTE_HOME>/pharos/`.
"""
function default_index_cache_path(root::AbstractString)
    norm_root = _normroot(root)
    key = first(_sha256_hex(norm_root), 24)
    priv = _pharos_private_root()
    if priv !== nothing
        custom = strip(get(ENV, "PALETTE_PHAROS_CACHE_DIR", ""))
        base = isempty(custom) ? joinpath(priv, "pharos") : abspath(expanduser(custom))
        r = rstrip(abspath(priv), '/')
        (base == r || startswith(base, r * "/")) ||
            error("private lab containment violation: Pharos cache dir ($base) escapes PALETTE_HOME ($r)")
        return joinpath(base, "index_$(key).jls")
    end
    custom = strip(get(ENV, "PALETTE_PHAROS_CACHE_DIR", ""))
    if !isempty(custom)
        return joinpath(abspath(expanduser(custom)), "index_$(key).jls")
    end
    m = @__MODULE__
    p = parentmodule(m)
    if p !== m && isdefined(p, :STATE_DIR)
        sd = try
            p.STATE_DIR[]
        catch
            ""
        end
        if !isempty(sd)
            return joinpath(sd, "pharos", "index_$(key).jls")
        end
    end
    return joinpath(norm_root, ".palette", "pharos_index_$(key).jls")
end

"""
    save_index(g::CodeGraph, path::AbstractString = default_index_cache_path(g.root)) -> String

Atomically persists `g` to `path` (with `0o600` file permissions) so future
sessions can reload it and incrementally `refresh!` unchanged files in milliseconds.
"""
function save_index(g::CodeGraph, path::AbstractString=default_index_cache_path(g.root))
    dest = abspath(expanduser(String(path)))
    _ensure_cache_dir!(dirname(dest))
    tmp = dest * ".tmp." * string(getpid()) * "." * string(rand(UInt32))
    payload = (
        format=INDEX_CACHE_FORMAT,
        julia_version=string(VERSION),
        root=g.root,
        graph=g,
    )
    try
        open(tmp, "w") do io
            Serialization.serialize(io, payload)
        end
        Sys.isunix() && chmod(tmp, 0o600)
        mv(tmp, dest; force=true)
        Sys.isunix() && chmod(dest, 0o600)
    catch
        rm(tmp; force=true)
        rethrow()
    end
    return dest
end

"""
    load_index(path::AbstractString; expected_root=nothing) -> Union{Nothing, CodeGraph}

Loads a previously saved `CodeGraph` from `path`. Returns `nothing` if the file
is missing, corrupt, from an incompatible format, or for a different `expected_root`.
"""
function load_index(path::AbstractString; expected_root::Union{Nothing, AbstractString}=nothing)
    src = abspath(expanduser(String(path)))
    isfile(src) || return nothing
    payload = try
        open(src, "r") do io
            Serialization.deserialize(io)
        end
    catch
        return nothing
    end
    payload isa NamedTuple || return nothing
    get(payload, :format, 0) == INDEX_CACHE_FORMAT || return nothing
    g = get(payload, :graph, nothing)
    g isa CodeGraph || return nothing
    if expected_root !== nothing && _normroot(g.root) != _normroot(expected_root)
        return nothing
    end
    return g
end

"""
    load_or_build(root::AbstractString; cache_path=nothing, save::Bool=true, files=nothing) -> CodeGraph

Reloads a cached `CodeGraph` from `cache_path` (defaulting to
`default_index_cache_path(root)`) and calls `refresh!(g)` so unchanged files are
skipped via mtime/size/inode stamps (or git status fast-path) in milliseconds.
Falls back to building a fresh `CodeGraph` on cache miss and persists it when `save=true`.
"""
function load_or_build(
    root::AbstractString;
    cache_path::Union{Nothing, AbstractString}=nothing,
    save::Bool=true,
    files=nothing,
)
    norm_root = _normroot(root)
    cpath = cache_path === nothing ? default_index_cache_path(norm_root) : abspath(expanduser(String(cache_path)))
    cached = load_index(cpath; expected_root=norm_root)
    if cached !== nothing
        refresh!(cached; files=files)
        cached.last["cache_hit"] = true
        cached.last["cache_path"] = cpath
        if save && (get(cached.last, "reparsed", 0) > 0 || get(cached.last, "removed", 0) > 0 || get(cached.last, "hashed", 0) > 0)
            try
                save_index(cached, cpath)
            catch
            end
        end
        return cached
    end
    g = CodeGraph(norm_root; files=files)
    g.last["cache_hit"] = false
    g.last["cache_path"] = cpath
    if save
        try
            save_index(g, cpath)
        catch
        end
    end
    return g
end

function Base.show(io::IO, ::MIME"text/plain", g::CodeGraph)
    for lang in (:julia, :python, :rust, :wgsl)
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
        name === nothing && return nothing
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
fn_types = (ast.FunctionDef, ast.AsyncFunctionDef)

def scan_stmts(stmts, prefix, i, out):
    for st in stmts:
        t = type(st)
        if t in fn_types:
            name = prefix + st.name
            out.append(f"{i}\\tD\\t{name}\\tfunction\\t{st.lineno}\\n")
            scan_stmts(st.body, name + ".", i, out)
        elif t is ast.ClassDef:
            name = prefix + st.name
            out.append(f"{i}\\tD\\t{name}\\tclass\\t{st.lineno}\\n")
            scan_stmts(st.body, name + ".", i, out)
        elif t is ast.Import:
            for a in st.names:
                out.append(f"{i}\\tI\\t{a.name}\\n")
        elif t is ast.ImportFrom:
            out.append(f"{i}\\tI\\t{'.' * st.level + (st.module or '')}\\n")
        elif t is ast.If or t is ast.While or t is ast.For or t is ast.AsyncFor:
            scan_stmts(st.body, prefix, i, out)
            st.orelse and scan_stmts(st.orelse, prefix, i, out)
        elif t is ast.Try or t is getattr(ast, 'TryStar', ast.Try):
            scan_stmts(st.body, prefix, i, out)
            for h in st.handlers:
                scan_stmts(h.body, prefix, i, out)
            st.orelse and scan_stmts(st.orelse, prefix, i, out)
            st.finalbody and scan_stmts(st.finalbody, prefix, i, out)
        elif t is ast.With or t is ast.AsyncWith:
            scan_stmts(st.body, prefix, i, out)
        elif hasattr(ast, 'Match') and t is ast.Match:
            for c in st.cases:
                scan_stmts(c.body, prefix, i, out)

for i, p in enumerate(paths):
    if not p:
        continue
    try:
        with open(p, 'rb') as f:
            tree = ast.parse(f.read(), filename=p)
    except Exception as e:
        w('%d\\tE\\t%s\\n' % (i, ' '.join((type(e).__name__ + ': ' + str(e)).split())))
        continue
    out = []
    if tree and hasattr(tree, 'body'):
        scan_stmts(tree.body, '', i, out)
    w(''.join(out))
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
        length(f) >= 3 || continue
        idx = parse(Int, f[1]) + 1
        1 <= idx <= length(recs) || continue
        defs, imports, err = recs[idx]
        if f[2] == "D" && length(f) >= 5
            push!(defs, Definition(f[3], Symbol(f[4]), parse(Int, f[5])))
        elseif f[2] == "I"
            push!(imports, f[3])
        elseif f[2] == "E"
            err[] = f[3]
        end
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

# Shared comment stripper (// and nested /* ... */) preserving line numbers 1-to-1.
function _strip_c_comments(text::AbstractString; support_strings::Bool=false)::String
    s = String(text)
    buf = IOBuffer()
    i = firstindex(s)
    n = ncodeunits(s)
    block_depth = 0
    while i <= n
        c = s[i]
        nxt_i = nextind(s, i)
        if block_depth > 0
            if c == '*' && nxt_i <= n && s[nxt_i] == '/'
                write(buf, "  ")
                block_depth -= 1
                i = nextind(s, nxt_i)
            elseif c == '/' && nxt_i <= n && s[nxt_i] == '*'
                write(buf, "  ")
                block_depth += 1
                i = nextind(s, nxt_i)
            elseif c == '\n'
                write(buf, '\n')
                i = nxt_i
            else
                write(buf, ' ')
                i = nxt_i
            end
            continue
        end
        if c == '/' && nxt_i <= n
            c2 = s[nxt_i]
            if c2 == '/'
                while i <= n && s[i] != '\n'
                    i = nextind(s, i)
                end
                continue
            elseif c2 == '*'
                write(buf, "  ")
                block_depth = 1
                i = nextind(s, nxt_i)
                continue
            end
        end
        if support_strings
            if c == 'r' && (i == 1 || !(isletter(s[prevind(s, i)]) || isdigit(s[prevind(s, i)]) || s[prevind(s, i)] == '_'))
                j = nxt_i
                hashes = 0
                while j <= n && s[j] == '#'
                    hashes += 1
                    j = nextind(s, j)
                end
                if j <= n && s[j] == '"'
                    write(buf, SubString(s, i, j))
                    i = nextind(s, j)
                    while i <= n
                        ch = s[i]
                        write(buf, ch)
                        if ch == '"'
                            k = nextind(s, i)
                            matched = 0
                            while matched < hashes && k <= n && s[k] == '#'
                                write(buf, '#')
                                matched += 1
                                k = nextind(s, k)
                            end
                            if matched == hashes
                                i = k
                                break
                            end
                        end
                        i = nextind(s, i)
                    end
                    continue
                end
            elseif c == '"'
                write(buf, '"')
                i = nxt_i
                escaped = false
                while i <= n
                    ch = s[i]
                    write(buf, ch)
                    i = nextind(s, i)
                    if escaped
                        escaped = false
                    elseif ch == '\\'
                        escaped = true
                    elseif ch == '"'
                        break
                    end
                end
                continue
            elseif c == '\'' && nxt_i <= n
                if s[nxt_i] == '\\'
                    k1 = nextind(s, nxt_i)
                    k2 = k1 <= n ? nextind(s, k1) : n + 1
                    if k2 <= n && s[k2] == '\''
                        write(buf, SubString(s, i, k2))
                        i = nextind(s, k2)
                        continue
                    end
                else
                    k1 = nextind(s, nxt_i)
                    if k1 <= n && s[k1] == '\''
                        write(buf, SubString(s, i, k1))
                        i = nextind(s, k1)
                        continue
                    end
                end
            end
        end
        write(buf, c)
        i = nxt_i
    end
    return String(take!(buf))
end

function _skip_balanced(s::AbstractString, open_idx::Int, open_c::Char, close_c::Char; allow_strings::Bool=false)
    i = nextind(s, open_idx)
    n = ncodeunits(s)
    depth = 1
    in_str = false
    escaped = false
    while i <= n && depth > 0
        c = s[i]
        if allow_strings && in_str
            if escaped
                escaped = false
            elseif c == '\\'
                escaped = true
            elseif c == '"'
                in_str = false
            end
        elseif allow_strings && c == '"'
            in_str = true
        elseif c == open_c
            depth += 1
        elseif c == close_c
            depth -= 1
        end
        i = nextind(s, i)
    end
    return depth == 0 ? i : nothing
end

function _strip_rust_attrs(line::AbstractString)
    s = lstrip(line)
    while startswith(s, "#[")
        after = _skip_balanced(s, nextind(s, firstindex(s)), '[', ']'; allow_strings=true)
        after === nothing && return s
        s = lstrip(SubString(s, after))
    end
    return s
end

function _match_rust_impl(line::AbstractString)
    m = match(r"^\s*(?:unsafe\s+)?impl\b(.*)$", line)
    m === nothing && return nothing
    rest = lstrip(m.captures[1])
    if startswith(rest, "<")
        after = _skip_balanced(rest, firstindex(rest), '<', '>')
        after === nothing && return nothing
        rest = lstrip(SubString(rest, after))
    end
    m2 = match(r"^([^{;]+?)(?:\s*\bwhere\b.*|\s*\{.*|\s*;.*)?$", rest)
    m2 === nothing && return nothing
    target = strip(String(m2.captures[1]))
    return isempty(target) ? nothing : target
end

# Rust: line-based approximation with nested comment & attribute handling.

const _RS_ITEM = r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const|extern(?:\s+\"[^\"]*\")?)\s+)*(fn|struct|enum|trait|mod|type|const|static|union)\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)"
const _RS_MACRO = r"^\s*macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)"
const _RS_USE = r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+([^;]+);"
const _RS_KIND = Dict("fn" => :fn, "struct" => :struct, "union" => :struct, "enum" => :enum,
    "trait" => :trait, "mod" => :mod, "type" => :type, "const" => :const, "static" => :static)

function _rust_entry(rel::String, text::String, h::String, stamp::NTuple{3,Float64})
    defs = Definition[]
    imports = String[]
    cleaned = _strip_c_comments(text; support_strings=true)
    for (i, raw_line) in enumerate(split(cleaned, '\n'))
        line = _strip_rust_attrs(raw_line)
        isempty(line) && continue
        if (m = match(_RS_ITEM, line)) !== nothing
            push!(defs, Definition(String(m.captures[2]), _RS_KIND[String(m.captures[1])], i))
        elseif (impl_target = _match_rust_impl(line)) !== nothing
            push!(defs, Definition(impl_target, :impl, i))
        elseif (m = match(_RS_MACRO, line)) !== nothing
            push!(defs, Definition(String(m.captures[1]), :macro, i))
        end
        (m = match(_RS_USE, line)) !== nothing && push!(imports, strip(String(m.captures[1])))
    end
    return FileEntry(rel, :rust, h, stamp, true, defs, Call[], String[], imports, nothing)
end

# WGSL: module-scope extraction with nested comment, attribute, and brace-depth tracking.

function _strip_wgsl_attrs(line::AbstractString)
    s = lstrip(line)
    while startswith(s, "@")
        m = match(r"^@[A-Za-z_][A-Za-z0-9_]*(.*)$", s)
        m === nothing && break
        rest = lstrip(m.captures[1])
        if startswith(rest, "(")
            after = _skip_balanced(rest, firstindex(rest), '(', ')')
            after === nothing && return s
            rest = lstrip(SubString(rest, after))
        end
        s = rest
    end
    return s
end

const _WGSL_DECL = r"^(fn|struct|var|const|override|alias)\b(?:\s*<[^>]*>)?\s+([A-Za-z_][A-Za-z0-9_]*)\b"
const _WGSL_KIND = Dict(
    "fn" => :fn,
    "struct" => :struct,
    "var" => :var,
    "const" => :const,
    "override" => :override,
    "alias" => :alias,
)

function _wgsl_entry(rel::String, text::String, h::String, stamp::NTuple{3,Float64})
    defs = Definition[]
    cleaned = _strip_c_comments(text; support_strings=false)
    brace_depth = 0
    for (i, raw_line) in enumerate(split(cleaned, '\n'))
        start_depth = brace_depth
        brace_depth = max(0, brace_depth + count(==('{'), raw_line) - count(==('}'), raw_line))
        start_depth == 0 || continue
        line = _strip_wgsl_attrs(raw_line)
        isempty(line) && continue
        if (m = match(_WGSL_DECL, line)) !== nothing
            push!(defs, Definition(String(m.captures[2]), _WGSL_KIND[String(m.captures[1])], i))
        end
    end
    return FileEntry(rel, :wgsl, h, stamp, true, defs, Call[], String[], String[], nothing)
end

end # module Pharos
