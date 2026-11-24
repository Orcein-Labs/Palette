using Test
using Palette
using Serialization

@testset "Palette Dogfood Wave 2 (Ergonomics & Reliability Upgrades)" begin

    @testset "Upgrade 1: Api.reload_methods! (Revise-style hot-reload without struct/module redefinition)" begin
        reset_kernel_state()
        tmp = mktempdir()
        try
            sub_file = joinpath(tmp, "helpers.jl")
            main_file = joinpath(tmp, "engine.jl")

            write(sub_file, """
            helper_scale(x::Int) = x * 2
            """)

            write(main_file, """
            module ReloadEngine
                abstract type AbstractNode end

                \"\"\"State container\"\"\"
                struct ModelState <: AbstractNode
                    x::Int
                end

                Base.@kwdef mutable struct EngineConfig
                    scale::Int = 2
                end

                const VERSION_TAG = "v1"

                include("helpers.jl")

                \"\"\"Forward pass\"\"\"
                @inline function forward(s::ModelState, cfg::EngineConfig)
                    return s.x * cfg.scale
                end

                @views @inbounds quick_sum(v::AbstractVector{Int}) = sum(v)

                @generated function type_tag(::T) where {T}
                    return :(string(\$T))
                end

                macro scale_expr(ex)
                    :(2 * \$(esc(ex)))
                end
            end
            """)

            run_code(code) = Palette.execute(Palette.ExecuteCode(code))
            @test run_code("include($(repr(main_file)))").result.success
            @test run_code("st = ReloadEngine.ModelState(7); cfg = ReloadEngine.EngineConfig(scale=3); ReloadEngine.forward(st, cfg)").result.data == 21
            @test run_code("ReloadEngine.quick_sum([1, 2, 3])").result.data == 6
            @test run_code("ReloadEngine.helper_scale(5)").result.data == 10
            @test run_code("ReloadEngine.@scale_expr(5)").result.data == 10

            mod = Palette.get_kernel_state().eval_module
            orig_mod = Base.invokelatest(getglobal, mod, :ReloadEngine)
            orig_state_type = Base.invokelatest(getglobal, orig_mod, :ModelState)
            orig_cfg_type = Base.invokelatest(getglobal, orig_mod, :EngineConfig)
            orig_st = Base.invokelatest(getglobal, mod, :st)

            # Edit methods in both main_file and sub_file while keeping all structs & consts intact
            write(sub_file, """
            helper_scale(x::Int) = x * 20
            """)
            write(main_file, """
            module ReloadEngine
                abstract type AbstractNode end

                \"\"\"State container\"\"\"
                struct ModelState <: AbstractNode
                    x::Int
                end

                Base.@kwdef mutable struct EngineConfig
                    scale::Int = 2
                end

                const VERSION_TAG = "v1"

                include("helpers.jl")

                \"\"\"Forward pass updated\"\"\"
                @inline function forward(s::ModelState, cfg::EngineConfig)
                    return s.x * cfg.scale + 100
                end

                @views @inbounds quick_sum(v::AbstractVector{Int}) = sum(v) * 10

                @generated function type_tag(::T) where {T}
                    return :("v2:" * string(\$T))
                end

                macro scale_expr(ex)
                    :(3 * \$(esc(ex)))
                end

                new_method(s::ModelState) = s.x + 1
            end
            """)

            res = Api.reload_methods!(main_file)
            @test res.path == abspath(main_file)
            @test res.skipped_types == 3
            @test res.updated_methods == 6
            @test res.target_module == nameof(mod)

            # Module and struct type identities must be preserved!
            @test Base.invokelatest(getglobal, mod, :ReloadEngine) === orig_mod
            @test Base.invokelatest(getglobal, orig_mod, :ModelState) === orig_state_type
            @test Base.invokelatest(getglobal, orig_mod, :EngineConfig) === orig_cfg_type
            @test orig_st isa orig_state_type

            # Existing struct instances seamlessly invoke the hot-reloaded methods!
            @test run_code("ReloadEngine.forward(st, cfg)").result.data == 121
            @test run_code("ReloadEngine.quick_sum([1, 2, 3])").result.data == 60
            @test run_code("ReloadEngine.helper_scale(5)").result.data == 100
            @test run_code("ReloadEngine.type_tag(st)").result.data == "v2:" * string(orig_state_type)
            @test run_code("ReloadEngine.@scale_expr(5)").result.data == 15
            @test run_code("ReloadEngine.new_method(st)").result.data == 8
            @test run_code("ReloadEngine.VERSION_TAG").result.data == "v1"

            # Passing mod=ReloadEngine directly descends cleanly without creating ReloadEngine.ReloadEngine
            res_sub = Api.reload_methods!(main_file; mod=orig_mod)
            @test res_sub.target_module === :ReloadEngine
            @test res_sub.skipped_types == 3
            @test res_sub.updated_methods == 6
            @test Base.invokelatest(getglobal, orig_mod, :ReloadEngine) === orig_mod
        finally
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "Upgrade 2: KernelScope -> Core.Main module mirroring for Serialization.deserialize" begin
        reset_kernel_state()
        tmp = mktempdir()
        jls_path = joinpath(tmp, "cli_payload.jls")
        try
            # Produce a real .jls artifact from a standalone Julia subprocess where
            # `Wave2SyncCli` lives in `Core.Main` (reproducing the exact dogfooding failure).
            cli_script = """
            using Serialization
            module Wave2SyncCli
                struct Payload
                    x::Int
                    tag::String
                end
            end
            serialize(ARGS[1], Wave2SyncCli.Payload(77, "from_cli"))
            """
            run(`$(Base.julia_cmd()) --startup-file=no -e $cli_script $jls_path`)
            @test isfile(jls_path)
            @test !isdefined(Core.Main, :Wave2SyncCli)

            # Define the matching module inside an isolated KernelScope-like module
            iso_scope = Module(:Wave2IsoScope_Test)
            Core.eval(iso_scope, :(
                module Wave2SyncCli
                    struct Payload
                        x::Int
                        tag::String
                    end
                end
            ))

            # Before syncing, Serialization.deserialize fails because Core.Main lacks Wave2SyncCli
            @test_throws UndefVarError Serialization.deserialize(jls_path)

            # Sync user modules from iso_scope to Core.Main
            synced = Palette.sync_kernel_modules_to_main!(iso_scope)
            @test :Wave2SyncCli in synced
            @test Base.invokelatest(isdefined, Core.Main, :Wave2SyncCli)
            @test Base.invokelatest(getglobal, Core.Main, :Wave2SyncCli) === Base.invokelatest(getglobal, iso_scope, :Wave2SyncCli)
            @test !Base.invokelatest(isdefined, Core.Main, :Wave2IsoScope_Test)

            # After syncing, Serialization.deserialize resolves Main.Wave2SyncCli.Payload transparently!
            deser = Base.invokelatest(Serialization.deserialize, jls_path)
            payload_t = Base.invokelatest(getglobal, Base.invokelatest(getglobal, iso_scope, :Wave2SyncCli), :Payload)
            @test deser isa payload_t
            @test deser.x == 77 && deser.tag == "from_cli"

            # Existing `const` in Core.Main is skipped safely without throwing
            const_guard_sym = :_Wave2ConstGuard_SafeSkip
            if !isdefined(Core.Main, const_guard_sym)
                Core.eval(Core.Main, :(const $(const_guard_sym) = 12345))
            end
            Core.eval(iso_scope, :(module $(const_guard_sym); end))
            @test Palette.sync_kernel_modules_to_main!(iso_scope) isa Vector{Symbol}
            @test Base.invokelatest(getglobal, Core.Main, const_guard_sym) === 12345

            # Automatic mirroring after execute_turn in src/turn.jl
            (receipt, interrupted, stuck), _ = Palette.execute_turn("""
            module Wave2TurnMirrored
                struct Item
                    v::Int
                end
            end
            Wave2TurnMirrored.Item(99)
            """, 10.0)
            @test receipt.result.success && !interrupted && !stuck
            @test Base.invokelatest(isdefined, Core.Main, :Wave2TurnMirrored)
            mod = Palette.get_kernel_state().eval_module
            @test Base.invokelatest(getglobal, Core.Main, :Wave2TurnMirrored) === Base.invokelatest(getglobal, mod, :Wave2TurnMirrored)

            # Serialization round-trip of struct created inside KernelScope
            buf = IOBuffer()
            Serialization.serialize(buf, receipt.result.data)
            seekstart(buf)
            rt = Base.invokelatest(Serialization.deserialize, buf)
            @test rt.v == 99
        finally
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "Upgrade 3: Persistent pinned cache & revival >16 MiB sidecar spill" begin
        # Part A: Api.pin_cache!, Api.get_cache, Api.cache_get!, Api.clear_cache!
        cache_dir = mktempdir()
        try
            @test Api.get_cache(:missing_key; default=:fallback, dir=cache_dir) === :fallback

            mat = reshape(Float32.(1:16), 4, 4)
            @test Api.pin_cache!(:weights, mat; dir=cache_dir) == mat
            @test Api.get_cache(:weights; dir=cache_dir) == mat

            calls = Ref(0)
            v1 = Api.cache_get!("stage1/clauses:v2"; dir=cache_dir) do
                calls[] += 1
                Dict("rows" => [10, 20, 30], "ok" => true)
            end
            v2 = Api.cache_get!("stage1/clauses:v2"; dir=cache_dir) do
                calls[] += 1
                error("builder should not run on cache hit")
            end
            @test calls[] == 1
            @test v1 == v2

            # Caching `nothing` as a valid value still hits cache on second call
            nothing_calls = Ref(0)
            n1 = Api.cache_get!(:explicit_nothing; dir=cache_dir) do
                nothing_calls[] += 1
                nothing
            end
            n2 = Api.cache_get!(:explicit_nothing; dir=cache_dir) do
                nothing_calls[] += 1
                :wrong
            end
            @test n1 === nothing && n2 === nothing
            @test nothing_calls[] == 1

            # Clear single key vs clear all
            @test Api.clear_cache!(; key=:weights, dir=cache_dir) == 1
            @test Api.get_cache(:weights; dir=cache_dir) === nothing
            @test Api.get_cache("stage1/clauses:v2"; dir=cache_dir) == v1
            @test Api.clear_cache!(; dir=cache_dir) == 2
            @test Api.get_cache("stage1/clauses:v2"; dir=cache_dir) === nothing
        finally
            rm(cache_dir; recursive=true, force=true)
        end

        # Default workspace pinned_cache location
        ws_cache = mktempdir()
        old_ws = Palette.WORKSPACE_ROOT[]
        try
            Palette.WORKSPACE_ROOT[] = ws_cache
            Api.pin_cache!(:ws_item, [1, 2, 3])
            @test isdir(joinpath(ws_cache, ".palette", "pinned_cache"))
            @test Api.get_cache(:ws_item) == [1, 2, 3]
            @test Api.clear_cache!(:ws_item) == 1
        finally
            Palette.WORKSPACE_ROOT[] = old_ws
            rm(ws_cache; recursive=true, force=true)
        end

        # Part B: Revival sidecar spill for >16 MiB binding under default limits
        reset_kernel_state()
        old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
        ws = mktempdir()
        st = mktempdir()
        Palette.WORKSPACE_ROOT[] = ws
        Palette.STATE_DIR[] = st
        try
            # ~18 MiB array (> 16 MiB inline cap, < 512 MiB sidecar cap)
            r = Palette.execute(Palette.ExecuteCode("""
            big_tensor = fill(Int32(42), 4_500_000)
            tiny_val = 7
            length(big_tensor)
            """))
            @test r.result.success && r.result.data == 4_500_000

            manifest = Palette.snapshot_state!(1)
            @test manifest !== nothing
            @test manifest["bytes"] < 16 * 1024 * 1024
            @test manifest["sidecar_bytes"] >= 18_000_000

            b_big = only(filter(b -> b["name"] == "big_tensor", manifest["bindings"]))
            @test b_big["class"] == "data"
            @test haskey(b_big, "sidecar")
            @test startswith(b_big["sidecar"], "sidecar-")
            @test isfile(joinpath(st, b_big["sidecar"]))

            g_big = only(filter(g -> "big_tensor" in g["names"], manifest["data"]))
            @test g_big["sidecar"] === true
            @test g_big["bytes"] >= 18_000_000

            # Full kernel reset & revival restores both the 18 MiB sidecar binding and the inline scalar
            Palette.reset_kernel_state()
            report = Palette.revive_state!()
            @test occursin("restored exactly", report)
            @test occursin("big_tensor", report)
            @test occursin("tiny_val", report)
            @test !occursin("not revived", report)

            mod = Palette.get_kernel_state().eval_module
            revived_tensor = Base.invokelatest(getglobal, mod, :big_tensor)
            @test revived_tensor isa Vector{Int32}
            @test length(revived_tensor) == 4_500_000
            @test revived_tensor[1] == Int32(42) && revived_tensor[end] == Int32(42)
            @test Base.invokelatest(getglobal, mod, :tiny_val) == 7

            # Missing sidecar file reports cleanly as lost (not a crash)
            rm(joinpath(st, b_big["sidecar"]); force=true)
            Palette.reset_kernel_state()
            report_missing = Palette.revive_state!()
            @test occursin("big_tensor", report_missing) && occursin("sidecar file is missing", report_missing)
            @test Base.invokelatest(getglobal, Palette.get_kernel_state().eval_module, :tiny_val) == 7

            # Sidecar cap enforcement when explicitly lowered via PALETTE_STATE_MAX_SIDECAR_BYTES
            Palette.reset_kernel_state()
            Palette.execute(Palette.ExecuteCode("too_huge = zeros(UInt8, 100_000); ok_small = 1"))
            withenv("PALETTE_STATE_MAX_BINDING_BYTES" => "10000", "PALETTE_STATE_MAX_SIDECAR_BYTES" => "50000") do
                m2 = Palette.snapshot_state!(2)
                b_huge = only(filter(b -> b["name"] == "too_huge", m2["bindings"]))
                @test b_huge["class"] == "runtime"
                @test occursin("sidecar limit", b_huge["reason"])
            end
        finally
            Palette.reset_kernel_state()
            Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
            rm(ws; recursive=true, force=true)
            rm(st; recursive=true, force=true)
        end
    end

    @testset "Upgrade 4: Pharos.summary & Api.summary compact codebase overview" begin
        reset_kernel_state()
        repo = mktempdir()
        old_root = Palette.WORKSPACE_ROOT[]
        try
            mkpath(joinpath(repo, "src"))
            write(joinpath(repo, "src", "core.jl"), """
            module CoreMod
                abstract type AbstractCore end
                struct CoreItem <: AbstractCore
                    id::Int
                end
                include("util.jl")
                function run_core(x::CoreItem)
                    return util_inc(x.id)
                end
            end
            """)
            write(joinpath(repo, "src", "util.jl"), """
            util_inc(x) = x + 1
            macro noop(ex)
                esc(ex)
            end
            """)
            write(joinpath(repo, "src", "helper.py"), """
            class PyHelper:
                def step(self):
                    return 1
            """)

            s_pharos = Pharos.summary(repo)
            s_api = Api.summary(repo)
            s_graph = Api.summary(Api.codegraph(repo))
            @test s_pharos == s_api
            @test s_api == s_graph

            @test s_api.root == Pharos._normroot(repo)
            @test s_api.file_count == 3
            @test s_api.symbol_count >= 7
            @test length(s_api.files) == 3

            core_entry = only(filter(f -> f.path == "src/core.jl", s_api.files))
            @test [m.name for m in core_entry.modules] == ["CoreMod"]
            @test all(m -> m.line >= 1, core_entry.modules)
            @test [t.name for t in core_entry.types] == ["AbstractCore", "CoreItem"]
            @test [f.name for f in core_entry.functions] == ["run_core"]
            @test core_entry.includes == ["util.jl"]

            util_entry = only(filter(f -> f.path == "src/util.jl", s_api.files))
            @test [f.name for f in util_entry.functions] == ["util_inc", "@noop"]

            # max_files cap
            s_capped = Api.summary(repo; max_files=1)
            @test s_capped.file_count == 3
            @test length(s_capped.files) == 1

            # Active workspace / codegraph! default when target is nothing or ""
            Palette.codegraph!(repo)
            @test Api.summary() == s_api
            @test Api.summary("") == s_api
            @test Pharos.summary() == s_api

            # Works directly inside a Palette turn
            res = Palette.execute(Palette.ExecuteCode("Api.summary()"))
            @test res.result.success
            @test res.result.data.file_count == 3
        finally
            Palette.codegraph!(nothing)
            Palette.WORKSPACE_ROOT[] = old_root
            rm(repo; recursive=true, force=true)
        end
    end

    @testset "Upgrade 5: Api.bench & Api.@bench warmup + allocation + latency probe" begin
        reset_kernel_state()

        function zero_alloc_dot(a::Vector{Float64}, b::Vector{Float64})
            s = 0.0
            @inbounds @simd for i in eachindex(a, b)
                s += a[i] * b[i]
            end
            return s
        end

        alloc_kernel(n::Int) = zeros(Float64, n)

        a = collect(1.0:512.0)
        b = collect(512.0:-1.0:1.0)

        r0 = Api.bench(zero_alloc_dot, a, b; warmup=2, samples=7)
        @test r0.value ≈ sum(a .* b)
        @test r0.alloc_bytes == 0
        @test r0.min_ms >= 0.0
        @test r0.median_ms >= r0.min_ms
        @test r0.samples == 7

        r_alloc = Api.bench(alloc_kernel, 200; warmup=1, samples=5)
        @test length(r_alloc.value) == 200
        @test r_alloc.alloc_bytes >= 1600
        @test r_alloc.samples == 5

        # Macro form on local variables
        r_mac_local = Api.@bench zero_alloc_dot(a, b)
        @test r_mac_local.value ≈ r0.value
        @test r_mac_local.alloc_bytes == 0
        @test r_mac_local.samples == 5

        # Macro form with explicit warmup/samples options
        r_mac_opts = Api.@bench zero_alloc_dot(a, b) warmup=2 samples=6
        @test r_mac_opts.alloc_bytes == 0
        @test r_mac_opts.samples == 6

        # Macro form on allocating expression measures its allocations
        r_mac_alloc = Api.@bench sum(zeros(Float64, 128))
        @test r_mac_alloc.value == 0.0
        @test r_mac_alloc.alloc_bytes >= 1024

        # Inside a Palette turn with non-const top-level globals (zero spurious boxing allocs!)
        Palette.execute(Palette.ExecuteCode("""
        function turn_zero_alloc(x::Vector{Float64})
            s = 0.0
            @inbounds for v in x
                s += v
            end
            return s
        end
        turn_buf = collect(1.0:1000.0)
        """))
        r_turn = Palette.execute(Palette.ExecuteCode("Api.@bench turn_zero_alloc(turn_buf)"))
        @test r_turn.result.success
        @test r_turn.result.data.value == 500500.0
        @test r_turn.result.data.alloc_bytes == 0
        @test r_turn.result.data.samples == 5
    end

end
