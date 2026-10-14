//! tmux popups as floating Warp panes (U14, R30, KTD10, KTD14).
//!
//! A popup tmux opens on Warp's control client runs hidden: nothing draws it, it blocks the
//! script that opened it, and it takes the keys Warp sends through `send-keys -K`. Warp keeps
//! popups out of that path in three ways:
//!
//! - **Bindings.** A binding that runs `display-popup` is intercepted (KTD10) and runs its
//!   command in a detached tmux window instead ([`bound_popup_command`]).
//! - **Scripts.** Warp's `tmux` wrapper sits first on PATH in Warp's panes and in the server's
//!   global environment ([`path_with_wrapper`]). It turns a popup that would land on a control
//!   client into the same kind of window and blocks until it exits, as the popup would.
//! - **Everything else.** A script that runs tmux by absolute path still opens a hidden popup.
//!   Warp probes the server for it ([`probe_commands`]): a popup is the only child of the tmux
//!   server that has a terminal but is not a pane. Warp then stops sending keys through tmux,
//!   where the popup would swallow them, and offers to close it ([`close_popup_command`]).
//!
//! Popup windows carry [`POPUP_WINDOW_NAME`]; Warp shows them as a floating pane over the
//! session's tab rather than as tabs of their own.

use std::collections::HashSet;
use std::time::Duration;

use instant::Instant;
use warp_terminal::tmux::{CommandReply, PaneId, quote_argument};

use super::native_ui::PopupSpec;

/// The name (or name prefix, before `:` and the popup's title) of a popup's window.
pub const POPUP_WINDOW_NAME: &str = "warp-popup";

/// Whether a window is a popup Warp shows as a floating pane.
pub fn is_popup_window_name(name: &str) -> bool {
    name == POPUP_WINDOW_NAME
        || name
            .strip_prefix(POPUP_WINDOW_NAME)
            .is_some_and(|rest| rest.starts_with(':'))
}

/// The popup's `-T` title, from its window name.
pub fn popup_title(name: &str) -> Option<String> {
    let title = name.strip_prefix(POPUP_WINDOW_NAME)?.strip_prefix(':')?;
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// The window name for a popup titled `title`.
pub fn popup_window_name(title: Option<&str>) -> String {
    match title.map(str::trim).filter(|title| !title.is_empty()) {
        Some(title) => format!("{POPUP_WINDOW_NAME}:{title}"),
        None => POPUP_WINDOW_NAME.to_owned(),
    }
}

/// Runs a bound `display-popup`'s command in a detached window and prints the window's id.
/// The arguments are spliced as bound, so tmux parses them exactly as the binding would; one
/// that could end the command or run past its own word is quoted instead.
pub fn bound_popup_command(spec: &PopupSpec) -> String {
    let mut command = format!(
        "new-window -d -P -F '#{{window_id}}' -n {}",
        quote_argument(&popup_window_name(spec.title.as_deref()))
    );
    if let Some(directory) = &spec.directory_raw {
        command.push_str(" -c ");
        command.push_str(&splice_argument(directory));
    }
    for environment in &spec.environment_raw {
        command.push_str(" -e ");
        command.push_str(&splice_argument(environment));
    }
    for argument in &spec.command_raw {
        command.push(' ');
        command.push_str(&splice_argument(argument));
    }
    command
}

/// `raw` when tmux would read it as exactly one plain argument, or else `raw` double-quoted
/// with control characters removed.
fn splice_argument(raw: &str) -> String {
    if is_one_plain_word(raw) {
        raw.to_owned()
    } else {
        let text: String = raw.chars().filter(|c| !c.is_control()).collect();
        quote_argument(&text)
    }
}

/// Whether tmux reads `raw` as one argument that is not a command block and does not end the
/// command: no control characters, no whitespace outside quotes, balanced quotes, and no
/// trailing `;` (which tmux treats as a separator, escaped or not).
fn is_one_plain_word(raw: &str) -> bool {
    if raw.is_empty()
        || raw.starts_with('{')
        || raw.ends_with(';')
        || raw.chars().any(char::is_control)
    {
        return false;
    }
    let mut chars = raw.chars();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\\') | (Some('"'), '\\') => {
                if chars.next().is_none() {
                    return false;
                }
            }
            (None, '\'' | '"') => quote = Some(c),
            (None, c) if c.is_whitespace() => return false,
            (Some(open), c) if c == open => quote = None,
            _ => {}
        }
    }
    quote.is_none()
}

