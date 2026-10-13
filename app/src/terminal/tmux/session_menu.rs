//! The tmux session menu (U8, R2): New and Attach for the focused pane's host, from the tab bar's
//! new-session menu and the command palette.
//!
//! Sessions are listed with `tmux ls` run through the focused session's command executor, so a
//! Warpified SSH pane lists the remote host's sessions without typing into the pane. Choosing an
//! entry runs a control-mode `tmux` command in the focused pane. Both use the `-L`/`-S` socket
//! the pane last opened tmux with.

use std::sync::Arc;

use super::entry::{TmuxSocket, shell_quote};
use crate::terminal::model::session::Session;

pub const NEW_SESSION_LABEL: &str = "New tmux session";
pub const ATTACH_LABEL: &str = "Attach to tmux session…";

/// The pane the session menu acts on.
#[derive(Clone, Debug)]
pub struct TmuxMenuTarget {
    /// The pane's active session; `tmux ls` runs through its command executor.
    pub session: Arc<Session>,
    pub socket: Option<TmuxSocket>,
    /// The session's shell already runs inside tmux.
    pub nested: bool,
}

/// What a session menu entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxMenuAction {
    NewSession,
    /// List the host's sessions to choose one to attach.
    ListSessions,
    /// Attach the named session.
    Attach(String),
}

/// One row of `tmux ls`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedSession {
    pub name: String,
    pub windows: u32,
    pub attached_clients: u32,
}

/// The result of listing a host's sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListOutcome {
    /// The running sessions; empty when no server runs.
    Sessions(Vec<ListedSession>),
    /// `tmux ls` failed for another reason, with tmux's or the shell's message.
    Failed(String),
}

/// A row of the attach menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    /// `action` is `None` for an informational row.
    Item {
        label: String,
        action: Option<TmuxMenuAction>,
    },
    Separator,
}

impl MenuEntry {
    pub fn action(label: impl Into<String>, action: TmuxMenuAction) -> Self {
        Self::Item {
            label: label.into(),
            action: Some(action),
        }
    }

    pub fn note(label: impl Into<String>) -> Self {
        Self::Item {
            label: label.into(),
            action: None,
        }
    }
}

/// Lists sessions with the counts first, so a name containing spaces is the rest of the line.
const LIST_FORMAT: &str = "'#{session_windows} #{session_attached} #{session_name}'";

/// The `tmux ls` command for the host, on `socket` when one is remembered.
pub fn list_sessions_command(socket: Option<&TmuxSocket>) -> String {
    format!("{} ls -F {LIST_FORMAT}", tmux_prefix(socket, false))
}

/// The sessions in `tmux ls` output in [`LIST_FORMAT`]. Rows that do not parse are skipped.
pub fn parse_list_sessions(stdout: &str) -> Vec<ListedSession> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            let windows = fields.next()?.parse().ok()?;
            let attached_clients = fields.next()?.parse().ok()?;
            let name = fields.next().filter(|name| !name.is_empty())?;
            Some(ListedSession {
                name: name.to_owned(),
                windows,
                attached_clients,
            })
        })
        .collect()
}

/// Interprets a finished `tmux ls`. No server running means no sessions, not an error.
pub fn list_outcome(success: bool, stdout: &str, stderr: &str) -> ListOutcome {
    if success {
        return ListOutcome::Sessions(parse_list_sessions(stdout));
    }
    let message = stderr.trim();
    if message.contains("no server running") || message.starts_with("error connecting to") {
        return ListOutcome::Sessions(Vec::new());
    }
    if message.is_empty() {
        ListOutcome::Failed("tmux ls failed".to_owned())
    } else {
        ListOutcome::Failed(clean(message))
    }
}

/// A session's menu label: its name, window count, and whether a client is attached.
pub fn session_label(session: &ListedSession) -> String {
    let windows = match session.windows {
        1 => "1 window".to_owned(),
        count => format!("{count} windows"),
    };
    let attached = if session.attached_clients > 0 {
        ", attached"
    } else {
        ""
    };
    format!("{} — {windows}{attached}", clean(&session.name))
}

/// The attach menu: one row per session, or why there are none, then New.
pub fn attach_menu_entries(outcome: &ListOutcome) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = match outcome {
        ListOutcome::Sessions(sessions) if sessions.is_empty() => {
            vec![MenuEntry::note("No tmux sessions")]
        }
        ListOutcome::Sessions(sessions) => sessions
            .iter()
            .map(|session| {
                MenuEntry::action(
                    session_label(session),
                    TmuxMenuAction::Attach(session.name.clone()),
                )
            })
            .collect(),
        ListOutcome::Failed(message) => {
            vec![MenuEntry::note(format!(
                "Could not list tmux sessions: {message}"
            ))]
        }
    };
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::action(
        NEW_SESSION_LABEL,
        TmuxMenuAction::NewSession,
    ));
    entries
}

/// The shell command a chosen entry runs in the focused pane. `control_mode` is false for a pane
/// already inside tmux, so the command runs raw there and explains why (R6).
pub fn command_for(
    action: &TmuxMenuAction,
    socket: Option<&TmuxSocket>,
    control_mode: bool,
) -> Option<String> {
    let tmux = tmux_prefix(socket, control_mode);
    match action {
        TmuxMenuAction::NewSession => Some(format!("{tmux} new")),
        TmuxMenuAction::Attach(name) => {
            // `=` makes tmux match the name exactly rather than as a prefix.
            Some(format!(
                "{tmux} attach -t {}",
                shell_quote(&format!("={name}"))
            ))
        }
        TmuxMenuAction::ListSessions => None,
    }
}

fn tmux_prefix(socket: Option<&TmuxSocket>, control_mode: bool) -> String {
    let mut prefix = "tmux".to_owned();
    if control_mode {
        prefix.push_str(" -CC");
    }
    if let Some(socket) = socket {
        prefix.push(' ');
        prefix.push_str(&socket.global_flags());
    }
    prefix
}

/// Text from tmux or a shell, safe for a menu label.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
#[path = "session_menu_tests.rs"]
mod tests;
