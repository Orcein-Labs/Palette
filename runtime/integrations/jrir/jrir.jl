# ===========================================================================
# JRIR — Julia ↔ Rust Intermediate Representation & Zero-Copy LLVM Bridge
#
# Pure-Julia capability ("dependencies": []) providing:
# 1. Canonical JRIR Type & Signature Schema (`JRIRField`, `JRIRStruct`,
#    `JRIRParam`, `JRIRFn`, `JRIRModule`) with bidirectional Rust ↔ Julia ↔
#    LLVM IR type mapping and automatic slice lowering (`&[T]` / `&mut [T]`
#    → `(ptr::Ptr{T}, len::UInt64)`).
# 2. Deterministic System-V / C-ABI Layout Calculator & Verifier
#    (`compute_c_layout`, `verify_julia_struct_layout`) checking `isbitstype`,
#    `sizeof`, `Base.datatype_alignment`, and per-field `fieldoffset`.
# 3. Cross-Version LLVM 19–23 → LLVM 18 IR Normalizer (`normalize_llvm_ir`)
#    bridging `rustc` (LLVM 19–22+) and `clang` (LLVM 19–23+) textual `.ll`
#    into `Base.llvmcall` with `alwaysinline`, enabling cross-language loop
#    unrolling and AVX2 FMA vectorization with zero `callq` barrier and
#    0 bytes allocated.
# ===========================================================================

using LinearAlgebra
using Libdl

const _JRIR_VERSION = "0.1.0"
const _JRIR_MAX_MODULES = 32
const _JRIR_MAX_SOURCE_BYTES = 262_144
const _JRIR_MAX_IR_BYTES = 1_048_576
const _JRIR_MAX_NAME_BYTES = 128

# ---------------------------------------------------------------------------
# 1. Bidirectional Rust ↔ Julia ↔ LLVM Type Table & C-ABI Layout Engine
# ---------------------------------------------------------------------------

struct JRIRPrimSpec
    rust_type::String
    julia_type::DataType
    llvm_type::String
    size_bytes::Int
    align_bytes::Int
end

const _JRIR_PRIMS = Dict{String,JRIRPrimSpec}(
    "f32"   => JRIRPrimSpec("f32",   Float32, "float",  4, 4),
    "f64"   => JRIRPrimSpec("f64",   Float64, "double", 8, 8),
    "i8"    => JRIRPrimSpec("i8",    Int8,    "i8",     1, 1),
    "u8"    => JRIRPrimSpec("u8",    UInt8,   "i8",     1, 1),
    "i16"   => JRIRPrimSpec("i16",   Int16,   "i16",    2, 2),
    "u16"   => JRIRPrimSpec("u16",   UInt16,  "i16",    2, 2),
    "i32"   => JRIRPrimSpec("i32",   Int32,   "i32",    4, 4),
    "u32"   => JRIRPrimSpec("u32",   UInt32,  "i32",    4, 4),
    "i64"   => JRIRPrimSpec("i64",   Int64,   "i64",    8, 8),
    "u64"   => JRIRPrimSpec("u64",   UInt64,  "i64",    8, 8),
    "isize" => JRIRPrimSpec("isize", Int64,   "i64",    sizeof(Int), sizeof(Int)),
    "usize" => JRIRPrimSpec("usize", UInt64,  "i64",    sizeof(UInt), sizeof(UInt)),
    "bool"  => JRIRPrimSpec("bool",  Bool,    "i8",     1, 1),
    "()"    => JRIRPrimSpec("()",    Cvoid,   "void",   0, 1),
    "void"  => JRIRPrimSpec("void",  Cvoid,   "void",   0, 1),
)

struct JRIRField
    name::String
    rust_type::String
    size_bytes::Int
    align_bytes::Int
    offset_bytes::Int
end

struct JRIRStruct
    name::String
    fields::Vector{JRIRField}
    size_bytes::Int
    align_bytes::Int
    padding_bytes::Int
end

struct JRIRParam
    name::String
    rust_type::String
    abi_kind::Symbol      # :scalar, :ptr, :mut_ptr, :slice_ptr, :slice_len
    elem_type::String     # underlying scalar or struct name
    llvm_type::String     # "float", "i64", "ptr", etc.
    julia_abi_type::DataType
end

struct JRIRFn
    name::String
    params::Vector{JRIRParam}
    ret_rust_type::String
    ret_llvm_type::String
    ret_julia_type::DataType
    normalized_ir::String
    compiled_caller::Any
end

mutable struct JRIRModule
    name::String
    rust_source::String
    raw_llvm_ir::String
    structs::Dict{String,JRIRStruct}
    functions::Dict{String,JRIRFn}
end

const _JRIR_MODULES = Dict{String,JRIRModule}()
const _JRIR_LOCK = ReentrantLock()

@inline function _align_up(offset::Int, align::Int)::Int
    return (offset + align - 1) & ~(align - 1)
end

function _resolve_type_size_align(
    rust_ty::AbstractString,
    known_structs::Dict{String,JRIRStruct},
)::Tuple{Int,Int}
    t = strip(rust_ty)
    if haskey(_JRIR_PRIMS, t)
        spec = _JRIR_PRIMS[t]
        return (spec.size_bytes, spec.align_bytes)
    elseif haskey(known_structs, t)
        st = known_structs[t]
        return (st.size_bytes, st.align_bytes)
    elseif startswith(t, "*") || startswith(t, "&")
        return (sizeof(Ptr{Cvoid}), sizeof(Ptr{Cvoid}))
    end
    throw(ArgumentError("unknown JRIR type '$t' (must be primitive, pointer/ref, or previously declared #[repr(C)] struct)"))
end

"""
    compute_c_layout(name::AbstractString, raw_fields::Vector{Tuple{String,String}};
                     known_structs=Dict{String,JRIRStruct}()) -> JRIRStruct

Compute exact System-V / C-ABI `#[repr(C)]` struct layout (`offset_bytes`,
`size_bytes`, `align_bytes`, and `padding_bytes`).
"""
function compute_c_layout(
    name::AbstractString,
    raw_fields::Vector{Tuple{String,String}};
    known_structs::Dict{String,JRIRStruct}=Dict{String,JRIRStruct}(),
)::JRIRStruct
    isempty(raw_fields) && throw(ArgumentError("struct '$name' must have at least one field"))
    fields = Vector{JRIRField}(undef, length(raw_fields))
    offset = 0
    max_align = 1
    payload_bytes = 0

    for (idx, (fname, ftype)) in enumerate(raw_fields)
        sz, al = _resolve_type_size_align(ftype, known_structs)
        sz > 0 || throw(ArgumentError("zero-sized field '$fname::$ftype' is not valid in #[repr(C)] struct '$name'"))
        offset = _align_up(offset, al)
        fields[idx] = JRIRField(fname, ftype, sz, al, offset)
        offset += sz
        payload_bytes += sz
        max_align = max(max_align, al)
    end

    total_size = _align_up(offset, max_align)
    padding = total_size - payload_bytes
    return JRIRStruct(String(name), fields, total_size, max_align, padding)
end

