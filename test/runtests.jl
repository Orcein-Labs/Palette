using Test
using Palette
using JSON
using Sockets
using SHA

# A stand-in for the broker and the agent host behind it: each connection gets
# one JSON request; `reply` maps (type, payload) to the broker's response.
function with_fake_broker(f, reply)
    dir = mktempdir(); path = joinpath(dir, "broker.sock")
    server = Sockets.listen(path)
    seen = Any[]
    task = @async while isopen(server)
        conn = try accept(server) catch; break end
        try
            req = JSON.parse(readline(conn))
            push!(seen, req)
            type, payload = req["category"] == "host_request" ? (req["params"]["type"], req["params"]["payload"]) : (req["category"], req["params"])
            write(conn, JSON.json(reply(type, payload)) * "\n")
        finally
            close(conn)
        end
    end
    old = get(ENV, "PALETTE_BROKER_SOCKET", nothing)
    ENV["PALETTE_BROKER_SOCKET"] = path
    try
        f(seen)
    finally
        old === nothing ? delete!(ENV, "PALETTE_BROKER_SOCKET") : (ENV["PALETTE_BROKER_SOCKET"] = old)
        close(server)
    end
end
ok(result) = Dict("approved" => true, "result" => Dict("status" => "ok", "result" => result))

@testset "RLM from Julia" begin
    R = Palette.Api.rlm
    child = Dict("rlm_child_id" => "c1", "name" => "w", "session_dir" => "/s/c1", "model" => "openai/luna")
    with_fake_broker((t, p) -> t == "rlm.run" ? ok(child) :
                                t == "rlm.collect" ? ok(Dict("results" => [Dict("rlm_child_id" => "c1", "status" => "done",
                                    "settled" => true, "answer_preview" => "42", "session_name" => "w")])) :
                                t == "rlm.list_subagents" ? ok(Dict("subagents" => [Dict("rlm_child_id" => "c1",
                                    "session_name" => "w", "session_dir" => "/s/c1", "status" => "running",
                                    "activity" => Dict("kind" => "executing", "tool_name" => "palette"))])) :
                                t == "rlm.find_models" ? ok(Dict("models" => [Dict("provider" => "openai", "id" => "luna",
                                    "name" => "Luna", "selector" => "openai/luna")])) :
                                t == "rlm.progress.note" ? ok(Dict("accepted" => false, "retry_after_ms" => 900)) :
                                t == "rlm.delete_subagent" ? ok(Dict("subagent" => Dict("rlm_child_id" => "c1",
                                    "session_name" => "w", "session_dir" => "/s/c1", "status" => "completed"))) :
                                Dict("approved" => false, "reason" => "not allowed")) do seen
        h = R.spawn("add 40 and 2"; name = "w", thinking = "low")
        @test h isa R.SpawnHandle && h.rlm_child_id == "c1" && h.model == "openai/luna"
        @test seen[end]["category"] == "host_request"
        @test seen[end]["params"]["payload"] == Dict("prompt" => "add 40 and 2", "kwargs" => Dict("name" => "w", "thinking" => "low"))
        r = only(R.collect(h))
        @test r.status == "done" && r.settled && r.answer_preview == "42" && r.error === nothing
        @test seen[end]["params"]["payload"]["targets"] == ["c1"]
        @test R.collect()[1].rlm_child_id == "c1" && seen[end]["params"]["payload"]["targets"] == []
        sa = only(R.list_subagents())
        @test sa.status == "running" && sa.activity.kind == "executing"
        @test R.collect([h, sa, " w "]; timeout_ms = 10)[1].settled
        @test seen[end]["params"]["payload"]["targets"] == ["c1", "c1", "w"]
        @test only(R.find_models("luna")).selector == "openai/luna"
        n = R.progress_note("  halfway  ")
        @test !n.accepted && n.retry_after_ms == 900 && seen[end]["params"]["payload"]["message"] == "halfway"
        @test R.delete_subagent(h).status == "completed"
        # The same checks and messages as the Python kernel.
        @test_throws "timeout_ms must be a non-negative int" R.collect(h; timeout_ms = -1)
        @test_throws "timeout_ms must be at most 50000" R.collect(h; timeout_ms = 60_000)
        @test_throws "targets must be nothing, a target, or a vector" R.collect(42)
        @test_throws "collect target must be" R.collect([h, 42])
        @test_throws "message must not be empty" R.progress_note("   ")
        @test_throws "at most 512 characters" R.progress_note("😀"^300)   # 600 UTF-16 units
        @test_throws "target must not be empty" R.delete_subagent(" ")
        # Denied by policy: the broker's reason reaches the caller.
        @test_throws "was not allowed: not allowed" R.create_session("x")
    end
    # The host's own error, and malformed replies.
    with_fake_broker((t, p) -> t == "rlm.run" ? Dict("approved" => true, "result" => Dict("status" => "error", "error" => "name 'w' is taken")) :
                               ok(Dict("results" => "nope"))) do _
        @test_throws "name 'w' is taken" R.spawn("x"; name = "w")
        @test_throws "rlm.collect returned an invalid results list" R.collect()
    end
    with_fake_broker((t, p) -> ok(Dict("rlm_child_id" => "c1"))) do _
        @test_throws "rlm.spawn returned an invalid spawn handle" R.spawn("x"; name = "w")
    end
end

@testset "Stress runner" begin
    r = Palette.stress("[ \$((STRESS_RUN % 13)) -ne 0 ] || { echo boom at \$STRESS_RUN; exit 3; }"; n=100, jobs=8)
    @test r.runs == 100 && r.outcomes == Dict(0 => 93, 3 => 7)
    @test r.first_failure[1] == 13 && occursin("boom at 13", r.first_failure[3])
    text = sprint(show, MIME"text/plain"(), r)
    @test occursin("100 runs in", text) && occursin("7 failed (exit codes: 0×93, 3×7)", text) && occursin("STRESS_RUN=13", text)
    ok = Palette.stress("true"; n=10, jobs=2)
    @test ok.first_failure === nothing && occursin("0 failed", sprint(show, MIME"text/plain"(), ok))
    slow = Palette.stress("sleep 1"; n=100, jobs=2, seconds=2)
    @test 0 < slow.runs < 100 && occursin("of 100 runs", sprint(show, MIME"text/plain"(), slow))
end

# Run before "EphemeralTool isolation", whose same-process case pirates Base methods for the rest of the process.
@testset "Output digest" begin
    fx(name) = read(joinpath(@__DIR__, "fixtures", "digest", name), String)
    # Real tool output, captured from each toolchain, after a long build log.
    noise = join(["   Compiling dep-$i v0.1.$i (registry)" for i in 1:150], "\n") * "\n"
    head(name) = first(split(Palette.with_digest(noise * fx(name)), "\n\n"))
    @test startswith(head("cargo_build.txt"), "[digest of 185 lines of output: 3 errors]")
    @test occursin("src/main.rs:3:18  error[E0308]: mismatched types", head("cargo_build.txt"))
    @test occursin("2 failing tests", head("cargo_test.txt"))
    @test occursin("src/main.rs:4:23  test tests::adds panicked", head("cargo_test.txt"))
    @test occursin("a.ts:4:9  error TS2304: Cannot find name 'missing'.", head("tsc.txt"))
    @test occursin("a.ts:2:19  error TS2322", head("tsc_pretty.txt"))   # ANSI-coloured
    @test occursin("bin/m.ml:1  error This constant has type string but an expression was expected of type int", head("dune.txt"))
    @test occursin("./m.go:2:14  error declared and not used: x", head("go_build.txt"))
    @test occursin("m_test.go:3  test TestAdd: Add(2,2)=0", head("go_test.txt"))
    @test occursin("test_x.py:3  test test_x.T.test_a: AssertionError: 2 != 3", head("py_unittest.txt"))
    @test occursin("1 failing test]", head("node_test.txt"))
    # The whole output still follows the digest.
    @test endswith(Palette.with_digest(noise * fx("cargo_build.txt")), fx("cargo_build.txt"))
    # Short output, and long output with nothing failing, are left as they are.
    @test Palette.with_digest(fx("tsc.txt")) == fx("tsc.txt")
    @test Palette.with_digest(noise) == noise
    @test Palette.with_digest("\e[31mred\e[0m plain") == "red plain"
    # A flood of failures is capped.
    flood = noise * join(["test t$i ... FAILED" for i in 1:40], "\n")
    @test occursin("… and 28 more", Palette.with_digest(flood))
end

