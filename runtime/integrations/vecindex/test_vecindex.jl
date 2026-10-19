using Test
using LinearAlgebra
using Random

module VecIndexTestMod
    include("vecindex.jl")
end

const VI = VecIndexTestMod

function _naive_oracle(mat::Matrix{Float32}, q::Vector{Float32}, k::Int)
    N = size(mat, 2)
    qn = q ./ norm(q)
    scores = [dot(view(mat, :, j), qn) for j in 1:N]
    perm = partialsortperm(scores, 1:min(k, N); rev=true)
    return collect(perm), scores[perm]
end

function _alloc_gate_exact(store::VI.VecStore, q::Vector{Float32}, k::Int)
    VI.search_exact!(store.top_idx_buf, store.top_val_buf, store, q, k; normalize_query=false)
    return @allocated VI.search_exact!(store.top_idx_buf, store.top_val_buf, store, q, k; normalize_query=false)
end

function _alloc_gate_ivf(store::VI.VecStore, q::Vector{Float32}, k::Int, n_probe::Int)
    VI.search_ivf!(store.top_idx_buf, store.top_val_buf, store, q, k; n_probe=n_probe, normalize_query=false)
    return @allocated VI.search_ivf!(store.top_idx_buf, store.top_val_buf, store, q, k; n_probe=n_probe, normalize_query=false)
end

function _alloc_gate_embed(buf::Vector{Float32}, text::String)
    VI.embed_text!(buf, text)
    return @allocated VI.embed_text!(buf, text)
end