"""
    verify_julia_struct_layout(jstruct::JRIRStruct, ::Type{T}) where {T} -> NamedTuple

Verify that Julia type `T` is an `isbitstype` whose `sizeof`, `datatype_alignment`,
field count, field sizes, and `fieldoffset`s match `jstruct` byte-for-byte.
Throws `DimensionMismatch` or `ArgumentError` on any ABI mismatch.
"""
function verify_julia_struct_layout(jstruct::JRIRStruct, ::Type{T}) where {T}
    isbitstype(T) || throw(ArgumentError("Julia type $T is not isbitstype; cannot share zero-copy #[repr(C)] memory"))
    n_fields = fieldcount(T)
    n_fields == length(jstruct.fields) || throw(DimensionMismatch(
        "field count mismatch for $(jstruct.name) vs $T: Rust has $(length(jstruct.fields)), Julia has $n_fields"
    ))
    sizeof(T) == jstruct.size_bytes || throw(DimensionMismatch(
        "sizeof mismatch for $(jstruct.name) vs $T: Rust #[repr(C)] is $(jstruct.size_bytes) B, Julia is $(sizeof(T)) B"
    ))
    jl_align = Int(Base.datatype_alignment(T))
    jl_align == jstruct.align_bytes || throw(DimensionMismatch(
        "alignment mismatch for $(jstruct.name) vs $T: Rust #[repr(C)] is $(jstruct.align_bytes) B, Julia is $(jl_align) B"
    ))
    for (i, rf) in enumerate(jstruct.fields)
        jl_off = Int(fieldoffset(T, i))
        jl_sz = sizeof(fieldtype(T, i))
        jl_off == rf.offset_bytes || throw(DimensionMismatch(
            "fieldoffset mismatch at field $i ($(rf.name)): Rust offset is $(rf.offset_bytes) B, Julia offset is $(jl_off) B"
        ))
        jl_sz == rf.size_bytes || throw(DimensionMismatch(
            "field size mismatch at field $i ($(rf.name)): Rust size is $(rf.size_bytes) B, Julia size is $(jl_sz) B"
        ))
    end
    return (
        valid = true,
        struct_name = jstruct.name,
        julia_type = string(T),
        size_bytes = jstruct.size_bytes,
        align_bytes = jstruct.align_bytes,
        padding_bytes = jstruct.padding_bytes,
        field_offsets = [f.name => f.offset_bytes for f in jstruct.fields],
    )
end

# ---------------------------------------------------------------------------
# 2. Rust Source Schema Parser (`#[repr(C)] struct` & `extern "C" fn`)
# ---------------------------------------------------------------------------

function _parse_param_abi(pname::AbstractString, ptype_raw::AbstractString)::Vector{JRIRParam}
    t = strip(ptype_raw)
    # 1. Rust slices: `&[T]` or `&mut [T]` -> lower to 2 ABI params: (ptr: Ptr{T}, len: UInt64)
    m_slice = match(r"^&\s*(mut\s+)?\[\s*([A-Za-z_][A-Za-z0-9_]*)\s*\]$", t)
    if m_slice !== nothing
        is_mut = m_slice.captures[1] !== nothing
        elem = String(m_slice.captures[2])
        el_jl = haskey(_JRIR_PRIMS, elem) ? _JRIR_PRIMS[elem].julia_type : Cvoid
        return [
            JRIRParam(String(pname) * "_ptr", t, is_mut ? :mut_slice_ptr : :slice_ptr, elem, "ptr", Ptr{el_jl}),
            JRIRParam(String(pname) * "_len", "usize", :slice_len, "usize", "i64", UInt64),
        ]
    end
    # 2. References & raw pointers: `&T`, `&mut T`, `*const T`, `*mut T`
    m_ptr = match(r"^(?:&\s*(mut\s+)?|\*\s*(const|mut)\s+)([A-Za-z_][A-Za-z0-9_]*)$", t)
    if m_ptr !== nothing
        is_mut = (m_ptr.captures[1] !== nothing) || (m_ptr.captures[2] == "mut")
        elem = String(m_ptr.captures[3])
        el_jl = haskey(_JRIR_PRIMS, elem) ? _JRIR_PRIMS[elem].julia_type : Cvoid
        return [JRIRParam(String(pname), t, is_mut ? :mut_ptr : :ptr, elem, "ptr", Ptr{el_jl})]
    end
    # 3. Primitives
    if haskey(_JRIR_PRIMS, t)
        spec = _JRIR_PRIMS[t]
        return [JRIRParam(String(pname), t, :scalar, t, spec.llvm_type, spec.julia_type)]
    end
    throw(ArgumentError("unsupported Rust parameter type '$t' for parameter '$pname'"))
end

function _parse_return_abi(ret_raw::AbstractString)::Tuple{String,String,DataType}
    t = isempty(strip(ret_raw)) ? "()" : strip(ret_raw)
    if haskey(_JRIR_PRIMS, t)
        spec = _JRIR_PRIMS[t]
        return (spec.rust_type, spec.llvm_type, spec.julia_type)
    elseif startswith(t, "*") || startswith(t, "&")
        return (String(t), "ptr", Ptr{Cvoid})
    end
    throw(ArgumentError("unsupported Rust return type '$t'"))
end

"""
    parse_rust_jrir(rust_src::AbstractString) -> (structs, functions)

Parse `#[repr(C)] pub struct` declarations and `pub (unsafe)? extern "C" fn`
signatures from Rust source text into canonical JRIR descriptors.
"""
function parse_rust_jrir(rust_src::AbstractString)
    ncodeunits(rust_src) <= _JRIR_MAX_SOURCE_BYTES ||
        throw(ArgumentError("rust_src exceeds maximum size ($_JRIR_MAX_SOURCE_BYTES bytes)"))

    structs = Dict{String,JRIRStruct}()
    functions = Dict{String,JRIRFn}()

    # Strip line comments for clean structural parsing
    clean_lines = String[]
    for line in eachsplit(rust_src, '\n')
        c_idx = findfirst("//", line)
        push!(clean_lines, c_idx === nothing ? String(line) : String(SubString(line, 1, first(c_idx) - 1)))
    end
    cleaned = join(clean_lines, "\n")

    # 1. Extract #[repr(C)] structs
    for m in eachmatch(r"#\s*\[\s*repr\s*\(\s*C\s*\)\s*\]\s*(?:pub\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{([^}]*)\}"s, cleaned)
        sname = String(m.captures[1])
        body = m.captures[2]
        raw_fields = Tuple{String,String}[]
        for fpart in eachsplit(body, ',')
            fstr = strip(fpart)
            isempty(fstr) && continue
            mf = match(r"^(?:pub\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.+)$", fstr)
            mf === nothing && throw(ArgumentError("invalid field syntax '$fstr' in struct '$sname'"))
            push!(raw_fields, (String(mf.captures[1]), strip(String(mf.captures[2]))))
        end
        structs[sname] = compute_c_layout(sname, raw_fields; known_structs=structs)
    end

    # 2. Extract extern "C" fn signatures (or pub fn kernels)
    fn_pat = r"(?:pub\s+)?(?:unsafe\s+)?(?:extern\s+\"C\"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?:->\s*([^{]+))?\s*\{"s
    for m in eachmatch(fn_pat, cleaned)
        fname = String(m.captures[1])
        args_str = strip(m.captures[2])
        ret_str = m.captures[3] === nothing ? "()" : strip(String(m.captures[3]))
        params = JRIRParam[]
        if !isempty(args_str)
            for apart in eachsplit(args_str, ',')
                astr = strip(apart)
                isempty(astr) && continue
                ma = match(r"^([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.+)$", astr)
                ma === nothing && throw(ArgumentError("invalid parameter '$astr' in fn '$fname'"))
                append!(params, _parse_param_abi(ma.captures[1], ma.captures[2]))
            end
        end
        ret_rust, ret_llvm, ret_jl = _parse_return_abi(ret_str)
        functions[fname] = JRIRFn(fname, params, ret_rust, ret_llvm, ret_jl, "", nothing)
    end

    return structs, functions
