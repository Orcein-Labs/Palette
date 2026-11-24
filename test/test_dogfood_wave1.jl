using Test
using Palette

@testset "Palette Fix Wave 1 (Agent Dogfooding Ergonomics)" begin

    @testset "Fix 1: Pharos export, session binding, explicit-graph forwarding, codegraph!, grep_defs" begin
        reset_kernel_state()
        @test isdefined(Main, :Pharos) || isdefined(@__MODULE__, :Pharos)
        @test Palette.Pharos isa Module
        @test Palette.Api isa Module
        @test :Pharos in Palette.KERNEL_BINDINGS

        run_code(code) = Palette.execute(Palette.ExecuteCode(code))

        # Pharos is bound directly in the session eval_module and hidden from varinfo()
        @test run_code("Pharos isa Module").result.data === true
        @test run_code("Pharos === Palette.Pharos").result.data === true
        @test run_code("Pharos === Api.Pharos").result.data === true
        rows = run_code("varinfo()").result.data.rows
        @test !("Pharos" in first.(rows))
        @test execute(EphemeralTool("Pharos isa Module")).result.data === true

        # Build two separate temp repos (workspace root vs external repo root)
        ws_dir = mktempdir()
        ext_dir = mktempdir()
        mkpath(joinpath(ws_dir, "src"))
        mkpath(joinpath(ext_dir, "src"))
        mkpath(joinpath(ext_dir, "test"))
        write(joinpath(ws_dir, "src", "ws_only.jl"), """
        ws_helper(x) = x + 1
        ws_caller(x) = ws_helper(x)
        """)
        write(joinpath(ext_dir, "src", "core_engine.jl"), """
        struct EngineState
            count::Int
        end
        engine_step(s::EngineState) = s.count + 1
        engine_run(s::EngineState) = engine_step(s)
        """)
        write(joinpath(ext_dir, "test", "test_engine.jl"), """
        engine_test_case() = engine_run(EngineState(1))
        """)

        old_root = Palette.WORKSPACE_ROOT[]
        try
            Palette.WORKSPACE_ROOT[] = ws_dir
            Palette.codegraph!(nothing)

            # Explicit CodeGraph forwarding on both Palette and Api
            g_ext = Palette.codegraph(ext_dir)
            defs_p = Palette.definitions(g_ext, "engine_step")
            defs_a = Api.definitions(g_ext, "engine_step")
            @test length(defs_p) == 1
            @test defs_p == defs_a
            @test defs_p[1][1] == "src/core_engine.jl"
            @test defs_p[1][2].name == "engine_step"

            calls_p = Palette.callers(g_ext, "engine_step")
            calls_a = Api.callers(g_ext, "engine_step")
            @test length(calls_p) == 1
            @test calls_p == calls_a
            @test calls_p[1][2].caller == "engine_run"

            # Optional root=... kwarg on Api.definitions / Api.callers / Api.grep_defs
            @test length(Api.definitions("ws_helper")) == 1
            @test isempty(Api.definitions("engine_step"))
            @test length(Api.definitions("engine_step"; root=ext_dir)) == 1
            @test length(Api.callers("engine_step"; root=ext_dir)) == 1

            # Api.codegraph!(ext_dir) binds external repo as active graph in open workspaces
            g_bound = Api.codegraph!(ext_dir)
            @test g_bound.root == Palette.Pharos._normroot(ext_dir)
            @test length(Api.definitions("engine_step")) == 1
            @test length(Api.callers("engine_step")) == 1
            @test length(Api.definitions("ws_helper")) == 0

            # In-turn usage of Pharos + Api forwarding
            @test run_code("length(Api.definitions(\"engine_step\"))").result.data == 1
            @test run_code("g_t = Api.codegraph(); length(Api.definitions(g_t, \"engine_step\"))").result.data == 1
            @test run_code("Pharos.refresh!(g_t) isa Pharos.CodeGraph").result.data === true

            # grep_defs with substring, path_prefix, ./ normalization, and ignorecase
            all_engine = Pharos.grep_defs(g_ext, "engine")
            @test [m.name for m in all_engine] == ["engine_step", "engine_run", "engine_test_case"]
            @test all(m -> m isa NamedTuple{(:file, :line, :kind, :name)}, all_engine)

            src_only = Api.grep_defs(g_ext, "engine"; path_prefix="src/")
            @test [m.name for m in src_only] == ["engine_step", "engine_run"]
            @test all(m -> startswith(m.file, "src/"), src_only)

            dot_slash = Api.grep_defs(g_ext, "engine"; path_prefix="./src/")
            @test dot_slash == src_only

            icase = Api.grep_defs("ENGINE"; path_prefix="./src/", ignorecase=true)
            @test [m.name for m in icase] == ["EngineState", "engine_step", "engine_run"]

            # Reset back to WORKSPACE_ROOT[]
            @test Api.codegraph!(nothing) === nothing
            @test length(Api.definitions("ws_helper")) == 1
        finally
            Palette.codegraph!(nothing)
            Palette.WORKSPACE_ROOT[] = old_root
            rm(ws_dir; recursive=true, force=true)
            rm(ext_dir; recursive=true, force=true)
        end
    end

    @testset "Fix 2: Shared read-only depot helper & operator-mode timeout child cleanup" begin
        # Part A: depot_mode() and enable_shared_depot!()
        old_env = get(ENV, "PALETTE_DEPOT_MODE", nothing)
        old_depots = copy(Base.DEPOT_PATH)
        fake_depot = mktempdir()
        try
            delete!(ENV, "PALETTE_DEPOT_MODE")
            @test Api.depot_mode() === :isolated

            primary_before = first(Base.DEPOT_PATH)
            depots_after = Api.enable_shared_depot!(fake_depot)
            @test Api.depot_mode() === :shared_readonly
            @test first(Base.DEPOT_PATH) == primary_before
            @test last(Base.DEPOT_PATH) == abspath(fake_depot)
            @test depots_after == Base.DEPOT_PATH

            # Idempotent re-enable does not duplicate entries
            n_before = length(Base.DEPOT_PATH)
            Api.enable_shared_depot!(fake_depot)
            @test length(Base.DEPOT_PATH) == n_before
        finally
            empty!(Base.DEPOT_PATH)
            append!(Base.DEPOT_PATH, old_depots)
            if old_env === nothing
                delete!(ENV, "PALETTE_DEPOT_MODE")
            else
                ENV["PALETTE_DEPOT_MODE"] = old_env
            end
            rm(fake_depot; recursive=true, force=true)
        end

        # Part B: Operator-mode timeout reaps child & grandchild processes spawned by the turn,
        # while preserving processes started by earlier turns.
        if Sys.islinux()
            reset_kernel_state()
            pid_file = tempname()
            earlier_proc = run(`sleep 60`; wait=false)
            earlier_pid = Int(getpid(earlier_proc))
            try
                # Turn spawns a bash parent + sleep grandchild and blocks past timeout_s
                code = """
                run(`bash -c "sleep 60 & echo \\\$! > $(pid_file); wait"`)
                """
                (receipt, interrupted, stuck), _ = Palette.execute_turn(code, 0.4)
                @test interrupted
                @test !stuck

                # Wait briefly for OS reaping of killed children
                sleep(0.15)
                @test isfile(pid_file)
                grandchild_pid = parse(Int, strip(read(pid_file, String)))
                # Grandchild must have been killed (kill(pid, 0) fails with ESRCH or process is gone/zombie)
                alive_and_not_zombie(pid) = begin
                    isfile("/proc/$pid/stat") || return false
                    st = try read("/proc/$pid/stat", String) catch; return false end
                    rp = findlast(')', st)
                    rp === nothing && return false
                    state_ch = split(st[rp+2:end])[1]
                    return state_ch != "Z"
                end
                @test !alive_and_not_zombie(grandchild_pid)
                # Earlier turn's process must remain alive!
                @test alive_and_not_zombie(earlier_pid)
            finally
                try kill(earlier_proc); wait(earlier_proc) catch end
                rm(pid_file; force=true)
            end
        end
    end

    @testset "Fix 3: Disk-backed large read-only mmap binding revival (Api.bind_mmap)" begin
        reset_kernel_state()
        old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
        ws = mktempdir()
        st = mktempdir()
        Palette.WORKSPACE_ROOT[] = ws
        Palette.STATE_DIR[] = st
        data_file = joinpath(ws, "features.bin")
        offset_file = joinpath(ws, "offset_features.bin")
        try
            # Write a 256 KiB Float32 matrix (much larger than a lowered 4 KiB binding cap)
            mat = reshape(Float32.(1:(256 * 256)), 256, 256)
            open(data_file, "w") do io
                write(io, mat)
            end
            # Write a file with a 16-byte header followed by 10 Int64 values
            hdr = UInt8[0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
            vec64 = Int64.(101:110)
            open(offset_file, "w") do io
                write(io, hdr)
                write(io, vec64)
            end

            withenv("PALETTE_STATE_MAX_BINDING_BYTES" => "4096") do
                arr = Api.bind_mmap(:BIG_X, data_file, Float32, (256, 256))
                @test size(arr) == (256, 256)
                @test arr[1, 1] == 1.0f0 && arr[256, 256] == Float32(256 * 256)

                off_arr = Api.bind_mmap(:OFF_V, offset_file, Int64, (10,); offset=16)
                @test collect(off_arr) == vec64

                # Snapshot must NOT serialize the 256 KiB array bytes!
                manifest = Palette.snapshot_state!(1)
                @test manifest["bytes"] == 0
                b_big = only(filter(b -> b["name"] == "BIG_X", manifest["bindings"]))
                @test b_big["class"] == "mmap"
                @test b_big["mmap"]["eltype"] == "Float32"
                @test b_big["mmap"]["dims"] == [256, 256]
                @test b_big["mmap"]["offset"] == 0

                b_off = only(filter(b -> b["name"] == "OFF_V", manifest["bindings"]))
                @test b_off["class"] == "mmap"
                @test b_off["mmap"]["offset"] == 16

                # Simulate kernel crash & full revival
                Palette.reset_kernel_state()
                report = Palette.revive_state!()
                @test occursin("restored exactly", report)
                @test occursin("BIG_X", report)
                @test occursin("OFF_V", report)
                @test !occursin("not revived", report)

                mod = Palette.get_kernel_state().eval_module
                revived_x = Base.invokelatest(getglobal, mod, :BIG_X)
                revived_v = Base.invokelatest(getglobal, mod, :OFF_V)
                @test size(revived_x) == (256, 256)
                @test revived_x[1, 1] == 1.0f0 && revived_x[256, 256] == Float32(256 * 256)
                @test collect(revived_v) == vec64

                # Survives a second snapshot + revival cycle (MMAP_BINDINGS was re-registered!)
                manifest2 = Palette.snapshot_state!(2)
                @test manifest2["bytes"] == 0
                Palette.reset_kernel_state()
                report2 = Palette.revive_state!()
                @test occursin("BIG_X", report2)
                @test !occursin("not revived", report2)

                # Missing backing file is reported cleanly as lost (not a crash)
                rm(offset_file; force=true)
                Palette.reset_kernel_state()
                report3 = Palette.revive_state!()
                @test occursin("BIG_X", report3)
                @test occursin("OFF_V", report3) && occursin("missing", report3)
            end
        finally
            Palette.reset_kernel_state()
            Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
            rm(ws; recursive=true, force=true)
            rm(st; recursive=true, force=true)
        end
    end

    @testset "Fix 5: Isolated verification helper (Api.eval_isolated)" begin
        reset_kernel_state()
        mod = Palette.get_kernel_state().eval_module

        # Runs inside a compiled helper function (tests Julia 1.12 world-age safety!)
        function run_from_compiled_fn()
            code = """
            struct LocalPoint
                x::Int
                y::Int
            end
            compute(a, b; scale=1) = (LocalPoint(a, b).x + LocalPoint(a, b).y) * scale
            """
            tests = """
            @assert compute(2, 3) == 5
            @assert compute(2, 3; scale=4) == 20
            :tests_passed
            """
            return Api.eval_isolated(code; entry=:compute, args=(10, 5), kwargs=(scale=3,), tests=tests)
        end

        res = run_from_compiled_fn()
        @test res.ok === true
        @test res.module_name isa Symbol
        @test res.value == 45
        @test res.elapsed_s >= 0.0
        @test res.error === nothing

        # Must NOT pollute the persistent session eval_module
        @test !Base.invokelatest(isdefined, mod, :LocalPoint)
        @test !Base.invokelatest(isdefined, mod, :compute)

        # Code + tests without entry returns test value
        res_t = Api.eval_isolated("f(x) = x * 2"; tests="f(21)")
        @test res_t.ok === true
        @test res_t.value == 42

        # Errors are caught cleanly into structured NamedTuple
        res_err = Api.eval_isolated("f(x::Int) = x + 1"; entry=:f, args=("bad",))
        @test res_err.ok === false
        @test res_err.value === nothing
        @test res_err.error isa String && occursin("MethodError", res_err.error)

        # Missing entry reports clear error
        res_missing = Api.eval_isolated("x = 1"; entry=:no_such_fn)
        @test res_missing.ok === false
        @test occursin("no_such_fn", res_missing.error)
    end

    @testset "Fix 6: Clean error payloads (suppress spurious task-print notice footer)" begin
        reset_kernel_state()

        # Scrubs spurious Julia runtime task-print notices
        noisy = "\nSYSTEM: caught exception of type :ErrorException while trying to print a failed Task notice; giving up\n"
        @test Palette.scrub_task_print_notices(noisy) == ""
        @test Palette.task_notice_hint(noisy) == ""

        mixed = "real line\nSYSTEM: caught exception of type :ErrorException while trying to print a failed Task notice; giving up\nafter\n"
        @test Palette.task_notice_hint(mixed) == "real line\nafter\n"

        # Coincidental mention of "failed Task notice" does NOT trigger the task-failure footer
        coincidental = "searching for failed Task notice in docs\n"
        @test Palette.task_notice_hint(coincidental) == coincidental

        # Genuine unhandled task error banner still receives the actionable hint
        real_unhandled = "UNHANDLED TASK ERROR: boom\nStacktrace:\n [1] error\n"
        hinted = Palette.task_notice_hint(real_unhandled)
        @test occursin("Palette.jobs()", hinted)

        # Ordinary MethodError returns compact payload without SYSTEM notice or internal provenance.jl frames
        (receipt, interrupted, stuck), out = Palette.execute_turn("1 + \"a\"", 10.0)
        @test !receipt.result.success
        @test !interrupted && !stuck
        @test occursin("MethodError", receipt.result.error)
        @test !occursin("failed Task notice", receipt.result.error)
        @test !occursin("failed Task notice", out)
        @test !occursin("provenance.jl", receipt.result.error)
        @test !occursin("Palette.jobs()", Palette.task_notice_hint(out))

        # FdWriter satisfies complete writable IO interface used by errormonitor/IOContext
        p = tempname()
        open(p, "a+") do io
            w = Palette.FdWriter(Cint(2), io)
            @test isopen(w)
            @test iswritable(w)
            @test !isreadable(w)
            @test displaysize(w) == (24, 80)
            @test lock(w) === nothing
            @test unlock(w) === nothing
        end
        rm(p; force=true)
    end

end
