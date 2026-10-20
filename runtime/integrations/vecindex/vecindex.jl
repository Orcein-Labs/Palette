# Pure-Julia zero-allocation Contiguous-IVF + BLAS vector and text/code index for Palette.
using LinearAlgebra
using Random
using SHA

const _VECINDEX_VERSION = "0.1.0"
const _MAX_STORES = 32
const _MIN_DIM = 8
const _MAX_DIM = 4096
const _MAX_ITEMS = 250_000
const _MAX_BATCH = 4096
const _MAX_TEXT_BYTES = 32_768
const _MAX_ID_BYTES = 512
const _MAX_K = 256
const _DEFAULT_DIM = 256
const _DEFAULT_CAPACITY = 256
const _IVF_AUTO_MIN_ITEMS = 50_000



"""
    IVFIndex

Contiguous-cluster Inverted File (IVF) index. Vectors `1:indexed_count` are
permuted by coarse centroid into `embeddings_perm` so probing a cluster streams
contiguous columns in L1/L2 cache with 4-way AVX2 FMA register blocking.
"""
mutable struct IVFIndex
    n_list::Int
    n_probe::Int
    indexed_count::Int
    valid::Bool
    centroids::Matrix{Float32}       # d × n_list (unit L2 columns)
    embeddings_perm::Matrix{Float32} # d × indexed_count (contiguous by cluster)
    perm_to_id::Vector{Int}          # perm_idx -> store item index (1..indexed_count)
    id_to_perm::Vector{Int}          # store item index -> perm_idx
    cluster_offsets::Vector{Int}     # length n_list + 1 (1-based CSR offsets)
    coarse_scores::Vector{Float32}   # length n_list scratch
    coarse_idx::Vector{Int}          # length n_list scratch
    coarse_val::Vector{Float32}      # length n_list scratch
end

function _empty_ivf(dim::Int)
    return IVFIndex(
        0, 0, 0, false,
        Matrix{Float32}(undef, dim, 0),
        Matrix{Float32}(undef, dim, 0),
        Int[], Int[], Int[1],
        Float32[], Int[], Float32[],
    )
end

"""
    VecStore

Preallocated column-major vector and metadata store with zero-allocation
exact BLAS (`gemv!`) and approximate Contiguous-IVF (`_scan_columns_reg4!`) search.
"""
mutable struct VecStore
    name::String
    dim::Int
    count::Int
    capacity::Int
    embeddings::Matrix{Float32}              # dim × capacity (unit L2 columns)
    ids::Vector{String}                      # length capacity (valid 1:count)
    id_to_idx::Dict{String,Int}              # external id -> 1:count
    metadata::Vector{Dict{String,Any}}       # length capacity (valid 1:count)
    q_norm::Vector{Float32}                  # length dim scratch
    scores_buf::Vector{Float32}              # length capacity scratch for exact BLAS
    top_idx_buf::Vector{Int}                 # length _MAX_K scratch
    top_val_buf::Vector{Float32}             # length _MAX_K scratch
    filter_mask::BitVector                   # length capacity preallocated mask for duck-type filtering
    ivf::IVFIndex
end

const _STORES = Dict{String,VecStore}()
const _STORES_LOCK = ReentrantLock()

function VecStore(name::AbstractString; dim::Int=_DEFAULT_DIM, capacity::Int=_DEFAULT_CAPACITY)
    _MIN_DIM <= dim <= _MAX_DIM || throw(ArgumentError("dim must be in $_MIN_DIM..$_MAX_DIM"))
    1 <= capacity <= _MAX_ITEMS || throw(ArgumentError("capacity must be in 1..$_MAX_ITEMS"))
    cap = max(capacity, 16)
    return VecStore(
        String(name),
        dim,
        0,
        cap,
        zeros(Float32, dim, cap),
        Vector{String}(undef, cap),
        Dict{String,Int}(),
        Vector{Dict{String,Any}}(undef, cap),
        zeros(Float32, dim),
        zeros(Float32, cap),
        zeros(Int, _MAX_K),
        fill(-Inf32, _MAX_K),
        trues(cap),
        _empty_ivf(dim),
    )
end

function _ensure_capacity!(store::VecStore, needed::Int)
    needed <= _MAX_ITEMS || throw(ArgumentError("store capacity cannot exceed $_MAX_ITEMS items"))
    needed <= store.capacity && return store
    new_cap = store.capacity
    while new_cap < needed
        new_cap = min(_MAX_ITEMS, max(new_cap * 2, 16))
    end
    new_emb = zeros(Float32, store.dim, new_cap)
    if store.count > 0
        @views copyto!(new_emb[:, 1:store.count], store.embeddings[:, 1:store.count])
    end
    store.embeddings = new_emb
    resize!(store.ids, new_cap)
    resize!(store.metadata, new_cap)
    resize!(store.scores_buf, new_cap)
    resize!(store.filter_mask, new_cap)
    store.capacity = new_cap
    return store
end

@inline function _normalize_copy!(dst::AbstractVector{Float32}, src::AbstractVector{<:Real})
    d = length(dst)
    length(src) == d || throw(DimensionMismatch("expected vector of length $d, got $(length(src))"))
    norm_sq = 0.0f0
    @inbounds for i in 1:d
        v = Float32(src[i])
        isfinite(v) || throw(ArgumentError("vector contains non-finite value at index $i"))
        norm_sq = muladd(v, v, norm_sq)
    end
    norm_sq > 0.0f0 || throw(ArgumentError("vector norm must be positive"))
    inv_norm = 1.0f0 / sqrt(norm_sq)
    @inbounds @simd for i in 1:d
        dst[i] = Float32(src[i]) * inv_norm
    end
    return dst
end

# ---------------------------------------------------------------------------
# Zero-Allocation SplitMix64 Feature Hashing for Text & Code
# ---------------------------------------------------------------------------

@inline function _mix64(x::UInt64)::UInt64
    z = x + 0x9e3779b97f4a7c15
    z = (z ⊻ (z >> 30)) * 0xbf58476d1ce4e5b9
    z = (z ⊻ (z >> 27)) * 0x94d049bb133111eb
    return z ⊻ (z >> 31)
end

@inline function _accum_feature!(out::AbstractVector{Float32}, h::UInt64, weight::Float32, d::UInt64)
    m1 = _mix64(h)
    idx1 = Int((m1 % d) + 1)
    sign1 = ifelse((m1 & 0x8000000000000000) == 0, weight, -weight)
    @inbounds out[idx1] += sign1

    m2 = _mix64(m1 ⊻ 0xd6e8feb86659fd93)
    idx2 = Int((m2 % d) + 1)
    sign2 = ifelse((m2 & 0x8000000000000000) == 0, weight, -weight)
    @inbounds out[idx2] += sign2
    return nothing
end

@inline _is_ident_byte(b::UInt8)::Bool =
    (UInt8('a') <= b <= UInt8('z')) ||
    (UInt8('A') <= b <= UInt8('Z')) ||
    (UInt8('0') <= b <= UInt8('9')) ||
    (b == UInt8('_')) || (b == UInt8('!')) || (b >= 0x80)

@inline _is_upper_byte(b::UInt8)::Bool = (UInt8('A') <= b <= UInt8('Z'))
@inline _is_lower_byte(b::UInt8)::Bool = (UInt8('a') <= b <= UInt8('z'))
@inline _fold_lower(b::UInt8)::UInt8 = _is_upper_byte(b) ? (b + 0x20) : b

