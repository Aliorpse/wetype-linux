#!/bin/sh
# Remove WeType completely.
#
#   sudo scripts/uninstall.sh            # /usr files, payload, state, service
#   scripts/uninstall.sh --destdir DIR   # only the staged tree
#
# A direct uninstall removes everything the project put on the machine:
# the files under /usr, the fetched payload and installer cache in bin/, and
# the invoking user's state (plugin config, dictionaries, Wine prefix). The
# systemd user service is disabled first. A staged uninstall (--destdir) only
# removes files from that tree and leaves the running system alone.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

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
            sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

if [ -n "$destdir" ]; then
    [ -d "$destdir" ] || die "no such directory: $destdir"
    destdir=$(CDPATH= cd -- "$destdir" && pwd)
    user_home=""
elif [ "$(id -u)" -ne 0 ]; then
    die "removing files from /usr needs root; pass --destdir for a staged tree"
else
    user=${SUDO_USER:-root}
    user_home=$(getent passwd "$user" | cut -d: -f6) || user_home=
    [ -n "$user_home" ] || user_home=$(eval echo "~$user")
fi

if [ -z "$destdir" ] && command -v systemctl >/dev/null 2>&1; then
    as_user systemctl --user disable --now wetype-daemon.service 2>/dev/null || true
fi

rm -f "$destdir/usr/bin/wetype-daemon"
rm -f "$destdir/usr/lib/fcitx5/libwetype.so"
rmdir "$destdir/usr/lib/fcitx5" 2>/dev/null || true
rm -f "$destdir/usr/share/fcitx5/addon/wetype.conf"
rm -f "$destdir/usr/share/fcitx5/inputmethod/wetype.conf"
for icon in "$destdir"/usr/share/icons/hicolor/*/apps/fcitx-wetype.png; do
    [ -e "$icon" ] && rm -f "$icon"
done
rm -rf "$destdir/usr/share/wetype"
rm -f "$destdir/usr/lib/systemd/user/wetype-daemon.service"

if [ -z "$destdir" ]; then
    # Per-user state: fcitx5 plugin config, dictionaries and the Wine prefix
    # (the last two live under %S/wetype).
    rm -f "$user_home/.config/fcitx5/conf/wetype.conf"
    rm -rf "$user_home/.local/state/wetype"

    # Build-time payload and installer cache.
    rm -rf "$root/bin/engine" "$root/bin/downloads"
    rmdir "$root/bin" 2>/dev/null || true
fi

echo "removed WeType from ${destdir:-/usr}"
echo "restart fcitx5 so it drops the addon: fcitx5 -r -d"
