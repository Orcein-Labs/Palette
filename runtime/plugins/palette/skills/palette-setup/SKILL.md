---
name: palette-setup
description: Set up the bundled Palette Julia runtime, connect its MCP tools, or verify a newly installed Palette plugin.
---

Read the bundled INSTALL.md before setup. On a Linux or WSL2 host, prefer the
one-command installer it gives: one approval covers the whole install, and it
registers local clients and checks a call. Otherwise run the bundled setup.py
with the documented Julia, Python, Rust and Bubblewrap prerequisites. Preserve
external workspace state. Use the paths setup.py prints rather than guessing
host paths.

A local client may launch the prepared palette-mcp stdio command. A cloud chat
needs its supported remote MCP connection or tunnel; uploading this plugin does
not create that service. If no tools are available, explain the missing setup
step instead of claiming that Julia ran. Never invent an endpoint or app ID.

Once tools are connected, create a fresh thread-scoped workspace and record its
returned ID. Evaluate probe = 41, then probe + 1 in a separate call and check 42.
Verify another workspace does not share probe. Report actual tool results and
any setup limitation. Optional BRAIN_BLAST.md is separate context, not required.
