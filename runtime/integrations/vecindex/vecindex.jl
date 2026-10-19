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
const _IVF_AUTO_MIN_ITEMS = 1024

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
    actual_n_probe = n_probe > 0 ? min(n_probe, actual_n_list) : clamp(div(actual_n_list + 7, 8), 2, actual_n_list)

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

function select_topk_max!(top_idx::AbstractVector{Int}, top_val::AbstractVector{Float32},
                          scores::AbstractVector{Float32}, k::Int)
    1 <= k <= min(length(top_idx), length(top_val)) ||
        throw(BoundsError("top_idx and top_val must have length >= k ($k)"))
    n = length(scores)
    actual_k = min(k, n)
    @inbounds for i in 1:k
        top_val[i] = -Inf32
        top_idx[i] = 0
    end
    actual_k == 0 && return top_idx
    @inbounds for j in 1:n
        _insert_topk!(top_idx, top_val, actual_k, j, scores[j])
    end
    return top_idx
end

"""
    _scan_columns_reg4!(top_idx, top_val, k, mat, q_norm, start_col, end_col, id_map)

4-column AVX2 FMA (`muladd`) register-blocked cosine similarity kernel.
Loads `q_norm[i]` once per 4 database columns and fuses top-k insertion inline
with **0 bytes allocated**. If `isempty(id_map)`, uses column index `p` directly.
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
        if s1 > top_val[k]
            _insert_topk!(top_idx, top_val, k, use_map ? id_map[p] : p, s1)
        end
        if s2 > top_val[k]
            _insert_topk!(top_idx, top_val, k, use_map ? id_map[p + 1] : (p + 1), s2)
        end
        if s3 > top_val[k]
            _insert_topk!(top_idx, top_val, k, use_map ? id_map[p + 2] : (p + 2), s3)
        end
        if s4 > top_val[k]
            _insert_topk!(top_idx, top_val, k, use_map ? id_map[p + 3] : (p + 3), s4)
        end
        p += 4
    end
    @inbounds while p <= end_col
        s = 0.0f0
        @simd for i in 1:d
            s = muladd(mat[i, p], q_norm[i], s)
        end
        if s > top_val[k]
            _insert_topk!(top_idx, top_val, k, use_map ? id_map[p] : p, s)
        end
        p += 1
    end
    return nothing
end

"""
    search_exact!(top_idx::Vector{Int}, top_val::Vector{Float32},
                  store::VecStore, query::AbstractVector{Float32}, k::Int;
                  normalize_query::Bool=true) -> Int

Zero-allocation exact cosine search using OpenBLAS `gemv!('T', ...)` and in-place
top-k insertion. Returns the number of candidates scored (`store.count`).
"""
function search_exact!(
    top_idx::Vector{Int},
    top_val::Vector{Float32},
    store::VecStore,
    query::AbstractVector{Float32},
    k::Int;
    normalize_query::Bool=true,
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
    select_topk_max!(top_idx, top_val, scores_view, actual_k)
    return N
end

const _EMPTY_INT_VEC = Int[]

"""
    search_ivf!(top_idx::Vector{Int}, top_val::Vector{Float32},
                store::VecStore, query::AbstractVector{Float32}, k::Int;
                n_probe::Int=0, normalize_query::Bool=true) -> Int

Zero-allocation Contiguous-Cluster IVF search with 4-column AVX2 FMA register blocking.
Also scans any unindexed incremental tail (`ivf.indexed_count + 1 : store.count`)
so newly upserted items are found with 100% recall before an IVF rebuild.
Returns the number of candidate vectors scored.
"""
function search_ivf!(
    top_idx::Vector{Int},
    top_val::Vector{Float32},
    store::VecStore,
    query::AbstractVector{Float32},
    k::Int;
    n_probe::Int=0,
    normalize_query::Bool=true,
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
        return search_exact!(top_idx, top_val, store, query, k; normalize_query=normalize_query)
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
        )
        scored += (N - ivf.indexed_count)
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
# Pharos Codegraph Integration
# ---------------------------------------------------------------------------

function _embed_symbol_with_callees!(
    primary_buf::Vector{Float32},
    callee_buf::Vector{Float32},
    primary_text::AbstractString,
    calls_str::AbstractString,
)
    embed_text!(primary_buf, primary_text)
    if !isempty(calls_str)
        embed_text!(callee_buf, calls_str)
        d = length(primary_buf)
        @inbounds @simd for i in 1:d
            primary_buf[i] = muladd(0.85f0, primary_buf[i], 0.15f0 * callee_buf[i])
        end
    end
    return primary_buf
end

"""
    index_codegraph!(store::VecStore, cg) -> Int

