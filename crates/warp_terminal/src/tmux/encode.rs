//! Builders for outgoing tmux control-mode command lines.
//!
//! Every builder returns whole command lines. tmux answers each line with exactly one
//! `%begin`/`%end` (or `%error`) block, so a reply correlator counts one expected reply per
//! returned line.

use super::parser::PaneId;

/// Bytes per `send-keys -H` command. Keeps each line well under tmux's command length limits.
const SEND_KEYS_CHUNK_BYTES: usize = 128;

/// Encodes `bytes` as literal input for `pane_id`, using hex so no byte is reinterpreted as a key
/// name or by tmux's command parser. Returns one command line per element; each ends in `\n`.
pub fn send_keys_command(pane_id: PaneId, bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(SEND_KEYS_CHUNK_BYTES)
        .map(|chunk| {
            let mut command = format!("send-keys -t {pane_id} -H");
            for byte in chunk {
                command.push_str(&format!(" {byte:02x}"));
            }
            command.push('\n');
            command
        })
        .collect()
}

/// Sets the control client's size. One command line.
pub fn refresh_client_size_command(columns: usize, rows: usize) -> String {
    format!("refresh-client -C {columns}x{rows}\n")
}

#[cfg(test)]
#[path = "encode_tests.rs"]
mod tests;
