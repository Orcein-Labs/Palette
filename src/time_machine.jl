# Time Machine: named checkpoints of the live world, restored or forked.
#
# A checkpoint is the end-of-call snapshot (see revival.jl), hard-linked into
# STATE_DIR/checkpoints/<name>/, plus a copy of each included file's source in
# STATE_DIR/blobs/<sha256>. Snapshot files are never changed after they are
# written, so linking them is safe. `checkpoint.json` records the identities a
# restore is checked against, and the directory appears whole or not at all.
#
# A restore is asked for by the host (palette_control action=restore), which
# writes STATE_DIR/restore.json and starts a new kernel. Before revival, that
# kernel saves the world it is leaving as an automatic checkpoint, swaps the
# checkpoint's snapshot in, and pins its includes to the copies, so the world
# runs the checkpoint's code whatever the workspace files say now. Revival
# then rebuilds it as usual and the report says, per identity, whether it is
# the same. Workspace files are never rewritten: git owns files.

const TIME_MACHINE_FORMAT = 1
const CHECKPOINT_NAME = r"^[A-Za-z0-9._-]{1,64}$"   # as CHECKPOINT_NAME in time_machine.py
const MAX_WORLD_HISTORY = 200
# Automatic checkpoints a restore keeps: the oldest beyond this are removed. At
# least 1, so the restore's own undo checkpoint survives.
max_auto_checkpoints() = max(1, something(tryparse(Int, get(ENV, "PALETTE_AUTO_CHECKPOINTS", "")), 5))
# (name, note, call) of the checkpoint the current call asked for.
const PENDING_CHECKPOINT = Ref{Union{Nothing, Tuple{String, String, Int}}}(nothing)
# What the next call is told about the checkpoint the last one asked for.
const TIME_MACHINE_NOTICE = Ref("")
# The restore this kernel started with: set before revival, read when it ends.
const RESTORE_INFO = Ref{Union{Nothing, Dict{String, Any}}}(nothing)

checkpoints_dir() = joinpath(STATE_DIR[], "checkpoints")
blob_path(sha::AbstractString) = joinpath(STATE_DIR[], "blobs", sha)
utc_now() = Dates.format(Dates.now(Dates.UTC), "yyyy-mm-ddTHH:MM:SSZ")

# The code a restored world runs: file => sha256 of the checkpoint copy it
# runs, for each file a pinned include brought in. The latest include of a
# file decides, so including it again unpins it and the files it includes.
function pinned_files(defs=DEFINITION_LOG)
    pins = Dict{String, String}()
    brought = Dict{String, Vector{String}}()   # include path => the files its pin covers
    for e in defs
        e["kind"] == "include" || continue
        p = e["path"]
        foreach(f -> delete!(pins, f), get(brought, p, [p]))
        pinned = get(e, "pinned", nothing)
        pinned isa AbstractDict || (delete!(brought, p); continue)
        merge!(pins, pinned)
        brought[p] = collect(String, keys(pinned))
    end
    return pins
end

# Whether a value's source changed, against the code the world runs: a
# checkpoint copy when the file is pinned, else the workspace file.
function world_source_changed(p, evidence, pins::AbstractDict; strong=false)
    haskey(pins, p) || return source_changed(evidence; strong)
    return get(evidence, "sha256", nothing) != pins[p]
end
short_digest(x) = first(bytes2hex(sha256(JSON.json(x))), 16)

function link_or_copy(src::String, dst::String)
    rm(dst; force=true)
    try
        hardlink(src, dst)
    catch
        cp(src, dst)
    end
    return dst
end

"""
    Palette.checkpoint(name; note = "")

Names the world as it is when this call completes. `Palette.checkpoints()`
lists checkpoints; `palette_control` with action `restore` and this name brings
the world back to it, and `fork: true` restores it as a new world whose parent
is this checkpoint. The next call's output confirms the checkpoint was saved.
"""
function checkpoint(name::AbstractString; note::AbstractString="")
    isempty(STATE_DIR[]) && error("this kernel has no state directory, so it cannot keep checkpoints")
    occursin(CHECKPOINT_NAME, name) || error("a checkpoint name is 1–64 letters, digits, `.`, `_` or `-`, not $(repr(name))")
    startswith(name, "auto-") && error("names starting with `auto-` are kept for the checkpoints a restore saves")
    isdir(joinpath(checkpoints_dir(), name)) &&
        error("checkpoint `$name` exists; Palette.forget_checkpoint(\"$name\") removes it")
    call = next_call()
    PENDING_CHECKPOINT[] = (String(name), String(note), call)
    return "checkpoint `$name` is saved when this call (call $call) completes"