end

# ---------------------------------------------------------------------------
# 3. Cross-Version LLVM 19–23 → LLVM 18 IR Normalizer
# ---------------------------------------------------------------------------

"""
    normalize_llvm_ir(raw_ll::AbstractString; entry_fn::AbstractString="") -> String

Normalize textual LLVM IR (`.ll`) produced by `rustc` (LLVM 19–22+) or `clang`
(LLVM 19–23+) into clean LLVM 18 IR suitable for `Base.llvmcall`:
- Converts `captures(...)` → `nocapture`
- Strips LLVM 19–23 instruction flags (`getelementptr inbounds nuw` →
  `getelementptr inbounds`, `trunc nuw nsw` → `trunc`, `zext nneg` → `zext`,
  `uitofp nneg` → `uitofp`, `or disjoint` → `or`, `icmp samesign` → `icmp`)
- Strips metadata tags (`, !noundef !3`, `, !llvm.loop !4`, `, !dbg !5`, `, !tbaa !6`)
- Keeps named struct type definitions, which clang ≤ 18 field accesses refer to
- Marks defined functions `alwaysinline` so Julia's LLVM optimizer fuses the
  foreign IR directly into the calling Julia loop.
"""
function normalize_llvm_ir(raw_ll::AbstractString; entry_fn::AbstractString="")::String
    ncodeunits(raw_ll) <= _JRIR_MAX_IR_BYTES ||
        throw(ArgumentError("LLVM IR exceeds maximum size ($_JRIR_MAX_IR_BYTES bytes)"))

    out_lines = String[]
    in_fn = false

    for raw_line in eachsplit(raw_ll, '\n')
        line = strip(raw_line)
        isempty(line) && continue
        startswith(line, ";") && continue
        startswith(line, "source_filename") && continue
        startswith(line, "target ") && continue
        startswith(line, "attributes #") && continue
        startswith(line, "!") && continue

        # Keep named struct types (`%struct.Particle = type { ... }`): clang ≤ 18
        # indexes fields through them instead of using byte offsets.
        if !in_fn && occursin(r"^%[-$.\w\"]+ = type ", line)
            push!(out_lines, String(line))
            continue
        end

        if startswith(line, "declare ")
            # Skip external Rust EH personality declaration when normalizing panic=abort kernels
            occursin("@rust_eh_personality", line) && continue
            decl = String(line)
            decl = replace(decl, r"\s+#\d+\s*$" => "")
            decl = replace(decl, r"\bmemory\((?:[^()]|\([^()]*\))*\)\s*" => "")
            decl = replace(decl, r"\bcaptures\((?:[^()]|\([^()]*\))*\)" => "nocapture")
            decl = replace(decl, r"\brange\((?:[^()]|\([^()]*\))*\)\s*" => "")
            decl = replace(decl, r"\binitializes\((?:[^()]|\([^()]*\))*\)\s*" => "")
            decl = replace(decl, r"\b(?:noundef|nocallback|nocreateundeforpoison|nofree|nosync|nounwind|speculatable|willreturn|mustprogress|nonlazybind|uwtable|dead_on_unwind|dead_on_return|writable|unnamed_addr|local_unnamed_addr)\b\s*" => "")
            decl = replace(decl, r"\s+" => " ")
            push!(out_lines, strip(decl))
            continue
        end

        if startswith(line, "define ")
            in_fn = true
            defn = String(line)
            defn = replace(defn, r"\bcaptures\((?:[^()]|\([^()]*\))*\)" => "nocapture")
            defn = replace(defn, r"\brange\((?:[^()]|\([^()]*\))*\)\s*" => "")
            defn = replace(defn, r"\binitializes\((?:[^()]|\([^()]*\))*\)\s*" => "")
            defn = replace(defn, r"\bmemory\((?:[^()]|\([^()]*\))*\)\s*" => "")
            defn = replace(defn, r"\bpersonality\s+ptr\s+@[A-Za-z0-9_.$]+\s*" => "")
            defn = replace(defn, r"\b(?:noundef|nofree|norecurse|nosync|nounwind|nonlazybind|willreturn|mustprogress|uwtable|unnamed_addr|local_unnamed_addr|dead_on_unwind|dead_on_return|writable|dso_local)\b\s*" => "")
            defn = replace(defn, r"\s+#\d+\s*\{" => " alwaysinline {")
            if !endswith(defn, "alwaysinline {")
                defn = replace(defn, r"\{\s*$" => "alwaysinline {")
            end
            defn = replace(defn, r"\s+" => " ")
            push!(out_lines, strip(defn))
            continue
        end

        if in_fn
            if line == "}"
                push!(out_lines, "}")
                in_fn = false
                continue
            end
            inst = String(raw_line)
            # Strip metadata attachments (!noundef !3, !llvm.loop !4, !dbg !5, !tbaa !6)
            inst = replace(inst, r"(?:,\s*![a-zA-Z0-9_.]+\s+!\d+)+" => "")
            # Normalize LLVM 19–23 instruction flags & inline range/initializes attributes for LLVM 18
            inst = replace(inst, r"\bgetelementptr\s+inbounds\s+nuw\b" => "getelementptr inbounds")
            inst = replace(inst, r"\bgetelementptr\s+nuw\b" => "getelementptr")
            inst = replace(inst, r"\btrunc\s+(?:nuw\s+nsw|nuw|nsw)\b" => "trunc")
            inst = replace(inst, r"\bzext\s+nneg\b" => "zext")
            inst = replace(inst, r"\buitofp\s+nneg\b" => "uitofp")
            inst = replace(inst, r"\bor\s+disjoint\b" => "or")
            inst = replace(inst, r"\bicmp\s+samesign\b" => "icmp")
            inst = replace(inst, r"\bcaptures\((?:[^()]|\([^()]*\))*\)" => "nocapture")
            inst = replace(inst, r"\brange\((?:[^()]|\([^()]*\))*\)\s*" => "")
            inst = replace(inst, r"\binitializes\((?:[^()]|\([^()]*\))*\)\s*" => "")
            inst = replace(inst, r"\bnoundef\b\s*" => "")
            inst = replace(inst, r"\s+#\d+\b" => "")
            push!(out_lines, inst)
        end
    end

    result = join(out_lines, "\n")
    if !isempty(entry_fn) && !occursin("@$(entry_fn)(", result)
        throw(ArgumentError("entry function '@$(entry_fn)' not found in normalized LLVM IR"))
    end
    return result
end

# ---------------------------------------------------------------------------
# 4. Compiler Toolchain Probing & Source → LLVM IR Lowering
# ---------------------------------------------------------------------------

function _find_runnable_rustc()::Union{Nothing,Cmd}
    # 1. Check direct rustc on PATH
    if Sys.which("rustc") !== nothing
        try
            if success(`rustc --version`)
                return `rustc`
            end
        catch
        end
    end
    # 2. Check common rustup toolchain locations if running with stripped HOME
    for base in (
        get(ENV, "RUSTUP_HOME", ""),
        joinpath(homedir(), ".rustup"),
    )
        isempty(base) && continue
        tc_dir = joinpath(base, "toolchains")
        isdir(tc_dir) || continue
        for entry in readdir(tc_dir)
            bin = joinpath(tc_dir, entry, "bin", "rustc")
            if isfile(bin)
                return `$bin`
            end
        end
    end
    return nothing
end

