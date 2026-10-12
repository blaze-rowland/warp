//! Native tmux integration via tmux control mode (`tmux -CC`).

#[cfg(feature = "local_tty")]
pub mod gateway;
pub mod settings;