end

"""
    save_end_of_call!(call; waiting)

Saves the state `call` ended with (`nothing`: an interrupted call, which saves
nothing), and the checkpoint the call asked for, which does not give way to a
waiting request.
"""
function save_end_of_call!(call::Union{Nothing, Int}; @nospecialize(waiting::Function = () -> false))
    pending = PENDING_CHECKPOINT[]
    PENDING_CHECKPOINT[] = nothing
    if pending === nothing
        call === nothing || snapshot_state!(call; waiting)
        return nothing
    end
    name, note, asked = pending
    if call === nothing || call != asked
        TIME_MACHINE_NOTICE[] = "[time machine] Checkpoint `$name` was not saved: call $asked did not complete."
        return nothing
    end
    TIME_MACHINE_NOTICE[] = try
        manifest = snapshot_state!(call; waiting=() -> false)
        manifest === nothing && error("the snapshot was not written")
        record = save_checkpoint!(name, note, manifest)
        kept = count(i -> i["kept"], record["includes"])
        "[time machine] Saved checkpoint `$name` (the world at the end of call $call: $(length(record["definitions"])) definitions, " *
        "$(length(record["data"])) data groups, $kept of $(length(record["includes"])) included files copied)."
    catch e
        rm(joinpath(checkpoints_dir(), name * ".partial"); force=true, recursive=true)
        "[time machine] Checkpoint `$name` was not saved: $(first(sprint(showerror, e), 300))"
    end
    return nothing
end

# The files an include brings in: the file and its nested includes, as on disk.
function included_files(path::String, depth::Int=0)
    (depth > 8 || !isfile(path)) && return String[]
    out = [path]
    ex = try Meta.parseall(read(path, String); filename=path) catch; return out end
    for st in top_statements(ex)
        definition_kind(st) == "include" && append!(out, included_files(abspath(dirname(path), st.args[2]), depth + 1))
    end
    return unique(out)
end

# The sources an include of `p` read, copied to the blob store as file =>
# sha256, or nothing when `p` changed since the include read it.
function keep_sources(p::String, sha::String)
    (isfile(p) && file_sha(p) == sha) || return nothing
    d = Dict{String, Any}()
    for f in included_files(p)
        bytes = read(f)
        h = bytes2hex(sha256(bytes))
        isfile(blob_path(h)) || write_state_file!(joinpath(STATE_DIR[], "blobs"), h, String(bytes))
        d[f] = h
    end
    return d
end

# Copies each include's sources to the blob store, when the file still has the
# content the include read; a file changed since cannot be copied as it was.
function keep_include_sources!(m)
    mkpath(joinpath(STATE_DIR[], "blobs"))
    out = Dict{String, Any}[]
    kept = Dict{Tuple{String, String}, Any}()   # a file included in many calls is copied once
    for e in m["definitions"]
        e["kind"] == "include" || continue
        p = e["path"]
        pinned = get(e, "pinned", nothing)
        files = if pinned isa AbstractDict
            # Already running a checkpoint's copy: that copy is the source.
            all(sha -> isfile(blob_path(sha)), values(pinned)) ? Dict{String, Any}(pinned) : nothing
        else
            get!(() -> keep_sources(p, e["sha256"]), kept, (p, e["sha256"]))
        end
        push!(out, Dict{String, Any}("path" => p, "sha256" => e["sha256"], "kept" => files !== nothing,
                                     "files" => something(files, Dict{String, Any}())))
    end
    return out
end

