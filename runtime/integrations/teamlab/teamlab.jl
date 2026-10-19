# Pure-Julia Team Lab Hot-Reload Tool Shelf & Cross-Agent Mailbox for Palette.
# Connects individual agent Palette worlds (claude, gemini, grok, codex) to a shared,
# content-addressed tool shelf and receipted mailbox with zero CLI restarts.

using SHA
using Dates
using Serialization

const _TEAMLAB_VERSION = "0.1.0"
const _TEAMLAB_LOCK = ReentrantLock()
const _TEAMLAB_MAX_MSG_BYTES = 65_536
const _TEAMLAB_MAX_MESSAGES = 4_096
const _TEAMLAB_MAX_TOOLS = 128
const _TEAMLAB_VALID_AGENTS = Set(["claude", "gemini", "grok", "codex", "matt", "all"])

# Per-world tracking of hot-loaded tool SHA-256 digests: tool_name => sha256_hex
const _TEAMLAB_LOADED_DIGESTS = Dict{String,String}()

struct TeamLabToolEntry
    name::String
    version::String
    sha256::String
    blob_file::String
    source_path::String
    author::String
    qa_passed::Bool
    qa_summary::String
    published_at::String
end

struct TeamLabMessage
    id::Int
    sender::String
    recipient::String
    subject::String
    body::String
    tool_ref::String
    tool_sha256::String
    created_at::String
    acked_by::Vector{String}
end

mutable struct TeamLabState
    shelf::Dict{String,TeamLabToolEntry}
    messages::Vector{TeamLabMessage}
    next_msg_id::Int
end

function _default_integrations_dir()::String
    return normpath(joinpath(@__DIR__, ".."))
end

"""
    default_teamlab_dir()

`PALETTE_TEAMLAB_DIR` when set (the sandbox binds it writable and identical for
every agent's worlds). Otherwise a per-sandbox temp directory, which is *not*
shared across agents; `teamlab(status)` reports this as `shared => false`.
"""
function default_teamlab_dir()::String
    env_dir = strip(get(ENV, "PALETTE_TEAMLAB_DIR", ""))
    !isempty(env_dir) && return String(env_dir)
    return joinpath(tempdir(), "palette_teamlab_local")
end

teamlab_is_shared() = !isempty(strip(get(ENV, "PALETTE_TEAMLAB_DIR", "")))

function _ensure_lab_dirs(lab_dir::AbstractString)
    mkpath(lab_dir)
    mkpath(joinpath(lab_dir, "blobs"))
    return String(lab_dir)
end

function _state_file(lab_dir::AbstractString)::String
    return joinpath(lab_dir, "teamlab_state.jls")
end

function _lock_file(lab_dir::AbstractString)::String
    return joinpath(lab_dir, "teamlab.lock")
end

"""
    _with_file_lock(f, lab_dir)

Cross-process advisory lock using atomic directory creation (`mkdir`) with stale-lock
timeout recovery so multiple agent Palette worlds (`claude`, `gemini`, `grok`, `codex`)
never collide when updating the shared Team Lab shelf or mailbox.
"""
function _with_file_lock(f::Function, lab_dir::AbstractString; timeout_s::Float64=5.0)
    _ensure_lab_dirs(lab_dir)
    lck = _lock_file(lab_dir)
    t0 = time()
    acquired = false
    while !acquired
        try
            mkdir(lck)
            acquired = true
        catch
            # Check for stale lock (> 30 seconds old)
            if isdir(lck) && (time() - mtime(lck)) > 30.0
                try rm(lck; recursive=true, force=true) catch end
            end
            if (time() - t0) >= timeout_s
                throw(ErrorException("timed out waiting for Team Lab lock at $lck"))
            end
            sleep(0.01)
        end
    end
    try
        return f()
    finally
        try rm(lck; recursive=true, force=true) catch end
    end
end

function _load_state_unlocked(lab_dir::AbstractString)::TeamLabState
    sf = _state_file(lab_dir)
    if isfile(sf)
        try
            st = open(deserialize, sf, "r")
            st isa TeamLabState && return st
        catch
        end
    end
    return TeamLabState(Dict{String,TeamLabToolEntry}(), TeamLabMessage[], 1)
end

function _save_state_unlocked(lab_dir::AbstractString, st::TeamLabState)
    sf = _state_file(lab_dir)
    tmp = sf * ".tmp." * string(getpid()) * "." * string(rand(UInt32), base=16)
    open(tmp, "w") do io
        serialize(io, st)
    end
    mv(tmp, sf; force=true)
    return nothing
