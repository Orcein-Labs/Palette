#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Prepare Palette on Linux, register it with local clients, and check it works.
# Builds the sandbox host if this source has none, then runs `palette-host setup`
# with the arguments given here (see `sh setup.sh --help`).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
# A source checkout holds this file at runtime/plugins/; a client bundle holds
# it beside sdk/.
if [ -f "$here/../../Project.toml" ]; then
    repo=$(cd "$here/../.." && pwd)
else
    repo=$here/sdk
fi
# --repo-dir names another source (an existing installation, for
# --connect-only); its host runs setup.
previous=
for word in "$@"; do
    [ "$previous" = --repo-dir ] && repo=$word
    previous=$word
done
host=$repo/runtime/host/target/release/palette-host

# A release archive carries a prebuilt host; a checkout builds its own.
cargo=$(command -v cargo || true)
if [ ! -x "$host" ] || { [ -e "$repo/.git" ] && [ -n "$cargo" ]; }; then
    if [ -z "$cargo" ]; then
        echo "This source has no prebuilt host: install Rust (https://rustup.rs) or use the release installer" >&2
        exit 1
    fi
    echo "==> Building the sandbox host"
    "$cargo" build --manifest-path "$repo/runtime/host/Cargo.toml" --release --locked
fi
exec "$host" setup --repo-dir "$repo" "$@"
