# The turn machinery of runtime/scripts/session_loop.jl: capturing a turn's output,
# running it under a deadline, and shaping its value for the protocol. It
# lives in the package so its native code is cached with the package; as
# part of the unprecompiled script it compiled in every new kernel, about
# seven seconds before the first reply.

# Enough for any output a model can use; the host applies its own tighter cap.
const MAX_OUTPUT_BYTES = 256 * 1024
const MAX_DATA_JSON_BYTES = 256 * 1024

# Output is whatever bytes the turn printed. JSON.jl copies invalid UTF-8
# into the line as-is, and the reader rejected the whole line.
valid_utf8(s::String) = isvalid(s) ? s : String(map(c -> isvalid(c) ? c : '\ufffd', collect(s)))

function safe_json(x)
    json = try
        JSON.json(x)
    catch
        JSON.json(string(x))
    end
    return valid_utf8(json)
end

# Julia-level stdout and stderr for a turn: every write is one write(2) to
# the capture file, which fd 1 and fd 2 (and so every `run` child) also
# point at. Writes land in the order they happen, with no buffering and no
# task switch; through a libuv pipe each `println` cost ~50us, and printing
# 10^6 lines outran the turn limit.
struct FdWriter <: IO
    fd::Cint
    target::IOStream
end
function Base.unsafe_write(w::FdWriter, p::Ptr{UInt8}, n::UInt)
    done = 0
    while done < n
        r = ccall(:write, Cssize_t, (Cint, Ptr{UInt8}, Csize_t), w.fd, p + done, n - done)
        if r < 0
            Libc.errno() == Libc.EINTR && continue
            throw(SystemError("write", Libc.errno()))
        end
        done += r
    end
    return Int(n)
end
Base.write(w::FdWriter, b::UInt8) = (r = Ref(b); GC.@preserve r unsafe_write(w, Base.unsafe_convert(Ptr{UInt8}, r), UInt(1)))
Base.isopen(::FdWriter) = true
Base.flush(::FdWriter) = nothing

# `redirect_stdout(devnull) do ... end` in turn code restores the stream it
# found, which is an FdWriter, and Base has no method for that: the idiom
# threw, and a redirect to a file left fd 1 on that file for the rest of the
# turn, so everything printed after it was lost.
function (f::Base.RedirectStdStream)(w::FdWriter)
    f.unix_fd in (1, 2) || throw(ArgumentError("cannot redirect fd $(f.unix_fd) to an FdWriter"))
    f(w.target)
    Base._redirect_io_global(w, f.unix_fd)
    return w
end

# tempdir() is looked up when a turn runs: this file is precompiled, and a
# constant would hold the build machine's temp directory.
capture_path() = joinpath(tempdir(), "palette-turn-output")

# Runs `f` with fd 1 and fd 2 on a fresh append-only file. A process the
# turn left running keeps its descriptor to that file, now unlinked, so its
# later output reaches neither this turn nor the next one.
function capture_output(@nospecialize(f))
    # Boxed so the closures below compile once, not once per caller: the
    # first real turn compiled them again for its own closure type (0.7s).
    fn = Ref{Function}(f)
    path = capture_path()
    rm(path; force=true)
    file = open(path, "a+")
    value = nothing
    try
        redirect_stdout(file) do
            redirect_stderr(file) do
                Base._redirect_io_global(FdWriter(1, file), 1)
                Base._redirect_io_global(FdWriter(2, file), 2)
                value = fn[]()
            end
        end
    finally
        close(file)
    end
    total = filesize(path)
    output = String(open(io -> read(io, MAX_OUTPUT_BYTES), path))
    keep_for_late_output!(path, total)
    total > MAX_OUTPUT_BYTES && (output *= "\n[output truncated: $total bytes total]")
    return value, output
end

# A process a call left running keeps writing to that call's capture file.
# The last few are kept, renamed, and what grows in them after their call is
# reported as background output at the start of the next call.
const LATE_FILES = Tuple{String, Int}[]
const KEEP_LATE_FILES = 32
const LATE_SEQ = Ref(0)

function keep_for_late_output!(path::String, total::Int)
    late = "$path-$(LATE_SEQ[] += 1)"
    mv(path, late; force=true)
    push!(LATE_FILES, (late, total))
    while length(LATE_FILES) > KEEP_LATE_FILES
        rm(popfirst!(LATE_FILES)[1]; force=true)
    end
    return
end

function late_output()
    parts = String[]
    for (i, (path, read_to)) in enumerate(LATE_FILES)
        total = isfile(path) ? filesize(path) : 0
        total > read_to || continue
        skip = max(read_to, total - MAX_BACKGROUND_BYTES)
        push!(parts, open(f -> (seek(f, skip); String(read(f))), path))
        LATE_FILES[i] = (path, drop_reported!(path, total))
    end
    return join(parts)
end

