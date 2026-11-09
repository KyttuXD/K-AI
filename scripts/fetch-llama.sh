#!/usr/bin/env bash
# Puts llama-server and its shared libraries for one package artifact into a folder:
#   scripts/fetch-llama.sh <artifact> <dest>
# artifact: windows-x64, linux-x64, linux-arm64, macos-arm64 (official llama.cpp release assets, checked against the
# SHA-256 pinned from the release assets' "digest" fields), or macos-x64 (built from source here, so it runs on older
# Intel Macs than the official build). windows-x64 gets the Vulkan build in <dest> and the CPU build in <dest>/cpu.
# LLAMA_TAG overrides the pinned tag; then set LLAMA_SHA256 (and LLAMA_CPU_SHA256 for the Windows CPU build) too, or
# the downloads are used unchecked (with a warning).
# LLAMA_MACOS_MIN sets the macos-x64 deployment target (default 10.15, the oldest llama.cpp's C++17 code builds for).
set -euo pipefail
artifact=${1:?usage: fetch-llama.sh <artifact> <dest>}
dest=${2:?usage: fetch-llama.sh <artifact> <dest>}
pinned=b12110
tag=${LLAMA_TAG:-$pinned}
# The commit the pinned tag points to; a source build of another commit fails.
pinned_commit=cd959bae99ff4bed22bef2604f45241c09bbb8c8

# asset_for <artifact>: prints "<asset> <sha256>" (no sum for a source build).
asset_for() {
  case "$1" in
    windows-x64) echo "llama-$tag-bin-win-vulkan-x64.zip 9732a5e48887ca54d0bfe4af4324dc85fd56cd8cda19b52df3542fa6ebe6874f" ;;
    windows-cpu) echo "llama-$tag-bin-win-cpu-x64.zip 98901e327c252875accaf2d1bf364e10e868843b0b63468baad1aa945cdc5da2" ;;
    linux-x64) echo "llama-$tag-bin-ubuntu-x64.tar.gz 368227edcc229be5f9bed16a2223d79b5e41d0ac2e19bcf5825e82291c060e55" ;;
    linux-arm64) echo "llama-$tag-bin-ubuntu-arm64.tar.gz 4758b51335e37b4346251f2687d64841ff1c07ecb27fc46323945bb2ec9c44d2" ;;
    macos-arm64) echo "llama-$tag-bin-macos-arm64.tar.gz 4f5b8de12e0b9516658ca6a5764c6ffb3d7c659c3dd538556b1413c7b4050662" ;;
    macos-x64) echo "" ;;
    *) echo "fetch-llama.sh: unknown artifact '$1'" >&2; return 2 ;;
  esac
}
[[ "$artifact" != windows-cpu ]] || { echo "fetch-llama.sh: windows-cpu comes with windows-x64" >&2; exit 2; }
asset_for "$artifact" > /dev/null

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rm -rf "$dest"
mkdir -p "$dest"

sha256() { if command -v sha256sum > /dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

# fetch <artifact> <out> <override sum>: downloads and checks one release asset, then copies the folder holding
# llama-server into out.
fetch() {
  local asset sum x="$work/x-$1" server license
  read -r asset sum <<< "$(asset_for "$1")"
  [[ "$tag" == "$pinned" ]] || sum=$3
  curl -fsSL --retry 5 --retry-delay 5 -o "$work/$asset" "https://github.com/ggml-org/llama.cpp/releases/download/$tag/$asset"
  if [[ -n "$sum" ]]; then
    got=$(sha256 "$work/$asset")
    [[ "$got" == "$sum" ]] || { echo "::error::$asset has SHA-256 $got, expected $sum" >&2; exit 1; }
  else
    echo "::warning::no SHA-256 pinned for $asset; using it unchecked" >&2
  fi
  mkdir -p "$x" "$2"
  case "$asset" in
    *.zip) unzip -q "$work/$asset" -d "$x" ;;
    *) tar -xzf "$work/$asset" -C "$x" ;;
  esac
  server=$(find "$x" \( -name llama-server -o -name llama-server.exe \) -type f | head -1)
  [[ -n "$server" ]] || { echo "::error::$asset has no llama-server" >&2; exit 1; }
  # Every binary and shared library sits next to llama-server; -a keeps the .so/.dylib version symlinks.
  cp -a "$(dirname "$server")/." "$2/"
  license=$(find "$x" -maxdepth 2 -type f -iname 'LICENSE*' | head -1)
  [[ -z "$license" || -e "$2/LICENSE" ]] || cp "$license" "$2/LICENSE-llama.cpp.txt"
}

if [[ "$artifact" == macos-x64 ]]; then
  src="$work/llama.cpp"
  git clone --quiet --depth 1 --branch "$tag" https://github.com/ggml-org/llama.cpp "$src"
  commit=$(git -C "$src" rev-parse HEAD)
  if [[ "$tag" == "$pinned" && "$commit" != "$pinned_commit" ]]; then
    echo "::error::llama.cpp $tag is $commit, expected $pinned_commit" >&2
    exit 1
  fi
  # CPU only (the app runs Intel Macs on the CPU anyway), with every x86 CPU variant built as a module that ggml picks
  # at start (sandybridge on 2012 Macs, haswell with AVX2/FMA on 2013 and later) instead of one fixed ISA, and no
  # HTTPS or embedded web UI: the app talks to it over localhost. The modules and libggml*/libllama dylibs sit next to
  # llama-server (@loader_path). BLAS/Accelerate are off: ggml's Apple BLAS path selects cblas symbols that exist only
  # on macOS 13.3+, and the CPU backend does the matmuls. package.sh checks every Mach-O file's minimum macOS.
  cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_OSX_ARCHITECTURES=x86_64 -DCMAKE_OSX_DEPLOYMENT_TARGET="${LLAMA_MACOS_MIN:-10.15}" \
    -DCMAKE_INSTALL_RPATH=@loader_path -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
    -DLLAMA_BUILD_NUMBER="${tag#b}" -DLLAMA_BUILD_COMMIT="${commit:0:8}" \
    -DGGML_METAL=OFF -DGGML_BLAS=OFF -DGGML_ACCELERATE=OFF -DGGML_NATIVE=OFF \
    -DBUILD_SHARED_LIBS=ON -DGGML_BACKEND_DL=ON -DGGML_CPU_ALL_VARIANTS=ON \
    -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=ON \
    -DLLAMA_OPENSSL=OFF -DLLAMA_CURL=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF
  # ggml depends on every backend module, so this target builds the CPU variants too.
  cmake --build "$src/build" --config Release --target llama-server --parallel "$(sysctl -n hw.ncpu 2> /dev/null || echo 4)"
  cp -a "$src/build/bin/." "$dest/"
  cp "$src/LICENSE" "$dest/LICENSE-llama.cpp.txt"
  compgen -G "$dest/*ggml-cpu-*" > /dev/null || { echo "::error::no ggml CPU variant modules were built" >&2; exit 1; }
else
  fetch "$artifact" "$dest" "${LLAMA_SHA256:-}"
  [[ "$artifact" != windows-x64 ]] || fetch windows-cpu "$dest/cpu" "${LLAMA_CPU_SHA256:-}"
fi
chmod +x "$dest"/llama-server* 2> /dev/null || true
printf 'llama.cpp %s for %s in %s\n' "$tag" "$artifact" "$dest"