"""
    embed_text!(out::AbstractVector{Float32}, text::AbstractString) -> out

Compute a unit-normalized `Float32` embedding from `text` in-place with **0 heap
allocations**. Extracts case-folded identifier/word tokens (weight 2.0),
`snake_case` / `camelCase` sub-tokens (weight 1.5), and character 3/4-grams
(weight 0.75) in a single pass over `codeunits(text)`.
"""
function embed_text!(out::AbstractVector{Float32}, text::AbstractString)
    d = length(out)
    d >= _MIN_DIM || throw(ArgumentError("output dimension must be >= $_MIN_DIM"))
    du = UInt64(d)
    fill!(out, 0.0f0)

    cu = codeunits(text)
    n = length(cu)
    n > 0 || throw(ArgumentError("text must be non-empty"))

    FNV_OFFSET = 0xcbf29ce484222325
    FNV_PRIME  = 0x00000100000001b3

    tok_h = FNV_OFFSET
    sub_h = FNV_OFFSET
    tok_len = 0
    sub_len = 0
    prev_b = UInt8(0)
    b1 = UInt8(0)
    b2 = UInt8(0)
    b3 = UInt8(0)

    @inbounds for i in 1:n
        raw_b = cu[i]
        if _is_ident_byte(raw_b)
            low_b = _fold_lower(raw_b)
            # Detect snake_case, Julia mutating '!', or camelCase sub-token boundaries
            if raw_b == UInt8('_') || raw_b == UInt8('!')
                if sub_len >= 2
                    _accum_feature!(out, sub_h, 1.5f0, du)
                end
                sub_h = FNV_OFFSET
                sub_len = 0
            else
                if _is_upper_byte(raw_b) && _is_lower_byte(prev_b) && sub_len >= 2
                    _accum_feature!(out, sub_h, 1.5f0, du)
                    sub_h = FNV_OFFSET
                    sub_len = 0
                end
                sub_h = (sub_h ⊻ UInt64(low_b)) * FNV_PRIME
                sub_len += 1
            end

            tok_h = (tok_h ⊻ UInt64(low_b)) * FNV_PRIME
            tok_len += 1

            # Character 3-grams and 4-grams within the token
            if tok_len >= 3
                ng3 = (UInt64(b2) << 16) | (UInt64(b1) << 8) | UInt64(low_b)
                _accum_feature!(out, ng3 ⊻ 0x3333333333333333, 0.75f0, du)
            end
            if tok_len >= 4
                ng4 = (UInt64(b3) << 24) | (UInt64(b2) << 16) | (UInt64(b1) << 8) | UInt64(low_b)
                _accum_feature!(out, ng4 ⊻ 0x4444444444444444, 0.75f0, du)
            end
            b3 = b2
            b2 = b1
            b1 = low_b
            prev_b = raw_b
        else
            if tok_len > 0
                _accum_feature!(out, tok_h, 2.0f0, du)
                if sub_len >= 2 && sub_len < tok_len
                    _accum_feature!(out, sub_h, 1.5f0, du)
                end
                tok_h = FNV_OFFSET
                sub_h = FNV_OFFSET
                tok_len = 0
                sub_len = 0
                prev_b = UInt8(0)
            end
        end
    end

    if tok_len > 0
        _accum_feature!(out, tok_h, 2.0f0, du)
        if sub_len >= 2 && sub_len < tok_len
            _accum_feature!(out, sub_h, 1.5f0, du)
        end
    end

    # Fallback if text had only punctuation/whitespace
    norm_sq = 0.0f0
    @inbounds for i in 1:d
        norm_sq = muladd(out[i], out[i], norm_sq)
    end
    if norm_sq == 0.0f0
        h = FNV_OFFSET
        @inbounds for i in 1:n
            h = (h ⊻ UInt64(cu[i])) * FNV_PRIME
        end
        _accum_feature!(out, h, 1.0f0, du)
        @inbounds for i in 1:d
            norm_sq = muladd(out[i], out[i], norm_sq)
        end
    end

    inv_norm = 1.0f0 / sqrt(norm_sq)
    @inbounds @simd for i in 1:d
        out[i] *= inv_norm
    end
    return out
end

# ---------------------------------------------------------------------------
# Upsert & Delete Operations
# ---------------------------------------------------------------------------

"""
    upsert!(store::VecStore, id::AbstractString, vec::AbstractVector{<:Real};
            metadata::Dict{String,Any}=Dict{String,Any}()) -> Int

Insert or update `id` in `store` with unit-normalized `vec`.
Appending new items preserves an existing IVF index (`ivf.valid == true`) by
scanning the unindexed tail (`ivf.indexed_count + 1 : store.count`) at query time.
Overwriting an already-IVF-indexed item updates `embeddings` and marks `ivf.valid = false`.
"""
function upsert!(store::VecStore, id::AbstractString, vec::AbstractVector{<:Real};
                 metadata::Dict{String,Any}=Dict{String,Any}())
    id_str = String(id)
    !isempty(id_str) && ncodeunits(id_str) <= _MAX_ID_BYTES ||
        throw(ArgumentError("id must be a non-empty string of at most $_MAX_ID_BYTES bytes"))
    existing = get(store.id_to_idx, id_str, 0)
    if existing > 0
        @views _normalize_copy!(store.embeddings[:, existing], vec)
        store.metadata[existing] = metadata
        if store.ivf.valid && existing <= store.ivf.indexed_count
            store.ivf.valid = false
        end
        return existing
    else
        new_idx = store.count + 1
        _ensure_capacity!(store, new_idx)
        @views _normalize_copy!(store.embeddings[:, new_idx], vec)
        store.ids[new_idx] = id_str
        store.metadata[new_idx] = metadata
        store.id_to_idx[id_str] = new_idx
        store.count = new_idx
        return new_idx
    end
end

function upsert_text!(store::VecStore, id::AbstractString, text::AbstractString;
                      metadata::Dict{String,Any}=Dict{String,Any}())
    ncodeunits(text) <= _MAX_TEXT_BYTES ||
        throw(ArgumentError("text exceeds the $_MAX_TEXT_BYTES-byte limit"))
    embed_text!(store.q_norm, text)
    return upsert!(store, id, store.q_norm; metadata=metadata)
end

function delete_item!(store::VecStore, id::AbstractString)::Bool
    idx = get(store.id_to_idx, String(id), 0)
    idx == 0 && return false
    last_idx = store.count
    delete!(store.id_to_idx, String(id))
    if idx < last_idx
        moved_id = store.ids[last_idx]
        @views copyto!(store.embeddings[:, idx], store.embeddings[:, last_idx])
        store.ids[idx] = moved_id
        store.metadata[idx] = store.metadata[last_idx]
        store.id_to_idx[moved_id] = idx
    end
    store.ids[last_idx] = ""
    store.metadata[last_idx] = Dict{String,Any}()
    store.count = last_idx - 1
    store.ivf.valid = false
    return true
end

# ---------------------------------------------------------------------------
# Contiguous-Cluster IVF Index Builder (Spherical k-Means + Column Permutation)
# ---------------------------------------------------------------------------

"""
    build_ivf!(store::VecStore; n_list::Int=0, n_probe::Int=0, iters::Int=3, seed::Int=42) -> IVFIndex

Build a Contiguous-Cluster Inverted File (IVF) index over `store.embeddings[:, 1:store.count]`.
Uses BLAS `gemm!` spherical k-means and permutes database columns contiguously by cluster.
"""
function build_ivf!(store::VecStore; n_list::Int=0, n_probe::Int=0, iters::Int=3, seed::Int=42)
    N = store.count
    d = store.dim
    N > 0 || throw(ArgumentError("cannot build IVF index on an empty VecStore"))

    actual_n_list = n_list > 0 ? min(n_list, N) : clamp(round(Int, sqrt(N) / 2), 4, min(256, N))
    actual_n_probe = n_probe > 0 ? min(n_probe, actual_n_list) : clamp(div(actual_n_list + 2, 3), 2, actual_n_list)


    rng = MersenneTwister(seed)
    init_ids = randperm(rng, N)[1:actual_n_list]
    mat_view = @view store.embeddings[:, 1:N]
    centroids = Matrix{Float32}(undef, d, actual_n_list)
    @inbounds for c in 1:actual_n_list
        @views copyto!(centroids[:, c], mat_view[:, init_ids[c]])
    end

    assign = zeros(Int, N)
    counts = zeros(Int, actual_n_list)
    sim_mat = Matrix{Float32}(undef, actual_n_list, N)

    for _ in 1:max(iters, 1)
        mul!(sim_mat, transpose(centroids), mat_view)
        fill!(counts, 0)
        new_centroids = zeros(Float32, d, actual_n_list)
        @inbounds for j in 1:N
            best_c = 1
            best_v = sim_mat[1, j]
            for c in 2:actual_n_list
                v = sim_mat[c, j]
                if v > best_v
                    best_v = v
                    best_c = c
                end
            end
            assign[j] = best_c
            counts[best_c] += 1
            for i in 1:d
                new_centroids[i, best_c] += mat_view[i, j]
            end
        end
        @inbounds for c in 1:actual_n_list
            if counts[c] > 0
                norm_sq = 0.0f0
                for i in 1:d
                    norm_sq = muladd(new_centroids[i, c], new_centroids[i, c], norm_sq)
                end
                inv_n = norm_sq > 0.0f0 ? (1.0f0 / sqrt(norm_sq)) : 1.0f0
                for i in 1:d
                    centroids[i, c] = new_centroids[i, c] * inv_n
                end
            end
        end
    end

    cluster_offsets = zeros(Int, actual_n_list + 1)
    cluster_offsets[1] = 1
    @inbounds for c in 1:actual_n_list
        cluster_offsets[c + 1] = cluster_offsets[c] + counts[c]
    end

    perm_to_id = zeros(Int, N)
    id_to_perm = zeros(Int, N)
    embeddings_perm = Matrix{Float32}(undef, d, N)
    write_pos = copy(cluster_offsets[1:actual_n_list])

    @inbounds for j in 1:N
        c = assign[j]
        pos = write_pos[c]
        write_pos[c] = pos + 1
        perm_to_id[pos] = j
        id_to_perm[j] = pos
        for i in 1:d
            embeddings_perm[i, pos] = mat_view[i, j]
        end
    end

    store.ivf = IVFIndex(
        actual_n_list,
        actual_n_probe,
        N,
        true,
        centroids,
        embeddings_perm,
        perm_to_id,
        id_to_perm,
        cluster_offsets,
        zeros(Float32, actual_n_list),
        zeros(Int, actual_n_list),
        fill(-Inf32, actual_n_list),
    )
    return store.ivf
