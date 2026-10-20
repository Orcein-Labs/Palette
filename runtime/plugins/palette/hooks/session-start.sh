#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Tell a new Claude Code session which Palette is installed, or how to install it.
home=${PALETTE_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/palette}
if [ -x "$home/client/palette-mcp" ] && version=$(readlink "$home/current" 2>/dev/null); then
    echo "Palette ${version##*/} is installed: the palette MCP tool evaluates Julia in a persistent sandboxed world. Its TeamLab capability (Palette.runtime(\"invoke\", Dict(\"id\"=>\"teamlab\", ...))) shares tools and notes between this user's agents."
else
    echo "The Palette plugin is installed but the Palette runtime is not. To install it (Linux or WSL2): curl -fsSL https://github.com/latentcollapse/Palette/releases/latest/download/install.sh | sh"
fi