/// The window id in a [`bound_popup_command`] reply.
pub fn parse_window_id(reply: &CommandReply) -> Option<String> {
    if reply.error {
        return None;
    }
    let line = String::from_utf8_lossy(reply.lines.first()?)
        .trim()
        .to_owned();
    (line.starts_with('@') && line[1..].chars().all(|c| c.is_ascii_digit())).then_some(line)
}

/// A popup's window closes when its command exits, even if the user set `remain-on-exit`. A
/// command that already exited is cleaned up too. `if-shell -F` evaluates the format without a
/// shell, so the line still answers with one reply block.
pub fn popup_window_options_command(window_id: &str) -> String {
    format!(
        "set-option -w -t {window_id} remain-on-exit off ; \
         if-shell -F -t {window_id} \"#{{pane_dead}}\" \"kill-window -t {window_id}\""
    )
}

/// Closes the popup open on `client`.
pub fn close_popup_command(client: &str) -> String {
    format!("display-popup -C -c {}", quote_argument(client))
}

// PATH.

/// Whether a PATH entry is a Warp wrapper directory (`~/.cache/warp/tmux/<version>/bin`).
pub fn is_wrapper_dir(entry: &str) -> bool {
    let entry = entry.trim_end_matches('/');
    entry.ends_with("/bin")
        && entry
            .rsplit('/')
            .nth(2)
            .is_some_and(|parent| parent == "tmux")
        && entry.contains("/.cache/warp/tmux/")
}

/// `path` without any Warp wrapper directory.
pub fn path_without_wrapper(path: &str) -> String {
    path.split(':')
        .filter(|entry| !is_wrapper_dir(entry))
        .collect::<Vec<_>>()
        .join(":")
}

/// `path` with `wrapper_dir` first and no other Warp wrapper directory.
pub fn path_with_wrapper(path: &str, wrapper_dir: &str) -> String {
    let rest = path_without_wrapper(path);
    if rest.is_empty() {
        wrapper_dir.to_owned()
    } else {
        format!("{wrapper_dir}:{rest}")
    }
}

/// The wrapper's directory, beside the launcher at `launcher_path`.
pub fn wrapper_dir(launcher_path: &str) -> Option<String> {
    let (dir, _) = launcher_path.rsplit_once('/')?;
    Some(format!("{dir}/bin"))
}

/// Reads PATH from the server's global environment.
pub const GLOBAL_PATH_QUERY: &str = "show-environment -g PATH";

/// PATH in a [`GLOBAL_PATH_QUERY`] reply. `None` when it is unset or the query failed.
pub fn parse_global_path(reply: &CommandReply) -> Option<String> {
    if reply.error {
        return None;
    }
    let line = String::from_utf8_lossy(reply.lines.first()?).into_owned();
    let path = line.strip_prefix("PATH=")?;
    (!path.is_empty()).then(|| path.to_owned())
}

/// Sets PATH in the server's global environment, so new panes and `run-shell` jobs see it.
pub fn set_global_path_command(path: &str) -> String {
    let path: String = path.chars().filter(|c| !c.is_control()).collect();
    format!("set-environment -g PATH {}", quote_argument(&path))
}

// Hidden popups.

/// The server option the probe records the server's terminal children in.
pub const PROBE_OPTION: &str = "@warp-popup-probe";

/// Records the tmux server's children that have a terminal in [`PROBE_OPTION`], prefixed with
/// `token;` so [`parse_probe`] can tell this run's record from an earlier one.
///
/// The script runs on the server's host as a background `run-shell` job, so a slow `ps` never
/// holds up tmux's command queue. The job is a child of the server, so `$PPID` is the server.
/// `ps` prints `?`/`??` for a process without a terminal.
pub fn probe_start_command(token: u64) -> String {
    let script = format!(
        "tmux set-option -g {PROBE_OPTION} \"{token};$(ps -A -o ppid= -o pid= -o tty= -o comm= \
         | awk -v s=\"$PPID\" \"\\$1 == s && \\$3 !~ /^[?]/ {{ print \\$2 \\\" \\\" \\$4 }}\" \
         | tr \"\\n\" \",\")\""
    );
    super::integration::run_shell_background(&script)
}

