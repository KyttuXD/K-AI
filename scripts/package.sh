#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --locked
build_root="${CARGO_TARGET_DIR:-target}"
mkdir -p dist
if [[ "$(uname -s)" == Darwin ]]; then
  app='dist/KYTTU - AI.app/Contents'
  mkdir -p "$app/MacOS"
  cp "$build_root/release/kyttu-ai" "$app/MacOS/kyttu-ai"
  chmod +x "$app/MacOS/kyttu-ai"
  cat > "$app/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>kyttu-ai</string><key>CFBundleName</key><string>KYTTU - AI</string><key>CFBundleIdentifier</key><string>app.kyttu.ai</string><key>CFBundleVersion</key><string>0.2.0</string><key>CFBundlePackageType</key><string>APPL</string><key>NSHighResolutionCapable</key><true/></dict></plist>
PLIST
else
  cp "$build_root/release/kyttu-ai" 'dist/kyttu-ai'
  chmod +x dist/kyttu-ai
fi
cp README.md dist/
printf 'Packaged KYTTU - AI in dist/\n'