@testset "Workspace map" begin
    root = mktempdir()
    @test occursin("the workspace is empty", Palette.workspace_map(root))
    mkpath(joinpath(root, "core", "src")); mkpath(joinpath(root, "api", "src")); mkpath(joinpath(root, "node_modules", "x"))
    write(joinpath(root, "Cargo.toml"), "[workspace]\nmembers = [\"core\"]\n")
    write(joinpath(root, "core", "src", "lib.rs"), "pub fn f() {}\n")
    write(joinpath(root, "api", "package.json"), "{\"scripts\": {\"build\": \"tsc\", \"test\": \"node --test\"}}")
    write(joinpath(root, "api", "src", "index.ts"), "export const x = 1\n")
    write(joinpath(root, "node_modules", "x", "a.js"), "")
    write(joinpath(root, "Makefile"), "build:\n\tcargo build\ntest: build\n\tcargo test\n.PHONY: build test\n")
    m = Palette.workspace_map(root)
    @test startswith(m, "[workspace map] 5 files")          # node_modules left out
    @test occursin("Rust 1", m) && occursin("TypeScript 1", m) && !occursin("JavaScript", m)
    @test occursin("cargo: Cargo.toml (workspace)", m)
    @test occursin("npm: api/package.json scripts: build, test", m) || occursin("npm: api/package.json scripts: test, build", m)
    @test occursin("Makefile: Makefile targets: build, test", m)
    @test length(m) <= Palette.MAP_MAX_CHARS + 2
end

@testset "KernelState" begin
    reset_kernel_state()
    state = get_kernel_state()

    @test state isa KernelState
    @test !isnothing(state.id)
    @test !isnothing(state.created_at)
    @test isempty(state.variables)
    @test isempty(state.execution_history)
end

@testset "Bounded nested results (field B03)" begin
    @test Palette.bounded_data((method_list=methods(Palette.Api.output),)) isa AbstractString
    normal = (values=[[1, 2], [3, 4]], label="λ")
    @test Palette.bounded_data(normal) === normal
    cycle = Any[]; push!(cycle, cycle)
    @test Palette.bounded_data(cycle) isa AbstractString
    for scalar in (Symbol(repeat("q", 2_000_000)), repeat("\0", Palette.MAX_DATA_JSON_BYTES))
        projected = Palette.bounded_data(scalar)
        @test projected isa AbstractString && projected !== scalar && ncodeunits(projected) <= Palette.MAX_OUTPUT_BYTES + 64
    end
end

@testset "ExecuteCode Operation" begin
    reset_kernel_state()

    op = ExecuteCode("x = 42")
    receipt = execute(op)

    @test receipt isa OperationReceipt
    @test receipt.operation_type == "ExecuteCode"
    @test receipt.result.success
    @test receipt.result.data == 42
    @testset "Whole-call parse gate (field B01)" begin
        execute(ExecuteCode("parse_guard = Ref(7)"))
        for tail in ("function incomplete(", ")", "begin\n1")
            bad = execute(ExecuteCode("parse_guard[] = 99\n" * tail))
            @test !bad.result.success && occursin("ParseError", bad.result.error)
            @test execute(ExecuteCode("parse_guard[]")).result.data == 7
        end
        @test execute(ExecuteCode("parse_guard[] = 8; parse_guard[]")).result.data == 8
        @test !execute(ExecuteCode("parse_guard[] = 9; error(\"runtime failure\")")).result.success
        @test execute(ExecuteCode("parse_guard[]")).result.data == 9
        @test execute(ExecuteCode("Expr(:incomplete, :literal)")).result.success
    end
end

@testset "Phase 1: real cross-request state persistence" begin
    reset_kernel_state()

    execute(ExecuteCode("x = 41"))
    r = execute(ExecuteCode("x + 1"))
    @test r.result.success
    @test r.result.data == 42

    execute(ExecuteCode("square(n) = n * n"))
    r2 = execute(ExecuteCode("square(7)"))
    @test r2.result.success
    @test r2.result.data == 49

    execute(ExecuteCode("buf = Int[]"))
    r3 = execute(ExecuteCode("push!(buf, 1); push!(buf, 2); buf"))
    @test r3.result.success
    @test r3.result.data == [1, 2]

    # A thrown exception must not corrupt the session's persistent state.
    r4 = execute(ExecuteCode("error(\"boom\")"))
    @test !r4.result.success
    r5 = execute(ExecuteCode("x"))
    @test r5.result.success
    @test r5.result.data == 41
end

@testset "GetState Operation" begin
    reset_kernel_state()

    execute(ExecuteCode("test_var = 123"))

    op = GetState()
    response = execute(op)

    @test response isa StructuredResponse
    @test response.result.success
    @test "test_var" in response.result.data["variables"]

    keyed = execute(GetState("test_var"))
    @test keyed.result.data["exists"]
    @test keyed.result.data["value"] == 123
end

@testset "OperatorVocabulary" begin
    @test CODE_EXECUTION_VOCAB isa OperatorVocabulary
    @test CODE_EXECUTION_VOCAB.name == "CodeExecution"

    @test OPERATOR_INVOCATION_VOCAB isa OperatorVocabulary
    @test STATE_QUERY_VOCAB isa OperatorVocabulary
    @test isempty(OperatorVocabulary("minimal", "default schemas", String[]).input_schema)
    @test isempty(OperatorVocabulary("input", "default output", String[], Dict{String,Any}()).output_schema)
end

@testset "InvokeOperator (real registered function)" begin
    reset_kernel_state()
    state = get_kernel_state()
    register_operator!(state, "add_one", args -> args["n"] + 1)

    receipt = execute(InvokeOperator("add_one"; args=Dict("n" => 41)))
    @test receipt.result.success
    @test receipt.result.data == 42
    # the caller gets the value; the log keeps only the outcome
    @test state.receipt_log[end].result.data === nothing
    @test state.receipt_log[end].result.success
    @test state.receipt_log[end].metadata["operator"] == "add_one"

    missing_receipt = execute(InvokeOperator("does_not_exist"))
    @test !missing_receipt.result.success
end

@testset "ShellEscape (real subprocess)" begin
    reset_kernel_state()
    receipt = execute(ShellEscape("echo hello"))
    @test receipt.result.success
    @test strip(receipt.result.data["stdout"]) == "hello"
    @test receipt.result.data["exit_code"] == 0
    logged = get_kernel_state().receipt_log[end]
    @test logged.result.data === nothing && logged.result.success
    @test logged.metadata["command"] == "echo hello"

    fail_receipt = execute(ShellEscape("exit 3"))
    @test !fail_receipt.result.success
    @test fail_receipt.result.data["exit_code"] == 3
end

@testset "fs_digest broker request" begin
    # Category-aware fake broker: with_fake_broker maps host_request-shaped
    # params; fs_digest needs the raw category, so this test runs its own
    # minimal broker over the same one-JSON-line protocol.
    reset_kernel_state()
    dir = mktempdir()
    path = joinpath(dir, "watched.txt")
    write(path, "independently observed\n")
    server = Sockets.listen(joinpath(dir, "b.sock"))
    seen = Any[]
    task = @async while isopen(server)
        conn = try Sockets.accept(server) catch; break end
        req = JSON.parse(readline(conn))
        push!(seen, req)
        resp = if req["category"] == "fs_digest" && req["params"]["paths"] == [path]
            Dict("id" => req["id"], "approved" => true,
                 "result" => Dict("digests" => Dict(path => Dict("sha256" => "ff"^32)),
                                  "observer" => "host", "algorithm" => "sha256"))
        else
            Dict("id" => req["id"], "approved" => false, "reason" => "not in ceiling")
        end
        write(conn, JSON.json(resp) * "\n")
        close(conn)
    end
    old = get(ENV, "PALETTE_BROKER_SOCKET", nothing)
    ENV["PALETTE_BROKER_SOCKET"] = joinpath(dir, "b.sock")
    try
        result = Palette.fs_digest([path])
        @test result["observer"] == "host"
        @test result["digests"][path]["sha256"] == "ff"^32
        @test seen[1]["category"] == "fs_digest"
        @test seen[1]["params"]["paths"] == [path]
        @test_throws ErrorException Palette.fs_digest([joinpath(dir, "other.txt")])
    finally
        old === nothing ? delete!(ENV, "PALETTE_BROKER_SOCKET") : (ENV["PALETTE_BROKER_SOCKET"] = old)
        close(server)
    end
end

