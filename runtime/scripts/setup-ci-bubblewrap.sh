#!/usr/bin/env bash
# GitHub Ubuntu runners only; leave global AppArmor/userns policy intact.
set -euo pipefail
here=$(dirname "$0")
sudo sh "$here/setup-sandbox-host.sh" install
# Exercise the namespace setup that previously failed before Julia startup.
sh "$here/setup-sandbox-host.sh" check
