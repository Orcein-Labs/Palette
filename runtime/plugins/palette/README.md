# Palette plugin

This directory supplies the portable Agent Plugins manifest and a stdio MCP
configuration. Install the runtime before enabling it. The template calls
`palette-mcp` on PATH; the installer creates that command and can generate an
absolute-path configuration for clients that do not expand plugin paths.

From the Palette repository root, after installing the runtime prerequisites:

```sh
julia --project=. -e 'using Pkg; Pkg.instantiate()'
cargo build --manifest-path runtime/host/Cargo.toml --release --locked
python3 runtime/security/prewarm_depot.py --project-dir "$PWD" \
  --host-bin "$PWD/runtime/host/target/release/palette-host"
python3 runtime/security/install_operator_plugin.py --format portable \
  --plugin-dir "$HOME/.local/share/palette-plugin" \
  --workspace-dir /absolute/path/to/workspace \
  --bin-dir "$HOME/.local/bin"
```

Cold preparation can take several minutes. Complete it before connecting an
interactive client; see [preparation and updates](../../docs/install.md).

Import that generated directory in an Agent Plugins-compatible client. For the
Codex compatibility format, omit `--format portable` and choose a separate plugin
directory. Generic stdio MCP clients can run `palette-mcp`. First install and
start the user service described in [installation](../../docs/install.md#user-service-lifecycle);
the launcher connects to its stable runtime socket and does not own the Julia
daemon lifecycle.

The repository marketplace is `.agents/plugins/marketplace.json`. Install the
runtime and put the generated `palette-mcp` command on PATH before enabling its
Palette entry. Marketplace availability depends on the client.

ChatGPT Chat requires a registered remote connection or Secure MCP Tunnel to
this service; local files alone do not make a cloud conversation reach it.
Public directory publication requires a supported hosted endpoint and review.
No personal tunnel IDs, connection IDs, credentials, extra kernels, or external
SDKs are distributed here.

The server offers `palette`, `palette_control`, `palette_workspace`, and
`palette_patch`. These tools share the core runtime and authority contracts.
A client lacking form confirmation cannot automatically approve exact patches.

Packaging follows the [OpenAI plugin format](https://developers.openai.com/plugins/build/plugins).
Connection setup follows [OpenAI's connection guide](https://developers.openai.com/plugins/deploy/connect-chatgpt).
