use super::*;
use crate::terminal::tmux::TmuxOriginId;

fn local() -> ShortcutTab {
    ShortcutTab {
        hidden: false,
        tmux: None,
    }
}

fn origin_tab() -> ShortcutTab {
    ShortcutTab {
        hidden: true,
        tmux: None,
    }
}

fn tmux(origin: u64, index: u32) -> ShortcutTab {
    ShortcutTab {
        hidden: false,
        tmux: Some(TmuxWindowSlot {
            origin: TmuxOriginId::new_for_test(origin),
            index: Some(index),
        }),
    }
}

/// The user's window: the hidden origin tab, then tmux windows 1, 2, 3, 4, 6, 7 ("Review" is 6).
fn session_with_gap() -> Vec<ShortcutTab> {
    let mut tabs = vec![origin_tab()];
    tabs.extend([1, 2, 3, 4, 6, 7].map(|index| tmux(1, index)));
    tabs
}

const REVIEW: usize = 5;

#[test]
fn tmux_focused_cmd_n_selects_the_window_with_tmux_index_n() {
    let tabs = session_with_gap();
    let focused = 2; // tmux window 2
    assert_eq!(
        shortcut_target(&tabs, focused, TabShortcut::Number(6)),
        Some(REVIEW)
    );
    assert_eq!(
        shortcut_target(&tabs, focused, TabShortcut::Number(1)),
        Some(1)
    );
    assert_eq!(
        shortcut_target(&tabs, focused, TabShortcut::Number(5)),
        None
    );
    // cmd+9 is tmux window 9, not the last tab.
    assert_eq!(shortcut_target(&tabs, focused, TabShortcut::Last), None);
}

#[test]
fn tmux_focused_labels_are_the_tmux_window_indices() {
    let tabs = session_with_gap();
    let labels: Vec<Option<usize>> = shortcut_labels(&tabs, 2)
        .into_iter()
        .map(|label| label.map(TabShortcut::number))
        .collect();
    assert_eq!(
        labels,
        vec![None, Some(1), Some(2), Some(3), Some(4), Some(6), Some(7)]
    );
}

#[test]
fn tmux_index_9_takes_the_last_tab_binding_and_indices_past_9_have_none() {
    let tabs = vec![origin_tab(), tmux(1, 0), tmux(1, 9), tmux(1, 10)];
    assert_eq!(
        shortcut_labels(&tabs, 1),
        vec![None, None, Some(TabShortcut::Last), None]
    );
    assert_eq!(shortcut_target(&tabs, 1, TabShortcut::Last), Some(2));
}

#[test]
fn tmux_focused_leaves_local_tabs_and_other_sessions_unlabeled_and_unreachable() {
    let tabs = vec![local(), origin_tab(), tmux(1, 1), tmux(1, 2), tmux(2, 3)];
    assert_eq!(
        shortcut_labels(&tabs, 2),
        vec![
            None,
            None,
            Some(TabShortcut::Number(1)),
            Some(TabShortcut::Number(2)),
            None
        ]
    );
    assert_eq!(shortcut_target(&tabs, 2, TabShortcut::Number(3)), None);
    assert_eq!(shortcut_target(&tabs, 2, TabShortcut::Number(2)), Some(3));
}

#[test]
fn local_focused_labels_and_selection_skip_the_hidden_origin_tab() {
    // A local tab, the hidden origin tab, then two tmux windows.
    let tabs = vec![local(), origin_tab(), tmux(1, 1), tmux(1, 2)];
    let labels = shortcut_labels(&tabs, 0);
    assert_eq!(
        labels,
        vec![
            Some(TabShortcut::Number(1)),
            None,
            Some(TabShortcut::Number(2)),
            Some(TabShortcut::Number(3)),
        ]
    );
    assert_eq!(shortcut_target(&tabs, 0, TabShortcut::Number(2)), Some(2));
    assert_eq!(shortcut_target(&tabs, 0, TabShortcut::Number(3)), Some(3));
    assert_eq!(shortcut_target(&tabs, 0, TabShortcut::Number(4)), None);
    assert_eq!(shortcut_target(&tabs, 0, TabShortcut::Last), Some(3));
}

#[test]
fn every_label_selects_its_own_tab() {
    let mut tabs = vec![local(), origin_tab()];
    tabs.extend((1..=12).map(|index| tmux(1, index)));
    tabs.extend((0..10).map(|_| local()));
    for active in [0, 2, 9, tabs.len() - 1] {
        for (index, label) in shortcut_labels(&tabs, active).into_iter().enumerate() {
            if let Some(shortcut) = label {
                assert_eq!(
                    shortcut_target(&tabs, active, shortcut),
                    Some(index),
                    "active {active}, tab {index}, {shortcut:?}"
                );
            }
        }
    }
}

#[test]
fn local_labels_follow_the_numbered_and_last_tab_bindings() {
    let tabs: Vec<ShortcutTab> = (0..10).map(|_| local()).collect();
    let labels = shortcut_labels(&tabs, 0);
    assert_eq!(labels[7], Some(TabShortcut::Number(8)));
    assert_eq!(labels[8], None);
    assert_eq!(labels[9], Some(TabShortcut::Last));
}

#[test]
fn binding_names_match_the_activate_tab_bindings() {
    use crate::tab::{TAB_ACTIVATE_BINDING_NAMES, TAB_ACTIVATE_LAST_BINDING_NAME};
    assert_eq!(
        TabShortcut::Number(1).binding_name(),
        TAB_ACTIVATE_BINDING_NAMES[0]
    );
    assert_eq!(
        TabShortcut::Number(8).binding_name(),
        TAB_ACTIVATE_BINDING_NAMES[7]
    );
    assert_eq!(
        TabShortcut::Last.binding_name(),
        TAB_ACTIVATE_LAST_BINDING_NAME
    );
    assert_eq!(TabShortcut::from_number(9), Some(TabShortcut::Last));
    assert_eq!(TabShortcut::from_number(0), None);
    assert_eq!(TabShortcut::from_number(10), None);
}
