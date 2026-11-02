#!/bin/sh
# Fetch the WeType payload this source tree expects.
#
#   scripts/fetch_wetype.sh          # the version pinned in wetype.lock
#   scripts/fetch_wetype.sh --update # newest version; rewrite lock and version
#   scripts/fetch_wetype.sh --check  # only report locked vs newest
#   scripts/fetch_wetype.sh --version X # a specific version (no lock write)
#   scripts/fetch_wetype.sh --force  # re-unpack even if already there
#
# The payload is placed in bin/engine and the installer is cached in
# bin/downloads, both relative to the repository root. Nothing is installed:
# install.sh reads bin/engine and stages it under /usr.
#
# Version discovery goes through the winget-pkgs manifests, which are
# machine-readable and track upstream closely:
#   manifests/t/Tencent/WeType/<version>/Tencent.WeType.installer.yaml
# That file also carries the official InstallerUrl and InstallerSha256, so a
# version nobody verified by hand still arrives with an official checksum.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
lock="$root/wetype.lock"
winget_api="https://api.github.com/repos/microsoft/winget-pkgs/contents/manifests/t/Tencent/WeType"
winget_raw="https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/t/Tencent/WeType"

# Runtime closure of libwxhld.dll, derived with objdump -p (ime_net pulls in
# wcwss). Anything the engine loads from its own directory belongs here.
runtime_dlls="libwxhld.dll XNet.dll tensorflowlite_c.dll ime_net.dll wcwss.dll"

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
    echo "$*" >&2
    exit 1
}

lock_get() {
    tr -d '\r' <"$lock" \
        | sed -n "s/^[[:space:]]*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -1
}

lock_set() {
    sed -i "s|\"$1\": \"[^\"]*\"|\"$1\": \"$2\"|" "$lock"
}

# The project version is <semver>+wetype<engine version>, so bumping the engine
# has to bump it too. Cargo.lock pins the workspace members' versions, and the
# Meson build uses --locked, so refresh it as well.
sync_project_version() {
    base=$(sed -n 's/^version = "\([^"+]*\).*/\1/p' "$root/Cargo.toml" | head -1)
    [ -n "$base" ] || die "cannot read the workspace version from Cargo.toml"
    new="$base+wetype$1"
    sed -i "s|^version = \"[^\"]*\"$|version = \"$new\"|" "$root/Cargo.toml"
    sed -i "s|^  version: '[^']*',$|  version: '$new',|" "$root/frontends/fcitx5/meson.build"
    grep -q "version = \"$new\"" "$root/Cargo.toml" ||
        die "could not update the workspace version in Cargo.toml"
    grep -q "version: '$new'" "$root/frontends/fcitx5/meson.build" ||
        die "could not update the project version in meson.build"
    if command -v cargo >/dev/null 2>&1; then
        (cd "$root" && cargo update --workspace --offline >/dev/null 2>&1) ||
            (cd "$root" && cargo update --workspace >/dev/null 2>&1) ||
            echo "warning: could not refresh Cargo.lock; run cargo update --workspace" >&2
    fi
    echo "project version: $new"
}

fetch_to() {
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --connect-timeout 20 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget"
    fi
}

fetch_text() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --connect-timeout 20 "$1"
    else
        wget -q -O - "$1"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "need sha256sum or shasum"
    fi
}

latest_version() {
    fetch_text "$winget_api" \
        | grep -o '"name": *"[0-9][^"]*"' \
        | sed 's/.*"name": *"//; s/"$//' \
        | sort -V | tail -1
}

# manifest_field VERSION FIELD - first match from the winget installer
# manifest, empty when that version is not there.
manifest_field() {
    # The manifest is CRLF terminated; without the tr the URL keeps a trailing
    # \r and curl rejects it as a bad path.
    fetch_text "$winget_raw/$1/Tencent.WeType.installer.yaml" 2>/dev/null \
        | tr -d '\r' \
        | sed -n "s/^  $2: //p" | head -1
}

mode=install
force=0
version=""
update=0
while [ $# -gt 0 ]; do
    case $1 in
        --update) update=1 ;;
        --check) mode=check ;;
        --version)
            shift
            [ $# -gt 0 ] || die "--version needs a value"
            version=$1
            ;;
        --force) force=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

dest=$root/bin/engine
cache=$root/bin/downloads

locked=$(lock_get version)
[ -n "$locked" ] || die "$lock has no \"version\" field"

