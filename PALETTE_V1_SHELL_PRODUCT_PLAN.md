# Palette v1 — Interactive Computational Shell Product Plan

**Status:** Proposed design / implementation roadmap; not a description of shipped functionality  
**Recorded:** 2026-10-11  
**Intended handoff:** Luna for scoped implementation, with human review and independent test evidence  
**Target:** Palette repository, on a documentation-only planning branch  
**Decision origin:** Review of Gemini's Bash-replacement proposal, reconciled with Palette's existing Julia kernel, Rust host, capability architecture, and revival behavior

> **North star:** Palette becomes a shell you can live in: ordinary host commands, persistent Julia computation, inspectable execution results, recoverability evidence, and intentionally separated authority for humans and agents. Replace Bash as the default interactive workflow **without** pretending to replace every Bash script in the ecosystem.

## 0. Executive decision record

### Adopt

1. **Structured subprocess execution** as the canonical way to launch external programs. Julia `Cmd` and `pipeline` already exist; build a carefully specified Palette process contract above them, not a competing subprocess implementation.
2. **A real terminal experience** with a `palette` entrypoint, command-first mode, Julia mode, editing/history/completion, exit codes, signals, and eventually PTYs and job control.
3. **Two execution profiles:** trusted user-operated local shell and independently sandboxed agent workers. Shared semantics do not imply shared permissions or live state.
4. **Crash-recovery semantics with evidence:** recover only supported data/state; classify uncertain external effects rather than blindly replaying commands.
5. **Self-hosted iteration:** where authorized, use Palette to inspect, build, test, and diagnose Palette from an isolated development worktree.

### Reject or defer

- **No implicit `bash -c` for ordinary command dispatch.** Keep explicit legacy Bash compatibility while migrating internals.
- **No claim that `Cmd` is a sandbox.** The existing Rust host, namespaces and capability broker remain the authority boundary.
- **No promise of exact pre-crash restoration** of running processes, I/O handles, shell jobs, credentials, native pointers, or non-idempotent side effects.
- **No assumption that `p"one" |> p"two"` creates a pipeline.** Julia's `|>` applies functions; OS pipelines need explicitly defined semantics.
- **No wholesale Rust rewrite** of Python/Node tools as a prerequisite to the shell.
- **No speculative file renames** as a substitute for usability, correctness, isolation and testing.
- **No default-login-shell cutover in early milestones.** Keep the existing OS shell as a fallback until independent acceptance tests pass.

### Working definition of success

A user can open `palette`, type `cd src`, `ls`, `git status`, `cat README.md | grep ...`, run Julia calculations in persistent state, inspect job/execution outcomes, recover supported state after a forced crash, and keep doing ordinary development work **without Palette internally routing these operations through Bash**. Explicit execution of third-party Bash scripts remains allowed.

## 1. Existing baseline — do not rewrite blindly

Repository documentation checked on 2026-10-11:

- `README.md`: persistent Julia state, whole-call parsing, revived states, MCP/host stdio, existing `sh"cmd"`/`bash(...)` access, Rust host, Python adapters, bubblewrap, tests.
- `src/Palette.jl`: Julia operator types, results, receipts and kernel state.
- `runtime/docs/operator.md`: normal Julia `run`/`read` already work within an authorized session; revival does **not** replay arbitrary calls.
- `runtime/docs/authority.md`: Rust broker enforces configured ceilings; allowed host commands are argv-based and do not invoke a shell; worker code can run subprocesses **inside** its configured sandbox; host effects require broker authority.
- `runtime/docs/workspaces.md`: workspace routing, patch approval, test-in-disposable-worker and protected trust-chain boundaries.
- `CONTRIBUTING.md`: focused feature branches, tests, archive experimental data privately, public docs describe supported behavior, additional toolchains/UIs may belong in independently installable components.

**Important:** these are checked documentation/contracts, not proof that every envisaged UI or workflow works today. Capture branch/commit, runtime versions and baseline test outputs before implementation. This plan must never be confused with deployed capability.

