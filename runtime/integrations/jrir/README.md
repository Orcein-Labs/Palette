# Optional `jrir` Capability (Julia ↔ Rust Intermediate Representation)

Zero-dependency, pure-Julia cross-language IR & ABI fusion capability for Palette (`"dependencies": []`).
Eliminates FFI/ABI boilerplate and dynamic library (`callq` / PLT) barriers between Rust and Julia by sharing a canonical C-ABI layout schema and splicing normalized `rustc` / `clang` LLVM IR directly into Julia's LLVM optimization pipeline via `Base.llvmcall`.

## Architecture

1. **Canonical JRIR Schema & Deterministic C-ABI Verifier (`parse_rust_jrir`, `compute_c_layout`, `verify_julia_struct_layout`)**
   - Parses `#[repr(C)] pub struct` declarations and `#[no_mangle] pub extern "C" fn` signatures directly from Rust source text.
   - Automatically lowers Rust slice references (`&[T]` and `&mut [T]`) into 2-word fat-pointer ABI pairs `(ptr::Ptr{T}, len::UInt64)`.
   - Computes exact System-V / C-ABI `offset_bytes`, `size_bytes`, `align_bytes`, and `padding_bytes`, and cross-checks them against any corresponding Julia `isbitstype` struct via `sizeof(T)`, `Base.datatype_alignment(T)`, and `fieldoffset(T, i)`.

2. **Cross-Version LLVM 19–23 → LLVM 18 IR Normalizer (`normalize_llvm_ir`)**
   - Bridges the LLVM version gap between modern `rustc` (LLVM 22+) / `clang` (LLVM 23+) and Julia 1.12 (LLVM 18.1.7).
   - Normalizes post-instruction flags (`getelementptr inbounds nuw` → `getelementptr inbounds`, `trunc nuw nsw` → `trunc`, `zext nneg` → `zext`, `uitofp nneg` → `uitofp`, `or disjoint` → `or`, `icmp samesign` → `icmp`), nested parameter/instruction attributes (`captures(none)` → `nocapture`, `initializes((0, 32))`, inline `range(i64 ...)`), `personality ptr @rust_eh_personality` on `panic=abort` functions, and metadata attachments (`!noundef`, `!llvm.loop`), while marking entry functions `alwaysinline`.

3. **Zero-Allocation `Base.llvmcall` Inlining & SIMD Loop Fusion (`compile_jrir_module!`, `build_simd_struct_loop!`)**
   - Splices normalized Rust LLVM IR directly into Julia's `@inbounds @simd` loops, allowing LLVM to inline the Rust function and 4x SIMD-unroll across struct arrays with **zero `callq` instructions** and **0 bytes allocated**.

4. **Built-In Cyan / Palette Rust ↔ Julia Seam Kernels**
   - `jrir_compute_civil_dos!(secs::Int64, out_ref::Ref{JRIRCivilDateTime}) -> UInt32`: Zero-allocation Howard Hinnant civil calendar + DOS timestamp kernel compiled from `runtime/host/src/receipt_archive.rs:337-365`.
   - `jrir_crc32_ieee(data, seed::UInt32=0x00000000) -> UInt32`: Zero-allocation SIMD `<4 x i32>` xor-reduction IEEE 802.3 CRC32 kernel compiled from `runtime/host/src/receipt_archive.rs:288-319`.
   - `jrir_strip_ansi!(dst::Vector{UInt8}, src)` / `jrir_strip_ansi(s::AbstractString)`: Closes the `src/digest.jl:14` `strip_ansi` regex allocation seam, achieving a **`12.8x` speedup** (`51.34 µs` vs `658.13 µs` on a 69 KB ANSI log) with **`0 B` allocated** in-place.

## Benchmarks ($N = 100,000$ `Particle` Struct Updates, `Float32`)

| Execution Path | Post-Warmup Latency | Heap Allocations | Speedup vs Naive FFI | Speedup vs Cached `.so` Loop | `callq` in Loop |
| :--- | ---: | ---: | ---: | ---: | :---: |
| Baseline 0: Dynamic `dlsym` + `ccall` per element | `10,808.72 µs` | `0 B` | `1.0x` | `0.02x` | Yes (`100,000`) |
| Candidate 1: Cached pointer `.so` `ccall` in loop | `194.82 µs` | `0 B` | `55.5x` | `1.00x` | Yes (`100,000`) |
| Candidate 2: Batch slice `.so` `ccall` (`(ptr, len)`) | `134.87 µs` | `0 B` | `80.1x` | `1.44x` | Yes (`1`) |
| **Candidate 3: JRIR `Base.llvmcall` (`alwaysinline` + `@simd`)** | **`100.53 µs`** | **`0 B`** | **`107.5x`** | **`1.94x`** | **No (`0`)** |