@testset "host_command vector args and completed result cache" begin
    job, polls = "host-command-vector", Ref(0)
    okay, denied = result -> Dict("approved" => true, "result" => result), Dict("approved" => false, "reason" => "not in ceiling")
    reply = function (_, payload)
        get(payload, "name", "") == "denied" && return denied
        if payload["action"] == "start"
            return okay(Dict("job" => job, "running" => true))
        elseif payload["action"] == "poll" && payload["job"] == job
            polls[] += 1
            return okay(polls[] == 1 ? Dict("job" => job, "running" => true, "stdout" => "first\n", "stderr" => "") :
                Dict("job" => job, "running" => false, "stdout" => "last\n", "stderr" => "warn\n",
                     "stdout_tail" => "first\nlast\n", "stderr_tail" => "warn\n"))
        end
        okay(Dict("returncode" => 0, "stdout" => "", "stderr" => ""))
    end
    try
        with_fake_broker(reply) do seen
            @test (Palette.host_command(job, AbstractString[SubString("git", 1, 3), "π"]; wait=false)["job"],
                   Palette.Api.host_command(job, String[]; wait=false)["job"]) == (job, job)
            @test [(r["params"]["args"], r["params"]["cwd"]) for r in seen[1:2]] == [(["git", "π"], pwd()), (String[], pwd())]
            first, finished = Palette.host_command_poll(job), Palette.host_command_poll(job)
            @test first["stdout"] == "first\n" && finished["running"] === false && finished["stdout"] == "last\n" && finished["stderr"] == "warn\n"
            @test finished["stdout_tail"] == "first\nlast\n" && finished["stderr_tail"] == "warn\n" && Palette.host_command_poll(job) == finished && length(seen) == 4
            @test_throws ErrorException Palette.Api.host_command("denied", AbstractString["π"]); @test_throws MethodError Palette.host_command("invalid", Any["x"])
        end
    finally
        pop!(Palette.HOST_JOBS, job, nothing); pop!(Palette.HOST_JOB_RESULTS, job, nothing); filter!(id -> id != job, Palette.HOST_JOB_RESULT_ORDER)
    end
end

@testset "ShellEscape typed errors + output digests" begin
    reset_kernel_state()

    clean = execute(ShellEscape("exit 3"))
    @test clean.result.data["error_class"] == "clean_failure"
    @test !clean.result.success
    @test clean.metadata["error_class"] == "clean_failure"
    @test clean.metadata["working_dir"] == pwd()

    tout = execute(ShellEscape("sleep 5"; timeout_ms = 80))
    @test tout.result.data["error_class"] == "timeout"
    @test !tout.result.success

    dir = mktempdir(); rm(dir; force = true)
    spawn = execute(ShellEscape("pwd"; working_dir = dir))
    @test spawn.result.data["error_class"] == "spawn_error"
    @test !spawn.result.success

    dir2 = mktempdir(); script = joinpath(dir2, "killself.sh")
    write(script, "#!/bin/sh\nkill -9 \$\$\n")
    sig = execute(ShellEscape("exec sh '$script'"))
    @test sig.result.data["error_class"] == "signal_death"
    @test sig.result.data["exit_code"] == 137
    @test execute(ShellEscape("exit 137")).result.data["error_class"] == "clean_failure"

    ok = execute(ShellEscape("printf hi"))
    @test ok.result.success
    @test !haskey(ok.result.data, "error_class")
    @test ok.result.data["stdout_sha256"] == bytes2hex(sha256("hi")) # printf adds no newline
    @test ok.result.data["stderr_sha256"] == bytes2hex(sha256(""))
end

@testset "ShellEscape (signal death reports failure)" begin
    # Julia's Process.exitcode is 0 alongside a nonzero
    # termsignal, so a SIGKILLed shell used to be reported as success with
    # exit_code 0. It must report failure with 128 + signal, like a shell.
    reset_kernel_state()
    dir = mktempdir()
    script = joinpath(dir, "killself.sh")
    write(script, "#!/bin/sh\nkill -9 \$\$\n")
    receipt = execute(ShellEscape("sh '$script'"))
    @test !receipt.result.success
    @test receipt.result.data["exit_code"] == 137 # 128 + SIGKILL(9)
end

@testset "DiscoveryService" begin
    reset_kernel_state()

    service = DiscoveryService()
    result = discover(service)

    @test result isa IntrospectionResult
    @test length(result.available_operators) > 0
    @test length(result.vocabularies) > 0
end

@testset "OperationReceipts" begin
    reset_kernel_state()
    state = get_kernel_state()

    execute(ExecuteCode("a = 1"))
    execute(ExecuteCode("b = 2"))
    # GetState returns a StructuredResponse, not an OperationReceipt -- by
    # design it's a pure query and doesn't append to receipt_log (receipts
    # are for auditing state-changing/effectful operations).
    execute(GetState())

    @test length(state.receipt_log) == 2

    receipt = state.receipt_log[1]
    @test !isnothing(receipt.id)
    @test !isnothing(receipt.timestamp)
    @test !isnothing(receipt.operation_type)
end

@testset "SafetyGuard" begin
    guard = SafetyGuard(5000, 1024, Set{DataType}([ExecuteCode, GetState]), [r"rm -rf", r":\(\)\{:\|:&\};:"])

    @test guard.max_execution_time_ms == 5000
    @test any(p -> p.pattern == "rm -rf", guard.blocked_patterns)

    reset_kernel_state()
    allowed = safe_execute(ExecuteCode("1 + 1"), guard)
    @test allowed isa OperationReceipt
    @test allowed.result.success
    @test allowed.result.data == 2

    disallowed = safe_execute(ShellEscape("echo nope"), guard)
    @test disallowed isa StructuredResponse
    @test !disallowed.result.success

    blocked = safe_execute(ExecuteCode("run(`rm -rf /`)"), guard)
    @test blocked isa StructuredResponse
    @test !blocked.result.success
end

@testset "ErrorHandler" begin
    handler = ErrorHandler(:log; max_retries=5)

    @test handler.strategy == :log
    @test handler.max_retries == 5
end

@testset "Demo Functions" begin
    state = demo_setup()
    @test state isa KernelState

    result = run_demo()
    @test result isa IntrospectionResult
end