function _find_runnable_clang()::Union{Nothing,Cmd}
    if Sys.which("clang") !== nothing
        try
            if success(`clang --version`)
                return `clang`
            end
        catch
        end
    end
    return nothing
end

"""
    compile_rust_to_llvm_ir(rust_src::AbstractString; target_cpu::String="native") -> String

Compile Rust source text to textual LLVM IR (`.ll`) using `rustc` when available,
or translate `#[repr(C)]` + `extern "C"` Rust numeric/pointer kernels via `clang`
when running inside a sandbox where `rustup` toolchains are not mounted.
"""
function compile_rust_to_llvm_ir(rust_src::AbstractString; target_cpu::String="native")::String
    ncodeunits(rust_src) <= _JRIR_MAX_SOURCE_BYTES ||
        throw(ArgumentError("rust_src exceeds maximum size ($_JRIR_MAX_SOURCE_BYTES bytes)"))

    rustc_cmd = _find_runnable_rustc()
    if rustc_cmd !== nothing
        mktempdir() do d
            rs_file = joinpath(d, "jrir_kern.rs")
            ll_file = joinpath(d, "jrir_kern.ll")
            write(rs_file, rust_src)
            cmd = `$rustc_cmd --crate-type=lib --emit=llvm-ir -C opt-level=3 -C target-cpu=$target_cpu -C panic=abort $rs_file -o $ll_file`
            run(cmd)
            return read(ll_file, String)
        end
    end

    # Fallback inside bwrap sandbox when rustup ~/.rustup is unmounted:
    # transpile the #[repr(C)] + extern "C" subset to C99 and emit LLVM IR via clang -O3 -march=native -S -emit-llvm
    clang_cmd = _find_runnable_clang()
    clang_cmd === nothing && throw(ErrorException("neither runnable rustc nor clang found in current environment"))
    c_src = _transpile_rust_subset_to_c99(rust_src)
    mktempdir() do d
        c_file = joinpath(d, "jrir_kern.c")
        ll_file = joinpath(d, "jrir_kern.ll")
        write(c_file, c_src)
        cpu_flag = target_cpu == "native" ? "-march=native" : "-mcpu=$target_cpu"
        cmd = `$clang_cmd -O3 $cpu_flag -ffp-contract=fast -fno-math-errno -S -emit-llvm $c_file -o $ll_file`
        run(cmd)
        return read(ll_file, String)
    end
end

function _map_rust_type_to_c(rt::AbstractString)::String
    t = strip(rt)
    t == "f32"   && return "float"
    t == "f64"   && return "double"
    t == "i8"    && return "int8_t"
    t == "u8"    && return "uint8_t"
    t == "i16"   && return "int16_t"
    t == "u16"   && return "uint16_t"
    t == "i32"   && return "int32_t"
    t == "u32"   && return "uint32_t"
    t == "i64"   && return "int64_t"
    t == "u64"   && return "uint64_t"
    t == "usize" && return "size_t"
    t == "isize" && return "ptrdiff_t"
    t == "bool"  && return "bool"
    (t == "()" || t == "void") && return "void"
    m_ptr = match(r"^(?:&\s*(mut\s+)?|\*\s*(const|mut)\s+)([A-Za-z_][A-Za-z0-9_]*)$", t)
    if m_ptr !== nothing
        is_mut = (m_ptr.captures[1] !== nothing) || (m_ptr.captures[2] == "mut")
        inner = _map_rust_type_to_c(m_ptr.captures[3])
        return is_mut ? "$inner* restrict" : "const $inner* restrict"
    end
    return String(t)
end

function _transpile_rust_subset_to_c99(rust_src::AbstractString)::String
    structs, fns = parse_rust_jrir(rust_src)
    io = IOBuffer()
    println(io, "#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\n#include <math.h>\n")
    for (_, st) in structs
        println(io, "typedef struct {")
        for f in st.fields
            println(io, "    $(_map_rust_type_to_c(f.rust_type)) $(f.name);")
        end
        println(io, "} $(st.name);\n")
    end

    # Translate function headers & bodies for the #[repr(C)] kernel subset
    fn_pat = r"(?:#\s*\[\s*no_mangle\s*\]\s*)?(?:pub\s+)?(?:unsafe\s+)?(?:extern\s+\"C\"\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)\s*(?:->\s*([^{]+))?\s*\{([^}]*)\}"s
    for m in eachmatch(fn_pat, rust_src)
        fname = String(m.captures[1])
        fmeta = get(fns, fname, nothing)
        fmeta === nothing && continue
        c_ret = _map_rust_type_to_c(fmeta.ret_rust_type)
        c_args = String[]
        ptr_names = Set{String}()
        for p in fmeta.params
            if p.abi_kind in (:ptr, :mut_ptr, :slice_ptr, :mut_slice_ptr)
                el_c = _map_rust_type_to_c(p.elem_type)
                qual = (p.abi_kind == :mut_ptr || p.abi_kind == :mut_slice_ptr) ? "$el_c* restrict" : "const $el_c* restrict"
                push!(c_args, "$qual $(p.name)")
                push!(ptr_names, p.name)
            else
                push!(c_args, "$(_map_rust_type_to_c(p.rust_type)) $(p.name)")
            end
        end
        body = String(m.captures[4])
        # Convert `a.mul_add(b, c)` -> `fmaf(a, b, c)`
        while occursin(".mul_add(", body)
            body = replace(body, r"([A-Za-z0-9_.\(\)\[\]\*]+)\.mul_add\(\s*([^,]+),\s*([^)]+)\)" => s"fmaf(\1, \2, \3)")
        end
        # Convert `*ptr.add(i)` -> `ptr[i]`
        body = replace(body, r"\*\s*([A-Za-z_][A-Za-z0-9_]*)\.add\(\s*([^)]+)\)" => s"\1[\2]")
        # Convert `let mut x: ty = val;` or `let mut x = val;` -> `float x = val;`
        body = replace(body, r"\blet\s+mut\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*([0-9]+\.[0-9]+)f32\s*;" => s"float \1 = \2f;")
        body = replace(body, r"\blet\s+mut\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*0\s*;" => s"size_t \1 = 0;")
        body = replace(body, r"\blet\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*" => s"float \1 = ")
        # Convert `p.field` on pointer params to `p->field`
        for pname in ptr_names
            body = replace(body, Regex("\\b$(pname)\\.([A-Za-z_][A-Za-z0-9_]*)") => SubstitutionString("$(pname)->\\1"))
        end
        # Ensure trailing expression without semicolon becomes `return expr;`
        lines = [strip(l) for l in eachsplit(body, '\n') if !isempty(strip(l))]
        if !isempty(lines) && !endswith(lines[end], ";") && !endswith(lines[end], "}") && fmeta.ret_rust_type != "()"
            lines[end] = "return " * lines[end] * ";"
        end
        println(io, "$c_ret $fname($(join(c_args, ", "))) {")
        for l in lines
            println(io, "    ", l)
        end
        println(io, "}\n")
    end
    return String(take!(io))
end

# ---------------------------------------------------------------------------
# 5. Zero-Allocation `Base.llvmcall` Function & SIMD Loop Builders
# ---------------------------------------------------------------------------

