# Runs only while the package is being precompiled. A new kernel used to
# compile its whole turn path while the model waited for the first reply:
# JSON both ways, execute's success and error paths, output capture, the
# deadline machinery and the shaping of values for the protocol. Running
# representative turns here caches that native code with the package.
#
# Every failure is swallowed: a workload problem must never fail the build,
# only leave some code uncompiled. Turn code runs in WorkloadScope, since
# precompilation refuses evaluation into a module this package does not own,
# and the kernel state is discarded afterwards.
baremodule WorkloadScope
using Base
end
# @compile_workload also keeps what the workload compiles in other packages'
# methods, such as JSON's parser and Base's show methods for common results.
# A bare jl_generating_output block kept only Palette's own.
@compile_workload begin
    try
        GLOBAL_STATE[] = KernelState(WorkloadScope)
        JSON.parse("""{"request_id":"1","kind":"EXECUTE","code":"1 + 1","payload":null,"timeout_s":60}"""; dicttype=Dict{String, Any})
        execute_turn("1", nothing)
        # Requests as the worker serves them: read from a pipe, answered
        # through a function. As script code, the first request compiled all
        # of this, about 1.1 s. The results are those models ask for most.
        let pipe = Base.link_pipe!(Pipe(); reader_supports_async=true, writer_supports_async=true), replies = String[]
            codes = ("1 + 1", "x = [1 2; 3 4]\nf(y) = y .+ 1\nf(x)",
                     "for i in 1:3\n    s = i\nend\nprintln(\"done\")",
                     "d = Dict(\"a\" => 1); (d, :b, 1.5, \"s\", nothing)",
                     "error(\"workload\")", "undefined_name + 1", "g(x::Int) = x\ng(\"s\")", "s = \"\$(1",
                     "\"ok\"", "readdir(tempdir())[1:0]", "Dict(:a => 1)", "rand(3)", "1.5")
            for (i, code) in enumerate(codes)
                request = Dict{String,Any}("request_id" => string(i), "kind" => "EXECUTE", "code" => code)
                i == 1 && (request["timeout_s"] = 60)
                println(pipe.in, JSON.json(request))
            end
            for request in (Dict("request_id" => "p", "kind" => "EXECUTE", "code" => "length(PAYLOAD[\"a\"])",
                                 "payload" => Dict("a" => "t"), "digest" => false),
                            Dict("request_id" => "o", "kind" => "OTHER", "code" => ""))
                println(pipe.in, JSON.json(request))
            end
            println(pipe.in, "not json\n")
            close(pipe.in)
            serve_requests(resp -> push!(replies, safe_json(resp)), pipe.out, () -> false, "e")
            close(pipe)
        end
        # The result's additions: the digest of long output, the jobs line, and
        # the workspace map, which opens a session's first result and took 8.7 s
        # there compiled cold.
        with_digest(repeat("   Compiling dep v0.1.0\n", 200) * "error[E0308]: mismatched types\n --> src/lib.rs:3:5\n" *
                    "a.ts(2,19): error TS2322: x\ntest t ... FAILED\n--- FAIL: TestX (0.00s)\n    x_test.go:3: m\n" *
                    "FAIL: test_a (m.T.test_a)\nFile \"m.py\", line 3, in t\nAssertionError: 1 != 2\n" *
                    "File \"a.ml\", line 1, characters 1-2:\nError: e\n  more\n✖ x (1ms)\n\e[31mred\e[0m\n")
        report_jobs(WorkloadScope)
        sprint(show, MIME"text/plain"(), stress("exit \$((STRESS_RUN % 2))"; n=4, jobs=2, seconds=10))
        let root = mktempdir()
            mkpath(joinpath(root, "src"))
            write(joinpath(root, "src", "lib.rs"), "")
            write(joinpath(root, "Cargo.toml"), "[workspace]\n")
            write(joinpath(root, "package.json"), "{\"scripts\": {\"test\": \"x\"}}")
            write(joinpath(root, "Makefile"), "build:\n\ttrue\n")
            workspace_map(root)
            try
                run(pipeline(`git -C $root init -q`; stdout=devnull, stderr=devnull))
                workspace_map(root)
            catch
            end
            rm(root; recursive=true, force=true)
        end
        # Saving and reviving state: the first snapshot of a new kernel ran
        # this code cold, and a harness closing the session right after the
        # reply killed it before it finished.
        STATE_DIR[] = mktempdir()
        # No definitions: replaying them would redefine WorkloadScope's
        # methods, which precompilation forbids.
        empty!(DEFINITION_LOG)
        snapshot_state!(6; waiting = () -> false)
        revive_state!()
    catch e
        # Everything after the failing line goes uncompiled, and the kernel's first calls are slow.
        @warn "precompile workload stopped early" exception=(e, catch_backtrace())
    finally
        isempty(STATE_DIR[]) || rm(STATE_DIR[]; recursive=true, force=true)
        STATE_DIR[] = ""
        REVIVAL_REPORT[] = ""
        revival_observation!()
        LAST_SAVED_CALL[] = 0
        LAST_SNAPSHOT_SECONDS[] = 0.0
        GLOBAL_STATE[] = nothing
        empty!(OUTPUTS)
        empty!(WORKSPACE_PACKAGES)
        empty!(WORKSPACE_DECLARATIONS)
        empty!(RELOAD_REPORTS)
        RELOAD_SEQUENCE[] = 0
        empty!(USED_FILES)
        empty!(DEFINITION_LOG)
        empty!(CALL_FILES)
        foreach(f -> rm(f[1]; force=true), LATE_FILES)
        empty!(LATE_FILES)
        empty!(BINDING_SEEN)
        empty!(BINDING_ORIGINS)
        empty!(BINDING_IDENTITIES)
        OBSERVATION_ACK[] = nothing
        empty!(TURN_COSTS)
        empty!(LAST_RUNNING[])
        empty!(REPORTED_TASKS)
        empty!(FINISH_REPORTED)
        empty!(HOST_JOBS); empty!(HOST_JOB_RESULTS); empty!(HOST_JOB_RESULT_ORDER)
        empty!(HOST_FINISH_REPORTED)
        empty!(TASK_OBSERVATIONS)
        TASK_SEQUENCE[] = 0
        TASK_EPOCH[] = ""
    end
    precompile(Base.open, (Base.RawFD,))
end
