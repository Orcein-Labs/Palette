# Think in Julia. Work in Palette.

An optional operator briefing for the standalone Palette surface. Read this
alongside the actual tool schema and [operator contract](../runtime/docs/operator.md).
Deployment-specific inference engines, other language kernels, agent bridges,
and compiler toolchains are not assumed to exist.

## Build a representation you can keep

Before choosing a command, choose the representation that makes the task easy.
A dependency problem wants a graph; repeated comparisons want indexed records;
a numerical claim wants a calculation with explicit assumptions. Parse once,
keep the parsed value, and build small functions that answer your next questions.
Use exact computation for counts, equality, arithmetic, parsing, and comparisons.
Human intent and uncertain premises still require judgment.

Palette retains a Julia world across calls. A call can add an instrument to that
world, rather than produce another page of prose. Keep useful data and methods;
return the smallest evidence that changes the decision. Summarize a large result
at the boundary and leave the full result in a named binding or workspace file.
Do not fill a conversation with thousands of records just to search them later.

```julia
struct Measurement{T<:Real}
    name::String
    baseline::T
    candidate::T
end
relative_change(m::Measurement) = (m.candidate - m.baseline) / m.baseline
measurements = [Measurement("latency", 100.0, 82.0),
                Measurement("allocated bytes", 4000.0, 4400.0)]
[(name=m.name, change=relative_change(m)) for m in measurements]
```

Here the records and function survive the call. New observations can reuse them.
Define what a metric means before comparing it: lower latency and lower quality
are very different outcomes. Handle zero baselines explicitly in real analyses.

## Keep exploration interactive and hot paths compiled

Put performance-critical work in functions and pass session data as arguments.
Use function barriers between loosely typed input and typed computation. Measure
compilation separately from warm execution; inspect allocations and inferred
code before adding optimization annotations. Arrays are column-major: visit
contiguous elements when the algorithm permits. Preallocate repeated outputs;
choose views or copies by measured workload. Fused broadcasting avoids some
intermediates, but deliberate unfusing can avoid repeated expensive computation.
These practices come from Julia's [performance manual](https://docs.julialang.org/en/v1/manual/performance-tips/).

Write generic methods with clear contracts; let dispatch specialize useful
behavior. Parameterize fields that carry varying concrete types. Follow the
standard `!` convention for mutating APIs. Own either the function or a type when
adding methods; avoid extending someone else's function for someone else's
types. Prefer readable functions and standard interfaces over clever machinery.
See Julia's [style guide](https://docs.julialang.org/en/v1/manual/style-guide/).

```julia
function energy!(dest::AbstractVector, src::AbstractVector)
    axes(dest) == axes(src) || throw(DimensionMismatch("different axes"))
    for i in eachindex(dest, src)
        dest[i] = abs2(src[i])
    end
    dest
end
samples = Float64[1, -2, 3]
energies = similar(samples)
energy!(energies, samples)
@assert energies == [1.0, 4.0, 9.0]
```

Retaining `energies` lets later calls reuse storage. A mutation also changes
what subsequent experiments observe: make copies where a baseline must remain
fixed, and document aliasing when several bindings share an object.

## Operate on evidence, not imagined continuity

Start an unfamiliar desk with `kernelinfo()` and `varinfo()`. Confirm the active
workspace and `Palette.worldinfo()` before touching project state. Through MCP,
create a routed world and retain its `workspace_id` and `context_id`; include
both on later calls. Caller-supplied context keys prevent accidental collisions,
not impersonation. Separate worlds can still share project files. Omitting
routing identifiers selects the explicitly shared legacy world.

Bindings, supported definitions, and data may be reconstructed after restart.
Tasks, process handles, channels, and foreign runtime state may be lost. Read
`Palette.revival()` before using restored results. A snapshot is not a promise
that every completed call survived. A useful old result can remain stale even
when its bytes were faithfully restored.

Use `Palette.provenance(:binding; strong=true)` when a source observation needs
a content check. This reports observed dependencies, not complete causal truth:
dynamic reads, qualified includes, and in-place mutation can escape tracking.
“No observed staleness” cannot establish that every external input is fresh.

For results produced by another worker, keep publication identity beside the
artifact and read it with the tracked API:

```julia
artifact = Palette.read_artifact("results.txt";
                                 authority_path="results.commit.json")
results_text = artifact.content
Palette.provenance(:results_text; strong=true)
```

The authority JSON contains `generation`, `commit`, and the content's SHA-256
`digest`. Publish it only for completed content. This lets Palette distinguish
retained observations from a superseding publication. An ordinary `read` does
not make that claim.

## Use each transport for what it carries

Julia code carries operations. Payloads carry text. Files carry durable artifacts.
Use the MCP payload for source text, patches, and complicated shell scripts;
write it with `write("experiment.jl", PAYLOAD)` rather than nesting quotations.
`bash("… $x")` interpolates Julia values; in `sh"…"`, `$` belongs to the shell.
Standard Julia `Cmd` values execute a program without shell pipelines or globs.
Check exit status and result content before accepting a subprocess result.

