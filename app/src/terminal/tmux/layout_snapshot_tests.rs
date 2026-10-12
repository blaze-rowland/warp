use warp_terminal::tmux::{
    LayoutNode, PaneId, WindowFlags, layout_checksum, parse_layout_change, parse_window_layout,
};

use super::*;
use crate::app_state::{
    BranchSnapshot, LeafContents, LeafSnapshot, PaneFlex, PaneNodeSnapshot, SplitDirection,
    TmuxPaneSnapshot,
};
use crate::terminal::tmux::TmuxOriginId;

const ORIGIN: TmuxOriginId = TmuxOriginId(4);

/// A layout string with its checksum, from the body tmux would send.
fn layout(body: &str) -> String {
    format!("{:04x},{body}", layout_checksum(body))
}

fn node(body: &str) -> LayoutNode {
    parse_window_layout(&layout(body)).expect("valid layout")
}

fn leaf(pane: u32, focused: bool, command: Option<&str>) -> PaneNodeSnapshot {
    PaneNodeSnapshot::Leaf(LeafSnapshot {
        is_focused: focused,
        custom_vertical_tabs_title: None,
        contents: LeafContents::TmuxPane(TmuxPaneSnapshot {
            origin: ORIGIN,
            pane_id: PaneId(pane),
            current_command: command.map(str::to_owned),
        }),
    })
}

fn no_commands(_: PaneId) -> Option<String> {
    None
}

#[test]
fn a_single_pane_becomes_one_focused_leaf() {
    let snapshot = layout_to_snapshot(ORIGIN, &node("80x24,0,0,1"), Some(PaneId(1)), &|_| {
        Some("zsh".to_owned())
    });
    assert_eq!(snapshot, leaf(1, true, Some("zsh")));
}

#[test]
fn a_side_by_side_split_keeps_tmux_cell_proportions() {
    // 60 + 1 border + 19 = 80 columns.
    let layout = node("80x24,0,0{60x24,0,0,1,19x24,61,0,2}");
    let snapshot = layout_to_snapshot(ORIGIN, &layout, Some(PaneId(2)), &no_commands);
    let PaneNodeSnapshot::Branch(BranchSnapshot {
        direction,
        children,
    }) = snapshot
    else {
        panic!("expected a branch");
    };
    assert_eq!(direction, SplitDirection::Horizontal);
    assert_eq!(children.len(), 2);
    let (PaneFlex(left), left_leaf) = &children[0];
    let (PaneFlex(right), right_leaf) = &children[1];
    assert_eq!(left_leaf, &leaf(1, false, None));
    assert_eq!(right_leaf, &leaf(2, true, None));
    // Flexes average 1.0 like Warp's own splits, in tmux's 60:19 ratio.
    assert!((left + right - 2.0).abs() < 1e-4);
    assert!((left / right - 60.0 / 19.0).abs() < 1e-4);
}

#[test]
fn nested_splits_keep_their_orientation() {
    let layout = node("80x24,0,0{40x24,0,0,1,39x24,41,0[39x12,41,0,2,39x11,41,13,3]}");
    let snapshot = layout_to_snapshot(ORIGIN, &layout, None, &no_commands);
    let PaneNodeSnapshot::Branch(root) = snapshot else {
        panic!("expected a branch");
    };
    assert_eq!(root.direction, SplitDirection::Horizontal);
    let PaneNodeSnapshot::Branch(column) = &root.children[1].1 else {
        panic!("expected a nested branch");
    };
    assert_eq!(column.direction, SplitDirection::Vertical);
    assert_eq!(column.children[0].1, leaf(2, false, None));
    assert_eq!(column.children[1].1, leaf(3, false, None));
}

#[test]
fn a_zoomed_window_shows_only_the_zoomed_pane() {
    let full = layout("80x24,0,0{40x24,0,0,1,39x24,41,0,2}");
    let visible = layout("80x24,0,0,2");
    let window = parse_layout_change(&full, Some(&visible), &WindowFlags::new("*Z")).unwrap();
    let shown = tab_layout(&window);
    assert_eq!(shown.pane_ids(), vec![PaneId(2)]);
    let snapshot = layout_to_snapshot(ORIGIN, shown, Some(PaneId(2)), &no_commands);
    assert_eq!(snapshot, leaf(2, true, None));
}

