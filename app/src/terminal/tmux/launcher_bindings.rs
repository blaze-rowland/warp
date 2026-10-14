//! tmux bindings that start a shell, started through Warp's launcher instead.
//!
//! A pane tmux creates for a binding like `bind | split-window -h -c "#{pane_current_path}"`
//! starts tmux's default shell, which Warp never integrates while a plain client is attached
//! (R7, KTD7), so it stays a plain terminal. When a key Warp routes to tmux resolves to a single
//! `split-window` or `new-window` with no shell-command, Warp sends that command itself with
//! the binding's flags kept as bound and U9's launcher appended as the shell-command, as its own
//! splits and windows do. Formats in the flags (`-c "#{pane_current_path}"`) stay as bound for
//! tmux to expand; arguments are split with tmux's quoting rules ([`split_commands`]) and passed
//! on exactly as `list-keys` printed them.

use warp_terminal::tmux::PaneId;

use super::integration::launcher_shell_command;
use super::native_ui::{parse_args, split_commands};

/// A bound command rewritten to start its shell through the launcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherBinding {
    /// The command's canonical name, for logs.
    pub name: &'static str,
    /// The command to send instead of the key.
    pub line: String,
}

/// The command to send instead of a key bound to `command`, pressed in `pane_id`, when the
/// binding would start tmux's default shell and the launcher at `launcher` is installed with
/// pane integration on. `None` leaves the key to tmux.
pub fn launcher_binding(
    command: &str,
    pane_id: PaneId,
    launcher: Option<&str>,
    integrate_panes: bool,
) -> Option<LauncherBinding> {
    let launcher = launcher.filter(|launcher| integrate_panes && !launcher.is_empty())?;
    let mut commands = split_commands(command);
    if commands.len() != 1 {
        return None;
    }
    let tokens = commands.pop()?;
    let (name_token, args) = tokens.split_first()?;
    let (name, flags) = match name_token.text.as_str() {
        "split-window" | "splitw" => ("split-window", &SPLIT_WINDOW),
        "new-window" | "neww" => ("new-window", &NEW_WINDOW),
        _ => return None,
    };
    let parsed = parse_args(args, flags.with_value);
    if !parsed.positional.is_empty()
        || !parsed
            .switches
            .iter()
            .all(|switch| flags.switches.contains(*switch))
    {
        return None;
    }
    let mut line = name_token.raw.clone();
    // `split-window` splits the pane the key was pressed in, as Warp's own splits do. tmux
    // refuses a pane as `new-window`'s target, which stays the client's current window.
    if name == "split-window" && !parsed.has('t') {
        line.push_str(&format!(" -t {pane_id}"));
    }
    for arg in args {
        line.push(' ');
        line.push_str(&arg.raw);
    }
    line.push(' ');
    line.push_str(&launcher_shell_command(launcher));
    Some(LauncherBinding { name, line })
}

/// The flags a command takes, as tmux 3.4 defines them, less any that do not start a shell.
struct Flags {
    switches: &'static str,
    with_value: &'static str,
}

/// `-I` (forward stdin instead of starting a shell) and flags newer than 3.4 are left out.
const SPLIT_WINDOW: Flags = Flags {
    switches: "bdfhPvZ",
    with_value: "celpFt",
};

const NEW_WINDOW: Flags = Flags {
    switches: "abdkPS",
    with_value: "cenFt",
};

#[cfg(test)]
#[path = "launcher_bindings_tests.rs"]
mod tests;
