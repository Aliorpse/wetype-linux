#!/bin/sh
# Install WeType into /usr, optionally staged under DESTDIR.
#
#   scripts/install.sh --destdir PKGDIR   # package build (PKGBUILD)
#   sudo scripts/install.sh               # direct /usr install
#
# The engine payload must already exist in bin/engine (scripts/fetch_wetype.sh
# puts it there) and the binaries must be built; install.sh does neither.
#
# A staged install (--destdir) touches only that tree. A direct /usr install
# also runs `systemctl --user enable --now wetype-daemon` for the invoking user
# (SUDO_USER when run through sudo) and points them at the fcitx5 setup step.
#
# Layout:
#   /usr/bin/wetype-daemon
#   /usr/lib/fcitx5/libwetype.so
#   /usr/share/fcitx5/addon/wetype.conf
#   /usr/share/fcitx5/inputmethod/wetype.conf
#   /usr/share/icons/hicolor/*/apps/fcitx-wetype.png
#   /usr/share/wetype/engine/{libwxhld.dll,...,BuildIn,wetype-engine.exe}
#   /usr/lib/systemd/user/wetype-daemon.service
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

target_dir=${CARGO_TARGET_DIR:-$root/target}
engine_src=$root/bin/engine
destdir=${DESTDIR:-}

die() {
    echo "$*" >&2
    exit 1
}

# systemd --user always talks to the invoking user's session. When the script
# runs through sudo, that is SUDO_USER, not root.
as_user() {
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
        uid=$(id -u "$SUDO_USER")
        sudo -u "$SUDO_USER" env \
            XDG_RUNTIME_DIR="/run/user/$uid" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
            "$@"
    else
        "$@"
    fi
}

while [ $# -gt 0 ]; do
    case $1 in
        --destdir)
            shift
            [ $# -gt 0 ] || die "--destdir needs a value"
            destdir=$1
            ;;
        -h | --help)
            sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

if [ -n "$destdir" ]; then
    mkdir -p "$destdir"
    destdir=$(CDPATH= cd -- "$destdir" && pwd)
elif [ "$(id -u)" -ne 0 ]; then
    die "installing to /usr needs root; pass --destdir for a staged install"
fi

[ -f "$engine_src/libwxhld.dll" ] ||
    die "no engine payload in $engine_src
run scripts/fetch_wetype.sh first; install.sh never downloads it"
[ -f "$engine_src/wetype-engine.exe" ] ||
    die "no engine host in $engine_src
scripts/build.sh produces it"

locked=$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' wetype.lock | head -1)
have=$(cat "$engine_src/.wetype-version" 2>/dev/null || true)
if [ -n "$locked" ] && [ -n "$have" ] && [ "$locked" != "$have" ]; then
    echo "warning: payload is WeType $have but wetype.lock pins $locked" >&2
fi

# The layout is spelled out here, not in meson.build: a package build gets one
# file list to read, and there is no Meson install step whose prefix and
# DESTDIR semantics have to be mirrored.
echo "==> install"
daemon_build=$target_dir/x86_64-unknown-linux-gnu/release/wetype-daemon
[ -f "$daemon_build" ] || die "no daemon at $daemon_build; run scripts/build.sh first"
install -Dm755 "$daemon_build" "$destdir/usr/bin/wetype-daemon"

addon_build=frontends/fcitx5/build/libwetype.so
[ -f "$addon_build" ] || die "no addon at $addon_build; run scripts/build.sh first"
install -Dm755 "$addon_build" "$destdir/usr/lib/fcitx5/libwetype.so"
install -Dm644 frontends/fcitx5/data/wetype-addon.conf \
    "$destdir/usr/share/fcitx5/addon/wetype.conf"
install -Dm644 frontends/fcitx5/data/wetype.conf \
    "$destdir/usr/share/fcitx5/inputmethod/wetype.conf"
for icon in frontends/fcitx5/data/icons/hicolor/*/apps/fcitx-wetype.png; do
    install -Dm644 "$icon" "$destdir/usr/share/${icon#frontends/fcitx5/data/}"
done

# Refresh the hicolor cache so the icon shows up immediately. Only for a real
# /usr install (the script is root there); a DESTDIR stage must not touch the
# host cache.
if [ -z "$destdir" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
    echo "==> icon cache"
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor ||
        echo "warning: gtk-update-icon-cache failed" >&2
fi

echo "==> engine payload"
install -d "$destdir/usr/share/wetype/engine"
cp -a "$engine_src/." "$destdir/usr/share/wetype/engine/"
chmod 0755 "$destdir/usr/share/wetype/engine/wetype-engine.exe"

echo "==> systemd user unit"
unitdir=$destdir/usr/lib/systemd/user
install -d "$unitdir"
sed -e "s|@BINDIR@|/usr/bin|g" \
    -e "s|@ENGINE_DIR@|/usr/share/wetype/engine|g" \
    -e "s|@DATA_DIR@|%S/wetype|g" \
    -e "s|@WINEPREFIX@|%S/wetype/wineprefix|g" \
    packaging/systemd/wetype-daemon.service.in \
    >"$unitdir/wetype-daemon.service"

if [ -n "$destdir" ]; then
    echo "staged under $destdir"
    exit 0
fi

echo "==> enable user service"
if command -v systemctl >/dev/null 2>&1; then
    as_user systemctl --user daemon-reload || true
    if ! as_user systemctl --user enable --now wetype-daemon.service; then
        echo "could not enable the service; run this in your session:" >&2
        echo "  systemctl --user enable --now wetype-daemon" >&2
    fi
else
    echo "systemctl not found; enable the unit manually:" >&2
    echo "  systemctl --user enable --now wetype-daemon" >&2
fi

echo
echo "installed into /usr; restart fcitx5 and add 微信输入法 to its input methods:"
echo "  fcitx5 -r -d && fcitx5-configtool"
