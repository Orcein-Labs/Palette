# Operator API

Each call evaluates Julia in a persistent execution module. Bind values to keep
them across calls; `ans` holds the last successful result. Whole-call parse errors
produce no effects. Runtime errors can leave effects from earlier statements.

Inside the session, `Main` is the execution world. `Palette.world()` returns that
module and `Palette.worldinfo()` reports its identity and epoch. The session
helper binding exposes the restricted operator API, not the complete package.

| Helper | Purpose |
| --- | --- |
| `varinfo()` | Named bindings, size, and summaries |
| `kernelinfo()` | Runtime, workspace, packages, and execution contract |
| `bash("script")`, `sh"script"` | Shell execution and captured results |
| `Palette.output(call)` | Retained output from an earlier call |
| `Palette.costs(call)` | Recorded execution and transport phase observations |
| `Palette.revival()` | Detailed report of the most recent restart |
| `Palette.reloads()` | Observed workspace package reloads and limitations |
| `Palette.provenance(:name; strong=false)` | Binding, definition, source, and artifact observations |
| `Palette.read_artifact(path; authority_path=nothing)` | Tracked UTF-8 artifact read |
| `Palette.request_capability(category, params)` | Request an effect from the host broker |

`bash("… $x")` uses Julia interpolation. In `sh"…"`, `$` belongs to the shell.
For a complex script, send a tool payload and call `bash(PAYLOAD)`; payload is
verbatim text for this call only. Standard Julia `run` and `read` also work within
the session's authority envelope.

## Call costs

`Palette.costs(call)` reports `execution_seconds` and `compile_seconds`, the part
of execution the process spent compiling. `first_call_in_kernel` is true for the
first agent call a kernel runs after its warm-up, the call that pays for any
compilation the warm-up missed. Compile timing nests, so `@time` still works.

`palette_control` `status` returns `costs` for the current kernel: the host's
start-up seconds, the first call's costs, the warm-call count, and medians over
the last 20 warm calls. Round trips are measured by the operator, from request to
result. Ephemeral calls are not counted.

## Revival and freshness

A restart reconstructs supported definitions and deserializes supported data.
Reconstruction orders eager dependencies before their consumers, records actually
executed includes, and reports unavailable prerequisites. It does not replay
arbitrary whole calls. Tasks, handles, foreign runtime settings, and other
unsupported objects can be lost. Read the report before trusting restored values.

Named file evidence is attached at statement/include boundaries and inherited
through observed binding references. Unrelated literal assignments in another
statement do not inherit the entire call's file history. Include return values
also retain the included file's evidence. Blocks with local declarations or
returns retain their evaluation scope and use a conservative block observation.

Content digests are captured when a named source is observed and checked during
revival or `provenance(...; strong=true)`. A source whose size and modification
time were preserved can still be stale. Older snapshots without a saved digest
cannot establish that content baseline retroactively.

```julia
artifact = Palette.read_artifact("result.txt"; authority_path="result.commit.json")
result_text = artifact.content
Palette.provenance(:result_text)
```

The optional authority file is JSON containing `generation`, `commit`, and
`digest` (the content's SHA-256). A mismatched committed digest rejects the read.
Changed content or authority tokens mark the retained observation
`artifact_superseded`; useful old data remains available. Ordinary `Base.read`
does not claim knowledge of an external writer's commit authority.

These are observations, not full causal provenance. Dynamic reads, qualified
`Base.include`, and in-place mutations can escape observation. An absence of
observed staleness is not proof of freshness.
