//! The tmux-style status strip under a tmux tab while the vertical tabs panel is closed: the
//! session name, then each of the session's windows as `<index> <name>` in tmux's window-index
//! order, with the window the tab shows marked current. Clicking an entry activates that
//! window's tab.

use warp_terminal::tmux::{TmuxWindow, WindowId};

use super::TmuxOriginId;
use super::workspace_sync::{sanitize_title, window_title};

/// The longest a session or window name gets in the strip before it is ellipsized.
pub const MAX_NAME_CHARS: usize = 24;
/// The space between two strip entries, in characters.
pub const ENTRY_GAP_CHARS: usize = 2;
/// Marks text that was cut short, and windows left out on either side.
pub const ELLIPSIS: char = '\u{2026}';

/// One window in the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripEntry {
    pub window_id: WindowId,
    /// `<index> <name>`, sanitized and ellipsized.
    pub label: String,
    /// The window the tab shows.
    pub current: bool,
}

/// What the strip shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusStrip {
    /// The session name, sanitized and ellipsized, without brackets.
    pub session: String,
    /// The windows that fit, in tmux window-index order.
    pub entries: Vec<StripEntry>,
    /// Windows with lower indices were left out to fit the width budget.
    pub hidden_before: bool,
    /// Windows with higher indices were left out to fit the width budget.
    pub hidden_after: bool,
}

impl StatusStrip {
    /// The tmux window a click on entry `entry` selects.
    pub fn click_target(
        &self,
        origin: TmuxOriginId,
        entry: usize,
    ) -> Option<(TmuxOriginId, WindowId)> {
        self.entries
            .get(entry)
            .map(|entry| (origin, entry.window_id))
    }
}

/// `text` cut to at most `max_chars` characters, ending in [`ELLIPSIS`] when cut.
pub fn ellipsize(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars - 1).collect();
    cut.push(ELLIPSIS);
    cut
}

/// An entry's label: `<index> <name>`, or the index alone when the name is empty. A window
/// tmux has not reported an index for yet shows its title alone.
fn entry_label(window: &TmuxWindow, max_name_chars: usize) -> String {
    let Some(index) = window.index else {
        return ellipsize(&window_title(window), max_name_chars);
    };
    let name = window
        .name
        .as_deref()
        .map(sanitize_title)
        .unwrap_or_default();
    if name.is_empty() {
        index.to_string()
    } else {
        format!("{index} {}", ellipsize(&name, max_name_chars))
    }
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// The strip for `windows` of `session`, with `current` marked, fit into `budget_chars`
/// characters. The current window always stays in the strip; windows furthest from it are left
/// out first.
pub fn build_status_strip<'a>(
    session: &str,
    windows: impl IntoIterator<Item = &'a TmuxWindow>,
    current: WindowId,
    budget_chars: usize,
) -> StatusStrip {
    let session = ellipsize(&sanitize_title(session), MAX_NAME_CHARS);
    let mut windows: Vec<&TmuxWindow> = windows.into_iter().collect();
    // tmux's window-index order; windows still waiting on an index go last.
    windows.sort_by_key(|window| (window.index.is_none(), window.index, window.id.0));
    let mut entries: Vec<StripEntry> = windows
        .iter()
        .map(|window| StripEntry {
            window_id: window.id,
            label: entry_label(window, MAX_NAME_CHARS),
            current: window.id == current,
        })
        .collect();

    // `[session]` and the gap after it.
    let available = budget_chars.saturating_sub(chars(&session) + 2 + ENTRY_GAP_CHARS);
    let last = entries.len().saturating_sub(1);
    let marker = 1 + ENTRY_GAP_CHARS;
    let cost = |entries: &[StripEntry], lo: usize, hi: usize| {
        entries[lo..=hi]
            .iter()
            .map(|entry| chars(&entry.label))
            .sum::<usize>()
            + ENTRY_GAP_CHARS * (hi - lo)
            + if lo > 0 { marker } else { 0 }
            + if hi < last { marker } else { 0 }
    };
    if entries.is_empty() || cost(&entries, 0, last) <= available {
        return StatusStrip {
            session,
            entries,
            hidden_before: false,
            hidden_after: false,
        };
    }

    // Grow outward from the current window, one step right then one left, while it fits.
    let anchor = entries.iter().position(|entry| entry.current).unwrap_or(0);
    let (mut lo, mut hi) = (anchor, anchor);
    loop {
        let mut grew = false;
        if hi < last && cost(&entries, lo, hi + 1) <= available {
            hi += 1;
            grew = true;
        }
        if lo > 0 && cost(&entries, lo - 1, hi) <= available {
            lo -= 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }

    // The current window alone is too wide: shorten its name to what is left.
    if lo == hi && cost(&entries, lo, hi) > available {
        let window = windows[anchor];
        let markers = cost(&entries, lo, hi) - chars(&entries[anchor].label);
        let room = available.saturating_sub(markers);
        let prefix = window
            .index
            .map(|index| chars(&index.to_string()) + 1)
            .unwrap_or(0);
        entries[anchor].label = entry_label(window, room.saturating_sub(prefix).max(1));
    }

    StatusStrip {
        session,
        hidden_before: lo > 0,
        hidden_after: hi < last,
        entries: entries.drain(lo..=hi).collect(),
    }
}

/// The tab index showing `origin`'s `window_id`, among `tabs` (each tab's index and the tmux
/// window it shows, if any). Window ids are per server, so two sessions can share one.
pub fn tab_for_window(
    tabs: impl IntoIterator<Item = (usize, Option<(TmuxOriginId, WindowId)>)>,
    origin: TmuxOriginId,
    window_id: WindowId,
) -> Option<usize> {
    tabs.into_iter()
        .find(|(_, shown)| *shown == Some((origin, window_id)))
        .map(|(index, _)| index)
}

#[cfg(test)]
#[path = "status_strip_tests.rs"]
mod tests;
