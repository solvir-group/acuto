# Profile sourced by Acuto's integrated PowerShell terminal.
#
# Loaded via -NoExit -Command, so it applies ONLY inside Acuto. The user's own
# $PROFILE is loaded by PowerShell first and is left alone.

$AcutoBin = 'C:/Users/broad/zed/target/debug/zed.exe'
$AcutoData = 'C:/Users/broad/zed-fork-data'

# Open paths in the running Acuto window rather than launching a second copy.
# The binary routes to the existing instance over its named pipe when the
# user-data-dir matches, so this reuses the window the terminal lives in.
function acuto { & $AcutoBin --user-data-dir $AcutoData @args }

# `code` resolves to VS Code's CLI on this machine, so typing it here opened a
# different editor. A function shadows the external command; `& (Get-Command
# code.cmd).Source` still reaches real VS Code if it is ever wanted.
function code { acuto @args }

# Editors spawned by git, npm and friends should also stay in this window.
$env:EDITOR = "$AcutoBin --user-data-dir $AcutoData --wait"
$env:VISUAL = $env:EDITOR