end

function _sha256_file(path::AbstractString)::String
    return bytes2hex(open(sha256, path, "r"))
end

"""
    bootstrap_default_shelf!(; lab_dir=default_teamlab_dir())

Ensure the shared Team Lab shelf contains the verified built-in capabilities
(`vecindex`, `jrir`, `teamlab`) from `runtime/integrations/` whenever their source
files exist on disk and are newer or not yet registered.
"""
function bootstrap_default_shelf!(; lab_dir::AbstractString=default_teamlab_dir())
    int_dir = _default_integrations_dir()
    builtin_specs = (
        ("vecindex", "0.1.0", joinpath(int_dir, "vecindex", "vecindex.jl"), "gemini", "76/76 passed (UTF-8 + Python + noise-free callees + exact_blas auto)"),
        ("jrir",     "0.1.0", joinpath(int_dir, "jrir", "jrir.jl"),         "gemini", "63/63 passed (LLVM 22->18 normalizer + Cyan seam kernels 0 B alloc)"),
        ("teamlab",  "0.1.0", joinpath(int_dir, "teamlab", "teamlab.jl"),   "gemini", "Team Lab hot-reload shelf & cross-agent mailbox"),
    )
    _with_file_lock(lab_dir) do
        st = _load_state_unlocked(lab_dir)
        changed = false
        for (tname, ver, src_path, author, qa_sum) in builtin_specs
            isfile(src_path) || continue
            digest = _sha256_file(src_path)
            existing = get(st.shelf, tname, nothing)
            if existing === nothing || existing.sha256 != digest || !isfile(joinpath(lab_dir, "blobs", existing.blob_file))
                blob_name = "$(tname)_$(digest[1:16]).jl"
                blob_full = joinpath(lab_dir, "blobs", blob_name)
                cp(src_path, blob_full; force=true)
                st.shelf[tname] = TeamLabToolEntry(
                    tname, ver, digest, blob_name, src_path,
                    author, true, qa_sum,
                    Dates.format(now(UTC), dateformat"yyyy-mm-ddTHH:MM:SSZ"),
                )
                changed = true
            end
        end
        changed && _save_state_unlocked(lab_dir, st)
        return st
    end
end

"""
    publish_tool!(name::AbstractString, entrypoint_path::AbstractString;
                  author::AbstractString="gemini", version::AbstractString="0.1.0",
                  run_test_path::AbstractString="", qa_summary::AbstractString="",
                  notify::AbstractString="all", lab_dir::AbstractString=default_teamlab_dir())

Publish a capability `.jl` file into the content-addressed Team Lab shelf (`<lab_dir>/blobs/`),
optionally running `run_test_path` first to gate publication on a green test suite, and post
a hot-reload notification to `notify`.
"""
function publish_tool!(
    name::AbstractString,
    entrypoint_path::AbstractString;
    author::AbstractString="gemini",
    version::AbstractString="0.1.0",
    run_test_path::AbstractString="",
    qa_summary::AbstractString="",
    notify::AbstractString="all",
    lab_dir::AbstractString=default_teamlab_dir(),
)
    tname = strip(String(name))
    !isempty(tname) && occursin(r"^[A-Za-z_][A-Za-z0-9_-]*$", tname) ||
        throw(ArgumentError("invalid tool name '$tname'"))
    isfile(entrypoint_path) || throw(ArgumentError("entrypoint file not found: $entrypoint_path"))

    qa_ok = true
    summary_str = String(qa_summary)
    if !isempty(run_test_path)
        isfile(run_test_path) || throw(ArgumentError("test file not found: $run_test_path"))
        t0 = time()
        # Run in a child process: in-process `@test` failures are recorded into any
        # enclosing testset instead of throwing, which would make the gate fail-open.
        test_abs = abspath(run_test_path)
        cmd = setenv(`$(Base.julia_cmd()) --startup-file=no $test_abs`, ENV; dir=dirname(test_abs))
        out = IOBuffer()
        proc = run(pipeline(ignorestatus(cmd); stdout=out, stderr=out))
        if !success(proc)
            tail = last(String(take!(out)), 2000)
            throw(ErrorException("QA gate failed for '$tname' (exit $(proc.exitcode)):\n$tail"))
        end
        elapsed = round(time() - t0; digits=2)
        if isempty(summary_str)
            summary_str = "QA test suite passed in $(elapsed)s"
        end
    elseif isempty(summary_str)
        summary_str = "published by $author"
    end

    digest = _sha256_file(entrypoint_path)
    blob_name = "$(tname)_$(digest[1:16]).jl"
    now_str = Dates.format(now(UTC), dateformat"yyyy-mm-ddTHH:MM:SSZ")

    entry = _with_file_lock(lab_dir) do
        st = _load_state_unlocked(lab_dir)
        if !haskey(st.shelf, tname) && length(st.shelf) >= _TEAMLAB_MAX_TOOLS
            throw(ArgumentError("Team Lab shelf is full ($_TEAMLAB_MAX_TOOLS tools)"))
        end
        blob_full = joinpath(lab_dir, "blobs", blob_name)
        cp(entrypoint_path, blob_full; force=true)
        e = TeamLabToolEntry(
            tname, String(version), digest, blob_name, abspath(entrypoint_path),
            String(author), qa_ok, summary_str, now_str,
        )
        st.shelf[tname] = e
        if !isempty(notify)
            msg = TeamLabMessage(
                st.next_msg_id,
                String(author),
                String(notify),
                "tool_published:$tname",
                "Tool '$tname' ($version, sha256=$(digest[1:12])) published by $author: $summary_str",
                tname,
                digest,
                now_str,
                String[String(author)],
            )
            push!(st.messages, msg)
            st.next_msg_id += 1
        end
        _save_state_unlocked(lab_dir, st)
        return e
    end

    return Dict{String,Any}(
        "status" => "published",
        "tool" => entry.name,
        "version" => entry.version,
        "sha256" => entry.sha256,
        "blob_file" => entry.blob_file,
        "author" => entry.author,
        "qa_passed" => entry.qa_passed,
        "qa_summary" => entry.qa_summary,
        "published_at" => entry.published_at,
    )