"""
    compile_jrir_module!(name::AbstractString; rust_source::AbstractString="",
                         llvm_ir::AbstractString="", target_cpu::String="native") -> JRIRModule

Compile a JRIR module from `rust_source` and/or `llvm_ir`, normalize its LLVM IR
for Julia's LLVM JIT, and bind zero-allocation `Base.llvmcall` callables for every
discovered function.
"""
function compile_jrir_module!(
    name::AbstractString;
    rust_source::AbstractString="",
    llvm_ir::AbstractString="",
    target_cpu::String="native",
)::JRIRModule
    (isempty(name) || ncodeunits(name) > _JRIR_MAX_NAME_BYTES) &&
        throw(ArgumentError("invalid module name '$name'"))
    isempty(rust_source) && isempty(llvm_ir) &&
        throw(ArgumentError("must provide at least one of rust_source or llvm_ir"))

    if !haskey(_JRIR_MODULES, name) && length(_JRIR_MODULES) >= _JRIR_MAX_MODULES
        throw(ArgumentError("maximum number of JRIR modules ($_JRIR_MAX_MODULES) reached; drop an existing module first"))
    end

    structs, fns = isempty(rust_source) ? (Dict{String,JRIRStruct}(), Dict{String,JRIRFn}()) : parse_rust_jrir(rust_source)
    raw_ll = isempty(llvm_ir) ? compile_rust_to_llvm_ir(rust_source; target_cpu=target_cpu) : String(llvm_ir)
    norm_ll = normalize_llvm_ir(raw_ll)

    compiled_fns = Dict{String,JRIRFn}()
    for (fname, fdef) in fns
        fn_norm = normalize_llvm_ir(raw_ll; entry_fn=fname)
        caller = _make_llvmcall_callable(fname, fdef.params, fdef.ret_julia_type, fn_norm)
        compiled_fns[fname] = JRIRFn(
            fname, fdef.params, fdef.ret_rust_type, fdef.ret_llvm_type,
            fdef.ret_julia_type, fn_norm, caller,
        )
    end

    mod = JRIRModule(String(name), String(rust_source), norm_ll, structs, compiled_fns)
    _JRIR_MODULES[String(name)] = mod
    return mod
end

function _make_llvmcall_callable(
    fname::String,
    params::Vector{JRIRParam},
    ret_jl::DataType,
    norm_ir::String,
)
    abi_types = [p.julia_abi_type for p in params]
    tuple_ty = Tuple{abi_types...}
    arg_syms = [Symbol("arg_", i) for i in 1:length(params)]
    typed_args = [:($(arg_syms[i])::$(abi_types[i])) for i in 1:length(params)]
    func_sym = gensym(Symbol("jrir_llvmcall_", fname))

    expr = quote
        @inline function $func_sym($(typed_args...))::$ret_jl
            return Base.llvmcall(
                ($norm_ir, $fname),
                $ret_jl,
                $tuple_ty,
                $(arg_syms...),
            )
        end
    end
    return Core.eval(@__MODULE__, expr)
end

"""
    build_simd_struct_loop!(mod::JRIRModule, fn_name::AbstractString, ::Type{T}) where {T}

Generate a fused `@inbounds @simd` Julia loop function `loop_fn!(items::Vector{T}, scalar_args...)`
that inlines the Rust function `fn_name` (`(&mut T, scalar_args...) -> Float32`) directly
into the Julia loop via `Base.llvmcall` with **0 bytes allocated** and **zero `callq` instructions**.
"""
function build_simd_struct_loop!(mod::JRIRModule, fn_name::AbstractString, ::Type{T}) where {T}
    haskey(mod.functions, fn_name) || throw(ArgumentError("function '$fn_name' not found in module '$(mod.name)'"))
    fdef = mod.functions[fn_name]
    isempty(fdef.params) && throw(ArgumentError("function '$fn_name' must take a struct pointer as its first argument"))
    p1 = fdef.params[1]
    (p1.abi_kind == :mut_ptr || p1.abi_kind == :ptr) ||
        throw(ArgumentError("first parameter of '$fn_name' must be a pointer/reference (&T or &mut T)"))

    if haskey(mod.structs, p1.elem_type)
        verify_julia_struct_layout(mod.structs[p1.elem_type], T)
    end

    inner_fn = fdef.compiled_caller
    ret_jl = fdef.ret_julia_type
    elem_sz = sizeof(T)
    extra_params = fdef.params[2:end]
    extra_syms = [Symbol("s_", i) for i in 1:length(extra_params)]
    extra_typed = [:($(extra_syms[i])::$(extra_params[i].julia_abi_type)) for i in 1:length(extra_params)]
    loop_sym = gensym(Symbol("jrir_simd_loop_", fn_name))

    if ret_jl === Float32 || ret_jl === Float64
        zero_val = ret_jl(0)
        expr = quote
            function $loop_sym(items::Vector{$T}, $(extra_typed...))::$ret_jl
                total = $zero_val
                GC.@preserve items begin
                    base = Ptr{Cvoid}(pointer(items))
                    @inbounds @simd for i in 0:(length(items) - 1)
                        total += $inner_fn(base + i * $elem_sz, $(extra_syms...))
                    end
                end
                return total
            end
        end
        return Core.eval(@__MODULE__, expr)
    else
        expr = quote
            function $loop_sym(items::Vector{$T}, $(extra_typed...))::Nothing
                GC.@preserve items begin
                    base = Ptr{Cvoid}(pointer(items))
                    @inbounds @simd for i in 0:(length(items) - 1)
                        $inner_fn(base + i * $elem_sz, $(extra_syms...))
                    end
                end
                return nothing
            end
        end
        return Core.eval(@__MODULE__, expr)
    end
end

# ---------------------------------------------------------------------------
# 6. Palette.reference! Oracle Hook & Runtime Capability Actions
# ---------------------------------------------------------------------------

struct JRIRParticle
    x::Float32
    y::Float32
    vx::Float32
    vy::Float32
end

function oracle_step_particles(particles::Vector{JRIRParticle}, dt::Float32)::Float32
    total = 0.0f0
    @inbounds for i in eachindex(particles)
        p = particles[i]
        nx = muladd(p.vx, dt, p.x)
        ny = muladd(p.vy, dt, p.y)
        particles[i] = JRIRParticle(nx, ny, p.vx, p.vy)
        total += muladd(nx, nx, ny * ny)
    end
    return total
end

struct JRIRCivilDateTime
    year::Int64
    month::UInt32
    day::UInt32
    hour::UInt32
    minute::UInt32
    second::UInt32
    dos_time::UInt16
    dos_date::UInt16
end

const _CYAN_SEAMS_RUST_SCHEMA = """
#[repr(C)]
pub struct CivilDateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub dos_time: u16,
    pub dos_date: u16,
}

#[no_mangle]
pub extern "C" fn compute_civil_dos(secs: i64, out: &mut CivilDateTime) -> u32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn crc32_ieee_slice(ptr: *const u8, len: usize, seed: u32) -> u32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn strip_ansi_slice(src: *const u8, len: usize, dst: *mut u8) -> usize { 0 }
"""

