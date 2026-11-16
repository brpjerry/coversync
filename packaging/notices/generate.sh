#!/bin/bash
# Write the third-party notices for one of coversync's programs: the
# licenses of the Rust crates compiled into it (from Cargo.lock, with
# cargo-about) and, for coversyncd, of the npm packages in its web UI (as
# the UI build lists them: build the UI first).
#
#   packaging/notices/generate.sh OUT_FILE           coversyncd
#   packaging/notices/generate.sh --agent OUT_FILE   coversync, the sync agent
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [[ ${1:-} == --agent ]]; then
  {
    cat <<'EOF'
Third-party software in coversync's sync agent
==============================================

coversync, the sync agent, includes the Rust crates below, each under the
license shown with it. coversync itself is licensed under the GNU Affero
General Public License, version 3 or later.

Rust crates
===========

EOF
    cargo about generate --frozen -c "$here/about.toml" \
      -m "$root/crates/coversync-cli/Cargo.toml" "$here/about.hbs"
  } > "$2"
  exit
fi

bundled=$root/web/.svelte-kit/bundled-packages.json
if [[ ! -f $bundled ]]; then
  echo "$bundled is missing: build the UI first (cd web && pnpm build)." >&2
  exit 1
fi

{
  cat <<'EOF'
Third-party software in coversync
=================================

coversyncd includes the Rust crates and npm packages below, each under the
license shown with it. coversync itself is licensed under the GNU Affero
General Public License, version 3 or later.

Rust crates
===========

EOF
  cargo about generate --frozen -c "$here/about.toml" \
    -m "$root/crates/coversyncd/Cargo.toml" "$here/about.hbs"
  cat <<'EOF'

Web UI
======

EOF
  node "$here/web.mjs" "$bundled"
} > "$1"
