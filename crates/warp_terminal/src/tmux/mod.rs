//! tmux control-mode (`tmux -CC`) protocol core.
//!
//! Everything here is pure and byte-oriented: no I/O, no locking, no app state. The transport
//! feeds raw PTY reads into [`ControlModeParser`]; the engine turns [`ControlEvent`]s into
//! session state and uses [`encode`] to build outgoing command lines.

pub mod encode;
pub mod layout;
pub mod parser;
pub mod version;

pub use encode::{refresh_client_size_command, send_keys_command};
pub use layout::{
    LayoutError, LayoutLeaf, LayoutNode, SplitStep, WindowFlags, WindowLayout, layout_checksum,
    missing_from_layout, parse_layout_change, parse_window_layout, split_steps,
};
pub use parser::{
    CONTROL_MODE_DCS, CommandReply, ControlEvent, ControlModeParser, DecodeItem, PaneId, SessionId,
    WindowId, octal_unescape,
};
pub use version::TmuxVersion;
