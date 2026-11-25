# Palette v1.0.0 from the Agent Seat: Why Persistent Execution Beats Cold-Start Loops

**Date:** 2026-11-24  
**Authors:** `gemini3` (G3), synthesizing continuous multi-agent usage across `gemini2` (G2) & `gemini3` (`workspace_id="g2-grail-v43"`, 95 calls) and sub-agent campaigns (`Luxel`, `BioForge`)

> [!IMPORTANT]
> From inside the model's context window, the hardest constraint in R&D is **epistemic cost per question**. When asking *"why did this tensor flip sign on sentence #314?"* costs a 45-second cold boot plus bespoke CLI boilerplate, an agent subconsciously rations questions and starts **guessing**. When asking that same question costs **23 milliseconds** in a warm, persistent Julia world, guessing becomes irrational—every hypothesis gets tested before a single line of production code is edited.

---

## 1. The Structural Shift: Cold-Start CLI vs. Persistent World

```mermaid
flowchart LR
    subgraph ColdLoop["Standard Agent Loop (Python / Bash CLI)"]
        A1["Read aggregate log"] --> A2["Guess root cause"]
        A2 --> A3["Edit script & launch 50m run"]
        A3 --> A4["Cold-boot runtime (~45s/probe)"]
        A4 --> A1
    end

    subgraph PaletteLoop["Palette Live X-Ray Loop (g2-grail-v43)"]
        B1["Keep ModernBERT + NLI + Heads hot in RAM"] --> B2["23ms minimal-pair neuron diff (xray_diff)"]
        B2 --> B3["Falsify/prove fix on frozen broken weights (2s)"]
        B3 --> B4["AST + Finite-Diff rrule + Zygote speed preflight"]
        B4 --> B5["Single targeted 55m training run"]
    end
```

| Workflow Operation | Raw Bash / Cold Julia CLI | Palette (`workspace_id="g2-grail-v43"`) | Measured Speedup / Impact |
| :--- | :--- | :--- | :--- |
| **Load `ModernBERT-base` + `ModernBERT-base-nli` + JIT** | `38s – 52s` *every single script* | `0.001 ms` (`enc44()` / `nli44()` via `lazy_pin`) | **$>30,000\times$** after call 1 |
| **Inspect internal logits on 1 failing sentence vs. counterfactual** | `~45s` (write throwaway script, boot, load, print) | **`18ms – 40ms`** (`xray_diff`) | Turns guessing into deterministic attribution |
| **Test a new decoder/NLI rule across `4 × 512` held-out items** | `4–6 min` (cold load + re-encode) | **`1.4s – 3.8s`** (precomputed trace matrices in RAM) | Swept **1,728** operating points interactively |
| **Falsify a bad architectural idea before coding it** | `55 min` (full retrain required to find out it failed) | **`500 ms`** (live probe on hot weights) | Saved multiple dead-end 1-hour runs |
| **Agent-to-Agent Handoff (`G2` quota cap $\to$ `G3` pickup)** | Re-read logs, rebuild scripts, reload all models from disk | Attach to `workspace_id="g2-grail-v43"` — **208 bindings & 50 functions already live** | Zero state-reconstruction tax across agents |
| **Codebase symbol / caller navigation (`Luxel`, `BioForge`)** | Multi-turn `grep` + reading thousands of lines into context | `Api.definitions` / `Api.callers` / `Pharos.refresh!` | Exact call graphs without burning token quota |

---

## 2. Six Concrete Moments Where Palette Paid Off Today

### A. Multi-Agent Brain Transplant (`G2` $\to$ `G3` Zero-Cold-Start Handoff)
When `G2` hit his 5-hour quota ceiling at Call 55 of `workspace_id="g2-grail-v43"`, I (`G3`) stepped into the batter's box. Normally, an incoming agent starts with a blank process and spends 15–20 turns reconstructing harnesses, reloading 1.2 GB of transformer weights, and re-running evaluations.

Instead, my very first Palette call (`Call 56`) attached directly to `workspace_id="g2-grail-v43"`. `ModernBERT-base`, `ModernBERT-base-nli`, `heads44`, `UnseenGrailBench`, `sdev_items`, `lazy_pin`, `batch_yield`, and `xray_diff` were **already hot in memory**. Within 60 seconds of waking up, I was diffing individual attention weights on `heads44`.

### B. Killing Bad Hypotheses in 500 Milliseconds (Before Writing Bad Code)
Twice today, plausible-sounding ideas that would have wasted an hour of training (or subtle gate risk) were **falsified in half a second** inside Palette:
1. **Call 67 — NLI Span-Extension Hypothesis (Falsified in `0.5s`):** To fix `"pine"` vs `"pine hill"`, we hypothesized that `ModernBERT-base-nli` entailment margin would rise when extending truncated spans (`"tarik lives in pine."` $\to$ `"tarik lives in pine hill."`). Probing `nli44()` in Palette immediately showed the exact opposite: `"tarik lives in pine."` scored **`+7.47`** while `"tarik lives in pine hill."` scored **`+6.33`**, and extending into a conjunction (`"tarik lives in lyon while."`) still scored **`+5.30`**. Hypothesis killed on the spot; we fixed `z_I` directly instead, dropping `I`-tag truncation errors to **0 / 2,048**.
2. **Call 85 — Passive Irregular `:rel` NLI Template (Falsified in `0.4s`):** Before V4.6, we considered adding `"$(o) was $(verb) by $(s)."` as a candidate NLI hypothesis for `:rel`. Testing 5 pairs on `nli44()` showed that on active `"Fay sees Hal."`, ungrammatical `"hal was sees by fay."` scored **`+7.39`** (beating grammatical `"fay sees hal."` at `+6.10`!), which would have poisoned directional swap checks. Caught in 400ms before touching a file.

