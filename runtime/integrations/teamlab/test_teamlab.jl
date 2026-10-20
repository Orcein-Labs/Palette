# End to end: two agents' kernels against real `palette-host broker`
# processes that share one shop directory, as two agents' sessions would.
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
            probe_id, bump_id = "", ""
            @testset "publish checks the tool first" begin
                pub = as(claude) do
                    teamlab("action" => "publish", "kind" => "function", "tag" => "Doubles a number and adds one",
                            "source" => PROBE_V1, "test_source" => PROBE_TEST, "note" => "first cut")
                end
                @test pub["published"] === true
                @test (pub["binding"], pub["kind"], pub["version"]) == ("probe", "function", 1)
                @test pub["new_listing"] === true && pub["pinned"] === true
                probe_id = pub["id"]

                # A second version of one's own listing needs no kind or tag.
                bad = as(() -> teamlab("action" => "publish", "source" => PROBE_V2, "test_source" => PROBE_TEST), claude)
                @test bad["published"] === false
                @test bad["binding"] == "probe"
                @test occursin("Test Failed", bad["test_output"]) || occursin("Error", bad["test_output"])

                # The binding is the one public function the source defines.
                helped = as(claude) do
                    teamlab("action" => "publish", "kind" => "function", "tag" => "Adds one", "source" => "_one() = 1\nbump(x) = x + _one()\n")
                end
                @test helped["binding"] == "bump"
                bump_id = helped["id"]
                err(args...) = try as(() -> teamlab(args...), claude); nothing catch e; e end
                e = err("action" => "publish", "source" => "a() = 1\nb() = 2\n", "tag" => "Two")
                @test e isa ArgumentError && occursin("several: a, b", e.msg)
                @test err("action" => "publish", "binding" => "nobody", "source" => "somebody() = 1") isa ArgumentError
                @test err("action" => "publish", "kind" => "module", "source" => PROBE_V1, "tag" => "x") isa ArgumentError
                @test err("action" => "publish", "kind" => "macro", "source" => PROBE_V1, "tag" => "x") isa ArgumentError
                @test err("action" => "publish", "source" => PROBE_V1, "lab_dir" => tmp) isa ArgumentError
                @test err("action" => "publish", "kind" => "function") isa ArgumentError
                @test err("action" => "explode") isa ArgumentError
                # The host asks a new listing for a tag.
                e = err("action" => "publish", "source" => "lonely(x) = x\n")
                @test e isa ErrorException && occursin("tag", e.msg)

                st = as(() -> teamlab("action" => "status"), claude)
                @test st["agent"] == "claude"
                @test sort([t["binding"] for t in st["pinned"]]) == ["bump", "probe"]
                probe = only(filter(t -> t["id"] == probe_id, st["pinned"]))
                @test probe["versions"] == 1
                @test probe["tag"] == "Doubles a number and adds one"
                @test probe["latest"]["test"]["passed"] === true
                @test isempty(st["new"])
            end

            twice_id = ""
            @testset "a file and its test can be published by path" begin
                dir = mkpath(joinpath(tmp, "src"))
                write(joinpath(dir, "twice.jl"), "twice(x) = 2x\n")
                write(joinpath(dir, "test_twice.jl"), "using Test; include(\"twice.jl\"); @test twice(4) == 8\n")
                pub = as(codex) do
                    teamlab("action" => "publish", "tag" => "Doubles a number", "path" => joinpath(dir, "twice.jl"),
                            "test" => joinpath(dir, "test_twice.jl"))
                end
                @test pub["published"] === true
                twice_id = pub["id"]
                @test as(() -> teamlab("action" => "fetch", "id" => twice_id), claude)["author"] == "codex"
                # New in the shop for the other agent.
                @test [t["id"] for t in as(() -> teamlab("action" => "status"), claude)["new"]] == [twice_id]
            end

            @testset "browse searches kind, binding, tag and author" begin
                ids(query) = [l["id"] for l in as(() -> teamlab("action" => "browse", "query" => query), codex)["listings"]]
                everything = as(() -> teamlab("action" => "browse"), codex)
                @test everything["listed"] == 3
                @test [l["id"] for l in everything["listings"]] == [twice_id, bump_id, probe_id]  # newest first
                @test ids("adds ONE claude") == [bump_id, probe_id]
                @test ids("doubles codex") == [twice_id]
                @test ids("function $probe_id") == [probe_id]
                @test isempty(ids("module"))
            end

            @testset "approval is someone else's" begin
                @test_throws ErrorException as(() -> teamlab("action" => "approve", "id" => probe_id), claude)
                as(() -> teamlab("action" => "approve", "id" => probe_id, "note" => "read it"), codex)
                fetched = as(() -> teamlab("action" => "fetch", "id" => probe_id), claude)
                @test [a["by"] for a in fetched["approvals"]] == ["codex"]
                @test fetched["source"] == PROBE_V1
            end

            @testset "try loads a listing without pinning it" begin
                t = as(() -> teamlab("action" => "try", "id" => twice_id), claude)
                @test (t["binding"], t["pinned"], t["replaced"]) == ("twice", false, nothing)
                @test in_world(:(TeamLab.twice(4))) == 8
                @test !any(p -> p["id"] == twice_id, as(() -> teamlab("action" => "status"), claude)["pinned"])
                @test_throws ErrorException as(() -> teamlab("action" => "try", "id" => "0000ffff"), claude)
                @test_throws ArgumentError as(() -> teamlab("action" => "try", "id" => twice_id, "pin" => true), claude)
            end

            @testset "sync binds pinned tools into the world and hot-reloads them" begin
                s1 = as(() -> teamlab("action" => "sync"), claude)
                @test sort([l["binding"] for l in s1["loaded"]]) == ["bump", "probe"]
                @test isempty(s1["failed"]) && isempty(s1["clashes"])
                @test in_world(:(TeamLab.probe(10))) == 21

                s2 = as(() -> teamlab("action" => "sync"), claude)
                @test isempty(s2["loaded"])
                @test sort(s2["current"]) == ["bump", "probe"]

                # By id, the client reads the binding from the listing.
                pub = as(() -> teamlab("action" => "publish", "id" => probe_id, "source" => PROBE_V2), claude)
                @test (pub["id"], pub["version"]) == (probe_id, 2)
                s3 = as(() -> teamlab("action" => "sync"), claude)
                @test [l["version"] for l in s3["loaded"]] == [2]
                @test in_world(:(TeamLab.probe(10))) == 107
            end

            @testset "another agent pins, falls behind, and updates" begin
                sha2 = as(() -> teamlab("action" => "fetch", "id" => probe_id), claude)["sha256"]
                as(() -> teamlab("action" => "pin", "id" => probe_id, "sha256" => sha2), codex)
                probe = only(filter(t -> t["id"] == probe_id, as(() -> teamlab("action" => "status"), codex)["pinned"]))
                @test probe["pinned_version"] == 2
                @test probe["behind"] === false

                v3 = as(() -> teamlab("action" => "publish", "id" => probe_id, "source" => "probe(x) = x\n"), claude)
                @test as(() -> teamlab("action" => "sync"), codex)["behind"] == [probe_id]
                up = as(() -> teamlab("action" => "update", "ids" => [probe_id]), codex)
                @test only(up["updated"])["to"] == v3["sha256"]
                @test only(up["updated"])["from_version"] == 2
                as(() -> teamlab("action" => "unpin", "id" => probe_id), codex)
                @test !any(t -> t["id"] == probe_id, as(() -> teamlab("action" => "status"), codex)["pinned"])
                s = as(() -> teamlab("action" => "sync"), codex)
                @test !("probe" in s["current"]) && !("probe" in [l["binding"] for l in s["loaded"]])
            end

            @testset "one binding per agent" begin
                mine = as(codex) do
                    teamlab("action" => "publish", "kind" => "function", "tag" => "Returns its argument", "source" => "probe(x) = x\n")
                end
                @test mine["new_listing"] === true && mine["id"] != probe_id && mine["pinned"] === true
                e = try as(() -> teamlab("action" => "pin", "id" => probe_id), codex); nothing catch e; e end
                @test e isa ErrorException && occursin("unpin it first", e.msg)
            end

            @testset "mailbox" begin
                sent = as(claude) do
                    teamlab("action" => "send", "to" => "codex", "subject" => "probe v3", "body" => "it is the identity now", "listing" => probe_id)
                end
                @test_throws ErrorException as(() -> teamlab("action" => "send", "to" => "claude", "subject" => "me"), claude)
                inbox = as(() -> teamlab("action" => "inbox"), codex)
                @test inbox["unread"] == 1
                @test only(inbox["messages"])["subject"] == "probe v3"
                @test only(inbox["messages"])["listing"] == probe_id
                @test as(() -> teamlab("action" => "inbox"), claude)["unread"] == 0
                ack = as(() -> teamlab("action" => "ack", "ids" => [sent["id"]]), codex)
                @test ack["acked"] == [sent["id"]]
                @test as(() -> teamlab("action" => "inbox"), codex)["unread"] == 0
            end

            @testset "the shop outlives a session" begin
                stop(claude)
                claude = start_broker(tmp, "claude", lab)
                probe = only(filter(t -> t["id"] == probe_id, as(() -> teamlab("action" => "status"), claude)["pinned"]))
                @test probe["versions"] == 3
                @test probe["pinned_version"] == 3
            end

            @testset "the author decides on a version another agent proposes" begin
                propose(source) = as(() -> teamlab("action" => "publish", "id" => probe_id, "source" => source, "note" => "triples"), codex)
                p1 = propose("probe(x) = 3x\n")
                @test (p1["published"], p1["proposed"], p1["version"], p1["pinned"]) == (true, true, nothing, false)
                @test p1["author"] == "claude" && occursin("accepts it", p1["next"])
                @test_throws ErrorException as(() -> teamlab("action" => "pin", "id" => probe_id, "sha256" => p1["sha256"]), codex)
                @test_throws ErrorException as(() -> teamlab("action" => "accept", "id" => probe_id, "sha256" => p1["sha256"]), codex)

                st = as(() -> teamlab("action" => "status"), claude)
                @test [(p["id"], p["sha256"], p["author"]) for p in st["proposals"]] == [(probe_id, p1["sha256"], "codex")]
                mail = only(as(() -> teamlab("action" => "inbox"), claude)["messages"])
                @test mail["from"] == "codex" && occursin("proposed a new version of probe", mail["subject"])
                t = as(() -> teamlab("action" => "try", "id" => probe_id, "sha256" => p1["sha256"]), claude)
                @test t["pending"] === true && occursin("proposal", t["keep"])
                @test in_world(:(TeamLab.probe(2))) == 6

                d = as(() -> teamlab("action" => "decline", "id" => probe_id, "sha256" => p1["sha256"], "note" => "not yet"), claude)
                @test (d["declined"], d["withdrawn"]) == (true, false)
                @test occursin("declined your version of probe", only(as(() -> teamlab("action" => "inbox"), codex)["messages"])["subject"])

                p2 = propose("probe(x) = 3x + 1\n")
                a = as(() -> teamlab("action" => "accept", "id" => probe_id, "sha256" => p2["sha256"]), claude)
                @test (a["version"], a["author"], a["pinned"]) == (4, "codex", true)
                probe = only(filter(t -> t["id"] == probe_id, as(() -> teamlab("action" => "status"), claude)["pinned"]))
                @test (probe["versions"], probe["pinned_version"]) == (4, 4)
                @test isempty(as(() -> teamlab("action" => "status"), claude)["proposals"])
            end

            @testset "the kernel side holds no path to the shop" begin
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
