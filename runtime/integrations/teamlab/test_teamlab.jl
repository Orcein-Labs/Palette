# End to end: two agents' kernels against real `palette-host broker`
# processes that share one shelf directory, as two agents' sessions would.
# Set PALETTE_HOST to a built palette-host, or build runtime/host first.

using Test
using Palette

const HOST = let
    root = normpath(joinpath(@__DIR__, "..", "..", "host", "target"))
    candidates = filter(!isempty, [get(ENV, "PALETTE_HOST", ""),
                                   joinpath(root, "release", "palette-host"),
                                   joinpath(root, "debug", "palette-host")])
    i = findfirst(isfile, candidates)
    i === nothing && error("no palette-host: set PALETTE_HOST or run `cargo build --manifest-path runtime/host/Cargo.toml`")
    candidates[i]
end

# The capability, loaded the way the runtime loads one.
const TL = let m = Module(:PaletteCapability_teamlab_test)
    Core.eval(m, :(const Palette = $Palette))
    Base.include(m, joinpath(@__DIR__, "teamlab.jl"))
    m
end
teamlab(args...) = Base.invokelatest(TL.teamlab, Dict{String,Any}(args...))

struct Broker
    proc::Base.Process
    socket::String
end

function start_broker(tmp, agent, lab)
    socket = joinpath(tmp, "$agent.sock")
    ispath(socket) && rm(socket)
    ceiling = Palette.JSON.json(Dict("teamlab" => Dict("dir" => lab)))
    cmd = `$HOST broker --ceiling $ceiling --receipts $(joinpath(tmp, "$agent-receipts.jsonl")) --socket $socket`
    env = merge(ENV, Dict("PALETTE_AGENT" => agent, "PALETTE_PACKAGE_DEPOT" => joinpath(tmp, "depot")))
    proc = open(pipeline(setenv(cmd, env); stderr=joinpath(tmp, "$agent-stderr.txt")), "r+")
    line = readline(proc)
    occursin("READY", line) || error("the $agent broker did not start: $line\n$(read(joinpath(tmp, "$agent-stderr.txt"), String))")
    return Broker(proc, socket)
end

function stop(b::Broker)
    close(b.proc.in)
    wait(b.proc)
end

as(f, b::Broker) = withenv(f, "PALETTE_BROKER_SOCKET" => b.socket)

world() = Palette.get_kernel_state().eval_module
in_world(ex) = Core.eval(world(), ex)

const PROBE_V1 = "probe(x) = 2x + 1\n"
const PROBE_V2 = "probe(x) = 10x + 7\n"
const PROBE_TEST = """
using Test
include("probe.jl")
@test probe(20) == 41
"""