# The session module's name is an internal uuid, and types defined by turn
# code print qualified with it (`Main.KernelScope_2d97….P(3)`).
function scrub(s::AbstractString)
    name = string(nameof(get_kernel_state().eval_module))
    return replace(s, "Main.$name." => "", "$name." => "", "Main.$name" => "Main", name => "Main")
end
scrub(x) = x

const TEST_PKG = Base.PkgId(Base.UUID("8dfed614-e22c-5e08-85e1-65c5234f0b40"), "Test")

# A passing `include("test/runtests.jl")` returns its DefaultTestSet, whose
# default display dumps every nested testset down to each one's RNG state:
# 14,000 to 25,000 characters a run in the traces, all of it elided. It is
# shown as its counts, and the testset itself stays in `ans`.
function testset_summary(value)
    test = get(Base.loaded_modules, TEST_PKG, nothing)
    test === nothing && return nothing
    value isa test.DefaultTestSet || return nothing
    c = Base.invokelatest(test.get_test_counts, value)
    return "Test.DefaultTestSet \"$(value.description)\": $(c.passes + c.cumulative_passes) passed, " *
           "$(c.fails + c.cumulative_fails) failed, $(c.errors + c.cumulative_errors) errored, " *
           "$(c.broken + c.cumulative_broken) broken ($(c.duration))"
end

struct DisplayLimit <: Exception end
struct DisplayWriter <: IO
    bytes::Vector{UInt8}
    limit::Int
end
DisplayWriter(bytes::Vector{UInt8}) = DisplayWriter(bytes, MAX_OUTPUT_BYTES)
Base.isopen(::DisplayWriter) = true
Base.flush(::DisplayWriter) = nothing
function Base.unsafe_write(w::DisplayWriter, p::Ptr{UInt8}, n::UInt)
    remaining = w.limit - length(w.bytes)
    keep = min(Int(min(n, UInt(MAX_OUTPUT_BYTES))), remaining)
    keep > 0 && append!(w.bytes, unsafe_wrap(Vector{UInt8}, p, keep; own=false))
    n > UInt(remaining) && throw(DisplayLimit())
    return Int(n)
end
function Base.write(w::DisplayWriter, b::UInt8)
    length(w.bytes) < w.limit || throw(DisplayLimit())
    push!(w.bytes, b)
    return 1
end

function text_display(value)
    value === nothing && return nothing
    writer = DisplayWriter(UInt8[])
    try
        summary = testset_summary(value)
        if summary === nothing
            Base.invokelatest(show, IOContext(writer, :limit => true), MIME"text/plain"(), value)
        else
            write(writer, summary)
        end
        return scrub(valid_utf8(String(writer.bytes)))
    catch e
        e isa InterruptException && rethrow()
        e isa DisplayLimit && return scrub(valid_utf8(String(writer.bytes))) * "\n[display truncated at $MAX_OUTPUT_BYTES bytes]"
        return "<display failed: $(nameof(typeof(e)))>"
    end
end

# `data` carries the raw value only when it is a scalar or a short collection
# of short things. The host renders `display`; encoding a large value just to
# learn its size took 14.7s for a 400 MB array and 4.6s for a million-entry
# Dict, and `Base.summarysize` of that Dict alone takes 4.9s.
const SCALAR = Union{Nothing, Bool, Int8, Int16, Int32, Int64, Int128, UInt8, UInt16, UInt32, UInt64, UInt128,
                     Float16, Float32, Float64, String, SubString{String}, Symbol}
const MAX_DATA_ELEMENTS = 1000
const COLLECTION = Union{Array, BitArray, Dict, Set, Tuple, NamedTuple}
short(x) = x isa SCALAR ? !(x isa Union{AbstractString, Symbol} && sizeof(x) > MAX_DATA_JSON_BYTES) :
           x isa COLLECTION && length(x) <= MAX_DATA_ELEMENTS

# A short outer collection can contain reflection objects whose JSON fields
# reach cyclic compiler caches. Inspect leaves before asking JSON to encode
# them; cycles and excessively nested collections exhaust this bounded walk.
function bounded_json_shape(value, budget::Base.RefValue{Int}, depth::Int=0)
    budget[] -= 1
    (budget[] >= 0 && depth <= 8) || return false
    value isa SCALAR && return short(value) && !(value isa AbstractFloat && !isfinite(value))
    (value isa COLLECTION && short(value)) || return false
    if value isa Dict
        all(k -> (k isa Union{String, Symbol} || (k isa SCALAR && k isa Integer)) &&
                 bounded_json_shape(k, budget, depth + 1), keys(value)) || return false
    end
    items = value isa Dict ? values(value) : value
    return all(x -> bounded_json_shape(x, budget, depth + 1), items)
end

function bounded_data(value)
    if value isa Union{SCALAR, COLLECTION} && short(value)
        try
            bounded_json_shape(value, Ref(MAX_DATA_ELEMENTS + 1)) || return text_display(value)
            JSON.print(DisplayWriter(UInt8[], MAX_DATA_JSON_BYTES), value)
            return value
        catch e
            e isa InterruptException && rethrow()
        end
    end
    return text_display(value)
