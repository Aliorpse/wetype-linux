#!/bin/sh
# Set the project version to <base>+wetype<engine version>.
#
#   scripts/set-version.sh            # keep the current base, refresh the engine part
#   scripts/set-version.sh 0.2.0      # bump the project version
#
# The engine version comes from wetype.lock, so it is never typed twice. The
# same string is written to every place that embeds the project version; Cargo.lock
# is refreshed because the Meson build uses --locked.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
lock=wetype.lock

die() {
    echo "$*" >&2
    exit 1
}

usage() {
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
}

case ${1:-} in
    -h | --help)
        usage
        exit 0
        ;;
esac

[ $# -le 1 ] || die "usage: $0 [base-version]"

engine=$(tr -d '\r' <"$lock" | sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
[ -n "$engine" ] || die "$lock has no \"version\" field"

if [ $# -eq 1 ]; then
    base=$1
else
    base=$(sed -n 's/^version = "\([^"+]*\).*/\1/p' Cargo.toml | head -1)
    [ -n "$base" ] || die "cannot read the workspace version from Cargo.toml"
fi
case $base in
    *+*) die "base version must not contain build metadata: $base" ;;
esac

new="$base+wetype$engine"

sed -i "s|^version = \"[^\"]*\"$|version = \"$new\"|" Cargo.toml
sed -i "s|^  version: '[^']*',$|  version: '$new',|" frontends/fcitx5/meson.build
sed -i "s|^pkgver=.*|pkgver=$new|" packaging/aur/PKGBUILD

grep -q "version = \"$new\"" Cargo.toml ||
    die "could not update the workspace version in Cargo.toml"
grep -q "version: '$new'" frontends/fcitx5/meson.build ||
    die "could not update the project version in meson.build"
grep -q "^pkgver=$new$" packaging/aur/PKGBUILD ||
    die "could not update pkgver in packaging/aur/PKGBUILD"

if command -v cargo >/dev/null 2>&1; then
    (cargo update --workspace --offline >/dev/null 2>&1) ||
        (cargo update --workspace) ||
        echo "warning: could not refresh Cargo.lock; run cargo update --workspace" >&2
fi

echo "project version: $new"