end

# ---------------------------------------------------------------------------
# Zero-Allocation Top-K Selection & 4-Way AVX2 FMA Inner Kernel
# ---------------------------------------------------------------------------

@inline function _insert_topk!(top_idx::AbstractVector{Int}, top_val::AbstractVector{Float32},
                               k::Int, doc_id::Int, s::Float32)
    @inbounds if s > top_val[k]
        pos = k
        while pos > 1 && s > top_val[pos - 1]
            top_val[pos] = top_val[pos - 1]
            top_idx[pos] = top_idx[pos - 1]
            pos -= 1
        end
        top_val[pos] = s
        top_idx[pos] = doc_id
    end
    return nothing
end

function select_topk_max!(
    top_idx::AbstractVector{Int},
    top_val::AbstractVector{Float32},
    scores::AbstractVector{Float32},
    k::Int,
    allow_mask::Union{Nothing,BitVector}=nothing,
)
    1 <= k <= min(length(top_idx), length(top_val)) ||
        throw(BoundsError("top_idx and top_val must have length >= k ($k)"))
    n = length(scores)
    actual_k = min(k, n)
    @inbounds for i in 1:k
        top_val[i] = -Inf32
        top_idx[i] = 0
    end
    actual_k == 0 && return top_idx
    if allow_mask === nothing
        @inbounds for j in 1:n
            _insert_topk!(top_idx, top_val, actual_k, j, scores[j])
        end
    else
        length(allow_mask) >= n || throw(BoundsError("allow_mask length < scores length"))
        @inbounds for j in 1:n
            if allow_mask[j]
                _insert_topk!(top_idx, top_val, actual_k, j, scores[j])
            end
        end
    end
    return top_idx
end

"""
    _scan_columns_reg4!(top_idx, top_val, k, mat, q_norm, start_col, end_col, id_map, allow_mask=nothing)

4-column AVX2 FMA (`muladd`) register-blocked cosine similarity kernel.
Loads `q_norm[i]` once per 4 database columns and fuses optional `allow_mask`
filtering and top-k insertion inline with **0 bytes allocated**.
"""
@inline function _scan_columns_reg4!(
    top_idx::Vector{Int},
    top_val::Vector{Float32},
    k::Int,
    mat::Matrix{Float32},
    q_norm::Vector{Float32},
    start_col::Int,
    end_col::Int,
    id_map::Vector{Int},
    allow_mask::Union{Nothing,BitVector}=nothing,
)
    d = size(mat, 1)
    use_map = !isempty(id_map)
    (1 <= k <= min(length(top_idx), length(top_val)) &&
     length(q_norm) == d && 1 <= start_col && end_col <= size(mat, 2) &&
     (!use_map || end_col <= length(id_map))) ||
        throw(DimensionMismatch("invalid buffer dimensions in _scan_columns_reg4!"))
    p = start_col
    @inbounds while p + 3 <= end_col
        s1 = 0.0f0
        s2 = 0.0f0
        s3 = 0.0f0
        s4 = 0.0f0
        @simd for i in 1:d
            qi = q_norm[i]
            s1 = muladd(mat[i, p],     qi, s1)
            s2 = muladd(mat[i, p + 1], qi, s2)
            s3 = muladd(mat[i, p + 2], qi, s3)
            s4 = muladd(mat[i, p + 3], qi, s4)
        end
        id1 = use_map ? id_map[p] : p
        if (allow_mask === nothing || allow_mask[id1]) && s1 > top_val[k]
            _insert_topk!(top_idx, top_val, k, id1, s1)
        end
        id2 = use_map ? id_map[p + 1] : (p + 1)
        if (allow_mask === nothing || allow_mask[id2]) && s2 > top_val[k]
            _insert_topk!(top_idx, top_val, k, id2, s2)
        end
        id3 = use_map ? id_map[p + 2] : (p + 2)
        if (allow_mask === nothing || allow_mask[id3]) && s3 > top_val[k]
            _insert_topk!(top_idx, top_val, k, id3, s3)
        end
        id4 = use_map ? id_map[p + 3] : (p + 3)
        if (allow_mask === nothing || allow_mask[id4]) && s4 > top_val[k]
            _insert_topk!(top_idx, top_val, k, id4, s4)
        end
        p += 4
    end
    @inbounds while p <= end_col
        id = use_map ? id_map[p] : p
        if allow_mask === nothing || allow_mask[id]
            s = 0.0f0
            @simd for i in 1:d
                s = muladd(mat[i, p], q_norm[i], s)
            end
            if s > top_val[k]
                _insert_topk!(top_idx, top_val, k, id, s)
            end
        end
        p += 1
    end
    return nothing
end

"""
    search_exact!(top_idx::Vector{Int}, top_val::Vector{Float32},
                  store::VecStore, query::AbstractVector{Float32}, k::Int;
                  normalize_query::Bool=true, allow_mask::Union{Nothing,BitVector}=nothing) -> Int

Zero-allocation exact cosine search using OpenBLAS `gemv!('T', ...)` and in-place
top-k insertion. Supports inline `allow_mask` pre-filtering.
"""
function search_exact!(
    top_idx::Vector{Int},
    top_val::Vector{Float32},
    store::VecStore,
    query::AbstractVector{Float32},
    k::Int;
    normalize_query::Bool=true,
    allow_mask::Union{Nothing,BitVector}=nothing,
)::Int
    1 <= k <= min(length(top_idx), length(top_val)) ||
        throw(BoundsError("top_idx and top_val must have length >= k ($k)"))
    N = store.count
    actual_k = min(k, N)
    @inbounds for i in 1:k
        top_val[i] = -Inf32
        top_idx[i] = 0
    end
    N == 0 && return 0
    if normalize_query
        _normalize_copy!(store.q_norm, query)
    elseif query !== store.q_norm
        copyto!(store.q_norm, query)
    end
    mat_view = @view store.embeddings[:, 1:N]
    scores_view = @view store.scores_buf[1:N]
    BLAS.gemv!('T', 1.0f0, mat_view, store.q_norm, 0.0f0, scores_view)
    select_topk_max!(top_idx, top_val, scores_view, actual_k, allow_mask)
    return N
end

const _EMPTY_INT_VEC = Int[]

