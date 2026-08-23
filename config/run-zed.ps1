# run-zed.ps1 - launch the fork against its isolated data dir.
#
#   .\config\run-zed.ps1            # launch the already-built binary (fast, no cargo)
#   .\config\run-zed.ps1 -Build     # build first, then launch
#   .\config\run-zed.ps1 -DataDir 'D:\other'
#
# WHY THIS EXISTS, beyond passing --user-data-dir:
#
# Cargo's fingerprint includes profile settings. The build is done with
# CARGO_PROFILE_DEV_DEBUG=line-tables-only and CARGO_PROFILE_DEV_INCREMENTAL=false,
# so ANY later `cargo run`/`cargo build` in this repo that does not set the same two
# variables looks like a different profile to cargo and triggers a FULL ~48 minute
# rebuild of all 1800+ crates. Always go through this script or build-zed.ps1.
#
# Launching the built binary directly (the default below) sidesteps cargo entirely
# and cannot trigger a rebuild.

[CmdletBinding()]
param(
    [string]$DataDir = 'C:\Users\broad\zed-fork-data',
    [switch]$Build
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$exe  = Join-Path $repo 'target\debug\zed.exe'

# Must match build-zed.ps1 exactly or cargo rebuilds from scratch.
$env:CARGO_PROFILE_DEV_DEBUG = 'line-tables-only'
$env:CARGO_PROFILE_DEV_INCREMENTAL = 'false'

if ($Build) {
    Write-Host "building first (jobs=4, debug=$env:CARGO_PROFILE_DEV_DEBUG)..."
    Push-Location $repo
    try { cargo build -j 4; if ($LASTEXITCODE -ne 0) { throw "build failed ($LASTEXITCODE)" } }
    finally { Pop-Location }
}

if (-not (Test-Path $exe)) {
    throw "No binary at $exe - run .\build-zed.ps1 first, or pass -Build."
}

Write-Host "binary   : $exe"
Write-Host "data dir : $DataDir"
Write-Host "channel  : dev  (db\0-dev - installed Zed uses 0-stable, no overlap)"

Start-Process -FilePath $exe -ArgumentList '--user-data-dir', $DataDir
Write-Host 'launched.'
