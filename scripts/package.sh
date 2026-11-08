#!/usr/bin/env bash
# Builds KYTTU - AI and packages it into dist/ with llama-server bundled:
#   Linux: dist/kyttu-ai, dist/README.md, dist/llama/
#   macOS: dist/KYTTU - AI.app (llama-server in Contents/Resources/llama), dist/README.md
# KYTTU_PROFILE: cargo profile (default release; build.yml uses ci so the test build's dependencies are reused).
# KYTTU_ARTIFACT: linux-x64, linux-arm64, macos-x64 or macos-arm64 (default: this machine's).
# KYTTU_SKIP_LLAMA=1 leaves llama-server out; KYTTU_LLAMA_DIR points at an existing llama folder, otherwise it is
# fetched once into .llama/<artifact> (scripts/fetch-llama.sh).
# MACOSX_DEPLOYMENT_TARGET defaults to 10.13 on Intel and 11.0 on Apple Silicon.
#   scripts/package.sh --check-minos   only checks the minimum macOS versions of an existing dist/ (CI runs it)
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)-$(uname -m)" in
  Darwin-x86_64) detected=macos-x64 ;;
  Darwin-arm64) detected=macos-arm64 ;;
  Linux-aarch64 | Linux-arm64) detected=linux-arm64 ;;
  *) detected=linux-x64 ;;
esac
artifact=${KYTTU_ARTIFACT:-$detected}
app='dist/KYTTU - AI.app/Contents'
# The official Apple Silicon llama-server sets its own minimum, so only the source-built Intel one is held to a limit.
if [[ "$artifact" == macos-x64 ]]; then app_min=10.13 llama_min=10.15; else app_min=11.0 llama_min=''; fi
[[ "$artifact" != macos-* ]] || export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-$app_min}

# The minimum macOS a Mach-O file loads on: LC_BUILD_VERSION's minos, or LC_VERSION_MIN_MACOSX's version, which the
# linker writes instead for targets below 10.14.
minos() {
  otool -l "$1" | awk '$1 == "cmd" { cmd = $2 }
    cmd == "LC_BUILD_VERSION" && $1 == "minos" { print $2; exit }
    cmd == "LC_VERSION_MIN_MACOSX" && $1 == "version" { print $2; exit }'
}
# version_le a b: a <= b, comparing up to three dotted numbers.
version_le() {
  awk -v a="$1" -v b="$2" 'BEGIN { split(a, x, "."); split(b, y, ".")
    for (i = 1; i <= 3; i++) { if (x[i] + 0 < y[i] + 0) exit 0; if (x[i] + 0 > y[i] + 0) exit 1 } exit 0 }'
}
# check_minos <file> <max>: fails when the file needs a newer macOS than max; with no max it only reports.
check_minos() {
  local got
  got=$(minos "$1")
  [[ -n "$got" ]] || { echo "::error::no minimum macOS version in $1"; return 1; }
  if [[ -z "$2" ]]; then
    echo "$1: macOS $got or later"
  elif version_le "$got" "$2"; then
    echo "$1: macOS $got or later (at most $2 allowed)"
  else
    echo "::error::$1 needs macOS $got, above $2"
    return 1
  fi
}
check_dist_minos() {
  check_minos "$app/MacOS/kyttu-ai" "$MACOSX_DEPLOYMENT_TARGET"
  local f
  [[ -d "$app/Resources/llama" ]] || return 0
  while IFS= read -r f; do
    if file -b "$f" | grep -q Mach-O; then check_minos "$f" "$llama_min"; fi
  done < <(find "$app/Resources/llama" -type f)
}
if [[ "${1:-}" == --check-minos ]]; then
  check_dist_minos
  exit
fi

profile=${KYTTU_PROFILE:-release}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cargo build --locked --profile "$profile"
out_dir=$profile
[[ "$profile" != dev ]] || out_dir=debug
bin="${CARGO_TARGET_DIR:-target}/$out_dir/kyttu-ai"

llama=''
if [[ "${KYTTU_SKIP_LLAMA:-}" != 1 ]]; then
  llama=${KYTTU_LLAMA_DIR:-.llama/$artifact}
  [[ -x "$llama/llama-server" ]] || bash scripts/fetch-llama.sh "$artifact" "$llama"
fi

rm -rf dist
mkdir -p dist
if [[ "$artifact" == macos-* ]]; then
  mkdir -p "$app/MacOS" "$app/Resources"
  cp "$bin" "$app/MacOS/kyttu-ai"
  chmod +x "$app/MacOS/kyttu-ai"
  [[ -z "$llama" ]] || cp -a "$llama" "$app/Resources/llama"
  cat > "$app/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>kyttu-ai</string>
  <key>CFBundleName</key><string>KYTTU - AI</string>
  <key>CFBundleDisplayName</key><string>KYTTU - AI</string>
  <key>CFBundleIdentifier</key><string>app.kyttu.ai</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  # Ad hoc signature over the whole bundle, so Apple Silicon accepts the copied binaries and libraries as one app.
  codesign --force --deep --sign - "dist/KYTTU - AI.app" || echo '::warning::ad hoc signing failed'
  check_dist_minos
else
  cp "$bin" dist/kyttu-ai
  chmod +x dist/kyttu-ai
  [[ -z "$llama" ]] || cp -a "$llama" dist/llama
fi
cp README.md dist/
printf 'Packaged KYTTU - AI %s (%s, %s) in dist/\n' "$version" "$artifact" "$profile"
