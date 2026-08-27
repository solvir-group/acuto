# Acuto shell integration for zsh.
#
# Emits OSC 133 semantic prompt markers and OSC 7 working-directory reports so
# the editor knows where a prompt begins, when a command is running, and where
# the shell currently is. Without this the editor has to guess, and a prompt
# detector that guesses is wrong constantly.
#
# Loaded through a ZDOTDIR shim, which sources the user's own .zshrc first and
# then restores ZDOTDIR so nothing downstream sees the shim. The user's rc files
# are never modified.
#
# Hard rule: this must never break the shell. Every hook is defensive, and the
# whole file bails out early rather than half-installing.

# Never install twice — a nested shell, or a user who also sources this
# manually, would otherwise emit duplicate markers and double-count history.
if [[ -n "${ACUTO_SHELL_INTEGRATION_LOADED:-}" ]]; then
  return 0
fi
ACUTO_SHELL_INTEGRATION_LOADED=1

# Only meaningful when the terminal is actually Acuto. A user copying this into
# another terminal gets a no-op rather than stray escape sequences painted on
# their screen.
if [[ "${TERM_PROGRAM:-}" != "Acuto" ]]; then
  return 0
fi

__acuto_osc() {
  # printf rather than echo: echo's handling of backslashes varies by shell
  # option, and a mangled escape sequence prints garbage into the prompt.
  builtin printf '\033]%s\007' "$1"
}

__acuto_report_cwd() {
  # OSC 7 wants a file:// URL. Percent-encode the bytes that would otherwise
  # terminate or re-interpret the URL; leaving them raw turns a directory with
  # a space or a '%' in its name into an unparsable report.
  local encoded=""
  local i char
  for (( i = 1; i <= ${#PWD}; i++ )); do
    char="${PWD[i]}"
    case "$char" in
      [a-zA-Z0-9/._~-]) encoded+="$char" ;;
      *) encoded+=$(builtin printf '%%%02X' "'$char") ;;
    esac
  done
  __acuto_osc "7;file://${HOST:-}${encoded}"
}

__acuto_prompt_start() {
  __acuto_osc "133;A"
}

__acuto_prompt_end() {
  __acuto_osc "133;B"
}

__acuto_preexec() {
  __acuto_osc "133;C"
}

__acuto_precmd() {
  # Captured first: anything else here would overwrite $?.
  local exit_code=$?
  # Suppressed for the very first prompt, where no command has run yet and
  # reporting an exit code would invent a result for nothing.
  if [[ -n "${__acuto_command_running:-}" ]]; then
    __acuto_osc "133;D;${exit_code}"
    unset __acuto_command_running
  fi
  __acuto_report_cwd
  __acuto_prompt_start
}

__acuto_preexec_wrapper() {
  __acuto_command_running=1
  __acuto_preexec
}

# add-zsh-hook is the supported way to attach without clobbering hooks a user
# or their framework already installed. If it is unavailable the integration
# declines to install rather than overwriting precmd/preexec by hand.
if autoload -Uz add-zsh-hook 2>/dev/null; then
  add-zsh-hook precmd __acuto_precmd
  add-zsh-hook preexec __acuto_preexec_wrapper
else
  return 0
fi

# The B marker must land at the end of the prompt, which is where the user's
# input begins. Appending to PS1 rather than replacing it keeps whatever theme
# the user already has.
PS1="${PS1}%{$(__acuto_prompt_end)%}"

return 0