@testset "EphemeralTool isolation" begin
    reset_kernel_state()
    state = get_kernel_state()

    # ordinary ephemeral code works and does not leak into the persistent
    # session's own eval_module
    r1 = execute(EphemeralTool("helper(x) = x * 2; helper(21)"))
    @test r1.result.success
    @test r1.result.data == 42
    @test !isdefined(state.eval_module, :helper)
    @test state.receipt_log[end].result.data === nothing
    @test state.receipt_log[end].metadata["tool_id"] == r1.metadata["tool_id"]

    # ephemeral code reaches authority through Palette, as turn code does,
    # and nothing else of the package
    @test execute(EphemeralTool("Palette.request_capability isa Function")).result.data === true
    @test execute(EphemeralTool("Api === Palette")).result.data === true
    @test execute(EphemeralTool("isdefined(Palette, :Pharos)")).result.data === true
    @test execute(EphemeralTool("Palette.Pharos isa Module")).result.data === true
    @test !execute(EphemeralTool("Palette.reset_kernel_state()")).result.success

    # `import Mod: f` then a bare `function f(...)` is the confirmed real
    # way to permanently extend a foreign generic function -- must be
    # rejected before eval, not cleaned up after (Julia has no method
    # deletion). Checked against REAL Base.show behavior, not the tool's
    # own self-reported success/failure.
    r2 = execute(EphemeralTool("import Base: show; function show(io::IO, x::Int); print(io, \"pwned\"); end"))
    @test !r2.result.success
    @test r2.result.error !== nothing && occursin("import", r2.result.error)
    @test sprint(show, 42) == "42"

    # a fully qualified `function Mod.name(...)` extends the same way with
    # no `import` at all -- confirmed separately, must also be rejected
    r3 = execute(EphemeralTool("function Base.show(io::IO, x::Int); print(io, \"pwned2\"); end"))
    @test !r3.result.success
    @test sprint(show, 42) == "42"

    # the same qualified attempt, but joined onto a prior statement with a
    # semicolon -- regression case: semicolon-joined statements parse as a
    # NESTED :toplevel Expr that an earlier version of this check never
    # recursed into, letting this exact shape bypass the guard silently
    r3b = execute(EphemeralTool("x = 1; function Base.show(io::IO, x::Int); print(io, \"pwned3\"); end"))
    @test !r3b.result.success
    @test sprint(show, 42) == "42"

    # a qualified def NESTED inside a function body -- regression case:
    # an earlier version of this check only inspected top-level statements
    # and their immediate :block/:toplevel children, missing anything
    # nested one level deeper (inside a function body, let, if, for, ...)
    r3c = execute(EphemeralTool("function outer(); function Base.show(io::IO, x::Int); print(io, \"n\"); end; end"))
    @test !r3c.result.success
    @test sprint(show, 42) == "42"

    # `Base.include(...)` (qualified) loads and runs a file's contents
    # with zero syntactic trace in the SUBMITTED source -- must be
    # rejected outright, since there is nothing in the ephemeral tool's
    # own source for this check to inspect once the file is loaded
    mktemp() do path, io
        write(io, "import Base: show; function show(io::IO, x::Int); print(io, \"file-pwned\"); end")
        close(io)
        r3d = execute(EphemeralTool("Base.include(@__MODULE__, \"$(path)\")"))
        @test !r3d.result.success
        @test sprint(show, 42) == "42"
    end

    # KNOWN, DOCUMENTED, UNCLOSABLE GAP -- not a regression, a permanent
    # fact about running inside a language with reflective eval: a value
    # only ASSEMBLED at runtime (here, a quoted Expr passed to Core.eval)
    # has no syntactic trace in the submitted source at all, so no
    # parse-time check can see it coming. This test exists so that a
    # future change silently "closing" this without updating
    # check_ephemeral_source!'s own docstring (which says explicitly this
    # is a hygiene check, not a security boundary) gets caught by a test
    # failure instead of quietly changing the security claim being made.
    # Real isolation for genuinely untrusted code is spawn_child_worker
    # (OS-level, already adversarially proven), not this same-process guard.
    r3e = execute(EphemeralTool("Core.eval(Base, :(function show(io::IO, x::Int); print(io, \"PWNED2\"); end))"))
    @test r3e.result.success  # succeeds -- this IS the documented open gap
    @test sprint(show, 42) != "42"  # Base.show really is compromised at this point
    # Undo it: `Base.delete_method` removes the injected method and integers
    # print through Base's own again. Leaving it in place made every later
    # test in this process print integers as "PWNED2" (Palette seam S-7: it
    # broke Expr round-trip checks and the test summary's own counts). Never
    # run this pattern against a persistent session worth continuing to trust.
    Base.delete_method(which(show, (IO, Int)))
    @test sprint(show, 42) == "42"

    # `using` alone (no import) remains fully unrestricted -- ephemeral
    # tools keep unbounded power to USE anything, only extension is guarded
    r4 = execute(EphemeralTool("using Statistics; Statistics.mean([1.0, 2.0, 3.0])"))
    @test r4.result.success
    @test r4.result.data == 2.0

    # two ephemeral tools do not share state with each other either
    execute(EphemeralTool("z = 999"))
    r5 = execute(EphemeralTool("!@isdefined(z)"))
    @test r5.result.success
    @test r5.result.data == true

    # every retirement leaves a mechanical, content-addressed provenance
    # record -- cold by default (a file on disk), never preloaded
    prov_path = joinpath(pwd(), ".palette", "provenance.jsonl")
    @test isfile(prov_path)
    lines = readlines(prov_path)
    @test length(lines) >= 5
    last_record = JSON.parse(lines[end])
    @test last_record["schema"] == "palette.tool_capsule.v1"
    @test haskey(last_record, "source_hash")
    @test length(last_record["source_hash"]) == 64  # sha256 hex
    @test last_record["disposition"] == "RETIRED"  # this one succeeded

    # regression: disposition used to be hardcoded "RETIRED" regardless of
    # result.success, so a failed ephemeral tool's capsule was
    # indistinguishable from a successful one in the provenance log
    execute(EphemeralTool("error(\"deliberate failure\")"))
    failed_record = JSON.parse(readlines(prov_path)[end])
    @test failed_record["success"] == false
    @test failed_record["disposition"] == "FAILED"

    rm(joinpath(pwd(), ".palette"); recursive=true, force=true)
end

@testset "Session module helpers" begin
    reset_kernel_state()
    run(code) = execute(ExecuteCode(code)).result

    # bash returns the whole result, not only the exit code
    r = run("r = bash(\"echo out; echo err >&2; exit 4\")")
    @test r.success
    @test r.data isa Palette.ShellResult
    @test (r.data.exitcode, r.data.stdout, r.data.stderr) == (4, "out\n", "err\n")
    @test !success(r.data)
    @test run("sh\"x=2; echo \$((x+1))\"").data.stdout == "3\n"

    # ans holds the last call's value
    run("40 + 2")
    @test run("ans").data == 42

    # varinfo lists the session's own bindings and none of the kernel's
    rows = run("varinfo()").data.rows
    @test "r" in first.(rows)
    @test isempty(intersect(first.(rows), string.(collect(Palette.KERNEL_BINDINGS))))
    @test run("ls isa Function").data
    @test run("Joovy isa Module").data
    @test run("joovy_compile_tiered(\"joovy_t(x)=x+1\"; tier=1, name=:joovy_t)(1)").data == 2

    # turn code reaches authority through Palette, and nothing else of the package
    @test run("Palette.request_capability isa Function").data
    @test !run("Palette.reset_kernel_state()").success

    # the receipt log keeps outcomes, not values
    run("zeros(10^6)")
    @test get_kernel_state().receipt_log[end].result.data === nothing
end

@testset "independent @async bash notifies bash.completed after the turn" begin
    reset_kernel_state()
    is_bash_done(s) = get(get(s, "params", Dict()), "type", nothing) == "bash.completed"
    with_fake_broker((t, p) -> t == "bash.completed" ? ok(Dict()) :
                               Dict("approved" => false, "reason" => "not allowed")) do seen
        (receipt, _, _), _ = Palette.execute_turn("job = @async bash(\"true\")", nothing)
        @test receipt.result.success
        t0 = time()
        while time() - t0 < 5 && !any(is_bash_done, seen)
            sleep(0.05)
        end
        @test any(is_bash_done, seen)
        req = seen[findfirst(is_bash_done, seen)]
        @test req["params"]["payload"]["command"] == "true"
        @test req["params"]["payload"]["exitCode"] == 0
        @test req["params"]["payload"]["pid"] isa Integer && req["params"]["payload"]["pid"] > 0
    end
    with_fake_broker((t, p) -> t == "bash.completed" ? ok(Dict()) :
                               Dict("approved" => false, "reason" => "not allowed")) do seen
        (receipt, _, _), _ = Palette.execute_turn("bash(\"true\")", nothing)
        @test receipt.result.success
        t0 = time()
        while time() - t0 < 0.5
            sleep(0.05)
        end
        @test !any(is_bash_done, seen)
    end
end

@testset "execute_turn names its call while it runs" begin
    # Callers other than a session request (an ephemeral process, the
    # warm-up) get capability requests in their X-Ray too.
    reset_kernel_state()
    traced = Dict{String, Any}("name" => "traced", "source" => "call", "budget" => nothing)
    with_fake_broker((t, p) -> ok(nothing)) do seen
        (receipt, _, _), _ = Palette.execute_turn("Palette.request_capability(\"host_request\", Dict(\"type\" => \"probe\", \"payload\" => nothing)); sum([1, 2])",
                                                  nothing; context=traced)
        @test receipt.result.success
        @test length(seen) == 1
    end
    rec = Palette.XRAY[1]
    @test length(rec["capability_requests"]) == 1 && rec["capability_requests"][1]["approved"] === true
    @test any(d -> d["function"] == "sum", rec["dispatch"])
    @test Palette.CURRENT_CALL[] == 0 && Palette.CALL_CONTEXT[] === nothing
end

@testset "A corrupt checkpoint record is skipped and keeps every copied source" begin
    old_state = Palette.STATE_DIR[]
    Palette.STATE_DIR[] = mktempdir()
    try
        for (name, text) in (("good", """{"name": "good", "created": "2026-01-01T00:00:00Z", "includes": []}"""),
                             ("torn", """{"name": "torn", "crea"""))
            mkpath(joinpath(Palette.checkpoints_dir(), name))
            write(joinpath(Palette.checkpoints_dir(), name, "checkpoint.json"), text)
        end
        mkpath(Palette.blobs_dir())
        write(Palette.blob_path("abc"), "source")
        @test [r["name"] for r in Palette.checkpoint_records()] == ["good"]
        @test Palette.remove_unused_blobs!(Dict{String, Any}[]) == 0
        @test isfile(Palette.blob_path("abc"))
        rm(joinpath(Palette.checkpoints_dir(), "torn"); recursive=true)
        @test Palette.remove_unused_blobs!(Dict{String, Any}[]) == 1
    finally
        Palette.STATE_DIR[] = old_state
    end