end

const TURN_COSTS = Dict{Int, Dict{String, Any}}()

function project_turn_result!(receipt::OperationReceipt)
    value = receipt.result.data
    data = receipt.result.success ? bounded_data(value) : nothing
    display = !receipt.result.success ? nothing : data === value ? text_display(value) : data
    receipt.metadata["transport"] = Dict{String, Any}("data" => data, "display" => display)
    return receipt
end

# Processes in this sandbox, other than the kernel and bwrap's reaper (PID 1,
# the kernel's parent). The sandbox has its own PID namespace, so /proc lists
# exactly what turns started. Outside such a namespace (a bare test run) there
# is no safe way to tell, and nothing is reaped.
own_namespace() = getpid() == 1 ||
    (ccall(:getppid, Cint, ()) == 1 && isfile("/proc/1/comm") && strip(read("/proc/1/comm", String)) == "bwrap")
function sandbox_pids()
    own_namespace() || return Set{Int}()
    pids = Set{Int}()
    self = getpid()
    for d in readdir("/proc")
        pid = tryparse(Int, d)
        pid === nothing || pid == 1 || pid == self || push!(pids, pid)
    end
    return pids
end
# Kills what a turn started, so a wait on it returns, and nothing it began
# keeps running after the turn is reported interrupted. Processes earlier
# turns started are left alone.
function reap(before::Set{Int})
    for pid in setdiff(sandbox_pids(), before)
        ccall(:kill, Cint, (Cint, Cint), pid, 9)
    end
end

# How long after the deadline a turn has to finish once interrupted. Turn code
# that catches the InterruptException gets it again every second.
const INTERRUPT_GRACE_S = 5.0

"""
    run_turn(f, timeout_s) -> (value, interrupted, stuck)

Runs `f` in its own task. At `timeout_s` the task gets an InterruptException
and the processes it started are killed; waiting work (sleep, run, read, I/O)
ends there with the kernel and every binding intact. The deadline is noticed
only when the task yields: compute that never yields runs until the host
kills the kernel. `stuck` means the task was still running when the grace
period ended.
"""
function run_turn(@nospecialize(f), timeout_s)
    t = Task(f)
    t.sticky = true
    schedule(t)
    timeout_s === nothing && return (fetch(t), false, false)
    before = sandbox_pids()
    interrupted = Ref(false)
    outcome = Channel{Symbol}(2)
    @async (try wait(t) catch end; put!(outcome, :done))
    deadline = Timer(timeout_s)
    watchdog = @async begin
        try
            wait(deadline)
        catch
            return  # closed: the turn finished in time
        end
        istaskdone(t) && return
        interrupted[] = true
        giveup = time() + INTERRUPT_GRACE_S
        while !istaskdone(t) && time() < giveup
            try
                schedule(t, InterruptException(); error=true)
            catch
            end
            reap(before)
            timedwait(() -> istaskdone(t), 1.0; pollint=0.05)
        end
        istaskdone(t) || put!(outcome, :stuck)
    end
    result = take!(outcome)
    close(deadline)
    result === :stuck && return (nothing, true, true)
    interrupted[] && reap(before)
    # An interrupt that lands after the turn's own error handling, while its
    # bookkeeping runs, fails the task itself.
    value = try
        fetch(t)
    catch e
        e isa TaskFailedException ? e.task.exception : e
    end
    return (value, interrupted[], false)
end

# Between calls, stdout and stderr (and fd 1 and 2 of every child) point at
# this file. A task still running after its call printed into it, and nothing
# read it back, so its output and its failure reached nobody.
const SINK = Ref{Union{Nothing, IOStream}}(nothing)
const SINK_PATH = Ref("")
const SINK_READ = Ref(0)
const MAX_BACKGROUND_BYTES = 16 * 1024
# Output already reported is dropped once a file holding it passes this size:
# the sandbox's /tmp is memory, and a chatty background job (50 KB/s in the
# endurance pass) grew it by 4 GB a day. Every writer holds these files in
# append mode, so truncating them is safe.
const MAX_REPORTED_BYTES = 1024 * 1024
function drop_reported!(path::String, read_to::Int)
    read_to > MAX_REPORTED_BYTES || return read_to
    open(path, "w") do _ end   # truncate
    return 0
end
const REPORTED_TASKS = WeakKeyDict{Task, Nothing}()
const FINISH_REPORTED = WeakKeyDict{Task, Nothing}()
# What the last jobs line said was running, so an unchanged set is not repeated.
const LAST_RUNNING = Ref(String[])
const TASK_OBSERVATIONS = WeakKeyDict{Task, Dict{String, Any}}()
const TASK_SEQUENCE = Ref(0)
const TASK_EPOCH = Ref("")

