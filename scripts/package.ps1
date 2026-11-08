param([switch]$Dev)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $projectRoot
try {
    if ($Dev) { cargo build --locked } else { cargo build --release --locked }
    if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }
    $outputFolder = Join-Path $projectRoot 'dist'
    New-Item -ItemType Directory -Path $outputFolder -Force | Out-Null
    $buildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $projectRoot 'target' }
    $profileName = if ($Dev) { 'debug' } else { 'release' }
    Copy-Item -LiteralPath (Join-Path $buildRoot "$profileName\kyttu-ai.exe") -Destination (Join-Path $outputFolder 'KYTTU - AI.exe') -Force
    Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination $outputFolder -Force
    Write-Output "Packaged app: $outputFolder\KYTTU - AI.exe"
} finally {
    Pop-Location
}
