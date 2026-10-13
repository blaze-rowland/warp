//! Warp's tmux plugin and the continuum autosave stand-in (U10, R5, R9, KTD8, KTD11).
//!
//! **The plugin.** `warp.tmux` ships in the launcher bundle Warp installs on each tmux host
//! ([`super::integration::LauncherBundle`]). Loaded from `~/.tmux.conf` ([`plugin_install_line`]),
//! it sets tmux's `default-command` to the launcher, so every shell tmux starts on its own
//! (another client's split, a script's window, a tmux-resurrect restore) carries verified
//! integration options, and Warp shows blocks there whenever it attaches. With the plugin loaded,
//! Warp never types its init command into a pane ([`super::integration::new_pane_plan`]).
//!
//! **Continuum.** tmux-continuum saves from a `#()` job in the status line, which a control client
//! never draws. While Warp is the only client, Warp runs the same save on continuum's interval
//! ([`ContinuumAutosave`]). The interval check and the `@continuum-save-last-timestamp` update
//! happen on the tmux host, exactly as continuum's `continuum_save.sh` does them, so a save from a
//! plain client's status line and one from Warp never run within the same interval.

use std::time::Duration;

use warp_terminal::tmux::CommandReply;

use super::integration::{INSTALL_ROOT, run_shell};

/// The plugin's entry script in the launcher bundle.
pub const PLUGIN_FILE: &str = "warp.tmux";
/// The link in the install root that points at the newest install, so the plugin line in
/// `~/.tmux.conf` stays the same across Warp versions.
pub const CURRENT_DIR: &str = "current";
/// Set by the plugin while it is loaded.
pub const PLUGIN_OPTION: &str = "@warp-plugin";
/// The `default-command` the plugin replaced, restored by `warp.tmux unload`.
pub const SAVED_DEFAULT_COMMAND_OPTION: &str = "@warp-saved-default-command";

/// The line a user adds to `~/.tmux.conf` to load the plugin. It does nothing until Warp has
/// installed its files on that host.
pub fn plugin_install_line() -> String {
    format!("run-shell '~/{INSTALL_ROOT}/{CURRENT_DIR}/{PLUGIN_FILE} 2>/dev/null || true'")
}

/// The plugin's hint in the tmux settings section, with the line to copy.
pub fn plugin_install_text() -> String {
    format!(
        "Give shells tmux starts on its own Warp features: splits from other tmux clients, \
         script windows, and tmux-resurrect restores. Add this line to ~/.tmux.conf: {} \
         Warp installs the plugin on each tmux host when it attaches, over SSH too, so the same \
         line works on remote hosts.",
        plugin_install_line()
    )
}

/// Reads [`PLUGIN_OPTION`].
pub fn plugin_query() -> String {
    format!("show-options -gqv {PLUGIN_OPTION}")
}

/// Whether a [`plugin_query`] reply says the plugin is loaded.
pub fn parse_plugin(reply: &CommandReply) -> bool {
    !reply.error
        && reply
            .lines
            .first()
            .is_some_and(|line| !String::from_utf8_lossy(line).trim().is_empty())
}

/// `path` as it should be after Warp detaches: without Warp's versioned wrapper directories, but
/// with the plugin's stable one (`~/.cache/warp/tmux/current/bin`), which stays while the plugin
/// is loaded.
pub fn path_after_detach(path: &str) -> String {
    path.split(':')
        .filter(|entry| !super::popup::is_wrapper_dir(entry) || is_plugin_wrapper_dir(entry))
        .collect::<Vec<_>>()
        .join(":")
}

fn is_plugin_wrapper_dir(entry: &str) -> bool {
    entry
        .trim_end_matches('/')
        .ends_with(&format!("/{INSTALL_ROOT}/{CURRENT_DIR}/bin"))
}

// Continuum.

