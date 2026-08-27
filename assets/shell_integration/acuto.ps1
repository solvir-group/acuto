# Acuto shell integration for PowerShell.
#
# Emits OSC 133 semantic prompt markers and OSC 7 working-directory reports.
# See acuto.zsh for the rationale.
#
# Passed via `-EncodedCommand` rather than written to a .ps1 and dot-sourced.
# ExecutionPolicy governs script *files*, so a restrictive policy — the default
# on Windows for anything not signed — would block a file but not an inline
# command. Windows is a first-class target here, so the integration cannot be
# the one thing that silently fails on it.
#
# Encoded rather than passed as a plain `-Command` argument because PowerShell
# re-parses that argument and strips the script's own double quotes, leaving
# something that cannot parse at all.
#
# Hard rule: this must never break the shell. The whole body is wrapped so that
# any failure leaves the user with a working prompt and no integration, rather
# than a broken prompt.

try {
    if ($env:ACUTO_SHELL_INTEGRATION_LOADED) { return }
    $env:ACUTO_SHELL_INTEGRATION_LOADED = '1'

    if ($env:TERM_PROGRAM -ne 'Acuto') { return }

    function global:__Acuto-Osc([string] $Payload) {
        # [char]27 and [char]7 rather than escape sequences: `e is PowerShell
        # 6+ only, and Windows PowerShell 5.1 is still the default shell on
        # plenty of machines.
        [Console]::Write("$([char]27)]$Payload$([char]7)")
    }

    function global:__Acuto-ReportCwd {
        $path = (Get-Location).Path
        # Only report filesystem locations. PowerShell providers make
        # `Set-Location HKLM:\` legal, and reporting that as a file:// URL
        # would point the editor at a directory that does not exist.
        if ((Get-Location).Provider.Name -ne 'FileSystem') { return }

        $encoded = [System.Uri]::EscapeDataString($path)
        # EscapeDataString encodes the separators too, which would flatten the
        # path; put them back and normalise to forward slashes for the URL.
        $encoded = $encoded -replace '%5C', '/' -replace '%2F', '/'
        __Acuto-Osc "7;file:///$($encoded.TrimStart('/'))"
    }

    # The previous command's success is captured before anything else runs.
    # PowerShell has no exit code for native failures beyond $LASTEXITCODE, and
    # $? for cmdlets, so both are consulted.
    function global:__Acuto-LastExitCode {
        if ($null -ne $global:LASTEXITCODE -and $global:LASTEXITCODE -ne 0) {
            return $global:LASTEXITCODE
        }
        if ($? -eq $false) { return 1 }
        return 0
    }

    # Wrap the user's existing prompt rather than replacing it, so themes such
    # as oh-my-posh or starship keep working.
    if (-not (Test-Path function:global:__Acuto-OriginalPrompt)) {
        if (Test-Path function:global:prompt) {
            Rename-Item function:global:prompt global:__Acuto-OriginalPrompt
        } else {
            function global:__Acuto-OriginalPrompt { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) " }
        }
    }

    function global:prompt {
        $exitCode = __Acuto-LastExitCode

        # Cheap and idempotent; PSReadLine may have replaced our handler since
        # the last prompt.
        __Acuto-ArmEnterHandler

        # D before A: the previous command finished, then a new prompt starts.
        #
        # Emitted unconditionally rather than gated on having seen a command
        # start. The only pre-execution hook PowerShell offers is a PSReadLine
        # key handler, and that does not fire reliably — over a pty it does not
        # fire at all. Hanging the entire feature off it meant no command was
        # ever recorded. The editor ignores a D it has no command for, so a
        # spurious one at the first prompt costs nothing.
        __Acuto-Osc "133;D;$exitCode"
        $env:ACUTO_COMMAND_RUNNING = $null

        __Acuto-ReportCwd
        __Acuto-Osc "133;A"

        $rendered = ''
        try {
            $rendered = __Acuto-OriginalPrompt
        } catch {
            # A broken user prompt must not take the integration down with it,
            # and vice versa.
            $rendered = "PS $((Get-Location).Path)> "
        }

        # B marks where input begins, so it goes after everything the prompt
        # prints.
        "$rendered$([char]27)]133;B$([char]7)"
    }

    # Best-effort only. The C marker tells the editor a command is running so
    # it can suppress completions, which is worth having — but nothing depends
    # on it: the command itself is recovered from the D marker and the line the
    # editor already watched the user type. PSReadLine is optional, its key
    # handlers are replaced when it initialises, and over a pty this does not
    # fire at all.
    function global:__Acuto-ArmEnterHandler {
        try {
            $existing = Get-PSReadLineKeyHandler -Bound |
                Where-Object { $_.Key -eq 'Enter' -and $_.Function -eq 'AcutoAcceptLine' }
            if ($existing) { return }

            Set-PSReadLineKeyHandler -Key Enter `
                -BriefDescription 'AcutoAcceptLine' `
                -Description 'Reports command start to Acuto, then accepts the line.' `
                -ScriptBlock {
                    param($key, $arg)
                    $env:ACUTO_COMMAND_RUNNING = '1'
                    [Console]::Write("$([char]27)]133;C$([char]7)")
                    [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
                }
        } catch {
            # No PSReadLine, or a version without these cmdlets. The prompt
            # markers still work; only command timing is lost.
        }
    }
} catch {
    # Deliberately silent. A shell that prints an integration error on every
    # start is worse than one with no integration.
}