"""
    save_checkpoint!(name, note, manifest) -> Dict

Links the snapshot `manifest` describes into a checkpoint called `name`, and
returns its record.
"""
function save_checkpoint!(name::String, note::String, m::AbstractDict)
    dir = STATE_DIR[]
    final = joinpath(checkpoints_dir(), name)
    isdir(final) && error("checkpoint `$name` exists")
    tmp = final * ".partial"
    rm(tmp; force=true, recursive=true)
    mkpath(tmp)
    link_or_copy(joinpath(dir, "manifest.json"), joinpath(tmp, "manifest.json"))
    for g in m["data"]
        link_or_copy(joinpath(dir, g["file"]), joinpath(tmp, g["file"]))
    end
    w = world_record()
    record = Dict{String, Any}(
        "format" => TIME_MACHINE_FORMAT, "name" => name, "note" => note, "world" => w["id"], "call" => m["call"],
        "created" => utc_now(), "manifest_sha256" => file_sha(joinpath(tmp, "manifest.json")),
        "definitions" => [Dict{String, Any}("name" => b["name"], "digest" => short_digest(b["check"]))
                          for b in m["bindings"] if b["class"] == "definition"],
        "includes" => keep_include_sources!(m),
        "data" => [Dict{String, Any}("names" => g["names"], "file" => g["file"], "sha256" => g["sha256"]) for g in m["data"]])
    write(joinpath(tmp, "checkpoint.json"), JSON.json(record))   # last: a checkpoint without it is incomplete
    mv(tmp, final)
    note_world_event!(Dict{String, Any}("event" => "checkpoint", "time" => record["created"], "checkpoint" => name, "call" => m["call"]); world=w)
    return record
end

# --- world identity ------------------------------------------------------

world_path() = joinpath(STATE_DIR[], "world.json")

# This state directory's world: its id, the checkpoint it was forked from, and
# what happened to it. Made on first use.
function world_record()
    path = world_path()
    isfile(path) && return try read_json(path) catch; new_world(nothing) end
    w = new_world(nothing)
    write_state_file!(STATE_DIR[], "world.json", JSON.json(w))
    return w
end

new_world(parent) = Dict{String, Any}("id" => string(uuid4()), "parent" => parent, "created" => utc_now(), "history" => Any[])

function note_world_event!(event::AbstractDict; world=world_record())
    push!(world["history"], event)
    length(world["history"]) > MAX_WORLD_HISTORY && deleteat!(world["history"], 1:length(world["history"]) - MAX_WORLD_HISTORY)
    write_state_file!(STATE_DIR[], "world.json", JSON.json(world))
    return world
end

# --- listing and forgetting ----------------------------------------------

function checkpoint_records(dir::String=STATE_DIR[])
    root = joinpath(dir, "checkpoints")
    isdir(root) || return Dict{String, Any}[]
    records = [read_json(joinpath(root, n, "checkpoint.json")) for n in readdir(root)
               if isfile(joinpath(root, n, "checkpoint.json"))]
    return sort!(records; by=r -> (r["created"], r["name"]))
end

"""
    Palette.checkpoints() -> (; world, checkpoints)

This world's id, the checkpoint it was forked from, and its history of
checkpoints and restores, with every checkpoint kept: name, note, call, world
and time.
"""
function checkpoints()
    isempty(STATE_DIR[]) && error("this kernel has no state directory, so it keeps no checkpoints")
    # Dicts, like the world record and the host's listing.
    list = [Dict{String, Any}(k => r[k] for k in ("name", "call", "created", "note", "world")) for r in checkpoint_records()]
    return (world=world_record(), checkpoints=list)
end

"""
    Palette.forget_checkpoint(name)

Removes checkpoint `name`, and each copied source no checkpoint or the current
world uses any more.
"""
function forget_checkpoint(name::AbstractString)
    occursin(CHECKPOINT_NAME, name) || error("no checkpoint is named $(repr(name))")
    dir = joinpath(checkpoints_dir(), name)
    isdir(dir) || error("no checkpoint is named `$name`")
    rm(dir; recursive=true)
    removed = remove_unused_blobs!(DEFINITION_LOG)
    return "checkpoint `$name` removed, and $removed copied source$(removed == 1 ? "" : "s") no longer used"
end

