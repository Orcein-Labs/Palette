# Live X-Ray Protocol: Turning Persistent Julia State into a Model Debugger

**Date:** 2026-11-24  
**Context:** Field study from dogfooding Palette during the Orcein Stack's Stage-0/Stage-1 neuro-symbolic parser campaign (`BioForge` / Grail V4.2 $\to$ V4.3) and `CNIR` / `Luxel` verification runs.

---

## 1. Why Cold-Start CLI Loops Fail on Subtle Model Bugs

Standard agent-driven ML debugging runs in a slow, blind loop:

```
edit script -> cold-boot Julia/Python (~45s) -> reload weights -> train 45m -> read aggregate score -> guess
```

When an error rate is `0.26` or `0.04` on an unseen split, aggregate metrics hide *which* mechanism broke. Re-booting the runtime to inspect a single failing sentence costs ~30–60 seconds per question, so agents fall back to guessing at hyperparameters or training distributions instead of inspecting internal activations.

Keeping the encoder, verifier, and trained heads hot in RAM inside a scoped Palette workspace (`palette_workspace`) flips the cost curve: inspecting a single failing sentence against a minimal counterfactual drops from **~45 seconds to ~23 milliseconds**, making neuron-level root-cause attribution practical mid-session.

---

## 2. Case Study: Grail V4.2 $\to$ V4.3 (Live X-Ray in Palette)

During the J2 Grail push (`BioForge`), V4.2 (`K=3` bootstrap ensemble over frozen `ModernBERT-base`) stalled on two blind unseen-syntax sets (`val_unseen_A: 0.760–0.852`, `val_unseen_B: 0.744–0.756, risk=0.26`). Rather than blindly adding more training data, G2 kept `ModernBERT-base`, `ModernBERT-base-nli`, and all three trained heads resident in Palette and probed individual failure cases directly:

1. **Cross-Clause Word Bleed ("Noisy Neighbor")**
   - *Symptom:* On `"Because Claire works at Veridian, Claire lives in Basel."`, the model abstained as `:ambiguity`.
   - *Palette X-Ray:* Querying per-word object logits at the `"lives"` event showed `"Basel"` at `+2.36` and `"Veridian"` (from the first clause) at `+0.23`. Unnormalized sigmoids let any token above `0.0` fire as a second object, triggering the two-fill ambiguity guard.
   - *Fix:* Head-to-head softmax competition across candidate spans plus an adjacency constraint for genuine two-fill pairs (`A and B` / `A or B`).

2. **Multi-Clause Label Poisoning ("Gaslighting Teacher")**
   - *Symptom:* V4.2 abstained on simple single-clause assertions like `"Ingrid lived in Cove Kelso."`
   - *Palette X-Ray:* Contrastive probe between `"Ingrid lived in Cove Kelso."` (ambiguity logit `+3.44`) and `"Ingrid has lived in Cove Kelso."` (fact logit `+3.25`). In V4.2's generator, two-sentence items with one ambiguous clause (`"Ingrid lived in Geneva. Marcus might work at Basalt."`) stamped `:ambiguity` onto *both* clauses, teaching the model that plain past-tense verbs were ambiguous.
   - *Fix:* Supervise each clause with its own true polarity while keeping sentence-level abstention intact.

3. **Argument-Order Blindspot ("Backwards Passive")**
   - *Symptom:* All 49 mistakes out of 512 items on `val_unseen_B` came from reversing `(subj, rel, obj)` on three constructions (`"Julian was employed by Linden Labs."`, `"Cove Kelso is where Landon lives."`, `"South Corin is home to Fabian."`).
   - *Palette X-Ray:* Auditing the training mix showed ~99% of passive items used `by`-agent `:rel` frames with inverted orderings, leaving employment passives and locative inversions uncovered.
   - *Fix:* Added targeted minimal constructions for those grammatical forms (with zero vocabulary overlap with the held-out sets).

4. **Directional Symmetry Check on Frozen Broken Weights ("Sanity-Swap")**
   - *Palette X-Ray:* Before retraining anything, G2 tested a bidirectional NLI check on the *broken V4.2 weights* inside Palette: whenever the parser proposed `(Linden Labs, works_at, Julian)`, the NLI verifier scored both `"Linden Labs works at Julian."` (`-0.95`) and `"Julian works at Linden Labs."` (`+1.67`).
   - *Result:* That single symmetry check cut blind mistakes on V4.2 from **49 down to 3** in seconds—proving the verifier invariant before spending ~50 minutes retraining V4.3.

### Outcome (Observed in `probe_v43.log` & `eval_v43_tense_nli.log`)

- **Raw V4.3 Single Heads (before ensemble voting, thresholds, or NLI):**
  - `val_unseen_A`: `0.941 – 0.951` GE (`cov = 0.98`, `risk = 0.049`)
  - Blind `val_unseen_B`: `0.912 – 0.959` GE (`cov = 0.97`, `risk = 0.033`)
- **With Tense-Aware Bidirectional NLI (`M2, vote_min=2`):**
  - `val_unseen_A`: `0.9160` GE, `cov = 0.878`, **`risk = 0.0033`** (`1` wrong)
  - Blind `val_unseen_B`: `0.8984` GE, `cov = 0.847`, **`risk = 0.0000`** (`0` wrong out of 512)

---

## 3. Reusable Palette X-Ray Protocol (Checklist for Future Campaigns)

When debugging models or compilers in Palette, follow this 4-step loop:

1. **Keep State Hot & Bounded (`@ephemeral` / `Api.bind_mmap` / `reinclude_module!`):**
   Keep frozen backbones and large read-only weights in RAM (`@ephemeral` or `Api.bind_mmap`) so snapshots stay small, and use `Api.reinclude_module!` / `Api.reload_methods!` to iterate on decoders without restarting the kernel or polluting `Main`.
2. **Contrastive Minimal-Pair Probing:**
   Never debug aggregate `risk` in the abstract. Pull 3 failing examples per error mode and diff internal logits against a 1-token counterfactual (`lived` vs `has lived`, `A r B` vs `B r A`).
3. **Prove Decode/Verifier Fixes on Frozen Broken Weights First:**
   Test every decoder or verifier change against the *current broken checkpoint* in Palette before launching a new multi-minute training run. If it doesn't move the needle on existing weights, diagnose further before retraining.
4. **AST + Gradient + Allocation Preflight:**
   Run `Meta.parseall`, finite-difference `rrule` checks, `Api.warntype_summary`, and `Api.@bench` / `@alloc_barrier` in Palette before launching background sweeps.
