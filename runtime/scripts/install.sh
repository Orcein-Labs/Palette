#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Install Palette on Linux (or inside WSL2) and connect it to local clients:
#   curl -fsSL https://github.com/latentcollapse/Palette/releases/latest/download/install.sh | sh
# Run again to update; `install.sh --update` installs a newer release only.
# Arguments after the options below go to `palette-host setup` (for example --replace,
# to take over a client's existing palette server). Settings, all optional:
#   PALETTE_VERSION           release tag to install (default: the latest release)
#   PALETTE_HOME              where everything goes (default: ~/.local/share/palette)
#   PALETTE_RELEASE_URL       download the release from this base URL instead
#                             (a mirror, or file:///path for a local copy)
#   PALETTE_AUTO_UPDATE       1 or 0: install the daily update timer without asking
#   PALETTE_WSL_DISTRIBUTION  set by install.ps1 so Windows clients launch through WSL
set -eu

repo=latentcollapse/Palette
home=${PALETTE_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/palette}
asset=palette-linux-x86_64.tar.gz
versions=$home/versions
update=
if [ "${1:-}" = --update ]; then update=1; shift; fi

say() { printf '==> %s\n' "$*"; }
die() { printf 'Palette install stopped: %s\n' "$*" >&2; exit 1; }
interactive() { [ -z "$update" ] && (: </dev/tty) 2>/dev/null; }

[ "$(uname -s)" = Linux ] || die "Palette runs on Linux; on Windows use install.ps1, which sets up WSL2"
command -v curl >/dev/null || die "curl is not installed"
command -v tar >/dev/null || die "tar is not installed"
mkdir -p "$versions"

# Each installation is a directory under versions/ that is never changed once
# it is complete (.palette-ready), and `current` names the one in use. A
# client already running keeps its version until it restarts.
here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || true)
if [ -f "$0" ] && [ -f "$here/install.sh" ] && [ -e "$here/../../.git" ]; then
    # From a source checkout: install its last commit, with a host built from it.
    src=$(cd "$here/../.." && pwd)
    command -v git >/dev/null || die "git is not installed"
    rev=$(git -C "$src" rev-parse --short=12 HEAD)
    [ -z "$(git -C "$src" status --porcelain --untracked-files=no)" ] \
        || say "Uncommitted changes in $src are not installed; installing commit $rev"
    id=git-$rev
    say "Installing commit $rev of the source checkout at $src"
    if [ ! -f "$versions/$id/.palette-ready" ]; then
        command -v cargo >/dev/null || die "building the host needs Rust (https://rustup.rs)"
        stage=$versions/.$id.partial
        rm -rf "$stage" && mkdir -p "$stage"
        git -C "$src" archive HEAD | tar -x -C "$stage"
        # The checkout's resolved packages, when it has them, are the ones it was tested with.
        [ -f "$src/Manifest.toml" ] && cp "$src/Manifest.toml" "$stage/"
        say "Building the sandbox host"
        # One target directory for every commit, so a rebuild is incremental.
        CARGO_TARGET_DIR=$home/build cargo build --manifest-path "$stage/runtime/host/Cargo.toml" --release --locked
        mkdir -p "$stage/runtime/host/target/release"
        cp "$home/build/release/palette-host" "$stage/runtime/host/target/release/palette-host"
        touch "$stage/.palette-ready"
        rm -rf "${versions:?}/$id" && mv "$stage" "$versions/$id"
    fi
else
    if [ -n "${PALETTE_RELEASE_URL:-}" ]; then
        url=$PALETTE_RELEASE_URL tag=${PALETTE_VERSION:-custom}
    else
        tag=${PALETTE_VERSION:-}
        if [ -z "$tag" ]; then
            latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") \
                || die "could not find the latest release"
            tag=${latest##*/}
            case $tag in v*) ;; *) die "could not find the latest release (got $latest)" ;; esac
        fi
        url=https://github.com/$repo/releases/download/$tag
    fi
    if [ -n "$update" ]; then
        installed=$(cat "$home/current/.palette-release" 2>/dev/null || true)
        case $installed in
            ''|custom) say "This installation did not come from a published release; nothing to update"; exit 0 ;;
        esac
        # A mirror names no release, so there is nothing to compare with.
        [ "$tag" = custom ] && { say "Set PALETTE_VERSION to update from $url"; exit 0; }
        newest=$(printf '%s\n%s\n' "${installed#v}" "${tag#v}" | sort -V | tail -n 1)
        if [ "$installed" = "$tag" ] || [ "$newest" != "${tag#v}" ]; then
            say "Palette $installed is up to date"; exit 0
        fi
    fi
    id=$tag
    # A mirror's archive is named by its checksum, so two downloads that
    # differ are never confused.
    if [ "$tag" = custom ] || [ ! -f "$versions/$id/.palette-ready" ]; then
        download=$versions/.download
        rm -rf "$download" && mkdir -p "$download"
        say "Downloading Palette $tag"
        curl -fsSL "$url/$asset" -o "$download/$asset" || die "could not download $url/$asset"
        curl -fsSL "$url/SHA256SUMS.txt" -o "$download/SHA256SUMS.txt" || die "could not download the checksums"
        (cd "$download" && grep " $asset\$" SHA256SUMS.txt | sha256sum --check --quiet) \
            || die "the download does not match its published checksum"
        [ "$tag" = custom ] && id=custom-$(sha256sum "$download/$asset" | cut -c1-12)
    fi
    if [ ! -f "$versions/$id/.palette-ready" ]; then
        stage=$download/sdk
        mkdir "$stage"
        tar -xzf "$download/$asset" -C "$stage" --strip-components=1
        host=$stage/runtime/host/target/release/palette-host
        # The prebuilt host is x86_64 and needs glibc 2.35 or newer; elsewhere
        # it is built here, which needs Rust.
        if [ "$(uname -m)" != x86_64 ] || ! "$host" 2>&1 | grep -q 'usage: palette-host'; then
            rm -f "$host"
            command -v cargo >/dev/null || die "the prebuilt host does not run on this system; install Rust (https://rustup.rs) and run this again"
            say "Building the sandbox host"
            CARGO_TARGET_DIR=$home/build cargo build --manifest-path "$stage/runtime/host/Cargo.toml" --release --locked
            cp "$home/build/release/palette-host" "$host"
        fi
        printf '%s\n' "$tag" > "$stage/.palette-release"
        touch "$stage/.palette-ready"
        rm -rf "${versions:?}/$id" && mv "$stage" "$versions/$id"
    fi
    rm -rf "$versions/.download"
