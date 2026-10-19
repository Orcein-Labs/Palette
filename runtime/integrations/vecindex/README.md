# Optional `vecindex` Capability

Zero-allocation, pure-Julia vector and subspace search capability for Palette.
Uses only Julia standard libraries (`LinearAlgebra`, `Random`, `SHA`) so it
requires no external package dependencies (`"dependencies": []`).

## Architecture

1. **Contiguous-Cluster Inverted File (`IVFIndex`) + 4-Way AVX2 FMA Register Blocking (`_scan_columns_reg4!`)**
   - `build_ivf!` partitions unit-normalized `Float32` embeddings into `n_list` coarse centroids via BLAS `gemm!` spherical $k$-means and permutes database vectors contiguously by cluster into `embeddings_perm::Matrix{Float32}` (`d × indexed_count`).
   - Probing a cluster streams contiguous cache lines in column-major order.
   - `_scan_columns_reg4!` processes 4 database columns simultaneously per `@simd` pass over `1:d` using `muladd`, loading `q_norm[i]` into AVX2 registers once per 4 columns and fusing top-$k$ insertion inline without writing intermediate score arrays.
   - Any items appended after `build_ivf!` (`ivf.indexed_count + 1 : store.count`) are scanned in-place via `_scan_columns_reg4!` at query time so incremental inserts have 100% recall before an IVF rebuild.
2. **Exact BLAS Engine (`search_exact!`) & `Palette.reference!` Integration**
   - Uses `BLAS.gemv!('T', 1.0f0, mat_view, q_norm, 0.0f0, scores_view)` and in-place `select_topk_max!`, allocating **0 bytes** in the hot loop.
   - `register_palette_hooks!()` binds `Palette.reference!(search_ivf_ids, Tuple{VecStore,Vector{Float32},Int}, search_exact_ids)` so running under `Palette.context!("reference")` automatically cross-checks IVF results against exact BLAS.
3. **Zero-Allocation Text, Duck-Type Subspace Search & Polyglot Codegraph Indexing (`embed_text!`, `index_codegraph!`, `compute_duck_mask!`)**
   - Single-pass UTF-8 codeunit scanner with SplitMix64 feature hashing over case-folded whole tokens (weight `2.0`), `snake_case` / `camelCase` / Julia `!` sub-tokens (weight `1.5`), and character 3/4-grams (weight `0.75`).
   - `index_codegraph!` indexes `Palette.codegraph()` symbols plus a polyglot file scanner (`.jl`, `.py`, `.rs`, `.ts`, `.tsx`, `.js`, `.jsx`, `.go`, `.c`, `.cpp`, `.h`, `.cs`), extracting structural duck-type traits (`fields`, `callees`, `params`, `arity`, `mutating`, `kind`, `language`) per symbol.
   - `compute_duck_mask!(store, filter)` populates a preallocated `BitVector` (`store.filter_mask`) that is applied inline inside `select_topk_max!` and `_scan_columns_reg4!` with **0 bytes allocated** during masked `search_exact!` / `search_ivf!`, or scanned directly for pure structural duck-type queries (when no `query_text` or `query` vector is supplied).

## Usage via `Palette.runtime`

```julia
Palette.runtime("invoke", Dict("id" => "vecindex",
    "arguments" => Dict("action" => "diagnose")))

Palette.runtime("invoke", Dict("id" => "vecindex",
    "arguments" => Dict("action" => "index_codegraph", "store" => "code")))

# Semantic + structural duck-type subspace search:
Palette.runtime("invoke", Dict("id" => "vecindex",
    "arguments" => Dict(
        "action" => "search",
        "store" => "code",
        "query_text" => "inventory digest verification",
        "k" => 5,
        "filter" => Dict(
            "language" => "julia",
            "kind" => "function",
            "has_callees_all" => ["sha256", "bytes2hex"],
        ),
    )))

# Pure structural duck-type query (find everything touching both .mgr and .tokenizer):
Palette.runtime("invoke", Dict("id" => "vecindex",
    "arguments" => Dict(
        "action" => "search",
        "store" => "code",
        "filter" => Dict("has_fields_all" => ["mgr", "tokenizer"]),
    )))

Palette.runtime("invoke", Dict("id" => "vecindex",
    "arguments" => Dict("action" => "drop", "store" => "code")))
```

## Benchmarks ($N = 20,000$, $d = 256$, `Float32`, $k = 10$)

| Kernel | Post-Warmup Latency | Heap Allocations | Speedup vs Naive | Recall@10 |
| :--- | ---: | ---: | ---: | ---: |
| Naive normalizing cosine oracle | `10,353.2 µs` | `44,066.6 KB` | `1.0x` | `1.00` |
| `mul!` + `partialsortperm` | `384.0 µs` | `241.7 KB` | `27.0x` | `1.00` |
| `search_exact!` (`BLAS.gemv!` + `select_topk_max!`) | `118.4 µs` | `0 B` | `87.5x` | `1.00` |
| `search_ivf!` (Contiguous IVF + 4-way AVX2 FMA) | **`56.0 µs`** | **`0 B`** | **`185.0x`** | **`1.00`** |
