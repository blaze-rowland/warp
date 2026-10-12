//! The attached tmux session as Warp knows it: windows, their layouts, and panes.
//!
//! The model is plain data. [`super::client::ControlClient`] feeds it from notifications and
//! `list-windows` / `list-panes` replies; the app reads it to build tabs and splits.

use std::collections::BTreeMap;

use super::layout::{WindowFlags, WindowLayout, parse_layout_change};
use super::parser::{PaneId, SessionId, WindowId};

/// `list-windows -F` format matching [`parse_window_row`]. The name is last because it is the
/// only free-text field.
pub const LIST_WINDOWS_FORMAT: &str = "#{window_id}\t#{window_index}\t#{window_active}\t\
#{window_layout}\t#{window_visible_layout}\t#{window_flags}\t#{window_name}";

/// `list-panes -F` format matching [`parse_pane_row`].
/// The current command is last because it is the only free-text field.
pub const LIST_PANES_FORMAT: &str = "#{pane_id}\t#{window_id}\t#{pane_active}\t#{alternate_on}\t\
#{pane_in_mode}\t#{pane_width}\t#{pane_height}\t#{pane_mode}\t#{pane_current_command}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSession {
    pub id: SessionId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxWindow {
    pub id: WindowId,
    /// `None` until a `list-windows` reply covers a window announced by `%window-add`.
    pub index: Option<u32>,
    pub name: Option<String>,
    pub layout: Option<WindowLayout>,
    pub active_pane: Option<PaneId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxPane {
    pub id: PaneId,
    pub window_id: WindowId,
    /// True while the pane shows its alternate screen (full-screen apps).
    pub alternate_on: bool,
    /// The pane's mode (`copy-mode`, `tree-mode`, ...), or `None` when it shows its program.
    pub mode: Option<String>,
    pub width: u32,
    pub height: u32,
    /// `#{pane_current_command}`: the pane's foreground program (`fish`, `vim`, ...), or `None`
    /// when tmux did not report one.
    pub current_command: Option<String>,
}

/// One row of a [`LIST_WINDOWS_FORMAT`] reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRow {
    pub id: WindowId,
    pub index: u32,
    pub active: bool,
    /// `None` when tmux reported a layout that does not parse.
    pub layout: Option<WindowLayout>,
    pub name: String,
}

/// One row of a [`LIST_PANES_FORMAT`] reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub id: PaneId,
    pub window_id: WindowId,
    pub active: bool,
    pub alternate_on: bool,
    pub mode: Option<String>,
    pub width: u32,
    pub height: u32,
    pub current_command: Option<String>,
}

/// A difference found when a `list-windows` reply replaces the window set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowChange {
    Added(WindowId),
    Closed(WindowId),
    Renamed(WindowId, String),
    LayoutChanged(WindowId),
}

/// Panes that a layout change added to or removed from a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutDelta {
    pub added: Vec<PaneId>,
    pub removed: Vec<PaneId>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionModel {
    session: Option<TmuxSession>,
    windows: BTreeMap<WindowId, TmuxWindow>,
    panes: BTreeMap<PaneId, TmuxPane>,
    active_window: Option<WindowId>,
}

pub fn parse_window_row(line: &[u8]) -> Option<WindowRow> {
    let line = String::from_utf8_lossy(line);
    let mut fields = line.splitn(7, '\t');
    let id = WindowId::parse(fields.next()?)?;
    let index = fields.next()?.parse().ok()?;
    let active = fields.next()? == "1";
    let layout = fields.next()?;
    let visible_layout = fields.next()?;
    let flags = WindowFlags::new(fields.next()?);
    let name = fields.next()?.to_owned();
    let layout = parse_layout_change(layout, Some(visible_layout), &flags).ok();
    Some(WindowRow {
        id,
        index,
        active,
        layout,
        name,
    })
}

pub fn parse_pane_row(line: &[u8]) -> Option<PaneRow> {
    let line = String::from_utf8_lossy(line);
    let mut fields = line.splitn(9, '\t');
    let id = PaneId::parse(fields.next()?)?;
    let window_id = WindowId::parse(fields.next()?)?;
    let active = fields.next()? == "1";
    let alternate_on = fields.next()? == "1";
    let in_mode = fields.next()? == "1";
    let width = fields.next()?.parse().ok()?;
    let height = fields.next()?.parse().ok()?;
    let mode = fields.next()?;
    let mode = (in_mode || !mode.is_empty()).then(|| mode.to_owned());
    let current_command = fields
        .next()
        .filter(|command| !command.is_empty())
        .map(str::to_owned);
    Some(PaneRow {
        id,
        window_id,
        active,
        alternate_on,
        mode,
        width,
        height,
        current_command,
    })
}

impl SessionModel {
    pub fn session(&self) -> Option<&TmuxSession> {
        self.session.as_ref()
    }

    pub fn windows(&self) -> impl Iterator<Item = &TmuxWindow> {
        self.windows.values()
    }

    pub fn window(&self, id: WindowId) -> Option<&TmuxWindow> {
        self.windows.get(&id)
    }

    pub fn panes(&self) -> impl Iterator<Item = &TmuxPane> {
        self.panes.values()
    }

    pub fn pane(&self, id: PaneId) -> Option<&TmuxPane> {
        self.panes.get(&id)
    }

    pub fn active_window(&self) -> Option<WindowId> {
        self.active_window
    }