## 2. Product definition and personas

### 2.1 Primary personas

- **Human daily-driver developer:** fastest path from opening terminal to navigating files, running editors/builds, processing output and computing interactively.
- **Computational developer/researcher:** persistent Julia variables, libraries, rich representations, provenance and reproducible experiments.
- **LLM-connected agent:** same command concepts, but only within a separately bounded OS workspace and preauthorized capability ceiling.
- **Operator/maintainer:** can diagnose execution, inspect receipts, distinguish restored versus lost state, test updates and perform deliberate deployment.

### 2.2 v1 product boundaries

**In scope for v1 candidate**
- Dedicated launch command `palette` from a pinned installed distribution.
- A command-first interactive mode with deterministic native command parsing.
- An explicit Julia evaluation path without switching tools.
- Structured native subprocesses: argv, pipeline, cwd, env, redirection, exit status, output capture and streaming.
- Real terminal behavior for the chosen supported platform: signals, terminal sizing, interactive children and minimum useful job control.
- Durable history and supported shell-session metadata.
- Explicit trusted versus restricted mode and effect reporting.
- Reproducible installer, docs, tests, fallback strategy.

**Not promised in v1**
- Drop-in POSIX/Bash grammar compatibility or running arbitrary `.sh` files as Palette source.
- Replacement of `/bin/sh`, installers that require Bash, or operating-system boot scripts.
- A cross-platform shell without demonstrated support for each claimed OS.
- Perfect resurrection of processes or external changes.
- Automatic application of code patches to the running supervisor.
- New agent autonomy/credentials merely because a terminal was introduced.

### 2.3 Experience goals (measured, not asserted)

- Common commands require no function wrapper or `bash(...)`.
- A novice can tell whether input is command syntax or Julia syntax.
- A user can cancel a foreground child without corrupting the Palette session.
- Output behaves reasonably with programs that require a TTY.
- Prompt startup and simple command latency are acceptable against a recorded baseline (`bash`, Julia REPL and current Palette; record machine and measurement method).
- Errors explicitly identify parsing failures, spawn failures, nonzero exits, timeouts, cancellations and authority denials.
- Switching to Bash compatibility is an explicit, visible choice rather than a hidden fallback.

## 3. Target architecture

```text
                 Interactive terminal / palette CLI
                     | line editor, history,
                     | completions, prompt, mode
                     v
                 Palette input frontend
                     | explicit input mode;
                     | parse + validate entire command
              +------+---------------------+
              |                            |
      Shell command AST               Julia evaluator
              |                            |
              v                            v
      Palette Process API          Persistent Julia world
              |                            |
       Execution policy             Observations/revival
              |                            |
    +---------+---------------+------------+
    |                         |
Trusted human profile      Restricted agent profile
(OS user authority)         (separate OS process / sandbox)
    |                         |
Native Julia Cmd            Native Cmd only within sandbox
and Rust PTY host           OR Rust broker for privileged effects
    |                         |
    +-------------+-----------+
                  |
          Receipts / audit /
          typed errors / journal
```

**Boundary rules**

1. **Session separation:** no agent attachment inherits a human shell's live Julia module, unrestricted terminal, host environment or credentials.
2. **Process separation:** sandboxing is an OS-process/namespace boundary, not a Julia thread. Use existing bubblewrap and host policy; a mode flag alone provides no isolation.
3. **Capability separation:** ordinary worker-local processes can run subject to OS confinement. Only privileged, out-of-sandbox effects use the Rust broker; the worker cannot grant itself authority.
4. **No shell injection by construction:** command invocation preserves program/argv elements. The frontend must parse and reject unsupported syntax; it must not concatenate them into a shell command string.
5. **State is not authority:** restored bindings, history or files cannot revive an expired capability grant.
6. **Deploy separately:** a modified prototype worktree is not the deployed supervisor. Never hot-swap a live security boundary because a test passed inside its worker.