/// Commands whose replies [`hidden_popups`] needs once [`probe_start_command`]'s job finished,
/// in order: read the record, list every pane's process, and list the clients.
pub fn probe_read_commands() -> [String; 3] {
    [
        format!("show-options -gqv {PROBE_OPTION}"),
        "list-panes -a -F '#{pane_pid}'".to_owned(),
        "list-clients -F '#{client_control_mode}\t#{client_activity}\t#{client_name}'".to_owned(),
    ]
}

/// A child process of the tmux server with a terminal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerChild {
    pub pid: u32,
    /// The program's name (no directory).
    pub command: String,
}

/// The children in the probe's record, or `None` when the record is not `token`'s run (its job
/// has not finished yet).
pub fn parse_probe(reply: &CommandReply, token: u64) -> Option<Vec<ServerChild>> {
    if reply.error {
        return None;
    }
    let text: String = reply
        .lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>()
        .join(",");
    let (record_token, text) = text.split_once(';')?;
    if record_token.trim().parse::<u64>().ok()? != token {
        return None;
    }
    let children = text
        .split(',')
        .filter_map(|entry| {
            let (pid, command) = entry.trim().split_once(' ')?;
            let command = command.trim();
            let command = command.rsplit('/').next().unwrap_or(command);
            Some(ServerChild {
                pid: pid.parse().ok()?,
                command: command.chars().filter(|c| !c.is_control()).collect(),
            })
        })
        .collect();
    Some(children)
}

/// Past this a probe that never got its replies (a dropped or rejected command) is given up.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How soon to look again when a probe saw a popup that [`HiddenPopupWatch`] has yet to confirm.
pub const CONFIRM_DELAY: Duration = Duration::from_secs(1);

/// The first delay while watching an offered popup for it to go; it doubles up to
/// [`MAX_WATCH_DELAY`].
pub const WATCH_DELAY: Duration = Duration::from_secs(3);
pub const MAX_WATCH_DELAY: Duration = Duration::from_secs(15);

/// One session's probe runs: at most one at a time, each with its own token.
#[derive(Clone, Debug, Default)]
pub struct ProbeState {
    running: Option<(u64, Instant)>,
    next_token: u64,
    watch_delay: Option<Duration>,
}

impl ProbeState {
    /// Starts a probe and returns its token, unless one is running and not yet given up.
    pub fn start(&mut self, now: Instant) -> Option<u64> {
        if let Some((_, started)) = self.running
            && now.saturating_duration_since(started) < PROBE_TIMEOUT
        {
            return None;
        }
        self.next_token += 1;
        self.running = Some((self.next_token, now));
        Some(self.next_token)
    }

    /// Whether `token` is the probe running now; it no longer is.
    pub fn finish(&mut self, token: u64) -> bool {
        if self.running.is_some_and(|(running, _)| running == token) {
            self.running = None;
            true
        } else {
            false
        }
    }

    /// Gives up the probe `token` (its record never appeared).
    pub fn abandon(&mut self, token: u64) {
        self.finish(token);
    }

    /// When to probe again after one that `found` candidates while `offered` a popup. `None`
    /// waits for an on-demand probe.
    pub fn next_delay(&mut self, found: bool, offered: bool) -> Option<Duration> {
        if offered {
            let delay = self
                .watch_delay
                .map_or(WATCH_DELAY, |delay| (delay * 2).min(MAX_WATCH_DELAY));
            self.watch_delay = Some(delay);
            return Some(delay);
        }
        self.watch_delay = None;
        found.then_some(CONFIRM_DELAY)
    }
}