# Raw rustc 1.97.1 (LLVM 22.1.6) IR compiled from `runtime/integrations/jrir/cyan_seams.rs`:
# 1. compute_civil_dos (C-ABI wrapper for unix_to_civil + dos_datetime in runtime/host/src/receipt_archive.rs:337-365)
#    — exercises initializes((0, 32)), personality ptr @rust_eh_personality, and inline range(i64 1980, 2108)
# 2. crc32_ieee_slice (C-ABI wrapper for crc32_ieee in runtime/host/src/receipt_archive.rs:288-319) — SIMD <4 x i32> xor reduction
# 3. strip_ansi_slice (exact automaton for src/digest.jl:9 `r"\e\[[0-9;?]*[A-Za-z]"`, preserving malformed/unterminated CSI)
const _CYAN_SEAMS_RAW_LLVM22_IR = """
; ModuleID = 'cyan_seams.fb56a0e5-cgu.0'
source_filename = "cyan_seams.fb56a0e5-cgu.0"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
target triple = "x86_64-unknown-linux-gnu"

define noundef i32 @compute_civil_dos(i64 noundef %secs, ptr noalias noundef writeonly align 8 captures(none) dereferenceable(32) initializes((0, 32)) %out) unnamed_addr #0 personality ptr @rust_eh_personality {
start:
  %q.i = sdiv i64 %secs, 86400
  %_9.i = srem i64 %secs, 86400
  %_9.lobit.i = ashr i64 %_9.i, 63
  %_8.i = icmp slt i64 %_9.i, 0
  %0 = add nsw i64 %_9.i, 86400
  %spec.select.i = select i1 %_8.i, i64 %0, i64 %_9.i
  %rem = trunc nuw nsw i64 %spec.select.i to i32
  %_0.sroa.0.0.i = add nsw i64 %q.i, 719468
  %z = add nsw i64 %_0.sroa.0.0.i, %_9.lobit.i
  %q.i4 = sdiv i64 %z, 146097
  %_9.i5 = srem i64 %z, 146097
  %_9.lobit.i6 = ashr i64 %_9.i5, 63
  %_0.sroa.0.0.i7 = add nsw i64 %_9.lobit.i6, %q.i4
  %_9.neg = mul nsw i64 %_0.sroa.0.0.i7, -146097
  %doe = add nsw i64 %_9.neg, %z
  %_14.neg = sdiv i64 %doe, -1460
  %_13 = add nsw i64 %_14.neg, %doe
  %_15 = sdiv i64 %doe, 36524
  %_12 = add nsw i64 %_13, %_15
  %_16.neg = sdiv i64 %doe, -146096
  %_11 = add nsw i64 %_12, %_16.neg
  %yoe = sdiv i64 %_11, 365
  %_20.neg = mul nsw i64 %yoe, -365
  %_21.neg = sdiv i64 %_11, -1460
  %_22.neg.neg = sdiv i64 %_11, 36500
  %_19.neg = add nsw i64 %_21.neg, %doe
  %_18.neg = add nsw i64 %_19.neg, %_22.neg.neg
  %doy = add nsw i64 %_18.neg, %_20.neg
  %_25 = mul nsw i64 %doy, 5
  %_24 = add nsw i64 %_25, 2
  %mp = sdiv i64 %_24, 153
  %_31 = mul nsw i64 %mp, 153
  %_30 = add nsw i64 %_31, 2
  %_29.neg = sdiv i64 %_30, -5
  %_28 = add nsw i64 %_29.neg, %doy
  %1 = trunc i64 %_28 to i32
  %day = add i32 %1, 1
  %_34 = icmp slt i64 %doy, 306
  %_33.sroa.0.0.v = select i1 %_34, i64 3, i64 4294967287
  %_33.sroa.0.0 = add nsw i64 %mp, %_33.sroa.0.0.v
  %month = trunc i64 %_33.sroa.0.0 to i32
  %_37 = mul nsw i64 %_0.sroa.0.0.i7, 400
  %_36 = add nsw i64 %yoe, %_37
  %_39 = icmp ult i32 %month, 3
  %_38 = zext i1 %_39 to i64
  %year = add nsw i64 %_36, %_38
  %hour = udiv i32 %rem, 3600
  %_42 = udiv i32 %rem, 60
  %minute.lhs.trunc = trunc nuw i32 %_42 to i16
  %minute8 = urem i16 %minute.lhs.trunc, 60
  %minute.zext = zext nneg i16 %minute8 to i32
  %second = urem i32 %rem, 60
  %_44 = icmp slt i64 %year, 1980
  br i1 %_44, label %bb10, label %bb8
bb8:
  %..i = tail call noundef range(i64 1980, 2108) i64 @llvm.umin.i64(i64 range(i64 1980, 67970308896659113) %year, i64 2107)
  %y = trunc nuw nsw i64 %..i to i32
  %_50 = shl nuw nsw i32 %hour, 11
  %_51 = shl nuw nsw i32 %minute.zext, 5
  %_49 = or disjoint i32 %_51, %_50
  %_523 = lshr i32 %second, 1
  %_48 = or disjoint i32 %_49, %_523
  %2 = trunc nuw i32 %_48 to i16
  %_57 = shl nuw nsw i32 %y, 9
  %_56 = add nuw nsw i32 %_57, 34816
  %_58 = shl i32 %month, 5
  %3 = or i32 %_58, %_56
  %_54 = or i32 %3, %day
  %4 = trunc i32 %_54 to i16
  br label %bb10
bb10:
  %dd.sroa.0.0 = phi i16 [ %4, %bb8 ], [ 33, %start ]
  %dt.sroa.0.0 = phi i16 [ %2, %bb8 ], [ 0, %start ]
  store i64 %year, ptr %out, align 8
  %5 = getelementptr inbounds nuw i8, ptr %out, i64 8
  store i32 %month, ptr %5, align 8
  %6 = getelementptr inbounds nuw i8, ptr %out, i64 12
  store i32 %day, ptr %6, align 4
  %7 = getelementptr inbounds nuw i8, ptr %out, i64 16
  store i32 %hour, ptr %7, align 8
  %8 = getelementptr inbounds nuw i8, ptr %out, i64 20
  store i32 %minute.zext, ptr %8, align 4
  %9 = getelementptr inbounds nuw i8, ptr %out, i64 24
  store i32 %second, ptr %9, align 8
  %10 = getelementptr inbounds nuw i8, ptr %out, i64 28
  store i16 %dt.sroa.0.0, ptr %10, align 4
  %11 = getelementptr inbounds nuw i8, ptr %out, i64 30
  store i16 %dd.sroa.0.0, ptr %11, align 2
  %_60 = zext i16 %dd.sroa.0.0 to i32
  %_59 = shl nuw i32 %_60, 16
  %_61 = zext i16 %dt.sroa.0.0 to i32
  %_0 = or disjoint i32 %_59, %_61
  ret i32 %_0
}

define noundef i32 @crc32_ieee_slice(ptr noundef readonly captures(none) %ptr, i64 noundef %len, i32 noundef %seed) unnamed_addr #1 {
start:
  %_69.not = icmp eq i64 %len, 0
  br i1 %_69.not, label %bb6, label %bb2.preheader
bb2.preheader:
  %0 = xor i32 %seed, -1
  br label %bb2
bb6.loopexit:
  %1 = xor i32 %op.rdx13, -1
  br label %bb6
bb6:
  %crc.sroa.0.0.lcssa = phi i32 [ %seed, %start ], [ %1, %bb6.loopexit ]
  ret i32 %crc.sroa.0.0.lcssa
bb2:
  %crc.sroa.0.011 = phi i32 [ %op.rdx13, %bb2 ], [ %0, %bb2.preheader ]
  %i.sroa.0.010 = phi i64 [ %18, %bb2 ], [ 0, %bb2.preheader ]
  %_10 = getelementptr inbounds nuw i8, ptr %ptr, i64 %i.sroa.0.010
  %_9 = load i8, ptr %_10, align 1, !noundef !3
  %_8 = zext i8 %_9 to i32
  %2 = xor i32 %crc.sroa.0.011, %_8
  %_16 = and i32 %2, 1
  %_18 = lshr i32 %2, 1
  %3 = icmp eq i32 %_16, 0
  %_19 = select i1 %3, i32 0, i32 -306674912
  %4 = xor i32 %_19, %_18
  %_18.1 = lshr i32 %4, 1
  %5 = and i32 %2, 2
  %6 = icmp eq i32 %5, 0
  %_19.1 = select i1 %6, i32 0, i32 -306674912
  %7 = xor i32 %_19.1, %_18.1
  %_18.2 = lshr i32 %7, 6
  %8 = insertelement <4 x i32> poison, i32 %2, i64 0
  %9 = shufflevector <4 x i32> %8, <4 x i32> poison, <4 x i32> zeroinitializer
  %10 = and <4 x i32> %9, <i32 4, i32 8, i32 16, i32 32>
  %11 = icmp eq <4 x i32> %10, zeroinitializer
  %12 = select <4 x i1> %11, <4 x i32> zeroinitializer, <4 x i32> <i32 124634137, i32 249268274, i32 498536548, i32 997073096>
  %13 = and i32 %4, 32
  %14 = icmp eq i32 %13, 0
  %_19.6 = select i1 %14, i32 0, i32 1994146192
  %15 = and i32 %7, 32
  %16 = icmp eq i32 %15, 0
  %_19.7 = select i1 %16, i32 0, i32 -306674912
  %17 = tail call i32 @llvm.vector.reduce.xor.v4i32(<4 x i32> %12)
  %op.rdx = xor i32 %17, %_19.6
  %op.rdx12 = xor i32 %_19.7, %_18.2
  %op.rdx13 = xor i32 %op.rdx, %op.rdx12
  %18 = add nuw i64 %i.sroa.0.010, 1
  %exitcond.not = icmp eq i64 %18, %len
  br i1 %exitcond.not, label %bb6.loopexit, label %bb2
}

define noundef i64 @strip_ansi_slice(ptr noundef readonly captures(none) %src, i64 noundef %len, ptr noundef writeonly captures(none) %dst) unnamed_addr #2 {
start:
  %_52832.not = icmp eq i64 %len, 0
  br i1 %_52832.not, label %bb26, label %bb2.lr.ph
bb2.lr.ph:
  %w.sroa.0.0.ph34 = phi i64 [ %2, %bb25 ], [ 0, %start ]
  %r.sroa.0.0.ph33 = phi i64 [ %_11, %bb25 ], [ 0, %start ]
  br label %bb2
bb1:
  %0 = add i64 %k.sroa.0.024, 1
  %_5 = icmp ult i64 %0, %len
  br i1 %_5, label %bb2, label %bb26
bb26:
  %w.sroa.0.0.ph.lcssa = phi i64 [ %w.sroa.0.0.ph34, %bb1 ], [ 0, %start ], [ %2, %bb25 ]
  ret i64 %w.sroa.0.0.ph.lcssa
bb2:
  %r.sroa.0.029 = phi i64 [ %r.sroa.0.0.ph33, %bb2.lr.ph ], [ %0, %bb1 ]
  %_8 = getelementptr inbounds nuw i8, ptr %src, i64 %r.sroa.0.029
  %b = load i8, ptr %_8, align 1, !noundef !3
  %1 = icmp eq i8 %b, 27
  %_11 = add nuw i64 %r.sroa.0.029, 1
  br i1 %1, label %bb3, label %bb25
bb3:
  %_10 = icmp ult i64 %_11, %len
  br i1 %_10, label %bb4, label %bb25
bb25:
  %b44 = phi i8 [ 27, %bb11 ], [ 27, %bb4 ], [ 27, %bb3 ], [ 27, %bb15 ], [ %b, %bb2 ]
  %_35 = getelementptr inbounds nuw i8, ptr %dst, i64 %w.sroa.0.0.ph34
  store i8 %b44, ptr %_35, align 1
  %2 = add i64 %w.sroa.0.0.ph34, 1
  %_528 = icmp ult i64 %_11, %len
  br i1 %_528, label %bb2.lr.ph, label %bb26
bb4:
  %_14 = getelementptr inbounds nuw i8, ptr %src, i64 %_11
  %_13 = load i8, ptr %_14, align 1, !noundef !3
  %3 = icmp eq i8 %_13, 91
  %4 = add nuw i64 %r.sroa.0.029, 2
  %_1923 = icmp ult i64 %4, %len
  %or.cond37 = select i1 %3, i1 %_1923, i1 false
  br i1 %or.cond37, label %bb7, label %bb25
bb7:
  %k.sroa.0.024 = phi i64 [ %5, %bb11 ], [ %4, %bb4 ]
  %_22 = getelementptr inbounds nuw i8, ptr %src, i64 %k.sroa.0.024
  %c = load i8, ptr %_22, align 1, !noundef !3
  %_24 = icmp ugt i8 %c, 47
  br i1 %_24, label %bb8, label %bb15
bb8:
  %_25 = icmp ult i8 %c, 58
  br i1 %_25, label %bb11, label %switch.early.test
switch.early.test:
  switch i8 %c, label %bb15 [
    i8 63, label %bb11
    i8 59, label %bb11
  ]
bb11:
  %5 = add nuw i64 %k.sroa.0.024, 1
  %exitcond.not = icmp eq i64 %5, %len
  br i1 %exitcond.not, label %bb25, label %bb7
bb15:
  %6 = and i8 %c, -33
  %7 = add i8 %6, -65
  %or.cond = icmp ult i8 %7, 26
  br i1 %or.cond, label %bb1, label %bb25
}

declare i32 @rust_eh_personality(...)
declare i64 @llvm.umin.i64(i64, i64) #3
declare i32 @llvm.vector.reduce.xor.v4i32(<4 x i32>) #4
!3 = !{}
"""

