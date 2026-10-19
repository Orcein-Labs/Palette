# Phases B–E: deep-dive answers before building

The finish plan asks each Phase B–E capability to answer the Exotic Capability
Ledger's §13 questions before it is built. It also asks for the ledger's
evidence labels after it is built. This doc gives the answers and the design
each one commits to. The labels below are the state before building. Each
phase's section is updated with its measured status when the phase lands.

Every capability builds on what the kernel already records, so the
"Julia leverage" and "Stack leverage" answers point at real code:

| Part | What it records |
|---|---|
| Revival snapshots (`src/revival.jl`) | Bindings classed as definition, data, import or runtime. Definitions are replayed from source, data is serialized with sha256, and type fingerprints and method sets are checked after a rebuild. |
| Turn costs (`execute_turn`, `src/turn.jl`) | Compile time and run time for each call. |
| Broker receipts (`runtime/host`) | Every host request. |

## B. Time Machine (MT-01, P-04)

1. **User power.** Name a point in a live session, keep working, break things,
   and go back. Or fork from that point into a new world with its ancestry
   recorded. The restored world is checked against the identities the
   checkpoint recorded, and the report names any that differ.
2. **Julia leverage.**
   - Method sets and type fingerprints give code an identity that can be
     checked after a rebuild.
   - `Serialization` restores data into a module of the same name.
   - Top-level definitions replay from source, so a checkpoint stores source
     text and serialized data instead of a process image.
3. **Stack leverage.**
   - Palette already writes a complete snapshot after each call and revives
     it with a per-binding verdict.
   - A checkpoint is a named, immutable copy of that snapshot, plus
     content-addressed copies of the files it included.
   - The host owns kernel restarts, and `palette_control` drives them.
4. **Minimum demo.** The ledger's seven steps, on a Luxel analysis. The
   analysis loads the converge4 frame timings, defines a summary operator
   and computes a result. Then:
   1. Edit the included operator file so it computes something else.
   2. Change a binding and an environment variable.
   3. Restore the checkpoint.
   4. Rerun the analysis and get the same digest.
   5. Show the restore receipt: environment identity, each definition's
      method-set digest, each include's sha256 and where it was replayed from,
      and each data group's sha256.
5. **Economic metric.**
   - **Time to get back to a known-good world:** the kernel restart cost
     versus rerunning the analysis. Measured on the converge4 analysis
     (2026-10-18): a restore takes 8.9 to 11.5 s end to end (host start
     6.9 to 8.7 s, the prepared depot reused), an undo 11.4 s and a fork
     10.2 s. The first call after a restore takes 3.9 s. Rerunning that
     analysis takes under 0.01 s once compiled, so for it a restore is the
     slower way back. A restore pays off when the world takes more than
     about 10 s to rebuild, or cannot be rebuilt at all.
   - **Storage per checkpoint:** data files are hard-linked, since snapshot
     files are never modified after they are written. Include sources are
     stored once per content hash. Measured: 32 KB per checkpoint and 4 KB
     per distinct source for the converge4 analysis. Saving one takes no
     time the call can see (the checkpoint call took 0.05 s).
6. **Correctness / safety boundary.**
   - A restore never claims an identity it did not check.
   - Workspace files are never rewritten; git owns files. A restored world
     runs its code from the checkpoint's copies, and the report names each
     workspace file that differs from the copy the world runs.
   - The world being left is saved as an automatic checkpoint first, so a
     restore can always be undone.
   - Data from another Julia or package environment is not restored. This
     is the existing revival rule.
7. **Failure modes.**
   - These cannot be rewound: external resources, background processes,
     GPU and device state, and `ENV` changes made outside the kernel. The
     report names them, using the revival text that already exists.
   - Nested includes are kept as they were at checkpoint time. If one
     changed between the `include` and the checkpoint, nothing can detect it.
   - Workspace packages (`using` of a package in the workspace) load from
     current sources. The report says when those sources differ.
   - Blob storage grows with distinct include contents.
     `Palette.forget_checkpoint` removes a checkpoint and any blob no longer
     referenced.
