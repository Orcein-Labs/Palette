using Test
isdefined(@__MODULE__, :Pharos) || include(joinpath(@__DIR__, "..", "src", "pharos.jl"))
using .Pharos

_write(path, text) = (mkpath(dirname(path)); write(path, text); path)
_defs(e) = Dict(d.name => d.kind for d in e.definitions)
_calls(e) = [(c.caller, c.callee) for c in e.calls]

@testset "Pharos" begin

    @testset "Julia extraction" begin
        d = mktempdir()
        src = """
        module Outer
        using ..Parent, Base.Threads
        using A.B: c, d
        import Foo
        include("inner.jl")
        include(@__DIR__, "other.jl")
        abstract type Abs <: Number end
        primitive type Bits 8 end
        struct Pair{T} <: AbstractPair
            a::T
        end
        mutable struct Box
            v
        end
        const LIMIT = 10
        const Typed::Int = 3
        const ta, tb = 1, 2
        "docstring"
        function long(x)
            return g(x) + 1
        end
        short(x) = h(x)
        Base.show(io::IO, x) = print(io, x)
        Base.:+(a, b) = Base.add(a, b)
        function zero_method end
        typed(x::T)::Int where T = k(x)
        @inline function inl(x)
            x .+ f.(x)
        end
        macro mymac(ex)
            esc(ex)
        end
        end
        """
        f = _write(joinpath(d, "mod.jl"), src)
        g = CodeGraph(d)
        e = g.files["mod.jl"]
        @test e.error === nothing
        @test !e.approximate
        @test e.language === :julia
        k = _defs(e)
        @test k["Outer"] === :module
        @test k["Abs"] === :abstract
        @test k["Bits"] === :primitive
        @test k["Pair"] === :struct
        @test k["Box"] === :struct
        @test k["LIMIT"] === :const
        @test k["Typed"] === :const
        @test k["ta"] === :const && k["tb"] === :const
        @test k["long"] === :function
        @test k["short"] === :function
        @test k["Base.show"] === :function
        @test k["Base.+"] === :function
        @test k["zero_method"] === :function
        @test k["typed"] === :function
        @test k["inl"] === :function
        @test k["@mymac"] === :macro
        @test !haskey(k, "x")
        cs = _calls(e)
        @test ("long", "g") in cs
        @test ("short", "h") in cs
        @test ("Base.show", "print") in cs
        @test ("inl", "f") in cs
        @test ("typed", "k") in cs
        @test any(c -> c[2] == "Base.add", cs)
        @test e.includes == ["inner.jl", "other.jl"]
        @test "..Parent" in e.imports
        @test "Base.Threads" in e.imports
        @test "A.B" in e.imports
        @test "Foo" in e.imports
        @test any(c -> c.caller == "Outer" && c.callee == "include", e.calls)
    end

    @testset "Julia syntax error keeps earlier definitions" begin
        d = mktempdir()
        _write(joinpath(d, "broken.jl"), """
        function before(x)
            x + 1
        end
        const KEEP = 1
        function broken(
        """)
        e = CodeGraph(d).files["broken.jl"]
        @test e.error !== nothing
        names = [x.name for x in e.definitions]
        @test "before" in names
        @test "KEEP" in names
    end

    @testset "Python extraction" begin
        if Sys.which("python3") === nothing
            @test_skip false
        else
            d = mktempdir()
            _write(joinpath(d, "m.py"), """
            import os.path
            from . import sibling
            from .pkg import thing

            def plain(x):
                return x

            async def fetch():
                pass

            class C:
                def m(self):
                    pass
            """)
            e = CodeGraph(d).files["m.py"]
            @test e.error === nothing
            @test !e.approximate
            k = _defs(e)
            @test k["plain"] === :function
            @test k["fetch"] === :function
            @test k["C"] === :class
            @test k["C.m"] === :function
            @test "os.path" in e.imports
            @test "." in e.imports
            @test ".pkg" in e.imports
        end
    end

    @testset "Rust extraction" begin
        d = mktempdir()
        _write(joinpath(d, "lib.rs"), """
        use std::fmt;
        pub use crate::inner::Thing;
        // fn commented_out() {}
        fn private() {}
        pub fn public() {}
        pub(crate) async fn crate_async() {}
        pub struct Point { x: i32 }
        enum Color { Red }
        pub trait Shape { fn area(&self) -> f64; }
        impl fmt::Display for Point {
            fn fmt(&self) {}
        }
        macro_rules! make {
            () => {};
        }
        """)
        e = CodeGraph(d).files["lib.rs"]
        @test e.approximate
        @test e.language === :rust
        k = _defs(e)
        @test k["private"] === :fn
        @test k["public"] === :fn
        @test k["crate_async"] === :fn
        @test k["Point"] === :struct
        @test k["Color"] === :enum
        @test k["Shape"] === :trait
        @test k["fmt::Display for Point"] === :impl
        @test k["make"] === :macro
        @test !haskey(k, "commented_out")
        @test "std::fmt" in e.imports
        @test "crate::inner::Thing" in e.imports
    end

    @testset "source_files" begin
        d = mktempdir()
        _write(joinpath(d, "a.jl"), "f() = 1\n")
        _write(joinpath(d, "sub", "b.py"), "def g(): pass\n")
        _write(joinpath(d, "sub", "c.txt"), "not source\n")
        _write(joinpath(d, ".git", "hook.jl"), "x = 1\n")
        _write(joinpath(d, "vendor", "v.jl"), "x = 1\n")
        _write(joinpath(d, "target", "t.rs"), "fn t() {}\n")
        _write(joinpath(d, "node_modules", "n.py"), "x = 1\n")
        _write(joinpath(d, "build", "bld.jl"), "x = 1\n")
        _write(joinpath(d, "lib", "ok.rs"), "fn ok() {}\n")
        symlink(d, joinpath(d, "sub", "loop"))
        paths = source_files(d)
        @test paths == ["a.jl", "lib/ok.rs", "sub/b.py"]
    end

    if Sys.which("git") !== nothing
        @testset "source_files in a git work tree: tracked and untracked, not ignored" begin
            d = mktempdir()
            run(pipeline(`git -C $d init -q`, stdout=devnull, stderr=devnull))
            _write(joinpath(d, ".gitignore"), "out/\n")
            _write(joinpath(d, "a.jl"), "f() = 1\n")
            _write(joinpath(d, "out", "gen.jl"), "x = 1\n")
            _write(joinpath(d, "vendor", "v.rs"), "fn v() {}\n")
            run(`git -C $d add a.jl`)
            @test source_files(d) == ["a.jl", "vendor/v.rs"]
        end
    end

    @testset "refresh! stamps, hashes and removals" begin
        d = mktempdir()
        p1 = _write(joinpath(d, "one.jl"), "f(x) = g(x)\n")
        _write(joinpath(d, "two.jl"), "h() = 1\n")
        _write(joinpath(d, "three.py"), "def p(): pass\n")
        _write(joinpath(d, "four.rs"), "fn q() {}\n")
        _write(joinpath(d, "five.jl"), "const Z = 2\n")
        g = CodeGraph(d)
        @test length(g.files) == 5
        @test g.last["reparsed"] == 5

        refresh!(g)
        @test g.last["reparsed"] == 0
        @test g.last["hashed"] == 0
        @test g.last["removed"] == 0

        d1 = digest(g)
        @test d1 == digest(g)

        run(`touch -d @1700000000 $p1`)
        refresh!(g)
        @test g.last["hashed"] == 1
        @test g.last["reparsed"] == 0
        @test digest(g) == d1

        write(p1, "f(x) = k(x)\n")
        refresh!(g)
        @test g.last["reparsed"] == 1
        @test g.last["reparsed_paths"] == ["one.jl"]
        @test digest(g) != d1
        @test any(c -> c == ("f", "k"), _calls(g.files["one.jl"]))

        rm(joinpath(d, "two.jl"))
        refresh!(g)
        @test g.last["removed"] == 1
        @test !haskey(g.files, "two.jl")

        out = sprint(show, MIME"text/plain"(), g)
        @test count(==('\n'), out) <= 5
        @test occursin("julia", out) && occursin("root", out)
    end

    @testset "definitions and callers" begin
        d = mktempdir()
        _write(joinpath(d, "lib.jl"), """
        Base.show(io, x) = print(io, x)
        function g(x)
            return x
        end
        function f(x)
            g(x)
            Base.push!(x, 1)
        end
        """)
        g = CodeGraph(d)
        defs = definitions(g, "show")
        @test any(t -> t[2].name == "Base.show", defs)
        cs = callers(g, "g")
        @test length(cs) == 1
        @test cs[1][2].caller == "f"
        @test any(t -> t[2].caller == "f", callers(g, "push!"))
        @test isempty(definitions(g, "how"))
    end

    @testset "CodeGraph with explicit files list" begin
        d = mktempdir()
        _write(joinpath(d, "keep.jl"), "k() = 1\n")
        _write(joinpath(d, "skip.jl"), "s() = 1\n")
        g = CodeGraph(d; files=["keep.jl"])
        @test collect(keys(g.files)) == ["keep.jl"]
    end

    @testset "SHA-256 Fast Path edge cases & parity" begin
        cases = [
            "",
            "a",
            "The quick brown fox jumps over the lazy dog",
            "embedded\0nul\0bytes",
            "Unicode αβγδε 你好 🚀🔥",
            repeat("Z", 100_000)
        ]
        for c in cases
            @test Pharos._sha256_hex(c) == bytes2hex(Pharos.SHA.sha256(c))
        end
    end

    if Sys.which("git") !== nothing
        @testset "Incremental Git refresh adversarial mutations" begin
            d = mktempdir()
            run(pipeline(`git -C $d init -q`, stdout=devnull, stderr=devnull))
            run(`git -C $d config user.name "Test"`)
            run(`git -C $d config user.email "test@example.com"`)

            _write(joinpath(d, "src", "a.jl"), "f(x) = x + 1\n")
            _write(joinpath(d, "src", "b.py"), "def run(): pass\n")
            run(`git -C $d add .`)
            run(`git -C $d commit -q -m "init"`)

            g = CodeGraph(d)
            @test length(g.files) == 2
            @test g.last["strategy"] == :full

            # 1. Zero change warm refresh
            refresh!(g)
            @test g.last["strategy"] == :git_clean
            @test g.last["reparsed"] == 0
            @test g.last["hashed"] == 0

            # 2. In-place edit of tracked file
            write(joinpath(d, "src", "a.jl"), "f(x) = x + 10\n")
            refresh!(g)
            @test g.last["strategy"] == :git_incremental
            @test g.last["reparsed"] == 1
            @test g.last["reparsed_paths"] == ["src/a.jl"]
            run(`git -C $d commit -q -am "update a"`)
            g.git_head = Pharos._git_head(d)

            # 3. New untracked file
            _write(joinpath(d, "src", "new.rs"), "fn foo() {}\n")
            refresh!(g)
            @test g.last["strategy"] == :git_incremental
            @test haskey(g.files, "src/new.rs")
            @test "src/new.rs" in g.last["reparsed_paths"]

            # 4. Deletion of untracked file
            rm(joinpath(d, "src", "new.rs"))
            refresh!(g)
            @test g.last["strategy"] == :git_incremental
            @test !haskey(g.files, "src/new.rs")
            @test g.last["removed"] == 1

            # 5. Non-source change (e.g. documentation or config)
            write(joinpath(d, "README.md"), "# Title\n")
            refresh!(g)
            @test g.last["strategy"] == :git_non_source
            @test g.last["reparsed"] == 0

            # 6. Digest parity with fresh full scan
            g_fresh = CodeGraph(d)
            @test digest(g) == digest(g_fresh)

            # 7. Explicit full refresh fallback
            refresh!(g; full=true)
            @test g.last["strategy"] == :full
            @test digest(g) == digest(g_fresh)
        end
    end

    if Sys.which("python3") !== nothing
        @testset "Python AST statement traversal & complex syntax" begin
            d = mktempdir()
            _write(joinpath(d, "complex.py"), """
            import sys, os.path as osp
            from math import sin, cos
            from ..parent import helper

            GLOBAL_CONST = 100

            def outer_func(x):
                def inner_func(y):
                    return x + y
                return inner_func

            class OuterClass:
                class NestedClass:
                    def nested_method(self):
                        pass
                async def async_method(self):
                    pass

            if True:
                def branch_func(): pass
            else:
                def other_branch(): pass

            try:
                def try_func(): pass
            except Exception:
                def except_func(): pass
            finally:
                def finally_func(): pass

            with open(__file__) as f:
                def with_func(): pass
            """)
            g = CodeGraph(d)
            e = g.files["complex.py"]
            @test e.error === nothing
            defs = _defs(e)
            @test defs["outer_func"] === :function
            @test defs["outer_func.inner_func"] === :function
            @test defs["OuterClass"] === :class
            @test defs["OuterClass.NestedClass"] === :class
            @test defs["OuterClass.NestedClass.nested_method"] === :function
            @test defs["OuterClass.async_method"] === :function
            @test defs["branch_func"] === :function
            @test defs["other_branch"] === :function
            @test defs["try_func"] === :function
            @test defs["except_func"] === :function
            @test defs["finally_func"] === :function
            @test defs["with_func"] === :function
            @test "sys" in e.imports
            @test "os.path" in e.imports
            @test "math" in e.imports
            @test "..parent" in e.imports
        end
    end

    @testset "Rust advanced extraction (comments, attributes, generics, mod/type/const)" begin
        d = mktempdir()
        try
            _write(joinpath(d, "adv.rs"), """
            pub mod submod;
            mod inline_mod {
                pub type HandleId = u64;
            }
            pub const MAX_ELEMS: usize = 1024;
            pub static GLOBAL_LOCK: u32 = 0;
            /*
               fn inside_block_comment() {}
               /* struct NestedCommentFake {} */
            */
            const FAKE_IN_STR: &str = "/* fn fake_str() {} */ // not a comment";
            let raw_str = r#"/* struct RawFake {} */"#;
            #[inline]
            pub const fn fast_const_fn() -> usize { 42 }
            #[cfg(test)] #[allow(dead_code)] pub fn attr_inline_fn() {}
            pub type BoxedResult<T> = Result<T, ()>;
            struct Holder<T>(T);
            impl<T: Into<Vec<u8>>> Holder<T> where T: Clone {
                pub fn inner(&self) {}
            }
            """)
            e = CodeGraph(d).files["adv.rs"]
            @test e.language === :rust
            k = _defs(e)
            @test k["submod"] === :mod
            @test k["inline_mod"] === :mod
            @test k["HandleId"] === :type
            @test k["MAX_ELEMS"] === :const
            @test k["GLOBAL_LOCK"] === :static
            @test k["fast_const_fn"] === :fn
            @test k["attr_inline_fn"] === :fn
            @test k["BoxedResult"] === :type
            @test k["Holder"] === :struct
            @test k["Holder<T>"] === :impl
            @test k["inner"] === :fn
            @test !haskey(k, "inside_block_comment")
            @test !haskey(k, "NestedCommentFake")
            @test !haskey(k, "fake_str")
            @test !haskey(k, "RawFake")
        finally
            rm(d; recursive=true, force=true)
        end
    end

    @testset "WGSL extraction & multi-language grep_defs" begin
        d = mktempdir()
        try
            _write(joinpath(d, "shaders", "pass.wgsl"), """
            // fn commented_line() {}
            /*
               struct BlockCommented {}
               /* var nested_commented: f32; */
            */
            alias Vec4f = vec4<f32>;
            const WORKGROUP_SIZE: u32 = 64u;
            @id(0) override ENABLE_BLOOM: bool = true;

            struct Uniforms {
                mvp: mat4x4<f32>,
                exposure: f32,
            };

            @group(0) @binding(0) var<uniform> uniforms: Uniforms;
            @group(0) @binding(1) var<storage, read_write> out_pixels: array<Vec4f>;
            @group(1) @binding(0) var color_tex: texture_2d<f32>;

            @compute @workgroup_size(64, 1, 1)
            fn cs_render(@builtin(global_invocation_id) gid: vec3<u32>) {
                var local_tmp: f32 = 1.0;
                const local_k: u32 = 2u;
                out_pixels[gid.x] = Vec4f(local_tmp);
            }

            fn helper_shade(v: Vec4f) -> Vec4f {
                var local_accum: Vec4f = v;
                return local_accum;
            }
            """)
            _write(joinpath(d, "src", "renderer.rs"), """
            pub struct RenderPass { id: u32 }
            pub fn render_frame() {}
            """)
            _write(joinpath(d, "src", "bridge.jl"), """
            struct RenderConfig
                width::Int
            end
            render_dispatch(cfg::RenderConfig) = cfg.width
            """)

            g = CodeGraph(d)
            @test haskey(g.files, "shaders/pass.wgsl")
            ew = g.files["shaders/pass.wgsl"]
            @test ew.language === :wgsl
            @test ew.approximate
            kw = _defs(ew)
            @test kw["Vec4f"] === :alias
            @test kw["WORKGROUP_SIZE"] === :const
            @test kw["ENABLE_BLOOM"] === :override
            @test kw["Uniforms"] === :struct
            @test kw["uniforms"] === :var
            @test kw["out_pixels"] === :var
            @test kw["color_tex"] === :var
            @test kw["cs_render"] === :fn
            @test kw["helper_shade"] === :fn
            @test !haskey(kw, "commented_line")
            @test !haskey(kw, "BlockCommented")
            @test !haskey(kw, "nested_commented")
            @test !haskey(kw, "local_tmp")
            @test !haskey(kw, "local_k")
            @test !haskey(kw, "local_accum")

            # show() remains compact (<= 5 newlines) even with wgsl line included
            out = sprint(show, MIME"text/plain"(), g)
            @test count(==('\n'), out) <= 5
            @test occursin("wgsl", out)

            # grep_defs across all languages vs exts filtering & regex
            all_render = grep_defs(g, "render"; ignorecase=true)
            @test [(m.file, m.kind, m.name) for m in all_render] == [
                ("shaders/pass.wgsl", :fn, "cs_render"),
                ("src/bridge.jl", :struct, "RenderConfig"),
                ("src/bridge.jl", :function, "render_dispatch"),
                ("src/renderer.rs", :struct, "RenderPass"),
                ("src/renderer.rs", :fn, "render_frame"),
            ]

            wgsl_only = grep_defs(g, "render"; exts=(".wgsl",), ignorecase=true)
            @test [(m.file, m.name) for m in wgsl_only] == [("shaders/pass.wgsl", "cs_render")]

            rs_wgsl = grep_defs(g, r"^(?:Render|Uniforms|cs_)"; exts=("rs", "wgsl"))
            @test [(m.file, m.kind, m.name) for m in rs_wgsl] == [
                ("shaders/pass.wgsl", :struct, "Uniforms"),
                ("shaders/pass.wgsl", :fn, "cs_render"),
                ("src/renderer.rs", :struct, "RenderPass"),
            ]

            # Empty exts matches nothing cleanly
            @test isempty(grep_defs(g, "render"; exts=String[], ignorecase=true))

            # summary includes wgsl files & alias types
            sm = Pharos.summary(g)
            @test sm.file_count == 3
            wg_sum = only(filter(f -> f.path == "shaders/pass.wgsl", sm.files))
            @test [f.name for f in wg_sum.functions] == ["cs_render", "helper_shade"]
            @test Set([t.name for t in wg_sum.types]) == Set(["Vec4f", "Uniforms"])
        finally
            rm(d; recursive=true, force=true)
        end
    end
end