"""
    search_ivf!(top_idx::Vector{Int}, top_val::Vector{Float32},
                store::VecStore, query::AbstractVector{Float32}, k::Int;
                n_probe::Int=0, normalize_query::Bool=true,
                allow_mask::Union{Nothing,BitVector}=nothing) -> Int

Zero-allocation Contiguous-Cluster IVF search with 4-column AVX2 FMA register blocking
and optional `allow_mask` duck-type pre-filtering.
"""
function search_ivf!(
    top_idx::Vector{Int},
    top_val::Vector{Float32},
    store::VecStore,
    query::AbstractVector{Float32},
    k::Int;
    n_probe::Int=0,
    normalize_query::Bool=true,
    allow_mask::Union{Nothing,BitVector}=nothing,
)::Int
    1 <= k <= min(length(top_idx), length(top_val)) ||
        throw(BoundsError("top_idx and top_val must have length >= k ($k)"))
    N = store.count
    actual_k = min(k, N)
    @inbounds for i in 1:k
        top_val[i] = -Inf32
        top_idx[i] = 0
    end
    N == 0 && return 0

    ivf = store.ivf
    if !ivf.valid || ivf.n_list == 0
        return search_exact!(top_idx, top_val, store, query, k;
                             normalize_query=normalize_query, allow_mask=allow_mask)
    end

    if normalize_query
        _normalize_copy!(store.q_norm, query)
    elseif query !== store.q_norm
        copyto!(store.q_norm, query)
    end

    probes = n_probe > 0 ? min(n_probe, ivf.n_list) : ivf.n_probe
    BLAS.gemv!('T', 1.0f0, ivf.centroids, store.q_norm, 0.0f0, ivf.coarse_scores)
    select_topk_max!(ivf.coarse_idx, ivf.coarse_val, ivf.coarse_scores, probes)

    scored = 0
    @inbounds for c_rank in 1:probes
        c = ivf.coarse_idx[c_rank]
        c <= 0 && continue
        start_col = ivf.cluster_offsets[c]
        end_col = ivf.cluster_offsets[c + 1] - 1
        if end_col >= start_col
            _scan_columns_reg4!(
                top_idx, top_val, actual_k,
                ivf.embeddings_perm, store.q_norm,
                start_col, end_col, ivf.perm_to_id,
                allow_mask,
            )
            scored += (end_col - start_col + 1)
        end
    end

    # Scan any incremental appends added since build_ivf!
    if N > ivf.indexed_count
        tail_start = ivf.indexed_count + 1
        _scan_columns_reg4!(
            top_idx, top_val, actual_k,
            store.embeddings, store.q_norm,
            tail_start, N, _EMPTY_INT_VEC,
            allow_mask,
        )
        scored += (N - ivf.indexed_count)
    end

    # If a selective allow_mask yielded fewer than actual_k hits in the probed coarse centroids,
    # scan all allowed columns directly (still 0 bytes allocated) so filtered recall is never starved.
    if allow_mask !== nothing && probes < ivf.n_list && top_idx[actual_k] == 0
        @inbounds for i in 1:k
            top_val[i] = -Inf32
            top_idx[i] = 0
        end
        _scan_columns_reg4!(
            top_idx, top_val, actual_k,
            store.embeddings, store.q_norm,
            1, N, _EMPTY_INT_VEC,
            allow_mask,
        )
        return N
    end

    return scored
end

# High-level allocation-minimal wrapper for Palette.reference! context verification
function search_exact_ids(store::VecStore, query::AbstractVector{Float32}, k::Int)::Vector{Int}
    actual_k = min(k, store.count, _MAX_K)
    actual_k <= 0 && return Int[]
    search_exact!(store.top_idx_buf, store.top_val_buf, store, query, actual_k)
    return store.top_idx_buf[1:actual_k]
end

function search_ivf_ids(store::VecStore, query::AbstractVector{Float32}, k::Int)::Vector{Int}
    actual_k = min(k, store.count, _MAX_K)
    actual_k <= 0 && return Int[]
    search_ivf!(store.top_idx_buf, store.top_val_buf, store, query, actual_k)
    return store.top_idx_buf[1:actual_k]
end

function register_palette_hooks!()
    if isdefined(@__MODULE__, :Palette) && isdefined(Palette, :reference!)
        Palette.reference!(search_ivf_ids, Tuple{VecStore,Vector{Float32},Int}, search_exact_ids)
        return true
    end
    return false
end

# ---------------------------------------------------------------------------
# Duck-Type Trait Extraction & Polyglot Codegraph Integration
# ---------------------------------------------------------------------------

const _POLYGLOT_EXT_LANG = Dict{String,String}(
    ".jl"  => "julia",
    ".py"  => "python",
    ".rs"  => "rust",
    ".ts"  => "typescript",
    ".tsx" => "typescript",
    ".js"  => "javascript",
    ".jsx" => "javascript",
    ".go"  => "go",
    ".c"   => "c",
    ".h"   => "c",
    ".cpp" => "cpp",
    ".cc"  => "cpp",
    ".hpp" => "cpp",
    ".cs"  => "csharp",
)

const _SKIP_SCAN_DIRS = Set([
    ".git", "vendor", "target", "node_modules", "__pycache__", "build", "dist", "_build", "venv", ".venv",
    "scratch", "archive", "backup", "tmp", "old",
])

function _is_scratch_dir(d::AbstractString)::Bool
    startswith(d, ".") && return true
    d in _SKIP_SCAN_DIRS && return true
    dl = lowercase(d)
    (startswith(dl, "backup_") || startswith(dl, "scratch_")) && return true
    return false
end

function _is_scratch_path(rel::AbstractString)::Bool
    for part in eachsplit(rel, '/')
        _is_scratch_dir(part) && return true
    end
    return false
end

# Git's own ignore rules decide when the root is a repository; the name
# heuristics above decide otherwise.
_skip_path(rel::AbstractString, git_allowed::Union{Nothing,Set{String}}) =
    git_allowed === nothing ? _is_scratch_path(rel) : !(rel in git_allowed)


const _NOISE_CALLEES = Set([
    "+", "-", "*", "/", "\\", "^", "%", "==", "!=", "===", "!==", "<", "<=", ">", ">=",
    ":", "=>", "|", "&", "!", "~", "getindex", "setindex!", "firstindex", "lastindex", "Colon",
])

function _is_noise_callee(c::AbstractString)::Bool
    isempty(c) && return true
    c in _NOISE_CALLEES && return true
    # Filter pure operator symbols (no ASCII letters or underscore)
    for b in codeunits(c)
        if (b >= UInt8('A') && b <= UInt8('Z')) || (b >= UInt8('a') && b <= UInt8('z')) || b == UInt8('_')
            return false
        end
    end
    return true
end

"""
    _extract_line_duck_traits(lines, start_line, end_line) -> (fields, callees, params, arity)

Fast, language-agnostic lexical extractor over `lines[start_line:end_line]`:
- `fields`: `.identifier` property accesses (`self.mgr`, `store.ivf`, `this.state`)
  and `field::Type` / `field: Type` struct declarations.
- `callees`: `identifier(` invocations (excluding AST operator noise).
- `params` & `arity`: parameter names, type tags, and base classes from the signature header.
"""
function _extract_line_duck_traits(lines::Vector{String}, start_line::Int, end_line::Int)
    fields = String[]
    callees = String[]
    params = String[]
    arity = 0
    s_line = clamp(start_line, 1, length(lines))
    e_line = clamp(end_line, s_line, min(length(lines), s_line + 250))

    # Extract parameters from signature line(s) (UTF-8 safe via nextind/prevind)
    header = lines[s_line]
    if s_line < e_line && !occursin(')', header)
        header = header * " " * lines[s_line + 1]
    end
    p_open = findfirst('(', header)
    p_close = p_open === nothing ? nothing : findnext(')', header, nextind(header, p_open))
    if p_open !== nothing && p_close !== nothing && p_close > nextind(header, p_open)
        inner = SubString(header, nextind(header, p_open), prevind(header, p_close))
        for part in eachsplit(inner, ',')
            tok = strip(part)
            isempty(tok) && continue
            (tok == "self" || tok == "cls" || tok == "&self" || tok == "&mut self" || tok == "this") && continue
            arity += 1
            for m in eachmatch(r"[A-Za-z_][A-Za-z0-9_!]*", tok)
                push!(params, String(m.match))
            end
        end
    end

    # Python return type annotation `-> ReturnType:` on def header
    m_py_ret = match(r"->\s*([A-Za-z_][A-Za-z0-9_.\[\], ]*)\s*:", header)
    if m_py_ret !== nothing
        for tm in eachmatch(r"[A-Za-z_][A-Za-z0-9_]*", m_py_ret.captures[1])
            push!(params, String(tm.match))
        end
    end

    kw_skip = Set([
        "if", "elif", "for", "foreach", "while", "switch", "return", "catch", "except", "with",
        "lock", "using", "fixed", "function", "def", "fn", "class", "in", "where", "nameof",
        "typeof", "sizeof", "default", "print", "len", "range", "isinstance", "super",
    ])
    # Extract C# / C++ / Rust / TS inheritance / implemented interfaces from header line (e.g. `class GamePlayer : GameLiving, IGamePlayer`)
    m_inh = match(r"(?:class|struct|interface)\s+[A-Za-z_][A-Za-z0-9_]*\s*:\s*([^{]+)", header)
    if m_inh !== nothing
        for base_part in eachsplit(m_inh.captures[1], ',')
            b = strip(base_part)
            m_b = match(r"^([A-Za-z_][A-Za-z0-9_]*)", b)
            m_b !== nothing && push!(params, String(m_b.captures[1]))
        end
    end

    for ln in s_line:e_line
        line = lines[ln]
        # 1. Dot-property accesses: obj.field (skipping numeric literals like 1.0f0)
        for m in eachmatch(r"[A-Za-z_\)\]]\.([A-Za-z_][A-Za-z0-9_]*)", line)
            f = String(m.captures[1])
            push!(fields, f)
        end
        # 1b. Python imports (`from pkg.mod import sym1, sym2` or `import pkg`)
        m_from = match(r"^\s*from\s+([A-Za-z_][A-Za-z0-9_.]*)\s+import\s+(.+)$", line)
        if m_from !== nothing
            push!(params, String(m_from.captures[1]))
            for imp in eachsplit(m_from.captures[2], ',')
                m_i = match(r"^([A-Za-z_][A-Za-z0-9_]*)", strip(imp))
                m_i !== nothing && push!(callees, String(m_i.captures[1]))
            end
        end
        # 2a. Julia / Rust / TS / Python typed field declarations (`centroids::Matrix{Float32}` or `pub count: usize`)
        if ln > s_line
            m_decl = match(r"^\s*(?:pub\s+|readonly\s+|private\s+|public\s+|protected\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*(?:::|:)\s*([A-Za-z_][A-Za-z0-9_]*)", line)
            if m_decl !== nothing && !(String(m_decl.captures[1]) in ("if", "elif", "else", "for", "while", "try", "except", "finally", "with", "class", "def", "return"))
                push!(fields, String(m_decl.captures[1]))
                push!(params, String(m_decl.captures[2]))
            else
                # 2b. C# / C++ / Java property & field declarations (`public virtual int Health { get; set; }`, `protected readonly GameClient m_client;`)
                m_cs = match(r"^\s*(?:(?:public|private|protected|internal|virtual|override|abstract|sealed|static|readonly|volatile|new)\s+)+([A-Za-z_][A-Za-z0-9_<>.,\[\]\?]*)\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:\{|;|=>|=)", line)
                if m_cs !== nothing
                    t_raw = String(m_cs.captures[1])
                    f_name = String(m_cs.captures[2])
                    if !(t_raw in ("class", "struct", "interface", "enum", "return", "namespace", "using"))
                        push!(fields, f_name)
                        for tm in eachmatch(r"[A-Za-z_][A-Za-z0-9_]*", t_raw)
                            push!(params, String(tm.match))
                        end
                    end
                end
            end
        end
        # 3. Function/method calls: callee(
        for m in eachmatch(r"([A-Za-z_][A-Za-z0-9_!]*)\s*\(", line)
            c = String(m.captures[1])
            (c in kw_skip || _is_noise_callee(c)) && continue
            push!(callees, c)
        end
    end
    return unique!(fields), unique!(callees), unique!(params), arity
