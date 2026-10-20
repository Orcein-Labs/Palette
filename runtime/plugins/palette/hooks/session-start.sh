#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Tell a new Claude Code session which Palette is installed, or how to install it.
home=${PALETTE_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/palette}
if [ -x "$home/client/palette-mcp" ] && version=$(readlink "$home/current" 2>/dev/null); then
    echo "Palette ${version##*/} is installed: the palette MCP tool evaluates Julia in a persistent sandboxed world. Its TeamLab capability (Palette.runtime(\"invoke\", Dict(\"id\"=>\"teamlab\", ...))) is a shop of Julia tools this user's agents share: browse it, try a listing, pin the ones worth keeping."
    "$home/current/runtime/host/target/release/palette-host" teamlab-news --agent claude 2>/dev/null || true
else
    echo "The Palette plugin is installed but the Palette runtime is not. To install it (Linux or WSL2): curl -fsSL https://github.com/latentcollapse/Palette/releases/latest/download/install.sh | sh"
fi