## 4. Interface contracts before implementation

These are **proposed schemas**, not current exported types. Luna must inspect existing types/results and reconcile naming before coding.

### 4.1 Process request

```text
ExecSpec {
  program: absolute-or-resolved executable identifier
  argv: array<string>             # args separated, never reinterpreted by Bash
  cwd: canonical logical cwd
  env_delta: map<string,string?>  # controlled updates/deletions
  stdin: inherit | bytes | file | pipeline
  stdout: inherit | capture | file | pipeline
  stderr: inherit | capture | file
  timeout: optional duration
  tty: none | pty
  authority_context: opaque host-owned session identifier
  output_limits: optional bounded capture limits
}
PipelineSpec {
  stages: array<ExecSpec>
  pipefail_policy: documented setting
}
```

Key invariants:
- Program lookup uses a defined PATH policy and produces the resolved executable in the receipt where permitted. No implicit globbing inside the process API.
- A shell lexer/AST may expand user-friendly syntax **before** producing `ExecSpec`; the native process layer must accept already-separated args.
- Strings from user input, agent input and environment stay data unless intentionally parsed as a user command line.
- Environment merges are explicit; values containing spaces, `$`, backticks or semicolons do not acquire shell semantics.
- Arguments preserve Unicode and byte/encoding behavior as supported by the OS; reject invalid values clearly.
- Pipeline and redirection setup failures have first-class failure results; do not mistake a successful final stage for success of all stages unless the selected policy says so.
- File redirection must be created in the intended authority domain and obey sandbox mounts.
- Large output can be streamed and truncated with visible flags, without unbounded RAM or disk use.
- Execution supports deadline, cancellation, process-tree cleanup and deterministic release of handles.
- The first implementation need not expose every option above, but unsupported fields must fail visibly rather than silently degrade.

### 4.2 Execution result/receipt

```text
ExecutionResult {
  request_id, session_id, time_started, time_ended
  launch_status: started | refused | failed_to_spawn
  stage_results: [{program, pid_if_allowed, exit_code?, signal?, state}]
  aggregate_exit_code?, success
  stdout_ref?, stderr_ref?, stdout_truncated, stderr_truncated
  timed_out, cancelled
  cwd, profile, authority_decision_ref?, effect_receipt_ref?
  error: typed_error?
}
```

Distinguish `not_found`, `permission_denied`, `invalid_argv`, `unsupported_syntax`, `spawn_failed`, `nonzero_exit`, `timeout`, `cancelled`, `broker_denied`, and `unknown_completion`. Never mark a command completed solely because a request started.

### 4.3 Command grammar: specify and test, not accidentally inherit Bash

**Proposed first-step UX** (subject to real prototype evaluation):

```text
palette> pwd
palette> cd src
palette> ls -la
palette> git status
palette> :j x = 41
41
palette> :j x + 1
42
palette> :julia          # explicitly enter multiline Julia mode
julia> # Julia expressions...
julia> :shell          # return to command mode
palette> echo hello | grep hello
```

- Default command mode: external command and named builtins. No ambiguous auto-eval fallback.
- `:j EXPR` evaluates one Julia expression; `:julia` / `:shell` are explicit mode switches. Exact names are provisional until UX test.
- Command mode's lexer handles quoted strings, whitespace and backslash rules documented by Palette. Never use `eval` or `bash -c` to repair failed tokenization.
- Stage 1 grammar: program + argv; named builtins (`cd`, `pwd`, `exit`, `history`); status codes.
- Stage 2: pipeline `|`, redirection `<`, `>`, `>>` and approved expansion rules.
- Stage 3: foreground/background `&`, jobs, `fg`, `bg`, signals and PTY behavior.
- Expansion policy must state whether environment expansion (`$NAME`), home expansion (`~`), globbing, command substitution, brace expansion and parameter operators exist. **Never silently borrow Bash expansion.** Recommend implementing only explicitly tested, useful subsets; command substitution and arbitrary shell metaprogramming are deferred.
- Parsing a whole input should not start any subprocess if its grammar is invalid. For a multi-command construct, define atomicity limits explicitly.
- History should record the input mode, sanitized/redacted input where required, outcome and timestamp. Sensitive arguments cannot be presumed safe to persist verbatim.

