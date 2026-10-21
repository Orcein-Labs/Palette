# Offline installer and capability deployment contract tests.
using Test
using JSON

include(joinpath(@__DIR__, "install.jl"))
using .GessoInstall: MODEL_FILES, deploy_capability, install_gesso_source, install_model, inventory_digest,
                     sha256_file, validate_model_source, copy_file, parse_args, capture

const JULIA = joinpath(Sys.BINDIR, Base.julia_exename())
const PALETTE_ROOT = dirname(dirname(dirname(realpath(@__DIR__))))

"A scratch root holding a SmolLM2-shaped model fixture, removed after `body` runs."
function with_fixture(body)
    mktempdir() do root
        model = joinpath(root, "source-model")
        mkdir(model)
        cfg = Dict("model_type" => "llama", "hidden_size" => 576, "num_hidden_layers" => 30,
                   "num_attention_heads" => 9, "num_key_value_heads" => 3, "vocab_size" => 49152,
                   "tie_word_embeddings" => true, "eos_token_id" => 0, "max_position_embeddings" => 8192)
        for name in MODEL_FILES
            write(joinpath(model, name), name == "config.json" ? JSON.json(cfg) :
                                         name == "tokenizer_config.json" ? JSON.json(Dict("tokenizer_class" => "GPT2Tokenizer")) :
                                         name * "\n")
        end
        body(root, model)
    end
end

function run_julia(script, args...; load_path)
    out, err = IOBuffer(), IOBuffer()
    cmd = addenv(`$JULIA --startup-file=no -e $script $args`, "JULIA_LOAD_PATH" => load_path)
    ok = success(pipeline(cmd; stdout=out, stderr=err))
    return ok, String(take!(out)), String(take!(err))
end

deploy(root, artifact_root, info) =
    deploy_capability(joinpath(root, "runtime"), artifact_root, info, "revision-a", "a"^64, "0.1.0", @__DIR__)

