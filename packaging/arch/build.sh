#!/bin/bash
# Build the pacman package from this checkout's HEAD (uncommitted changes
# are left out): packs the commit into the tarball the PKGBUILD names, so
# makepkg uses it instead of downloading the release, then runs makepkg in a
# scratch directory. Missing build dependencies are installed with sudo.
#
#   packaging/arch/build.sh [OUT_DIR] [makepkg options...]
#
# The package lands in OUT_DIR (default dist/).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)
out=$(realpath -m "${1:-$root/dist}")
shift $(($# > 0 ? 1 : 0))

pkgver=$(sed -n 's/^pkgver=//p' "$here/PKGBUILD")
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/crates/coversyncd/Cargo.toml")
if [[ $pkgver != "$version" ]]; then
  echo "PKGBUILD's pkgver ($pkgver) isn't coversyncd's version ($version)." >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$here/PKGBUILD" "$work/"
git -C "$root" archive --prefix="coversync-$pkgver/" -o "$work/coversync-$pkgver.tar.gz" HEAD
mkdir -p "$out"
cd "$work"
PKGDEST="$out" makepkg --syncdeps --noconfirm "$@"
