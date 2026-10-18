#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Install Palette on Linux (or inside WSL2) and connect it to local clients:
#   curl -fsSL https://github.com/latentcollapse/Palette/releases/latest/download/install.sh | sh
# Run again to update. Settings, all optional:
#   PALETTE_VERSION           release tag to install (default: the latest release)
#   PALETTE_HOME              where everything goes (default: ~/.local/share/palette)
#   PALETTE_RELEASE_URL       download the release from this base URL instead
#                             (a mirror, or file:///path for a local copy)
#   PALETTE_WSL_DISTRIBUTION  set by install.ps1 so Windows clients launch through WSL
set -eu

repo=latentcollapse/Palette
home=${PALETTE_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/palette}
asset=palette-linux-x86_64.tar.gz

say() { printf '==> %s\n' "$*"; }
die() { printf 'Palette install stopped: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "Palette runs on Linux; on Windows use install.ps1, which sets up WSL2"
command -v curl >/dev/null || die "curl is not installed"
command -v tar >/dev/null || die "tar is not installed"

# Run from a source checkout, the checkout is the SDK and the host is built
# from it; otherwise the release archive is downloaded and checked.
here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || true)
if [ -f "$0" ] && [ -f "$here/install.sh" ] && [ -f "$here/../../Project.toml" ]; then
    sdk=$(cd "$here/../.." && pwd)
    say "Using the source checkout at $sdk"
else
    if [ -n "${PALETTE_RELEASE_URL:-}" ]; then
        url=$PALETTE_RELEASE_URL
    elif [ -n "${PALETTE_VERSION:-}" ]; then
        url=https://github.com/$repo/releases/download/$PALETTE_VERSION
    else
        url=https://github.com/$repo/releases/latest/download
    fi
    sdk=$home/sdk
    download=$home/.download
    rm -rf "$download" && mkdir -p "$download"
    say "Downloading Palette ${PALETTE_VERSION:-(latest release)}"
    curl -fsSL "$url/$asset" -o "$download/$asset" || die "could not download $url/$asset"
    curl -fsSL "$url/SHA256SUMS.txt" -o "$download/SHA256SUMS.txt" || die "could not download the checksums"
    (cd "$download" && grep " $asset\$" SHA256SUMS.txt | sha256sum --check --quiet) \
        || die "the download does not match its published checksum"
    mkdir "$download/sdk"
    tar -xzf "$download/$asset" -C "$download/sdk" --strip-components=1
    host=$download/sdk/runtime/host/target/release/palette-host
    # The prebuilt host is x86_64 and needs glibc 2.35 or newer; elsewhere
    # setup builds it, which needs Rust.
    if [ "$(uname -m)" != x86_64 ] || ! "$host" 2>&1 | grep -q 'usage: palette-host'; then
        rm -f "$host"
        command -v cargo >/dev/null || die "the prebuilt host does not run on this system; install Rust (https://rustup.rs) and run this again"
    fi
    rm -rf "$home/sdk.previous"
    [ -d "$sdk" ] && mv "$sdk" "$home/sdk.previous"
    mv "$download/sdk" "$sdk"
    rm -rf "$download"
fi

# The sandbox needs Python 3 and bubblewrap, and on some distributions an
# AppArmor profile; installing those is the one step that needs root.
sandbox="$sdk/runtime/scripts/setup-sandbox-host.sh"
if ! sh "$sandbox" check 2>/dev/null; then
    if [ "$(id -u)" = 0 ]; then
        sh "$sandbox" install
    elif command -v sudo >/dev/null && (: </dev/tty) 2>/dev/null; then
        say "Installing the sandbox's system packages (needs your password once)"
        sudo sh "$sandbox" install </dev/tty
    else
        die "run this once as root, then run the installer again: sudo sh $sandbox install"
    fi
    sh "$sandbox" check || die "the sandbox cannot start on this system; see $sdk/runtime/docs/install.md"
fi

julia_ok() {
    command -v julia >/dev/null && julia --startup-file=no -e 'exit(VERSION >= v"1.10" ? 0 : 1)' </dev/null
}
[ -x "$HOME/.juliaup/bin/julia" ] && PATH=$HOME/.juliaup/bin:$PATH
if ! julia_ok; then
    say "Installing Julia with juliaup"
    curl -fsSL https://install.julialang.org | sh -s -- --yes
    PATH=$HOME/.juliaup/bin:$PATH
    julia_ok || die "Julia 1.10 or newer is needed; juliaup did not provide it"
fi
export PATH

set -- --repo-dir "$sdk" --output-dir "$home/client" --data-dir "$home/data" --depot-dir "$home/depot"
[ -n "${PALETTE_WSL_DISTRIBUTION:-}" ] && set -- "$@" --wsl-distribution "$PALETTE_WSL_DISTRIBUTION"
python3 "$sdk/runtime/plugins/setup.py" "$@" || exit 1
rm -rf "$home/sdk.previous"
# Under install.ps1 the Windows side prints the next steps.
[ -n "${PALETTE_WSL_DISTRIBUTION:-}" ] && exit 0
cat <<EOF

Palette is installed in $home.
Start a new Claude Code or Codex session and ask it to evaluate x = 41, then x + 1.
Other clients: point them at $home/client/palette-mcp (stdio MCP).
To remove it: claude mcp remove palette; codex mcp remove palette; rm -rf $home
EOF
