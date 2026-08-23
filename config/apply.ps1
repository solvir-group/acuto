# apply.ps1 - copy fork config from the repo into the fork's isolated data dir.
#
# The repo is the source of truth; the data dir is a build artifact.
# Edit files under config/ , run this, and Zed hot-reloads without a restart.
#
#   .\config\apply.ps1                      # default data dir
#   .\config\apply.ps1 -DataDir 'D:\other'  # override
#
# Zed's themes watcher polls the DESTINATION directory, so the copy itself
# is what triggers the reload. Nothing here requires a rebuild.

[CmdletBinding()]
param(
    [string]$DataDir = 'C:\Users\broad\zed-fork-data'
)

$ErrorActionPreference = 'Stop'
$repoConfig = Split-Path -Parent $MyInvocation.MyCommand.Path
$destConfig = Join-Path $DataDir 'config'

Write-Host "source : $repoConfig"
Write-Host "dest   : $destConfig"
Write-Host ''

# themes/*.json -> <data>/config/themes/
$srcThemes = Join-Path $repoConfig 'themes'
if (Test-Path $srcThemes) {
    $destThemes = Join-Path $destConfig 'themes'
    New-Item $destThemes -ItemType Directory -Force | Out-Null
    foreach ($f in Get-ChildItem $srcThemes -Filter *.json) {
        Copy-Item $f.FullName (Join-Path $destThemes $f.Name) -Force
        Write-Host "  theme    -> $($f.Name)"
    }
}

# top-level config files land beside them (settings.json / keymap.json in Phase 3)
foreach ($name in 'settings.json', 'keymap.json') {
    $src = Join-Path $repoConfig $name
    if (Test-Path $src) {
        New-Item $destConfig -ItemType Directory -Force | Out-Null
        Copy-Item $src (Join-Path $destConfig $name) -Force
        Write-Host "  config   -> $name"
    }
}

Write-Host ''
Write-Host 'Done. If Zed is running, the theme reloads on its own.'
Write-Host 'Select it with: cmd palette -> "theme selector: toggle" -> Monochrome Dark'