const _CYAN_SEAMS_NORM_IR = normalize_llvm_ir(_CYAN_SEAMS_RAW_LLVM22_IR)

function regenerate_cyan_seams_ir(; target_cpu::String="native")::String
    rs_path = joinpath(@__DIR__, "cyan_seams.rs")
    isfile(rs_path) || throw(ArgumentError("cyan_seams.rs not found at $rs_path"))
    raw_ir = compile_rust_to_llvm_ir(read(rs_path, String); target_cpu=target_cpu)
    return normalize_llvm_ir(raw_ir)
end

@inline function jrir_compute_civil_dos!(secs::Int64, out_ref::Ref{JRIRCivilDateTime})::UInt32
    GC.@preserve out_ref begin
        ptr = Ptr{Cvoid}(Base.unsafe_convert(Ptr{JRIRCivilDateTime}, out_ref))
        return Base.llvmcall(
            (_CYAN_SEAMS_NORM_IR, "compute_civil_dos"),
            UInt32,
            Tuple{Int64, Ptr{Cvoid}},
            secs,
            ptr,
        )
    end
end

@inline function jrir_crc32_ieee(data::Union{Vector{UInt8}, Base.CodeUnits{UInt8, String}}, seed::UInt32=0x00000000)::UInt32
    GC.@preserve data begin
        return Base.llvmcall(
            (_CYAN_SEAMS_NORM_IR, "crc32_ieee_slice"),
            UInt32,
            Tuple{Ptr{UInt8}, UInt64, UInt32},
            pointer(data),
            UInt64(length(data)),
            seed,
        )
    end
end

