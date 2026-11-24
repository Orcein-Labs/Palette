using Test
using Palette
using Sockets
using JSON

@testset "Palette Dogfood Wave 3 & Private Lab Isolation" begin

    @testset "Part A.1: Private Lab single-root containment (PALETTE_HOME, 0o700/0o600, zero leakage, durable revival)" begin
        reset_kernel_state()
        tmp = mktempdir()
        priv_home = joinpath(tmp, "inquisitor_private_home")
        fake_home = joinpath(tmp, "shared_user_home")
        shared_palette_dir = joinpath(fake_home, ".local", "share", "palette")
        ws = joinpath(priv_home, "workspace")
        outside_dir = joinpath(tmp, "outside_escape")
        mkpath(fake_home)
        mkpath(outside_dir)

        old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
        old_init_env = copy(Palette.INITIAL_ENV[])
        try
            # Fail-closed when PALETTE_PRIVATE_MODE=1 has no root directory
            withenv(
                "PALETTE_PRIVATE_MODE" => "1",
                "PALETTE_HOME" => nothing,
                "PALETTE_PRIVATE_DIR" => nothing,
                "PALETTE_DATA_HOME" => nothing,
            ) do
                @test_throws ErrorException Palette.private_lab_root()
            end

            withenv(
                "HOME" => fake_home,
                "PALETTE_HOME" => priv_home,
                "PALETTE_PRIVATE_MODE" => "1",
                "PALETTE_PRIVATE_DIR" => nothing,
                "PALETTE_PRIVATE_STRICT" => nothing,
                "PALETTE_PROVENANCE_DIR" => nothing,
                "PALETTE_PINNED_CACHE_DIR" => nothing,
                "PALETTE_PHAROS_CACHE_DIR" => nothing,
                "PALETTE_STATE_MAX_BINDING_BYTES" => nothing,
                "PALETTE_STATE_MAX_SIDECAR_BYTES" => nothing,
            ) do
                Palette.STATE_DIR[] = ""
                cfg = Palette.configure_private_lab!(; strict=false)
                @test cfg !== nothing
                @test cfg.root == abspath(priv_home)
                @test Palette.is_private_mode()
                @test Palette.STATE_DIR[] == joinpath(abspath(priv_home), "state")
                Palette.WORKSPACE_ROOT[] = ws
                Palette.INITIAL_ENV[] = Dict{String, String}(ENV)

                # Strict 0o700 permissions on PALETTE_HOME and every created subdirectory
                @test (stat(priv_home).mode & 0o777) == 0o700
                for sub in ("workspace", "state", "worlds", "scratch", "packages", "logs", "run", "provenance", "pharos", "pinned_cache")
                    d = joinpath(priv_home, sub)
                    @test isdir(d)
                    @test (stat(d).mode & 0o777) == 0o700
                end

                # Containment violation fails closed
                Palette.STATE_DIR[] = outside_dir
                @test_throws ErrorException Palette.effective_state_dir(; create=true)
                Palette.STATE_DIR[] = joinpath(abspath(priv_home), "state")

                withenv("PALETTE_PINNED_CACHE_DIR" => outside_dir) do
                    @test_throws ErrorException Palette.default_pinned_cache_dir()
                end

                # Execute inline value + >16 MiB sidecar tensor + pinned cache + ephemeral provenance + Pharos index
                r = Palette.execute(Palette.ExecuteCode("""
                oracle_secret = 987654321
                big_oracle_tensor = fill(Int32(17), 4_500_000)
                length(big_oracle_tensor)
                """))
                @test r.result.success && r.result.data == 4_500_000

                manifest = Palette.snapshot_state!(1)
                @test manifest !== nothing
                state_dir = Palette.STATE_DIR[]
                manifest_path = joinpath(state_dir, "manifest.json")
                @test isfile(manifest_path)
                @test (stat(manifest_path).mode & 0o777) == 0o600

                # Verify data + sidecar files are strictly inside PALETTE_HOME/state with 0o600 perms
                b_big = only(filter(b -> b["name"] == "big_oracle_tensor", manifest["bindings"]))
                @test haskey(b_big, "sidecar")
                sidecar_path = joinpath(state_dir, b_big["sidecar"])
                @test isfile(sidecar_path)
                @test startswith(abspath(sidecar_path), abspath(priv_home) * "/")
                @test (stat(sidecar_path).mode & 0o777) == 0o600
                for g in manifest["data"]
                    fp = joinpath(state_dir, g["file"])
                    @test isfile(fp)
                    @test (stat(fp).mode & 0o777) == 0o600
                end

                # Pinned cache routes strictly to PALETTE_HOME/pinned_cache with 0o600 perms
                @test Palette.default_pinned_cache_dir() == joinpath(abspath(priv_home), "pinned_cache")
                Api.pin_cache!(:private_oracle_weights, Float32[1.5, 2.5, 3.5])
                pc_files = filter(f -> endswith(f, ".jls"), readdir(joinpath(priv_home, "pinned_cache"); join=true))
                @test length(pc_files) == 1
                @test (stat(pc_files[1]).mode & 0o777) == 0o600

                # Ephemeral provenance routes strictly to PALETTE_HOME/provenance with 0o600 perms
                eph = Palette.execute(Palette.EphemeralTool("40 + 2"))
                @test eph.result.success && eph.result.data == 42
                prov_file = joinpath(priv_home, "provenance", "provenance.jsonl")
                @test isfile(prov_file)
                @test (stat(prov_file).mode & 0o777) == 0o600

                # Pharos index cache routes strictly to PALETTE_HOME/pharos with 0o600 perms
                write(joinpath(ws, "oracle.jl"), "oracle_fn(x) = x + 1\n")
                g_priv = Pharos.load_or_build(ws)
                pharos_cache = g_priv.last["cache_path"]
                @test startswith(abspath(pharos_cache), joinpath(abspath(priv_home), "pharos") * "/")
                @test isfile(pharos_cache)
                @test (stat(pharos_cache).mode & 0o777) == 0o600

                # ZERO leakage outside PALETTE_HOME: 0 bytes written to shared locations
                @test !ispath(shared_palette_dir)
                @test !ispath(joinpath(fake_home, ".palette"))
                @test !ispath(joinpath(fake_home, ".local", "share", "operator-surfaces"))
                @test !ispath(joinpath(ws, ".palette"))

                # Durable reload across fresh KernelState (simulating full restart)
                Palette.reset_kernel_state()
                report = Palette.revive_state!()
                @test occursin("restored exactly", report)
                @test occursin("oracle_secret", report)
                @test occursin("big_oracle_tensor", report)
                mod = Palette.get_kernel_state().eval_module
                @test Base.invokelatest(getglobal, mod, :oracle_secret) == 987654321
                revived_big = Base.invokelatest(getglobal, mod, :big_oracle_tensor)
                @test length(revived_big) == 4_500_000 && revived_big[1] == Int32(17)
                @test Api.get_cache(:private_oracle_weights) == Float32[1.5, 2.5, 3.5]
            end
        finally
            Palette.reset_kernel_state()
            Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
            Palette.INITIAL_ENV[] = old_init_env
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "Part A.2: /proc-based unboxed same-user builder detection (Palette.check_private_isolation)" begin
        tmp = mktempdir()
        fake_proc = joinpath(tmp, "proc")
        mkpath(fake_proc)

        function add_fake_proc(pid::Int, comm::String, uid::Int, mnt_ns::String)
            pdir = joinpath(fake_proc, string(pid))
            mkpath(joinpath(pdir, "ns"))
            write(joinpath(pdir, "comm"), comm * "\n")
            write(joinpath(pdir, "status"), "Name:\t$(comm)\nUid:\t$(uid)\t$(uid)\t$(uid)\t$(uid)\n")
            symlink(mnt_ns, joinpath(pdir, "ns", "mnt"))
        end

        try
            own_pid = 2000
            own_uid = 1005
            own_ns = "mnt:[4026531840]"

            # Self process (ignored even with builder comm)
            add_fake_proc(own_pid, "gemini", own_uid, own_ns)
            # Non-builder same UID + same ns (ignored)
            add_fake_proc(2001, "julia", own_uid, own_ns)
            # Boxed builder in builder-box (same UID, DIFFERENT mount namespace -> safe)
            add_fake_proc(2002, "grok", own_uid, "mnt:[4026539991]")
            # Separate Linux user builder (DIFFERENT UID, same mount namespace -> safe)
            add_fake_proc(2003, "antigravity", 2099, own_ns)

            clean = Palette.check_private_isolation(;
                allow_unboxed=false,
                proc_root=fake_proc,
                own_pid=own_pid,
                own_uid=own_uid,
                own_mnt_ns=own_ns,
            )
            @test clean.isolated === true
            @test isempty(clean.unboxed_builders)
            @test length(clean.boxed_builders) == 1 && clean.boxed_builders[1].pid == 2002
            @test length(clean.other_uid_builders) == 1 && clean.other_uid_builders[1].pid == 2003

            # Introduce unboxed same-UID same-ns builders (`agy` and `gemini`)
            add_fake_proc(2004, "agy", own_uid, own_ns)
            add_fake_proc(2005, "gemini", own_uid, own_ns)

            # Strict mode (allow_unboxed=false) refuses with clear diagnostic naming PIDs, comms, and both real fixes
            err_msg = try
                Palette.check_private_isolation(;
                    allow_unboxed=false,
                    proc_root=fake_proc,
                    own_pid=own_pid,
                    own_uid=own_uid,
                    own_mnt_ns=own_ns,
                )
                ""
            catch e
                sprint(showerror, e)
            end
            @test occursin("pid=2004 comm=agy", err_msg)
            @test occursin("pid=2005 comm=gemini", err_msg)
            @test occursin("builder-box", err_msg)
            @test occursin("inquisitor", err_msg)

            # Non-strict mode (allow_unboxed=true) emits warning and returns structured report without throwing
            dirty = @test_logs (:warn, r"unboxed same-user builder") Api.check_private_isolation(;
                allow_unboxed=true,
                proc_root=fake_proc,
                own_pid=own_pid,
                own_uid=own_uid,
                own_mnt_ns=own_ns,
            )
            @test dirty.isolated === false
            @test [b.pid for b in dirty.unboxed_builders] == [2004, 2005]
            @test [b.comm for b in dirty.unboxed_builders] == ["agy", "gemini"]

            # Live /proc smoke check
            live = Base.with_logger(Base.NullLogger()) do
                Palette.check_private_isolation(; allow_unboxed=true)
            end
            @test live.own_pid == getpid()
            @test live.own_uid >= 0
            if islink("/proc/self/ns/mnt")
                @test live.own_mnt_ns !== nothing && startswith(live.own_mnt_ns, "mnt:[")
            end
        finally
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "Part B.1: BLAS thread control (Api.set_blas_threads!, PALETTE_BLAS_THREADS, revival)" begin
        reset_kernel_state()
        orig_threads = Api.get_blas_threads()
        old_env_blas = get(ENV, "PALETTE_BLAS_THREADS", nothing)
        old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
        old_init_env = copy(Palette.INITIAL_ENV[])
        ws = mktempdir()
        st = mktempdir()
        try
            @test Api.set_blas_threads!(1) == 1
            @test Api.get_blas_threads() == 1
            @test ENV["PALETTE_BLAS_THREADS"] == "1"

            @test Api.set_blas_threads!(2) == 2
            @test Api.get_blas_threads() == 2
            @test ENV["PALETTE_BLAS_THREADS"] == "2"

            @test_throws ArgumentError Api.set_blas_threads!(0)
            @test_throws ArgumentError Api.set_blas_threads!(-3)

            # Works inside a Palette turn
            r_turn = Palette.execute(Palette.ExecuteCode("Api.set_blas_threads!(1); Api.get_blas_threads()"))
            @test r_turn.result.success && r_turn.result.data == 1

            # Workspace PALETTE_BLAS_THREADS=1 applies on boot AND survives revival
            Palette.WORKSPACE_ROOT[] = ws
            Palette.STATE_DIR[] = st
            withenv("PALETTE_BLAS_THREADS" => "1") do
                Palette.INITIAL_ENV[] = Dict{String, String}(ENV)
                @test Palette.apply_configured_blas_threads!() == 1
                @test Api.get_blas_threads() == 1
                Palette.execute(Palette.ExecuteCode("bench_marker = 42"))
                @test Palette.snapshot_state!(1) !== nothing

                # Simulate fresh kernel booting with different BLAS count before revival
                Palette.reset_kernel_state()
                Api.set_blas_threads!(2)
                ENV["PALETTE_BLAS_THREADS"] = "1"
                @test Api.get_blas_threads() == 2
                report = Palette.revive_state!()
                @test occursin("bench_marker", report)
                @test Api.get_blas_threads() == 1
            end
        finally
            Palette.reset_kernel_state()
            Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
            Palette.INITIAL_ENV[] = old_init_env
            Api.set_blas_threads!(max(1, orig_threads))
            if old_env_blas === nothing
                delete!(ENV, "PALETTE_BLAS_THREADS")
            else
                ENV["PALETTE_BLAS_THREADS"] = old_env_blas
            end
            rm(ws; recursive=true, force=true)
            rm(st; recursive=true, force=true)
        end
    end

    @testset "Part B.3: Api.serve_unix_socket warm-worker AF_UNIX server" begin
        tmp = mktempdir()
        sock = joinpath(tmp, "warm_worker.sock")
        regular_file = joinpath(tmp, "not_a_socket.txt")
        write(regular_file, "keep me")
        try
            # Refuses to overwrite a non-socket file
            @test_throws ErrorException Api.serve_unix_socket(identity, regular_file)
            @test read(regular_file, String) == "keep me"

            # Create a stale socket file first and verify serve_unix_socket cleans it safely
            stale_srv = Sockets.listen(sock)
            close(stale_srv)
            # Some platforms unlink on close; ensure stale test works even if already unlinked
            h = Api.serve_unix_socket(sock; permissions=0o600) do line
                if line == "boom"
                    error("deliberate handler failure")
                elseif startswith(line, "add ")
                    parts = parse.(Int, split(line)[2:end])
                    return JSON.json(Dict("ok" => true, "sum" => sum(parts)))
                else
                    return "echo:" * line
                end
            end
            @test isopen(h)
            @test (stat(sock).mode & 0o777) == 0o600

            # Refuses to hijack a live active socket
            @test_throws ErrorException Api.serve_unix_socket(identity, sock)

            # Valid request round-trip
            c1 = Sockets.connect(sock)
            println(c1, "add 19 23")
            flush(c1)
            resp1 = JSON.parse(readline(c1))
            @test resp1["ok"] == true && resp1["sum"] == 42

            # Handler error is caught cleanly (structured JSON error, worker stays alive!)
            println(c1, "boom")
            flush(c1)
            resp_err = JSON.parse(readline(c1))
            @test resp_err["ok"] == false
            @test occursin("deliberate handler failure", resp_err["error"])
            @test resp_err["error_type"] == "ErrorException"
            close(c1)

            # Second client connection still succeeds after previous handler exception
            c2 = Sockets.connect(sock)
            println(c2, "hello_neon")
            flush(c2)
            @test readline(c2) == "echo:hello_neon"
            close(c2)

            @test h.requests_handled[] == 3

            # Clean stop! closes socket and removes socket file
            Api.stop!(h)
            @test !isopen(h)
            @test !ispath(sock)

            # max_requests auto-shutdown
            sock2 = joinpath(tmp, "limited.sock")
            h2 = Api.serve_unix_socket(line -> "ok:" * line, sock2; max_requests=2)
            c3 = Sockets.connect(sock2)
            println(c3, "one"); flush(c3); @test readline(c3) == "ok:one"
            println(c3, "two"); flush(c3); @test readline(c3) == "ok:two"
            close(c3)
            deadline = time() + 2.0
            while isopen(h2) && time() < deadline
                sleep(0.01)
            end
            @test !isopen(h2)
            @test h2.requests_handled[] == 2
            Api.stop!(h2)
        finally
            rm(tmp; recursive=true, force=true)
        end
    end

    @testset "Part B.4: Cached fast Pharos index persistence (save_index, load_index, load_or_build)" begin
        tmp = mktempdir()
        repo = joinpath(tmp, "repo")
        cache_path = joinpath(tmp, "cache", "pharos.jls")
        mkpath(joinpath(repo, "src"))
        try
            # Build 60 realistic Julia source files to test incremental cache reload
            for i in 1:60
                write(joinpath(repo, "src", "mod_$(lpad(i, 3, '0')).jl"), """
                module Mod$(i)
                    struct Item$(i)
                        x::Int
                        y::Float64
                    end
                    function compute_$(i)(a::Item$(i))
                        return a.x + Int(round(a.y)) + $(i)
                    end
                    helper_$(i)(v) = compute_$(i)(Item$(i)(v, 1.0))
                end
                """)
            end

            # Cold build + persist
            t0 = time()
            g1 = Pharos.load_or_build(repo; cache_path=cache_path)
            cold_s = time() - t0
            @test g1.last["cache_hit"] === false
            @test length(g1.files) == 60
            @test g1.last["reparsed"] == 60
            @test isfile(cache_path)
            @test (stat(cache_path).mode & 0o777) == 0o600

            # Warm reload: unchanged files skipped via stamp
            t1 = time()
            g2 = Pharos.load_or_build(repo; cache_path=cache_path)
            warm_s = time() - t1
            @test g2.last["cache_hit"] === true
            @test length(g2.files) == 60
            @test g2.last["reparsed"] == 0
            @test g2.last["hashed"] == 0
            @test g2.last["checked"] == 60
            @test Pharos.digest(g2) == Pharos.digest(g1)
            @test warm_s < max(cold_s, 0.25)

            # Edit exactly 1 file -> only 1 file reparsed, 59 skipped
            sleep(0.02)
            write(joinpath(repo, "src", "mod_007.jl"), """
            module Mod7
                struct Item7
                    x::Int
                end
                brand_new_symbol_7(a::Item7) = a.x * 7
            end
            """)
            g3 = Pharos.load_or_build(repo; cache_path=cache_path)
            @test g3.last["cache_hit"] === true
            @test g3.last["reparsed"] == 1
            @test g3.last["checked"] == 59
            @test !isempty(Pharos.definitions(g3, "brand_new_symbol_7"))

            # Subsequent reload sees updated cache with 0 reparses
            g4 = Pharos.load_or_build(repo; cache_path=cache_path)
            @test g4.last["cache_hit"] === true
            @test g4.last["reparsed"] == 0
            @test !isempty(Pharos.definitions(g4, "brand_new_symbol_7"))

            # Wrong expected_root returns nothing
            @test Pharos.load_index(cache_path; expected_root=tmp) === nothing

            # Corrupt cache falls back cleanly to fresh build
            write(cache_path, "not valid serialized data")
            @test Pharos.load_index(cache_path) === nothing
            g5 = Pharos.load_or_build(repo; cache_path=cache_path)
            @test g5.last["cache_hit"] === false
            @test length(g5.files) == 60
        finally
            rm(tmp; recursive=true, force=true)
        end
    end

end
