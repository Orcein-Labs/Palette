# Using routed Palette worlds and approved patches

Palette ships these reusable MCP adapters for independent execution worlds and exact approved patches.

## Choose a world

Call `palette_workspace` with action=create, workspace_id=your-lab, context_id=a-stable-unique-key, scope=thread. Include workspace_id and context_id on subsequent palette and palette_control calls. Context-active routing also works, and attachment persists across router restart. Explicit workspace_id wins. Missing both fields uses the visibly identified shared legacy world; it does not automatically create an isolated thread.

Scope thread compares caller-supplied keys. It prevents accidental collisions, not impersonation by a caller who knows the key. Project scope shares a world for the same canonical project folder; open is an explicit cross-project sharing choice. Project files stay shared even when execution worlds are independent.

Close retains the saved state. Another router may attach the same metadata but cannot execute a world until the current owner closes it. Ownership locks are outside the worker mount and inherited by the adapter; abrupt router death cannot immediately admit a second writer. The adapter handles EOF and normal teardown before releasing ownership. Uncertain calls are not automatically retried.

PALETTE_MAX_LIVE_WORKSPACES defaults to 4 per router. On reaching the cap, close a world before starting another. Inactivity beyond PALETTE_WORKSPACE_IDLE_SECONDS (default 900) retires worlds on subsequent requests. This is opportunistic idle retirement, not a background garbage collector. Tasks, channels and other non-revivable objects remain subject to the existing revival contract.

## Approve one exact patch

A trusted local operator configures PALETTE_PATCH_ROOTS as JSON mapping target names to canonical source directories. This mapping is outside Julia and is not set by tool arguments. Patch storage defaults to the router registry's patches directory, outside all worker mounts.

Use palette_patch action=read, target=palette-core, path=runtime/security/example.py to inspect bounded UTF-8 content and its SHA-256. Use action=prepare with target and changes, where each change is exactly `{path, before_sha256, content}`. Null before_sha256 means create a file. Existing parent directories are required. The response includes the full review diff, digest, expiry and request_id.

Credential and secret paths cannot be read or patched through this interface,
including requests approved before the current policy. Disposable repair stages
omit these files so candidate code cannot return their contents through test output.

Use action=apply with request_id in the same logical workspace. When the client advertises MCP form elicitation, the broker asks the client to present an explicit confirmation for the exact digest and review diff. Accepting that form authorizes one attempt. Client-confirmed approval is not proof of a named human's identity; this relies on a trusted MCP client honoring user interaction. Julia and model-supplied tool arguments cannot mint grants.

If the client does not support confirmation, the request remains pending. An explicitly authorized local administrator can inspect and approve it:

```
python3 runtime/security/patch_broker.py --storage /actual/registry/patches review REQUEST_ID
python3 runtime/security/patch_broker.py --storage /actual/registry/patches approve REQUEST_ID --digest REVIEWED_SHA256
```

Then repeat action=apply. A future grant must be authorized explicitly; merely requesting a patch is not permission for Codex or another agent to approve it on the user's behalf.

Grants expire after 15 minutes and bind the exact change set, root filesystem identity, target, and workspace. They are consumed before the helper runs, including failed and uncertain attempts. Before hashes are checked before any mutation and again before each replacement. Writes use a Linux Landlock-confined helper with descriptor-relative, no-symlink paths. Git metadata, traversal, hardlinks, deletion, renaming and creating directory trees are outside this first version. Landlock ABI >=3 is mandatory; unsupported hosts refuse application.

Individual replacements are atomic. A multi-file patch is not an all-or-nothing transaction. Use a quiescent source checkout; brokers sharing a canonical target serialize through a sibling lock across registries. Arbitrary unmanaged host writers do not participate in that lock. The host must be able to create this sibling lock. Partial completion lists committed paths and hashes and cannot be replayed with the same grant. Receipts survive the Julia world and record authority, timing, scope, digests and outcome.

Targets should be a separate source checkout. Editing that checkout does not deploy it: tests and a controlled installation switch remain separate actions. The deployed adapter's trust-chain paths are protected from self-application by the host classifier; changes there still require the existing explicit approval flow.

## Tested repair workflow

The host runtime can expose repair actions through `Palette.runtime("repair", payload)` without adding an MCP tool. The repair broker accepts `detect`, `classify`, `propose`, `test`, `status`, and `apply`. Detection reads only bounded files under a host-configured target. Proposals use the same exact `{path, before_sha256, content}` changes and digest binding as `palette_patch`.

