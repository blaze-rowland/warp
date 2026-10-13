# Warp's marker-only integration for a fish in a tmux pane, run by the launcher's --init-command.
set -gu WARP_SESSION_ID $WARP_SESSION_ID
set -gu WARP_TMUX_DIR $WARP_TMUX_DIR
set -gu WARP_TMUX_MARKER_ONLY $WARP_TMUX_MARKER_ONLY
set -gu WARP_USING_WINDOWS_CON_PTY $WARP_USING_WINDOWS_CON_PTY

function _warp_tmux_init_shell
    set -l _hostname (command -v hostname >/dev/null 2>&1 && command hostname 2>/dev/null || uname -n)
    set -l _user (command -v whoami >/dev/null 2>&1 && command whoami 2>/dev/null || echo $USER)
    set -l _msg (printf '{"hook": "InitShell", "value": {"session_id": %s, "shell": "fish", "user": "%s", "hostname": "%s"}}' "$WARP_SESSION_ID" "$_user" "$_hostname" | command od -An -v -tx1 | command tr -d " \n")
    printf '\x1b\x50\x24\x64%s\x1b\x5c' "$_msg"
end

_warp_tmux_init_shell
source "$WARP_TMUX_DIR/bootstrap.fish"

# A Warp that attaches later asks for the integration to be announced again; it shows blocks
# from the next prompt.
set -g _warp_tmux_announce_file "$WARP_TMUX_DIR/../announce/$WARP_SESSION_ID"
function _warp_tmux_precmd --on-event fish_prompt
    if test -e "$_warp_tmux_announce_file"
        command rm -f -- "$_warp_tmux_announce_file"
        _warp_tmux_init_shell
        warp_bootstrapped
    end
end
