# Shell startup file for terminals opened inside Acuto.
#
# Loaded via `bash --rcfile`, so it applies ONLY to Acuto's integrated terminal.
# Your global ~/.bashrc is sourced first and is otherwise left alone, so nothing
# here leaks into shells started outside the editor.

# Keep the user's own configuration.
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"

ACUTO_BIN="C:/Users/broad/zed/target/debug/zed.exe"
ACUTO_DATA_DIR="C:/Users/broad/zed-fork-data"

# Open paths in the running Acuto window rather than launching a second copy.
# The binary routes to the existing instance over its named pipe when the
# user-data-dir matches, so this reuses the window the terminal lives in.
acuto() {
    "$ACUTO_BIN" --user-data-dir "$ACUTO_DATA_DIR" "$@"
}

# `code` resolves to VS Code's CLI on this machine, so typing it in Acuto's
# terminal opened a different editor. Shadow it. `command code` still reaches
# the real VS Code if it is ever wanted.
code() {
    acuto "$@"
}

# Editors spawned by git, npm and friends should also stay in this window.
export EDITOR="$ACUTO_BIN --user-data-dir $ACUTO_DATA_DIR --wait"
export VISUAL="$EDITOR"