@testset "Gesso installer" begin
    @testset "G01_fixed_model_identity_and_hash_copy" begin
        with_fixture() do root, model
            hashes, _ = validate_model_source(model)
            @test Set(keys(hashes)) == Set(MODEL_FILES)
            artifact_root = joinpath(root, "server-models")
            installed, manifest = install_model(model, artifact_root)
            @test manifest["id"] == "smollm2-135m-" * first(inventory_digest(hashes), 16)
            @test JSON.parsefile(joinpath(installed, "artifact.json")) == manifest
            for name in MODEL_FILES
                @test read(joinpath(installed, name)) == read(joinpath(model, name))
                @test filemode(joinpath(installed, name)) & 0o222 == 0
            end
            @test install_model(model, artifact_root) == (installed, manifest)
            tampered = joinpath(installed, "merges.txt")
            chmod(tampered, 0o640)
            write(tampered, "tampered")
            @test_throws "does not match its inventory" install_model(model, artifact_root)
            elsewhere = joinpath(root, "elsewhere")
            mv(installed, elsewhere)
            symlink(elsewhere, installed)
            @test_throws "not a regular directory" install_model(model, artifact_root)
        end
    end

    @testset "G02_wrong_architecture_and_hardlinks_fail_closed" begin
        with_fixture() do root, model
            cfg_path = joinpath(model, "config.json")
            cfg = JSON.parsefile(cfg_path)
            cfg["model_type"] = "other"
            write(cfg_path, JSON.json(cfg))
            @test_throws "contracted SmolLM2-135M" validate_model_source(model)
            cfg["model_type"] = "llama"
            write(cfg_path, JSON.json(cfg))
            write(joinpath(model, "tokenizer_config.json"), JSON.json(Dict("tokenizer_class" => "LlamaTokenizer")))
            @test_throws "contracted GPT2Tokenizer" validate_model_source(model)
            write(joinpath(model, "tokenizer_config.json"), JSON.json(Dict("tokenizer_class" => "GPT2Tokenizer")))
            target = joinpath(model, "hardlink.txt")
            hardlink(joinpath(model, "merges.txt"), target)
            rm(joinpath(model, "merges.txt"))
            hardlink(target, joinpath(model, "merges.txt"))
            @test_throws "expected an unlinked regular file" validate_model_source(model)
        end
    end

    @testset "G03_package_copy_uses_only_clean_tracked_runtime_source" begin
        with_fixture() do root, _
            source = joinpath(root, "Gesso")
            mkpath(joinpath(source, "src"))
            mkdir(joinpath(source, "ext"))
            mkpath(joinpath(source, "libs", "Lava"))
            write(joinpath(source, "Project.toml"), "name = \"Gesso\"\nuuid = \"05f31162-62af-44f0-af37-0e48a49f1899\"\nversion = \"0.1.0\"\n")
            write(joinpath(source, "src", "Gesso.jl"), "module Gesso\nend\n")
            write(joinpath(source, "ext", "GessoCUDAExt.jl"), "module GessoCUDAExt\nend\n")
            write(joinpath(source, "libs", "Lava", "Lava.jl"), "module Lava\nend\n")
            git(args) = run(Cmd(`git $args`; dir=source))
            git(`init -q`)
            git(`config user.email test@example.invalid`)
            git(`config user.name "Installer Test"`)
            git(`add Project.toml src ext`)
            git(`commit -qm "test source"`)
            copied, _, version, source_hash = install_gesso_source(source, joinpath(root, "packages"))
            @test version == "0.1.0"
            @test read(joinpath(copied, "src", "Gesso.jl"), String) == "module Gesso\nend\n"
            @test source_hash == inventory_digest(Dict(
                "Project.toml" => sha256_file(joinpath(copied, "Project.toml")),
                "src/Gesso.jl" => sha256_file(joinpath(copied, "src", "Gesso.jl")),
                "ext/GessoCUDAExt.jl" => sha256_file(joinpath(copied, "ext", "GessoCUDAExt.jl"))))
            @test isfile(joinpath(copied, "ext", "GessoCUDAExt.jl"))
            @test !ispath(joinpath(copied, "libs"))
            @test [joinpath(dir, f) for (dir, _, files) in walkdir(joinpath(copied, "src")) for f in files if endswith(f, ".jl")] ==
                  [joinpath(copied, "src", "Gesso.jl")]
            changing_copy(src, dst) = (basename(src) == "Gesso.jl" && write(src, "module Gesso; const changed=true; end\n");
                                       copy_file(src, dst))
            @test_throws "committed source inventory" install_gesso_source(source, joinpath(root, "racing-packages"); copy=changing_copy)
            @test isempty(readdir(joinpath(root, "racing-packages", "sources", "gesso")))
            git(`checkout -- src/Gesso.jl`)
            write(joinpath(source, ".gitignore"), "src/ignored.jl\n")
            write(joinpath(source, "src", "ignored.jl"), "error(\"untracked\")")
            @test_throws "committed source frontier" install_gesso_source(source, joinpath(root, "ignored-packages"))
            write(joinpath(source, "Project.toml"), "name = \"Gesso\"\nuuid = \"00000000-0000-0000-0000-000000000000\"\nversion = \"0.1.0\"\n")
            @test_throws "unexpected package identity" install_gesso_source(source, joinpath(root, "other-packages"))
        end
    end

    @testset "G04_installed_capability_pins_artifact_and_optional_dependency" begin
        with_fixture() do root, model
            artifact_root, info = install_model(model, joinpath(root, "models"))
            capability, version = deploy(root, artifact_root, info)
            manifest = JSON.parsefile(joinpath(capability, "capability.json"))
            code = read(joinpath(capability, "gesso.jl"), String)
            @test manifest["id"] == "gesso"
            @test manifest["dependencies"] == ["Gesso", "JSON"]
            @test isempty(manifest["requires"])
            @test occursin(realpath(artifact_root), code)
            @test !occursin("__PALETTE_GESSO_", code)
            # No developer checkout path leaks into the deployed code.
            @test !occursin(r"/mnt/[a-z]/", code)
            @test occursin(info["id"], code)
            @test startswith(version, "0.1.0+")
        end
    end

    @testset "G05_deployed_capability_and_probe_parse_as_julia" begin
        with_fixture() do root, model
            artifact_root, info = install_model(model, joinpath(root, "models"))
            capability, _ = deploy(root, artifact_root, info)
            script = "function check(ex); ex isa Expr || return; ex.head in (:error, :incomplete) && error(string(ex)); foreach(check, ex.args); end; " *
                     "foreach(p -> check(Meta.parseall(read(p, String))), ARGS); println(\"GESSO_SOURCE_PARSE=ok\")"
            ok, out, err = run_julia(script, joinpath(capability, "gesso.jl"), joinpath(@__DIR__, "probe.jl"); load_path="@stdlib")
            @test ok || err
            @test occursin("GESSO_SOURCE_PARSE=ok", out)
        end
    end

    @testset "G06_optional_backend_import_sees_new_extension_methods" begin
        with_fixture() do root, model
            packages = joinpath(root, "fixture-packages")
            fixtures = Dict(
                "Gesso" => "module Gesso; struct GessoError <: Exception; code::Symbol; message::String; end; const ERR_RESOURCE_LIMIT=:ERR_RESOURCE_LIMIT; gesso_error(code, message; kwargs...)=GessoError(code,message); function backend_name end; function supports end; architecture_spec(path)=:fixture; required_semantics(spec)=[:fixture_capability]; end",
                "JSON" => "module JSON; end",
                "CUDA" => "module CUDA; import Gesso; function __init__(); @eval Gesso begin; struct CUDABackend end; backend_name(::CUDABackend)=:fixture_cuda; supports(::CUDABackend, cap)=cap===:fixture_capability; end; end; end")
            for (name, source) in fixtures
                mkpath(joinpath(packages, name, "src"))
                write(joinpath(packages, name, "src", name * ".jl"), source)
            end
            artifact_root, info = install_model(model, joinpath(root, "models"))
            capability, _ = deploy(root, artifact_root, info)
            fixture_path = packages * ":@stdlib"
            script = "using Test; include(ARGS[1]); r=_backend_diagnostic(\"CUDA\", :cuda); println(r); @test r[\"status\"] == \"available\"; " *
                     "@test r[\"backend\"] == \"fixture_cuda\"; @test r[\"supports\"][\"fixture_capability\"]; println(\"GESSO_FIXTURE_BACKEND=fixture_cuda\")"
            ok, out, err = run_julia(script, joinpath(capability, "gesso.jl"); load_path=fixture_path)
            @test ok || out * err
            @test occursin("GESSO_FIXTURE_BACKEND=fixture_cuda", out)
            probe_script = "using Test; include(ARGS[1]); err=try check_optional_backend(\"cuda\"); nothing catch e; e end; @test err isa Gesso.GessoError; " *
                           "@test err.code === Gesso.ERR_RESOURCE_LIMIT; @test occursin(\"device construction only\", err.message); println(\"GESSO_PROBE_EXTENSION=constructed\")"
            ok, out, err = run_julia(probe_script, joinpath(@__DIR__, "probe.jl"); load_path=fixture_path)
            @test ok || out * err
            @test occursin("GESSO_PROBE_EXTENSION=constructed", out)
            validate_script = "using Test; include(ARGS[1]); @test _artifact(false)[\"id\"] == _GESSO_ARTIFACT_ID; rm(joinpath(_GESSO_ARTIFACT_ROOT,\"config.json\")); " *
                              "symlink(ARGS[2],joinpath(_GESSO_ARTIFACT_ROOT,\"config.json\")); @test_throws ArgumentError _artifact(false); println(\"GESSO_ARTIFACT_LINK=denied\")"
            rm(joinpath(packages, "JSON"); recursive=true)
            chmod(artifact_root, 0o750)
            ok, out, err = run_julia(validate_script, joinpath(capability, "gesso.jl"), joinpath(model, "config.json");
                                     load_path="$packages:$PALETTE_ROOT:@stdlib")
            @test ok || out * err
            @test occursin("GESSO_ARTIFACT_LINK=denied", out)
        end
    end

    @testset "G07_cli_equals_form_and_hard_registration_timeout" begin
        dirs = ["--gesso-source=/g", "--model-source", "/m", "--package-store=/p=q", "--artifact-store", "/a", "--runtime-root=/r"]
        parsed = parse_args(dirs)
        @test parsed[:gesso_source] == "/g" && parsed[:package_store] == "/p=q" && parsed[:runtime_root] == "/r"
        @test_throws ArgumentError parse_args([dirs; "--julia="; "/j"])
        @test_throws ArgumentError parse_args([dirs; "--gesso-source=/again"])
        @test_throws ArgumentError parse_args([dirs; "--bogus=1"])
        started = time()
        err = try capture(`sh -c 'trap "" TERM; exec sleep 30'`, 1); nothing catch e; e end
        @test err isa ErrorException && occursin("timed out after 1 seconds", err.msg)
        @test time() - started < 10
    end
end