end

function _scan_polyglot_defs(lines::Vector{String}, lang::String)
    defs = Vector{NamedTuple{(:name, :kind, :line),Tuple{String,String,Int}}}()
    for (idx, line) in enumerate(lines)
        if lang == "typescript" || lang == "javascript"
            m = match(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?(function|class|interface|type|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)", line)
            if m !== nothing
                push!(defs, (name=String(m.captures[2]), kind=String(m.captures[1]), line=idx))
            else
                m_const = match(r"^(?:export\s+)const\s+([A-Za-z_$][A-Za-z0-9_$]*)", line)
                m_const !== nothing && push!(defs, (name=String(m_const.captures[1]), kind="const", line=idx))
            end
        elseif lang == "go"
            m_fn = match(r"^\s*func\s+(?:\([^)]+\)\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(", line)
            if m_fn !== nothing
                push!(defs, (name=String(m_fn.captures[1]), kind="function", line=idx))
            else
                m_ty = match(r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)\s+(struct|interface)", line)
                m_ty !== nothing && push!(defs, (name=String(m_ty.captures[1]), kind=String(m_ty.captures[2]), line=idx))
            end
        elseif lang in ("c", "cpp", "csharp")
            m_st = match(r"^\s*(?:template\s*<[^>]*>\s*)?(?:(?:public|private|protected|internal|abstract|sealed|static|partial|readonly|unsafe|new)\s+)*(struct|class|interface|enum)\s+([A-Za-z_][A-Za-z0-9_]*)", line)
            if m_st !== nothing
                push!(defs, (name=String(m_st.captures[2]), kind=String(m_st.captures[1]), line=idx))
            else
                m_fn = match(r"^\s*(?:(?:public|private|protected|internal|virtual|override|abstract|sealed|static|async|extern|unsafe|inline|constexpr|new)\s+)*(?:[A-Za-z_][A-Za-z0-9_:<>\*&\?,\[\]\s]*)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*$", line)
                if m_fn !== nothing && !(String(m_fn.captures[1]) in ("if", "for", "foreach", "while", "switch", "return", "catch", "lock", "using", "fixed", "nameof", "typeof", "sizeof", "base", "this"))
                    push!(defs, (name=String(m_fn.captures[1]), kind="function", line=idx))
                end
            end
        elseif lang == "python"
            m = match(r"^\s*(?:async\s+)?(def|class)\s+([A-Za-z_][A-Za-z0-9_]*)", line)
            if m !== nothing
                k = m.captures[1] == "def" ? "function" : "class"
                push!(defs, (name=String(m.captures[2]), kind=k, line=idx))
            end
        elseif lang == "rust"
            m = match(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+)?(fn|struct|enum|trait|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)", line)
            if m !== nothing
                k = m.captures[1] == "fn" ? "function" : String(m.captures[1])
                push!(defs, (name=String(m.captures[2]), kind=k, line=idx))
            end
        elseif lang == "julia"
            m = match(r"^\s*(?:@[A-Za-z_.]+\s+)?(?:mutable\s+)?(function|struct|macro|const)\s+([A-Za-z_][A-Za-z0-9_!]*)", line)
            if m !== nothing
                push!(defs, (name=String(m.captures[2]), kind=String(m.captures[1]), line=idx))
            end
        end
    end
    # For top-level Python scripts without def/class, index a synthetic module entry at line 1
    if lang == "python" && isempty(defs) && !isempty(lines)
        push!(defs, (name="<module>", kind="module", line=1))
    end
    return defs
end

function _embed_symbol_with_callees!(
    primary_buf::Vector{Float32},
    callee_buf::Vector{Float32},
    primary_text::AbstractString,
    behavior_text::AbstractString,
)
    embed_text!(primary_buf, primary_text)
    if !isempty(behavior_text)
        embed_text!(callee_buf, behavior_text)
        d = length(primary_buf)
        @inbounds @simd for i in 1:d
            primary_buf[i] = muladd(0.80f0, primary_buf[i], 0.20f0 * callee_buf[i])
        end
    end
    return primary_buf
end

function _read_source_lines(root::AbstractString, relpath::AbstractString)::Vector{String}
    isempty(root) && return String[]
    full = joinpath(root, relpath)
    (!isfile(full) || islink(full)) && return String[]
    return try
        readlines(full)
    catch
        String[]
    end
end

function _git_nonignored_files(root_dir::AbstractString)::Union{Nothing,Set{String}}
    isempty(root_dir) && return nothing
    ispath(joinpath(root_dir, ".git")) || return nothing
    Sys.which("git") === nothing && return nothing
    try
        raw = read(`git -C $root_dir ls-files -z --cached --others --exclude-standard`, String)
        return Set{String}(replace(String(s), '\\' => '/') for s in eachsplit(raw, '\0') if !isempty(s))
    catch
        return nothing
    end
end

"""
    index_codegraph!(store::VecStore, cg; include_polyglot::Bool=true) -> Int

Index symbols from `Palette.codegraph()` (a `Pharos.CodeGraph`) into `store`
with full duck-type traits (`fields`, `callees`, `params`, `arity`,
`mutating`). When `include_polyglot=true` and
`cg.root` is present, also scans additional polyglot files (`.ts`, `.js`, `.go`,
`.c`, `.cpp`, `.h`, `.cs`) under `cg.root`.
"""
function index_codegraph!(store::VecStore, cg; include_polyglot::Bool=true)::Int
    hasproperty(cg, :files) || throw(ArgumentError("expected a Pharos.CodeGraph"))
    root_dir = hasproperty(cg, :root) ? String(cg.root) : ""
    git_allowed = _git_nonignored_files(root_dir)
    primary_buf = zeros(Float32, store.dim)
    callee_buf = zeros(Float32, store.dim)
    indexed = 0

    for path in sort!(collect(keys(cg.files)))
        _skip_path(replace(String(path), '\\' => '/'), git_allowed) && continue
        entry = cg.files[path]
        lang = string(entry.language)
        lines = _read_source_lines(root_dir, path)
        callees_by_caller = Dict{String,Vector{String}}()
        for c in entry.calls
            _is_noise_callee(c.callee) && continue
            push!(get!(callees_by_caller, c.caller, String[]), c.callee)
        end
        defs = entry.definitions
        if isempty(defs) && lang == "python" && !isempty(lines)
            defs = [(name=splitext(basename(path))[1], kind=:module, line=1)]
        end
        n_defs = length(defs)
        for (d_idx, def) in enumerate(defs)
            name = def.name
            isempty(name) && continue
            kind = string(def.kind)
            line = def.line
            next_line = d_idx < n_defs ? max(line, defs[d_idx + 1].line - 1) : length(lines)
            fields, lex_callees, params, arity = isempty(lines) ?
                (String[], String[], String[], 0) :
                _extract_line_duck_traits(lines, line, next_line)
            callees = unique!(vcat(get(callees_by_caller, name, String[]), lex_callees))
            filter!(c -> c != name && !_is_noise_callee(c), callees)
            mutating = endswith(name, "!")
            behavior_str = join(vcat(callees, fields, params), " ")
            id = "$(path):$(line):$(name)"
            primary_text = "$(name) $(name) $(name) $(kind) $(lang) $(path)"
            _embed_symbol_with_callees!(primary_buf, callee_buf, primary_text, behavior_str)
            meta = Dict{String,Any}(
                "name" => name,
                "kind" => kind,
                "language" => lang,
                "file" => path,
                "line" => line,
                "callees" => callees,
                "fields" => fields,
                "params" => params,
                "arity" => arity,
                "mutating" => mutating,
            )
            upsert!(store, id, primary_buf; metadata=meta)
            indexed += 1
        end
    end

    if include_polyglot && !isempty(root_dir) && isdir(root_dir)
        indexed += _index_extra_polyglot_files!(store, root_dir, Set(keys(cg.files)), primary_buf, callee_buf, git_allowed)
    end
    return indexed
end

function _index_extra_polyglot_files!(
    store::VecStore,
    root_dir::String,
    already_indexed::Set{String},
    primary_buf::Vector{Float32},
    callee_buf::Vector{Float32},
    git_allowed::Union{Nothing,Set{String}}=nothing,
)::Int
    indexed = 0
    for (dir, dirs, files) in walkdir(root_dir; follow_symlinks=false, onerror=_ -> nothing)
        filter!(d -> !_is_scratch_dir(d), dirs)
        for fname in sort!(files)
            _, ext = splitext(fname)
            lang = get(_POLYGLOT_EXT_LANG, lowercase(ext), nothing)
            lang === nothing && continue
            full = joinpath(dir, fname)
            islink(full) && continue
            rel = replace(relpath(full, root_dir), '\\' => '/')
            rel in already_indexed && continue
            _skip_path(rel, git_allowed) && continue
            lines = _read_source_lines(root_dir, rel)
            isempty(lines) && continue
            defs = _scan_polyglot_defs(lines, lang)
            n_defs = length(defs)
            for (d_idx, def) in enumerate(defs)
                store.count >= _MAX_ITEMS && return indexed
                name = def.name
                kind = def.kind
                line = def.line
                next_line = d_idx < n_defs ? max(line, defs[d_idx + 1].line - 1) : length(lines)
                fields, callees, params, arity = _extract_line_duck_traits(lines, line, next_line)
                filter!(c -> c != name, callees)
                mutating = endswith(name, "!")
                behavior_str = join(vcat(callees, fields, params), " ")
                id = "$(rel):$(line):$(name)"
                primary_text = "$(name) $(name) $(name) $(kind) $(lang) $(rel)"
                _embed_symbol_with_callees!(primary_buf, callee_buf, primary_text, behavior_str)
                meta = Dict{String,Any}(
                    "name" => name,
                    "kind" => kind,
                    "language" => lang,
                    "file" => rel,
                    "line" => line,
                    "callees" => callees,
                    "fields" => fields,
                    "params" => params,
                    "arity" => arity,
                    "mutating" => mutating,
                )
                upsert!(store, id, primary_buf; metadata=meta)
                indexed += 1
            end
        end
    end
    return indexed
end

# ---------------------------------------------------------------------------
# Duck-Type & Structural Subspace Filter Evaluation
# ---------------------------------------------------------------------------

const _ALLOWED_FILTER_KEYS = (
    "kind", "language", "file_contains", "mutating",
    "has_fields", "has_fields_all",
    "has_callees", "has_callees_all",
    "has_params", "min_arity", "max_arity", "metadata",
)

function _to_string_list(val, field_name::String)::Vector{String}
    if val isa String
        return String[val]
    elseif val isa AbstractVector
        out = String[]
        for x in val
            x isa String || throw(ArgumentError("filter.$field_name elements must be strings"))
            push!(out, x)
        end
        return out
    end
    throw(ArgumentError("filter.$field_name must be a string or array of strings"))
end

function _has_any_trait(meta::Dict{String,Any}, key::String, targets::Vector{String})::Bool
    isempty(targets) && return true
    arr = get(meta, key, nothing)
    arr isa AbstractVector || return false
    for t in targets
        for item in arr
            if item == t
                return true
            end
        end
    end
    return false
end

function _has_all_traits(meta::Dict{String,Any}, key::String, targets::Vector{String})::Bool
    isempty(targets) && return true
    arr = get(meta, key, nothing)
    arr isa AbstractVector || return false
    for t in targets
        found = false
        for item in arr
            if item == t
                found = true
                break
            end
        end
        found || return false
    end
    return true
end

"""
    compute_filter_mask!(store::VecStore, filt::AbstractDict) -> Int

Evaluate structural and duck-type filter predicates across `store.metadata[1:store.count]`
into `store.filter_mask[1:store.count]`. Returns the number of matching items.
"""
function compute_filter_mask!(store::VecStore, filt::AbstractDict)::Int
    for k in keys(filt)
        String(k) in _ALLOWED_FILTER_KEYS ||
            throw(ArgumentError("unsupported filter key '$(k)'; allowed: $(join(_ALLOWED_FILTER_KEYS, ", "))"))
    end
    kinds = haskey(filt, "kind") ? _to_string_list(filt["kind"], "kind") : String[]
    if "function" in kinds && !("fn" in kinds)
        push!(kinds, "fn")
    elseif "fn" in kinds && !("function" in kinds)
        push!(kinds, "function")
    end
    langs = haskey(filt, "language") ? _to_string_list(filt["language"], "language") : String[]
    file_sub = get(filt, "file_contains", nothing)
    file_sub === nothing || file_sub isa String || throw(ArgumentError("filter.file_contains must be a string"))
    mut_req = get(filt, "mutating", nothing)
    mut_req === nothing || mut_req isa Bool || throw(ArgumentError("filter.mutating must be a boolean"))
    f_any = haskey(filt, "has_fields") ? _to_string_list(filt["has_fields"], "has_fields") : String[]
    f_all = haskey(filt, "has_fields_all") ? _to_string_list(filt["has_fields_all"], "has_fields_all") : String[]
    c_any = haskey(filt, "has_callees") ? _to_string_list(filt["has_callees"], "has_callees") : String[]
    c_all = haskey(filt, "has_callees_all") ? _to_string_list(filt["has_callees_all"], "has_callees_all") : String[]
    p_any = haskey(filt, "has_params") ? _to_string_list(filt["has_params"], "has_params") : String[]
    min_ar = haskey(filt, "min_arity") ? _require_int_range(filt["min_arity"], "filter.min_arity", 0, 256) : -1
    max_ar = haskey(filt, "max_arity") ? _require_int_range(filt["max_arity"], "filter.max_arity", 0, 256) : -1
    exact_meta = get(filt, "metadata", nothing)
    exact_meta === nothing || exact_meta isa AbstractDict || throw(ArgumentError("filter.metadata must be an object"))

    matched = 0
    @inbounds for i in 1:store.count
        m = store.metadata[i]
        ok = true
        if !isempty(kinds) && !(String(get(m, "kind", "")) in kinds)
            ok = false
        end
        if ok && !isempty(langs) && !(String(get(m, "language", "")) in langs)
            ok = false
        end
        if ok && file_sub !== nothing && !occursin(file_sub, String(get(m, "file", "")))
            ok = false
        end
        if ok && mut_req !== nothing && Bool(get(m, "mutating", false)) != mut_req
            ok = false
        end
        if ok && !isempty(f_any) && !_has_any_trait(m, "fields", f_any)
            ok = false
        end
        if ok && !isempty(f_all) && !_has_all_traits(m, "fields", f_all)
            ok = false
        end
        if ok && !isempty(c_any) && !_has_any_trait(m, "callees", c_any)
            ok = false
        end
        if ok && !isempty(c_all) && !_has_all_traits(m, "callees", c_all)
            ok = false
        end
        if ok && !isempty(p_any) && !_has_any_trait(m, "params", p_any)
            ok = false
        end
        if ok && (min_ar >= 0 || max_ar >= 0)
            ar = Int(get(m, "arity", 0))
            if (min_ar >= 0 && ar < min_ar) || (max_ar >= 0 && ar > max_ar)
                ok = false
            end
        end
        if ok && exact_meta !== nothing
            for (mk, mv) in exact_meta
                if get(m, String(mk), nothing) != mv
                    ok = false
                    break
                end
            end
        end
        store.filter_mask[i] = ok
        matched += ifelse(ok, 1, 0)
    end
    return matched
end

# ---------------------------------------------------------------------------
# Runtime Capability ABI Entrypoint (`Palette.runtime("invoke", ...)`)
# ---------------------------------------------------------------------------

function _store_memory_bytes(store::VecStore)::Int
    base = sizeof(store.embeddings) + sizeof(store.q_norm) + sizeof(store.scores_buf) +
           sizeof(store.top_idx_buf) + sizeof(store.top_val_buf) + sizeof(store.filter_mask)
    ivf = store.ivf
    ivf_bytes = sizeof(ivf.centroids) + sizeof(ivf.embeddings_perm) +
                sizeof(ivf.perm_to_id) + sizeof(ivf.id_to_perm) +
                sizeof(ivf.cluster_offsets) + sizeof(ivf.coarse_scores) +
                sizeof(ivf.coarse_idx) + sizeof(ivf.coarse_val)
    return base + ivf_bytes
end

function _store_stats(store::VecStore)
    return Dict{String,Any}(
        "name" => store.name,
        "dim" => store.dim,
        "count" => store.count,
        "capacity" => store.capacity,
        "memory_bytes" => _store_memory_bytes(store),
        "ivf" => Dict{String,Any}(
            "valid" => store.ivf.valid,
            "n_list" => store.ivf.n_list,
            "n_probe" => store.ivf.n_probe,
            "indexed_count" => store.ivf.indexed_count,
            "unindexed_tail" => max(0, store.count - store.ivf.indexed_count),
        ),
    )
end

function _check_allowed_keys(args::Dict{String,Any}, allowed::Tuple{Vararg{String}}, err_msg::String)
    for k in keys(args)
        String(k) in allowed || throw(ArgumentError(err_msg))
    end
    return nothing
end

function _require_int_range(val, name::String, lo::Int, hi::Int)::Int
    val isa Integer && !(val isa Bool) && lo <= val <= hi ||
        throw(ArgumentError("$name must be an integer in $lo..$hi"))
    return Int(val)
end

function _validate_store_name(args::Dict{String,Any})::String
    name = get(args, "store", "default")
    name isa String && !isempty(name) && ncodeunits(name) <= 128 &&
        occursin(r"^[A-Za-z0-9_.\-]+$", name) ||
        throw(ArgumentError("store must be an alphanumeric/[-_.] name of at most 128 bytes"))
    return name
end

function _ensure_store_slot!(name::String)
    if !haskey(_STORES, name) && length(_STORES) >= _MAX_STORES
        throw(ArgumentError("maximum number of VecStores ($_MAX_STORES) reached; drop unused stores first"))
    end
    return nothing
end

function _get_store(args::Dict{String,Any})::VecStore
    name = _validate_store_name(args)
    store = get(_STORES, name, nothing)
    store === nothing && throw(ArgumentError("VecStore '$name' does not exist; create it first"))
    return store
end

function _parse_float32_vec(raw, expected_dim::Int)::Vector{Float32}
    raw isa AbstractVector || throw(ArgumentError("vector must be an array of numbers"))
    length(raw) == expected_dim ||
        throw(DimensionMismatch("expected vector of length $expected_dim, got $(length(raw))"))
    out = Vector{Float32}(undef, expected_dim)
    @inbounds for i in 1:expected_dim
        v = raw[i]
        v isa Real && !(v isa Bool) || throw(ArgumentError("vector element at $i must be a real number"))
        fv = Float32(v)
        isfinite(fv) || throw(ArgumentError("vector element at $i must be finite"))
        out[i] = fv
    end
    return out
end

function _diagnose()
    total_items = sum((s.count for s in values(_STORES)); init=0)
    total_bytes = sum((_store_memory_bytes(s) for s in values(_STORES)); init=0)
    return Dict{String,Any}(
        "status" => "ready",
        "version" => _VECINDEX_VERSION,
        "store_count" => length(_STORES),
        "max_stores" => _MAX_STORES,
        "total_items" => total_items,
        "total_memory_bytes" => total_bytes,
        "blas_vendor" => string(BLAS.lbt_get_config()),
        "stores" => [_store_stats(_STORES[k]) for k in sort!(collect(keys(_STORES)))],
    )
end

function _action_create(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "dim", "capacity", "overwrite"),
        "create accepts only store, dim, capacity, and overwrite")
    name = _validate_store_name(args)
    dim = _require_int_range(get(args, "dim", _DEFAULT_DIM), "dim", _MIN_DIM, _MAX_DIM)
    cap = _require_int_range(get(args, "capacity", _DEFAULT_CAPACITY), "capacity", 1, _MAX_ITEMS)
    overwrite = get(args, "overwrite", true)
    overwrite isa Bool || throw(ArgumentError("overwrite must be a boolean"))

    if haskey(_STORES, name) && !overwrite
        throw(ArgumentError("VecStore '$name' already exists and overwrite=false"))
    end
    _ensure_store_slot!(name)
    store = VecStore(name; dim=dim, capacity=cap)
    _STORES[name] = store
    return Dict{String,Any}("status" => "created", "store" => _store_stats(store))