fi
sdk=$versions/$id

# The sandbox needs Python 3 and bubblewrap, and on some distributions an
# AppArmor profile; installing those is the one step that needs root.
sandbox="$sdk/runtime/scripts/setup-sandbox-host.sh"
if ! sh "$sandbox" check 2>/dev/null; then
    if [ "$(id -u)" = 0 ]; then
        sh "$sandbox" install
    elif command -v sudo >/dev/null && interactive; then
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

# The host's setup prepares the version, checks it, and only then points the
# client launcher at it, so a failed update leaves the working installation in use.
set -- --repo-dir "$sdk" --output-dir "$home/client" --data-dir "$home/data" --depot-dir "$home/depot" \
    --runtime-root "$home/capabilities" --ceiling "$home/config/ceiling.json" "$@"
[ -n "${PALETTE_WSL_DISTRIBUTION:-}" ] && set -- "$@" --wsl-distribution "$PALETTE_WSL_DISTRIBUTION"
# An update leaves the clients' registrations as they are: the launcher path does not change.
[ -n "$update" ] && set -- "$@" --no-register
"$sdk/runtime/host/target/release/palette-host" setup "$@" || exit 1
ln -sfn "versions/$id" "$home/current.new" && mv -T "$home/current.new" "$home/current"

# Remove all but the two newest earlier versions, and the layout before
# versions/, skipping any a running process still uses.
in_use() { grep -qsaF "$1/" /proc/[0-9]*/cmdline; }
keep=2
for old in $(ls -1t "$versions"); do
    [ "$old" = "$id" ] && continue
    if [ "$keep" -gt 0 ]; then keep=$((keep - 1)); continue; fi
    in_use "$versions/$old" || rm -rf "${versions:?}/$old"
done
for old in "$home/sdk" "$home/sdk.previous"; do
    [ -d "$old" ] && ! in_use "$old" && rm -rf "$old"
done

# Claude Code also gets the Palette plugin: its skills, and a session-start
# line that says which version is installed.
if [ -z "$update" ] && command -v claude >/dev/null \
   && ! { [ -n "${WSL_DISTRO_NAME:-}" ] && case $(command -v claude) in /mnt/*) true ;; *) false ;; esac; }; then
    if claude plugin marketplace add "$home/current" >/dev/null 2>&1 || claude plugin marketplace list 2>/dev/null | grep -q palette; then
        claude plugin install palette@palette >/dev/null 2>&1 && say "Installed the Palette plugin for Claude Code" \
            || say "Could not install the Claude Code plugin; run: claude plugin install palette@palette"
    fi
elif [ -n "$update" ] && command -v claude >/dev/null; then
    claude plugin marketplace update palette >/dev/null 2>&1 && claude plugin update palette@palette >/dev/null 2>&1 || true
fi

# A release from GitHub can update itself once a day.
if [ -z "$update" ] && [ -z "${PALETTE_RELEASE_URL:-}" ] && [ -f "$sdk/.palette-release" ]; then
    units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
    if [ ! -f "$units/palette-update.timer" ] && command -v systemctl >/dev/null \
       && systemctl --user show-environment >/dev/null 2>&1; then
        answer=${PALETTE_AUTO_UPDATE:-}
        if [ -z "$answer" ] && interactive; then
            printf 'Update Palette automatically once a day? [Y/n] '
            read -r answer </dev/tty || answer=
        fi
        case $answer in [Nn]*|0) ;; *)
            mkdir -p "$units"
            cat > "$units/palette-update.service" <<EOF
[Unit]
Description=Update Palette to its newest release

[Service]
Type=oneshot
Environment=PALETTE_HOME=$home
ExecStart=/bin/sh $home/current/runtime/scripts/install.sh --update
EOF
            cat > "$units/palette-update.timer" <<EOF
[Unit]
Description=Update Palette once a day

[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
EOF
            systemctl --user daemon-reload && systemctl --user enable --now palette-update.timer >/dev/null 2>&1 \
                && say "Palette updates itself daily (systemctl --user disable --now palette-update.timer to stop)" \
                || say "Could not start the update timer; run install.sh --update to update"
        ;; esac
    fi
fi

[ -n "$update" ] && { say "Updated Palette to $id; clients use it from their next start"; exit 0; }
# Under install.ps1 the Windows side prints the next steps.
[ -n "${PALETTE_WSL_DISTRIBUTION:-}" ] && exit 0
cat <<EOF

Palette $id is installed in $home.
Start a new agent session (Claude Code, Codex, Grok, Antigravity) and ask it to evaluate x = 41, then x + 1.
Other clients: point them at $home/client/palette-mcp (stdio MCP).
To remove it, see Removal in $home/current/runtime/docs/install.md.
EOF