@testset "VecIndex RPDO + PEA Verification Suite" begin
    @testset "1. Oracle Equivalence & Contiguous-IVF Recall" begin
        rng = MersenneTwister(20261019)
        d = 128
        n_clusters = 16
        per_cluster = 64
        N = n_clusters * per_cluster
        store = VI.VecStore("oracle_test"; dim=d, capacity=N)

        centers = randn(rng, Float32, d, n_clusters)
        for c in 1:n_clusters
            centers[:, c] ./= norm(view(centers, :, c))
            for m in 1:per_cluster
                idx = (c - 1) * per_cluster + m
                v = centers[:, c] .+ 0.12f0 .* randn(rng, Float32, d)
                VI.upsert!(store, "doc_$idx", v; metadata=Dict{String,Any}("cluster" => c))
            end
        end

        q = centers[:, 5] .+ 0.08f0 .* randn(rng, Float32, d)
        q ./= norm(q)
        oracle_idx, oracle_val = _naive_oracle(store.embeddings[:, 1:N], q, 10)

        # Exact BLAS must match naive oracle 100%
        scored_exact = VI.search_exact!(store.top_idx_buf, store.top_val_buf, store, q, 10)
        @test scored_exact == N
        @test store.top_idx_buf[1:10] == oracle_idx
        @test isapprox(store.top_val_buf[1:10], oracle_val; atol=1f-5)

        # Contiguous-Cluster IVF with 4-way AVX2 FMA register blocking
        VI.build_ivf!(store; n_list=n_clusters, n_probe=4, iters=4, seed=42)
        @test store.ivf.valid
        scored_ivf = VI.search_ivf!(store.top_idx_buf, store.top_val_buf, store, q, 10; n_probe=4)
        @test scored_ivf < N
        recall = length(intersect(Set(store.top_idx_buf[1:10]), Set(oracle_idx))) / 10.0
        @test recall == 1.0

        # Full-probe IVF must match exact oracle 100% on arbitrary queries
        q_rand = randn(rng, Float32, d)
        o_idx2, o_val2 = _naive_oracle(store.embeddings[:, 1:N], q_rand, 10)
        VI.search_ivf!(store.top_idx_buf, store.top_val_buf, store, q_rand, 10; n_probe=n_clusters)
        @test store.top_idx_buf[1:10] == o_idx2
        @test isapprox(store.top_val_buf[1:10], o_val2; atol=1f-5)
    end

    @testset "2. Zero-Allocation Hot Paths (@allocated == 0)" begin
        rng = MersenneTwister(77)
        d = 256
        store = VI.VecStore("alloc_test"; dim=d, capacity=256)
        for i in 1:128
            VI.upsert!(store, "item_$i", randn(rng, Float32, d))
        end
        VI.build_ivf!(store; n_list=8, n_probe=3)
        q = zeros(Float32, d)

        @test _alloc_gate_embed(q, "snapshot_state! in revival.jl serializes world bindings") == 0
        @test _alloc_gate_exact(store, q, 10) == 0
        @test _alloc_gate_ivf(store, q, 10, 3) == 0

        # Append unindexed tail items (store.count > store.ivf.indexed_count) and verify 0 allocations
        VI.upsert!(store, "tail_1", randn(rng, Float32, d))
        VI.upsert!(store, "tail_2", randn(rng, Float32, d))
        @test store.ivf.valid
        @test store.count == store.ivf.indexed_count + 2
        @test _alloc_gate_ivf(store, q, 10, 3) == 0
    end

    @testset "3. Incremental Tail Recall, Exception Safety, & Delete" begin
        rng = MersenneTwister(99)
        d = 64
        store = VI.VecStore("lifecycle_test"; dim=d, capacity=32)
        for i in 1:32
            VI.upsert!(store, "k_$i", randn(rng, Float32, d))
        end
        VI.build_ivf!(store; n_list=4, n_probe=2)
        @test store.ivf.valid

        # Append a target vector in the unindexed tail and verify IVF finds it at rank 1
        target = randn(rng, Float32, d)
        target ./= norm(target)
        VI.upsert!(store, "tail_target", target)
        @test store.ivf.valid
        VI.search_ivf!(store.top_idx_buf, store.top_val_buf, store, target, 5; n_probe=1)
        @test store.ids[store.top_idx_buf[1]] == "tail_target"
        @test isapprox(store.top_val_buf[1], 1.0f0; atol=1f-5)

        # Exception safety: invalid overwrite must NOT clobber existing vector or invalidate IVF
        orig_vec = copy(store.embeddings[:, 1])
        bad_nan = fill(Float32(NaN), d)
        bad_zero = zeros(Float32, d)
        @test_throws ArgumentError VI.upsert!(store, "k_1", bad_nan)
        @test_throws ArgumentError VI.upsert!(store, "k_1", bad_zero)
        @test_throws DimensionMismatch VI.upsert!(store, "k_1", ones(Float32, d + 1))
        @test store.embeddings[:, 1] == orig_vec
        @test store.ivf.valid

        # Valid overwrite of an already-IVF-indexed item marks ivf.valid = false
        VI.upsert!(store, "k_1", target)
        @test !store.ivf.valid

        # Delete item swaps with tail and updates id_to_idx
        count_before = store.count
        @test VI.delete_item!(store, "k_2")
        @test !VI.delete_item!(store, "k_2")
        @test store.count == count_before - 1
        @test !haskey(store.id_to_idx, "k_2")
    end

    @testset "4. Feature Hashing & Pharos Codegraph Indexing" begin
        d = 256
        v_mut = zeros(Float32, d)
        v_plain = zeros(Float32, d)
        v_camel = zeros(Float32, d)
        v_unrel = zeros(Float32, d)

        VI.embed_text!(v_mut, "snapshot_state!")
        VI.embed_text!(v_plain, "snapshot state")
        VI.embed_text!(v_camel, "snapshotState")
        VI.embed_text!(v_unrel, "quantum_fourier_transform")

        @test dot(v_mut, v_plain) > 0.5f0
        @test dot(v_mut, v_camel) > 0.5f0
        @test dot(v_mut, v_unrel) < 0.2f0

        # Test Pharos-shaped struct indexing
        def_t = NamedTuple{(:name, :kind, :line),Tuple{String,Symbol,Int}}
        call_t = NamedTuple{(:caller, :callee, :line),Tuple{String,String,Int}}
        entry_t = NamedTuple{(:path, :language, :definitions, :calls),Tuple{String,Symbol,Vector{def_t},Vector{call_t}}}
        fake_cg = (
            root = "/workspace",
            files = Dict(
                "src/revival.jl" => (
                    path = "src/revival.jl",
                    language = :julia,
                    definitions = [
                        (name = "snapshot_state!", kind = :function, line = 733),
                        (name = "SLOW_SNAPSHOT_SECONDS", kind = :const, line = 656),
                    ],
                    calls = [
                        (caller = "snapshot_state!", callee = "serialize_binding", line = 740),
                        (caller = "snapshot_state!", callee = "write_manifest", line = 755),
                    ],
                ),
            ),
        )
        cg_store = VI.VecStore("cg"; dim=d, capacity=16)
        @test VI.index_codegraph!(cg_store, fake_cg) == 2
        q_buf = zeros(Float32, d)
        VI.embed_text!(q_buf, "snapshot_state! revival")
        VI.search_exact!(cg_store.top_idx_buf, cg_store.top_val_buf, cg_store, q_buf, 2; normalize_query=false)
        @test cg_store.ids[cg_store.top_idx_buf[1]] == "src/revival.jl:733:snapshot_state!"
    end

    @testset "5. Runtime Capability ABI & Fail-Closed Validation" begin
        empty!(VI._STORES)
        diag = VI.vecindex(Dict{String,Any}("action" => "diagnose"))
        @test diag["status"] == "ready"
        @test diag["store_count"] == 0

        res_c = VI.vecindex(Dict{String,Any}("action" => "create", "store" => "abi_test", "dim" => 32, "capacity" => 16))
        @test res_c["status"] == "created"

        res_u = VI.vecindex(Dict{String,Any}(
            "action" => "upsert",
            "store" => "abi_test",
            "items" => [
                Dict{String,Any}("id" => "a", "text" => "julia vector database ivf search", "metadata" => Dict("tag" => 1)),
                Dict{String,Any}("id" => "b", "text" => "python asyncio event loop socket", "metadata" => Dict("tag" => 2)),
            ],
        ))
        @test res_u["upserted_count"] == 2

        res_s = VI.vecindex(Dict{String,Any}(
            "action" => "search",
            "store" => "abi_test",
            "query_text" => "julia ivf vector search",
            "k" => 2,
        ))
        @test res_s["status"] == "searched"
        @test res_s["hits"][1]["id"] == "a"

        # Fail-closed validation checks
        @test_throws ArgumentError VI.vecindex(Dict{String,Any}("action" => "unknown"))
        @test_throws ArgumentError VI.vecindex(Dict{String,Any}("action" => "diagnose", "extra" => 1))
        @test_throws ArgumentError VI.vecindex(Dict{String,Any}("action" => "create", "store" => "../bad"))
        @test_throws ArgumentError VI.vecindex(Dict{String,Any}("action" => "create", "store" => "s", "dim" => true))
        @test_throws ArgumentError VI.vecindex(Dict{String,Any}("action" => "create", "store" => "s", "dim" => 4))
        @test_throws ArgumentError VI.vecindex(Dict{String,Any}("action" => "upsert", "store" => "abi_test", "id" => "x", "vector" => fill(NaN, 32)))

        res_d = VI.vecindex(Dict{String,Any}("action" => "drop", "store" => "abi_test"))
        @test res_d["status"] == "dropped"
    end
end