8. **Competitive substitutability.**
   - Jupyter has no world restore.
   - CRIU and VM snapshots restore a process image, but they can't say
     which definitions or data are the same, and they are huge.
   - Git restores files, not the live world.
   - Reproducing this needs the per-binding classification and identity
     checks Palette already has.
9. **Product placement.** Palette, in the kernel and the host control. The
   Gesso roadmap explicitly keeps world snapshots out of Gesso.
10. **Evidence status.** MEASURED on one workload, 2026-10-18.
    - The seven steps ran through Palette on the converge4 frame-time
      analysis: digest `3e05fbdad263066c` before the break and after the
      restore. Including the changed file again, undoing through the
      automatic checkpoint (back to digest `7ee6ead301b57ab7`) and forking
      also ran.
    - `test_time_machine.py` covers the same steps on a small stand-in
      analysis, plus the adversarial cases: a damaged checkpoint, a damaged
      source copy, and code changed before the checkpoint.
    - The demo found three defects, all fixed: false "stale" reports after
      a restore, a `const` from an included file revived as a plain global,
      and nested names for undo checkpoints. See the dogfood friction log.
11. **Promotion rule.**
    - The seven-step demo passes on a real Luxel analysis, as an automated
      test plus a dogfood run.
    - The restore cost is measured.
    - The adversarial tests show that a restore never reports a changed
      identity as the same.

**Design.**

- **`Palette.checkpoint(name; note)`**
  - Writes a checkpoint of the world as it is at the end of the current
    call. The end-of-call snapshot is then forced: it does not give way to a
    waiting request.
  - The snapshot is hard-linked into `STATE_DIR/checkpoints/<name>/`.
  - Include sources go to `STATE_DIR/blobs/<sha256>` (nested includes too),
    kept only while the file still has the hash the include recorded.
  - `checkpoint.json` is written last. It records name, note, world, call,
    time, environment identity, each definition's check digest, each
    include's sha256 and whether it was kept, and each data group's sha256.
- **`Palette.checkpoints()`** lists the checkpoints and the world's lineage
  and history.
- **Restore** is `palette_control {"action": "restore", "checkpoint": name,
  "fork": bool}`.
  - The host writes `STATE_DIR/restore.json` and restarts the kernel.
  - Before revival, the starting kernel:
    1. verifies the checkpoint files' hashes;
    2. saves the current snapshot as `auto-<time>-before-<name>`, or
       `auto-<time>-before-undo` when it restores an automatic checkpoint;
    3. swaps the checkpoint's snapshot in, with the manifest as the commit
       point;
    4. updates `world.json`. A fork gets a new world id whose parent is the
       checkpoint.
  - Revival replays a restored include from its blob, whatever the workspace
    file holds now. The receipt says whether the workspace file is the same,
    differs or is gone. Those definition entries are pinned to the blob's
    hash, so later revivals keep running the checkpoint's code until the file
    is included again.
  - The revival observation carries a `checkpoint` record with each identity
    and its verdict. That record is also appended to `world.json`'s history,
    which is the durable receipt.
- **`palette_control {"action": "checkpoints"}`** lists checkpoints without a
  kernel, for when the world is broken.

## C. Execution X-Ray (MT-02, MP-10)

1. **User power.** For any call, see:
   - where the time went: compile, run and GC time;
   - how much it allocated;
   - which methods its top-level calls dispatched to;
   - which host receipts it caused.

   Receipts can be queried by call, kind or time.
2. **Julia leverage.**
   - `Base.cumulative_compile_time_ns` and `Base.gc_num` give compile time
     and allocation deltas.
   - `which` and `Base.method_instances` give the dispatched method and
     specialization.
3. **Stack leverage.**
   - Turn costs and broker receipts exist already.
   - X-Ray joins them per call: the host stamps each receipt with the
     request id of the call that caused it.
4. **Minimum demo.** On a Luxel bench-summary call, `Palette.xray(n)` shows:
   - compile vs run vs GC seconds;
   - bytes allocated;
   - the dispatched method of each top-level call;
   - the receipt ids of its host commands.

   `palette_control {"action": "receipts", "call": n}` returns those receipts
   in full.