end

@testset "The definition index follows appends and a replaced log" begin
    entry(call, kind, names...) = Dict{String, Any}("call" => call, "kind" => kind, "names" => collect(names))
    log = Palette.DEFINITION_LOG
    saved = copy(log)
    try
        empty!(log)
        push!(log, entry(1, "def", "f"), entry(1, "const", "K"))
        ix = Palette.definition_index()
        @test ix.defs == Dict("f" => [1])
        @test ix.by_call[1] == Set(["f", "K"])
        push!(log, entry(2, "def", "f", "g"))
        ix = Palette.definition_index()
        @test ix.defs == Dict("f" => [1, 3], "g" => [3])
        # The same length with other entries, as after a revival.
        empty!(log)
        append!(log, [entry(5, "def", "h"), entry(5, "def", "h"), entry(6, "include", "i")])
        ix = Palette.definition_index()
        @test ix.defs == Dict("h" => [1, 2])
        @test ix.by_call == Dict(5 => Set(["h"]), 6 => Set(["i"]))
    finally
        empty!(log); append!(log, saved)
    end
end

@testset "Revival checks see definitions made earlier in the same call" begin
    # Revival rebuilds definitions and checks them in one call, from a world
    # older than the new bindings. Julia 1.12 only warns about such reads and
    # later versions refuse them; --depwarn=error gives that stricter rule
    # today, under which the checks reported every rebuilt definition lost.
    script = """
    using Palette
    function define_then_check()
        m = Module(:RevivalWorldAge)
        Core.eval(m, :(struct Probe; x::Int; end))
        Core.eval(m, :(probe_f(x::Int) = x))
        (Palette.type_fingerprint(m, "Probe"), Palette.method_set(m, "probe_f"),
         Palette.current_definition(Base.invokelatest(getglobal, m, :Probe)))
    end
    fingerprint, methods_now, current = define_then_check()
    fingerprint == "Probe{}:x::Int64" || error("fingerprint: ", repr(fingerprint))
    methods_now isa Vector && length(methods_now) == 1 || error("methods: ", repr(methods_now))
    current || error("current_definition was false")
    print("ok")
    """
    cmd = `$(Base.julia_cmd()) --startup-file=no --depwarn=error --project=$(dirname(@__DIR__)) -e $script`
    @test readchomp(pipeline(cmd; stderr=stderr)) == "ok"
end

@testset "Pre-HELLO snapshot after started_call without manifest keeps ls" begin
    reset_kernel_state()
    old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
    Palette.WORKSPACE_ROOT[] = mktempdir()
    Palette.STATE_DIR[] = mktempdir()
    try
        Palette.note_started_call!(1, "sleep(999)")
        @test isfile(joinpath(Palette.STATE_DIR[], "started_call"))
        @test !isfile(joinpath(Palette.STATE_DIR[], "manifest.json"))
        Palette.ensure_pre_hello_snapshot!()
        @test isfile(joinpath(Palette.STATE_DIR[], "manifest.json"))
        Palette.reset_kernel_state()
        report = Palette.revive_state!()
        @test !occursin("No complete snapshot remains", report)
        mod = Palette.get_kernel_state().eval_module
        @test isdefined(mod, :ls)
        @test Core.eval(mod, :ls) isa Function
        @test :ls in Palette.KERNEL_BINDINGS
    finally
        Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
    end
end

@testset "Snapshot of a large value stays fast (seam S-2)" begin
    # A non-empty workspace root is what made `check_type` scan every loaded
    # module once per walked object (77 s for 100k objects); ephemeral
    # processes have an empty root and never showed it.
    reset_kernel_state()
    old_root, old_state = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
    Palette.WORKSPACE_ROOT[] = mktempdir()
    Palette.STATE_DIR[] = mktempdir()
    try
        # Not `big`: Julia 1.10 binds a name that a top-level call reads to
        # Base's export before the call runs, then refuses the assignment.
        @test execute(ExecuteCode("rows = Any[Any[Float64(i), Float64(i + 1)] for i in 1:100_000]; length(rows)")).result.data == 100_000
        Palette.snapshot_state!(1)                       # compile once
        started = time()
        manifest = Palette.snapshot_state!(2)
        @test manifest !== nothing
        @test time() - started < 5.0
    finally
        Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[] = old_root, old_state
    end
end

@testset "A definition that does not print back is kept exactly (seam S-3)" begin
    st = Meta.parse("meanrgb(I) = [sum(Float64(I[ch, r, c]) for r in 1:2, c in 1:2 if r == c) for ch in 1:3]")
    @test !Palette.round_trips(st)                       # Julia prints it as $(Expr(:filter, ...))
    @test Palette.round_trips(Meta.parse("f(x) = x + 1"))
    entry = Dict{String,Any}("code" => string(st), "expr_hex" => Palette.expr_hex(st))
    @test Base.remove_linenums!(deepcopy(Palette.logged_statement(entry))) == Base.remove_linenums!(deepcopy(st))
    # Logged as the session loop logs a call: the entry carries the Expr.
    empty!(Palette.DEFINITION_LOG)
    Palette.log_definitions!("meanrgb(I) = [sum(Float64(I[ch, r, c]) for r in 1:2, c in 1:2 if r == c) for ch in 1:3]", 1)
    logged = Palette.DEFINITION_LOG[end]
    @test haskey(logged, "expr_hex")
    @test Base.remove_linenums!(deepcopy(Palette.logged_statement(logged))) == Base.remove_linenums!(deepcopy(st))
end

@testset "Walk verdicts come from the type, cached per walk" begin
    reset_kernel_state()
    mod = get_kernel_state().eval_module
    execute(ExecuteCode("mutable struct WalkBox; v::Any; end"))
    Box = getglobal(mod, :WalkBox)
    cases = Any[
        (1, nothing), ("s", nothing), (big(2)^70, nothing), (r"a+", nothing), (IOBuffer("x"), nothing),
        (Task(() -> 1), "a Task cannot be revived"), (Ptr{Cvoid}(0), "it holds a pointer"),
        (devnull, "an open DevNull cannot be revived"), ([C_NULL], "its elements hold pointers"),
        (Any[1, Task(() -> 2)], "a Task cannot be revived"), (Dict(1 => Channel(1)), "a Channel cannot be revived"),
        (Box(Box(1)), nothing), (sin, nothing), (x -> x + 1, nothing), (Int, nothing),
    ]
    w = Palette.Walk(mod, IdDict{Any,Nothing}(), Set{String}(), Set{UInt}())
    for pass in 1:2                      # the second pass answers from the type cache
        for (v, expected) in cases
            empty!(w.seen)
            @test Palette.refusal(w, v) == expected
        end
    end
    @test "WalkBox" in w.user
    @test w.kinds[Vector{Any}] === :object && w.kinds[Int] === nothing
end

# Julia before 1.12 refuses a struct redefinition that changes the layout.
VERSION >= v"1.12" && @testset "A struct that changes layout names what still uses the old type" begin
    reset_kernel_state()
    printed(code) = mktemp() do path, io
        r = redirect_stdout(() -> execute(ExecuteCode(code)), io)
        flush(io)
        @test r.result.success
        read(path, String)
    end
    @test printed("struct Foo; x::Int; end; a = Foo(1); v = [Foo(2)]; n = 3\n" *
                  "area(f::Foo) = f.x; struct Bar; f::Foo; end") == ""
    # The same layout keeps the same type, so nothing is stale.
    @test printed("struct Foo; x::Int; end") == ""
    notice = printed("struct Foo; x::Int; y::Int; end")
    @test occursin("Foo changed layout", notice)
    @test occursin("values a, v;", notice) && occursin("methods of area;", notice) && occursin("fields of Bar.", notice)
    @test !occursin("n,", notice) && !occursin("ans", notice)
    # Once the old values and methods are rebuilt, the next change has nothing to report on them.
    printed("a = Foo(1, 2); v = [a]; area(f::Foo) = f.x * f.y; struct Bar2; f::Foo; end")
    notice = printed("Base.@kwdef struct Foo; x::Int = 1; y::Int = 2; z::Int = 3; end")
    @test occursin("values a, v;", notice) && occursin("methods of area;", notice) && occursin("fields of Bar2.", notice)
    @test !occursin("Bar,", notice) && !occursin("Bar.", notice)
    # A type nothing refers to changes without a notice.
    @test printed("struct Lone; x::Int; end; struct Lone; x::Float64; end") == ""
end

