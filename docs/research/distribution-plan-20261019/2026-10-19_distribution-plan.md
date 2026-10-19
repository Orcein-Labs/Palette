# Palette distribution plan

Date: 2026-10-19. Base: `main` at 14970e647.

Status: **not started.** This phase begins only after the entry gate below
passes. Until then, nothing in this plan is built.

## Goal

Palette becomes a commonly installed plugin for Claude Code, and for Codex
beside it. A user gets from nothing to a working Palette call in one command,
and from then on gets updates without thinking about them.

## Entry gate

All of the following must hold before any work below starts.

| Gate | Passes when |
|---|---|
| Roadmap done | Every exit gate in the [finish plan](../palette-finish-20261018/2026-10-18_palette-finish-plan.md) passes. As of this date these are not yet shown: Install on a fresh Windows machine with WSL2, the CI streak (10 consecutive green runs under 15 min), and Breadth (the gates hold on Gesso and Cyan work). |
| Audit done | The production engineering audit has run on `main`, and every finding is fixed or deliberately deferred with a reason. |
| No known bugs | No open bug issues, and no open bug in the [friction log](../palette-finish-20261018/2026-10-18_dogfood-friction.md). Items that fall below a gate (such as the 2.3 s before HELLO) don't count as bugs. |

Known items the audit must settle (from the phase reviews):

- `workspace_files` runs `git ls-files` without `-z`, so non-ASCII paths come back quoted.
- The flaky Rust test `sandbox::tests::session_depots_clone_the_newest_snapshot_not_the_live_depot` (failed 1 in 6 full runs).
- No gate fails when a `runtime/security/test_*.py` suite is missing from every CI list.
- `test-case-seconds.json` has no recorded times for `test_tools`, `test_xray`, `test_contexts` and `test_time_machine`.
- The skipped /simplify findings recorded for phases A6 and E.

## What exists today

- `runtime/plugins/build_bundles.py` builds three client archives on each release:
  - `Palette-Claude.zip`, an upload plugin. Its `.claude-plugin/plugin.json` is generated from the portable manifest, and its `.mcp.json` runs `palette-mcp` from the PATH. It embeds the whole SDK source.
  - `Palette-Claude.mcpb`, for Claude Desktop.
  - `Palette-ChatGPT.zip`, which reaches ChatGPT through a user-run MCP tunnel.
- `runtime/plugins/DISTRIBUTION.md` is the install guide shipped in those archives.
- `.agents/plugins/marketplace.json` is the Codex marketplace. No Claude Code marketplace exists, and no Claude manifest is checked in.
- `install.sh` registers Palette with `claude mcp add`, leaving an existing `palette` entry alone, and deletes `sdk.previous` before anything checks the new runtime.
- Licensing: the core is AGPL-3.0-only. The client packages, installers and the public docs that say so are Apache-2.0 (`runtime/licenses/README.md`).

## Channels, in order

1. **Palette's own Claude Code marketplace.** A `.claude-plugin/marketplace.json`
   at the repo root makes the Palette repository a marketplace. Nothing is
   submitted or reviewed. Users install with
   `claude plugin install palette --marketplace latentcollapse/Palette`. Auto-update
   is off by default for a third-party marketplace, and no file in the repo can
   turn it on. That is why the runtime updater (D6) and the version check (D2)
   are needed.
2. **Codex and ChatGPT.** `.agents/plugins/marketplace.json` and
   `Palette-ChatGPT.zip` already exist. Check both against the current formats
   and keep them in step with the Claude manifest.
3. **Anthropic's plugin directory.** Submission goes through the developer portal
   (claude.ai/directory/manage, which needs a paid plan) and a review. A listing
   reaches claude.ai, Cowork and Claude Code. Palette is a local stdio server that
   needs Julia, bubblewrap and the host binary on the user's machine, and hooks
   only run in Claude Code. A listing would therefore reach many users who can't
   run it. Decide after channel 1 is stable, against the directory's
   platform-support table and pre-submission checklist at that time.
4. **`claude-plugins-official`** takes no submissions; it goes through an Anthropic
   partner contact. Not pursued unless invited.

## Work items

**D1. Claude Code plugin manifest.** Check in `runtime/plugins/palette/.claude-plugin/plugin.json`
next to the existing agent-plugins manifest. `build_bundles.py` copies that file
instead of generating its own, so one file is the source for both.
- `name: palette` is permanent. A rename breaks every install.
- Add a `displayName` and a `relevance` block (Julia projects) so Claude Code can suggest it.
- `version` equals the release version and is bumped with every tag. Without a bump, users never receive the new commits.
- The plugin has no top-level `bin/` directory (org sync rejects one), and no file in Git LFS.
- The marketplace plugin carries no SDK. The runtime comes from `install.sh`. The upload ZIP keeps embedding the SDK for clients that have no marketplace.