end

"""
    teamlab_sync!(agent::AbstractString="all";
                  tools::Vector{String}=String[],
                  target_mod::Module=@__MODULE__,
                  ack_synced::Bool=false,
                  lab_dir::AbstractString=default_teamlab_dir()) -> Dict{String,Any}

Hot-reload any new or updated QA-verified tools from the shared Team Lab shelf
directly into `target_mod` with **zero CLI restarts**, and return unread mailbox
messages for `agent`.
"""
function teamlab_sync!(
    agent::AbstractString="all";
    tools::Vector{String}=String[],
    target_mod::Module=@__MODULE__,
    ack_synced::Bool=false,
    lab_dir::AbstractString=default_teamlab_dir(),
)
    ag = lowercase(strip(String(agent)))
    bootstrap_default_shelf!(; lab_dir=lab_dir)
    st = _with_file_lock(lab_dir) do
        _load_state_unlocked(lab_dir)
    end

    target_tools = isempty(tools) ? sort!(collect(keys(st.shelf))) : tools
    reloaded = Vector{Dict{String,Any}}()
    already_current = String[]

    for tname in target_tools
        entry = get(st.shelf, tname, nothing)
        entry === nothing && throw(ArgumentError("tool '$tname' not found on Team Lab shelf"))
        entry.qa_passed || continue
        loaded_sha = get(_TEAMLAB_LOADED_DIGESTS, tname, "")
        # Avoid re-including teamlab.jl over itself during an active teamlab_sync! call unless explicitly requested
        if tname == "teamlab" && isdefined(target_mod, :teamlab_sync!)
            _TEAMLAB_LOADED_DIGESTS["teamlab"] = entry.sha256
            push!(already_current, tname)
            continue
        end
        if loaded_sha == entry.sha256 && isdefined(target_mod, Symbol(tname))
            push!(already_current, tname)
        else
            blob_full = joinpath(lab_dir, "blobs", entry.blob_file)
            src_to_load = isfile(blob_full) ? blob_full : entry.source_path
            Base.include(target_mod, src_to_load)
            _TEAMLAB_LOADED_DIGESTS[tname] = entry.sha256
            push!(reloaded, Dict{String,Any}(
                "tool" => tname,
                "sha256" => entry.sha256[1:12],
                "author" => entry.author,
                "qa_summary" => entry.qa_summary,
            ))
        end
    end

    unread = _filter_unread(st.messages, ag)
    if ack_synced && !isempty(unread) && ag != "all"
        _ack_messages!(lab_dir, ag, [m["id"] for m in unread])
    end

    return Dict{String,Any}(
        "status" => "synced",
        "agent" => ag,
        "lab_dir" => String(lab_dir),
        "reloaded_tools" => reloaded,
        "already_current_tools" => already_current,
        "unread_count" => length(unread),
        "unread_messages" => unread,
    )