@testset "OP006 operator job notices contain only owned children" begin
    withenv("PALETTE_MODE" => "operator") do
        child = open(`cat`, "w")
        try
            jobs = Palette.background_processes()
            @test getpid(child) in first.(jobs)
            @test all(jobs) do (pid, _, _)
                stat = read("/proc/$pid/stat", String)
                parse(Int, split(stat[findlast(')', stat)+2:end])[2]) == getpid()
            end
        finally
            close(child.in); wait(child)
        end
    end
end

@testset "Revival persists and replays pure module aliases in prerequisite order" begin
    Palette.reset_kernel_state!(; process=true)
    old_ws, old_st = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
    ws, st, side_log = mktempdir(), mktempdir(), tempname()
    try
        # Unit checks: pure module-alias AST gate rejects side effects and non-paths
        @test Palette.module_alias_parts(:(const BF44 = GrailV46.BF)) == (name=:BF44, rhs=:(GrailV46.BF), is_const=true)
        @test Palette.module_alias_parts(:(const C = A.B.C)) == (name=:C, rhs=:(A.B.C), is_const=true)
        @test Palette.module_alias_parts(:(bf_var = GrailV46.BF)) == (name=:bf_var, rhs=:(GrailV46.BF), is_const=false)
        @test Palette.module_alias_parts(:(const global CG = A.B)) == (name=:CG, rhs=:(A.B), is_const=true)
        @test Palette.module_alias_parts(:(const X = make_something())) === nothing
        @test Palette.module_alias_parts(:(const X = (println(1); GrailV46.BF))) === nothing
        @test Palette.module_alias_parts(:(X = make_mod())) === nothing

        Palette.WORKSPACE_ROOT[] = ws; Palette.STATE_DIR[] = st
        write(joinpath(ws, "inc_aliases.jl"), "module IncMod; module Sub; k() = 7; end; end\nconst INC_SUB = IncMod.Sub\n")
        t1, _ = Palette.execute_turn("""
        module GrailV46; module BF; struct Box; x::Int; end; foo(x) = x + 44; end; end
        module Outer; module Mid; module Leaf; tag() = :leaf; end; end; end
        module ExpMod; export ExpSub; module ExpSub; v() = 9; end; end
        using .ExpMod
        include($(repr(joinpath(ws, "inc_aliases.jl"))))
        const BF44 = GrailV46.BF
        const BF44_M = Main.GrailV46.BF
        const A_MID = Outer.Mid
        const B_LEAF = A_MID.Leaf
        const EXP_ALIAS = ExpSub
        const IT_ALIAS = Base.Iterators
        const PREBOUND_IT = Base.Iterators
        bf_var = GrailV46.BF
        bf_overwritten = GrailV46.BF
        bf_dyn_over = GrailV46.BF
        enc44(x) = BF44.foo(x)
        struct AliasBoxHolder; b::BF44.Box; end
        holder = AliasBoxHolder(BF44.Box(8))
        enc_struct_box(b::BF44_M.Box) = b.x
        enc_chain() = (B_LEAF.tag(), INC_SUB.k(), EXP_ALIAS.v())
        enc_nonconst(x) = bf_var.foo(x)
        make_cl(off) = x -> BF44.foo(x) + off
        cl = make_cl(10)
        const CL_CONST = x -> BF44_M.foo(x) + 100
        """, 20.0)
        @test t1[1].result.success
        t2, _ = Palette.execute_turn("""
        side_effects = Int[]
        _log_side(n) = (push!(side_effects, n); open($(repr(side_log)), "a") do io; println(io, n); end)
        make_something() = (_log_side(1); (val=99,))
        make_mod() = (_log_side(2); Module(:DynMod))
        struct TrapObj; sub::Module; end
        module TrapTarget; z() = 5; end
        trap = TrapObj(TrapTarget)
        Base.getproperty(t::TrapObj, s::Symbol) = (_log_side(3); getfield(t, s))
        const X_CALL = make_something()
        const X_DYNMOD = make_mod()
        const X_TRAP = trap.sub
        x_dynvar = make_mod()
        bf_overwritten = 123
        bf_dyn_over = make_mod()
        """, 20.0)
        @test t2[1].result.success
        @test read(side_log, String) == "1\n2\n3\n2\n2\n"
        @test [e["kind"] for e in Palette.DEFINITION_LOG if "X_CALL" in e["names"]] == ["const"]
        @test !any(e -> "x_dynvar" in e["names"], Palette.DEFINITION_LOG)
        @test !any(e -> "X_TRAP" in e["names"] && e["kind"] == "module_alias", Palette.DEFINITION_LOG)
        write(side_log, "")
        @test Palette.snapshot_state!(2) !== nothing

        # Reverse definitions in manifest to verify prerequisite graph topological ordering
        mpath = joinpath(st, "manifest.json")
        m = JSON.parse(read(mpath, String))
        m["definitions"] = reverse(m["definitions"])
        write(mpath, JSON.json(m))

        # Full cold subprocess revival + idempotency + side-effect negative controls
        verify_script = """
        using Palette, Test
        Palette.WORKSPACE_ROOT[] = $(repr(ws))
        Palette.STATE_DIR[] = $(repr(st))
        name = Palette.revival_module_name()
        Palette.GLOBAL_STATE[] = Palette.KernelState(Module(Symbol(name)))
        mod = Palette.get_kernel_state().eval_module
        Core.eval(Main, Expr(:(=), nameof(mod), mod))
        Core.eval(mod, :(const PREBOUND_IT = Base.Iterators))
        rep = Base.invokelatest(Palette.revive_state!)
        @test !occursin("BF44", Palette.REVIVAL_OBSERVATION[]["lost"] |> join)
        @test Base.invokelatest(isdefined, mod, :BF44) && Base.invokelatest(isconst, mod, :BF44)
        @test Base.invokelatest(isdefined, mod, :bf_var) && !Base.invokelatest(isconst, mod, :bf_var)
        @test Base.invokelatest(getglobal, mod, :BF44) === Base.invokelatest(getglobal, mod, :BF44_M)
        @test Base.invokelatest(getglobal, mod, :bf_var) === Base.invokelatest(getglobal, mod, :BF44)
        @test Base.invokelatest(getglobal, mod, :IT_ALIAS) === Base.Iterators
        @test Base.invokelatest(getglobal, mod, :PREBOUND_IT) === Base.Iterators && Base.invokelatest(isconst, mod, :PREBOUND_IT)
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :enc44), 1) == 45
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :enc_struct_box), Base.invokelatest(getglobal, mod, :BF44).Box(7)) == 7
        @test Base.invokelatest(getglobal, mod, :holder).b.x == 8
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :enc_chain)) == (:leaf, 7, 9)
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :enc_nonconst), 2) == 46
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :cl), 1) == 55
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :CL_CONST), 1) == 145
        @test Base.invokelatest(getglobal, mod, :bf_overwritten) == 123
        @test !Base.invokelatest(isdefined, mod, :bf_dyn_over)
        @test !Base.invokelatest(isdefined, mod, :X_DYNMOD)
        @test !Base.invokelatest(isdefined, mod, :x_dynvar)
        @test !Base.invokelatest(isdefined, mod, :X_TRAP)
        @test Base.invokelatest(getglobal, mod, :X_CALL) == (val=99,)
        @test Base.invokelatest(getglobal, mod, :side_effects) == [1, 2, 3, 2, 2]
        @test read($(repr(side_log)), String) == ""
        # Idempotent re-revival when const aliases already exist in mod
        Base.invokelatest(Palette.revive_state!)
        @test Base.invokelatest(Base.invokelatest(getglobal, mod, :enc44), 3) == 47
        @test read($(repr(side_log)), String) == ""
        """
        run(`$(Base.julia_cmd()) -t1 --startup-file=no --project=$(dirname(@__DIR__)) -e $verify_script`)
    finally
        Palette.reset_kernel_state!(; process=true)
        Palette.WORKSPACE_ROOT[] = old_ws; Palette.STATE_DIR[] = old_st
        rm(ws; recursive=true, force=true); rm(st; recursive=true, force=true); rm(side_log; force=true)
    end
end