A whole-call parse failure executes nothing. A runtime failure can leave effects
from earlier statements. Inspect the world before deciding what can safely run
again. Never automatically repeat an uncertain mutation. Retained output is
available through `Palette.output(call)` while that kernel's ring survives;
`Palette.costs(call)` separates recorded phases and is queryable for completed
response encoding on a subsequent call. Neither replaces a durable experiment
receipt or a host delivery measurement.

Write checkpoints before risky experiments. Durable files should identify the
source revision, environment, inputs, metric definition, and outcome. Preserve
failed attempts as well as successes. A table of observations is more useful
than a remembered claim that the experiment “worked.”

## Turn the world into an experiment conductor

Keep a small reference implementation and compare a candidate against it before
timing. Preserve baseline inputs, include adversarial cases, and inspect the
actual outputs. Change one hypothesis at a time. Retain a typed result table,
raw traces, and enough metadata to reproduce each row. Check both performance
and correctness on the workload that motivated the change.

Inspect generated code when it answers a specific question. Explore dispatch or
specialization with a small reproducer before rewriting a subsystem. Keep
compiler experiments in separate source worktrees and build directories; the
Julia process hosting Palette should remain the stable conductor. Run candidate
runtimes as subjects, then compare against the baseline and real operator tests.
This arrangement requires separately provisioned build tools and authorization.

Reloading code does not reset every old method, object, or task. Check
`Palette.reloads()` and its limitations. Use a fresh world when you need a clean
experiment rather than assuming a source edit replaced every live identity.
Persist code in source files and reproducible tests once exploration finds a
useful result; a notebook-shaped world alone is not a maintainable release.

## Build reusable probes, not repeated transcripts

A few patterns compound particularly well in a persistent world:

- **Index the investigation.** Parse logs or source observations into records,
  index them by identifier, and retain a dependency graph. A change can then
  select the affected observations instead of forcing another complete scan.
  Update the index when its inputs change; retaining it does not make it fresh.
- **Keep an executable oracle.** Bind the trusted reference, candidate, fixtures,
  and comparator. Run the comparator before every timing batch. For algebraic
  code, enumerate small cases or check invariants as well as random examples.
- **Make experiments values.** Store configurations and explicit input hashes in
  a typed table, then map one experiment function across the configurations.
  Represent pending, failed, and completed runs separately. A missing result
  must never silently become a zero in an aggregate.
- **Stage the expensive boundary.** Load and parse a corpus once; run many cheap
  hypotheses over that representation. Materialize expensive shared transforms
  once when it helps, while keeping small one-off operations small.
- **Inspect what the compiler chose.** After `using InteractiveUtils`, use
  `@which` to inspect dispatch and `@code_warntype` to investigate an inference
  question. Compare a small concrete reproducer before reaching for generated
  functions or macros. Save observations with the runtime and source identity.
- **Separate controller from subject.** A stable Palette world can retain the
  experiment matrix while separate workers or subprocesses run unstable code.
  Inspect the deployment's actual child-worker capabilities first. Neither a
  Julia task nor a new module provides OS isolation.
- **Make evidence publishable.** Keep raw results in files and small summaries in
  bindings; publish a generation/digest authority record for external consumers.
  Another world can compare committed artifacts without sharing mutable objects.

A callable struct makes an instrument's parameters explicit, while allowing
Julia to specialize its implementation. This standard Julia pattern works well
when the instrument survives several questions:

```julia
struct Probe{F}
    name::String
    f::F
end
(p::Probe)(input) = (name=p.name, value=p.f(input))
positive_count = Probe("positive entries", xs -> count(>(0), xs))
positive_count([-2, 0, 3, 5])
```

The example returns `(name="positive entries", value=2)`. Retain the probe and
replace the input. If a probe captures a mutable object, that object is part of
its experiment state; record or copy it deliberately. Supported closures can
revive, but still inspect the restart report before trusting an instrument.

The useful change in thinking is to ask what you can build once and interrogate
many times. A graph query, executable comparator, profiler script, or publication
validator can save more effort than optimizing the wording of another command.

## Authority is explicit

Workspace writes and host effects depend on the configured execution envelope.
A read-only source tree does not become writable because the model can formulate
a patch. Use `palette_patch` to read, prepare, and review an exact change set;
a trusted client's confirmation or an explicitly authorized local operator must
approve its digest. Approval permits one bounded attempt, not general filesystem
access. Receipts describe committed files and failures; a multi-file patch is
not an all-or-nothing transaction. See [workspaces and patches](../runtime/docs/workspaces.md).

Do not target the deployed adapter's own source directory. Apply experimental
changes to a separate checkout, test them, then switch installations deliberately.
Treat effects on source, deployed binaries, and the current world as distinct.

## Leave a useful world behind

Retain instruments you will reuse and evidence that matters. Put large disposable
intermediates behind a short-lived function boundary instead of permanent global
bindings. Close idle routed worlds when finished; the current router also retires
idle worlds opportunistically on later requests. Closing preserves snapshots,
but live tasks cannot be assumed to survive. This is not a background collector
for arbitrary project files or experiments.

The next operator should be able to discover the active workspace, inspect the
current hypothesis and result table, find the checkpoint files, and identify
which observations are stale. That is the advantage of a persistent surface:
the next question starts with working instruments and inspectable evidence.