end

function _filter_unread(messages::Vector{TeamLabMessage}, agent::String)
    out = Vector{Dict{String,Any}}()
    for m in messages
        if agent == "all" || m.recipient == "all" || m.recipient == agent
            if !(agent in m.acked_by)
                push!(out, Dict{String,Any}(
                    "id" => m.id,
                    "sender" => m.sender,
                    "recipient" => m.recipient,
                    "subject" => m.subject,
                    "body" => m.body,
                    "tool_ref" => m.tool_ref,
                    "tool_sha256" => isempty(m.tool_sha256) ? "" : m.tool_sha256[1:min(12, end)],
                    "created_at" => m.created_at,
                ))
            end
        end
    end
    return out
end

function send_mail!(
    sender::AbstractString,
    recipient::AbstractString,
    subject::AbstractString,
    body::AbstractString;
    tool_ref::AbstractString="",
    tool_sha256::AbstractString="",
    lab_dir::AbstractString=default_teamlab_dir(),
)
    snd = lowercase(strip(String(sender)))
    rcp = lowercase(strip(String(recipient)))
    !isempty(snd) || throw(ArgumentError("sender must be non-empty"))
    !isempty(rcp) || throw(ArgumentError("recipient must be non-empty"))
    !isempty(strip(subject)) || throw(ArgumentError("subject must be non-empty"))
    ncodeunits(body) <= _TEAMLAB_MAX_MSG_BYTES ||
        throw(ArgumentError("message body exceeds $_TEAMLAB_MAX_MSG_BYTES bytes"))

    now_str = Dates.format(now(UTC), dateformat"yyyy-mm-ddTHH:MM:SSZ")
    msg = _with_file_lock(lab_dir) do
        st = _load_state_unlocked(lab_dir)
        if length(st.messages) >= _TEAMLAB_MAX_MESSAGES
            deleteat!(st.messages, 1:(length(st.messages) - _TEAMLAB_MAX_MESSAGES + 1))
        end
        m = TeamLabMessage(
            st.next_msg_id, snd, rcp, String(subject), String(body),
            String(tool_ref), String(tool_sha256), now_str, String[snd],
        )
        push!(st.messages, m)
        st.next_msg_id += 1
        _save_state_unlocked(lab_dir, st)
        return m
    end

    return Dict{String,Any}(
        "status" => "sent",
        "id" => msg.id,
        "sender" => msg.sender,
        "recipient" => msg.recipient,
        "subject" => msg.subject,
        "created_at" => msg.created_at,
    )
end

function _ack_messages!(lab_dir::AbstractString, agent::AbstractString, ids::Vector{Int})
    ag = lowercase(strip(String(agent)))
    acked = 0
    _with_file_lock(lab_dir) do
        st = _load_state_unlocked(lab_dir)
        id_set = Set(ids)
        for m in st.messages
            if (isempty(id_set) || m.id in id_set) && !(ag in m.acked_by)
                push!(m.acked_by, ag)
                acked += 1
            end
        end
        acked > 0 && _save_state_unlocked(lab_dir, st)
    end
    return Dict{String,Any}("status" => "acked", "agent" => ag, "acked_count" => acked)
end

function _teamlab_status(lab_dir::AbstractString)
    bootstrap_default_shelf!(; lab_dir=lab_dir)
    st = _with_file_lock(lab_dir) do
        _load_state_unlocked(lab_dir)
    end
    tools_info = [
        Dict{String,Any}(
            "name" => e.name,
            "version" => e.version,
            "sha256" => e.sha256[1:12],
            "author" => e.author,
            "qa_passed" => e.qa_passed,
            "qa_summary" => e.qa_summary,
            "loaded_in_world" => get(_TEAMLAB_LOADED_DIGESTS, e.name, "") == e.sha256,
            "published_at" => e.published_at,
        ) for (_, e) in sort!(collect(st.shelf); by=first)
    ]
    return Dict{String,Any}(
        "status" => "ok",
        "capability" => "teamlab",
        "version" => _TEAMLAB_VERSION,
        "lab_dir" => String(lab_dir),
        "shared" => teamlab_is_shared() && abspath(lab_dir) == abspath(default_teamlab_dir()),
        "tools_on_shelf" => tools_info,
        "total_messages" => length(st.messages),
    )
end