@testset "Api.pin: 0-byte snapshot, lazy reconstruction across revival, unpin, pinned" begin
    Palette.reset_kernel_state!(; process=true)
    old_ws, old_st = Palette.WORKSPACE_ROOT[], Palette.STATE_DIR[]
    ws, st, side_log = mktempdir(), mktempdir(), tempname()
    try
        Palette.WORKSPACE_ROOT[] = ws
        Palette.STATE_DIR[] = st

        # Direct in-process unit checks: caching, nothing value, lazy=true, unpin, pinned
        calls = Ref(0)
        acc = Api.pin(:local_small) do
            calls[] += 1
            [10, 20, 30]
        end
        @test acc isa Palette.PinnedRef
        @test acc() == [10, 20, 30]
        @test Api.pin(:local_small) === acc()
        @test calls[] == 1
        @test Palette.safe_summarysize(acc) == 0

        nothing_calls = Ref(0)
        n_acc = Api.pin(:nil_val) do
            nothing_calls[] += 1
            nothing
        end
        @test n_acc() === nothing
        @test Api.pin(:nil_val) === nothing
        @test nothing_calls[] == 1

        # Pure lazy=true pin has has_recipe === true without capturing mutable locals
        l_pure = Api.pin(:deferred_pure; lazy=true) do
            "lazy-pure"
        end
        pinfo_pure = only(filter(p -> p.name === :deferred_pure, Api.pinned()))
        @test pinfo_pure.loaded === false && pinfo_pure.bytes === nothing && pinfo_pure.has_recipe === true
        @test l_pure() == "lazy-pure"
        @test only(filter(p -> p.name === :deferred_pure, Api.pinned())).loaded === true

        # Closure capturing a mutable Ref works in RAM with has_recipe === false
        lazy_calls = Ref(0)
        l_acc = Api.pin(:deferred; lazy=true) do
            lazy_calls[] += 1
            "lazy-built"
        end
        @test lazy_calls[] == 0
        pinfo = only(filter(p -> p.name === :deferred, Api.pinned()))
        @test pinfo.loaded === false && pinfo.bytes === nothing && pinfo.has_recipe === false
        @test l_acc() == "lazy-built"
        @test lazy_calls[] == 1
        @test l_acc() == "lazy-built"
        @test lazy_calls[] == 1

        @test Api.unpin(:local_small) === true
        @test Api.unpin(:local_small) === false
        @test_throws ErrorException Api.pin(:local_small)
        @test_throws ErrorException acc()
        @test Set(p.name for p in Api.pinned()) == Set([:nil_val, :deferred_pure, :deferred])

        # Turn-level + cold-subprocess revival test with >16 MiB payload & 0-byte snapshot
        Palette.reset_kernel_state!(; process=true)
        Palette.WORKSPACE_ROOT[] = ws
        Palette.STATE_DIR[] = st
        write(side_log, "")

        withenv("PALETTE_STATE_MAX_BINDING_BYTES" => "4096", "PALETTE_STATE_MAX_SIDECAR_BYTES" => "0") do
            t1, _ = Palette.execute_turn("""
            module PinHelperMod
                module Sub
                    fill_val() = Float32(7.5)
                end
            end
            const PIN_SUB = PinHelperMod.Sub
            build_corpus(n) = (open($(repr(side_log)), "a") do io; println(io, "built:", n); end; fill(PIN_SUB.fill_val(), n))

            # 20 MiB array pinned -> 0 bytes in snapshot bindings!
            big_acc = Api.pin(:big_weights) do
                build_corpus(5_000_000)
            end
            # Also an unnamed pin accessed via Api.pin(:named_only)
            Api.pin(:named_only) do
                open($(repr(side_log)), "a") do io; println(io, "named_only"); end
                Dict("k" => 42)
            end
            # Unpinned ref should not resurrect
            temp_acc = Api.pin(:ephemeral_pin) do
                [1, 2, 3]
            end
            Api.unpin(:ephemeral_pin)
            length(big_acc())
            """, 30.0)
            @test t1[1].result.success
            @test t1[1].result.data == 5_000_000
            @test read(side_log, String) == "built:5000000\nnamed_only\n"
            @test [p.name for p in Api.pinned()] == [:big_weights, :named_only]

            # varinfo reports 0 bytes for big_acc
            vi = Palette.execute(Palette.ExecuteCode("varinfo()")).result.data
            vi_map = Dict(row[1] => row[2] for row in vi.rows)
            @test vi_map["big_acc"] == "0 bytes"

            manifest = Palette.snapshot_state!(1)
            @test manifest !== nothing
            @test manifest["bytes"] == 0
            @test manifest["sidecar_bytes"] == 0
            @test isempty(manifest["data"])
            b_big = only(filter(b -> b["name"] == "big_acc", manifest["bindings"]))
            @test b_big["class"] == "pin"
            @test b_big["pin_name"] == "big_weights"
            b_tmp = only(filter(b -> b["name"] == "temp_acc", manifest["bindings"]))
            @test b_tmp["class"] == "runtime" && occursin("unpinned", b_tmp["reason"])
            @test sort!([p["name"] for p in manifest["pins"]]) == ["big_weights", "named_only"]
        end

        write(side_log, "")
        verify_script = """
        using Palette, Test
        Palette.WORKSPACE_ROOT[] = $(repr(ws))
        Palette.STATE_DIR[] = $(repr(st))
        name = Palette.revival_module_name()
        Palette.GLOBAL_STATE[] = Palette.KernelState(Module(Symbol(name)))
        mod = Palette.get_kernel_state().eval_module
        Core.eval(Main, Expr(:(=), nameof(mod), mod))
        rep = Base.invokelatest(Palette.revive_state!)
        @test occursin("restored exactly", rep)
        @test occursin("big_acc", rep)
        obs = Api.revival()
        @test any(s -> startswith(s, "temp_acc "), obs["lost"])
        @test !any(s -> startswith(s, "big_acc "), obs["lost"])
        @test !any(s -> startswith(s, "big_weights"), obs["lost"])
        # Constructor MUST NOT have run eagerly during revival!
        @test read($(repr(side_log)), String) == ""

        ps_before = Api.pinned()
        @test [p.name for p in ps_before] == [:big_weights, :named_only]
        @test all(p -> !p.loaded && p.bytes === nothing && p.has_recipe, ps_before)

        # First access through returned accessor lazily reconstructs the 20 MiB vector
        acc_rev = Base.invokelatest(getglobal, mod, :big_acc)
        v1 = acc_rev()
        @test length(v1) == 5_000_000 && v1[1] == Float32(7.5) && v1[end] == Float32(7.5)
        @test read($(repr(side_log)), String) == "built:5000000\\n"

        # Second access and Api.pin(:big_weights) reuse cached instance (no rebuild)
        v2 = Api.pin(:big_weights)
        @test v2 === v1
        @test read($(repr(side_log)), String) == "built:5000000\\n"

        # Access via Api.pin(:named_only) lazily reconstructs the unnamed pin too
        @test Api.pin(:named_only) == Dict("k" => 42)
        @test read($(repr(side_log)), String) == "built:5000000\\nnamed_only\\n"
        @test Api.pin(:named_only) == Dict("k" => 42)
        @test read($(repr(side_log)), String) == "built:5000000\\nnamed_only\\n"

        ps_after = Api.pinned()
        @test all(p -> p.loaded && p.bytes !== nothing, ps_after)
        @test Api.unpin(:named_only) === true
        @test [p.name for p in Api.pinned()] == [:big_weights]
        """
        run(`$(Base.julia_cmd()) -t1 --startup-file=no --project=$(dirname(@__DIR__)) -e $verify_script`)
    finally
        Palette.reset_kernel_state!(; process=true)
        Palette.WORKSPACE_ROOT[] = old_ws; Palette.STATE_DIR[] = old_st
        rm(ws; recursive=true, force=true); rm(st; recursive=true, force=true); rm(side_log; force=true)
    end
end

