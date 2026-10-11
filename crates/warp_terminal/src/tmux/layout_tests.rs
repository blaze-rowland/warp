use super::*;

/// Prefix `body` with its tmux checksum, as `#{window_layout}` does.
fn checked(body: &str) -> String {
    format!("{:04x},{body}", layout_checksum(body))
}

fn parse(body: &str) -> LayoutNode {
    parse_window_layout(&checked(body)).unwrap()
}

fn ids(ids: &[u32]) -> Vec<PaneId> {
    ids.iter().copied().map(PaneId).collect()
}

#[test]
fn checksum_matches_values_recorded_from_tmux_3_7c() {
    assert_eq!(
        layout_checksum("80x24,0,0{40x24,0,0,0,39x24,41,0,1}"),
        0x8205
    );
    assert_eq!(layout_checksum("80x24,0,0,1"), 0xb25e);
}

#[test]
fn parses_a_single_pane() {
    let layout = parse_window_layout("b25e,80x24,0,0,1").unwrap();
    assert_eq!(
        layout,
        LayoutNode::Leaf(LayoutLeaf {
            width: 80,
            height: 24,
            x: 0,
            y: 0,
            pane_id: PaneId(1),
        })
    );
}

#[test]
fn parses_a_recorded_side_by_side_split() {
    let layout = parse_window_layout("8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1}").unwrap();
    assert_eq!(layout.pane_ids(), ids(&[0, 1]));
    let LayoutNode::Split {
        horizontal,
        children,
        ..
    } = layout
    else {
        panic!("expected split");
    };
    assert!(horizontal);
    assert_eq!(children.len(), 2);
}

#[test]
fn bad_checksum_is_rejected() {
    assert_eq!(
        parse_window_layout("8206,80x24,0,0{40x24,0,0,0,39x24,41,0,1}"),
        Err(LayoutError::BadChecksum {
            expected: 0x8206,
            actual: 0x8205
        })
    );
}

#[test]
fn missing_checksum_is_rejected() {
    assert_eq!(
        parse_window_layout("80x24,0,0,1"),
        Err(LayoutError::MissingChecksum)
    );
    assert_eq!(parse_window_layout(""), Err(LayoutError::MissingChecksum));
}

#[test]
fn malformed_bodies_with_valid_checksums_are_rejected() {
    for body in [
        "80x24,0,0",
        "80x24,0,0{",
        "80x24,0,0{40x24,0,0,0",
        "80x24,0,0{40x24,0,0,0]",
        "80x24,0,0{}",
        "80x24,0,0,1trailing",
        "80x,0,0,1",
        "99999999999x24,0,0,1",
    ] {
        assert_eq!(
            parse_window_layout(&checked(body)),
            Err(LayoutError::Malformed),
            "{body}"
        );
    }
}

#[test]
fn nesting_past_the_depth_cap_is_rejected_without_overflowing_the_stack() {
    fn nested(depth: usize) -> String {
        let mut body = String::new();
        let mut closes = Vec::with_capacity(depth);
        for level in 0..depth {
            let (open, close) = if level % 2 == 0 {
                ('{', '}')
            } else {
                ('[', ']')
            };
            body.push_str("1x1,0,0");
            body.push(open);
            closes.push(close);
        }
        body.push_str("1x1,0,0,0");
        body.extend(closes.into_iter().rev());
        body
    }
    assert!(parse_window_layout(&checked(&nested(MAX_LAYOUT_DEPTH - 1))).is_ok());
    assert_eq!(
        parse_window_layout(&checked(&nested(MAX_LAYOUT_DEPTH + 1))),
        Err(LayoutError::TooDeep)
    );
    // Far past the cap: must return an error, not recurse until the stack runs out.
    assert_eq!(
        parse_window_layout(&checked(&nested(100_000))),
        Err(LayoutError::TooDeep)
    );
}

#[test]
fn layout_change_reads_visible_layout_and_zoom_flag() {
    let flags = WindowFlags::new("*Z");
    let parsed = parse_layout_change(
        "8205,80x24,0,0{40x24,0,0,0,39x24,41,0,1}",
        Some("b25e,80x24,0,0,1"),
        &flags,
    )
    .unwrap();
    assert_eq!(parsed.layout.pane_ids(), ids(&[0, 1]));
    assert_eq!(parsed.visible_layout.pane_ids(), ids(&[1]));
    assert!(parsed.flags.is_zoomed());
    assert!(parsed.flags.is_active());
    assert!(!parsed.flags.is_last());
}

#[test]
fn layout_change_without_visible_layout_uses_the_full_layout() {
    let parsed = parse_layout_change("b25e,80x24,0,0,1", None, &WindowFlags::new("-")).unwrap();
    assert_eq!(parsed.visible_layout, parsed.layout);
    assert!(parsed.flags.is_last());
    assert!(!parsed.flags.is_zoomed());
}

#[test]
fn layout_change_rejects_a_bad_visible_layout() {
    assert!(matches!(
        parse_layout_change(
            "b25e,80x24,0,0,1",
            Some("0000,80x24,0,0,1"),
            &WindowFlags::new("*")
        ),
        Err(LayoutError::BadChecksum { .. })
    ));
}

#[test]
fn split_steps_emit_one_side_by_side_from_first_leaf() {
    assert_eq!(
        split_steps(&parse("80x24,0,0{40x24,0,0,0,39x24,41,0,1}")),
        vec![SplitStep {
            parent: ids(&[0]),
            new_pane: PaneId(1),
            side_by_side: true,
            parent_size: 40,
            new_size: 39,
        }]
    );
}

#[test]
fn stacked_split_is_not_side_by_side() {
    assert_eq!(
        split_steps(&parse("80x24,0,0[40x12,0,0,0,40x11,0,13,2]")),
        vec![SplitStep {
            parent: ids(&[0]),
            new_pane: PaneId(2),
            side_by_side: false,
            parent_size: 12,
            new_size: 11,
        }]
    );
}

#[test]
fn nested_split_preserves_tree_order_and_sizes() {
    let layout = parse("80x24,0,0{40x24,0,0,0,40x24,40,0[40x12,40,0,1,40x11,40,13,2]}");
    assert_eq!(layout.pane_ids(), ids(&[0, 1, 2]));
    assert_eq!(
        split_steps(&layout),
        vec![
            SplitStep {
                parent: ids(&[0]),
                new_pane: PaneId(1),
                side_by_side: true,
                parent_size: 40,
                new_size: 40,
            },
            SplitStep {
                parent: ids(&[1]),
                new_pane: PaneId(2),
                side_by_side: false,
                parent_size: 12,
                new_size: 11,
            },
        ]
    );
}

#[test]
fn nested_first_outer_child_splits_beside_the_subtree() {
    let layout = parse("80x24,0,0{40x24,0,0[40x12,0,0,0,40x11,40,13,1],39x24,41,0,2}");
    assert_eq!(
        split_steps(&layout),
        vec![
            SplitStep {
                parent: ids(&[0]),
                new_pane: PaneId(1),
                side_by_side: false,
                parent_size: 12,
                new_size: 11,
            },
            SplitStep {
                parent: ids(&[0, 1]),
                new_pane: PaneId(2),
                side_by_side: true,
                parent_size: 40,
                new_size: 39,
            },
        ]
    );
}

#[test]
fn missing_from_layout_returns_stale_pane_ids() {
    let layout = parse("80x24,0,0{40x24,0,0,0,39x24,41,0,1}");
    assert_eq!(missing_from_layout(&ids(&[0, 1, 9]), &layout), ids(&[9]));
}