function observe_tasks!(mod::Module, call::Int, code::String)
    for (task, _) in bound_tasks(mod)
        haskey(TASK_OBSERVATIONS, task) && continue
        TASK_OBSERVATIONS[task] = Dict{String, Any}("id" => (TASK_SEQUENCE[] += 1),
            "observed_epoch" => isempty(TASK_EPOCH[]) ? string(get_kernel_state().id) : TASK_EPOCH[],
            "first_observed_call" => call, "source_digest" => bytes2hex(SHA.sha256(code)),
            "world_at_first_observation" => string(Base.get_world_counter()),
            "creation_call" => nothing, "dispatch_policy" => "unknown; ordinary Julia dispatch unless callback explicitly uses Base.invokelatest")
    end
    return nothing
end

function task_observation(task::Task)
    result = deepcopy(get(TASK_OBSERVATIONS, task, Dict{String, Any}(
        "first_observed_call" => nothing, "creation_call" => nothing, "dispatch_policy" => "unknown")))
    result["state"] = istaskfailed(task) ? "failed" : istaskdone(task) ? "finished" : istaskstarted(task) ? "running" : "not_started"
    result["failure_type"] = istaskfailed(task) ? string(nameof(typeof(task.result))) : nothing
    result["failure"] = nothing
    if istaskfailed(task)
        writer = DisplayWriter(UInt8[], 500)
        try
            Base.invokelatest(showerror, writer, task.result)
        catch e
            e isa InterruptException && rethrow()
            e isa DisplayLimit || return merge(result, Dict("failure" => "<error rendering failed>"))
        end
        result["failure"] = scrub(valid_utf8(String(writer.bytes)))
    end
    result["limits"] = "First observation is not creation. Callback world age and late binding are not inferred from arbitrary task code. Unbound/internal tasks cannot be enumerated."
    return result
end

function task_observations()
    pairs = bound_tasks(get_kernel_state().eval_module)
    return Dict("tasks" => [merge(task_observation(t), Dict("name" => string(sym))) for (t, sym) in first(pairs, MAX_LISTED_BINDINGS)],
        "listed" => min(length(pairs), MAX_LISTED_BINDINGS), "total_named" => length(pairs))
end

function task_notice_hint(text::String)
    (occursin("failed Task notice", text) || occursin("Unhandled Task ERROR", strip_ansi(text))) || return text
    return text * "\n[Julia reported a task failure without an established task identity. Palette.jobs() inspects named tasks; this notice alone cannot establish its origin or creation call.]\n"
end

# Each Task the session holds, once, under the model's own name before `ans`.
function bound_tasks(mod::Module)
    byname = IdDict{Task, Symbol}()
    for sym in sort!(Base.invokelatest(names, mod; all=true); by=s -> s === :ans)
        Base.invokelatest(isdefined, mod, sym) || continue
        t = Base.invokelatest(getglobal, mod, sym)
        t isa Task && !haskey(byname, t) && (byname[t] = sym)
    end
    return sort!(collect(byname); by=p -> string(p[2]))
end

"""
    background_processes() -> Vector{Tuple{Int, String, Float64}}

Processes earlier calls left running in the sandbox, as (pid, command line,
seconds running): the kernel's children, and daemons the sandbox's init
adopted (a server started with `&`, a database started with pg_ctl).
"""
function background_processes()
    out = Tuple{Int, String, Float64}[]
    isfile("/proc/uptime") || return out
    me = getpid()
    uptime = parse(Float64, split(read("/proc/uptime", String))[1])
    hz = ccall(:sysconf, Clong, (Cint,), 2)   # _SC_CLK_TCK
    for p in readdir("/proc")
        pid = tryparse(Int, p)
        (pid === nothing || pid == me || pid == 1) && continue
        stat = try read("/proc/$pid/stat", String) catch; continue end
        rest = split(stat[findlast(')', stat)+2:end])
        ppid = parse(Int, rest[2])
        (ppid == me || ppid == 1) || continue
        cmd = strip(replace(try read("/proc/$pid/cmdline", String) catch; "" end, '\0' => ' '))
        (isempty(cmd) || occursin("session_loop.jl", cmd)) && continue
        push!(out, (pid, cmd, max(0.0, uptime - parse(Float64, rest[20]) / hz)))
    end
    return sort!(out; by=first)
end

fmt_age(s) = s < 60 ? "$(round(Int, s))s" : s < 3600 ? "$(round(Int, s / 60))m" : "$(floor(Int, s / 3600))h$(round(Int, (s % 3600) / 60))m"