# Removes each copied source that no checkpoint and none of `definitions` pins;
# returns how many.
function remove_unused_blobs!(definitions)
    used = Set{String}(sha for r in checkpoint_records() for i in r["includes"] for sha in values(i["files"]))
    for e in definitions
        pinned = get(e, "pinned", nothing)
        pinned isa AbstractDict && union!(used, values(pinned))
    end
    blobs = joinpath(STATE_DIR[], "blobs")
    removed = 0
    if isdir(blobs)
        for f in readdir(blobs)
            f in used || (rm(joinpath(blobs, f); force=true); removed += 1)
        end
    end
    return removed
end

# Removes the automatic checkpoints beyond the newest max_auto_checkpoints(),
# never `restored`, with the copies only they used, and any checkpoint a
# crash left half-written. Returns the names removed.
function prune_auto_checkpoints!(restored::String, definitions)
    root = checkpoints_dir()
    isdir(root) || return String[]
    foreach(n -> rm(joinpath(root, n); recursive=true), filter(n -> endswith(n, ".partial"), readdir(root)))
    autos = [r["name"] for r in checkpoint_records() if startswith(r["name"], "auto-") && r["name"] != restored]
    old = autos[1:max(0, length(autos) - max_auto_checkpoints())]
    isempty(old) && return old
    foreach(n -> rm(joinpath(root, n); recursive=true), old)
    remove_unused_blobs!(definitions)
    return old
end

# --- restore -------------------------------------------------------------

"""
    apply_pending_restore!()

Run by a starting kernel before revival. When the host asked for a restore,
saves the current snapshot as `auto-<time>-before-<name>` (`-before-undo`
when it restores an automatic one), swaps the
checkpoint's snapshot in, and records the restore in RESTORE_INFO; revival
then rebuilds it. Only the newest `PALETTE_AUTO_CHECKPOINTS` (default 5)
automatic checkpoints are kept. On any failure the current snapshot stays as it was, and
RESTORE_INFO says why.
"""
function apply_pending_restore!()
    dir = STATE_DIR[]
    marker = joinpath(dir, "restore.json")
    (isempty(dir) || !isfile(marker)) && return nothing
    req = try read_json(marker) catch e; Dict{String, Any}("checkpoint" => "?", "unreadable" => sprint(showerror, e)) end
    rm(marker; force=true)   # a restore that fails is not tried again by every later kernel
    name = string(get(req, "checkpoint", "?"))
    fork = get(req, "fork", false) === true
    RESTORE_INFO[] = try
        haskey(req, "unreadable") && error("the restore request could not be read: $(req["unreadable"])")
        swap_in_checkpoint!(name, fork)
    catch e
        info = Dict{String, Any}("checkpoint" => name, "fork" => fork, "error" => first(sprint(showerror, e), 400))
        try note_world_event!(Dict{String, Any}("event" => "restore_failed", "time" => utc_now(), "checkpoint" => name,
                                                "error" => info["error"])) catch end
        info
    end
    return nothing
end

# Checkpoint `name`'s record, once every file it names has the content it
# recorded; an error otherwise.
function verify_checkpoint(name::String)
    occursin(CHECKPOINT_NAME, name) || error("no checkpoint is named $(repr(name))")
    cdir = joinpath(checkpoints_dir(), name)
    isfile(joinpath(cdir, "checkpoint.json")) || error("no checkpoint is named `$name`")
    record = read_json(joinpath(cdir, "checkpoint.json"))
    file_sha(joinpath(cdir, "manifest.json")) == record["manifest_sha256"] || error("checkpoint `$name` is damaged: its manifest changed")
    for g in record["data"]
        f = joinpath(cdir, g["file"])
        (isfile(f) && file_sha(f) == g["sha256"]) || error("checkpoint `$name` is damaged: data file $(g["file"]) is missing or changed")
    end
    for i in record["includes"], (f, sha) in i["files"]
        (isfile(blob_path(sha)) && file_sha(blob_path(sha)) == sha) || error("checkpoint `$name` is damaged: the copy of $(f) is missing or changed")
    end
    return record
end

