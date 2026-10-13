#!/bin/sh
# Assemble "Topps NFC Bridge.app" from a built binary, sign it ad hoc, and optionally build a DMG.
#   packaging/macos/bundle.sh <binary> <version> <out-dir> [--dmg]
set -eu

BIN=$1
VERSION=$2
OUT=$3
DMG=${4:-}
HERE=$(cd "$(dirname "$0")" && pwd)
APP="$OUT/Topps NFC Bridge.app"

[ -x "$BIN" ] || { echo "not an executable: $BIN" >&2; exit 1; }
echo "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "bad version: $VERSION" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/topps-nfc-bridge"
cp "$HERE/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null

# Unsigned release: an ad hoc signature over the whole bundle (required on Apple silicon).
codesign --force --deep -s - "$APP"
codesign --verify --deep --strict "$APP"
echo "built $APP"

if [ "$DMG" = "--dmg" ]; then
    STAGE=$(mktemp -d)
    cp -R "$APP" "$STAGE/"
    ln -s /Applications "$STAGE/Applications"
    IMG="$OUT/Topps-NFC-Bridge-$VERSION-macos.dmg"
    rm -f "$IMG"
    hdiutil create -volname "Topps NFC Bridge" -srcfolder "$STAGE" -ov -format UDZO "$IMG" >/dev/null
    rm -rf "$STAGE"
    echo "built $IMG"
fi