# One line on the session's background work, when it changed since the last:
# what runs, and what finished and waits to be collected.
# host_command jobs this kernel started, by asking the broker only while
# there are some: a job that finished is named once, with the poll that
# returns its result.
const HOST_FINISH_REPORTED = Set{String}()
function report_host_jobs!(running::Vector{String}, finished::Vector{String})
    isempty(HOST_JOBS) && return
    # The line runs before the call's code and outside its deadline, so a
    # broker slow to answer skips it rather than holding up the call.
    listed = try
        host_jobs(; timeout_s=10)
    catch e
        push!(running, "host jobs (listing them failed: $(sprint(showerror, e)))")
        return
    end
    held = Set{String}()
    for j in listed
        id = string(j["job"]); haskey(HOST_JOBS, id) || continue
        push!(held, id)
        if j["running"] === true
            push!(running, "host job `$(j["name"])` (Palette.host_command_poll(\"$id\"))")
        elseif !(id in HOST_FINISH_REPORTED)
            push!(HOST_FINISH_REPORTED, id)
            push!(finished, "host job `$(j["name"])` (Palette.host_command_poll(\"$id\") returns its result)")
        end
    end
    # Jobs the broker no longer holds (its session restarted) are gone.
    for id in collect(keys(HOST_JOBS))
        id in held || (delete!(HOST_JOBS, id); delete!(HOST_FINISH_REPORTED, id))
    end
end

function report_jobs(mod::Module)
    running = String[]; finished = String[]
    for (t, sym) in bound_tasks(mod)
        if !istaskdone(t)
            push!(running, "task `$sym`")
        elseif !istaskfailed(t) && !haskey(FINISH_REPORTED, t)
            FINISH_REPORTED[t] = nothing
            push!(finished, "task `$sym` (fetch($sym) returns its value)")
        end
    end
    report_host_jobs!(running, finished)
    procs = background_processes()
    for (pid, cmd, _) in first(procs, 6)
        push!(running, "`$(first(cmd, 80))` (pid $pid)")
    end
    length(procs) > 6 && push!(running, "$(length(procs) - 6) more processes")
    changed = running != LAST_RUNNING[]
    LAST_RUNNING[] = running
    (changed || !isempty(finished)) || return nothing
    parts = String[]
    isempty(running) || push!(parts, "running: " * join(running, ", "))
    isempty(finished) || push!(parts, "finished: " * join(finished, ", "))
    isempty(parts) && (parts = ["nothing is running any more"])
    println("[background jobs: ", join(parts, "; "), "]")
    return nothing
end

"""
    report_background(mod)

Prints what happened since the last call: output that background tasks and
processes wrote, and each failed task bound in `mod`, once.
"""
function report_background(mod::Module)
    isempty(TIME_MACHINE_NOTICE[]) || (println(TIME_MACHINE_NOTICE[]); TIME_MACHINE_NOTICE[] = "")
    text = late_output()
    io = SINK[]
    if io !== nothing
        flush(io)
        total = filesize(SINK_PATH[])
        if total > SINK_READ[]
            skip = max(SINK_READ[], total - MAX_BACKGROUND_BYTES)
            text *= open(f -> (seek(f, skip); String(read(f))), SINK_PATH[])
            SINK_READ[] = drop_reported!(SINK_PATH[], total)
        end
    end
    if !isempty(text)
        length(text) > MAX_BACKGROUND_BYTES && (text = "…" * last(text, MAX_BACKGROUND_BYTES))
        print("[background output since the last call]\n", text, endswith(text, '\n') ? "" : "\n")
    end
    for (t, sym) in bound_tasks(mod)
        istaskfailed(t) && !haskey(REPORTED_TASKS, t) || continue
        REPORTED_TASKS[t] = nothing
        observation = task_observation(t)
        err = first(split(something(observation["failure"], "unknown failure"), '\n'))
        println("[background: task `$sym` failed: $err]")
        println("[task provenance: first observed call $(observation["first_observed_call"]); inspect Palette.jobs() for source/world observations]")
    end
    report_jobs(mod)
    return nothing
end

# Workspace files a call named, with their modification time and size then,
# and the call. A binding computed from a file the model later changed
# outside the kernel kept the old contents, with nothing to say so.
const USED_FILES = Dict{String, Tuple{Tuple{Float64, Int}, Int}}()
const MAX_USED_FILES = 500

file_stamp(path) = (st = stat(path); (st.mtime, Int(st.size)))

function note_file!(path::AbstractString, call::Int)
    root = WORKSPACE_ROOT[]
    isempty(root) && return
    full = abspath(root, path)
    startswith(full, root * "/") && isfile(full) || return
    (haskey(USED_FILES, full) || length(USED_FILES) < MAX_USED_FILES) || return
    USED_FILES[full] = (file_stamp(full), call)
    push!(get!(CALL_FILES, call, Tuple{String, Tuple{Float64, Int}}[]), (full, USED_FILES[full][1]))
    return
end

function string_literals!(out::Vector{String}, ex)
    if ex isa String
        push!(out, ex)
    elseif ex isa Expr
        foreach(a -> string_literals!(out, a), ex.args)
    end
    return out
end

"""
    note_files_named!(code, call)

Records each workspace file that `code` names in a string literal, sh"..."
commands included, as used by `call`.
"""
function note_files_named!(code::String, call::Int)
    ex = try
        Meta.parseall(code)
    catch
        return
    end
    for lit in string_literals!(String[], ex), tok in split(lit, (' ', '\t', '\n', '\'', '"', ';', '|', '<', '>', '(', ')', '=', ','))
        isempty(tok) || length(tok) > 4096 || note_file!(tok, call)
    end
    return
