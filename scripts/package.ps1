# Builds KYTTU - AI and packages it into dist/: "KYTTU - AI.exe", README.md and llama/ (llama-server and its DLLs).
#   ./scripts/package.ps1                 release build (what release.yml publishes)
#   ./scripts/package.ps1 -CargoProfile ci   the profile build.yml tests with, so its dependencies are reused
#   ./scripts/package.ps1 -Dev            debug build
# KYTTU_PROFILE sets the profile too. KYTTU_SKIP_LLAMA=1 leaves llama-server out (offline local builds);
# KYTTU_LLAMA_DIR points at an existing llama folder, otherwise it is fetched once into .llama/windows-x64.
param([switch]$Dev, [string]$CargoProfile = '')
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $projectRoot
try {
    $buildProfile = if ($Dev) { 'dev' } elseif ($CargoProfile) { $CargoProfile } elseif ($env:KYTTU_PROFILE) { $env:KYTTU_PROFILE } else { 'release' }
    cargo build --locked --profile $buildProfile
    if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
    $outputFolder = Join-Path $projectRoot 'dist'
    if (Test-Path -LiteralPath $outputFolder) { Remove-Item -LiteralPath $outputFolder -Recurse -Force }
    New-Item -ItemType Directory -Path $outputFolder -Force | Out-Null
    $buildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $projectRoot 'target' }
    $profileDir = if ($buildProfile -eq 'dev') { 'debug' } else { $buildProfile }
    Copy-Item -LiteralPath (Join-Path $buildRoot "$profileDir\kyttu-ai.exe") -Destination (Join-Path $outputFolder 'KYTTU - AI.exe') -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination $outputFolder -Force
    if ($env:KYTTU_SKIP_LLAMA -eq '1') {
        Write-Output 'KYTTU_SKIP_LLAMA=1: packaged without llama-server'
    } else {
        $llamaDir = if ($env:KYTTU_LLAMA_DIR) { $env:KYTTU_LLAMA_DIR } else { Join-Path $projectRoot '.llama\windows-x64' }
        if (-not (Test-Path -LiteralPath (Join-Path $llamaDir 'llama-server.exe'))) {
            & (Join-Path $PSScriptRoot 'fetch-llama.ps1') 'windows-x64' $llamaDir
        }
        $llamaOut = Join-Path $outputFolder 'llama'
        New-Item -ItemType Directory -Path $llamaOut -Force | Out-Null
        Copy-Item -Path (Join-Path $llamaDir '*') -Destination $llamaOut -Recurse -Force
    }
    Write-Output "Packaged app ($buildProfile): $outputFolder\KYTTU - AI.exe"
} finally {
    Pop-Location
}
