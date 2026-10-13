#!/bin/sh
# Warp's tmux plugin (KTD8).
#
# Makes every shell tmux starts without Warp's help Warp-ready: panes a plain client splits,
# windows a script opens, and panes tmux-resurrect restores. It sets tmux's `default-command` to
# Warp's pane launcher, which starts the shell with Warp's shell integration in marker-only mode:
# Warp's hooks are DCS/OSC sequences that tmux drops for plain clients, and the prompt and line
# editor are left alone. When Warp attaches, it verifies each pane's session id and shows blocks.
#
# Load it from ~/.tmux.conf (or as a TPM plugin; TPM runs `*.tmux` files with no arguments):
#
#   run-shell '~/.cache/warp/tmux/current/warp.tmux 2>/dev/null || true'
#
# Warp installs these files on the tmux server's host each time it attaches, locally or over SSH,
# and points `current` at the newest install. Without an install the plugin does nothing.
#
# `warp.tmux unload` restores the previous `default-command` and PATH.

warp_cache="$HOME/.cache/warp/tmux"

# The server's global PATH (or this job's, which tmux inherited), without any Warp wrapper
# directory. Mirrors `path_without_wrapper` in Warp.
warp_path_without_wrapper() {
  warp_path=$(tmux show-environment -g PATH 2>/dev/null)
  case $warp_path in
    PATH=*) warp_path=${warp_path#PATH=} ;;
    *) warp_path=$PATH ;;
  esac
  warp_rest=""
  warp_ifs=$IFS
  set -f
  IFS=:
  for warp_entry in $warp_path; do
    case $warp_entry in
      */.cache/warp/tmux/*/bin | */.cache/warp/tmux/*/bin/) continue ;;
    esac
    warp_rest="$warp_rest${warp_rest:+:}$warp_entry"
  done
  IFS=$warp_ifs
  set +f
  printf '%s' "$warp_rest"
}

warp_unload() {
  [ -n "$(tmux show-options -gqv @warp-plugin)" ] || return 0
  warp_saved=$(tmux show-options -gqv @warp-saved-default-command)
  if [ -n "$warp_saved" ]; then
    tmux set-option -g default-command "$warp_saved"
  else
    tmux set-option -gu default-command
  fi
  tmux set-option -gu @warp-saved-default-command
  tmux set-option -gu @warp-plugin
  tmux set-environment -g PATH "$(warp_path_without_wrapper)"
}

warp_load() {
  # The launcher at a path that stays the same across Warp versions; the copy beside this file
  # when it was copied elsewhere by hand.
  warp_dir="$warp_cache/current"
  if [ ! -x "$warp_dir/warp-shell.sh" ]; then
    warp_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" 2>/dev/null && pwd) || return 0
  fi
  warp_launcher="$warp_dir/warp-shell.sh"
  [ -x "$warp_launcher" ] || return 0
  case $warp_launcher in
    *\'* | *\"* | *\\* | *\$* | *\`*) return 0 ;;
  esac

  # tmux runs default-command with `default-shell -c`, so this is valid sh, bash, zsh and fish.
  # If the launcher is removed later, panes still get a login shell.
  warp_command="test -x '$warp_launcher' && exec '$warp_launcher'; exec \"\$SHELL\" -l"

  # Keep the user's own default-command for unload; never save Warp's (a second load).
  warp_current=$(tmux show-options -gqv default-command)
  case $warp_current in
    *warp-shell.sh*) ;;
    *) tmux set-option -g @warp-saved-default-command "$warp_current" ;;
  esac
  tmux set-option -g default-command "$warp_command"

  # The server's token, which session ids are derived from. Warp keeps an existing one.
  if [ -z "$(tmux show-options -gqv @warp-hook-token)" ]; then
    warp_token=$(od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
    [ -n "$warp_token" ] && tmux set-option -g @warp-hook-token "$warp_token"
  fi

  # KTD14: Warp's `tmux` wrapper first on PATH, so popups scripts open reach Warp.
  if [ -x "$warp_dir/bin/tmux" ]; then
    warp_rest=$(warp_path_without_wrapper)
    tmux set-environment -g PATH "$warp_dir/bin${warp_rest:+:}$warp_rest"
  fi

  tmux set-option -g @warp-plugin 1
}

case ${1:-load} in
  unload) warp_unload ;;
  *) warp_load ;;
esac
exit 0