## 5. Terminal mechanics required for a usable shell

A custom Julia REPL prompt is **necessary but insufficient**. On supported Unix terminals, plan for:

1. **Line editor:** editable multiline input in Julia mode; command-mode history; reverse search; clear keybindings; robust redraw after output.
2. **Completions:** filesystem paths, executables on PATH, commands, builtins and Julia names in their respective modes; avoid running untrusted completion hooks implicitly.
3. **Prompt:** cwd, active environment/workspace, profile and last exit status; visual trust boundary (trusted vs restricted) that is difficult to overlook.
4. **TTY/PTY:** interactive `vim`, `less`, `top`, `ssh` (where allowed), child terminal size propagation and clean stdin forwarding; some commands require direct TTY rather than buffered capture.
5. **Signals:** `Ctrl-C` reaches the foreground process group without crashing Palette; `Ctrl-D` exits when appropriate; suspend/resume semantics are specified.
6. **Job control:** asynchronous process launch, job list, foreground transfer and background resume, zombies reaped, tty modes restored after the child stops/exits.
7. **Exit semantics:** `$?`-equivalent via a documented builtin/API, consistent pipeline policy, distinct interrupt and spawn failure statuses.
8. **Environment/cwd:** `cd` changes session cwd (not the host process's global cwd in ways that leak between independent sessions). Environment changes are owned by that session and do not leak to other agent workers.
9. **Shutdown:** no orphaned child process groups, uncaught terminal raw mode or corrupted user terminal. Unexpected child failure must be recoverable.
10. **Detach/reconnect:** defer until defined; a persistent Julia daemon is not automatically a detachable PTY multiplexer.

**Architectural spike required:** determine whether Julia's REPL can safely own editing while the Rust host handles PTY/foreground process-group duties. Do not assume terminal-control methods work inside an MCP stdio worker. Test real terminal interactions from an actual terminal.

## 6. Durable session state and crash behavior

Palette currently documents restored/rebuilt/stale/lost/uncertain outcomes and disclaims arbitrary-call replay. Retain that truthful contract.

### Persistable shell state (candidate)

- cwd when it still exists and remains authorized;
- explicit exported environment deltas with secret filtering;
- prompt preferences, mode, safe aliases/functions once their serialization is defined;
- history entries with privacy controls and retention policy;
- session configuration, provenance, and references to effects/receipts.

### Never claim automatic reconstruction of

Running child processes, PTYs, job membership, open FDs, external program state, native pointers, arbitrary Julia tasks, OS environment changes made by another process, or side effects committed to network/filesystem beyond Palette's observed control.

### Intent / completion journal

1. Write structured **intent** with stable request ID before starting an externally effectful action.
2. Log broker authorization, actual launch and available process identifiers as separate events.
3. Record bounded completion/result and relevant receipt references after observation.
4. On forced crash, classify started-but-unfinished effect as **uncertain** until independently reconciled.
5. Never automatically repeat a potentially non-idempotent external operation just because its completion event is missing.
6. Snapshot supported session metadata at tested boundaries; use versioned schemas/migrations and atomic write/rename/fsync policies suited to the durability target.
7. If history writing or journal persistence fails, make degraded guarantees explicit. Define whether the session may continue.
8. Test interrupted writes, stale directories, changed executables, missing packages, re-grant requirements and process death between effect and receipt.

## 7. Security, sandbox and supply-chain requirements

- Existing Rust host + bubblewrap is authoritative for restricted workers; an agent's `exec` call must never inherit trusted shell permissions.
- All new terminal/broker operations require explicit ceilings where they escape the sandbox.
- PATH, cwd and executable resolution must be tied to the execution profile; do not execute attacker-controlled binaries due to silent cwd precedence.
- Preserve authorization receipts; guard against command/argv escalation via `sudo`, launchers, dynamic interpreters or unanticipated `extra_args` on privileged host commands. Operator-configured approved commands must be narrow.
- Never put secret-bearing host environment variables into agent Julia bindings, prompt templates, persistent history or stdout receipts.
- PTY bridging must prevent one session from reading another's input/output; refuse cross-session attachment without proper host-side authorization.
- Follow protected runtime/source boundaries; a sandboxed worker cannot bless a trust-chain patch.
- Dependencies may execute build scripts: continue to install allowed packages through existing constrained broker process; do not turn the shell launcher into an unscoped package installer.
- Reject control characters/escape injection in displayed untrusted output where needed for secure terminal UX, while preserving useful ANSI presentation when explicitly enabled.
- Fuzz parser, redirection paths and protocol framing; enforce limits on arguments, pipeline depth, capture size, open files and running jobs.
- Use explicit audit policy with redaction and separate operator diagnostics from agent-visible data.

## 8. Work breakdown, dependencies and acceptance gates

Every work packet is intended as an isolated, reviewable PR with real tests and captured before/after behavior. **Do not merge solely on agent self-reported success.**

### M0 — Baseline and architectural inventory

**Deliverables**
- Record pinned base commit; installed Julia/Rust/Python/Node versions; supported Linux distro/terminal; current launch commands and host profile.
- Map `bash(...)`, `sh"..."`, native `Cmd`, broker `host_command`, associated tests, journaling and terminal paths.
- Identify what can be a separate optional CLI/extension versus what requires core/host contract change.
- Record baseline `Pkg.test()`, Rust tests, analyzer/process gates on an authorized local build. Tests not executed must be labeled unexecuted with reason.

**Acceptance:** a concrete contract map, risk register, test baseline, and no production changes. **Gate:** human confirms which supported OS/platform is first.

### M1 — Structured process engine, headless

**Build**
- Define versioned `ExecSpec`, `PipelineSpec`, `ExecutionResult` and typed error vocabulary after inspecting existing types.
- Implement argv execution on Julia `Cmd` and `pipeline`; working directory/environment policy; captured stdout/stderr; bounded output; exit statuses; deadlines/cancellation.
- Integrate existing brokered request path only for effects beyond worker authority. Retain existing `bash` as explicitly named compatibility path.
- Unit-test parser-independent requests without launching a shell. Add integration tests in both trusted test environment and restricted workspace.

**Acceptance:** `echo`, `git status`, two-stage pipeline, Unicode/whitespace/quotes, exit 0/7, unknown executable, stderr-only child, output truncation, timeout, cancellation and broker denial return accurate typed results. Trace and confirm no `bash -c` for native `ExecSpec`. **Gate:** security review before adding privileged broker surfaces.

### M2 — Parser and minimum command-first prototype

**Build**
- Small versioned lexer/AST for the documented non-Bash subset.
- `palette` launcher in a **non-default** location; command mode + explicit Julia one-shot/mode.
- Builtins: `cd`, `pwd`, `exit`, `history`; per-session cwd/environment; clear syntax errors.
- Capture a real exploratory usability log of friction points; keep private raw runs out of public docs.

**Acceptance:** user can run a five-minute workflow with `cd`, `ls`, `git`, `:j x=41`, `:j x+1`, simple pipeline. Parsing bad input causes zero executions. Shell-quoted injection strings remain literal arguments. **Gate:** review grammar and user feedback before broadening syntax.

### M3 — Interactive terminal correctness

**Build**
- Line editing, complete/reverse-search, completion, prompt, colors without unsafe display behavior.
- Foreground TTY child, resize, `Ctrl-C`, `Ctrl-D`, terminal modes restored.
- Background jobs, process groups, `jobs`, `fg`, `bg` where supported.
- Clean disconnection/shutdown without unmanaged child processes.

**Acceptance:** manual PTY regression script across `vim`/`less`/a test interactive program, stopped/resumed child, repeated interrupts, resized window and abnormal child exit; automation via pty-based integration tests where feasible. **Gate:** no default shell rollout until signal/TTY reliability passes.

### M4 — Persistent shell state and truthful restart UX

**Build**
- Shell-session metadata schema and disk-backed history; redaction/retention.
- Durable journal event sequence with state/result/uncertainty classifications, integrated with existing revival reports.
- Forced-kill harness at defined points before spawn, after spawn and after external effect but before completion receipt.
- Restart UI reports restored/rebuilt/stale/lost/uncertain and reauthorization needs.

**Acceptance:** cwd and safe session settings restore when valid; invalid/missing paths show clear stale/lost outcomes; non-idempotent command is **not** re-executed after a crash; uncertain outcome is visible; prior credentials/grants are not revived. **Gate:** independent crash-data review.

### M5 — Trusted human profile vs. restricted agent profile

**Build**
- Explicit profile selection and visible prompt.
- Separate processes, home/environment isolation and broker-authorized effects.
- Clear path for same headless process API to be called via MCP or human CLI without shared state/authority.
- End-to-end tests attempting cross-profile access.

**Acceptance:** agent cannot read trusted env/secrets/home, cannot inherit PTY or host-only command grants, and cannot write protected source; human workflow retains legitimate host access. Denied operations create accurate receipts. **Gate:** threat-model review and actual sandbox tests.

### M6 — Packaging and developer experience

**Build**
- Reproducible `palette` executable/launcher, installed runtime version checks, init/config, upgrade/uninstall and fallback notes.
- Julia dependencies pinned as needed; Rust host build/release integration. Avoid Python/Node consolidation unless deployment data proves the benefit.
- Docs: quickstart, shell subset, mode switching, commands, security profiles, scripts compatibility, recovery and troubleshooting.
- Cross-shell workflows and performance measurements versus baselines.

**Acceptance:** clean install in an independently provisioned VM/container; launching does not require manual `julia --project=...`; uninstall restores original login shell; old Palette MCP workflows still pass. **Gate:** user chooses optional daily-driver trial.

### M7 — Opt-in daily-driver beta and v1 decision

**Build**
- Use Palette for real development tasks, including changes/tests for Palette in a separate worktree.
- Log friction against Bash and IPython: command typing, terminal handling, completion, Julia expressiveness, crash recovery, and latency.
- Fix high-frequency failures before expanding grammar.
- Document known limits and explicit Bash compatibility path.

**Graduation conditions:** one sustained trial across routine workflows with no silent privilege boundary failures, no recurrent terminal corruption, passing selected suite, reproducible rollback and a clear list of unsupported shell syntax. Publish v1 only after a human go/no-go review.

## 9. Comprehensive verification matrix

| Domain | Minimum test families | Required observation |
| --- | --- | --- |
| Grammar | quotes, escapes, empty args, special characters, unsupported operators, malformed pipelines | deterministic AST/error, no accidental launches |
| Injection | semicolons, `$`, backticks, spaces, untrusted file names, adversarial env | argv remains data; no interpreter fallback |
| Process | missing binary, nonzero exit, stderr, stdin EOF, UTF-8/binary output, timeout, stop/cancel, zombie reaping | exact statuses, bounded capture |
| Pipelines | all-success, early-stage failure, final-stage failure, broken pipe, mixed stderr, redirect failure | explicit policy and stage results |
| Environment | cwd changes, PATH lookup, vars removed/set, cwd deleted, symlinks, multiple sessions | no unwanted cross-session leakage |
| TTY | editor, pager, interactive prompts, resize, Ctrl-C/Ctrl-Z, fg/bg, terminal reset | real foreground process behavior |
| Security | unauthorized mounts, broker denials, credential inheritance, shared state, hostile executables | confinement enforced by OS/host |
| Durability | journal write failure, power/kill windows, stale cwd, outdated data, uncertain effects | honest labels, no unsafe replay |
| Compatibility | Julia evaluation, legacy `bash` APIs, MCP adapter, existing integrations | no unreviewed regressions |
| Packaging | clean install, upgrade, uninstall, failure fallback | reversible setup |
| Performance | cold launch, warm launch, simple command latency, pipeline throughput, memory growth | recorded comparable measurements |

**Repository verification commands to run in a compatible, authorized checkout:**

```sh
julia --project=. -e 'using Pkg; Pkg.test()'
cargo test --manifest-path runtime/host/Cargo.toml --locked
python3 runtime/scripts/test_julia_analyzer.py
node runtime/scripts/check-test-policy.mjs
# When the documented Linux process-test environment is provisioned:
python3 runtime/security/verify_operator.py --project-dir /absolute/path/to/test-env --implementation both
```

These are **instructions**, not claimed successful test runs. Use the repository's current `AGENTS.md`, CI policy and `CONTRIBUTING.md` when actual work begins; they supersede stale paths or test recipes in this proposal.

## 10. Luna implementation handoff protocol

**Delegation philosophy:** use Luna for bounded development and regression-test cycles, not for unilateral rewrites, security-policy relaxation or silent deployment. Cheap model inference may help throughput, but **actual cost, available access and quality are not confirmed** here.

### Recommended work packets (one branch/PR each)

| Packet | Depends on | Luna task | Review focus |
| --- | --- | --- | --- |
| P0 | M0 | inventory existing process doors/results/tests; propose minimal new contract | factual accuracy, backward compatibility |
| P1 | P0 | add headless argv execution and typed result tests | parser independence, no Bash |
| P2 | P1 | add native pipeline + redirection semantics | fd lifetime, pipefail, truncation |
| P3 | P1 | add parser/AST and injection test corpus | whole-input validation, no eval |
| P4 | P3 | command-first client + mode switch | ergonomics, no ambiguity |
| P5 | P4 | completion/history/builtins | no unsafe side effects in completion |
| P6 | P4 | terminal/PTY/job control integration spike | process groups, terminal restoration |
| P7 | P1/P6 | journal + crash/failure injection tests | recovery truthfulness |
| P8 | P1/P4/P7 | authority-profile integration tests | strict confinement |
| P9 | P4/P6/P8 | packaging, clean install and beta docs | reproducibility, rollback |

P2 and P3 can proceed independently **only after** P1's interface is stable. P6 is a spike that may reveal a needed Rust-side change; do not insist Julia REPL alone can own PTY control. P8 is never validated only by mocks.

### Template instruction for each delegated packet

1. Read `AGENTS.md`, `CONTRIBUTING.md`, this plan and relevant current source/docs.
2. Report the current symbols/paths and base commit before changing code. Do not assume this proposal names live APIs.
3. Scope the patch to **one** packet and one isolated worktree/branch. Do not modify unrelated Gesso/Cyan work.
4. Add negative and integration tests for the specific behavior, then implement the smallest passing change.
5. Return a concise artifact: modified paths, contract changes, actual commands run, pass/fail/skipped outcomes, security implications, known limitations and exact commit/PR.
6. Never claim tests passed if not run. Never convert a security denial into a broad allow rule to make CI green.
7. Do not deploy, change login shell, grant capabilities or alter protected runtime paths without explicit authorized review.

**Review rhythm:** Luna implements scoped packet -> independent reviewer checks interfaces/threat model -> Palette runs allowed tests in isolated workspace -> fix regressions -> merge only after gates -> start next packet.

## 11. Execution and self-hosting runbook

1. Create a dedicated clean worktree off a pinned base commit, **not** the live production checkout.
2. Capture `git status`, base commit and test-environment versions before a change.
3. Use the currently permitted Palette worker for *inspection, experiments and authorized workspace tests*. Its ability to evaluate Julia does not imply permission to rewrite the protected host.
4. Where source patching is needed, follow `runtime/docs/workspaces.md`: prepare exact diff+hash; use the real operator approval flow; test separately; do not infer approval.
5. Run unit/host tests inside an environment matching required toolchains and mounts. Record cases that cannot run inside a sandbox or without a PTY.
6. Keep exploratory transcripts/receipts private (`.archive/` or approved external storage), with redacted secrets; publish only validated product guidance.
7. Verify migration path and fallback in an independent user shell before any default-shell experiment.
8. After changes, inspect revival evidence following process restart; never assume persistence equals exact state recovery.
9. Route security-relevant patches and deployment to human review; keep the working Palette kernel protected from its own experiments.

## 12. Design uncertainties / decisions to settle by experiment

- Is the first **supported interactive OS** Linux only? (Given current supervised Linux stack, recommend Linux first; test WSL separately rather than promise parity.)
- Does trusted terminal UX use Julia REPL editing + a Rust PTY bridge, or a standalone Rust frontend that talks to the persistent Julia supervisor? Build minimal comparative spikes before committing.
- Where should the public process API live: Julia package core, optional shell module, or separate CLI companion? Prefer an optional frontend and minimal core contract changes, per `CONTRIBUTING.md`.
- How should environment expansion and globbing work without Bash ambiguity? Write examples and falsifiable tests before shipping.
- Should prompt enter command mode by default, or remember per-user preference? Suggest command-first for shell launcher and plain Julia for existing Julia use.
- Which exit-status policy for pipelines? Make it explicit and configurable; no silent assumptions about Bash `pipefail`.
- How are secrets excluded from saved history/journals while retaining enough evidence to troubleshoot?
- How are external-effect receipts linked to a local, interactive command with no broker mediation? Distinguish observed process status from guarantees about external world effects.
- What are target launch/latency budgets based on measured current machine numbers, and how much overhead is tolerable for rich durability?
- When is a real shell ready to be selected as an OS login shell? Require PTY, jobs, signals, startup/fallback and crash correctness first.

## 13. Change-control and release policy

- **M0–M2:** experimental prototype; no login-shell replacement and no official Bash-compatibility claim.
- **M3–M5:** security/terminal hardening; user testing by explicit opt-in only.
- **M6:** candidate installer and documented limits; reproducibility gates.
- **M7:** sustained real use and human v1 decision; only then consider default shell setup.
- Preserve explicit `bash` compatibility for scripts and programs which depend on it. Gradually migrate Palette-owned code away from implicit Bash.
- Preserve the established plugin/SDK and operator surfaces unless changes are versioned, documented and migration-tested.
- Avoid a Python/Node-to-Rust rewrite and branding cleanup until empirical deployment or maintenance data justifies them.
- Protect other ongoing tracks (Gesso, Cyan harness) by using isolated branches/worktrees, change reviews and explicit ownership.

## 14. Immediate next action

**Luna Packet P0:** On a fresh isolated branch, produce a source-grounded execution map identifying every entry point that can spawn a process, shell-string execution door, Rust host command dispatch path, existing result/receipt type, test hook, and permission boundary. Recommend the *smallest* new typed process API that preserves current behavior. Return a design PR and baseline evidence; **do not yet modify the production runtime**.

**Next human checkpoint:** review P0 and choose one authoritative command/result contract before P1 work begins.

---

### Reference documentation (existing repository)

- [README](README.md)
- [Core Julia implementation](src/Palette.jl)
- [Operator API](runtime/docs/operator.md)
- [Authority boundaries](runtime/docs/authority.md)
- [Workspaces and reviewed patches](runtime/docs/workspaces.md)
- [Contributing](CONTRIBUTING.md)
- [Development instructions](AGENTS.md)

**Interpretation:** This document is a detailed **proposal and handoff**, not a shipped feature, approved security design, successful CI run, or authorization to change a running service.
