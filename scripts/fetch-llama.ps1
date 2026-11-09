# Puts llama-server and its DLLs for one package artifact into a folder:
#   scripts/fetch-llama.ps1 <artifact> <dest>
# Downloads the official llama.cpp release asset and checks it against the pinned SHA-256 (same table as
# fetch-llama.sh, taken from the release assets' "digest" fields). windows-x64 gets the Vulkan build in <dest> and the
# CPU build in <dest>/cpu, which the app picks on machines without a usable GPU or AVX2.
# LLAMA_TAG overrides the pinned tag; then set LLAMA_SHA256 (and LLAMA_CPU_SHA256 for the CPU build) too, or the
# downloads are used unchecked. macos-x64 is built from source and only fetch-llama.sh does that.
param(
    [Parameter(Mandatory)][string]$Artifact,
    [Parameter(Mandatory)][string]$Destination
)
$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes downloads many times slower on Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'
$pinned = 'b12110'
$tag = if ($env:LLAMA_TAG) { $env:LLAMA_TAG } else { $pinned }
$assets = @{
    'windows-x64' = @("llama-$tag-bin-win-vulkan-x64.zip", '9732a5e48887ca54d0bfe4af4324dc85fd56cd8cda19b52df3542fa6ebe6874f')
    'windows-cpu' = @("llama-$tag-bin-win-cpu-x64.zip", '98901e327c252875accaf2d1bf364e10e868843b0b63468baad1aa945cdc5da2')
    'linux-x64'   = @("llama-$tag-bin-ubuntu-x64.tar.gz", '368227edcc229be5f9bed16a2223d79b5e41d0ac2e19bcf5825e82291c060e55')
    'linux-arm64' = @("llama-$tag-bin-ubuntu-arm64.tar.gz", '4758b51335e37b4346251f2687d64841ff1c07ecb27fc46323945bb2ec9c44d2')
    'macos-arm64' = @("llama-$tag-bin-macos-arm64.tar.gz", '4f5b8de12e0b9516658ca6a5764c6ffb3d7c659c3dd538556b1413c7b4050662')
}
if ($Artifact -eq 'windows-cpu' -or -not $assets.ContainsKey($Artifact)) {
    throw "fetch-llama.ps1: unknown or source-only artifact '$Artifact'"
}

# Downloads one asset, checks it and copies the folder holding llama-server into $Out.
function Get-LlamaAsset([string]$Key, [string]$Out, [string]$OverrideSum) {
    $asset, $sum = $assets[$Key]
    if ($tag -ne $pinned) { $sum = $OverrideSum }
    $work = Join-Path ([IO.Path]::GetTempPath()) ("kyttu-llama-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        $archive = Join-Path $work $asset
        $url = "https://github.com/ggml-org/llama.cpp/releases/download/$tag/$asset"
        for ($try = 1; ; $try++) {
            try { Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing; break }
            catch { if ($try -ge 4) { throw }; Start-Sleep -Seconds (5 * $try) }
        }
        if ($sum) {
            $got = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($got -ne $sum) { throw "$asset has SHA-256 $got, expected $sum" }
        } else {
            Write-Warning "No SHA-256 pinned for $asset; using it unchecked"
        }
        $extracted = Join-Path $work 'x'
        New-Item -ItemType Directory -Path $extracted | Out-Null
        if ($asset.EndsWith('.zip')) {
            Expand-Archive -LiteralPath $archive -DestinationPath $extracted
        } else {
            tar -xzf $archive -C $extracted
            if ($LASTEXITCODE -ne 0) { throw "Could not extract $asset" }
        }
        $server = Get-ChildItem -LiteralPath $extracted -Recurse -File |
            Where-Object { $_.Name -in 'llama-server.exe', 'llama-server' } | Select-Object -First 1
        if (-not $server) { throw "$asset has no llama-server" }
        New-Item -ItemType Directory -Path $Out -Force | Out-Null
        # Every binary and DLL sits next to llama-server.
        Copy-Item -Path (Join-Path $server.DirectoryName '*') -Destination $Out -Recurse -Force
        $license = Get-ChildItem -LiteralPath $extracted -File -Filter 'LICENSE*' -Recurse -Depth 1 | Select-Object -First 1
        if ($license -and -not (Test-Path -LiteralPath (Join-Path $Out 'LICENSE'))) {
            Copy-Item -LiteralPath $license.FullName -Destination (Join-Path $Out 'LICENSE-llama.cpp.txt')
        }
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (Test-Path -LiteralPath $Destination) { Remove-Item -LiteralPath $Destination -Recurse -Force }
Get-LlamaAsset $Artifact $Destination $env:LLAMA_SHA256
if ($Artifact -eq 'windows-x64') {
    Get-LlamaAsset 'windows-cpu' (Join-Path $Destination 'cpu') $env:LLAMA_CPU_SHA256
}
Write-Output "llama.cpp $tag for $Artifact in $Destination"