5. **Economic metric.** Time to answer "why was that call slow", and the
   tokens spent answering it. Without X-Ray, the model wraps the call in
   `@time` and reruns it.
6. **Correctness / safety boundary.**
   - Instrumentation never changes results.
   - The default level costs under 1 % of call time. Dispatch tracing turns
     on only in the traced context (D).
   - Receipt queries are read-only.
7. **Failure modes.**
   - Compile time is process-wide, so background tasks bleed into it. The
     report says so.
   - Dispatch resolution of a top-level call needs its argument types.
     Getting them from re-evaluation would have side effects, so they are
     taken only from the traced rewrite.
8. **Competitive substitutability.** `@time` and Profile give pieces, one
   call at a time, by hand. None of them links a call to its host effects.
9. **Product placement.** Palette. The Gesso drill-down is a later
   consumer.
10. **Evidence status.** MEASURED on one workload, 2026-10-19.
    - Demo: a traced call ran `gpu-smi`, `git rev-parse` and the converge4
      presented-frame bench at ultra (still camera, 396 s under the GPU
      lock), and a second traced call summarised its output (close 42.3 ms,
      medium 55.1 ms, wide 58.1 ms mean frame time). `Palette.xray(n)`
      showed compile, run and GC seconds, bytes allocated, the method each
      top-level call dispatched to, and the request id of each host command.
    - Join coverage: 5 of 5 host requests across both calls joined a receipt
      by request id; the receipt query read 0 unreadable lines.
    - Overhead: 0.74 % of a warm `1 + 1` call. The per-call statements
      Phases C and D added cost a median of 19.2 µs in the kernel (14
      batches of 2000), and one `calls.jsonl` append costs 3.4 µs, against a
      3.06 ms end-to-end warm call (median of 200). There is no X-Ray off
      switch to A/B against, so the numerator is the added code timed in
      place; it includes the turn-cost record that predates C, so it is an
      upper bound.
    - Compile time, GC time and allocation are process-wide; the record's
      `limits` field says so.
    - `test_xray.py` covers the join of every request in a call, results
      that tracing leaves unchanged, and the context on the call record;
      `test_receipt_query.py` covers the query, including that it leaves
      the files unchanged.
11. **Promotion rule.** The demo passes, overhead is measured under 1 % on
    warm calls, and receipt join coverage is 100 % of the host requests in a
    test session.

## D. Execution Contexts (MT-03)

1. **User power.** Run the same call under different execution laws without
   changing its code:
   - `traced`;
   - `reference` (registered reference implementations replace fast ones);
   - `memoized` (top-level calls with the same function and argument hashes
     reuse results);
   - `budgeted` (an allocation and time budget, enforced).

   Every receipt records the active context.
2. **Julia leverage.**
   - The call's top-level AST is rewritten before evaluation. Code is data,
     so this is a few lines of macro-free transformation.
   - Dispatch-based alternate implementations are registered as methods,
     not flags.
3. **Stack leverage.** The kernel already parses each call's top level for
   revival and provenance. Contexts reuse that parse, and X-Ray records the
   result.
4. **Minimum demo.** The ledger's demo: the same operator under the normal,
   reference, traced and alternate-implementation contexts. The semantic
   outputs are identical where they should be, and the four receipts differ.
5. **Economic metric.** Code that is not duplicated for A/B and debugging
   runs, and calls saved by memoization.
6. **Correctness / safety boundary.**
   - The context is always recorded in the call's costs and X-Ray record.
   - A context never persists past its call unless set as the session
     default, and that is reported in every reply while set.
   - Memoization never serves a result across a definition change: the key
     includes the method's identity.
7. **Failure modes.**
   - Contexts can conflict. Only one is active per call, and that is
     documented.
   - The rewrite reaches top-level calls only, not calls made inside
     functions. Reference implementations reach deeper because they replace
     methods for the call's duration.