end

# Package sources are reloaded, not reported.
in_workspace_package(path) = any(m -> startswith(path, dirname(something(pathof(m), "/nonexistent/x")) * "/"),
                                 keys(WORKSPACE_PACKAGES))

"""
    report_changed_files()

Prints, once per change, the workspace files that changed on disk since the
call that last named them.
"""
function report_changed_files()
    changed = String[]
    for (path, (stamp, call)) in collect(USED_FILES)
        in_workspace_package(path) && (delete!(USED_FILES, path); continue)
        now = isfile(path) ? file_stamp(path) : nothing
        now == stamp && continue
        push!(changed, "$(relpath(path, WORKSPACE_ROOT[])) ($(now === nothing ? "deleted; " : "")call $call)")
        now === nothing ? delete!(USED_FILES, path) : (USED_FILES[path] = (now, call))
    end
    isempty(changed) && return
    println("[changed on disk since the call that used it: ", join(sort!(changed), ", "),
            ". Values computed from these files before the change are out of date.]")
    return
end

# The binding each name held after the last call, and the call that set it.
# The host keeps the list from each reply, so when the kernel dies it can say
# which bindings were lost instead of only that they were.
const BINDING_SEEN = Dict{Symbol, Tuple{UInt, Int}}()
const BINDING_ORIGINS = Dict{Symbol, Dict{String, Any}}()
const BINDING_IDENTITIES = Dict{Symbol, Any}()
const MAX_LISTED_BINDINGS = 200

function binding_source_digest(call::Int)
    history = get_kernel_state().execution_history
    1 <= call <= length(history) || return nothing
    code = history[call].code
    return isempty(code) ? nothing : bytes2hex(SHA.sha256(code))
end

function definition_observation(value)
    mod = get_kernel_state().eval_module
    T = value isa Type && Base.unwrap_unionall(value) isa DataType ? Base.unwrap_unionall(value) : typeof(value)
    T isa DataType || return nothing
    owner = value isa Function ? parentmodule(value) : parentmodule(T)
    source_owner = Base.moduleroot(owner)
    snap = get(WORKSPACE_PACKAGES, source_owner, nothing)
    if snap !== nothing
        return Dict("kind" => "workspace_sources", "module" => string(owner), "module_identity" => string(objectid(owner)),
            "source_digest" => source_digest(snap), "reload_sequence" => RELOAD_SEQUENCE[])
    end
    name = value isa Function ? string(nameof(value)) : string(nameof(T))
    ancestor = owner
    while ancestor !== mod
        parent = parentmodule(ancestor)
        (parent === ancestor || parent === Main || parent === Core || parent === Base) && return nothing
        name = string(nameof(ancestor))
        ancestor = parent
    end
    i = findlast(e -> name in e["names"], DEFINITION_LOG)
    i === nothing && return nothing
    entry = DEFINITION_LOG[i]
    return Dict("kind" => "definition_recipe", "module" => scrub(string(owner)), "module_identity" => string(objectid(owner)),
        "source_digest" => bytes2hex(SHA.sha256(entry["code"])), "call" => entry["call"])
end

function binding_provenance(name::Symbol; strong::Bool=false)
    mod = get_kernel_state().eval_module
    Base.invokelatest(isdefined, mod, name) || return Dict("name" => string(name), "state" => "not_bound", "epoch" => TASK_EPOCH[])
    value = Base.invokelatest(getglobal, mod, name)
    seen = get(BINDING_SEEN, name, nothing)
    call = seen === nothing ? nothing : seen[2]
    origin = get(BINDING_ORIGINS, name, Dict{String, Any}())
    definition_now = definition_observation(value)
    definition_before = get(origin, "definition", nothing)
    definition_changed = definition_before !== nothing && definition_now !== nothing &&
        definition_before["source_digest"] != definition_now["source_digest"]
    T = typeof(value)
    changed_type = T isa DataType && !(value isa Function) && !current_definition(T)
    sources = binding_sources(origin, something(call, 0))
    pins = pinned_files()
    changed_files = [p for (p, evidence) in sources if world_source_changed(p, evidence, pins; strong) || p in get(origin, "strong_stale_sources", String[])]
    superseded = any(artifact_changed, get(origin, "artifacts", Dict{String, Any}[]))
    restoration = Dict(k => [s for s in get(REVIVAL_OBSERVATION[], k, String[]) if startswith(s, string(name) * " (")] for k in ("restored", "rebuilt", "stale", "lost", "uncertain"))
    return Dict{String, Any}("name" => string(name), "epoch" => TASK_EPOCH[], "type" => short_type(value),
        "first_observed_call" => call, "origin_epoch" => get(origin, "epoch", nothing),
        "source_digest" => get(origin, "source_digest", call === nothing ? nothing : binding_source_digest(call)),
        "definition_at_observation" => definition_before, "definition_current" => definition_now,
        "freshness" => superseded ? "artifact_superseded" : changed_type ? "earlier_type_definition" : !isempty(changed_files) ? "source_files_changed" : definition_changed ? "definition_sources_changed" : "no_observed_staleness",
        "changed_files" => changed_files, "named_files" => sort!(collect(keys(sources))), "source_dependencies" => sources,
        "artifacts" => get(origin, "artifacts", Dict{String, Any}[]), "strong_check" => strong, "revival" => restoration,
        "limits" => "Binding observation is not object creation or causal provenance. Definition recipes and workspace-source digests are source observations; partial reloads need Palette.reloads() evidence and do not establish which methods produced a value. Aliases, in-place mutations and unchanged-value assignments may be invisible. Named files use per-binding statement/RHS observations; in-place mutations and unobserved dynamic reads can be missed. Digests are checked at revival or with strong=true; legacy baselines may lack hashes. No observed staleness does not establish freshness.")
