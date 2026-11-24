using Test
using Palette

@testset "Palette Dogfood Wave 5 (Micro-Polish: Clean Reload, Barrier, Ergonomics)" begin

    @testset "W5-1: reinclude_module! & reset_bindings! (clean reload without Main/eval_module pollution)" begin
        reset_kernel_state()
        tmp = mktempdir()
        mod_file = joinpath(tmp, "w5_model.jl")
        multi_file = joinpath(tmp, "w5_multi.jl")
        wrapper_file = joinpath(tmp, "w5_wrapper.jl")
        inner_file = joinpath(tmp, "w5_inner.jl")
        flat_file = joinpath(tmp, "w5_flat.jl")
        try
            write(mod_file, """
            module W5Model
                export W5State, w5_step
                struct W5State
                    x::Int
                end
                w5_step(s::W5State) = s.x + 10
            end
            """)

            # 1. Auto-detects single module, returns fresh handle without polluting Core.Main or eval_module
            m1 = Palette.reinclude_module!(mod_file)
            @test m1 isa Module
            @test nameof(m1) === :W5Model
            @test :W5Model in Api.sandbox_modules()
            @test Base.invokelatest(Base.invokelatest(getglobal, m1, :w5_step),
                                    Base.invokelatest(Base.invokelatest(getglobal, m1, :W5State), 5)) == 15
            @test !Base.invokelatest(isdefined, Core.Main, :W5Model)
            @test !Base.invokelatest(isdefined, Core.Main, :W5State)
            eval_mod = Palette.get_kernel_state().eval_module
            @test !Base.invokelatest(isdefined, eval_mod, :W5Model)
            @test !Base.invokelatest(isdefined, eval_mod, :W5State)

            # 2. Modify struct layout & function, reload via Api.reinclude_module! and bare reinclude_module! in turn
            write(mod_file, """
            module W5Model
                export W5State, w5_step
                struct W5State
                    x::Int
                    y::Int
                end
                w5_step(s::W5State) = s.x * 100 + s.y
            end
            """)
            m2 = Api.reinclude_module!(mod_file)
            @test m2 !== m1
            @test Base.invokelatest(Base.invokelatest(getglobal, m2, :w5_step),
                                    Base.invokelatest(Base.invokelatest(getglobal, m2, :W5State), 3, 7)) == 307

            # Bare reinclude_module! works directly inside session eval_module
            r_turn = Palette.execute(Palette.ExecuteCode("""
            m_turn = reinclude_module!($(repr(mod_file)))
            m_turn.w5_step(m_turn.W5State(4, 9))
            """))
            @test r_turn.result.success
            @test r_turn.result.data == 409
            @test !Base.invokelatest(isdefined, Core.Main, :W5Model)
            @test !Base.invokelatest(isdefined, Core.Main, :m_turn)

            # 3. Multiple modules in one file: auto-detect picks last defined module; explicit mod_name picks named one
            write(multi_file, """
            module W5Alpha
                val() = :alpha
            end
            module W5Beta
                val() = :beta
            end
            """)
            m_last = Api.reinclude_module!(multi_file)
            @test nameof(m_last) === :W5Beta
            @test Base.invokelatest(Base.invokelatest(getglobal, m_last, :val)) === :beta

            m_first_kw = Api.reinclude_module!(multi_file; mod_name=:W5Alpha)
            @test nameof(m_first_kw) === :W5Alpha
            @test Base.invokelatest(Base.invokelatest(getglobal, m_first_kw, :val)) === :alpha

            m_first_pos = Palette.reinclude_module!(:W5Alpha, multi_file)
            @test nameof(m_first_pos) === :W5Alpha
            @test m_first_pos !== m_first_kw

            # 4. Wrapper file that include()s another file defining a module -> auto-detects inner module
            write(inner_file, """
            module W5IncludedInner
                inner_val() = 777
            end
            """)
            write(wrapper_file, """
            include($(repr(inner_file)))
            """)
            m_inc = Api.reinclude_module!(wrapper_file)
            @test nameof(m_inc) === :W5IncludedInner
            @test Base.invokelatest(Base.invokelatest(getglobal, m_inc, :inner_val)) == 777
            @test :W5IncludedInner in Api.sandbox_modules()

            # 5. Flat file without a module block loads cleanly into sandbox container
            write(flat_file, """
            struct W5Flat
                a::Int
            end
            w5_flat_fn(x::W5Flat) = x.a + 1
            """)
            f1 = Api.reinclude_module!(flat_file)
            @test startswith(string(nameof(f1)), "PaletteSandbox_")
            @test Base.invokelatest(Base.invokelatest(getglobal, f1, :w5_flat_fn),
                                    Base.invokelatest(Base.invokelatest(getglobal, f1, :W5Flat), 9)) == 10
            write(flat_file, """
            struct W5Flat
                a::Int
                b::Int
            end
            w5_flat_fn(x::W5Flat) = x.a + x.b
            """)
            f2 = Api.reinclude_module!(flat_file)
            @test f2 !== f1
            @test Base.invokelatest(Base.invokelatest(getglobal, f2, :w5_flat_fn),
                                    Base.invokelatest(Base.invokelatest(getglobal, f2, :W5Flat), 9, 11)) == 20

            # 6. Optional `into=` target module binds handle safely as mutable global across reloads
            target_scope = Module(:W5TargetScope)
            m_into1 = Api.reinclude_module!(mod_file; into=target_scope)
            @test Base.invokelatest(getglobal, target_scope, :W5Model) === m_into1
            @test !Base.invokelatest(isconst, target_scope, :W5Model)
            m_into2 = Api.reinclude_module!(mod_file; into=target_scope)
            @test m_into2 !== m_into1
            @test Base.invokelatest(getglobal, target_scope, :W5Model) === m_into2
            # Const binding in `into` does not throw
            Core.eval(target_scope, :(const W5Alpha = 42))
            m_const_safe = Api.reinclude_module!(:W5Alpha, multi_file; into=target_scope)
            @test Base.invokelatest(getglobal, target_scope, :W5Alpha) === 42
            @test Base.invokelatest(Base.invokelatest(getglobal, m_const_safe, :val)) === :alpha

            # Missing file throws ArgumentError
            @test_throws ArgumentError Api.reinclude_module!(joinpath(tmp, "does_not_exist.jl"))

            # 7. reset_bindings! (varargs, vector form, const/undef/kernel safety, cache cleanup)
            Palette.reset_kernel_state()
            r_setup = Palette.execute(Palette.ExecuteCode("""
            include($(repr(mod_file)))
            using .W5Model
            stale_obj = W5State(1, 2)
            mutable_x = 123
            @ephemeral ephem_buf = zeros(8)
            const FIXED_K = 99
            helper_f(x) = x + 1
            """))
            @test r_setup.result.success
            @test :ephem_buf in Api.ephemerals()

            # Reset via bare reset_bindings! and vector form inside turn, including const/undef/kernel syms
            r_res1 = Palette.execute(Palette.ExecuteCode("""
            reset_bindings!(:stale_obj, :ephem_buf, :FIXED_K, :helper_f, :never_defined, :Palette)
            """))
            @test r_res1.result.success
            @test r_res1.result.data.reset == [:stale_obj, :ephem_buf]
            @test r_res1.result.data.skipped == [:FIXED_K, :helper_f, :never_defined, :Palette]
            @test !(:ephem_buf in Api.ephemerals())

            em = Palette.get_kernel_state().eval_module
            @test Base.invokelatest(getglobal, em, :stale_obj) === nothing
            @test Base.invokelatest(getglobal, em, :ephem_buf) === nothing
            @test Base.invokelatest(getglobal, em, :FIXED_K) == 99

            # Vector form via Api.reset_bindings!
            res_vec = Api.reset_bindings!([:mutable_x, :missing_vec_sym])
            @test res_vec.reset == [:mutable_x]
            @test res_vec.skipped == [:missing_vec_sym]
            @test Base.invokelatest(getglobal, em, :mutable_x) === nothing
            @test !haskey(Palette.get_kernel_state().variables, "mutable_x")

            # After modifying mod_file & reloading via reinclude_module!, stale_obj no longer triggers
            # "an earlier definition" noise in binding_list!
            write(mod_file, """
            module W5Model
                export W5State, w5_step
                struct W5State
                    x::Int
                    y::Int
                    z::Int
                end
                w5_step(s::W5State) = s.x + s.y + s.z
            end
            """)
            r_reload = Palette.execute(Palette.ExecuteCode("""
            m_new = reinclude_module!($(repr(mod_file)))
            m_new.w5_step(m_new.W5State(10, 20, 30))
            """))
            @test r_reload.result.success
            @test r_reload.result.data == 60
            bl = Palette.binding_list!(em, 99)
            @test !any(row -> occursin("earlier definition", row), bl)

            # Explicit mod= argument does not alter session EPHEMERAL_BINDINGS
            Api.ephemeral!(:session_keep)
            other_mod = Module(:W5ResetOther)
            Core.eval(other_mod, :(session_keep = 55; const C = 1))
            res_other = Palette.reset_bindings!(:session_keep, :C, :no_such; mod=other_mod)
            @test res_other.reset == [:session_keep]
            @test res_other.skipped == [:C, :no_such]
            @test Base.invokelatest(getglobal, other_mod, :session_keep) === nothing
            @test :session_keep in Api.ephemerals()
        finally
            Palette.reset_kernel_state()
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "W5-2: @alloc_barrier macro & warntype_summary value overloads" begin
        reset_kernel_state()

        # 1. Non-const global bindings in eval_module: @alloc_barrier measures 0 B!
        r_init = Palette.execute(Palette.ExecuteCode("""
        function w5_zero_dot(a::Vector{Float64}, b::Vector{Float64})
            s = 0.0
            @inbounds @simd for i in eachindex(a, b)
                s += a[i] * b[i]
            end
            return s
        end
        function w5_kw_scaled(v::Vector{Float64}; scale::Float64=1.0, bias::Float64=0.0)
            s = 0.0
            @inbounds for x in v
                s += x * scale + bias
            end
            return s
        end
        struct W5Point
            x::Float64
            y::Float64
        end
        w5_alloc_vec(n::Int) = zeros(Float64, n)
        g_dot_fn = w5_zero_dot
        g_kw_fn = w5_kw_scaled
        g_a = collect(1.0:256.0)
        g_b = collect(256.0:-1.0:1.0)
        scale = 2.5
        g_extra_kw = (bias = 1.0,)
        g_args_pair = (g_a, g_b)
        g_n = 16
        """))
        @test r_init.result.success

        # Compare raw @allocated on non-const keyword call (allocates!) vs @alloc_barrier (0 B!)
        r_raw_kw = Palette.execute(Palette.ExecuteCode("@allocated g_kw_fn(g_a; scale, g_extra_kw...)"))
        @test r_raw_kw.result.success
        @test r_raw_kw.result.data > 0

        # Bare @alloc_barrier, Palette.@alloc_barrier, Api.@alloc_barrier on non-const globals
        for expr_code in (
            "@alloc_barrier g_dot_fn(g_a, g_b)",
            "Palette.@alloc_barrier g_dot_fn(g_a, g_b)",
            "Api.@alloc_barrier g_dot_fn(g_a, g_b)",
            "@alloc_barrier g_dot_fn(g_args_pair...)",
        )
            res = Palette.execute(Palette.ExecuteCode(expr_code))
            @test res.result.success
            @test res.result.data.value ≈ sum(collect(1.0:256.0) .* collect(256.0:-1.0:1.0))
            @test res.result.data.alloc_bytes == 0
            @test res.result.data.bytes == 0
        end

        # Keyword call forms (; kw, splat..., positional kw=) all measure 0 B
        for kw_code in (
            "@alloc_barrier g_kw_fn(g_a; scale, g_extra_kw...)",
            "@alloc_barrier g_kw_fn(g_a; scale=2.5, bias=1.0)",
            "@alloc_barrier g_kw_fn(g_a, scale=2.5, bias=1.0)",
        )
            res = Palette.execute(Palette.ExecuteCode(kw_code))
            @test res.result.success
            @test res.result.data.value ≈ sum(collect(1.0:256.0)) * 2.5 + 256.0
            @test res.result.data.alloc_bytes == 0
            @test res.result.data.bytes == 0
        end

        # Type constructor barrier
        r_ctor = Palette.execute(Palette.ExecuteCode("@alloc_barrier W5Point(1.5, 2.5)"))
        @test r_ctor.result.success
        @test r_ctor.result.data.alloc_bytes == 0
        @test r_ctor.result.data.bytes == 0

        # Allocating function reports real non-zero heap allocation (without JIT compilation noise)
        r_alloc = Palette.execute(Palette.ExecuteCode("@alloc_barrier w5_alloc_vec(g_n)"))
        @test r_alloc.result.success
        @test length(r_alloc.result.data.value) == 16
        @test r_alloc.result.data.alloc_bytes >= 128
        @test r_alloc.result.data.bytes == r_alloc.result.data.alloc_bytes

        # Arguments are evaluated exactly once (not twice for warmup + measured)
        r_once = Palette.execute(Palette.ExecuteCode("""
        eval_ticks = Ref(0)
        next_vec() = (eval_ticks[] += 1; g_a)
        res_once = @alloc_barrier w5_zero_dot(next_vec(), g_b)
        (res=res_once, ticks=eval_ticks[])
        """))
        @test r_once.result.success
        @test r_once.result.data.ticks == 1
        @test r_once.result.data.res.alloc_bytes == 0

        # Fallback expressions (block, do-block, dotted broadcast operator)
        r_blk = Palette.execute(Palette.ExecuteCode("""
        @alloc_barrier begin
            x1 = 10
            x2 = 32
            x1 + x2
        end
        """))
        @test r_blk.result.success && r_blk.result.data.value == 42 && r_blk.result.data.bytes == 0

        r_do = Palette.execute(Palette.ExecuteCode("""
        @alloc_barrier sum(1:10) do k
            k * k
        end
        """))
        @test r_do.result.success && r_do.result.data.value == 385 && r_do.result.data.bytes == 0

        r_dot = Palette.execute(Palette.ExecuteCode("@alloc_barrier (g_a .+ 1.0)"))
        @test r_dot.result.success && length(r_dot.result.data.value) == 256 && r_dot.result.data.bytes >= 2048

        # Direct Palette.@alloc_barrier inside local function scope
        function local_zero_fn(v::Vector{Float64})
            s = 0.0
            @inbounds for x in v
                s += x
            end
            return s
        end
        lv = collect(1.0:32.0)
        r_local = Palette.@alloc_barrier local_zero_fn(lv)
        @test r_local.value == 528.0
        @test r_local.alloc_bytes == 0
        @test r_local.bytes == 0

        # 2. warntype_summary ergonomic overloads
        w5_stable_add(x::Vector{Float64}, y::Float64) = sum(x) + y
        w5_zero_arg() = 42
        w5_unstable(n::Int) = (s = 0; for i in 1:n; s += 0.5; end; s)
        w5_pair_sum(p::Tuple{Int, Float64}) = p[1] + p[2]
        w5_anyvec_len(v::Vector{Any}) = length(v)

        # Preserve Wave 4 type-spec forms 100%
        @test Palette.warntype_summary(w5_stable_add, Tuple{Vector{Float64}, Float64}).is_concretely_typed === true
        @test Palette.warntype_summary(w5_stable_add, (Vector{Float64}, Float64)).is_concretely_typed === true
        @test Palette.warntype_summary(w5_stable_add, [Vector{Float64}, Float64]).is_concretely_typed === true
        @test Palette.warntype_summary(w5_zero_arg, ()).is_concretely_typed === true
        @test Palette.warntype_summary(w5_zero_arg, DataType[]).is_concretely_typed === true

        # Value-argument ergonomic forms (0, 1, and 2+ args) + warntype_summary_call
        @test Palette.warntype_summary(w5_zero_arg).is_concretely_typed === true
        @test Palette.warntype_summary(w5_zero_arg).return_type === Int

        ws_1_bad = Palette.warntype_summary(w5_unstable, 10)
        @test ws_1_bad.is_concretely_typed === false
        @test ws_1_bad.return_type == Union{Int, Float64}
        @test any(v -> v.name === :s, ws_1_bad.unstable_vars)

        ws_tuple_val = Api.warntype_summary(w5_pair_sum, (1, 2.5))
        @test ws_tuple_val.is_concretely_typed === true
        @test ws_tuple_val.return_type === Float64

        ws_anyvec = Api.warntype_summary(w5_anyvec_len, Any[])
        @test ws_anyvec.is_concretely_typed === true
        @test ws_anyvec.return_type === Int

        ws_2_ok = Palette.warntype_summary(w5_stable_add, lv, 3.0)
        @test ws_2_ok.is_concretely_typed === true
        @test ws_2_ok.return_type === Float64
        @test isempty(ws_2_ok.unstable_vars)
        @test ws_2_ok.unstable_exprs == 0

        ws_api_call = Api.warntype_summary_call(w5_stable_add, lv, 3.0)
        @test ws_api_call == ws_2_ok

        # Works inside a persistent Palette turn on non-const workspace globals
        r_wt_turn = Palette.execute(Palette.ExecuteCode("""
        (
            a=Palette.warntype_summary(w5_zero_dot, g_a, g_b),
            b=Api.warntype_summary(w5_zero_dot, g_a, g_b),
            c=Api.warntype_summary_call(w5_zero_dot, g_a, g_b),
        )
        """))
        @test r_wt_turn.result.success
        @test r_wt_turn.result.data.a.is_concretely_typed === true
        @test r_wt_turn.result.data.a.return_type === Float64
        @test r_wt_turn.result.data.b == r_wt_turn.result.data.a
        @test r_wt_turn.result.data.c == r_wt_turn.result.data.a
    end

end