@testset "TeamLab over the session host" begin
    mktempdir() do tmp
        lab = joinpath(tmp, "lab")
        claude, codex = start_broker(tmp, "claude", lab), start_broker(tmp, "codex", lab)
        try
            @testset "publish checks the tool first" begin
                pub = as(claude) do
                    teamlab("action" => "publish", "name" => "probe", "source" => PROBE_V1, "test_source" => PROBE_TEST, "note" => "first cut")
                end
                @test pub["published"] === true
                @test pub["version"] == 1
                @test pub["new"] === true
                @test pub["pinned"] === true

                bad = as(claude) do
                    teamlab("action" => "publish", "name" => "probe", "source" => PROBE_V2, "test_source" => PROBE_TEST)
                end
                @test bad["published"] === false
                @test occursin("Test Failed", bad["test_output"]) || occursin("Error", bad["test_output"])

                # A tool must define the binding it is named after.
                @test_throws ArgumentError as(() -> teamlab("action" => "publish", "name" => "nobody", "source" => "somebody() = 1"), claude)
                @test_throws ArgumentError as(() -> teamlab("action" => "publish", "name" => "probe", "source" => PROBE_V1, "lab_dir" => tmp), claude)
                @test_throws ArgumentError as(() -> teamlab("action" => "publish", "name" => "probe"), claude)
                @test_throws ArgumentError as(() -> teamlab("action" => "explode"), claude)

                st = as(() -> teamlab("action" => "status"), claude)
                @test st["agent"] == "claude"
                @test [t["name"] for t in st["tools"]] == ["probe"]
                @test st["tools"][1]["versions"] == 1
                @test st["tools"][1]["latest"]["test"]["passed"] === true
            end

            @testset "a file and its test can be published by path" begin
                dir = mkpath(joinpath(tmp, "src"))
                write(joinpath(dir, "twice.jl"), "twice(x) = 2x\n")
                write(joinpath(dir, "test_twice.jl"), "using Test; include(\"twice.jl\"); @test twice(4) == 8\n")
                pub = as(codex) do
                    teamlab("action" => "publish", "name" => "twice", "path" => joinpath(dir, "twice.jl"), "test" => joinpath(dir, "test_twice.jl"))
                end
                @test pub["published"] === true
                @test as(() -> teamlab("action" => "fetch", "name" => "twice"), claude)["author"] == "codex"
            end

            @testset "approval is someone else's" begin
                @test_throws ErrorException as(() -> teamlab("action" => "approve", "name" => "probe"), claude)
                as(() -> teamlab("action" => "approve", "name" => "probe", "note" => "read it"), codex)
                fetched = as(() -> teamlab("action" => "fetch", "name" => "probe"), claude)
                @test [a["by"] for a in fetched["approvals"]] == ["codex"]
                @test fetched["source"] == PROBE_V1
            end

            @testset "sync binds pinned tools into the world and hot-reloads them" begin
                s1 = as(() -> teamlab("action" => "sync"), claude)
                @test [l["name"] for l in s1["loaded"]] == ["probe"]
                @test isempty(s1["failed"])
                @test in_world(:(TeamLab.probe(10))) == 21

                s2 = as(() -> teamlab("action" => "sync"), claude)
                @test isempty(s2["loaded"])
                @test s2["current"] == ["probe"]

                pub = as(() -> teamlab("action" => "publish", "name" => "probe", "source" => PROBE_V2), claude)
                @test pub["version"] == 2
                s3 = as(() -> teamlab("action" => "sync"), claude)
                @test [l["version"] for l in s3["loaded"]] == [2]
                @test in_world(:(TeamLab.probe(10))) == 107
            end

            @testset "another agent pins, falls behind, and updates" begin
                as(() -> teamlab("action" => "pin", "name" => "probe", "sha256" => as(() -> teamlab("action" => "fetch", "name" => "probe"), claude)["sha256"]), codex)
                st = as(() -> teamlab("action" => "status"), codex)
                probe = only(filter(t -> t["name"] == "probe", st["tools"]))
                @test probe["pinned_version"] == 2
                @test probe["behind"] === false

                v3 = as(() -> teamlab("action" => "publish", "name" => "probe", "source" => "probe(x) = x\n"), claude)
                @test as(() -> teamlab("action" => "sync"), codex)["behind"] == ["probe"]
                up = as(() -> teamlab("action" => "update", "names" => ["probe"]), codex)
                @test only(up["updated"])["to"] == v3["sha256"]
                @test only(up["updated"])["from_version"] == 2
                as(() -> teamlab("action" => "unpin", "name" => "probe"), codex)
                st = as(() -> teamlab("action" => "status"), codex)
                @test only(filter(t -> t["name"] == "probe", st["tools"]))["pinned"] === nothing
                s = as(() -> teamlab("action" => "sync"), codex)
                @test !("probe" in s["current"]) && !("probe" in [l["name"] for l in s["loaded"]])
            end

            @testset "mailbox" begin
                sent = as(() -> teamlab("action" => "send", "to" => "codex", "subject" => "probe v3", "body" => "it is the identity now", "tool" => "probe"), claude)
                @test_throws ErrorException as(() -> teamlab("action" => "send", "to" => "claude", "subject" => "me"), claude)
                inbox = as(() -> teamlab("action" => "inbox"), codex)
                @test inbox["unread"] == 1
                @test only(inbox["messages"])["subject"] == "probe v3"
                @test as(() -> teamlab("action" => "inbox"), claude)["unread"] == 0
                ack = as(() -> teamlab("action" => "ack", "ids" => [sent["id"]]), codex)
                @test ack["acked"] == [sent["id"]]
                @test as(() -> teamlab("action" => "inbox"), codex)["unread"] == 0
            end

            @testset "the shelf outlives a session" begin
                stop(claude)
                claude = start_broker(tmp, "claude", lab)
                st = as(() -> teamlab("action" => "status"), claude)
                probe = only(filter(t -> t["name"] == "probe", st["tools"]))
                @test probe["versions"] == 3
                @test probe["pinned_version"] == 3
            end

            @testset "the kernel side holds no path to the shelf" begin
                # The directory is the host's; a kernel only ever asks for it.
                @test !occursin("ENV", read(joinpath(@__DIR__, "teamlab.jl"), String))
                @test filemode(lab) & 0o077 == 0
            end
        finally
            stop(claude)
            stop(codex)
        end
    end
end