end

function _upsert_single!(store::VecStore, item::AbstractDict)
    id = get(item, "id", nothing)
    id isa String || throw(ArgumentError("each upsert item requires a string 'id'"))
    meta_raw = get(item, "metadata", Dict{String,Any}())
    meta_raw isa AbstractDict || throw(ArgumentError("item metadata must be an object"))
    meta = Dict{String,Any}(String(k) => v for (k, v) in meta_raw)

    has_vec = haskey(item, "vector")
    has_text = haskey(item, "text")
    (has_vec ⊻ has_text) || throw(ArgumentError("each item must supply exactly one of 'vector' or 'text'"))

    if has_vec
        vec = _parse_float32_vec(item["vector"], store.dim)
        return upsert!(store, id, vec; metadata=meta)
    else
        text = item["text"]
        text isa String && !isempty(text) || throw(ArgumentError("item 'text' must be a non-empty string"))
        return upsert_text!(store, id, text; metadata=meta)
    end
end

function _action_upsert(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "id", "vector", "text", "metadata", "items", "auto_ivf"),
        "upsert accepts only store, id, vector, text, metadata, items, and auto_ivf")
    name = _validate_store_name(args)
    store = get!(_STORES, name) do
        _ensure_store_slot!(name)
        VecStore(name)
    end

    upserted = 0
    if haskey(args, "items")
        items = args["items"]
        items isa AbstractVector && !isempty(items) && length(items) <= _MAX_BATCH ||
            throw(ArgumentError("items must be a non-empty array of at most $_MAX_BATCH entries"))
        for item in items
            item isa AbstractDict || throw(ArgumentError("each entry in items must be an object"))
            _upsert_single!(store, item)
            upserted += 1
        end
    else
        _upsert_single!(store, args)
        upserted = 1
    end

    auto_ivf = get(args, "auto_ivf", false)
    auto_ivf isa Bool || throw(ArgumentError("auto_ivf must be a boolean"))
    if auto_ivf && store.count >= 16
        build_ivf!(store)
    end
    return Dict{String,Any}(
        "status" => "upserted",
        "upserted_count" => upserted,
        "store" => _store_stats(store),
    )