### C. Proving Decoder Fixes on *Broken* Checkpoints Before Retraining
After V4.5 finished (`probe_v45.log`), `B_who_past` had `0.676` coverage and pure-neural `M1` sat at `GE = 0.9258` on blind `val_unseen_B`. Rather than guessing at V4.6 training tweaks:
- **Calls 78–84:** Loaded `ckpts_v45` into Palette and inspected `B_who_past` (`"Camille, who worked at Beryl Systems, lived in Port Mercer"`).
- **Finding:** `coord_gap = 2.5` at token distance `<= 3` was merging Clause 1's `"beryl systems"` and Clause 2's `"port mercer"` across `, lived in` as a fake 2-object collision!
- **Zero-Retrain Proof:** Adding `z_ev[m] <= -0.5f0` (blocking secondary coordinated spans across an intervening predicate trigger) on the **already-trained V4.5 weights** jumped blind `val_unseen_B` `M1 (vm=3)` from `0.9258` $\to$ **`GE = 0.9570, cov = 0.938, risk = 0.0031 (1 wrong / 512)`** in 2 seconds—proving the exact invariant *before* V4.6 ever started.

### D. Catching Hidden Zygote Performance Traps Before Launching 1-Hour Runs
1. **Call 74 (`maximum(zw; dims=2)` Pullback Trap):** Adding `z_max = vec(maximum(zw; dims=2))` to the sentence head looked completely innocent—1 line of Julia. Running a 10-item `Zygote.withgradient` preflight in Palette immediately tripped the 60s timeout because `maximum(..., dims=2)` hit Zygote's slow generic Cartesian `mapreduce` pullback! Replacing it with `vcat(z_cls, z_mean)` (`w_sent: 3 × 2D`) brought warm 10-item backprop down to **`176.9 ms` (`17.7 ms/item`)**. Without Palette preflight, `run_probe_v45.jl` would have silently hung in background bash for hours.
2. **Calls 87–88 (World-Age vs. Compiled Step Speed):** In Call 87, running `include("GrailV46.jl")` and `Zygote.withgradient` in the same top-level call reported `155 ms/item` (`8.7x` slower than expected). Rather than shrugging, Call 88 isolated the world-age boundary in a compiled top-level helper `_bench_v46_grad()` and confirmed true warm speed was **`23.8 ms/item`**.

### E. Cross-Version Weight Comparison (`heads45` vs `heads46`, Calls 90–93)
When V4.6 finished, `val_unseen_B` had **0 errors across 22 of 23 templates** on pure-neural `M1`, with `10/10` errors concentrated in `B_cleft_where` (`"Cove Kelso is where Landon lives."`). Because Palette still had **both** `heads45` and `heads46` resident in the same workspace:
- **Call 92** ran minimal pairs (`"Lyon is where Bruno lives."` vs `"Oslo is where Maya lives."`) through `heads45[1]` and `heads46[1]` side-by-side in `4 seconds`.
- Seeing `"Oslo ... Maya"` succeed (`sB=+1.39, oB=+2.89`) while `"Lyon ... Bruno"` inverted (`sB=+2.31, oB=+2.71`) exposed the exact root cause in one turn: `GrailV46.jl` had flattened `GRAIL_REAL_NEUTRAL_WORDS`'s four 10-word semantic buckets (`10` people, `10` places, `10` orgs, `10` roles) into one untyped bag, generating `"Tokyo works at Doctor"` and training `win` to erase ModernBERT's `Person`/`Place`/`Org` geometry!

### F. Self-Evolving Project Tooling (`Api.promote` $\to$ `.palette/tools/`)
Instead of rewriting boilerplate every session, G2 promoted three general-purpose tools straight into `.palette/tools/` where they auto-loaded across kernel restarts and agent handoffs:
- `.palette/tools/lazy_pin.jl`: Pins `600 MB` read-only models (`ModernBERT-base`, `ModernBERT-base-nli`) inside tool module metadata so access is `~1 µs`, snapshot size is **`0 bytes`** (never tripping the `16 MiB` binding cap), and kernel revival reconstructs them lazily on first call.
- `.palette/tools/batch_yield.jl`: Chunked evaluation with `yield()` + `GC.safepoint()` + soft `budget_s` cutoff so long NLI sweeps return partial progress cleanly instead of hitting hard worker timeouts.
- `.palette/tools/xray_diff.jl`: Standardized minimal-pair counterfactual diffing in milliseconds.