The operator configures the target's fixed test recipe, frozen host-command protected paths, and any ordinary self-apply prefixes when constructing the host broker. None is accepted in a model request. Tests run in a disposable Bubblewrap worker with networking disabled, no inherited environment or capabilities, the staged candidate tree read-only, and only private scratch writable. Missing Bubblewrap or a failed recipe blocks application. A test result binds the exact proposed content digest and fixed recipe identity and arguments.

`test` starts verification and returns `testing`; inspect `status` in a later
call before applying. Verification does not consume the interactive Julia turn's
deadline. A host restart during verification reports unknown completion rather
than treating the interrupted candidate as passed. Fixed recipes can name narrow
host `read_roots` for a pinned toolchain and prepared dependencies. A Python
installation outside `/usr` needs its exact toolchain directory declared here;
an absolute interpreter in `argv` does not grant access to that directory. Optional
`julia_depots` must be a subset of those roots. The candidate and host dependency
mounts remain read-only, and new compile caches go in private temporary storage.

Self-application is limited to operator-classified `runtime`, `tools`, `recipes`, and sandbox-only `adapters` paths outside the immutable deployed trust chain. Canonical deployed `runtime/security`, `runtime/host`, `runtime/plugins`, and `runtime/scripts` remain protected even when the configured target is a subtree. The registry at `PALETTE_RUNTIME_ROOT` is protected by default. An operator may opt in one sandbox-only Julia source per installed capability with `sandbox_source_paths`; this never makes the manifest or neighboring files patchable. The target must be an immediate operator-owned capability directory beneath the configured registry, and the listed regular single-link `.jl` file must be the source named by its `capability.json`. It must also match an explicit `adapters` or `tools` prefix and pass that target's fixed test recipe before the broker can auto-apply it. Any independently protected host-command executable, imported-code root, config file, or working directory still requires approval.

For example, a trusted repair configuration for the installed `gesso` capability can select only its Julia entrypoint:

```json
{
  "test_recipes": {
    "gesso": {
      "id": "gesso-source-v1",
      "argv": ["/srv/palette/julia/bin/julia", "--startup-file=no", "--project=/srv/palette/packages/environment", "-e", "using Test; include(\"gesso.jl\"); d=gesso(Dict{String,Any}(\"action\"=>\"diagnose\")); @test d[\"status\"]==\"ready\"; @test d[\"cpu\"][\"status\"]==\"available\"; @test d[\"tokenizer\"][\"round_trip\"]"],
      "read_roots": ["/srv/palette/julia", "/srv/palette/base-depot", "/srv/palette/packages", "/srv/palette/packages/depot", "/srv/palette/gesso-models/smollm2-135m"],
      "julia_depots": ["/srv/palette/packages/depot", "/srv/palette/base-depot"]
    }
  },
  "auto_apply_prefixes": {"gesso": [["adapters", "gesso.jl"]]},
  "sandbox_source_paths": {"gesso": ["gesso.jl"]}
}
```

The operator separately maps `gesso` to the installed capability directory in `PALETTE_PATCH_ROOTS` and sets `PALETTE_RUNTIME_ROOT` to its registry parent. Replace the example roots with the pinned Julia toolchain, prepared base depot, managed package store, exact managed depot, and exact versioned model artifact printed by the installer. The inline verifier is fixed operator configuration; it runs from the staged capability directory and is not supplied by a patch request. Keep all recipe dependencies read-only and the repair configuration outside every patch target. Credentials and secrets are forbidden; unknown, configuration, test, and authority paths require the existing elicitation or local administrator approval for the exact patch digest. The repair payload cannot claim approval or supply a test command. Receipts record the linked patch request, tested content digest, recipe identity and arguments, result, and apply outcome, signed with a private host-side key that is unavailable to the worker.

## Deployment

The installer generates serve_palette.py as the entrypoint, which loads the workspace router. Existing plugin configuration and optional toolchain environment are preserved. Keep the legacy OPERATOR_WORKSPACE and PALETTE_STATE_DIR when migrating an existing desk if their layout passes the canonical separation checks. OPERATOR_WORKSPACE must not overlap the runtime source, saved state, PALETTE_WORKSPACE_STATE_ROOT, or host scratch area. Patch targets must exclude registry and grant storage. Invalid layouts fail before tool discovery; a workspace covering your home directory is therefore unsuitable for the routed adapter.

The MCP surface is palette, palette_control, palette_workspace, palette_patch. Client tool discovery may need refreshing after switching the entrypoint. Actual Chat confirmation availability must be established from its initialize capabilities and an interaction; local protocol tests alone do not prove the Chat UI.