    /// Records the attached session. Switching to a different session clears every window and
    /// pane; returns true when that happened.
    pub fn set_session(&mut self, id: SessionId, name: String) -> bool {
        let switched = self
            .session
            .as_ref()
            .is_some_and(|session| session.id != id);
        if switched {
            self.windows.clear();
            self.panes.clear();
            self.active_window = None;
        }
        self.session = Some(TmuxSession { id, name });
        switched
    }

    pub fn rename_session(&mut self, id: SessionId, name: String) -> bool {
        match &mut self.session {
            Some(session) if session.id == id => {
                session.name = name;
                true
            }
            _ => false,
        }
    }

    /// Returns true when the window was not known before.
    pub fn add_window(&mut self, id: WindowId) -> bool {
        if self.windows.contains_key(&id) {
            return false;
        }
        self.windows.insert(id, empty_window(id));
        true
    }

    /// Removes the window and its panes.
    pub fn close_window(&mut self, id: WindowId) -> Option<TmuxWindow> {
        let window = self.windows.remove(&id)?;
        self.panes.retain(|_, pane| pane.window_id != id);
        if self.active_window == Some(id) {
            self.active_window = None;
        }
        Some(window)
    }

    /// Returns true when the name changed.
    pub fn rename_window(&mut self, id: WindowId, name: String) -> bool {
        match self.windows.get_mut(&id) {
            Some(window) if window.name.as_deref() != Some(name.as_str()) => {
                window.name = Some(name);
                true
            }
            _ => false,
        }
    }

    pub fn set_active_window(&mut self, id: WindowId) {
        self.active_window = Some(id);
    }

    pub fn set_active_pane(&mut self, window_id: WindowId, pane_id: PaneId) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.active_pane = Some(pane_id);
        }
    }

    pub fn set_pane_mode(&mut self, id: PaneId, mode: Option<String>) {
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.mode = mode;
        }
    }

    /// Applies a window's layout, adding the window if needed, and creates or removes its panes
    /// to match.
    pub fn apply_layout(&mut self, id: WindowId, layout: WindowLayout) -> LayoutDelta {
        let mut delta = LayoutDelta::default();
        let leaves = layout.layout.leaves();
        for leaf in &leaves {
            match self.panes.get_mut(&leaf.pane_id) {
                Some(pane) => {
                    pane.window_id = id;
                    pane.width = leaf.width;
                    pane.height = leaf.height;
                }
                None => {
                    delta.added.push(leaf.pane_id);
                    self.panes.insert(
                        leaf.pane_id,
                        TmuxPane {
                            id: leaf.pane_id,
                            window_id: id,
                            alternate_on: false,
                            mode: None,
                            width: leaf.width,
                            height: leaf.height,
                            current_command: None,
                        },
                    );
                }
            }
        }
        let live: Vec<PaneId> = leaves.iter().map(|leaf| leaf.pane_id).collect();
        self.panes.retain(|pane_id, pane| {
            let keep = pane.window_id != id || live.contains(pane_id);
            if !keep {
                delta.removed.push(*pane_id);
            }
            keep
        });
        self.windows
            .entry(id)
            .or_insert_with(|| empty_window(id))
            .layout = Some(layout);
        delta
    }

    /// Replaces the window set with a full `list-windows` reply.
    pub fn replace_windows(&mut self, rows: Vec<WindowRow>) -> Vec<WindowChange> {
        let mut changes = Vec::new();
        let listed: Vec<WindowId> = rows.iter().map(|row| row.id).collect();
        let closed: Vec<WindowId> = self
            .windows
            .keys()
            .filter(|id| !listed.contains(id))
            .copied()
            .collect();
        for &id in &closed {
            self.close_window(id);
            changes.push(WindowChange::Closed(id));
        }
        let mut updates = Vec::new();
        for row in rows {
            if row.active {
                self.active_window = Some(row.id);
            }
            match self.windows.get_mut(&row.id) {
                Some(window) => {
                    window.index = Some(row.index);
                    if window.name.as_deref() != Some(row.name.as_str()) {
                        changes.push(WindowChange::Renamed(row.id, row.name.clone()));
                        window.name = Some(row.name);
                    }
                    if let Some(layout) = row.layout
                        && window.layout.as_ref() != Some(&layout)
                    {
                        changes.push(WindowChange::LayoutChanged(row.id));
                        updates.push((row.id, layout));
                    }
                }
                None => {
                    changes.push(WindowChange::Added(row.id));
                    let mut window = empty_window(row.id);
                    window.index = Some(row.index);
                    window.name = Some(row.name);
                    self.windows.insert(row.id, window);
                    if let Some(layout) = row.layout {
                        updates.push((row.id, layout));
                    }
                }
            }
        }
        for (id, layout) in updates {
            self.apply_layout(id, layout);
        }
        changes
    }

    /// Replaces the pane set with a full `list-panes -s` reply.
    pub fn replace_panes(&mut self, rows: Vec<PaneRow>) {
        self.panes.clear();
        for row in rows {
            if row.active {
                self.set_active_pane(row.window_id, row.id);
            }
            self.panes.insert(
                row.id,
                TmuxPane {
                    id: row.id,
                    window_id: row.window_id,
                    alternate_on: row.alternate_on,
                    mode: row.mode,
                    width: row.width,
                    height: row.height,
                    current_command: row.current_command,
                },
            );
        }
    }
}

fn empty_window(id: WindowId) -> TmuxWindow {
    TmuxWindow {
        id,
        index: None,
        name: None,
        layout: None,
        active_pane: None,
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
