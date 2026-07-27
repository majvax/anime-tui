#!/usr/bin/env bash
# Bump both AUR PKGBUILDs to a new version after a GitHub Release is published.
#
# Usage: packaging/update-aur.sh <version>   e.g. packaging/update-aur.sh 0.2.0
#
# It rewrites pkgver (and resets pkgrel to 1) in both PKGBUILDs, fetches the
# static release artifact's SHA-256 for the -bin package, and regenerates each
# .SRCINFO with `makepkg --printsrcinfo`. Run it from a checkout on an Arch box
# (needs makepkg + curl), then copy each package dir into its AUR git repo.
set -euo pipefail

ver="${1:?usage: update-aur.sh <version> (e.g. 0.2.0)}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="majvax/anime-tui"
target="x86_64-unknown-linux-musl"
artifact="anime-tui-${target}.tar.gz"
base="https://github.com/${repo}/releases/download/v${ver}"
url="${base}/${artifact}"
# The Release attaches a checksum sidecar named "<archive-basename>.sha256"
# (i.e. without the .tar.gz), per taiki-e/upload-rust-binary-action.
sidecar="${base}/anime-tui-${target}.sha256"

bump_pkgver() {
    local dir="$1"
    sed -i -E "s/^pkgver=.*/pkgver=${ver}/; s/^pkgrel=.*/pkgrel=1/" "${dir}/PKGBUILD"
}

echo "==> Source package (anime-tui)"
bump_pkgver "${here}/aur/anime-tui"

echo "==> Prebuilt package (anime-tui-bin)"
bump_pkgver "${here}/aur/anime-tui-bin"

echo "--> fetching SHA-256 for ${artifact}"
# Prefer the .sha256 sidecar uploaded alongside the artifact; fall back to hashing.
sha="$(curl -fsSL "${sidecar}" | awk '{print $1}')" || {
    echo "    .sha256 sidecar missing, hashing the artifact directly"
    sha="$(curl -fsSL "${url}" | sha256sum | awk '{print $1}')"
}
sed -i -E "s/^sha256sums=\('.*'\)/sha256sums=('${sha}')/" "${here}/aur/anime-tui-bin/PKGBUILD"

echo "==> Regenerating .SRCINFO files"
for dir in "${here}/aur/anime-tui" "${here}/aur/anime-tui-bin"; do
    ( cd "${dir}" && makepkg --printsrcinfo > .SRCINFO )
done

echo "Done. Review the diffs, then push each package dir to its AUR repo."
