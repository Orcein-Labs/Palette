# Palette finish plan

Date: 2026-10-18. Base: `main` at 6fe27dcd0.

## Why now

Palette is mature enough to stop hardening and start building the
capabilities it exists for. Luxel feature work is parked for this round.
Real Luxel, Gesso and Cyan work is the test workload from day one: each time
an agent has to fall back to Bash, that is the next seam to fix.

Capability source: the Gesso stack's
[Exotic Capability Master Ledger](https://github.com/latentcollapse/Gesso/blob/master/docs/research/EXOTIC_CAPABILITY_MASTER_LEDGER.md).
IDs below (P-xx, MT-xx, MP-xx) refer to it.

## Exit gates for the round

| Gate | Passes when |
|---|---|
| Shell parity | An agent runs a full Luxel session through Palette with no Bash: cargo build and test, git, GPU renders under the shared GPU lock, profiling. |
| Long work | Work over 60 s runs as a job or with a per-call deadline, with status, streamed output and cancel. |
| CI | `security-suite` finishes in under 15 min and is green on 10 consecutive runs. Today it takes 50–90 min and sometimes hangs in sandbox prewarm. |
| Speed | Warm calls take about 1 s or less. Cold start is well under today's 11 s. |
| Honest reporting | The dogfood report's seams 2–8 are fixed. |
| Breadth | The same gates pass on Gesso and Cyan work, not only Luxel. |

## Phases

### A. Dogfood blockers

1. **Long work.** Pass the host's `--turn-timeout` through the server, and add a per-call deadline. Add background jobs with status, streamed output and cancel.
2. **Host commands and the GPU path.** Make allowed host commands discoverable. Fix the ceiling that protects the user's home directory and so blocks the package store inside it. Make GPU work run under the shared lock with a timeout.
3. **Sandbox prewarm in CI.**
   - Reuse the precompile caches instead of recompiling about 80 packages on every run.
   - Capture the real error when a package fails to precompile (ZMQ today).
   - Stop the prewarm output from being truncated.
4. **Messages and bookkeeping** (dogfood seams 2–8):
   - Report a preparation failure as a preparation failure, not as a connection problem.
   - Clear a stale preparation state once a later prepare succeeds.
   - Make the kill message match what revival restores, and report the killed call.
   - Name the execution phase when a call times out.
   - Make patch targets discoverable, and print the approval command.
   - Give private instances their own package store by default.
   - Make the prewarm instantiate before it precompiles.
   - Make `list` say when other thread workspaces exist.
5. **Cold vs warm cost (MP-12).** Status and receipts report first-call cost separately from warm-call cost.

Exit: the Luxel L-10 workload (quality presets) runs entirely through Palette.

### B. Time Machine, bounded (MT-01, P-04)

Built on revival, snapshots and receipts, all of which exist today.

- Named checkpoints of a world.
- Restore a checkpoint, or fork a new world from it.
- A receipt naming the code, artifact and operator identities that were restored.

Acceptance: the ledger's seven-step demo, run on a real Luxel analysis.

### C. Execution X-Ray, Palette level (MT-02, MP-10)

- Per call: compile time vs run time, allocations, which method was dispatched, and links to the call's receipts.
- A way to query receipts.

The drill-down into Gesso, down to GPU kernels, waits for Gesso's speed floor.

### D. Execution Contexts (MT-03)

Run the same call in traced, reference, memoized or resource-budgeted mode
without rewriting it. The active context is always recorded in the receipt, so
no execution policy is hidden.

### E. Capability lifecycle and demand-loaded tools (P-02, P-03)

- A helper an agent writes can be promoted from ephemeral to project tool to trusted (canonical) tool, with evidence at each step.
- An index of tools is kept hot, and implementations load only when they are used.

## Out of scope for this round

The following tie to Gesso's speed floor or need real compiler evidence first:

- Schedule Forge (MT-04);
- Live Foundry (MT-05);
- Compiler Laboratory (MT-06).

## Rules

- Before building, each capability in B–E answers the ledger's §13 deep-dive questions: user power, minimum demo, what must never silently change, the economic metric and the promotion rule.
- After building, its evidence status is marked using the ledger's labels.
- Think as far ahead as the architecture permits; implement only as far as the receipts justify.
- Every phase is verified by an adversarial review before it merges.
- Friction found while dogfooding is logged next to this plan, then fixed or deliberately deferred.
