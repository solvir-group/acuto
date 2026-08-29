# Profile sourced by Acuto's integrated PowerShell terminal.
#
# Loaded via -NoExit -Command, so it applies ONLY inside Acuto. The user's own
# $PROFILE is loaded by PowerShell first and is left alone.

$AcutoBin = 'C:/Users/broad/zed/target/debug/zed.exe'
$AcutoData = 'C:/Users/broad/zed-fork-data'

# Turns an argument into something the running Acuto can resolve.
#
# A relative path is resolved by whichever process ends up handling it, and that
# is not this one: the launched binary forwards the request to the instance
# that already owns the data directory, and that instance resolves against its
# own working directory. `code hello.html` therefore opened a file called
# hello.html somewhere else entirely -- in practice a new window on a new
# one-file project, which is what it looked like from here.
#
# Made absolute against the terminal's own directory, which is what the person
# typing it meant. Flags pass through untouched, and so does a path that is
# already absolute.
function Resolve-AcutoArgument([string]$Value) {
    if ([string]::IsNullOrEmpty($Value)) { return $Value }
    if ($Value.StartsWith('-')) { return $Value }
    if ($Value -match '^[a-zA-Z][a-zA-Z0-9+.-]*://') { return $Value }

    # `file.rs:120` and `file.rs:120:8` are how Acuto is told to open at a
    # position. Only the path part is resolved; the trailing numbers are not
    # a path and joining them would produce one that does not exist.
    $suffix = ''
    $path = $Value
    if ($Value -match '^(?<path>.+?)(?<suffix>:\d+(:\d+)?)$') {
        $path = $Matches['path']
        $suffix = $Matches['suffix']
    }

    if ([System.IO.Path]::IsPathRooted($path)) { return $Value }

    # GetFullPath rather than Resolve-Path: the file may not exist yet, and
    # `code newfile.html` has to keep working.
    $full = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine((Get-Location).Path, $path))
    return "$full$suffix"
}

# Open paths in the running Acuto window rather than launching a second copy.
# The binary routes to the existing instance over its named pipe when the
# user-data-dir matches, so this reuses the window the terminal lives in.
function acuto {
    $resolved = @()
    foreach ($item in $args) { $resolved += Resolve-AcutoArgument ([string]$item) }
    & $AcutoBin --user-data-dir $AcutoData @resolved
}

# `code` resolves to VS Code's CLI on this machine, so typing it here opened a
# different editor. A function shadows the external command; `& (Get-Command
# code.cmd).Source` still reaches real VS Code if it is ever wanted.
function code { acuto @args }

# Editors spawned by git, npm and friends should also stay in this window.
# These are handed absolute paths by the tools that set them, so they need no
# resolution of their own.
$env:EDITOR = "$AcutoBin --user-data-dir $AcutoData --wait"
$env:VISUAL = $env:EDITOR