/// continuum's default interval when `@continuum-save-interval` is unset.
const CONTINUUM_DEFAULT_MINUTES: u64 = 15;
/// Where TPM installs tmux-resurrect's save script, used when resurrect has not recorded its own
/// path in `@resurrect-save-script-path` (it does when it loads).
const RESURRECT_DEFAULT_SAVE_SCRIPT: &str = "$HOME/.tmux/plugins/tmux-resurrect/scripts/save.sh";
/// The option continuum keeps its last save time in (seconds since the epoch, host clock).
const LAST_SAVE_OPTION: &str = "@continuum-save-last-timestamp";
/// How often Warp checks whether a save is due. The host compares against the interval, so a
/// save runs within a minute after it is due.
pub const AUTOSAVE_TICK: Duration = Duration::from_secs(60);

/// Reads what the autosave needs: continuum's interval, resurrect's save script, and whether
/// continuum hooked itself into the status line (which is how it runs, so it is installed even
/// when the interval is left at its default).
pub const CONTINUUM_QUERY: &str = "display-message -p '#{@continuum-save-interval}\t#{@resurrect-save-script-path}\t#{m:*continuum_save*,#{status-right}}#{m:*continuum_save*,#{status-left}}'";

/// Continuum's autosave as Warp runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContinuumAutosave {
    /// Time between saves.
    pub interval: Duration,
    /// resurrect's save script; `None` uses TPM's default location.
    pub save_script: Option<String>,
}

/// The autosave a [`CONTINUUM_QUERY`] reply describes. `None` when continuum is not installed,
/// its interval is 0 (continuum's "off"), or the reply is unusable.
pub fn parse_continuum(reply: &CommandReply) -> Option<ContinuumAutosave> {
    if reply.error {
        return None;
    }
    let line = String::from_utf8_lossy(reply.lines.first()?).into_owned();
    let mut fields = line.splitn(3, '\t');
    let interval = fields.next()?.trim();
    let save_script = fields.next()?.trim();
    let hooked = fields.next().unwrap_or_default().contains('1');
    if interval.is_empty() && !hooked {
        return None;
    }
    let minutes = if interval.is_empty() {
        CONTINUUM_DEFAULT_MINUTES
    } else {
        interval.parse().ok()?
    };
    if minutes == 0 {
        return None;
    }
    let usable = |path: &str| {
        path.starts_with('/')
            && !path
                .chars()
                .any(|c| c.is_control() || matches!(c, '\'' | '"' | '$' | '`' | '\\' | '#'))
    };
    Some(ContinuumAutosave {
        interval: Duration::from_secs(minutes * 60),
        save_script: usable(save_script).then(|| save_script.to_owned()),
    })
}

impl ContinuumAutosave {
    /// The save, run on the tmux host: when the interval has passed since continuum's last
    /// save, record the new save time and start resurrect's save quietly in the background, as
    /// continuum's own `continuum_save.sh` does. The check and the record run before tmux takes
    /// Warp's next command, so ticks never overlap into two saves.
    pub fn save_command(&self) -> String {
        let script = self
            .save_script
            .as_deref()
            .unwrap_or(RESURRECT_DEFAULT_SAVE_SCRIPT);
        let seconds = self.interval.as_secs().max(1);
        run_shell(&format!(
            "s=\"{script}\"; n=$(date +%s) && l=$(tmux show-options -gqv {LAST_SAVE_OPTION}) \
             && [ -x \"$s\" ] && [ \"$n\" -ge $(( ${{l:-0}} + {seconds} )) ] \
             && tmux set-option -g {LAST_SAVE_OPTION} \"$n\" && {{ \"$s\" quiet & }}"
        ))
    }
}

/// Whether Warp runs the save on this tick: only when no plain client is attached. A plain
/// client's status line runs continuum itself; an unreadable client list counts as one.
pub fn autosave_due(plain_clients: Option<&[String]>) -> bool {
    matches!(plain_clients, Some([]))
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;