end

function _action_build_ivf(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "n_list", "n_probe", "iters", "seed"),
        "build_ivf accepts only store, n_list, n_probe, iters, and seed")
    store = _get_store(args)
    n_list = _require_int_range(get(args, "n_list", 0), "n_list", 0, 4096)
    n_probe = _require_int_range(get(args, "n_probe", 0), "n_probe", 0, 4096)
    iters = _require_int_range(get(args, "iters", 3), "iters", 1, 20)
    seed = get(args, "seed", 42)
    seed isa Integer && !(seed isa Bool) || throw(ArgumentError("seed must be an integer"))

    t0 = time_ns()
    build_ivf!(store; n_list=n_list, n_probe=n_probe, iters=iters, seed=Int(seed))
    elapsed_ns = Int(time_ns() - t0)
    return Dict{String,Any}(
        "status" => "built",
        "elapsed_ns" => elapsed_ns,
        "store" => _store_stats(store),
    )
end

function _action_search(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "query_vector", "query_text", "query", "k", "mode", "n_probe", "filter"),
        "search accepts only store, query_vector, query_text (or query), k, mode, n_probe, and filter")
    store = _get_store(args)
    k = _require_int_range(get(args, "k", 10), "k", 1, _MAX_K)
    mode = get(args, "mode", "auto")
    mode isa String && mode in ("auto", "ivf", "exact") ||
        throw(ArgumentError("mode must be 'auto', 'ivf', or 'exact'"))
    n_probe = _require_int_range(get(args, "n_probe", 0), "n_probe", 0, 4096)

    has_vec = haskey(args, "query_vector")
    has_text = haskey(args, "query_text") || haskey(args, "query")
    has_filter = haskey(args, "filter")
    !(has_vec && has_text) ||
        throw(ArgumentError("search cannot specify both 'query_vector' and 'query_text'"))
    (has_vec || has_text || has_filter) ||
        throw(ArgumentError("search requires 'query_vector', 'query_text', or 'filter'"))

    allow_mask = nothing
    filter_matches = store.count
    if has_filter
        filt = args["filter"]
        filt isa AbstractDict || throw(ArgumentError("filter must be an object"))
        filter_matches = compute_filter_mask!(store, filt)
        allow_mask = store.filter_mask
    end

    # Pure structural / duck-type filter mode (when no query_vector or query_text is supplied)
    if !has_vec && !has_text
        t0 = time_ns()
        hits = Vector{Dict{String,Any}}()
        rank = 0
        @inbounds for idx in 1:store.count
            if allow_mask[idx]
                rank += 1
                push!(hits, Dict{String,Any}(
                    "rank" => rank,
                    "id" => store.ids[idx],
                    "score" => 1.0,
                    "metadata" => store.metadata[idx],
                ))
                rank >= k && break
            end
        end
        elapsed_ns = Int(time_ns() - t0)
        return Dict{String,Any}(
            "status" => "searched",
            "store" => store.name,
            "mode_used" => "duck_filter_only",
            "filter_matches" => filter_matches,
            "candidates_scored" => filter_matches,
            "total_items" => store.count,
            "elapsed_ns" => elapsed_ns,
            "hits" => hits,
        )
    end

    if has_vec
        q_vec = _parse_float32_vec(args["query_vector"], store.dim)
        _normalize_copy!(store.q_norm, q_vec)
    else
        q_text = haskey(args, "query_text") ? args["query_text"] : args["query"]
        q_text isa String && !isempty(q_text) && ncodeunits(q_text) <= _MAX_TEXT_BYTES ||
            throw(ArgumentError("query_text must be a non-empty string of at most $_MAX_TEXT_BYTES bytes"))
        embed_text!(store.q_norm, q_text)
    end

    use_ivf = if mode == "ivf"
        store.ivf.valid || build_ivf!(store)
        true
    elseif mode == "exact"
        false
    else
        store.ivf.valid && store.count >= _IVF_AUTO_MIN_ITEMS
    end

    actual_k = min(k, store.count)
    t0 = time_ns()
    scored = if actual_k == 0 || filter_matches == 0
        @inbounds for i in 1:actual_k
            store.top_idx_buf[i] = 0
            store.top_val_buf[i] = -Inf32
        end
        0
    elseif use_ivf
        search_ivf!(
            store.top_idx_buf, store.top_val_buf, store, store.q_norm, actual_k;
            n_probe=n_probe, normalize_query=false, allow_mask=allow_mask,
        )
    else
        search_exact!(
            store.top_idx_buf, store.top_val_buf, store, store.q_norm, actual_k;
            normalize_query=false, allow_mask=allow_mask,
        )
    end
    elapsed_ns = Int(time_ns() - t0)

    hits = Vector{Dict{String,Any}}()
    for rank in 1:actual_k
        idx = store.top_idx_buf[rank]
        idx <= 0 && break
        push!(hits, Dict{String,Any}(
            "rank" => rank,
            "id" => store.ids[idx],
            "score" => Float64(store.top_val_buf[rank]),
            "metadata" => store.metadata[idx],
        ))
    end

    return Dict{String,Any}(
        "status" => "searched",
        "store" => store.name,
        "mode_used" => use_ivf ? "ivf_reg4" : "exact_blas",
        "filter_matches" => filter_matches,
        "candidates_scored" => scored,
        "total_items" => store.count,
        "elapsed_ns" => elapsed_ns,
        "hits" => hits,
    )
