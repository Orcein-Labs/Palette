# Running a command many times, to catch a failure that shows up only
# sometimes: a race, an ordering bug, a flaky test. Each run gets its index in
# STRESS_RUN (and any seed the command derives from it), so a failing run can
# be repeated on its own.

struct StressReport
    cmd::String
    runs::Int
    requested::Int
    outcomes::Dict{Int, Int}              # exit code => count
    first_failure::Union{Nothing, Tuple{Int, Int, String}}   # (run, exit code, output tail)
    seconds::Float64
end

function Base.show(io::IO, ::MIME"text/plain", r::StressReport)
    fails = sum(v for (k, v) in r.outcomes if k != 0; init=0)
    print(io, "stress `", first(r.cmd, 80), "`: ", r.runs, r.runs < r.requested ? " of $(r.requested)" : "",
          " runs in ", round(r.seconds, digits=1), " s, ", fails, " failed")
    isempty(r.outcomes) || print(io, " (exit codes: ", join(("$k×$v" for (k, v) in sort!(collect(r.outcomes))), ", "), ")")
    if r.first_failure !== nothing
        run, code, tail = r.first_failure
        print(io, "\nfirst failure: run $run (STRESS_RUN=$run) exited $code; its output ended with:\n", tail)
    end
end

"""
    stress(cmd; n = 200, jobs = 8, seconds = 45) -> StressReport

Run the shell command `cmd` (a String, run by bash) `n` times, `jobs` at a
time, and report how often each exit code came back, and the first failing
run's index and output. Run `i` sees `STRESS_RUN=i`, so `STRESS_RUN=17 cmd`
repeats it. Stops early after `seconds`, so a call stays within its limit,
and says how many runs it made.
"""
function stress(cmd::AbstractString; n::Integer=200, jobs::Integer=8, seconds::Real=45)
    outcomes = Dict{Int, Int}(); first_failure = Ref{Union{Nothing, Tuple{Int, Int, String}}}(nothing)
    next = Threads.Atomic{Int}(0); done = Ref(0); lock = ReentrantLock()
    started = time()
    worker() = while true
        i = Threads.atomic_add!(next, 1) + 1
        (i > n || time() - started > seconds) && return
        # One file takes both streams. Julia 1.10 stops an IOBuffer taking
        # writes when the first of two pipes into it closes, so a buffer shared
        # by stdout and stderr lost whatever the other stream wrote after that.
        path, io = mktemp()
        try
            p = try
                run(pipeline(ignorestatus(addenv(`bash -c $cmd`, "STRESS_RUN" => string(i))); stdout=io, stderr=io))
            finally
                close(io)
            end
            code = p.exitcode
            Base.@lock lock begin
                outcomes[code] = get(outcomes, code, 0) + 1
                done[] += 1
                if code != 0 && (first_failure[] === nothing || i < first_failure[][1])
                    first_failure[] = (i, code, last(read(path, String), 1500))
                end
            end
        finally
            rm(path; force=true)
        end
    end
    @sync for _ in 1:max(1, jobs)
        @async worker()
    end
    return StressReport(String(cmd), done[], n, outcomes, first_failure[], time() - started)
end