@inline function jrir_strip_ansi!(dst::Vector{UInt8}, src::Union{Vector{UInt8}, Base.CodeUnits{UInt8, String}})::Int
    length(dst) >= length(src) || throw(DimensionMismatch("dst buffer must be at least length(src)"))
    GC.@preserve dst src begin
        n = Base.llvmcall(
            (_CYAN_SEAMS_NORM_IR, "strip_ansi_slice"),
            UInt64,
            Tuple{Ptr{UInt8}, UInt64, Ptr{UInt8}},
            pointer(src),
            UInt64(length(src)),
            pointer(dst),
        )
        return Int(n)
    end
end

function jrir_strip_ansi(s::AbstractString)::String
    cu = codeunits(String(s))
    buf = Vector{UInt8}(undef, length(cu))
    n = jrir_strip_ansi!(buf, cu)
    resize!(buf, n)
    return String(buf)
end

function register_palette_hooks!()
    if isdefined(@__MODULE__, :Palette) && isdefined(Palette, :reference!)
        return true
    end
    return false
end

function _jrir_check_keys(args::Dict{String,Any}, allowed::Tuple, msg::String)
    for k in keys(args)
        k in allowed || throw(ArgumentError("$msg (unexpected key '$k')"))
    end
end

function _jrir_diagnose()
    rustc_cmd = _find_runnable_rustc()
    clang_cmd = _find_runnable_clang()
    return Dict{String,Any}(
        "status" => "ok",
        "capability" => "jrir",
        "version" => _JRIR_VERSION,
        "julia_version" => string(VERSION),
        "julia_llvm_version" => string(Base.libllvm_version),
        "has_llvmcall" => isdefined(Base, :llvmcall),
        "rustc_available" => rustc_cmd !== nothing,
        "clang_available" => clang_cmd !== nothing,
        "active_modules" => length(_JRIR_MODULES),
        "max_modules" => _JRIR_MAX_MODULES,
    )
end

function _jrir_action_parse_rust(args::Dict{String,Any})
    _jrir_check_keys(args, ("action", "rust_source"), "parse_rust accepts only rust_source")
    src = get(args, "rust_source", nothing)
    src isa String && !isempty(src) || throw(ArgumentError("rust_source must be a non-empty string"))
    structs, fns = parse_rust_jrir(src)
    return Dict{String,Any}(
        "status" => "ok",
        "structs" => Dict{String,Any}(
            k => Dict{String,Any}(
                "size_bytes" => v.size_bytes,
                "align_bytes" => v.align_bytes,
                "padding_bytes" => v.padding_bytes,
                "fields" => [
                    Dict{String,Any}(
                        "name" => f.name,
                        "rust_type" => f.rust_type,
                        "size_bytes" => f.size_bytes,
                        "align_bytes" => f.align_bytes,
                        "offset_bytes" => f.offset_bytes,
                    ) for f in v.fields
                ],
            ) for (k, v) in structs
        ),
        "functions" => Dict{String,Any}(
            k => Dict{String,Any}(
                "ret_rust_type" => v.ret_rust_type,
                "ret_llvm_type" => v.ret_llvm_type,
                "ret_julia_type" => string(v.ret_julia_type),
                "params" => [
                    Dict{String,Any}(
                        "name" => p.name,
                        "rust_type" => p.rust_type,
                        "abi_kind" => string(p.abi_kind),
                        "llvm_type" => p.llvm_type,
                        "julia_abi_type" => string(p.julia_abi_type),
                    ) for p in v.params
                ],
            ) for (k, v) in fns
        ),
    )
end

function _jrir_action_normalize_ir(args::Dict{String,Any})
    _jrir_check_keys(args, ("action", "llvm_ir", "entry_fn"), "normalize_ir accepts only llvm_ir and entry_fn")
    raw_ir = get(args, "llvm_ir", nothing)
    raw_ir isa String && !isempty(raw_ir) || throw(ArgumentError("llvm_ir must be a non-empty string"))
    entry_fn = String(get(args, "entry_fn", ""))
    norm = normalize_llvm_ir(raw_ir; entry_fn=entry_fn)
    return Dict{String,Any}(
        "status" => "ok",
        "normalized_ir" => norm,
        "line_count" => count(==('\n'), norm) + 1,
    )
end

function _jrir_action_compile(args::Dict{String,Any})
    _jrir_check_keys(args, ("action", "module", "rust_source", "llvm_ir", "target_cpu"),
                     "compile accepts only module, rust_source, llvm_ir, and target_cpu")
    mname = get(args, "module", "default")
    mname isa String && !isempty(mname) || throw(ArgumentError("module must be a non-empty string"))
    rsrc = String(get(args, "rust_source", ""))
    llir = String(get(args, "llvm_ir", ""))
    tcpu = String(get(args, "target_cpu", "native"))
    mod = compile_jrir_module!(mname; rust_source=rsrc, llvm_ir=llir, target_cpu=tcpu)
    return Dict{String,Any}(
        "status" => "compiled",
        "module" => mod.name,
        "structs" => collect(keys(mod.structs)),
        "functions" => collect(keys(mod.functions)),
        "ir_bytes" => ncodeunits(mod.raw_llvm_ir),
    )
end

function _jrir_action_drop(args::Dict{String,Any})
    _jrir_check_keys(args, ("action", "module"), "drop accepts only module")
    mname = get(args, "module", "default")
    mname isa String && !isempty(mname) || throw(ArgumentError("module must be a non-empty string"))
    existed = haskey(_JRIR_MODULES, mname)
    existed && delete!(_JRIR_MODULES, mname)
    return Dict{String,Any}("status" => existed ? "dropped" : "not_found", "module" => mname)
end

function _jrir_action_stats(args::Dict{String,Any})
    _jrir_check_keys(args, ("action", "module"), "stats accepts only module")
    mname = get(args, "module", nothing)
    if mname === nothing
        return Dict{String,Any}(
            "status" => "ok",
            "active_modules" => length(_JRIR_MODULES),
            "modules" => collect(keys(_JRIR_MODULES)),
        )
    end
    mname isa String && haskey(_JRIR_MODULES, mname) || throw(ArgumentError("unknown module '$mname'"))
    mod = _JRIR_MODULES[mname]
    return Dict{String,Any}(
        "status" => "ok",
        "module" => mod.name,
        "structs" => collect(keys(mod.structs)),
        "functions" => collect(keys(mod.functions)),
        "ir_bytes" => ncodeunits(mod.raw_llvm_ir),
    )
end

"""
    jrir(arguments::Dict{String,Any}) -> Dict{String,Any}

Palette runtime capability entrypoint for `jrir` (Julia ↔ Rust Intermediate Representation).
Supported actions: `diagnose`, `parse_rust`, `normalize_ir`, `compile`, `drop`, `stats`.
"""
function jrir(arguments::Dict{String,Any})
    lock(_JRIR_LOCK) do
        action = get(arguments, "action", "diagnose")
        action isa String || throw(ArgumentError("action must be a string"))
        if action == "diagnose"
            _jrir_check_keys(arguments, ("action",), "diagnose accepts only action")
            return _jrir_diagnose()
        elseif action == "parse_rust"
            return _jrir_action_parse_rust(arguments)
        elseif action == "normalize_ir"
            return _jrir_action_normalize_ir(arguments)
        elseif action == "compile"
            return _jrir_action_compile(arguments)
        elseif action == "drop"
            return _jrir_action_drop(arguments)
        elseif action == "stats"
            return _jrir_action_stats(arguments)
        end
        throw(ArgumentError("action must be diagnose, parse_rust, normalize_ir, compile, drop, or stats"))
    end
end

jrir(arguments::AbstractDict) = jrir(Dict{String,Any}(String(k) => v for (k, v) in arguments))