#[test]
fn an_unchanged_layout_plans_nothing() {
    let shape = layout_shape(&node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    let plan = plan_pane_update(Some(&shape), &[PaneId(1), PaneId(2)], &shape);
    assert_eq!(plan, PaneUpdatePlan::default());
    assert_eq!(plan.kind, UpdateKind::Unchanged);
}

#[test]
fn a_new_pane_is_added_once_and_restructures() {
    let before = layout_shape(&node("80x24,0,0,1"));
    let after = layout_shape(&node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    let plan = plan_pane_update(Some(&before), &[PaneId(1)], &after);
    assert_eq!(plan.added, vec![PaneId(2)]);
    assert!(plan.removed.is_empty());
    assert_eq!(plan.kind, UpdateKind::Restructure);

    // The same layout arriving again (Warp's own split echoed back, or a refresh) adds nothing.
    let again = plan_pane_update(Some(&after), &[PaneId(1), PaneId(2)], &after);
    assert!(again.added.is_empty());
    assert_eq!(again.kind, UpdateKind::Unchanged);
}

#[test]
fn a_removed_pane_is_removed() {
    let before = layout_shape(&node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    let after = layout_shape(&node("80x24,0,0,1"));
    let plan = plan_pane_update(Some(&before), &[PaneId(1), PaneId(2)], &after);
    assert_eq!(plan.removed, vec![PaneId(2)]);
    assert!(plan.added.is_empty());
    assert_eq!(plan.kind, UpdateKind::Restructure);
}

#[test]
fn swapping_two_panes_restructures_without_adding_or_removing() {
    let before = layout_shape(&node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    let after = layout_shape(&node("80x24,0,0{40x24,0,0,2,39x24,41,0,1}"));
    let plan = plan_pane_update(Some(&before), &[PaneId(1), PaneId(2)], &after);
    assert!(plan.added.is_empty());
    assert!(plan.removed.is_empty());
    assert_eq!(plan.kind, UpdateKind::Restructure);
}

#[test]
fn a_resize_only_changes_flexes() {
    let before = layout_shape(&node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    let after = layout_shape(&node("80x24,0,0{20x24,0,0,1,59x24,21,0,2}"));
    let plan = plan_pane_update(Some(&before), &[PaneId(1), PaneId(2)], &after);
    assert_eq!(plan.kind, UpdateKind::Resize);
    assert!(plan.added.is_empty() && plan.removed.is_empty());
}

#[test]
fn a_pane_missing_from_the_tab_is_added_even_when_the_layout_is_known() {
    // The tab lost pane 2 (creation failed); the next layout puts it back.
    let shape = layout_shape(&node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    let plan = plan_pane_update(Some(&shape), &[PaneId(1)], &shape);
    assert_eq!(plan.added, vec![PaneId(2)]);
    assert_eq!(plan.kind, UpdateKind::Restructure);
}

#[test]
fn branch_flexes_follow_depth_first_order() {
    let layout = node("80x24,0,0{40x24,0,0,1,39x24,41,0[39x12,41,0,2,39x11,41,13,3]}");
    let flexes = branch_flexes(&layout);
    assert_eq!(flexes.len(), 2);
    assert_eq!(flexes[0].len(), 2);
    assert_eq!(flexes[1].len(), 2);
    assert!((flexes[1][0] / flexes[1][1] - 12.0 / 11.0).abs() < 1e-4);
}

#[test]
fn dragging_a_divider_becomes_resize_pane_on_the_first_pane_before_it() {
    let layout = node("80x24,0,0{40x24,0,0,1,39x24,41,0[39x12,41,0,2,39x11,41,13,3]}");
    // The user dragged the root divider so the left side takes 3/4 of 79 cells.
    let mut flexes = branch_flexes(&layout);
    flexes[0] = vec![1.5, 0.5];
    assert_eq!(
        resize_commands(&layout, &flexes),
        vec!["resize-pane -t %1 -x 59".to_owned()]
    );
}

#[test]
fn dragging_a_nested_divider_resizes_rows() {
    let layout = node("80x24,0,0{40x24,0,0,1,39x24,41,0[39x12,41,0,2,39x11,41,13,3]}");
    let mut flexes = branch_flexes(&layout);
    flexes[1] = vec![0.5, 1.5];
    // 23 rows split 1:3 is about 6 rows for the top pane.
    assert_eq!(
        resize_commands(&layout, &flexes),
        vec!["resize-pane -t %2 -y 6".to_owned()]
    );
}

#[test]
fn unchanged_flexes_send_nothing() {
    let layout = node("80x24,0,0{40x24,0,0,1,39x24,41,0,2}");
    assert!(resize_commands(&layout, &branch_flexes(&layout)).is_empty());
    // A different tree shape (the layout changed under the drag) sends nothing either.
    assert!(resize_commands(&layout, &[]).is_empty());
}
