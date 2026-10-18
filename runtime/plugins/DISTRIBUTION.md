# Install Palette in your chatbot

These bundles contain the clean Palette SDK source and client integration. They
do not contain preinstalled Julia, Rust, Python, extra kernels, credentials, or
a hosted service. A runtime runs on Linux, or in WSL2 on Windows.

## 1. Prepare your runtime

The quickest way is the one-command installer. It installs Julia if needed and
a prebuilt host, prepares the sandbox, registers Claude Code, Codex and, on
Windows, Claude Desktop, and checks one call:

```sh
curl -fsSL https://github.com/latentcollapse/Palette.jl/releases/latest/download/install.sh | sh
```

On Windows run `install.ps1` instead; `sdk/runtime/docs/install.md` gives the
command and the settings. The runtime is then in `~/.local/share/palette`, and
the files mentioned below are in its `client/` folder.

To prepare from this bundle instead, use Linux with Julia 1.10 or newer,
Python 3, bubblewrap and Rust/Cargo (the bundle has no prebuilt host). On
Windows, run this in a WSL2 distribution. Native macOS and native Windows
supervised runtimes are not supported. Cloud-only clients can connect to a
Linux host.

Download one client archive. Upload `Palette-ChatGPT.zip` through ChatGPT Plugins
**+**, or `Palette-Claude.zip` through Claude Customize > Plugins > Upload.
Each contains exactly one plugin, its runtime source and this setup guide.
For runtime preparation, extract that same download to a permanent location.
ChatGPT has a single `palette/` folder; Claude's files are at the archive root.
Claude Desktop's separate Extensions installer uses `Palette-Claude.mcpb`,
which also includes its runtime source; it is not a second required download.
The `.mcpb` is itself ZIP format: extract a copy and run setup from its root.
From the directory containing `setup.py`, run:

```sh
python3 setup.py
```

Configuration, data and the depot go to `~/.local/share/palette/` by default;
`--output-dir`, `--data-dir` and `--depot-dir` choose other places outside
`sdk/`. For Windows Claude Desktop add `--wsl-distribution YOUR_DISTRO_NAME`
(the name shown by `wsl.exe --list --quiet`). The helper instantiates the Julia
project, builds the locked Rust host, prewarms the real sandbox, and generates
`palette-launch.json`, `generic-mcp.json`, a local portable plugin, and a
`palette-mcp` stdio command. It registers that command with Claude Code and
Codex when they are installed and have no `palette` server (`--no-register`
skips this), then makes one test call (`--no-check` skips it). It never
requests model API calls or automatically configures a tunnel. Linux must
permit bubblewrap's user namespaces; `sdk/runtime/scripts/setup-sandbox-host.sh
install`, run as root, installs what the sandbox needs without disabling
isolation. See `sdk/runtime/docs/install.md`.

For an existing, already built and prewarmed installation, use
`--repo-dir /absolute/path/to/Palette --connect-only` with its existing depot.
This option checks required files but intentionally does not rebuild or prewarm.

## 2a. Claude

Upload **Palette-Claude.zip** through Customize > Plugins > Upload. Its
`.claude-plugin/plugin.json` manifest, setup skill and `.mcp.json` are at the
plugin root. Prepare the runtime from the same ZIP as described above.
Local clients need the generated `palette-mcp` executable on PATH, or the
absolute command from `generic-mcp.json`. Cloud Claude needs a supported remote
MCP connection to your Linux host; the upload does not host the runtime.

For Claude Desktop's **Extensions** installer, instead download and install
**Palette-Claude.mcpb**. Prepare its included source and select the generated
`palette-launch.json` when prompted. On Windows use `--wsl-distribution` during
setup and make the configuration accessible to the Windows app. This Node
bridge forwards MCP stdio to the selected Linux/WSL2 runtime.

These are alternative client formats. Neither ZIP contains a nested extension
that must be separately downloaded or uploaded.

Official formats:
- https://code.claude.com/docs/en/plugins-reference
- https://github.com/modelcontextprotocol/mcpb

## 2b. ChatGPT Chat

Extract `Palette-ChatGPT.zip` and prepare the runtime above. ChatGPT's cloud Chat
mode cannot execute a local `palette-mcp` command merely because you upload a ZIP.

1. Connect the generated `palette-mcp` executable to **your own** Secure MCP Tunnel
   using OpenAI's tunnel client instructions. Keep the client running on the
   prepared Linux machine. Use the exact absolute path printed by setup.py.
   Alternatively deploy a supported authenticated HTTPS MCP endpoint.
2. In ChatGPT, enable developer mode when available under your account policy.
3. Open Plugins, add a connection named Palette, choose Tunnel, and select your
   tunnel ID; or enter your hosted MCP URL. Review the discovered tools.
4. Start a new Chat conversation and select that connection.

No tunnel ID or API credential is included. Tunnel registration/authentication
is separate from your ChatGPT subscription and from Palette runtime setup.
Follow the current official instructions rather than guessing account permissions:

- https://developers.openai.com/api/docs/guides/secure-mcp-tunnels
- https://developers.openai.com/plugins/deploy/connect-chatgpt

Upload the original `Palette-ChatGPT.zip` through Plugins **+**. It contains
one `palette/` plugin folder, including the SDK and setup instructions, with no
nested plugin declarations. The package has `plugin.json` and `mcp.json` at its
plugin root for Agent Plugins-compatible
local clients. That template expects `palette-mcp` on PATH. Prefer importing the
**generated** `palette-local-plugin` directory, whose configuration has absolute
paths. Directory submission for public ChatGPT distribution requires the remote
endpoint/registration/review workflow; this archive is not an approved directory
listing and contains no fabricated app ID. Do not create duplicate registrations
when only the installed runtime changes.

## 3. Verify persistence and isolation

Ask the model to create a new thread-scoped workspace and use its returned
workspace ID explicitly. Evaluate `probe = 41`, then evaluate `probe + 1` in a
second call: expect 42. Start a separate workspace and check that `probe` is not
defined there. Caller-chosen context keys are routing hints, not authentication.

Provide `BRAIN_BLAST.md` as optional model context, or withhold it for unprimed
exploration. Use public supported API contracts; extra lab toolchains are absent.
Host write roots stay closed until the administrator configures them and the
broker receives exact-operation approval. A client without confirmation support
cannot grant that approval simply by evaluating Julia.

## Updates and license

Keep the SDK at a stable path and retain the external data/depot directories.
Setup preserves an existing generated plugin manifest; use a fresh output
directory to adopt updated listing metadata without overwriting local edits.
Installed with `install.sh`, run it again to update. Otherwise stop the
adapter, replace the SDK with the new source bundle, rebuild/prewarm, restart,
and refresh client tool discovery if schemas changed. Do not copy an
old Manifest blindly into a new source release. Keep a prior source bundle for
rollback; do not overwrite live runtime files during an active call.

Core is AGPL-3.0-only; these client packages/installers and identified public
docs are Apache-2.0. Inherited MIT grants/notices remain intact. The included
`sdk/runtime/licenses/README.md` gives the exact source boundaries. SHA256SUMS.txt
accompanies the artifacts; SOURCE.json records the precise source revision and
the repository-only authoring manifests omitted from the embedded SDK. Their
portable template is retained under `templates/` and installed outside the SDK.