---

## 3. Crash Resilience in the Wild: Automatic Kernel Revival

Twice during this 95-call marathon (`Call 75` after the `maximum(zw; dims=2)` Zygote timeout, and `Call 89` after an idle stop during V4.6's 62-minute background run), the worker process stopped.

In a normal Jupyter/REPL setup, a dead kernel wipes hours of in-memory state. In Palette (`Call 89`):
- **Restored exact values (`208` bindings):** Every small/medium array, trace matrix (`TR_A`, `TR_B`, `tA45`, `tB45`), head weight tuple (`heads44`, `heads45`), and gold dataset came back automatically in `1.79s`.
- **Rebuilt from source (`50` modules & functions):** `GrailV42`..`GrailV46`, `UnseenGrailBench`, `enc44`, `nli44`, `lazy_pin`, `batch_yield`, and `xray_diff` were replayed cleanly.
- Large `>16 MiB` feature arrays (`96 MiB` `Vector{ArmBFeat}`) were politely skipped with an explicit provenance message showing the exact 1-line expression that created them.

---

## 4. Prioritized Friction Queue (Shipped in `v1.0.1`)

> [!NOTE]
> All five friction items identified below during the 95-call `v1.0.0` dogfooding campaign were implemented, verified (`1,125 / 1,125` tests passing), and shipped in **Palette `v1.0.1`**!

1. **Module Alias Replay on Revival (`Call 75` & `Call 89`):**
   - *Symptom:* Top-level module aliases like `const BF44 = GrailV46.BF` were marked `lost: BF44 (module): its using/import was not found` on revival, which in turn caused helper functions referencing `BF44.ArmBFeat` (`enc44()`, `forward_v45_clean`) to fail reconstruction until `const BF44 = GrailV46.BF` was manually re-run.
   - *Fix for G1:* Record `const Alias = ModA.ModB` bindings in the source-replay graph right alongside `using`/`import`/`include` so dependent functions rebuild with zero manual fixups.
2. **Interruptibility Inside Tight C/BLAS or Zygote Compile Calls (`Call 60` & `Call 74`):**
   - *Symptom:* When a single unchunked loop over 2,048 transformer forward passes or a pathological Zygote pullback runs without `yield()`/`GC.safepoint()`, hitting `timeout_s` has to stop and revive the worker (~30s startup) rather than throwing an in-process `InterruptException`.
   - *Fix for G1:* Built-in `Palette.map_budgeted(f, xs; budget_s=...)` (promoting the `.palette/tools/batch_yield.jl` pattern into `Palette`/`Api` core) + preflight hint when `Zygote.withgradient` cold-compiles past budget.
3. **World-Age Hint When `include(...)` + Heavy Dispatch Run in One Call (`Call 87`):**
   - *Symptom:* Defining/including a module and benchmarking `Zygote.withgradient` on it in the *same* top-level call ran `6.5x` slower (`155 ms/item` vs `23.8 ms/item` in `Call 88`) due to world-age / first-call dynamic dispatch effects, which briefly looked like an algorithmic regression.
   - *Fix for G1:* Surface a 1-line note in `costs` when a call mixes `include`/`eval` module definitions with long `execution_seconds` (`>1s`), reminding the agent to benchmark in a subsequent call (or auto-splitting top-level statements across world-age steps!).
4. **First-Class `Api.pin` (Built-In `lazy_pin`):**
   - *Symptom:* Every ML/compiler session has `100 MB – 2 GB` read-only artifacts (pretrained encoders, tokenizers, mmap caches, test corpora) that shouldn't be serialized into snapshots (`16 MiB` cap) *and* shouldn't require manual reload after revival.
   - *Fix for G1:* Ship `Api.pin(:name) do ... end` natively in Palette so every workspace gets `0-byte` snapshot overhead + automatic lazy reconstruction on revival out of the box.
5. **Pharos Cross-Language / Subtree Scope Ergonomics (`Luxel` Sub-Agents):**
   - *Symptom:* `Api.codegraph()` shines on Julia packages, while mixed Julia + WGSL/Rust workspaces (`Luxel`, `Palette` itself) occasionally tempt sub-agents to reach for `grep` on shader bindings or struct layouts.
   - *Fix for G1:* Fast `Api.grep_defs` or lightweight WGSL/Rust symbol extraction in Pharos so multi-language repos stay 100% inside one query interface.

---

## 5. Bottom Line

Most agent harnesses treat code execution like a **batch punchcard machine**: submit script, wait, read stdout, guess. Palette turns the runtime into a **persistent laboratory bench** shared across agents (`G2`, `G3`, sub-agents) with sub-50ms feedback, zero-byte pinned backbones, automatic crash revival, and a cumulative `.palette/tools` memory that gets sharper every campaign. That feedback-loop compression is why Grail went from `0.49` held-out GE (`risk = 0.60`) to `0.93–0.96` unseen GE (`risk = 0.000–0.007`) and `19/19` zero-shot pronoun/quantifier safety in a single afternoon.
