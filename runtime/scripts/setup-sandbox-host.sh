#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Make this Linux host able to run Palette's sandbox.
#   setup-sandbox-host.sh check    as the user: exit 0 when the sandbox starts
#   setup-sandbox-host.sh install  as root: install Python 3 and bubblewrap and,
#                                  where AppArmor restricts user namespaces,
#                                  the upstream bwrap profile that permits them
# Global AppArmor and user-namespace policy is left as it is.
set -eu

check() {
    command -v python3 >/dev/null || { echo "python3 is not installed" >&2; return 1; }
    command -v bwrap >/dev/null || { echo "bubblewrap is not installed" >&2; return 1; }
    # The namespace setup the host uses, with every capability dropped inside.
    bwrap --unshare-user --unshare-net --unshare-pid --unshare-ipc \
        --disable-userns --assert-userns-disabled --cap-drop ALL \
        --ro-bind / / --proc /proc --dev /dev --die-with-parent \
        "$(command -v awk)" '/^CapEff:/ { found = 1; held = $2 ~ /[^0]/ } END { exit !found || held }' /proc/self/status \
        || { echo "bubblewrap cannot create the sandbox's namespaces here" >&2; return 1; }
}

install_packages() {
    if command -v apt-get >/dev/null; then
        apt-get update && apt-get install -y python3 bubblewrap
    elif command -v dnf >/dev/null; then
        dnf install -y python3 bubblewrap
    elif command -v pacman >/dev/null; then
        pacman -S --needed --noconfirm python bubblewrap
    elif command -v zypper >/dev/null; then
        zypper --non-interactive install python3 bubblewrap
    else
        echo "Unknown package manager: install python3 and bubblewrap, then run this again" >&2
        return 1
    fi
}

install_profile() {
    restrict=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
    [ -r "$restrict" ] && [ "$(cat "$restrict")" = 1 ] || return 0
    [ -e /etc/apparmor.d/bwrap-userns-restrict ] && return 0
    command -v apparmor_parser >/dev/null || apt-get install -y apparmor curl
    profile=$(mktemp)
    trap 'rm -f "$profile"' EXIT
    # Ubuntu recommends this upstream profile. Pin both revision and content.
    # It permits bwrap setup, then stacks a child profile denying capabilities.
    curl --fail --silent --show-error --location \
        https://gitlab.com/apparmor/apparmor/-/raw/72229df83059480f4e9fb1488624201bdbd61755/profiles/apparmor/profiles/extras/bwrap-userns-restrict \
        --output "$profile"
    printf '%s  %s\n' a964037f6cf0df1099f14226b037eaedde6237c86e715188e93eb460b30be859 "$profile" | sha256sum --check --quiet
    install -m 0644 "$profile" /etc/apparmor.d/bwrap-userns-restrict
    apparmor_parser --replace /etc/apparmor.d/bwrap-userns-restrict
}

case "${1:-}" in
    check) check ;;
    install)
        [ "$(id -u)" = 0 ] || { echo "install needs root: sudo sh $0 install" >&2; exit 1; }
        { command -v python3 && command -v bwrap; } >/dev/null || install_packages
        install_profile ;;
    *) echo "usage: $0 check|install" >&2; exit 2 ;;
esac
