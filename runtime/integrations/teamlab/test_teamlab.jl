using Test

module TeamLabTestMod
    include("teamlab.jl")
end
const TL = TeamLabTestMod

@testset "TeamLab Hot-Reload Shelf & Cross-Agent Mailbox Suite" begin
    mktempdir() do tmp_lab
        @testset "1. Bootstrap Default Shelf (vecindex, jrir, teamlab)" begin
            st = TL.teamlab(Dict("action" => "status", "lab_dir" => tmp_lab))
            @test st["status"] == "ok"
            tool_names = [t["name"] for t in st["tools_on_shelf"]]
            @test "vecindex" in tool_names
            @test "jrir" in tool_names
            @test "teamlab" in tool_names
        end

        @testset "2. Publish Custom Tool with QA Test Gate & Hot-Reload Sync" begin
            tool_file = joinpath(tmp_lab, "custom_probe.jl")
            test_file = joinpath(tmp_lab, "test_custom_probe.jl")
            write(tool_file, """
            custom_probe(x::Int) = x * 2 + 1
            """)
            write(test_file, """
            using Test
            include("custom_probe.jl")
            @test custom_probe(20) == 41
            """)

            pub = TL.teamlab(Dict(
                "action" => "publish",
                "tool" => "custom_probe",
                "entrypoint" => tool_file,
                "test_path" => test_file,
                "author" => "gemini",
                "version" => "0.1.0",
                "notify" => "grok",
                "lab_dir" => tmp_lab,
            ))
            @test pub["status"] == "published"
            @test pub["qa_passed"] == true
            sha_v1 = pub["sha256"]

            # Sync into an isolated simulated Grok bench module (zero CLI restart)
            GrokBench = Module(:GrokBench)
            sync_v1 = TL.teamlab_sync!("grok"; tools=["custom_probe"], target_mod=GrokBench, lab_dir=tmp_lab)
            @test sync_v1["status"] == "synced"
            @test length(sync_v1["reloaded_tools"]) == 1
            @test sync_v1["reloaded_tools"][1]["tool"] == "custom_probe"
            @test sync_v1["unread_count"] == 1
            @test Core.eval(GrokBench, :(custom_probe(10))) == 21

            # Second sync without changes -> already_current_tools
            sync_noop = TL.teamlab_sync!("grok"; tools=["custom_probe"], target_mod=GrokBench, lab_dir=tmp_lab)
            @test isempty(sync_noop["reloaded_tools"])
            @test "custom_probe" in sync_noop["already_current_tools"]

            # Update tool to v0.2.0 and hot-reload into the same GrokBench module without restart
            write(tool_file, """
            custom_probe(x::Int) = x * 10 + 7
            """)
            write(test_file, """
            using Test
            include("custom_probe.jl")
            @test custom_probe(10) == 107
            """)
            pub_v2 = TL.teamlab(Dict(
                "action" => "publish",
                "tool" => "custom_probe",
                "entrypoint" => tool_file,
                "test_path" => test_file,
                "author" => "gemini",
                "version" => "0.2.0",
                "notify" => "grok",
                "lab_dir" => tmp_lab,
            ))
            @test pub_v2["sha256"] != sha_v1

            sync_v2 = TL.teamlab_sync!("grok"; tools=["custom_probe"], target_mod=GrokBench, ack_synced=true, lab_dir=tmp_lab)
            @test length(sync_v2["reloaded_tools"]) == 1
            @test Core.eval(GrokBench, :(custom_probe(10))) == 107

            # Verify ack_synced cleared grok's inbox
            inb = TL.teamlab(Dict("action" => "inbox", "agent" => "grok", "lab_dir" => tmp_lab))
            @test inb["unread_count"] == 0
        end

        @testset "3. Cross-Agent Mailbox (claude <-> gemini <-> grok <-> codex)" begin
            sent = TL.teamlab(Dict(
                "action" => "send",
                "sender" => "claude",
                "recipient" => "all",
                "subject" => "task:gesso-audit",
                "body" => "Please verify softmax_n and Python callees on Gesso.",
                "lab_dir" => tmp_lab,
            ))
            @test sent["status"] == "sent"

            # Sender (claude) already acked own message; grok and gemini see it as unread
            inb_claude = TL.teamlab(Dict("action" => "inbox", "agent" => "claude", "lab_dir" => tmp_lab))
            @test inb_claude["unread_count"] == 0

            inb_grok = TL.teamlab(Dict("action" => "inbox", "agent" => "grok", "lab_dir" => tmp_lab))
            @test inb_grok["unread_count"] == 1
            @test inb_grok["messages"][1]["subject"] == "task:gesso-audit"

            ack_res = TL.teamlab(Dict("action" => "ack", "agent" => "grok", "ids" => [sent["id"]], "lab_dir" => tmp_lab))
            @test ack_res["acked_count"] == 1

            # Grok is now at 0 unread, while gemini still has 1 unread
            @test TL.teamlab(Dict("action" => "inbox", "agent" => "grok", "lab_dir" => tmp_lab))["unread_count"] == 0
            @test TL.teamlab(Dict("action" => "inbox", "agent" => "gemini", "lab_dir" => tmp_lab))["unread_count"] == 1
        end

        @testset "4. Failing QA Gate Rejects Publication" begin
            bad_tool = joinpath(tmp_lab, "bad_tool.jl")
            bad_test = joinpath(tmp_lab, "test_bad_tool.jl")
            write(bad_tool, "bad_fn() = 1\n")
            write(bad_test, "using Test; @test 1 == 2\n")
            @test_throws ErrorException TL.teamlab(Dict(
                "action" => "publish",
                "tool" => "bad_tool",
                "entrypoint" => bad_tool,
                "test_path" => bad_test,
                "lab_dir" => tmp_lab,
            ))
        end
    end
end
