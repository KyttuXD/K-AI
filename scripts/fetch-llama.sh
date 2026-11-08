#!/usr/bin/env bash
# Puts llama-server and its shared libraries for one package artifact into a folder:
#   scripts/fetch-llama.sh <artifact> <dest>
# artifact: windows-x64, linux-x64, linux-arm64, macos-arm64 (official llama.cpp release assets, checked against the
# pinned SHA-256), or macos-x64 (built from source here, so it runs on older Intel Macs than the official build).
# LLAMA_TAG overrides the pinned tag; then set LLAMA_SHA256 too, or the download is used unchecked (with a warning).
# LLAMA_MACOS_MIN sets the macos-x64 deployment target (default 10.15, the oldest llama.cpp's C++17 code builds for).
set -euo pipefail
artifact=${1:?usage: fetch-llama.sh <artifact> <dest>}
dest=${2:?usage: fetch-llama.sh <artifact> <dest>}
pinned=b12110
tag=${LLAMA_TAG:-$pinned}
# The commit the pinned tag points to; a source build of another commit fails.
pinned_commit=cd959bae99ff4bed22bef2604f45241c09bbb8c8

case "$artifact" in
  windows-x64) asset="llama-$tag-bin-win-vulkan-x64.zip" sum=9732a5e48887ca54d0bfe4af4324dc85fd56cd8cda19b52df3542fa6ebe6874f ;;
  linux-x64) asset="llama-$tag-bin-ubuntu-x64.tar.gz" sum=368227edcc229be5f9bed16a2223d79b5e41d0ac2e19bcf5825e82291c060e55 ;;
  linux-arm64) asset="llama-$tag-bin-ubuntu-arm64.tar.gz" sum=4758b51335e37b4346251f2687d64841ff1c07ecb27fc46323945bb2ec9c44d2 ;;
  macos-arm64) asset="llama-$tag-bin-macos-arm64.tar.gz" sum=4f5b8de12e0b9516658ca6a5764c6ffb3d7c659c3dd538556b1413c7b4050662 ;;
  macos-x64) asset='' sum='' ;;
  *) echo "fetch-llama.sh: unknown artifact '$artifact'" >&2; exit 2 ;;
esac
[[ "$tag" == "$pinned" ]] || sum=${LLAMA_SHA256:-}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rm -rf "$dest"
mkdir -p "$dest"

sha256() { if command -v sha256sum > /dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

if [[ "$artifact" == macos-x64 ]]; then
  src="$work/llama.cpp"
  git clone --quiet --depth 1 --branch "$tag" https://github.com/ggml-org/llama.cpp "$src"
  commit=$(git -C "$src" rev-parse HEAD)
  if [[ "$tag" == "$pinned" && "$commit" != "$pinned_commit" ]]; then
    echo "::error::llama.cpp $tag is $commit, expected $pinned_commit" >&2
    exit 1
  fi
  # CPU only (the app runs Intel Macs on the CPU anyway), fixed Ivy Bridge-class x86 flags (SSE4.2, AVX, F16C: every
  # Mac that runs 10.15 has them; AVX2/FMA/BMI2 would SIGILL on 2012-2013 Macs) instead of the build machine's, one
  # static binary, and no HTTPS or embedded web UI: the app talks to it over localhost. BLAS/Accelerate are off: ggml's
  # Apple BLAS path selects cblas symbols that exist only on macOS 13.3+, and the CPU backend does the matmuls.
  cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES=x86_64 -DCMAKE_OSX_DEPLOYMENT_TARGET="${LLAMA_MACOS_MIN:-10.15}" \
    -DLLAMA_BUILD_NUMBER="${tag#b}" -DLLAMA_BUILD_COMMIT="${commit:0:8}" \
    -DGGML_METAL=OFF -DGGML_BLAS=OFF -DGGML_ACCELERATE=OFF -DGGML_NATIVE=OFF -DBUILD_SHARED_LIBS=OFF \
    -DGGML_SSE42=ON -DGGML_AVX=ON -DGGML_AVX2=OFF -DGGML_FMA=OFF -DGGML_F16C=ON -DGGML_BMI2=OFF \
    -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=ON \
    -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF
  cmake --build "$src/build" --config Release --target llama-server --parallel "$(sysctl -n hw.ncpu 2> /dev/null || echo 4)"
  cp "$src/build/bin/llama-server" "$dest/"
  cp "$src/LICENSE" "$dest/LICENSE-llama.cpp.txt"
else
  curl -fsSL --retry 5 --retry-delay 5 -o "$work/$asset" "https://github.com/ggml-org/llama.cpp/releases/download/$tag/$asset"
  if [[ -n "$sum" ]]; then
    got=$(sha256 "$work/$asset")
    [[ "$got" == "$sum" ]] || { echo "::error::$asset has SHA-256 $got, expected $sum" >&2; exit 1; }
  else
    echo "::warning::no SHA-256 pinned for $asset; using it unchecked" >&2
  fi
  mkdir "$work/x"
  case "$asset" in
    *.zip) unzip -q "$work/$asset" -d "$work/x" ;;
    *) tar -xzf "$work/$asset" -C "$work/x" ;;
  esac
  server=$(find "$work/x" \( -name llama-server -o -name llama-server.exe \) -type f | head -1)
  [[ -n "$server" ]] || { echo "::error::$asset has no llama-server" >&2; exit 1; }
  # Every binary and shared library sits next to llama-server; -a keeps the .so/.dylib version symlinks.
  cp -a "$(dirname "$server")/." "$dest/"
  license=$(find "$work/x" -maxdepth 2 -type f -iname 'LICENSE*' | head -1)
  [[ -z "$license" || -e "$dest/LICENSE" ]] || cp "$license" "$dest/LICENSE-llama.cpp.txt"
fi
chmod +x "$dest"/llama-server* 2> /dev/null || true
printf 'llama.cpp %s for %s in %s\n' "$tag" "$artifact" "$dest"