function swap_in_checkpoint!(name::String, fork::Bool)
    dir = STATE_DIR[]
    record = verify_checkpoint(name)   # before anything changes
    cdir = joinpath(checkpoints_dir(), name)
    # The world being left, so the restore can be undone.
    saved = nothing
    if isfile(joinpath(dir, "manifest.json"))
        current = read_json(joinpath(dir, "manifest.json"))
        if get(current, "workspace", nothing) == WORKSPACE_ROOT[]
            # Restoring an automatic checkpoint is an undo; naming it keeps names from nesting.
            target = startswith(name, "auto-") ? "undo" : first(name, 32)
            saved = "auto-" * Dates.format(Dates.now(Dates.UTC), "yyyymmdd-HHMMSS") * "-before-" * target
            isdir(joinpath(checkpoints_dir(), saved)) && (saved *= "-" * string(rand(UInt16); base=16))
            save_checkpoint!(saved, "the world as it was before restoring `$name`", current)
        end
    end
    m = read_json(joinpath(cdir, "manifest.json"))
    # record["includes"] lists the manifest's includes in order; pin each to its own copy.
    for (e, i) in zip((e for e in m["definitions"] if e["kind"] == "include"), record["includes"])
        e["path"] == i["path"] || error("checkpoint `$name` is damaged: its includes do not match its manifest")
        i["kept"] && (e["pinned"] = i["files"])
    end
    keep = Set(String(g["file"]) for g in m["data"])
    for g in m["data"]
        link_or_copy(joinpath(cdir, g["file"]), joinpath(dir, g["file"]))
    end
    write_state_file!(dir, "manifest.json", JSON.json(m))   # the commit point
    rm(joinpath(dir, "started_call"); force=true)
    for f in readdir(dir)
        startswith(f, "data-") && !(f in keep) && rm(joinpath(dir, f); force=true)
    end
    # Clean-up only: the restore has already happened and must not fail on it.
    pruned = try prune_auto_checkpoints!(name, m["definitions"]) catch; String[] end
    old = world_record()
    if fork
        w = new_world(Dict{String, Any}("world" => old["id"], "checkpoint" => name, "call" => record["call"]))
        w["history"] = old["history"]
        write_state_file!(dir, "world.json", JSON.json(w))
    end
    return Dict{String, Any}("checkpoint" => name, "fork" => fork, "call" => record["call"], "record" => record,
                             "saved_before_restore" => saved, "pruned" => pruned, "previous_world" => old["id"],
                             "world" => world_record()["id"])
end

# The checkpoint's source texts by path, or why they cannot be read.
function pinned_sources(pinned::AbstractDict)
    out = Dict{String, String}()
    for (f, sha) in pinned
        b = blob_path(sha)
        isfile(b) || return "the checkpoint's copy of $(relpath(f, WORKSPACE_ROOT[])) is gone"
        bytes = read(b)
        bytes2hex(sha256(bytes)) == sha || return "the checkpoint's copy of $(relpath(f, WORKSPACE_ROOT[])) is damaged"
        out[f] = String(bytes)
    end
    return out
end