@testset "Api.map_budgeted: chunked yield, soft budget_s cutoff, empty input" begin
    reset_kernel_state()

    # Empty inputs
    e_vec = Api.map_budgeted(x -> x + 1, Int[])
    @test e_vec.results == Any[] && e_vec.completed == 0 && e_vec.total == 0 && e_vec.timed_out === false && e_vec.elapsed_s >= 0.0
    e_gen = Api.map_budgeted(x -> x * 2, (i for i in 1:0))
    @test e_gen.completed == 0 && e_gen.total == 0 && e_gen.timed_out === false

    # Full completion + type narrowing + cooperative yield at chunk boundaries
    yielded = Ref(false)
    bg = @async (yielded[] = true)
    full = Api.map_budgeted(x -> x * x, 1:10; budget_s=10.0, chunk=3)
    wait(bg)
    @test yielded[] === true
    @test full.results == [i * i for i in 1:10]
    @test eltype(full.results) === Int
    @test full.completed == 10
    @test full.total == 10
    @test full.timed_out === false
    @test full.elapsed_s >= 0.0

    # Soft budget_s early cutoff returns exact partial prefix
    partial = Api.map_budgeted(1:20; budget_s=0.12, chunk=2) do i
        sleep(0.03)
        i * 10
    end
    @test partial.timed_out === true
    @test 1 <= partial.completed < 20
    @test partial.total == 20
    @test partial.results == [i * 10 for i in 1:partial.completed]
    @test partial.elapsed_s >= 0.12

    # Validation errors
    @test_throws ArgumentError Api.map_budgeted(identity, 1:3; budget_s=0.0)
    @test_throws ArgumentError Api.map_budgeted(identity, 1:3; budget_s=-1.0)
    @test_throws ArgumentError Api.map_budgeted(identity, 1:3; chunk=0)

    # Inside Palette turn
    r_turn = Palette.execute(Palette.ExecuteCode("Api.map_budgeted(x -> x + 5, [1, 2, 3]; budget_s=5.0, chunk=2)"))
    @test r_turn.result.success
    @test r_turn.result.data.results == [6, 7, 8]
    @test r_turn.result.data.completed == 3 && !r_turn.result.data.timed_out
end

@testset "World-age benchmark hint on module/include turns >1s" begin
    Palette.reset_kernel_state!(; process=true)
    old_ws = Palette.WORKSPACE_ROOT[]
    ws = mktempdir()
    Palette.WORKSPACE_ROOT[] = ws
    try
        inc_file = joinpath(ws, "slow_mod.jl")
        write(inc_file, "module SlowIncMod; f(x) = x + 1; end\n")

        # 1. Fast include (<1s): MUST NOT attach world_age_hint
        (r_fast_inc, _, _), _ = Palette.execute_turn("include($(repr(inc_file))); SlowIncMod.f(1)", 20.0)
        @test r_fast_inc.result.success
        c_fast_inc = Palette.Api.costs(1)
        @test c_fast_inc["execution_seconds"] < 1.0
        @test !haskey(c_fast_inc, "world_age_hint")

        # 2. Slow pure compute (>1s) WITHOUT module/include: MUST NOT attach world_age_hint
        (r_slow_pure, _, _), _ = Palette.execute_turn("sleep(1.05); 42", 20.0)
        @test r_slow_pure.result.success
        c_slow_pure = Palette.Api.costs(2)
        @test c_slow_pure["execution_seconds"] > 1.0
        @test !haskey(c_slow_pure, "world_age_hint")

        # 3. Slow quoted AST literal :(module Fake; end) (>1s): MUST NOT attach world_age_hint
        (r_quoted, _, _), _ = Palette.execute_turn("ex = :(module QuotedFake; end); sleep(1.05); ex.head", 20.0)
        @test r_quoted.result.success
        c_quoted = Palette.Api.costs(3)
        @test c_quoted["execution_seconds"] > 1.0
        @test !haskey(c_quoted, "world_age_hint")

        # 4. include(...) + >1s execution: MUST attach world_age_hint
        (r_slow_inc, _, _), _ = Palette.execute_turn("include($(repr(inc_file))); sleep(1.05); SlowIncMod.f(2)", 20.0)
        @test r_slow_inc.result.success
        c_slow_inc = Palette.Api.costs(4)
        @test c_slow_inc["execution_seconds"] > 1.0
        @test haskey(c_slow_inc, "world_age_hint")
        @test occursin("world-age", lowercase(c_slow_inc["world_age_hint"]))
        @test Palette.xray(4)["world_age_hint"] == c_slow_inc["world_age_hint"]

        # 5. Inline module ... end + >1s execution: MUST attach world_age_hint
        (r_slow_mod, _, _), _ = Palette.execute_turn("module InlineSlowMod; g(x) = x * 2; end; sleep(1.05); InlineSlowMod.g(3)", 20.0)
        @test r_slow_mod.result.success
        c_slow_mod = Palette.Api.costs(5)
        @test c_slow_mod["execution_seconds"] > 1.0
        @test haskey(c_slow_mod, "world_age_hint")
    finally
        Palette.reset_kernel_state!(; process=true)
        Palette.WORKSPACE_ROOT[] = old_ws
        rm(ws; recursive=true, force=true)
    end
end

@testset "Api.grep_defs cross-language Rust + WGSL + Julia queries" begin
    Palette.reset_kernel_state!(; process=true)
    old_ws = Palette.WORKSPACE_ROOT[]
    repo = mktempdir()
    try
        mkpath(joinpath(repo, "crates", "core"))
        mkpath(joinpath(repo, "shaders"))
        mkpath(joinpath(repo, "julia"))
        write(joinpath(repo, "crates", "core", "lib.rs"), """
        pub mod pipeline;
        pub struct GpuPipeline { id: u32 }
        pub enum ShaderStage { Vertex, Fragment }
        pub trait KernelPass { fn run(&self); }
        impl KernelPass for GpuPipeline {
            fn run(&self) {}
        }
        pub const MAX_FRAMES: usize = 3;
        pub type FrameIdx = u64;
        macro_rules! gpu_assert { () => {}; }
        pub fn build_pipeline() {}
        """)
        write(joinpath(repo, "shaders", "compute.wgsl"), """
        alias Scalar = f32;
        const TILE: u32 = 16u;
        @id(0) override TILE_OVERRIDE: u32 = 16u;
        struct PipelineParams { scale: Scalar };
        @group(0) @binding(0) var<uniform> pipeline_params: PipelineParams;
        @compute @workgroup_size(16, 1, 1)
        fn run_pipeline(@builtin(global_invocation_id) id: vec3<u32>) {
            var local_pipeline_val: Scalar = 0.0;
        }
        """)
        write(joinpath(repo, "julia", "driver.jl"), """
        struct PipelineHost
            steps::Int
        end
        function run_pipeline_host(h::PipelineHost)
            return h.steps
        end
        """)

        Palette.WORKSPACE_ROOT[] = repo
        Palette.codegraph!(nothing)

        # Unified cross-language query via Api.grep_defs with root= and exts=
        all_hits = Api.grep_defs("pipeline"; root=repo, ignorecase=true)
        @test [(m.file, m.kind, m.name) for m in all_hits] == [
            ("crates/core/lib.rs", :mod, "pipeline"),
            ("crates/core/lib.rs", :struct, "GpuPipeline"),
            ("crates/core/lib.rs", :impl, "KernelPass for GpuPipeline"),
            ("crates/core/lib.rs", :fn, "build_pipeline"),
            ("julia/driver.jl", :struct, "PipelineHost"),
            ("julia/driver.jl", :function, "run_pipeline_host"),
            ("shaders/compute.wgsl", :struct, "PipelineParams"),
            ("shaders/compute.wgsl", :var, "pipeline_params"),
            ("shaders/compute.wgsl", :fn, "run_pipeline"),
        ]

        wgsl_rs = Api.grep_defs(r"Pipeline|TILE"; root=repo, exts=(".rs", ".wgsl"))
        @test [(m.file, m.kind, m.name) for m in wgsl_rs] == [
            ("crates/core/lib.rs", :struct, "GpuPipeline"),
            ("crates/core/lib.rs", :impl, "KernelPass for GpuPipeline"),
            ("shaders/compute.wgsl", :const, "TILE"),
            ("shaders/compute.wgsl", :override, "TILE_OVERRIDE"),
            ("shaders/compute.wgsl", :struct, "PipelineParams"),
        ]

        # Works inside a persistent Palette turn via Api.grep_defs and Palette.grep_defs
        r_turn = Palette.execute(Palette.ExecuteCode("Api.grep_defs(\"run_pipeline\"; root=$(repr(repo)), exts=(\".wgsl\", \".jl\"))"))
        @test r_turn.result.success
        @test [(m.file, m.name) for m in r_turn.result.data] == [
            ("julia/driver.jl", "run_pipeline_host"),
            ("shaders/compute.wgsl", "run_pipeline"),
        ]
    finally
        Palette.codegraph!(nothing)
        Palette.reset_kernel_state!(; process=true)
        Palette.WORKSPACE_ROOT[] = old_ws
        rm(repo; recursive=true, force=true)
    end
end

# The code graph Palette.codegraph() serves, tested on its own.
isdefined(@__MODULE__, :Pharos) || (const Pharos = Palette.Pharos)
include("pharos.jl")
include("test_dogfood_wave1.jl")
include("test_dogfood_wave2.jl")
include("test_dogfood_wave3.jl")
include("test_dogfood_wave4.jl")
include("test_dogfood_wave5.jl")
