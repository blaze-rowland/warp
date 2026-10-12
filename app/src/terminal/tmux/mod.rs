//! Native tmux integration via tmux control mode (`tmux -CC`).

#[cfg(feature = "local_tty")]
pub mod gateway;
pub mod hidden_tabs;
pub mod layout_snapshot;
#[cfg(feature = "local_tty")]
pub mod pane_event_loop;
#[cfg(feature = "local_tty")]
pub mod pane_manager;
pub mod settings;
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
