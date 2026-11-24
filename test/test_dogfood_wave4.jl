using Test
using Palette

@testset "Palette Dogfood Wave 4 (Safety, Isolation, Ergonomics & Perf)" begin

    @testset "W4-1: Safe summarysize short-circuit & @ephemeral RAM-only binding guard" begin
        reset_kernel_state()

        # 1. safe_summarysize on ordinary values, cycles, deep graphs, and CodeGraph
        @test Palette.safe_summarysize(42) == sizeof(Int)
        @test Palette.safe_summarysize("hello") isa Int && Palette.safe_summarysize("hello") > 0
        @test Palette.safe_summarysize(zeros(Float64, 100)) >= 800

        # Cyclic array terminates cleanly without stack overflow
        cyc = Any[]
        push!(cyc, cyc)
        @test Palette.safe_summarysize(cyc) isa Int

        # Deep recursive linked list (depth 5000) short-circuits fast without stack overflow
        mutable struct W4DeepNode
            next::Union{Nothing, W4DeepNode}
            val::Int
        end
        head = W4DeepNode(nothing, 0)
        for i in 1:5000
            head = W4DeepNode(head, i)
        end
        t0 = time()
        @test Palette.safe_summarysize(head; max_depth=32) === nothing
        @test time() - t0 < 0.25

        # CodeGraph immediately short-circuits in safe_summarysize
        repo = mktempdir()
        write(joinpath(repo, "sample.jl"), "sample_fn(x) = x + 1\n")
        g = Pharos.CodeGraph(repo)
        @test Palette.safe_summarysize(g) === nothing

        # 2. @ephemeral, ephemeral!, unephemeral!, ephemerals & snapshot skip
        old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
        ws = mktempdir()
        st = mktempdir()
        Palette.WORKSPACE_ROOT[] = ws
        Palette.STATE_DIR[] = st
        try
            # Turn-level @ephemeral macro works directly without prefix AND via Api.@ephemeral
            r1 = Palette.execute(Palette.ExecuteCode("""
            @ephemeral big_weights = fill(Float32(3.25), 5_000_000)
            Api.@ephemeral pair_a, pair_b = (10, 20)
            kept_scalar = 42
            g_cache = Api.codegraph($(repr(repo)))
            mutable struct TurnChain
                next::Union{Nothing, TurnChain}
                id::Int
            end
            deep_chain = TurnChain(nothing, 0)
            for i in 1:400
                deep_chain = TurnChain(deep_chain, i)
            end
            length(big_weights)
            """))
            @test r1.result.success
            @test r1.result.data == 5_000_000

            # Ephemeral bindings stay resident in RAM across turns
            r2 = Palette.execute(Palette.ExecuteCode("big_weights[1] + big_weights[end] + pair_a + pair_b"))
            @test r2.result.success
            @test r2.result.data ≈ 36.5

            @test :big_weights in Api.ephemerals()
            @test :pair_a in Api.ephemerals()
            @test :pair_b in Api.ephemerals()
            @test !(:kept_scalar in Api.ephemerals())

            # varinfo() handles ephemeral, CodeGraph, and deep chains safely with zero stack overflow
            vi = Palette.execute(Palette.ExecuteCode("varinfo()")).result.data
            vi_map = Dict(row[1] => row[2] for row in vi.rows)
            @test vi_map["big_weights"] == "ephemeral"
            @test vi_map["g_cache"] == "?"
            @test vi_map["deep_chain"] == "?"
            @test vi_map["kept_scalar"] != "?"

            # Snapshot skips @ephemeral bindings completely (0 sidecar bytes, tiny manifest),
            # skips CodeGraph, and depth-bounds deep_chain with zero stack overflow!
            manifest = Palette.snapshot_state!(1)
            @test manifest !== nothing
            @test manifest["sidecar_bytes"] == 0
            @test manifest["bytes"] < 4096
            @test isempty(filter(f -> startswith(f, "sidecar-"), readdir(st)))

            b_map = Dict(b["name"] => b for b in manifest["bindings"])
            @test b_map["big_weights"]["class"] == "ephemeral"
            @test b_map["pair_a"]["class"] == "ephemeral"
            @test b_map["g_cache"]["class"] == "runtime"
            @test occursin("index cache", b_map["g_cache"]["reason"])
            @test b_map["deep_chain"]["class"] == "runtime"
            @test occursin("depth", b_map["deep_chain"]["reason"])
            @test b_map["kept_scalar"]["class"] == "data"

            # Revival restores kept_scalar cleanly; ephemeral bindings do NOT appear in `lost`!
            Palette.reset_kernel_state()
            report = Palette.revive_state!()
            @test occursin("kept_scalar", report)
            @test occursin("ephemeral (RAM-only", report)
            obs = Palette.Api.revival()
            @test !any(s -> startswith(s, "big_weights "), obs["lost"])
            mod = Palette.get_kernel_state().eval_module
            @test Base.invokelatest(getglobal, mod, :kept_scalar) == 42

            # Fast shallow byte-bound short-circuit before serialization on oversized dense array
            Palette.reset_kernel_state()
            Palette.execute(Palette.ExecuteCode("oversized_arr = zeros(UInt8, 200_000); small_ok = 9"))
            withenv("PALETTE_STATE_MAX_BINDING_BYTES" => "8192", "PALETTE_STATE_MAX_SIDECAR_BYTES" => "0") do
                m_pre = Palette.snapshot_state!(2)
                b_over = only(filter(b -> b["name"] == "oversized_arr", m_pre["bindings"]))
                @test b_over["class"] == "runtime"
                @test occursin("limit per binding", b_over["reason"])
            end

            # ephemeral! / unephemeral! API toggling
            @test Api.ephemeral!(:manual_ephem) === :manual_ephem
            @test :manual_ephem in Api.ephemerals()
            @test Api.unephemeral!(:manual_ephem) === :manual_ephem
            @test !(:manual_ephem in Api.ephemerals())
        finally
            Palette.reset_kernel_state()
            Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
            rm(ws; recursive=true, force=true)
            rm(st; recursive=true, force=true)
            rm(repo; recursive=true, force=true)
        end
    end

    @testset "W4-4: Module reload & sandbox isolation (isolate_module / reset_sandbox_module!)" begin
        reset_kernel_state()
        tmp = mktempdir()
        mod_file = joinpath(tmp, "iso_model.jl")
        flat_file = joinpath(tmp, "flat_helper.jl")
        try
            write(mod_file, """
            module IsoModelW4
                export ItemW4, compute_w4
                struct ItemW4
                    x::Int
                end
                compute_w4(it::ItemW4) = it.x * 2
            end
            """)
            write(flat_file, """
            struct FlatRecW4
                v::Int
            end
            flat_eval_w4(r::FlatRecW4) = r.v + 5
            """)

            # Keep cached data in the persistent session
            Palette.execute(Palette.ExecuteCode("cached_dataset = [10, 20, 30]"))

            m1 = Api.isolate_module(:IsoModelW4, mod_file)
            @test m1 isa Module
            @test :IsoModelW4 in Api.sandbox_modules()
            @test Base.invokelatest(Base.invokelatest(getglobal, m1, :compute_w4),
                                    Base.invokelatest(Base.invokelatest(getglobal, m1, :ItemW4), 7)) == 14

            # Must NOT pollute Core.Main or trigger export ambiguity in eval_module
            @test !Base.invokelatest(isdefined, Core.Main, :IsoModelW4)
            @test !Base.invokelatest(isdefined, Core.Main, :ItemW4)
            @test !Base.invokelatest(isdefined, Core.Main, :compute_w4)
            mod = Palette.get_kernel_state().eval_module
            @test !Base.invokelatest(isdefined, mod, :ItemW4)
            @test !Base.invokelatest(isdefined, mod, :compute_w4)

            # reset=false returns the cached isolated module handle
            @test Api.isolate_module(:IsoModelW4, mod_file; reset=false) === m1

            # Edit struct layout AND method in mod_file, reload cleanly with reset=true
            write(mod_file, """
            module IsoModelW4
                export ItemW4, compute_w4
                struct ItemW4
                    x::Int
                    y::Int
                end
                compute_w4(it::ItemW4) = it.x + it.y * 10
            end
            """)
            m2 = Api.isolate_module(:IsoModelW4, mod_file; reset=true)
            @test m2 !== m1
            @test Base.invokelatest(Base.invokelatest(getglobal, m2, :compute_w4),
                                    Base.invokelatest(Base.invokelatest(getglobal, m2, :ItemW4), 3, 4)) == 43
            # Persistent session dataset remains 100% untouched
            @test Palette.execute(Palette.ExecuteCode("sum(cached_dataset)")).result.data == 60
            # Still zero pollution in Core.Main even after execute_turn/sync_kernel_modules_to_main!
            Palette.execute(Palette.ExecuteCode("iso_handle = Api.isolate_module(:IsoModelW4, $(repr(mod_file)); reset=false)"))
            @test !Base.invokelatest(isdefined, Core.Main, :iso_handle)
            @test !Base.invokelatest(isdefined, Core.Main, :IsoModelW4)

            # reset_sandbox_module! clears registry entry cleanly
            @test Api.reset_sandbox_module!(:IsoModelW4) === true
            @test Api.reset_sandbox_module!(:IsoModelW4) === false
            @test !(:IsoModelW4 in Api.sandbox_modules())
            m3 = Api.isolate_module(:IsoModelW4, mod_file; reset=false)
            @test m3 !== m2

            # Works for flat .jl files (no module block) too, including struct layout changes
            f1 = Api.isolate_module(:FlatBoxW4, flat_file)
            @test Base.invokelatest(Base.invokelatest(getglobal, f1, :flat_eval_w4),
                                    Base.invokelatest(Base.invokelatest(getglobal, f1, :FlatRecW4), 10)) == 15
            write(flat_file, """
            struct FlatRecW4
                v::Int
                scale::Int
            end
            flat_eval_w4(r::FlatRecW4) = r.v * r.scale
            """)
            f2 = Api.isolate_module(:FlatBoxW4, flat_file; reset=true)
            @test f2 !== f1
            @test Base.invokelatest(Base.invokelatest(getglobal, f2, :flat_eval_w4),
                                    Base.invokelatest(Base.invokelatest(getglobal, f2, :FlatRecW4), 10, 4)) == 40
        finally
            Palette.reset_kernel_state()
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "W4-5: Pharos ergonomics (Api <-> Pharos parity, graph= kwarg, match modes)" begin
        reset_kernel_state()
        repo = mktempdir()
        try
            mkpath(joinpath(repo, "src"))
            write(joinpath(repo, "src", "solvers.jl"), """
            module Solvers
                solve_ode(x) = x + 1
                solve_sde(x) = solve_ode(x) + 2
                presolve_ode(x) = solve_sde(x)
                Solvers.solve_pde(x) = solve_ode(x) * 3
            end
            """)
            g = Pharos.CodeGraph(repo)

            # graph=g kwarg parity across Api and Palette (both String and Symbol names)
            exact_p = Pharos.definitions(g, "solve_ode")
            exact_a = Api.definitions("solve_ode"; graph=g)
            exact_sym = Api.definitions(:solve_ode; graph=g)
            @test exact_a == exact_p
            @test exact_sym == exact_p
            @test length(exact_a) == 1 && exact_a[1][2].name == "solve_ode"

            calls_p = Pharos.callers(g, "solve_ode")
            calls_a = Api.callers("solve_ode"; graph=g)
            calls_sym = Api.callers(:solve_ode; graph=g)
            @test calls_a == calls_p
            @test calls_sym == calls_p
            @test length(calls_a) == 2

            # Default match=:exact returns 0 for partial prefix "solve_"
            @test isempty(Pharos.definitions(g, "solve_"))
            @test isempty(Api.definitions("solve_"; graph=g))

            # match=:prefix matches both bare and qualified tail names starting with "solve_"
            pref_p = Pharos.definitions(g, "solve_"; match=:prefix)
            pref_a = Api.definitions("solve_"; graph=g, match=:prefix)
            @test pref_a == pref_p
            @test [d.name for (_, d) in pref_a] == ["solve_ode", "solve_sde", "Solvers.solve_pde"]

            # match=:substring includes "presolve_ode" as well
            sub_p = Pharos.definitions(g, "solve_"; match=:substring)
            sub_a = Api.definitions("solve_"; graph=g, match=:substring)
            @test sub_a == sub_p
            @test [d.name for (_, d) in sub_a] == ["solve_ode", "solve_sde", "presolve_ode", "Solvers.solve_pde"]

            # match=:regex supports regular expressions
            rx_p = Pharos.definitions(g, "^solve_[os]de\$"; match=:regex)
            rx_a = Api.definitions("^solve_[os]de\$"; graph=g, match=:regex)
            @test rx_a == rx_p
            @test [d.name for (_, d) in rx_a] == ["solve_ode", "solve_sde"]

            # callers also supports match=:prefix and graph=g
            calls_pref = Api.callers("solve_"; graph=g, match=:prefix)
            @test calls_pref == Pharos.callers(g, "solve_"; match=:prefix)
            @test length(calls_pref) == 3

            # grep_defs also supports graph=g
            @test Api.grep_defs("solve_"; graph=g) == Pharos.grep_defs(g, "solve_")

            # Invalid match mode throws ArgumentError
            @test_throws ArgumentError Pharos.definitions(g, "solve_"; match=:fuzzy)
            @test_throws ArgumentError Api.definitions("solve_"; graph=g, match=:fuzzy)
        finally
            rm(repo; recursive=true, force=true)
        end
    end

    @testset "W4-6: Model-first perf helpers (Palette.bench & Palette.warntype_summary)" begin
        reset_kernel_state()

        function w4_stable_sum(v::Vector{Float64})
            s = 0.0
            @inbounds @simd for i in eachindex(v)
                s += v[i]
            end
            return s
        end

        function w4_unstable_sum(n::Int)
            s = 0
            for i in 1:n
                s += 0.5
            end
            return s
        end

        # Palette.bench defaults to samples=11, warmup=2 and returns full latency distribution
        buf = collect(1.0:1024.0)
        b = Palette.bench(w4_stable_sum, buf)
        @test b.samples == 11
        @test b.value == sum(buf)
        @test b.allocated_bytes == 0
        @test b.alloc_bytes == 0
        @test b.min_ms >= 0.0
        @test b.median_ms >= b.min_ms
        @test b.p95_ms >= b.median_ms
        @test b.mean_ms >= b.min_ms

        # Function barrier eliminates non-const global dispatch inside persistent turns
        Palette.execute(Palette.ExecuteCode("""
        function turn_fast_sum(x::Vector{Float64})
            s = 0.0
            @inbounds for v in x
                s += v
            end
            return s
        end
        nonconst_global_vec = collect(1.0:2048.0)
        """))
        r_bench = Palette.execute(Palette.ExecuteCode("Palette.bench(turn_fast_sum, nonconst_global_vec)"))
        @test r_bench.result.success
        @test r_bench.result.data.samples == 11
        @test r_bench.result.data.allocated_bytes == 0
        @test r_bench.result.data.p95_ms >= r_bench.result.data.median_ms

        # warntype_summary on concretely typed function
        ws_ok = Palette.warntype_summary(w4_stable_sum, (Vector{Float64},))
        @test ws_ok.is_concretely_typed === true
        @test ws_ok.return_type === Float64
        @test ws_ok.unstable_vars isa Vector{NamedTuple}
        @test isempty(ws_ok.unstable_vars)
        @test ws_ok.unstable_exprs == 0
        @test !isempty(ws_ok.method_signature)

        # Accepts Tuple{...} type, typeof.(args) tuple, and vector of types
        ws_ok2 = Api.warntype_summary(w4_stable_sum, Tuple{Vector{Float64}})
        ws_ok3 = Api.warntype_summary(w4_stable_sum, typeof.((buf,)))
        @test ws_ok2.is_concretely_typed === true
        @test ws_ok3.is_concretely_typed === true

        # warntype_summary on type-unstable function flags ONLY non-concrete slots & exprs
        ws_bad = Palette.warntype_summary(w4_unstable_sum, (Int,))
        @test ws_bad.is_concretely_typed === false
        @test ws_bad.return_type == Union{Int, Float64}
        @test ws_bad.unstable_vars isa Vector{NamedTuple}
        @test any(v -> v.name === :s, ws_bad.unstable_vars)
        @test ws_bad.unstable_exprs > 0
        @test occursin("w4_unstable_sum", ws_bad.method_signature)

        # Works inside a persistent Palette turn
        r_wt = Palette.execute(Palette.ExecuteCode("Api.warntype_summary(turn_fast_sum, typeof.((nonconst_global_vec,)))"))
        @test r_wt.result.success
        @test r_wt.result.data.is_concretely_typed === true
        @test r_wt.result.data.return_type === Float64
    end

end
