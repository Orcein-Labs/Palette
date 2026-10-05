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
        req = JSON.parse(readline(conn))
        push!(seen, req)
        write(conn, JSON.json(reply(req["params"]["type"], req["params"]["payload"])) * "\n")
        close(conn)
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

    missing_receipt = execute(InvokeOperator("does_not_exist"))
    @test !missing_receipt.result.success
end

@testset "ShellEscape (real subprocess)" begin
    reset_kernel_state()
    receipt = execute(ShellEscape("echo hello"))
    @test receipt.result.success
    @test strip(receipt.result.data["stdout"]) == "hello"
    @test receipt.result.data["exit_code"] == 0

    fail_receipt = execute(ShellEscape("exit 3"))
    @test !fail_receipt.result.success
    @test fail_receipt.result.data["exit_code"] == 3
end

@testset "fs_digest broker request (FM-SLICE-A2)" begin
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

@testset "ShellEscape typed errors + output digests (FM-SLICE-A1)" begin
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
    # FM-RQ-A1 probe P13: Julia's Process.exitcode is 0 alongside a nonzero
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

    # turn code reaches authority through Palette, and nothing else of the package
    @test run("Palette.request_capability isa Function").data
    @test !run("Palette.reset_kernel_state()").success

    # the receipt log keeps outcomes, not values
    run("zeros(10^6)")
    @test get_kernel_state().receipt_log[end].result.data === nothing
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
        @test execute(ExecuteCode("big = Any[Any[Float64(i), Float64(i + 1)] for i in 1:100_000]; length(big)")).result.data == 100_000
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