Index symbols from `Palette.codegraph()` (either a `Pharos.CodeGraph` struct or
a JSON-like `AbstractDict`) into `store` using `embed_text!`.
Primary symbol identity (name, kind, file) receives 85% normalized energy and
call-graph neighborhood (`callees`) receives 15% normalized energy so functions
with large call fan-out are never diluted by length normalization.
"""
function index_codegraph!(store::VecStore, cg::AbstractDict)::Int
    symbols = get(cg, "symbols", Any[])
    symbols isa AbstractVector || throw(ArgumentError("codegraph dict has no symbols array"))
    primary_buf = zeros(Float32, store.dim)
    callee_buf = zeros(Float32, store.dim)
    indexed = 0
    for sym in symbols
        sym isa AbstractDict || continue
        name = String(get(sym, "name", ""))
        isempty(name) && continue
        kind = String(get(sym, "kind", "symbol"))
        file = String(get(sym, "file", ""))
        line = get(sym, "line", 0)
        sig = String(get(sym, "signature", name))
        calls = get(sym, "calls", Any[])
        calls_str = calls isa AbstractVector ? join(String.(calls), " ") : ""
        id = isempty(file) ? name : "$(file):$(line):$(name)"
        primary_text = "$(name) $(name) $(name) $(kind) $(sig) $(file)"
        _embed_symbol_with_callees!(primary_buf, callee_buf, primary_text, calls_str)
        meta = Dict{String,Any}(
            "name" => name,
            "kind" => kind,
            "file" => file,
            "line" => line,
            "signature" => sig,
        )
        upsert!(store, id, primary_buf; metadata=meta)
        indexed += 1
    end
    return indexed
end

function index_codegraph!(store::VecStore, cg)::Int
    hasproperty(cg, :files) || throw(ArgumentError("expected a Pharos.CodeGraph or AbstractDict"))
    primary_buf = zeros(Float32, store.dim)
    callee_buf = zeros(Float32, store.dim)
    indexed = 0
    for path in sort!(collect(keys(cg.files)))
        entry = cg.files[path]
        lang = string(entry.language)
        callees_by_caller = Dict{String,Vector{String}}()
        for c in entry.calls
            push!(get!(callees_by_caller, c.caller, String[]), c.callee)
        end
        for def in entry.definitions
            name = def.name
            isempty(name) && continue
            kind = string(def.kind)
            line = def.line
            callees = unique(get(callees_by_caller, name, String[]))
            calls_str = isempty(callees) ? "" : join(callees, " ")
            id = "$(path):$(line):$(name)"
            primary_text = "$(name) $(name) $(name) $(kind) $(lang) $(path)"
            _embed_symbol_with_callees!(primary_buf, callee_buf, primary_text, calls_str)
            meta = Dict{String,Any}(
                "name" => name,
                "kind" => kind,
                "language" => lang,
                "file" => path,
                "line" => line,
                "callees" => callees,
            )
            upsert!(store, id, primary_buf; metadata=meta)
            indexed += 1
        end
    end
    return indexed
end

# ---------------------------------------------------------------------------
# Runtime Capability ABI Entrypoint (`Palette.runtime("invoke", ...)`)
# ---------------------------------------------------------------------------

function _store_memory_bytes(store::VecStore)::Int
    base = sizeof(store.embeddings) + sizeof(store.q_norm) + sizeof(store.scores_buf) +
           sizeof(store.top_idx_buf) + sizeof(store.top_val_buf)
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
    _check_allowed_keys(args, ("action", "store", "query_vector", "query_text", "k", "mode", "n_probe"),
        "search accepts only store, query_vector, query_text, k, mode, and n_probe")
    store = _get_store(args)
    k = _require_int_range(get(args, "k", 10), "k", 1, _MAX_K)
    mode = get(args, "mode", "auto")
    mode isa String && mode in ("auto", "ivf", "exact") ||
        throw(ArgumentError("mode must be 'auto', 'ivf', or 'exact'"))
    n_probe = _require_int_range(get(args, "n_probe", 0), "n_probe", 0, 4096)

    has_vec = haskey(args, "query_vector")
    has_text = haskey(args, "query_text")
    (has_vec ⊻ has_text) ||
        throw(ArgumentError("search requires exactly one of 'query_vector' or 'query_text'"))

    if has_vec
        q_vec = _parse_float32_vec(args["query_vector"], store.dim)
        _normalize_copy!(store.q_norm, q_vec)
    else
        q_text = args["query_text"]
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
    scored = if actual_k == 0
        0
    elseif use_ivf
        search_ivf!(
            store.top_idx_buf, store.top_val_buf, store, store.q_norm, actual_k;
            n_probe=n_probe, normalize_query=false,
        )
    else
        search_exact!(
            store.top_idx_buf, store.top_val_buf, store, store.q_norm, actual_k;
            normalize_query=false,
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
        "candidates_scored" => scored,
        "total_items" => store.count,
        "elapsed_ns" => elapsed_ns,
        "hits" => hits,
    )
end

function _action_index_codegraph(args::Dict{String,Any})
    _check_allowed_keys(args, ("action", "store", "dim", "build_ivf", "root"),
        "index_codegraph accepts only store, dim, build_ivf, and root")
    isdefined(@__MODULE__, :Palette) && isdefined(Palette, :codegraph) ||
        throw(ArgumentError("Palette.codegraph is not available in this module scope"))
    name = _validate_store_name(args)
    dim = _require_int_range(get(args, "dim", _DEFAULT_DIM), "dim", _MIN_DIM, _MAX_DIM)
    do_ivf = get(args, "build_ivf", true)
    do_ivf isa Bool || throw(ArgumentError("build_ivf must be a boolean"))
    root = get(args, "root", nothing)
    root === nothing || (root isa String && !isempty(root)) ||
        throw(ArgumentError("root must be a non-empty string"))

    _ensure_store_slot!(name)
    cg = root === nothing ? Palette.codegraph() : Palette.codegraph(root)
    est_cap = if cg isa AbstractDict
        length(get(cg, "symbols", Any[]))
    elseif hasproperty(cg, :files)
        sum((length(e.definitions) for e in values(cg.files)); init=0)
    else
        32
    end
    cap = clamp(max(est_cap, 32), 32, _MAX_ITEMS)
    store = VecStore(name; dim=dim, capacity=cap)
    _STORES[name] = store
    indexed = index_codegraph!(store, cg)
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
Supported actions: `diagnose`, `create`, `upsert`, `build_ivf`, `search`,
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
        elseif action == "search"
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
        throw(ArgumentError("action must be diagnose, create, upsert, build_ivf, search, index_codegraph, delete, drop, or stats"))
    end
end
