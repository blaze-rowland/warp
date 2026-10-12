//! Native tmux integration via tmux control mode (`tmux -CC`).

#[cfg(feature = "local_tty")]
pub mod gateway;
#[cfg(feature = "local_tty")]
pub mod pane_event_loop;
#[cfg(feature = "local_tty")]
pub mod pane_manager;
pub mod settings;

/// Identifies one origin PTY event loop. A respawned shell gets a new id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxOriginId(u64);

impl TmuxOriginId {
    #[cfg(test)]
    pub(crate) fn new_for_test(id: u64) -> Self {
        Self(id)
    }
}
