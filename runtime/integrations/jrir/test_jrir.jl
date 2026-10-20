using Test
using InteractiveUtils
using Random

module JRIRTestMod
    include("jrir.jl")
end
const J = JRIRTestMod

struct TestParticle
    x::Float32
    y::Float32
    vx::Float32
    vy::Float32
end

struct PaddedHeader
    tag::UInt8
    id::UInt64
    flags::UInt16
    weight::Float32
end

struct MisalignedHeader
    id::UInt64
    tag::UInt8
    flags::UInt16
    weight::Float32
end

mutable struct NonIsBitsParticle
    x::Float32
    y::Float32
    vx::Float32
    vy::Float32
end

@testset "JRIR RPDO + PEA Verification Suite" begin
    @testset "1. C-ABI Layout Calculator & Julia Struct Verifier" begin
        rust_src = """
        #[repr(C)]
        pub struct Particle {
            pub x: f32,
            pub y: f32,
            pub vx: f32,
            pub vy: f32,
        }

        #[repr(C)]
        pub struct PaddedHeader {
            pub tag: u8,
            pub id: u64,
            pub flags: u16,
            pub weight: f32,
        }
        """
        structs, _ = J.parse_rust_jrir(rust_src)
        @test haskey(structs, "Particle")
        @test haskey(structs, "PaddedHeader")

        p_layout = structs["Particle"]
        @test p_layout.size_bytes == 16
        @test p_layout.align_bytes == 4
        @test p_layout.padding_bytes == 0
        v_p = J.verify_julia_struct_layout(p_layout, TestParticle)
        @test v_p.valid
        @test v_p.field_offsets == ["x" => 0, "y" => 4, "vx" => 8, "vy" => 12]

        h_layout = structs["PaddedHeader"]
        @test h_layout.size_bytes == 24
        @test h_layout.align_bytes == 8
        @test h_layout.padding_bytes == 9
        v_h = J.verify_julia_struct_layout(h_layout, PaddedHeader)
        @test v_h.valid
        @test v_h.field_offsets == ["tag" => 0, "id" => 8, "flags" => 16, "weight" => 20]

        # Reject reordered / misaligned struct and non-isbitstype struct
        @test_throws DimensionMismatch J.verify_julia_struct_layout(h_layout, MisalignedHeader)
        @test_throws ArgumentError J.verify_julia_struct_layout(p_layout, NonIsBitsParticle)
    end

    @testset "2. Rust Signature & Slice Lowering Parser" begin
        rust_src = """
        #[no_mangle]
        pub unsafe extern "C" fn dot_slices(x: &[f32], y: &mut [f32], scale: f32) -> f32 {
            0.0
        }
        """
        _, fns = J.parse_rust_jrir(rust_src)
        @test haskey(fns, "dot_slices")
        f = fns["dot_slices"]
        @test f.ret_julia_type === Float32
        @test f.ret_llvm_type == "float"
        # 2 slices + 1 scalar = 5 ABI parameters
        @test length(f.params) == 5
        @test f.params[1].abi_kind == :slice_ptr && f.params[1].julia_abi_type === Ptr{Float32}
        @test f.params[2].abi_kind == :slice_len && f.params[2].julia_abi_type === UInt64
        @test f.params[3].abi_kind == :mut_slice_ptr && f.params[3].julia_abi_type === Ptr{Float32}
        @test f.params[4].abi_kind == :slice_len && f.params[4].julia_abi_type === UInt64
        @test f.params[5].abi_kind == :scalar && f.params[5].julia_abi_type === Float32
    end

    @testset "3. LLVM 19-23 -> LLVM 18 Cross-Version IR Normalizer & Zero-Alloc Execution" begin
        raw_rustc_22_particle = """
        ; ModuleID = 'kern.d1cb28c2384edfe3-cgu.0'
        source_filename = "kern.d1cb28c2384edfe3-cgu.0"
        target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
        target triple = "x86_64-unknown-linux-gnu"

        ; Function Attrs: mustprogress nofree norecurse nosync nounwind nonlazybind willreturn memory(argmem: readwrite) uwtable
        define noundef float @step_particle(ptr noalias noundef align 4 captures(none) dereferenceable(16) %p, float noundef %dt) unnamed_addr #0 {
        start:
          %0 = getelementptr inbounds nuw i8, ptr %p, i64 8
          %1 = load <2 x float>, ptr %0, align 4, !noundef !3
          %2 = load <2 x float>, ptr %p, align 4, !noundef !3
          %3 = insertelement <2 x float> poison, float %dt, i64 0
          %4 = shufflevector <2 x float> %3, <2 x float> poison, <2 x i32> zeroinitializer
          %5 = tail call <2 x float> @llvm.fma.v2f32(<2 x float> %1, <2 x float> %4, <2 x float> %2)
          store <2 x float> %5, ptr %p, align 4
          %foldExtExtBinop = fmul <2 x float> %5, %5
          %_11 = extractelement <2 x float> %foldExtExtBinop, i64 1
          %6 = extractelement <2 x float> %5, i64 0
          %7 = tail call float @llvm.fma.f32(float %6, float %6, float %_11)
          ret float %7
        }

        ; Function Attrs: mustprogress nocallback nocreateundeforpoison nofree nosync nounwind speculatable willreturn memory(none)
        declare float @llvm.fma.f32(float, float, float) #1

        ; Function Attrs: nocallback nocreateundeforpoison nofree nosync nounwind speculatable willreturn memory(none)
        declare <2 x float> @llvm.fma.v2f32(<2 x float>, <2 x float>, <2 x float>) #2

        attributes #0 = { mustprogress nofree norecurse nosync nounwind nonlazybind willreturn memory(argmem: readwrite) uwtable }
        !llvm.module.flags = !{!0}
        !0 = !{i32 8, !"PIC Level", i32 2}
        !3 = !{}
        """

        rust_src = """
        #[repr(C)]
        pub struct Particle {
            pub x: f32,
            pub y: f32,
            pub vx: f32,
            pub vy: f32,
        }

        #[no_mangle]
        pub extern "C" fn step_particle(p: &mut Particle, dt: f32) -> f32 {
            p.x = p.vx.mul_add(dt, p.x);
            p.y = p.vy.mul_add(dt, p.y);
            p.x.mul_add(p.x, p.y * p.y)
        }
        """

        mod = J.compile_jrir_module!("particle_mod"; rust_source=rust_src, llvm_ir=raw_rustc_22_particle)
        loop_fn! = J.build_simd_struct_loop!(mod, "step_particle", J.JRIRParticle)

        init = [J.JRIRParticle(Float32(i) * 0.01f0, Float32(-i) * 0.01f0, 0.5f0, -0.25f0) for i in 1:1024]
        ref_particles = copy(init)
        cand_particles = copy(init)

        ref_e = J.oracle_step_particles(ref_particles, 0.016f0)
        cand_e = loop_fn!(cand_particles, 0.016f0)

        @test cand_particles == ref_particles
        @test isapprox(cand_e, ref_e; rtol=1f-5)

        # Warmup and verify 0-byte hot-loop allocation & callq-free assembly
        loop_fn!(cand_particles, 0.016f0)
        alloc_bytes = @allocated loop_fn!(cand_particles, 0.016f0)
        @test alloc_bytes == 0

        asm = sprint(io -> code_native(io, loop_fn!, Tuple{Vector{J.JRIRParticle}, Float32}; debuginfo=:none))
        @test !occursin("callq", asm)

        J.jrir(Dict("action" => "drop", "module" => "particle_mod"))

        # clang ≤ 18 (Ubuntu 24.04's default) indexes fields through a named
        # struct type, so the normalizer must keep the type definition.
        raw_clang_18_particle = """
        ; ModuleID = 'jrir_kern.c'
        source_filename = "jrir_kern.c"
        target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"
        target triple = "x86_64-pc-linux-gnu"

        %struct.Particle = type { float, float, float, float }

        ; Function Attrs: mustprogress nofree norecurse nosync nounwind willreturn memory(argmem: readwrite) uwtable
        define dso_local float @step_particle(ptr nocapture noundef %0, float noundef %1) local_unnamed_addr #0 {
          %3 = getelementptr inbounds %struct.Particle, ptr %0, i64 0, i32 2
          %4 = load float, ptr %3, align 4, !tbaa !5
          %5 = load float, ptr %0, align 4, !tbaa !10
          %6 = tail call float @llvm.fmuladd.f32(float %4, float %1, float %5)
          store float %6, ptr %0, align 4, !tbaa !10
          %7 = getelementptr inbounds %struct.Particle, ptr %0, i64 0, i32 3
          %8 = load float, ptr %7, align 4, !tbaa !11
          %9 = getelementptr inbounds %struct.Particle, ptr %0, i64 0, i32 1
          %10 = load float, ptr %9, align 4, !tbaa !12
          %11 = tail call float @llvm.fmuladd.f32(float %8, float %1, float %10)
          store float %11, ptr %9, align 4, !tbaa !12
          %12 = fmul float %11, %11
          %13 = tail call float @llvm.fmuladd.f32(float %6, float %6, float %12)
          ret float %13
        }

        ; Function Attrs: mustprogress nocallback nofree nosync nounwind speculatable willreturn memory(none)
        declare float @llvm.fmuladd.f32(float, float, float) #1

        attributes #0 = { mustprogress nofree norecurse nosync nounwind willreturn memory(argmem: readwrite) uwtable }
        !5 = !{!6, !7, i64 8}
        """
        @test occursin("%struct.Particle = type { float, float, float, float }",
                       J.normalize_llvm_ir(raw_clang_18_particle; entry_fn="step_particle"))
        mod18 = J.compile_jrir_module!("particle_clang18"; rust_source=rust_src, llvm_ir=raw_clang_18_particle)
        loop18! = J.build_simd_struct_loop!(mod18, "step_particle", J.JRIRParticle)
        ref18 = copy(init)
        cand18 = copy(init)
        ref18_e = J.oracle_step_particles(ref18, 0.016f0)
        @test isapprox(loop18!(cand18, 0.016f0), ref18_e; rtol=1f-5)
        @test cand18 == ref18
        J.jrir(Dict("action" => "drop", "module" => "particle_clang18"))
    end

    @testset "4. Live Source Compilation (rustc or clang C99 fallback) & Capability API" begin
        diag = J.jrir(Dict("action" => "diagnose"))
        @test diag["status"] == "ok"
        @test diag["has_llvmcall"] == true

        rust_src = """
        #[repr(C)]
        pub struct Particle {
            pub x: f32,
            pub y: f32,
            pub vx: f32,
            pub vy: f32,
        }

        #[no_mangle]
        pub extern "C" fn step_particle(p: &mut Particle, dt: f32) -> f32 {
            p.x = p.vx.mul_add(dt, p.x);
            p.y = p.vy.mul_add(dt, p.y);
            p.x.mul_add(p.x, p.y * p.y)
        }
        """

        parsed = J.jrir(Dict("action" => "parse_rust", "rust_source" => rust_src))
        @test parsed["status"] == "ok"
        @test parsed["structs"]["Particle"]["size_bytes"] == 16
        @test haskey(parsed["functions"], "step_particle")

        comp = J.jrir(Dict("action" => "compile", "module" => "live_mod", "rust_source" => rust_src))
        @test comp["status"] == "compiled"
        @test "step_particle" in comp["functions"]

        st = J.jrir(Dict("action" => "stats", "module" => "live_mod"))
        @test st["status"] == "ok"
        @test st["ir_bytes"] > 50

        mod = J._JRIR_MODULES["live_mod"]
        loop_fn! = J.build_simd_struct_loop!(mod, "step_particle", J.JRIRParticle)
        pts = [J.JRIRParticle(1.0f0, 2.0f0, 0.5f0, -0.5f0) for _ in 1:64]
        ref_pts = copy(pts)
        e_ref = J.oracle_step_particles(ref_pts, 2.0f0)
        e_cand = loop_fn!(pts, 2.0f0)
        @test pts == ref_pts
        @test isapprox(e_cand, e_ref; rtol=1f-5)

        dropped = J.jrir(Dict("action" => "drop", "module" => "live_mod"))
        @test dropped["status"] == "dropped"
    end

    @testset "5. PEA Mode 2 Adversarial Input & Boundary Validation" begin
        @test_throws ArgumentError J.jrir(Dict("action" => "unknown_action"))
        @test_throws ArgumentError J.jrir(Dict("action" => "diagnose", "extra" => 1))
        @test_throws ArgumentError J.jrir(Dict("action" => "normalize_ir", "llvm_ir" => "define void @foo() {\nret void\n}", "entry_fn" => "missing_fn"))
        @test_throws ArgumentError J.compile_jrir_module!(""; rust_source="pub extern \"C\" fn f() {}")
        @test_throws ArgumentError J.compile_jrir_module!("valid_name")
    end

    @testset "6. Cyan/Palette Rust <-> Julia Seam Kernels (receipt_archive + digest strip_ansi)" begin
        # 1. Verify C-ABI layout of CivilDateTime (32 B, align 8 B, 0 B padding)
        structs, fns = J.parse_rust_jrir(J._CYAN_SEAMS_RUST_SCHEMA)
        @test haskey(structs, "CivilDateTime")
        c_layout = structs["CivilDateTime"]
        @test c_layout.size_bytes == 32
        @test c_layout.align_bytes == 8
        @test c_layout.padding_bytes == 0
        v_c = J.verify_julia_struct_layout(c_layout, J.JRIRCivilDateTime)
        @test v_c.valid

        # 2. Verify all 3 LLVM 22 normalizer cracks are stripped cleanly
        @test !occursin("initializes(", J._CYAN_SEAMS_NORM_IR)
        @test !occursin("rust_eh_personality", J._CYAN_SEAMS_NORM_IR)
        @test !occursin("range(", J._CYAN_SEAMS_NORM_IR)

        # 3. Verify compute_civil_dos against receipt_archive.rs test vectors (0 B allocated)
        dt_ref = Ref(J.JRIRCivilDateTime(0, 0, 0, 0, 0, 0, 0, 0))
        J.jrir_compute_civil_dos!(Int64(1_792_195_199), dt_ref)
        dt = dt_ref[]
        @test (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second) == (2026, 10, 16, 23, 59, 59)

        dos_leap = J.jrir_compute_civil_dos!(Int64(1_709_209_096), dt_ref)
        dt_leap = dt_ref[]
        @test (dt_leap.year, dt_leap.month, dt_leap.day, dt_leap.hour, dt_leap.minute, dt_leap.second) == (2024, 2, 29, 12, 18, 16)
        @test dt_leap.dos_date == UInt16(((2024 - 1980) << 9) | (2 << 5) | 29)
        @test dt_leap.dos_time == UInt16((12 << 11) | (18 << 5) | (16 >> 1))
        @test dos_leap == ((UInt32(dt_leap.dos_date) << 16) | UInt32(dt_leap.dos_time))
        @test (@allocated J.jrir_compute_civil_dos!(Int64(1_792_195_199), dt_ref)) == 0

        # 4. Verify crc32_ieee_slice against IEEE 802.3 test vector "123456789" -> 0xCBF43926 (0 B allocated)
        crc_input = Vector{UInt8}(codeunits("123456789"))
        @test J.jrir_crc32_ieee(crc_input) == 0xCBF43926
        @test (@allocated J.jrir_crc32_ieee(crc_input)) == 0

        # 5. Verify strip_ansi_slice against Regex(r"\e\[[0-9;]*[a-zA-Z]") (0 B allocated in-place)
        ansi_sample = "\e[32m   Compiling\e[0m palette-host v0.1.0\n\e[1;33mwarning\e[0m: unused variable `x`\n" ^ 64
        src_bytes = Vector{UInt8}(codeunits(ansi_sample))
        dst_bytes = Vector{UInt8}(undef, length(src_bytes))
        n_out = J.jrir_strip_ansi!(dst_bytes, src_bytes)
        @test String(dst_bytes[1:n_out]) == replace(ansi_sample, r"\e\[[0-9;]*[a-zA-Z]" => "")
        @test J.jrir_strip_ansi(ansi_sample) == replace(ansi_sample, r"\e\[[0-9;]*[a-zA-Z]" => "")
        @test (@allocated J.jrir_strip_ansi!(dst_bytes, src_bytes)) == 0
    end

    @testset "7. strip_ansi Malformed/Unterminated CSI + Adversarial Fuzz vs src/digest.jl Regex" begin
        oracle(s) = replace(s, r"\e\[[0-9;?]*[A-Za-z]" => "")
        @test J.jrir_strip_ansi("progress \e[ 50% done") == "progress \e[ 50% done"
        @test J.jrir_strip_ansi("\e[1;31") == "\e[1;31"
        @test J.jrir_strip_ansi("\e[1;\e[31mX") == "\e[1;X"
        @test J.jrir_strip_ansi("\e[?25l\e[?25h") == ""
        @test J.jrir_strip_ansi("") == ""
        @test J.jrir_strip_ansi("\e") == "\e"
        @test J.jrir_strip_ansi("\e[") == "\e["

        rng = MersenneTwister(20261019)
        alphabet = UInt8['\e', '[', '0', '1', '9', ';', '?', 'm', 'A', 'z', ' ', '%', 'x', '\n']
        mismatches = 0
        for _ in 1:2_000
            n = rand(rng, 0:48)
            s = String([rand(rng, alphabet) for _ in 1:n])
            J.jrir_strip_ansi(s) == oracle(s) || (mismatches += 1)
        end
        @test mismatches == 0
    end

    @testset "8. Capability manifest matches RuntimeRegistry schema" begin
        allowed = Set(["id", "version", "source", "entrypoint", "dependencies", "requires", "description"])
        authority = Set(["external_fs_write", "fs_digest", "host_command", "network_access",
                         "package_management", "spawn_child_worker", "teamlab"])
        for cap in ("jrir", "vecindex", "teamlab")
            path = joinpath(@__DIR__, "..", cap, "capability.json")
            isfile(path) || continue
            txt = read(path, String)
            keys_found = [m.captures[1] for m in eachmatch(r"^\s*\"([A-Za-z_]+)\"\s*:"m, txt)]
            @test all(k -> k in allowed, keys_found)
            @test occursin(Regex("\"entrypoint\"\\s*:\\s*\"$cap\""), txt)
            req = match(r"\"requires\"\s*:\s*\[([^\]]*)\]", txt)
            if req !== nothing
                src = read(joinpath(dirname(path), cap * ".jl"), String)
                for m in eachmatch(r"\"([^\"]+)\"", req.captures[1])
                    @test m.captures[1] in authority
                    # A requirement the source never exercises only hides the capability.
                    m.captures[1] == "host_command" && @test occursin("host_command(", src)
                end
            end
        end
        @test isfile(joinpath(@__DIR__, "cyan_seams.rs"))
        @test isdefined(J, :regenerate_cyan_seams_ir)
    end
end

