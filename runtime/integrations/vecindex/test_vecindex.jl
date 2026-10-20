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

    @testset "6. Filtered Duck-Type & Polyglot Subspace Search" begin
        d = 128
        cg_dict = Dict{String,Any}(
            "symbols" => [
                Dict{String,Any}(
                    "name" => "_generate",
                    "kind" => "function",
                    "language" => "julia",
                    "file" => "gesso.jl",
                    "line" => 183,
                    "fields" => ["mgr", "tokenizer", "context_length"],
                    "callees" => ["encode", "decode", "kv_footprint"],
                    "params" => ["args", "Dict"],
                    "arity" => 1,
                    "mutating" => false,
                ),
                Dict{String,Any}(
                    "name" => "snapshot_state!",
                    "kind" => "function",
                    "language" => "julia",
                    "file" => "revival.jl",
                    "line" => 733,
                    "fields" => ["manifest", "blobs"],
                    "callees" => ["sha256", "bytes2hex", "serialize"],
                    "params" => ["state", "KernelState"],
                    "arity" => 1,
                    "mutating" => true,
                ),
                Dict{String,Any}(
                    "name" => "HeldSnapshot",
                    "kind" => "struct",
                    "language" => "rust",
                    "file" => "sandbox.rs",
                    "line" => 824,
                    "fields" => ["dir", "lock_fd"],
                    "callees" => String[],
                    "params" => ["PathBuf", "RawFd"],
                    "arity" => 0,
                    "mutating" => false,
                ),
            ],
        )
        empty!(VI._STORES)
        st = VI.VecStore("duck_test"; dim=d, capacity=16)
        VI._STORES["duck_test"] = st
        items = [Dict{String,Any}("id" => "$(sym["file"]):$(sym["line"]):$(sym["name"])",
                                  "text" => "$(sym["name"]) $(sym["kind"]) $(sym["language"]) $(sym["file"])",
                                  "metadata" => sym) for sym in cg_dict["symbols"]]
        @test VI.vecindex(Dict{String,Any}("action" => "upsert", "store" => "duck_test", "items" => items))["upserted_count"] == 3

        # 1. Pure duck-type field filter (no query_text)
        r1 = VI.vecindex(Dict{String,Any}(
            "action" => "search",
            "store" => "duck_test",
            "filter" => Dict{String,Any}("has_fields_all" => ["mgr", "tokenizer"]),
        ))
        @test r1["mode_used"] == "duck_filter_only"
        @test r1["filter_matches"] == 1
        @test r1["hits"][1]["id"] == "gesso.jl:183:_generate"

        # 2. Combined vector + duck-type callee & mutating filter
        r2 = VI.vecindex(Dict{String,Any}(
            "action" => "search",
            "store" => "duck_test",
            "query_text" => "state serialization",
            "filter" => Dict{String,Any}(
                "language" => "julia",
                "mutating" => true,
                "has_callees_all" => ["sha256", "bytes2hex"],
            ),
        ))
        @test r2["filter_matches"] == 1
        @test r2["hits"][1]["id"] == "revival.jl:733:snapshot_state!"

        # 3. Polyglot scanner test (TypeScript & Go definitions + duck traits)
        ts_lines = [
            "export interface VectorConfig {",
            "  dim: number;",
            "  metric: string;",
            "}",
            "export function searchIndex(store: VecStore, query: Float32Array) {",
            "  const n = store.count;",
            "  return normalizeVec(query, store.embeddings);",
            "}",
        ]
        ts_defs = VI._scan_polyglot_defs(ts_lines, "typescript")
        @test length(ts_defs) == 2
        @test ts_defs[1].name == "VectorConfig" && ts_defs[1].kind == "interface"
        @test ts_defs[2].name == "searchIndex" && ts_defs[2].kind == "function"
        fields, callees, params, arity = VI._extract_line_duck_traits(ts_lines, 5, 8)
        @test "count" in fields && "embeddings" in fields
        @test "normalizeVec" in callees
        @test arity == 2

        # 4. C# (.cs) multi-modifier class, inheritance, property, and virtual/override method extraction
        cs_lines = [
            "public partial class GamePlayer : GameLiving, IGamePlayer {",
            "    protected readonly GameClient m_client;",
            "    public virtual int Mana { get; set; }",
            "    public override bool StartAttack(GameObject attackTarget) {",
            "        if (m_client.Out != null) { m_client.Out.SendAttackMode(AttackState); }",
            "        return base.StartAttack(attackTarget);",
            "    }",
            "}",
        ]
        cs_defs = VI._scan_polyglot_defs(cs_lines, "csharp")
        @test length(cs_defs) == 2
        @test cs_defs[1].name == "GamePlayer" && cs_defs[1].kind == "class"
        @test cs_defs[2].name == "StartAttack" && cs_defs[2].kind == "function"
        cs_cls_fields, _, cs_cls_params, _ = VI._extract_line_duck_traits(cs_lines, 1, 3)
        @test "m_client" in cs_cls_fields && "Mana" in cs_cls_fields
        @test "GameLiving" in cs_cls_params && "IGamePlayer" in cs_cls_params && "GameClient" in cs_cls_params
        cs_fn_fields, cs_fn_callees, cs_fn_params, cs_fn_arity = VI._extract_line_duck_traits(cs_lines, 4, 8)
        @test "Out" in cs_fn_fields
        @test "SendAttackMode" in cs_fn_callees
        @test "GameObject" in cs_fn_params && "attackTarget" in cs_fn_params
        @test cs_fn_arity == 1

        # 5. UTF-8 signature slicing (multi-byte Unicode identifiers like α, β) & Python top-level <module> + imports
        jl_utf8_lines = [
            "function _lava_scale!(dst::Vector{Float32}, α)",
            "    return _lava_sync!(dst)",
            "end",
        ]
        _, jl_utf8_callees, jl_utf8_params, jl_utf8_arity = VI._extract_line_duck_traits(jl_utf8_lines, 1, 3)
        @test jl_utf8_arity == 2
        @test "dst" in jl_utf8_params && "Vector" in jl_utf8_params
        @test "_lava_sync!" in jl_utf8_callees

        py_script_lines = [
            "from transformers import AutoModelForCausalLM, LlamaConfig",
            "model = AutoModelForCausalLM.from_pretrained('meta-llama/Llama-3')",
            "out = model.generate(input_ids)",
        ]
        py_defs = VI._scan_polyglot_defs(py_script_lines, "python")
        @test length(py_defs) == 1 && py_defs[1].name == "<module>" && py_defs[1].kind == "module"
        py_fields, py_callees, py_params, _ = VI._extract_line_duck_traits(py_script_lines, 1, 3)
        @test "transformers" in py_params
        @test "AutoModelForCausalLM" in py_callees && "LlamaConfig" in py_callees && "generate" in py_callees
        @test "from_pretrained" in py_fields

        # 6. Ergonomic 'query' action & 'query' parameter aliases
        r_alias = VI.vecindex(Dict("action" => "query", "store" => "duck_test", "query" => "snapshot_state", "k" => 1))
        @test r_alias["status"] == "searched"
        @test r_alias["hits"][1]["id"] == "revival.jl:733:snapshot_state!"

        VI.vecindex(Dict{String,Any}("action" => "drop", "store" => "duck_test"))
    end

    @testset "7. .gitignore-aware codegraph indexing & auto-mode threshold" begin
        @test VI._IVF_AUTO_MIN_ITEMS == 50_000
        if Sys.which("git") !== nothing
            mktempdir() do root
                run(pipeline(`git -C $root init -q`; stdout=devnull, stderr=devnull))
                write(joinpath(root, ".gitignore"), "research/\n")
                mkpath(joinpath(root, "research"))
                write(joinpath(root, "keep.jl"), "f(x) = x\n")
                write(joinpath(root, "research", "scratch.jl"), "g(x) = x\n")
                allowed = VI._git_nonignored_files(root)
                @test allowed !== nothing
                @test "keep.jl" in allowed
                @test !("research/scratch.jl" in allowed)

                def_t = NamedTuple{(:name, :kind, :line),Tuple{String,Symbol,Int}}
                call_t = NamedTuple{(:caller, :callee, :line),Tuple{String,String,Int}}
                mk(p, n) = (path=p, language=:julia, definitions=def_t[(name=n, kind=:function, line=1)], calls=call_t[])
                cg = (root=root, files=Dict("keep.jl" => mk("keep.jl", "f"),
                                            "research/scratch.jl" => mk("research/scratch.jl", "g")))
                st = VI.VecStore("gi"; dim=64, capacity=8)
                VI.index_codegraph!(st, cg; include_polyglot=false)
                ids = st.ids[1:st.count]
                @test any(id -> startswith(id, "keep.jl:"), ids)
                @test !any(id -> startswith(id, "research/"), ids)
            end
        end
    end
end

