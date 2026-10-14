use warp_terminal::tmux::{TmuxWindow, WindowId};

use super::*;
use crate::terminal::tmux::TmuxOriginId;

fn window(id: u32, index: Option<u32>, name: &str) -> TmuxWindow {
    TmuxWindow {
        id: WindowId(id),
        index,
        name: Some(name.to_owned()),
        layout: None,
        active_pane: None,
    }
}

fn labels(strip: &StatusStrip) -> Vec<&str> {
    strip
        .entries
        .iter()
        .map(|entry| entry.label.as_str())
        .collect()
}

fn current_label(strip: &StatusStrip) -> Option<&str> {
    strip
        .entries
        .iter()
        .find(|entry| entry.current)
        .map(|entry| entry.label.as_str())
}

/// The characters the strip takes, counting gaps and overflow markers like the builder does.
fn width(strip: &StatusStrip) -> usize {
    let session = strip.session.chars().count() + 2 + ENTRY_GAP_CHARS;
    let entries: usize = strip
        .entries
        .iter()
        .map(|entry| entry.label.chars().count())
        .sum::<usize>()
        + ENTRY_GAP_CHARS * strip.entries.len().saturating_sub(1);
    let markers =
        (strip.hidden_before as usize + strip.hidden_after as usize) * (1 + ENTRY_GAP_CHARS);
    session + entries + markers
}

#[test]
fn windows_are_listed_in_window_index_order_with_current_marked() {
    // Window ids and map order disagree with tmux's indices, as after `move-window`.
    let windows = [
        window(5, Some(7), "TEMU-WARP"),
        window(1, Some(1), "SKILLFUL_SKILL"),
        window(9, Some(2), "Rollback"),
    ];
    let strip = build_status_strip("Eventric", &windows, WindowId(5), usize::MAX);
    assert_eq!(strip.session, "Eventric");
    assert_eq!(
        labels(&strip),
        ["1 SKILLFUL_SKILL", "2 Rollback", "7 TEMU-WARP"]
    );
    assert_eq!(current_label(&strip), Some("7 TEMU-WARP"));
    assert!(!strip.hidden_before && !strip.hidden_after);
}

#[test]
fn windows_without_an_index_yet_go_last() {
    let windows = [window(3, None, "new"), window(1, Some(0), "zsh")];
    let strip = build_status_strip("s", &windows, WindowId(1), usize::MAX);
    assert_eq!(labels(&strip), ["0 zsh", "new"]);
}

#[test]
fn names_are_sanitized_and_ellipsized() {
    let long = "a".repeat(MAX_NAME_CHARS + 10);
    let windows = [
        window(1, Some(1), "\u{1b}]0;evil\u{7}vim"),
        window(2, Some(2), &long),
        window(3, Some(3), "   "),
    ];
    let strip = build_status_strip("\u{1b}[31mwork\n", &windows, WindowId(1), usize::MAX);
    assert_eq!(strip.session, "[31mwork");
    assert_eq!(strip.entries[0].label, "1 ]0;evilvim");
    let expected = format!("2 {}{ELLIPSIS}", "a".repeat(MAX_NAME_CHARS - 1));
    assert_eq!(strip.entries[1].label, expected);
    // An empty name leaves the index alone.
    assert_eq!(strip.entries[2].label, "3");
}

#[test]
fn ellipsize_cuts_on_characters_not_bytes() {
    assert_eq!(ellipsize("short", 10), "short");
    assert_eq!(ellipsize("ünïcödé", 4), format!("ünï{ELLIPSIS}"));
    assert_eq!(ellipsize("abc", 1), ELLIPSIS.to_string());
    assert_eq!(ellipsize("abc", 0), "");
}

#[test]
fn current_window_stays_visible_under_a_width_budget() {
    let windows: Vec<TmuxWindow> = (1..=9)
        .map(|index| window(index, Some(index), &format!("window{index}")))
        .collect();
    // Room for the session and about three entries.
    let budget = 40;
    for current in 1..=9 {
        let strip = build_status_strip("Eventric", &windows, WindowId(current), budget);
        assert_eq!(
            current_label(&strip),
            Some(format!("{current} window{current}").as_str()),
            "current {current}"
        );
        assert!(width(&strip) <= budget, "current {current}: {strip:?}");
        assert!(strip.entries.len() >= 2, "current {current}: {strip:?}");
        assert_eq!(
            strip.hidden_before,
            strip.entries[0].window_id != WindowId(1)
        );
        assert_eq!(
            strip.hidden_after,
            strip.entries.last().unwrap().window_id != WindowId(9)
        );
    }
}

#[test]
fn last_window_current_keeps_the_tail() {
    let windows: Vec<TmuxWindow> = (1..=9)
        .map(|index| window(index, Some(index), &format!("window{index}")))
        .collect();
    let strip = build_status_strip("s", &windows, WindowId(9), 30);
    assert!(strip.hidden_before);
    assert!(!strip.hidden_after);
    assert_eq!(strip.entries.last().unwrap().label, "9 window9");
}

#[test]
fn current_window_name_shrinks_when_it_alone_overflows() {
    let windows = [
        window(1, Some(1), "one"),
        window(2, Some(2), "a-very-long-window-name"),
        window(3, Some(3), "three"),
    ];
    let strip = build_status_strip("s", &windows, WindowId(2), 16);
    assert_eq!(strip.entries.len(), 1);
    let current = &strip.entries[0];
    assert!(current.current);
    assert!(current.label.starts_with("2 "), "{current:?}");
    assert!(current.label.ends_with(ELLIPSIS), "{current:?}");
    assert!(width(&strip) <= 16, "{strip:?}");
}

#[test]
fn click_maps_an_entry_to_its_origin_and_window() {
    let origin = TmuxOriginId::new_for_test(4);
    let windows = [window(8, Some(2), "b"), window(3, Some(1), "a")];
    let strip = build_status_strip("s", &windows, WindowId(3), usize::MAX);
    assert_eq!(strip.click_target(origin, 0), Some((origin, WindowId(3))));
    assert_eq!(strip.click_target(origin, 1), Some((origin, WindowId(8))));
    assert_eq!(strip.click_target(origin, 2), None);
}

#[test]
fn tab_lookup_matches_origin_and_window() {
    let first = TmuxOriginId::new_for_test(1);
    let second = TmuxOriginId::new_for_test(2);
    // Two servers can both have a window @1.
    let tabs = [
        (0, None),
        (1, Some((first, WindowId(1)))),
        (2, Some((second, WindowId(1)))),
        (3, Some((second, WindowId(2)))),
    ];
    assert_eq!(tab_for_window(tabs, second, WindowId(1)), Some(2));
    assert_eq!(tab_for_window(tabs, first, WindowId(1)), Some(1));
    assert_eq!(tab_for_window(tabs, first, WindowId(2)), None);
}
