//! Tab index mapping while a tmux origin tab is hidden (KTD4). The origin tab stays in the
//! workspace's tab list; cmd+1–9, next/previous tab and the tab bar count visible tabs only.
//! While a tmux tab is focused, cmd+1–9 follow tmux's window indices instead.

/// The tab index of the `visible`-th visible tab.
pub fn visible_to_raw(hidden: &[bool], visible: usize) -> Option<usize> {
    hidden
        .iter()
        .enumerate()
        .filter(|(_, hidden)| !**hidden)
        .nth(visible)
        .map(|(index, _)| index)
}

/// The visible position of tab `raw`, or `None` when it is hidden.
pub fn raw_to_visible(hidden: &[bool], raw: usize) -> Option<usize> {
    if *hidden.get(raw)? {
        return None;
    }
    Some(hidden[..raw].iter().filter(|hidden| !**hidden).count())
}

/// The next (or previous) visible tab from `current`, wrapping around.
pub fn step_visible(hidden: &[bool], current: usize, forward: bool) -> Option<usize> {
    let len = hidden.len();
    if len == 0 {
        return None;
    }
    (1..=len)
        .map(|step| {
            if forward {
                (current + step) % len
            } else {
                (current + len - step % len) % len
            }
        })
        .find(|index| !hidden[*index])
}

/// The last visible tab.
pub fn last_visible(hidden: &[bool]) -> Option<usize> {
    hidden.iter().rposition(|hidden| !*hidden)
}

/// `index` if it is visible, else the closest visible tab after it, else before it.
pub fn nearest_visible(hidden: &[bool], index: usize) -> Option<usize> {
    let index = index.min(hidden.len().checked_sub(1)?);
    (index..hidden.len())
        .find(|i| !hidden[*i])
        .or_else(|| (0..index).rev().find(|i| !hidden[*i]))
}

/// A tab-switching shortcut: cmd+1–8 (`Number`) or cmd+9 (`Last`, "switch to last tab").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabShortcut {
    Number(usize),
    Last,
}

impl TabShortcut {
    pub fn from_number(number: usize) -> Option<Self> {
        match number {
            1..=8 => Some(Self::Number(number)),
            9 => Some(Self::Last),
            _ => None,
        }
    }

    pub fn number(self) -> usize {
        match self {
            Self::Number(number) => number,
            Self::Last => 9,
        }
    }

    /// The keybinding whose keystroke the tab's hint shows.
    pub fn binding_name(self) -> &'static str {
        match self {
            Self::Number(number) => crate::tab::TAB_ACTIVATE_BINDING_NAMES[number - 1],
            Self::Last => crate::tab::TAB_ACTIVATE_LAST_BINDING_NAME,
        }
    }
}

/// The tmux window a tab shows: its session (origin) and tmux's `#{window_index}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TmuxWindowSlot {
    pub origin: super::TmuxOriginId,
    pub index: Option<u32>,
}

/// A tab as the tab-switching shortcuts see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutTab {
    pub hidden: bool,
    pub tmux: Option<TmuxWindowSlot>,
}

/// The tmux session whose window is the focused tab, if any.
fn focused_session(tabs: &[ShortcutTab], active: usize) -> Option<super::TmuxOriginId> {
    tabs.get(active)
        .filter(|tab| !tab.hidden)
        .and_then(|tab| tab.tmux)
        .map(|slot| slot.origin)
}

/// The tab `shortcut` selects while tab `active` is focused.
///
/// While a tmux tab is focused, cmd+N goes to the window whose tmux index is N in that tab's
/// session (as tmux's own `prefix N` does), or nowhere when the session has no window N; cmd+9
/// is window 9. Otherwise cmd+1–8 count visible tabs and cmd+9 is the last visible tab.
pub fn shortcut_target(
    tabs: &[ShortcutTab],
    active: usize,
    shortcut: TabShortcut,
) -> Option<usize> {
    if let Some(origin) = focused_session(tabs, active) {
        let index = u32::try_from(shortcut.number()).ok()?;
        return tabs.iter().position(|tab| {
            !tab.hidden
                && tab
                    .tmux
                    .is_some_and(|slot| slot.origin == origin && slot.index == Some(index))
        });
    }
    let hidden: Vec<bool> = tabs.iter().map(|tab| tab.hidden).collect();
    match shortcut {
        TabShortcut::Number(number) => visible_to_raw(&hidden, number.checked_sub(1)?),
        TabShortcut::Last => last_visible(&hidden),
    }
}

/// The shortcut each tab's hint shows while tab `active` is focused: exactly the shortcut that
/// selects it under [`shortcut_target`]. While a tmux tab is focused that is the session's
/// windows by tmux index (1–9), and other tabs show nothing; otherwise every visible tab, tmux
/// tabs included, shows its visible position, so the hidden origin tab is never counted.
pub fn shortcut_labels(tabs: &[ShortcutTab], active: usize) -> Vec<Option<TabShortcut>> {
    if let Some(origin) = focused_session(tabs, active) {
        return tabs
            .iter()
            .map(|tab| {
                let slot = tab
                    .tmux
                    .filter(|slot| !tab.hidden && slot.origin == origin)?;
                TabShortcut::from_number(usize::try_from(slot.index?).ok()?)
            })
            .collect();
    }
    let visible_count = tabs.iter().filter(|tab| !tab.hidden).count();
    let mut position = 0;
    tabs.iter()
        .map(|tab| {
            if tab.hidden {
                return None;
            }
            position += 1;
            if position <= 8 {
                Some(TabShortcut::Number(position))
            } else {
                (position == visible_count).then_some(TabShortcut::Last)
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "hidden_tabs_tests.rs"]
mod tests;
