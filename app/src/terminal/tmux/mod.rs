//! Native tmux integration via tmux control mode (`tmux -CC`).

pub mod entry;
#[cfg(feature = "local_tty")]
pub mod gateway;
pub mod hidden_tabs;
pub mod integration;
#[cfg(feature = "local_tty")]
pub mod key_capture;
pub mod key_tables;
#[cfg(feature = "local_tty")]
pub mod keys;
pub mod launcher_bindings;
pub mod layout_snapshot;
pub mod native_ui;
#[cfg(feature = "local_tty")]
pub mod pane_event_loop;
#[cfg(feature = "local_tty")]
pub mod pane_manager;
pub mod plugins;
pub mod popup;
#[cfg(feature = "local_tty")]
pub mod remote;
pub mod session_menu;
pub mod settings;
#[cfg(feature = "local_tty")]
pub mod status_strip;
#[cfg(feature = "local_tty")]
pub mod workspace_sync;

/// Identifies one origin PTY event loop. A respawned shell gets a new id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxOriginId(u64);

impl TmuxOriginId {
    #[cfg(test)]
    pub(crate) fn new_for_test(id: u64) -> Self {
        Self(id)
    }
}