8. **Competitive substitutability.** Cassette/IRTools overdubbing gives
   deeper reach, but it is fragile across Julia versions. Flag-based A/B
   duplicates code.
9. **Product placement.** Palette.
10. **Evidence status.** LANDED, 2026-10-19.
    - `test_contexts.py` runs the four-context demo: `g([1, 2, 3])` gives 12
      in normal, reference, traced and memoized, and the four X-Ray records
      differ (no extra field; the replaced method; the dispatched method;
      a memo miss, then a hit).
    - The same tests cover one context per call, the session default being
      reported in every reply and lost on restart, a redefinition between
      memoized calls missing, budgets interrupting by time and by bytes, and
      bad requests failing without harming the world.
    - The context is on every call's costs, X-Ray record and
      `calls.jsonl` line. The Phase C demo ran in the traced context.
11. **Promotion rule.** The four-context demo passes as a test, and receipts
    show the context every time.

## E. Capability lifecycle and demand-loaded tools (P-02, P-03), and the code graph

1. **User power.**
   - A helper the model wrote can be promoted with evidence at each step:
     1. *ephemeral*: a session binding;
     2. *project tool*: source saved in the project with its passing calls;
     3. *trusted*: approved by a human, through the patch broker's approval
        path.
   - A hot index lists every tool (name, signature, doc and stage) without
     loading any implementation. A tool loads on first use.
   - A native code graph of the workspace is a live value, refreshed per file
     by content hash and cited in receipts:
     - Julia: definitions, calls and includes;
     - Rust and Python: items and imports.
2. **Julia leverage.**
   - Tools are plain Julia files loaded into their own module on demand.
   - The code graph is built from `Meta.parseall` for Julia, so it matches
     what revival and provenance parse.
3. **Stack leverage.**
   - The patch broker already has approval modes.
   - Provenance already hashes files.
   - Receipts carry the graph's file digests.
4. **Minimum demo.**
   - **Lifecycle:** a helper used in 3 successful calls is promoted to a
     project tool. A fresh session lists it from the index, it loads on its
     first call, and the receipt cites its evidence.
   - **Code graph:** after one file changes, refreshing it re-parses only
     that file.
5. **Economic metric.**
   - **Tool context:** tool-description tokens per session, from the index
     versus full docs.
   - **Code graph:** refresh time per changed file.
6. **Correctness / safety boundary.**
   - Nothing reaches *trusted* without human approval.
   - A tool whose source hash differs from its evidence drops back to the
     project stage.
7. **Failure modes.**
   - The index goes stale when a file is edited outside Palette. Every index
     read checks file hashes.
   - Rust and Python graph extraction without a real parser is
     approximate, and is labelled that way.
8. **Competitive substitutability.**
   - MCP servers list every tool eagerly.
   - Graphify-style code graphs are offline snapshots, not live values tied
     to receipts.
9. **Product placement.** Palette.
10. **Evidence status.** ENGINEERING CANDIDATE (2026-10-19). Measured on
    Julia 1.12.6:
    - **Lifecycle:** the demo passes as `runtime/security/test_tools.py` on
      the Rust and the Python host (7 tests each).
    - **Tool context:** for 6 tools with 3 evidence entries each,
      `Palette.tools()` is 2438 bytes (about 609 tokens at 4 bytes per
      token). The full source and sidecars are 8332 bytes (about 2083
      tokens); the source alone, with docstrings, is 4825 bytes (about 1206
      tokens). An index read takes 0.24 ms. The 6 tools are a stand-in
      sample: the documented top-level functions of `src/pharos.jl`.
    - **Code graph on the Luxel repo** (459 files from `git ls-files`): the
      first build takes 6.6 s, a refresh with no change 0.089 s, and a
      refresh after one changed file 0.10 to 0.13 s (a 96 KB Julia file,
      a Python file and a Rust file). Listing files through git, rather than
      walking the tree, took the first build down from 55 s.
11. **Promotion rule.** The lifecycle demo passes as a test, the index
    token saving is measured, and the code-graph refresh is measured on the
    Luxel repo.
