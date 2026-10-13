# Warp's marker-only integration for a zsh in a tmux pane, sourced by the launcher.
typeset +x WARP_SESSION_ID WARP_TMUX_DIR WARP_TMUX_MARKER_ONLY WARP_USING_WINDOWS_CON_PTY

_warp_tmux_init_shell() {
  local _hostname _user _msg
  _hostname=$(command -pv hostname >/dev/null 2>&1 && command -p hostname 2>/dev/null || command -p uname -n)
  _user=$(command -pv whoami >/dev/null 2>&1 && command -p whoami 2>/dev/null || echo $USER)
  _msg=$(printf '{"hook": "InitShell", "value": {"session_id": %s, "shell": "zsh", "user": "%s", "hostname": "%s"}}' "$WARP_SESSION_ID" "$_user" "$_hostname" | command -p od -An -v -tx1 | command -p tr -d " \n")
  printf '\x1b\x50\x24\x64%s\x1b\x5c' "$_msg"
}

_warp_tmux_init_shell
source "$WARP_TMUX_DIR/bootstrap.zsh"

# A Warp that attaches later asks for the integration to be announced again; it shows blocks
# from the next prompt.
_warp_tmux_announce_file="$WARP_TMUX_DIR/../announce/$WARP_SESSION_ID"
# Warp's `tmux` wrapper goes first on PATH, after the user's startup files had their say, so
# scripts that open tmux popups reach it.
_warp_tmux_path() {
  case ":$PATH:" in
    ":$WARP_TMUX_DIR/bin:"*) ;;
    *) PATH="$WARP_TMUX_DIR/bin:$PATH" ;;
  esac
}
_warp_tmux_path

_warp_tmux_precmd() {
  _warp_tmux_path
  if [[ -e $_warp_tmux_announce_file ]]; then
    command -p rm -f -- "$_warp_tmux_announce_file"
    _warp_tmux_init_shell
    warp_bootstrapped
  fi
}
precmd_functions=(_warp_tmux_precmd $precmd_functions)