if [ "$mode" = check ]; then
    latest=$(latest_version) || die "cannot query winget-pkgs"
    [ -n "$latest" ] || die "winget-pkgs returned no WeType versions"
    echo "locked: $locked"
    echo "latest: $latest"
    if [ "$latest" != "$locked" ]; then
        echo "a newer WeType exists; run: scripts/fetch_wetype.sh --update"
    fi
    exit 0
fi

engine_lib=$(lock_get engine_library)
[ -n "$engine_lib" ] || engine_lib=libwxhld.dll
engine_sha=""
installer_sha=""
url=""

if [ "$update" -eq 1 ]; then
    version=$(latest_version) || die "cannot query winget-pkgs"
    [ -n "$version" ] || die "winget-pkgs returned no WeType versions"
    url=$(manifest_field "$version" InstallerUrl)
    installer_sha=$(manifest_field "$version" InstallerSha256 | tr 'A-F' 'a-f')
    [ -n "$url" ] || die "winget has no installer URL for WeType $version"
    [ -n "$installer_sha" ] || die "winget has no installer checksum for WeType $version"
elif [ -n "$version" ]; then
    # A version we did not pin: take the official URL and checksum from winget.
    url=$(manifest_field "$version" InstallerUrl)
    installer_sha=$(manifest_field "$version" InstallerSha256 | tr 'A-F' 'a-f')
    [ -n "$url" ] || url="https://download.z.weixin.qq.com/app/win/WeTypeSetup_$version.exe"
else
    version=$locked
    url=$(lock_get url)
    installer_sha=$(lock_get installer_sha256 | tr 'A-F' 'a-f')
    engine_sha=$(lock_get engine_sha256 | tr 'A-F' 'a-f')
fi
[ -n "$url" ] || die "$lock has no \"url\" field"

marker="$dest/.wetype-version"
if [ "$force" -eq 0 ] && [ -f "$marker" ] && [ "$(cat "$marker")" = "$version" ] &&
    [ -f "$dest/$engine_lib" ]; then
    if [ -z "$engine_sha" ] || [ "$(sha256 "$dest/$engine_lib")" = "$engine_sha" ]; then
        echo "WeType $version already unpacked in $dest"
        exit 0
    fi
fi

mkdir -p "$cache" "$dest"
installer="$cache/WeTypeSetup_$version.exe"
cached=0
if [ -f "$installer" ]; then
    if [ -z "$installer_sha" ] || [ "$(sha256 "$installer")" = "$installer_sha" ]; then
        cached=1
    fi
fi
if [ "$cached" -eq 0 ]; then
    echo "downloading $url"
    fetch_to "$url" "$installer.part"
    mv -f "$installer.part" "$installer"
fi
if [ -n "$installer_sha" ]; then
    actual=$(sha256 "$installer")
    [ "$actual" = "$installer_sha" ] ||
        die "installer sha256 mismatch for $version:
  expected $installer_sha
  actual   $actual"
else
    echo "warning: no official checksum for $version; the installer is unverified" >&2
fi

seven=$(command -v 7z || command -v 7za || true)
[ -n "$seven" ] || die "need 7z (p7zip) to unpack the installer"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
echo "unpacking WeType $version"
extract="$version/$engine_lib $version/BuildIn"
for d in $runtime_dlls; do
    extract="$extract $version/$d"
done
# Word splitting is intentional: these are archive member names without spaces.
# shellcheck disable=SC2086
"$seven" x -y -o"$tmp" "$installer" $extract >/dev/null

src="$tmp/$version"
[ -f "$src/$engine_lib" ] || die "installer does not contain $version/$engine_lib"
for d in $runtime_dlls; do
    [ -f "$src/$d" ] || die "installer does not contain $version/$d"
done
[ -d "$src/BuildIn" ] || die "installer does not contain $version/BuildIn"

extracted_sha=$(sha256 "$src/$engine_lib")
if [ -n "$engine_sha" ]; then
    [ "$extracted_sha" = "$engine_sha" ] ||
        die "engine library is not the tested build:
  expected $engine_sha
  actual   $extracted_sha"
fi

# Replace, do not merge: a stale BuildIn from another version would be loaded
# silently, and the version marker exists precisely to prevent that.
rm -rf "$dest/BuildIn"
for d in $runtime_dlls; do
    cp -f "$src/$d" "$dest/"
done
cp -r "$src/BuildIn" "$dest/"
printf '%s\n' "$version" >"$marker"

if [ "$update" -eq 1 ]; then
    lock_set version "$version"
    lock_set url "$url"
    lock_set installer_sha256 "$installer_sha"
    lock_set engine_sha256 "$extracted_sha"
    sync_project_version "$version"
    echo "wetype.lock updated to $version"
fi

echo "WeType $version ready in $dest"
du -sh "$dest"