end

function short_type(v)
    v isa Function && return "function"
    v isa Module && return "module"
    v isa Type && return "type"
    T = typeof(v)
    T isa DataType && !current_definition(T) &&
        return "$(nameof(T)), an earlier definition"
    t = scrub(string(T))
    return length(t) > 40 ? string(nameof(T)) : t
end

"""
    binding_list!(mod, call) -> Vector{String}

`name (type, call n)` for each of the session's own bindings, where `n` is
the last call that gave it a new value.
"""
function binding_list!(mod::Module, call::Int)
    rows = String[]
    live = Set{Symbol}()
    source_digest = binding_source_digest(call)
    definitions = IdDict{Any, Any}()
    for sym in sort!(Base.invokelatest(names, mod; all=true))
        (sym === nameof(mod) || sym in KERNEL_BINDINGS || startswith(string(sym), '#')) && continue
        Base.invokelatest(isdefined, mod, sym) || continue
        v = Base.invokelatest(getglobal, mod, sym)
        push!(live, sym)
        id = objectid(v)
        seen = get(BINDING_SEEN, sym, nothing)
        identity_changed = v isa Union{Type, Module} && get(BINDING_IDENTITIES, sym, nothing) !== v
        if seen === nothing || seen[1] != id || identity_changed
            BINDING_SEEN[sym] = (id, call)
            BINDING_ORIGINS[sym] = Dict("epoch" => TASK_EPOCH[], "source_digest" => source_digest)
            key = v isa Union{Type, Function} ? v : typeof(v)
            definition = get!(definitions, key) do
                definition_observation(v)
            end
            definition === nothing || (BINDING_ORIGINS[sym]["definition"] = definition)
        end
        if get(get(BINDING_ORIGINS, sym, Dict()), "observed_call", nothing) == call
            BINDING_ORIGINS[sym]["source_digest"] = source_digest
            definition = get!(definitions, v isa Union{Type, Function} ? v : typeof(v)) do
                definition_observation(v)
            end
            definition === nothing || (BINDING_ORIGINS[sym]["definition"] = definition)
        end
        v isa Union{Type, Module} ? (BINDING_IDENTITIES[sym] = v) : delete!(BINDING_IDENTITIES, sym)
        length(rows) < MAX_LISTED_BINDINGS && push!(rows, "$sym ($(short_type(v)), call $(BINDING_SEEN[sym][2]))")
    end
    filter!(kv -> kv.first in live, BINDING_SEEN)
    filter!(kv -> kv.first in live, BINDING_ORIGINS)
    filter!(kv -> kv.first in live, BINDING_IDENTITIES)
    return rows
end

const OBSERVATION_ACK = Ref{Union{Nothing, Dict{String, Any}}}(nothing)

function world_observation()
    definitions = Dict{String, Any}()
    for entry in DEFINITION_LOG, name in entry["names"]
        definitions[name] = (entry["call"], bytes2hex(SHA.sha256(entry["code"])))
    end
    jobs = Dict(string(name) => (objectid(task), istaskfailed(task) ? "failed" : istaskdone(task) ? "finished" : "running")
        for (task, name) in bound_tasks(get_kernel_state().eval_module))
    return Dict{String, Any}("epoch" => TASK_EPOCH[], "through_call" => length(get_kernel_state().execution_history),
        "bindings" => Dict(string(k) => v for (k, v) in BINDING_SEEN if k !== :ans),
        "definitions" => definitions, "files" => Dict(path => isfile(path) ? file_stamp(path) : nothing for path in keys(USED_FILES)),
        "jobs" => jobs, "reload_sequence" => RELOAD_SEQUENCE[], "revival" => get(REVIVAL_OBSERVATION[], "state", "unknown"))
end

