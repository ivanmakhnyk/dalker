#!/bin/sh
# Builds dalker_<version>_amd64.deb from a release binary.
#   packaging/linux/build-deb.sh <path/to/dalkerd> [out-dir]
# Run on Linux (or WSL); the staging tree must live on a Linux filesystem.
set -e
BIN=${1:?usage: build-deb.sh <dalkerd binary> [out-dir]}
OUT=${2:-.}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# Debian sorts "~" before the release, so 1.2.0-rc.1 becomes 1.2.0~rc.1 (older than 1.2.0).
VERSION=$(sh "$ROOT/scripts/version.sh" | sed 's/-/~/')
[ -n "$VERSION" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
STAGE=$(mktemp -d)
chmod 755 "$STAGE"
trap 'rm -rf "$STAGE"' EXIT

install -D -m 0755 "$BIN" "$STAGE/usr/bin/dalkerd"
install -D -m 0755 "$HERE/dalker-launch" "$STAGE/usr/lib/dalker/dalker-launch"
EXTSRC="$HERE/gnome-extension/dalker-border@ivanmakhnyk.github.io"
EXTDST="$STAGE/usr/share/gnome-shell/extensions/dalker-border@ivanmakhnyk.github.io"
install -D -m 0644 "$EXTSRC/metadata.json" "$EXTDST/metadata.json"
install -D -m 0644 "$EXTSRC/extension.js" "$EXTDST/extension.js"
install -D -m 0644 "$HERE/dalker.service" "$STAGE/lib/systemd/system/dalker.service"
# Host settings window (`dalkerd settings`): web access, password, MCP server and tokens.
# It runs as the desktop user and edits the root-only config through the pkexec helper.
install -D -m 0755 "$HERE/dalker-settings-helper" "$STAGE/usr/lib/dalker/dalker-settings-helper"
install -D -m 0644 "$HERE/dalker-settings.desktop" "$STAGE/usr/share/applications/dalker-settings.desktop"
# Dalker's icon for the menu entry (assets/branding).
for size in 16 24 32 48 64 128 256; do
    install -D -m 0644 "$ROOT/assets/branding/png/dalker-$size.png" \
        "$STAGE/usr/share/icons/hicolor/${size}x${size}/apps/dalker.png"
done
install -D -m 0644 "$ROOT/LICENSE" "$STAGE/usr/share/doc/dalker/copyright"
install -D -m 0644 "$ROOT/assets/branding/dalker-app.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/dalker.svg"
install -D -m 0644 "$HERE/io.github.ivanmakhnyk.Dalker.settings.policy" "$STAGE/usr/share/polkit-1/actions/io.github.ivanmakhnyk.Dalker.settings.policy"

mkdir -p "$STAGE/DEBIAN"
sed "s/@VERSION@/$VERSION/" "$HERE/debian/control" > "$STAGE/DEBIAN/control"
cp "$HERE/debian/templates" "$STAGE/DEBIAN/templates"
for f in config postinst prerm postrm; do
    install -m 0755 "$HERE/debian/$f" "$STAGE/DEBIAN/$f"
done
# The config file is created/owned by postinst (it holds the password hash); keep it on upgrade/remove.

fakeroot dpkg-deb --build "$STAGE" "$OUT/dalker_${VERSION}_amd64.deb" >/dev/null
echo "built $OUT/dalker_${VERSION}_amd64.deb"