"""
    restore_receipt!(same, envthen, envnow) -> (receipt, lines)

Run at the end of a revival that a restore started: each identity the
checkpoint recorded, and whether the revived world has it, appended to the
world's history. `same` holds the bindings revival found the same as when
they were saved. Returns the receipt and the report's lines.
"""
function restore_receipt!(same::Set{String}, envthen, envnow)
    info = RESTORE_INFO[]
    record = info["record"]
    env_same = envthen["julia"] == envnow["julia"] && envthen["manifest"] == envnow["manifest"]
    defs = [Dict{String, Any}("name" => d["name"], "digest" => d["digest"], "verdict" => d["name"] in same ? "same" : "differs")
            for d in record["definitions"]]
    incs = Dict{String, Any}[]
    # The latest include of a path is the code the world runs for it (see
    # pinned_files); an earlier one it superseded is not an identity.
    latest = Dict(i["path"] => i for i in record["includes"])
    for i in record["includes"]
        p = i["path"]
        latest[p] === i || continue
        push!(incs, Dict{String, Any}("path" => relpath(p, WORKSPACE_ROOT[]), "sha256" => i["sha256"],
            "replayed_from" => i["kept"] ? "checkpoint copy" : "workspace",
            "workspace" => isfile(p) ? (file_sha(p) == i["sha256"] ? "same" : "differs") : "gone"))
    end
    data = [Dict{String, Any}("names" => g["names"], "sha256" => g["sha256"],
                              "verdict" => all(n -> n in same, g["names"]) ? "same" : "differs") for g in record["data"]]
    identical = env_same && all(d -> d["verdict"] == "same", defs) && all(d -> d["verdict"] == "same", data) &&
                all(i -> i["replayed_from"] == "checkpoint copy" || i["workspace"] == "same", incs)
    receipt = Dict{String, Any}("event" => info["fork"] ? "fork" : "restore", "time" => utc_now(),
        "checkpoint" => info["checkpoint"], "call" => info["call"], "world" => info["world"],
        "previous_world" => info["previous_world"], "saved_before_restore" => info["saved_before_restore"],
        "identical" => identical,
        "environment" => Dict{String, Any}("julia" => envnow["julia"], "manifest" => envnow["manifest"],
                                           "verdict" => env_same ? "same" : "differs"),
        "definitions" => defs, "includes" => incs, "data" => data)
    note_world_event!(receipt)
    count_same(xs) = count(x -> x["verdict"] == "same", xs)
    differs = [i["path"] for i in incs if i["workspace"] != "same"]
    lines = ["[time machine] $(info["fork"] ? "Forked a new world from" : "Restored") checkpoint `$(info["checkpoint"])` " *
             "(the world at the end of call $(info["call"])). " *
             (identical ? "Every identity it recorded is the same." : "Not every identity it recorded is the same; Palette.revival()[\"checkpoint\"] lists each.")]
    push!(lines, "  identities: environment $(env_same ? "same" : "differs"); definitions $(count_same(defs)) of $(length(defs)) same; " *
                 "data groups $(count_same(data)) of $(length(data)) same; included files $(length(incs)), " *
                 "$(count(i -> i["replayed_from"] == "checkpoint copy", incs)) replayed from the checkpoint's copies.")
    isempty(differs) || push!(lines, "  The world runs the checkpoint's code; these workspace files differ from it (git owns files, " *
                                     "so they were not changed): $(join(differs, ", ")). include() one again to use the workspace version.")
    info["saved_before_restore"] === nothing ||
        push!(lines, "  The world it replaced is checkpoint `$(info["saved_before_restore"])`; restoring that undoes this.")
    pruned = get(info, "pruned", String[])
    isempty(pruned) || push!(lines, "  Removed the oldest automatic checkpoint$(length(pruned) == 1 ? "" : "s") " *
                                    "($(join(pruned, ", "))); the newest $(max_auto_checkpoints()) are kept.")
    return receipt, lines
end

# The receipt as the first reply after a restore carries it.
function checkpoint_summary(r::AbstractDict)
    haskey(r, "error") && return r
    differs(xs) = [get(x, "name", get(x, "path", nothing)) for x in xs if get(x, "verdict", "same") != "same" ||
                   (haskey(x, "workspace") && x["workspace"] != "same" && x["replayed_from"] != "checkpoint copy")]
    out = Dict{String, Any}(k => r[k] for k in ("event", "checkpoint", "call", "world", "previous_world",
                                                 "saved_before_restore", "identical"))
    out["environment"] = r["environment"]["verdict"]
    out["differs"] = [differs(r["definitions"]); differs(r["includes"]); [join(d["names"], ",") for d in r["data"] if d["verdict"] != "same"]]
    out["details"] = "Palette.revival()[\"checkpoint\"]"
    return out
end

# The revival report, with what happened to a restore that did not finish:
# one that failed before revival, or whose revival stopped before the receipt.
function restore_report(report::String)
    info = RESTORE_INFO[]
    (info === nothing || haskey(REVIVAL_OBSERVATION[], "checkpoint")) && return report
    name = info["checkpoint"]
    line = if haskey(info, "error")
        "[time machine] Restoring checkpoint `$name` failed: $(info["error"]). The world was revived as it was."
    else
        "[time machine] Checkpoint `$name`'s snapshot was swapped in, but its revival did not finish, so no identity was checked."
    end
    REVIVAL_OBSERVATION[]["checkpoint"] = Dict{String, Any}("event" => "restore_failed", "checkpoint" => name,
                                                             "error" => get(info, "error", "revival did not finish"))
    return isempty(report) ? line : line * "\n" * report
end
