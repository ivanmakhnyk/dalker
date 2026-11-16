#!/bin/sh
# Builds Dalker-<version>.pkg: Dalker.app in /Applications and the LaunchAgent that starts it
# at login.
#   packaging/macos/build-pkg.sh <path/to/dalkerd> [out-dir]
# Run on macOS (needs codesign, pkgbuild, productbuild, sips and iconutil).
#
# Signing: DALKER_SIGN_IDENTITY names a code-signing certificate in the keychain (the
# project's self-signed "Dalker" certificate for releases). Without it the app is signed ad
# hoc, which is fine for testing but makes macOS forget the Screen Recording and
# Accessibility permissions with every new build. The package itself is not signed.
set -e
BIN=${1:?usage: build-pkg.sh <dalkerd binary> [out-dir]}
OUT=${2:-.}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
VERSION=$(sh "$ROOT/scripts/version.sh")
[ -n "$VERSION" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
# Bundle versions are numbers only: 1.2.0-rc.1 becomes 1.2.0.
BUNDLE_VERSION=${VERSION%%-*}
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$OUT"

APP="$STAGE/root/Applications/Dalker.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 0755 "$BIN" "$APP/Contents/MacOS/dalkerd"
sed "s/@VERSION@/$BUNDLE_VERSION/" "$HERE/Info.plist" > "$APP/Contents/Info.plist"

# App icon from the PNGs in assets/branding (the largest one scaled up for 512 and 1024).
ICONSET="$STAGE/Dalker.iconset"
mkdir -p "$ICONSET"
PNG="$ROOT/assets/branding/png"
cp "$PNG/dalker-16.png" "$ICONSET/icon_16x16.png"
cp "$PNG/dalker-32.png" "$ICONSET/icon_16x16@2x.png"
cp "$PNG/dalker-32.png" "$ICONSET/icon_32x32.png"
cp "$PNG/dalker-64.png" "$ICONSET/icon_32x32@2x.png"
cp "$PNG/dalker-128.png" "$ICONSET/icon_128x128.png"
cp "$PNG/dalker-256.png" "$ICONSET/icon_128x128@2x.png"
cp "$PNG/dalker-256.png" "$ICONSET/icon_256x256.png"
sips -z 512 512 "$PNG/dalker-256.png" --out "$ICONSET/icon_256x256@2x.png" >/dev/null
cp "$ICONSET/icon_256x256@2x.png" "$ICONSET/icon_512x512.png"
sips -z 1024 1024 "$PNG/dalker-256.png" --out "$ICONSET/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Dalker.icns"

codesign --force --sign "${DALKER_SIGN_IDENTITY:--}" --identifier io.github.ivanmakhnyk.Dalker \
    --timestamp=none "$APP"
codesign --verify --strict "$APP"

mkdir -p "$STAGE/root/Library/LaunchAgents"
install -m 0644 "$HERE/io.github.ivanmakhnyk.Dalker.plist" "$STAGE/root/Library/LaunchAgents/"

# Keep the app where the LaunchAgent expects it, even if a copy exists elsewhere.
pkgbuild --analyze --root "$STAGE/root" "$STAGE/components.plist" >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO "$STAGE/components.plist"
pkgbuild --root "$STAGE/root" --component-plist "$STAGE/components.plist" \
    --identifier io.github.ivanmakhnyk.Dalker --version "$BUNDLE_VERSION" \
    --scripts "$HERE/scripts" --install-location / "$STAGE/Dalker-component.pkg"
productbuild --package "$STAGE/Dalker-component.pkg" "$OUT/Dalker-$VERSION.pkg"
echo "built $OUT/Dalker-$VERSION.pkg"