end

function _action_index_codegraph(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "dim", "build_ivf", "root", "include_polyglot"),
        "index_codegraph accepts only store, dim, build_ivf, root, and include_polyglot")
    isdefined(@__MODULE__, :Palette) && isdefined(Palette, :codegraph) ||
        throw(ArgumentError("Palette.codegraph is not available in this module scope"))
    name = _validate_store_name(args)
    dim = _require_int_range(get(args, "dim", _DEFAULT_DIM), "dim", _MIN_DIM, _MAX_DIM)
    do_ivf = get(args, "build_ivf", true)
    do_ivf isa Bool || throw(ArgumentError("build_ivf must be a boolean"))
    inc_poly = get(args, "include_polyglot", true)
    inc_poly isa Bool || throw(ArgumentError("include_polyglot must be a boolean"))
    root = get(args, "root", nothing)
    root === nothing || (root isa String && !isempty(root)) ||
        throw(ArgumentError("root must be a non-empty string"))

    _ensure_store_slot!(name)
    cg = root === nothing ? Palette.codegraph() : Palette.codegraph(root)
    est_cap = sum((length(e.definitions) for e in values(cg.files)); init=0)
    cap = clamp(max(est_cap, 32), 32, _MAX_ITEMS)
    store = VecStore(name; dim=dim, capacity=cap)
    _STORES[name] = store
    indexed = index_codegraph!(store, cg; include_polyglot=inc_poly)
    if do_ivf && store.count >= 16
        build_ivf!(store)
    end
    return Dict{String,Any}(
        "status" => "indexed_codegraph",
        "indexed_symbols" => indexed,
        "store" => _store_stats(store),
    )
end

function _action_delete(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "id"), "delete accepts only store and id")
    store = _get_store(args)
    id = get(args, "id", nothing)
    id isa String && !isempty(id) || throw(ArgumentError("id must be a non-empty string"))
    removed = delete_item!(store, id)
    return Dict{String,Any}("status" => removed ? "deleted" : "not_found", "id" => id, "count" => store.count)
end

function _action_drop(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store"), "drop accepts only store")
    name = _validate_store_name(args)
    existed = haskey(_STORES, name)
    delete!(_STORES, name)
    return Dict{String,Any}("status" => existed ? "dropped" : "not_found", "store" => name)
end

"""
    vecindex(arguments::Dict{String,Any}) -> Dict{String,Any}

Palette runtime capability entrypoint for `vecindex`.
Supported actions: `diagnose`, `create`, `upsert`, `build_ivf`, `search` (or `query`),
`index_codegraph`, `delete`, `drop`, `stats`.
"""
function vecindex(arguments::Dict{String,Any})
    lock(_STORES_LOCK) do
        action = get(arguments, "action", "diagnose")
        action isa String || throw(ArgumentError("action must be a string"))
        if action == "diagnose"
            _check_allowed_keys(arguments, ("action",), "diagnose accepts only action")
            return _diagnose()
        elseif action == "create"
            return _action_create(arguments)
        elseif action == "upsert"
            return _action_upsert(arguments)
        elseif action == "build_ivf"
            return _action_build_ivf(arguments)
        elseif action == "search" || action == "query"
            return _action_search(arguments)
        elseif action == "index_codegraph"
            return _action_index_codegraph(arguments)
        elseif action == "delete"
            return _action_delete(arguments)
        elseif action == "drop"
            return _action_drop(arguments)
        elseif action == "stats"
            _check_allowed_keys(arguments, ("action", "store"), "stats accepts only action and store")
            store = _get_store(arguments)
            return Dict{String,Any}("status" => "ok", "store" => _store_stats(store))
        end
        throw(ArgumentError("action must be diagnose, create, upsert, build_ivf, search (or query), index_codegraph, delete, drop, or stats"))
    end
end

vecindex(arguments::AbstractDict) = vecindex(Dict{String,Any}(String(k) => v for (k, v) in arguments))
