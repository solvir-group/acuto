# Acuto shell integration for bash.
#
# Emits OSC 133 semantic prompt markers and OSC 7 working-directory reports.
# See acuto.zsh for the rationale; the difference here is mechanical.
#
# bash has no preexec hook, so the DEBUG trap stands in for one. That trap fires
# before *every* command in a list, so it is gated to fire once per prompt.
#
# Loaded through `--init-file`, which sources the user's own rc first. The
# user's rc files are never modified.

if [[ -n "${ACUTO_SHELL_INTEGRATION_LOADED:-}" ]]; then
  return 0
fi
ACUTO_SHELL_INTEGRATION_LOADED=1

if [[ "${TERM_PROGRAM:-}" != "Acuto" ]]; then
  return 0
fi

__acuto_osc() {
  printf '\033]%s\007' "$1"
}

__acuto_report_cwd() {
  local encoded=""
  local i char
  for (( i = 0; i < ${#PWD}; i++ )); do
    char="${PWD:i:1}"
    case "$char" in
      [a-zA-Z0-9/._~-]) encoded+="$char" ;;
      *) encoded+=$(printf '%%%02X' "'$char") ;;
    esac
  done
  __acuto_osc "7;file://${HOSTNAME:-}${encoded}"
}

__acuto_preexec() {
  # The DEBUG trap also fires for the PROMPT_COMMAND itself and for each
  # command in a compound list. Without this gate a single `a && b` would
  # report two commands starting and the phase would desynchronise.
  if [[ -n "${__acuto_command_running:-}" ]]; then
    return
  fi
  # $BASH_COMMAND during PROMPT_COMMAND is the prompt machinery, not the user's
  # command; skipping it keeps the marker aligned with what actually ran.
  if [[ "${BASH_COMMAND:-}" == "${PROMPT_COMMAND:-}" ]]; then
    return
  fi
  __acuto_command_running=1
  __acuto_osc "133;C"
}

__acuto_precmd() {
  local exit_code=$?
  if [[ -n "${__acuto_command_running:-}" ]]; then
    __acuto_osc "133;D;${exit_code}"
    unset __acuto_command_running
  fi
  __acuto_report_cwd
  __acuto_osc "133;A"
  # Returning the original status keeps `$?` meaningful for anything else the
  # user has chained into PROMPT_COMMAND.
  return $exit_code
}

trap '__acuto_preexec' DEBUG

# Prepended rather than assigned, so a PROMPT_COMMAND the user already set
# still runs. bash 5.1+ supports an array; older versions need the string form.
if [[ -n "${PROMPT_COMMAND:-}" ]]; then
  PROMPT_COMMAND="__acuto_precmd;${PROMPT_COMMAND}"
else
  PROMPT_COMMAND="__acuto_precmd"
fi

# \[ \] wrap the sequence as zero-width, so bash's line editor does not count
# it toward the prompt length and wrap lines in the wrong place.
PS1="${PS1}\[$(__acuto_osc "133;B")\]"

return 0
