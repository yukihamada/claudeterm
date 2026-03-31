#!/bin/bash
set -e
cd "$(dirname "$0")/.."

echo "=== NOU Build ==="

# 1. Build claudeterm for macOS (arm64 + x86_64 universal)
echo "→ Building claudeterm (Rust)..."
cargo build --release 2>&1 | grep -E "Compiling|Finished|error"
CLAUDETERM_BIN="target/release/claudeterm"

# 2. Compile NOU Swift app
echo "→ Compiling NOU.swift..."
swiftc nou/NOU.swift \
    -o nou/NOU_bin \
    -framework Cocoa \
    -framework WebKit \
    -target arm64-apple-macosx13.0 \
    2>&1

# 3. Build .app bundle
APP="nou/NOU.app"
MACOS="$APP/Contents/MacOS"
rm -rf "$APP"
mkdir -p "$MACOS"

cp nou/Info.plist "$APP/Contents/"
cp nou/NOU_bin "$MACOS/NOU"
cp "$CLAUDETERM_BIN" "$MACOS/claudeterm"
chmod +x "$MACOS/NOU" "$MACOS/claudeterm"

echo "→ App bundle: $APP"

# 4. Ad-hoc sign (for local use; user can notarize separately)
codesign --force --deep --sign - "$APP" 2>/dev/null || true

# 5. Create DMG for distribution
echo "→ Creating DMG..."
DMG_DIR=$(mktemp -d)
cp -R "$APP" "$DMG_DIR/"
ln -s /Applications "$DMG_DIR/Applications"

hdiutil create -volname "NOU — Local Claude Terminal" \
    -srcfolder "$DMG_DIR" \
    -ov -format UDZO \
    -o nou/NOU.dmg 2>&1 | tail -3

rm -rf "$DMG_DIR" nou/NOU_bin

echo ""
echo "✓ Done!"
echo "  App:  $(pwd)/$APP"
echo "  DMG:  $(pwd)/nou/NOU.dmg"
echo ""
echo "Install: open nou/NOU.dmg  →  drag NOU to Applications"