function observation_delta(before::AbstractDict, after::AbstractDict)
    added = sort!(collect(setdiff(keys(after), keys(before))))
    removed = sort!(collect(setdiff(keys(before), keys(after))))
    changed = sort!([name for name in intersect(keys(before), keys(after)) if before[name] != after[name]])
    return Dict("new" => first(added, 20), "removed" => first(removed, 20), "changed" => first(changed, 20),
        "counts" => Dict("new" => length(added), "removed" => length(removed), "changed" => length(changed)))
end

function changes_since_ack(; acknowledge::Bool=false)
    current = world_observation()
    previous = OBSERVATION_ACK[]
    same_epoch = previous !== nothing && previous["epoch"] == current["epoch"]
    result = Dict{String, Any}("epoch" => current["epoch"], "observed_through_call" => current["through_call"],
        "acknowledged_through_call" => previous === nothing ? nothing : previous["through_call"], "acknowledged" => acknowledge,
        "baseline" => previous === nothing ? "not_acknowledged" : same_epoch ? "same_epoch" : "different_epoch",
        "capability_state" => "not_observed_by_kernel", "revival" => current["revival"],
        "reloads_since_ack" => [Dict(k => r[k] for k in ("sequence", "module", "status")) for r in first(
            [r for r in RELOAD_REPORTS if !same_epoch || r["sequence"] > previous["reload_sequence"]], 6)],
        "limits" => "Bindings cover the last completed binding observation, exclude ans, and may miss aliases, in-place mutation and unchanged-value assignments. Definitions are observed source recipes, not a causal method log. Files sample existing named-file mtime/size; jobs sample named tasks. Each delta lists at most 20 names, with full counts. Reload history is bounded to 64 reports. Capability authority is not inferred. Acknowledgment is explicit and does not survive restart.")
    for key in ("bindings", "definitions", "files", "jobs")
        result[key] = observation_delta(same_epoch ? previous[key] : Dict(), current[key])
    end
    acknowledge && (OBSERVATION_ACK[] = current)
    return result
end

"""
    execute_turn(code, timeout_s) -> ((receipt, interrupted, stuck), output)

One persistent turn: report what happened in the background and which used
files changed, reload edited workspace packages, run `code` under the
deadline, and capture everything it printed.
"""
function execute_turn(code::String, timeout_s::Union{Nothing, Float64})
    state = get_kernel_state()
    call = length(state.execution_history) + 1
    # The kernel's warm-up turns empty TURN_COSTS, so the first call they leave
    # is the first one an agent pays for.
    costs = Dict{String, Any}("call" => call, "phase" => "maintenance", "first_call_in_kernel" => isempty(TURN_COSTS),
        "execution_seconds" => nothing, "compile_seconds" => nothing,
        "projection_seconds" => nothing, "serialization_seconds" => nothing, "worker_pipe_seconds" => nothing,
        "limits" => "Execution includes parsing, lowering, import/compilation and bookkeeping. compile_seconds is the part of execution the process spent compiling, in any task. Completed response encoding and worker-pipe write/flush are queryable via Palette.costs(call) on a subsequent call; they exclude runtime/host/MCP delivery. Wall-clock observations include concurrent work.")
    TURN_COSTS[call] = costs
    filter!(kv -> kv.first > call - KEEP_OUTPUTS, TURN_COSTS)
    return capture_output() do
        maintained = time_ns()
        isempty(REVIVAL_REPORT[]) || (println(REVIVAL_REPORT[]); REVIVAL_REPORT[] = "")
        report_background(state.eval_module)
        report_changed_files()
        refresh_workspace_packages!()
        costs["maintenance_seconds"] = (time_ns() - maintained) / 1e9
        costs["phase"] = "execution"
        turn = run_turn(() -> begin
            # Compile timing nests (it counts enables), so user code may still use @time.
            Base.cumulative_compile_timing(true)
            compiled = first(Base.cumulative_compile_time_ns())
            receipt = try
                execute(ExecuteCode(code))
            finally
                costs["compile_seconds"] = (first(Base.cumulative_compile_time_ns()) - compiled) / 1e9
                Base.cumulative_compile_timing(false)
            end
            costs["execution_seconds"] = receipt.duration_ms / 1000
            costs["phase"] = "projection"
            started = time_ns()
            project_turn_result!(receipt)
            costs["projection_seconds"] = (time_ns() - started) / 1e9
            costs["phase"] = "completed"
            receipt.result.success || (costs["failed_phase"] = "execution")
            receipt
        end, timeout_s)
        # An interrupt that execute() caught leaves its failed receipt and lets
        # the closure finish, so the phase it already recorded is the one to keep.
        turn[2] && get!(costs, "failed_phase", costs["phase"])
        refresh_workspace_packages!(reload=false)
        note_files_named!(code, call)
        ok = turn[1] isa OperationReceipt && turn[1].result.success && !turn[2]
        log_definitions!(code, call; failed_in=ok ? nothing : state.eval_module)
        observe_tasks!(state.eval_module, call, code)
        turn
    end
end
