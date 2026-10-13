#!/bin/sh
# Warp's tmux pane launcher (marker-only shell integration).
#
# Warp starts the panes it creates with this script, and the tmux plugin can set it as
# `default-command`. It starts the pane's shell with Warp's shell integration sourced from the
# files installed beside it, so Warp never types anything into the pane. The integration only
# emits Warp's DCS/OSC hooks, which tmux drops for plain clients; the user's prompt and line
# editor are left alone.
#
# The session id Warp trusts is a keyed hash of the tmux server's `@warp-hook-token` and the pane
# id. It is recorded in the `@warp_session_id` and `@warp_shell` pane options so a Warp that
# attaches later can verify it. Without a token or a supported shell, the pane gets a plain shell.

warp_shell=$(tmux show-options -gqv default-shell 2>/dev/null)
[ -n "$warp_shell" ] || warp_shell=${SHELL:-/bin/sh}
warp_name=${warp_shell##*/}
warp_name=${warp_name#-}

case $warp_name in
  zsh | bash | fish) ;;
  *) exec "$warp_shell" ;;
esac

warp_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P) || exec "$warp_shell" -l
warp_token=$(tmux show-options -gqv @warp-hook-token 2>/dev/null)
if [ -z "$warp_token" ]; then
  # A session Warp started before attaching: create the server's token, as Warp would.
  warp_token=$(od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
  [ -n "$warp_token" ] && tmux set-option -g @warp-hook-token "$warp_token" 2>/dev/null
  # Another client may have set one first; use what the server kept.
  warp_token=$(tmux show-options -gqv @warp-hook-token 2>/dev/null)
fi
if [ -z "$warp_token" ] || [ -z "${TMUX_PANE:-}" ]; then
  exec "$warp_shell" -l
fi

warp_hash=$(printf '%s:%s' "$warp_token" "$TMUX_PANE" | {
  sha256sum 2>/dev/null || shasum -a 256 2>/dev/null || openssl dgst -sha256 -r 2>/dev/null
} | cut -c1-15)
case $warp_hash in
  *[!0-9a-f]* | '') exec "$warp_shell" -l ;;
esac
[ ${#warp_hash} -eq 15 ] || exec "$warp_shell" -l

WARP_SESSION_ID=$((0x$warp_hash))
tmux set-option -p -t "$TMUX_PANE" @warp_session_id "$WARP_SESSION_ID" 2>/dev/null
tmux set-option -p -t "$TMUX_PANE" @warp_shell "$warp_name" 2>/dev/null

WARP_TMUX_DIR=$warp_dir
WARP_TMUX_MARKER_ONLY=1
WARP_HONOR_PS1=1
WARP_USING_WINDOWS_CON_PTY=false
export WARP_SESSION_ID WARP_TMUX_DIR WARP_TMUX_MARKER_ONLY WARP_HONOR_PS1 WARP_USING_WINDOWS_CON_PTY
# Values a parent Warp shell may have left in the tmux server's environment.
unset WARP_IS_LOCAL_SHELL_SESSION WARP_BOOTSTRAPPED WARP_IS_SUBSHELL warp_dir warp_token warp_hash

case $warp_name in
  zsh)
    if [ -n "${ZDOTDIR+x}" ]; then
      WARP_TMUX_ZDOTDIR=$ZDOTDIR
      export WARP_TMUX_ZDOTDIR
    fi
    ZDOTDIR=$WARP_TMUX_DIR/zsh
    export ZDOTDIR
    exec "$warp_shell" -l
    ;;
  bash) exec "$warp_shell" --rcfile "$WARP_TMUX_DIR/init.bash" -i ;;
  fish) exec "$warp_shell" -l --init-command 'source "$WARP_TMUX_DIR/init.fish"' ;;
esac