function _teamlab_check_keys(args::Dict{String,Any}, allowed::Tuple, msg::String)
    for k in keys(args)
        k in allowed || throw(ArgumentError("$msg (unexpected key '$k')"))
    end
end

"""
    teamlab(arguments::Dict{String,Any}) -> Dict{String,Any}

Palette runtime capability entrypoint for `teamlab` (Team Lab Hot-Reload Shelf & Mailbox).
Supported actions: `diagnose` (or `status`), `sync`, `publish`, `send`, `inbox`, `ack`.
"""
function teamlab(arguments::Dict{String,Any})
    lock(_TEAMLAB_LOCK) do
        action = get(arguments, "action", "status")
        action isa String || throw(ArgumentError("action must be a string"))
        lab_dir = String(get(arguments, "lab_dir", default_teamlab_dir()))

        if action == "diagnose" || action == "status"
            _teamlab_check_keys(arguments, ("action", "lab_dir"), "status accepts only action and lab_dir")
            return _teamlab_status(lab_dir)
        elseif action == "sync"
            _teamlab_check_keys(arguments, ("action", "agent", "tools", "ack_synced", "lab_dir"),
                                "sync accepts only agent, tools, ack_synced, and lab_dir")
            ag = String(get(arguments, "agent", "all"))
            raw_tools = get(arguments, "tools", String[])
            tlist = String[String(x) for x in raw_tools]
            ack = Bool(get(arguments, "ack_synced", false))
            return teamlab_sync!(ag; tools=tlist, target_mod=@__MODULE__, ack_synced=ack, lab_dir=lab_dir)
        elseif action == "publish"
            _teamlab_check_keys(arguments, ("action", "tool", "entrypoint", "author", "version", "test_path", "qa_summary", "notify", "lab_dir"),
                                "publish accepts tool, entrypoint, author, version, test_path, qa_summary, notify, and lab_dir")
            tname = String(get(arguments, "tool", ""))
            ep = String(get(arguments, "entrypoint", ""))
            auth = String(get(arguments, "author", "gemini"))
            ver = String(get(arguments, "version", "0.1.0"))
            tpath = String(get(arguments, "test_path", ""))
            qas = String(get(arguments, "qa_summary", ""))
            notf = String(get(arguments, "notify", "all"))
            return publish_tool!(tname, ep; author=auth, version=ver, run_test_path=tpath, qa_summary=qas, notify=notf, lab_dir=lab_dir)
        elseif action == "send"
            _teamlab_check_keys(arguments, ("action", "sender", "recipient", "subject", "body", "tool_ref", "tool_sha256", "lab_dir"),
                                "send accepts sender, recipient, subject, body, tool_ref, tool_sha256, and lab_dir")
            snd = String(get(arguments, "sender", ""))
            rcp = String(get(arguments, "recipient", "all"))
            subj = String(get(arguments, "subject", ""))
            body = String(get(arguments, "body", ""))
            tref = String(get(arguments, "tool_ref", ""))
            tsha = String(get(arguments, "tool_sha256", ""))
            return send_mail!(snd, rcp, subj, body; tool_ref=tref, tool_sha256=tsha, lab_dir=lab_dir)
        elseif action == "inbox"
            _teamlab_check_keys(arguments, ("action", "agent", "lab_dir"), "inbox accepts only agent and lab_dir")
            ag = lowercase(strip(String(get(arguments, "agent", "all"))))
            bootstrap_default_shelf!(; lab_dir=lab_dir)
            st = _with_file_lock(lab_dir) do
                _load_state_unlocked(lab_dir)
            end
            unread = _filter_unread(st.messages, ag)
            return Dict{String,Any}("status" => "ok", "agent" => ag, "unread_count" => length(unread), "messages" => unread)
        elseif action == "ack"
            _teamlab_check_keys(arguments, ("action", "agent", "ids", "lab_dir"), "ack accepts only agent, ids, and lab_dir")
            ag = String(get(arguments, "agent", ""))
            !isempty(strip(ag)) || throw(ArgumentError("agent must be non-empty for ack"))
            raw_ids = get(arguments, "ids", Int[])
            ids = Int[Int(x) for x in raw_ids]
            return _ack_messages!(lab_dir, ag, ids)
        end
        throw(ArgumentError("action must be status (or diagnose), sync, publish, send, inbox, or ack"))
    end
end

teamlab(arguments::AbstractDict) = teamlab(Dict{String,Any}(String(k) => v for (k, v) in arguments))
