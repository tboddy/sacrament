#!/usr/bin/env bash
# Build Sacrament.app from assets/icon.png and the release binary, and install it.
#
# The bundle exists for one reason: on macOS an application's icon comes from
# its `.app`, never from the window. winit doesn't support window icons on macOS
# at all, so editing assets/icon.png does nothing visible until it has been
# through `iconutil` into an `.icns` inside a bundle. A bundle also buys the
# Dock, Spotlight, and being a drop target in the Finder.
#
# The CLI on $PATH stays a separate install (scripts/install-gui.sh). Both talk
# to the same running editor through the same socket, so `sacrament foo.rs` in a
# terminal opens a tab in the window you launched from the Dock.
#
# Usage:
#   scripts/bundle-mac.sh            build and install to ~/Applications
#   scripts/bundle-mac.sh --build    build into target/ only
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="Sacrament"
BUNDLE_ID="com.tboddy.sacrament"
ICON_SRC="assets/icon.png"
BUILD_ONLY=false
[ "${1:-}" = "--build" ] && BUILD_ONLY=true

for tool in sips iconutil; do
  command -v "$tool" >/dev/null 2>&1 || { echo "need $tool (macOS only)" >&2; exit 1; }
done
[ -f "$ICON_SRC" ] || { echo "missing $ICON_SRC" >&2; exit 1; }

echo "building release binary ..."
cargo build --release -p sacrament-gui

APP="target/$APP_NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# `iconutil` wants every size present in a directory named *.iconset. The @2x
# entries aren't optional: without them the Dock renders the 1x art scaled and
# it looks soft on any modern display.
iconset=$(mktemp -d)/icon.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SRC" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$ICON_SRC" \
    --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$APP/Contents/Resources/$APP_NAME.icns"
rm -rf "$(dirname "$iconset")"

cp target/release/sacrament "$APP/Contents/MacOS/sacrament"

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' crates/gui/Cargo.toml | head -1)
version=${version:-0.1.0}

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>sacrament</string>
  <key>CFBundleIconFile</key><string>$APP_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <!-- Without this the window is drawn at 1x and scaled, which on a Retina
       display makes the whole cell grid look blurry. -->
  <key>NSHighResolutionCapable</key><true/>
  <!-- A document editor: let the Finder hand files to it by drag or Open With. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Text Document</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSItemContentTypes</key>
      <array><string>public.plain-text</string><string>public.source-code</string></array>
    </dict>
  </array>
</dict>
</plist>
PLIST

if [ "$BUILD_ONLY" = true ]; then
  echo "built $APP"
  exit 0
fi

# ~/Applications rather than /Applications: it needs no admin rights, and
# Spotlight and the Dock treat it the same.
dest="$HOME/Applications"
mkdir -p "$dest"
rm -rf "${dest:?}/$APP_NAME.app"
cp -R "$APP" "$dest/"

# The Finder caches icons per bundle path. Touching the bundle makes it
# re-read one that changed, which is otherwise the reason a new icon "doesn't
# take" until logout.
touch "$dest/$APP_NAME.app"

echo "installed $dest/$APP_NAME.app"
running=$(pgrep -x sacrament 2>/dev/null | wc -l | tr -d ' ') || running=0
if [ "$running" -gt 0 ]; then
  echo "note: an instance is already running on the old binary — quit and relaunch."
fi