**D2. Launcher.** Today `.mcp.json` runs `palette-mcp` from the PATH. It changes
to a launcher under `${CLAUDE_PLUGIN_ROOT}/scripts/`, never a machine path.
- It finds the installed runtime.
- It checks that the runtime's version matches the plugin's.
- If the runtime is missing or the versions differ, it fails with one line naming the command to run, never silently. The runtime can't ship inside the plugin: the sandbox needs sudo once, and Julia and the host binary are platform-specific.

**D3. Hooks** (`hooks/hooks.json`). Each hook needs a measured reason, a
test and a latency budget. Candidates:
- *SessionStart* (including after compaction): Palette's status, the runtime version check, and a short reminder of the live world, so a compacted session doesn't forget its state.
- *PreToolUse on Bash:* point shell-style work at Palette. **Off by default** for public users, because it can deny a tool call. Matt chooses the default.
- *PreCompact:* checkpoint the world before the context is summarized, if Time Machine makes this cheap.

**D4. Marketplace.** `.claude-plugin/marketplace.json` at the repo root.
- `owner` is latentcollapse.
- One entry, `palette`, with source `./runtime/plugins/palette`.
- `build_bundles.py` adds the new file to its `excluded` set, so each upload archive still declares exactly one plugin.
- The public repo is 2.2 MB, so cloning it whole as the marketplace stays cheap. Keep it small: Claude Code times out re-clones of large repos.

**D5. Installer.** `install.sh` and `install.ps1` finish with the plugin install
instead of `claude mcp add`.
- **Migration:** a plain `palette` MCP registration that points at this installation is removed, so the user doesn't run two Palette servers. A registration that points elsewhere is left alone and reported, as today.
- When `claude` is not on the PATH, the installer prints the command instead.

**D6. Runtime updater.** A systemd user timer, offered at install time.
- It tracks release tags, never pushes to `main`.
- It installs the new release to a staging directory, prewarms it, and smoke-tests it (`x = 41`, then `x + 1`).
- Only then does it swap the runtime in. A failed step keeps the old runtime.
- `install.sh` keeps `sdk.previous` until the smoke test passes. Today it deletes it before.
- Windows/WSL2 support follows a measured run there.

**D7. Release workflow.**
- Run `claude plugin validate --strict`.
- Fail when the version in `plugin.json`, in the Codex manifest or in `Project.toml` differs from the tag.
- Tag only by SHA, as now.

**D8. Docs.**
- The README leads with the one-command install.
- `runtime/docs/install.md` covers install, update, rollback, uninstall and troubleshooting.
- A plain statement of what the sandbox allows and refuses. Users and directory reviewers both need it before trusting a plugin that runs code.

## Decisions for Matt before D1 starts

- **License.** The plugin is Apache-2.0, but the runtime it needs is AGPL-3.0-only. Many companies forbid AGPL software, which limits adoption at work. Keep the split, or relicense the core? Only Matt can decide, and contributor history may limit the options.
- **Hooks.** Which ones are on by default, especially the Bash redirect.
- **Updater.** Opt-in at install (proposed) or on by default.
- **Directory.** Whether to submit, after the check in channel 3.

## Exit gates for this phase

| Gate | Passes when |
|---|---|
| Fresh install | On a fresh Linux VM and a fresh Windows machine with WSL2, one command installs Palette and a Claude Code session calls it as a plugin. |
| Migration | Starting from today's `claude mcp add` install, the new installer leaves exactly one Palette server registered. Tested under a fake HOME with stub `claude` and `codex`. |
| Updates | A new tag reaches an installed machine through the timer. A release that fails its smoke test is rolled back, and the old runtime still answers. A plugin and runtime version mismatch is reported in one line. |
| Validation | `claude plugin validate --strict` and the version check run in the release workflow and pass. |
| Live machine | Matt's own machine moves onto the plugin layout only with his approval, in a separate step. |

## Facts this plan relies on

Checked on 2026-10-19 against Claude Code 2.1.303. Re-check them when the phase
starts, because the plugin system changes often.

- [Create a marketplace](https://code.claude.com/docs/en/plugin-marketplaces)
- [Publish and distribute a plugin](https://code.claude.com/docs/en/plugins/publish)
- [Host and maintain a marketplace](https://code.claude.com/docs/en/plugins/host-marketplace)
- [Submit a plugin](https://www.claude.com/docs/plugins/submit)
- [Discover plugins](https://code.claude.com/docs/en/discover-plugins)
