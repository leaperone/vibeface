#!/usr/bin/env bash
# Build Vibeface.app with swiftc (works with Xcode CLT; CI uses the same script).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="$ROOT/apps/macos"
SRC="$APP_DIR/Sources/VibefaceMac"
OUT_ROOT="${1:-$ROOT/.artifacts/macos}"
BUILD_CFG="${BUILD_CFG:-release}"
PRODUCT="Vibeface.app"
ICON_SRC="$SRC/Resources/AppIcon.png"
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT

OPT=(-O)
[[ "$BUILD_CFG" == "debug" ]] && OPT=()

swiftc -parse-as-library "${OPT[@]}" -o "$STAGING/VibefaceMac" \
  "$SRC/VibefaceMacApp.swift" \
  "$SRC/NodeSettingsStore.swift" \
  "$SRC/RootView.swift" \
  "$SRC/NodeWebView.swift" \
  -framework SwiftUI -framework WebKit -framework AppKit

rm -rf "$OUT_ROOT/$PRODUCT"
mkdir -p "$OUT_ROOT/$PRODUCT/Contents/MacOS" "$OUT_ROOT/$PRODUCT/Contents/Resources"

cp "$STAGING/VibefaceMac" "$OUT_ROOT/$PRODUCT/Contents/MacOS/Vibeface"
chmod +x "$OUT_ROOT/$PRODUCT/Contents/MacOS/Vibeface"

if [[ -f "$ICON_SRC" ]]; then
  mkdir -p "$OUT_ROOT/$PRODUCT/Contents/Resources/AppIcon.iconset"
  for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$ICON_SRC" --out "$OUT_ROOT/$PRODUCT/Contents/Resources/AppIcon.iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$ICON_SRC" --out "$OUT_ROOT/$PRODUCT/Contents/Resources/AppIcon.iconset/icon_${size}x${size}@2x.png" >/dev/null
  done
  iconutil -c icns "$OUT_ROOT/$PRODUCT/Contents/Resources/AppIcon.iconset" -o "$OUT_ROOT/$PRODUCT/Contents/Resources/AppIcon.icns"
  rm -rf "$OUT_ROOT/$PRODUCT/Contents/Resources/AppIcon.iconset"
fi

cat >"$OUT_ROOT/$PRODUCT/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleExecutable</key>
	<string>Vibeface</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIdentifier</key>
	<string>one.leaper.vibeface.macos</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>Vibeface</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.0.1</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
PLIST

echo "Built $OUT_ROOT/$PRODUCT"
