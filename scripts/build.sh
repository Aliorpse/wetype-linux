#!/bin/sh
# Build everything this project ships.
#
#   scripts/build.sh
#
# Products:
#   target/x86_64-unknown-linux-gnu/release/wetype-{daemon,cli}
#   bin/engine/wetype-engine.exe
#   frontends/fcitx5/build/libwetype.so
#
# The engine host is copied into bin/engine because it has to sit next to
# libwxhld.dll, which scripts/fetch_wetype.sh puts there. Meson only compiles
# the addon here; installation is install.sh's job.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

target_dir=${CARGO_TARGET_DIR:-$root/target}
linux_target=x86_64-unknown-linux-gnu
windows_target=x86_64-pc-windows-gnu

echo "==> rust binaries"
cargo build --release -p wetype-daemon -p wetype-cli --target "$linux_target"

echo "==> engine host"
cargo build --release -p wetype-engine --target "$windows_target"
mkdir -p bin/engine
cp -f "$target_dir/$windows_target/release/wetype-engine.exe" bin/engine/

echo "==> fcitx5 addon"
if [ ! -f frontends/fcitx5/build/build.ninja ]; then
    # The source directory has to be explicit: build.sh runs from the repo root,
    # which has no meson.build for Meson to discover.
    meson setup frontends/fcitx5/build frontends/fcitx5
fi
ninja -C frontends/fcitx5/build

echo "build complete"