/// The pane processes in a `list-panes -F '#{pane_pid}'` reply.
pub fn parse_pane_pids(reply: &CommandReply) -> HashSet<u32> {
    if reply.error {
        return HashSet::new();
    }
    reply
        .lines
        .iter()
        .filter_map(|line| String::from_utf8_lossy(line).trim().parse().ok())
        .collect()
}

/// One attached client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRow {
    pub control: bool,
    pub activity: u64,
    pub name: String,
}

pub fn parse_clients(reply: &CommandReply) -> Vec<ClientRow> {
    if reply.error {
        return Vec::new();
    }
    reply
        .lines
        .iter()
        .filter_map(|line| {
            let line = String::from_utf8_lossy(line);
            let mut fields = line.splitn(3, '\t');
            let control = fields.next()? == "1";
            let activity = fields.next()?.trim().parse().unwrap_or(0);
            let name = fields.next()?.to_owned();
            Some(ClientRow {
                control,
                activity,
                name,
            })
        })
        .collect()
}

/// The popups among the server's terminal children: those that are not panes. Popups that tmux
/// opened from a script land on the most recently active client, so they are hidden only when
/// that client is a control client (or no plain client is attached).
pub fn hidden_popups(
    children: &[ServerChild],
    pane_pids: &HashSet<u32>,
    clients: &[ClientRow],
) -> Vec<ServerChild> {
    let latest = clients.iter().max_by_key(|client| client.activity);
    let lands_on_control = match latest {
        Some(client) => client.control,
        None => true,
    };
    if !lands_on_control {
        return Vec::new();
    }
    children
        .iter()
        .filter(|child| !pane_pids.contains(&child.pid))
        .cloned()
        .collect()
}

/// Debounces probe results for one session: a popup is reported once it was seen by two probes
/// in a row, so a popup that closes on its own right away never raises the offer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HiddenPopupWatch {
    last: Vec<ServerChild>,
    offered: Option<ServerChild>,
}

/// A change in what the watch reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HiddenPopupChange {
    /// Offer to close this popup; keys stop going through tmux.
    Found(ServerChild),
    /// The popup is gone; keys go through tmux again.
    Gone,
}

impl HiddenPopupWatch {
    pub fn on_probe(&mut self, found: Vec<ServerChild>) -> Option<HiddenPopupChange> {
        let confirmed = found
            .iter()
            .find(|child| self.last.contains(child))
            .cloned();
        self.last = found;
        match (&self.offered, confirmed) {
            (None, Some(child)) => {
                self.offered = Some(child.clone());
                Some(HiddenPopupChange::Found(child))
            }
            (Some(offered), _) if !self.last.contains(offered) => {
                self.offered = None;
                Some(HiddenPopupChange::Gone)
            }
            _ => None,
        }
    }

    pub fn offered(&self) -> Option<&ServerChild> {
        self.offered.as_ref()
    }
}

/// The toast offering to close a hidden popup.
pub fn hidden_popup_text(child: &ServerChild) -> String {
    let name = if child.command.is_empty() {
        "A program"
    } else {
        child.command.as_str()
    };
    format!(
        "A tmux popup ({name}) opened where Warp can't show it. It blocks the script that opened \
         it, so tmux key bindings are paused. Click to close it."
    )
}

/// One popup window shown as a floating pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupWindow {
    pub window_id: warp_terminal::tmux::WindowId,
    pub pane_id: PaneId,
    pub title: Option<String>,
}

/// The popup windows in a session model, in window order.
pub fn popup_windows(model: &warp_terminal::tmux::SessionModel) -> Vec<PopupWindow> {
    let mut popups: Vec<PopupWindow> = model
        .windows()
        .filter_map(|window| {
            let name = window.name.as_deref()?;
            if !is_popup_window_name(name) {
                return None;
            }
            let pane_id = window.layout.as_ref()?.layout.pane_ids().first().copied()?;
            Some(PopupWindow {
                window_id: window.id,
                pane_id,
                title: popup_title(name),
            })
        })
        .collect();
    popups.sort_by_key(|popup| popup.window_id);
    popups
}

#[cfg(feature = "local_tty")]
mod model;
#[cfg(feature = "local_tty")]
pub use model::*;

#[cfg(test)]
#[path = "popup_tests.rs"]
pub(crate) mod tests;
