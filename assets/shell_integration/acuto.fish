# Acuto shell integration for fish.
#
# Emits OSC 133 semantic prompt markers and OSC 7 working-directory reports.
# See acuto.zsh for the rationale.
#
# fish has first-class events for exactly this, so there are no hooks to wrap
# and nothing to be careful about clobbering. Loaded from vendor_conf.d via
# XDG_DATA_DIRS, which fish sources automatically without the user's config
# being touched.

if set --query ACUTO_SHELL_INTEGRATION_LOADED
    exit 0
end
set --global ACUTO_SHELL_INTEGRATION_LOADED 1

if test "$TERM_PROGRAM" != "Acuto"
    exit 0
end

function __acuto_osc --argument-names payload
    printf '\033]%s\007' $payload
end

function __acuto_report_cwd
    # fish's string escape --style=url percent-encodes exactly the set OSC 7
    # needs, so there is no hand-rolled encoder here.
    set --local encoded (string escape --style=url -- $PWD)
    # The URL style escapes '/' as well, which would flatten the path into a
    # single component; put the separators back.
    set encoded (string replace --all '%2F' '/' -- $encoded)
    __acuto_osc "7;file://$hostname$encoded"
end

function __acuto_prompt_start --on-event fish_prompt
    __acuto_report_cwd
    __acuto_osc "133;A"
end

function __acuto_preexec --on-event fish_preexec
    __acuto_osc "133;C"
end

function __acuto_postexec --on-event fish_postexec
    # $status must be read before anything else runs.
    __acuto_osc "133;D;$status"
end

# The B marker belongs at the end of the prompt, where input begins. fish has
# no PS1 to append to, so fish_prompt is wrapped: the user's own prompt
# function is renamed and called, then the marker is emitted after it.
if functions --query fish_prompt; and not functions --query __acuto_original_fish_prompt
    functions --copy fish_prompt __acuto_original_fish_prompt

    function fish_prompt
        __acuto_original_fish_prompt
        __acuto_osc "133;B"
    end
end
