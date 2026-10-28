#!/bin/bash
# Cloud environment setup for coversync: set the environment's setup script to
# `cd coversync && tools/cloud-setup.sh`.
# Rust stable, Node and pnpm come with the image; this adds the optional tools
# (fpcalc for Smart suggestions; Firefox, geckodriver and websocket-client for
# tools/ui_smoke.py) and warms the pnpm store and cargo target dir.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# Ubuntu's firefox package is a snap stub, so use Mozilla's apt repo.
install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg \
  -o /etc/apt/keyrings/packages.mozilla.org.asc
echo "deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main" \
  > /etc/apt/sources.list.d/mozilla.list
printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' \
  > /etc/apt/preferences.d/mozilla

apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  libchromaprint-tools python3-websocket sqlite3 firefox

# GitHub release downloads are blocked here, so build geckodriver from crates.io.
command -v geckodriver >/dev/null || cargo install --locked geckodriver

cd "$(dirname "$0")/.."
# The daemon embeds web/build, so build the UI before the Rust crates.
(cd web && pnpm install --frozen-lockfile && pnpm build)
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
