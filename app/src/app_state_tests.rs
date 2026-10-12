use super::*;

#[test]
fn test_has_horizontal_split() {
    let single_leaf = PaneNodeSnapshot::Leaf(LeafSnapshot {
        is_focused: false,
        custom_vertical_tabs_title: None,
        contents: LeafContents::Code(CodePaneSnapShot::Local {
            tabs: vec![CodePaneTabSnapshot {
                path: Some(PathBuf::new()),
            }],
            active_tab_index: 0,
            source: None,
        }),
    });
    assert!(!single_leaf.has_horizontal_split());

    let horizontal_split = PaneNodeSnapshot::Branch(BranchSnapshot {
        direction: SplitDirection::Horizontal,
        children: vec![
            (
                PaneFlex(1.),
                PaneNodeSnapshot::Leaf(LeafSnapshot {
                    is_focused: false,
                    custom_vertical_tabs_title: None,
                    contents: LeafContents::Code(CodePaneSnapShot::Local {
                        tabs: vec![CodePaneTabSnapshot {
                            path: Some(PathBuf::new()),
                        }],
                        active_tab_index: 0,
                        source: None,
                    }),
                }),
            ),
            (
                PaneFlex(1.),
                PaneNodeSnapshot::Leaf(LeafSnapshot {
                    is_focused: false,
                    custom_vertical_tabs_title: None,
                    contents: LeafContents::Code(CodePaneSnapShot::Local {
                        tabs: vec![CodePaneTabSnapshot {
                            path: Some(PathBuf::new()),
                        }],
                        active_tab_index: 0,
                        source: None,
                    }),
                }),
            ),
        ],
    });
    assert!(horizontal_split.has_horizontal_split());
}

#[test]
fn test_code_pane_snapshot_single_tab() {
    let snapshot = CodePaneSnapShot::Local {
        tabs: vec![CodePaneTabSnapshot {
            path: Some(PathBuf::from("/tmp/test.rs")),
        }],
        active_tab_index: 0,
        source: Some(CodeSource::FileTree {
            location: crate::code::buffer_location::LocalOrRemotePath::Local(PathBuf::from(
                "/tmp/test.rs",
            )),
        }),
    };
    let CodePaneSnapShot::Local {
        tabs,
        active_tab_index,
        source,
    } = &snapshot;
    assert_eq!(tabs.len(), 1);
    assert_eq!(*active_tab_index, 0);
    assert_eq!(tabs[0].path, Some(PathBuf::from("/tmp/test.rs")));
    assert!(matches!(source, Some(CodeSource::FileTree { .. })));
}

#[test]
fn test_code_pane_snapshot_with_multiple_tabs() {
    let snapshot = CodePaneSnapShot::Local {
        tabs: vec![
            CodePaneTabSnapshot {
                path: Some(PathBuf::from("/tmp/main.rs")),
            },
            CodePaneTabSnapshot {
                path: Some(PathBuf::from("/tmp/lib.rs")),
            },
            CodePaneTabSnapshot { path: None },
        ],
        active_tab_index: 1,
        source: Some(CodeSource::Link {
            path: PathBuf::from("/tmp/main.rs"),
            range_start: None,
            range_end: None,
        }),
    };
    let CodePaneSnapShot::Local {
        tabs,
        active_tab_index,
        source,
    } = &snapshot;
    assert_eq!(tabs.len(), 3);
    assert_eq!(*active_tab_index, 1);
    assert_eq!(tabs[0].path, Some(PathBuf::from("/tmp/main.rs")));
    assert_eq!(tabs[1].path, Some(PathBuf::from("/tmp/lib.rs")));
    assert_eq!(tabs[2].path, None);
    assert!(matches!(source, Some(CodeSource::Link { .. })));
}

fn terminal_leaf() -> PaneNodeSnapshot {
    PaneNodeSnapshot::Leaf(LeafSnapshot {
        is_focused: true,
        custom_vertical_tabs_title: None,
        contents: LeafContents::NetworkLog,
    })
}

fn tmux_leaf(pane: u32) -> PaneNodeSnapshot {
    PaneNodeSnapshot::Leaf(LeafSnapshot {
        is_focused: false,
        custom_vertical_tabs_title: None,
        contents: LeafContents::TmuxPane(TmuxPaneSnapshot {
            origin: TmuxOriginId::new_for_test(1),
            pane_id: warp_terminal::tmux::PaneId(pane),
            current_command: None,
        }),
    })
}

#[test]
fn tabs_with_tmux_panes_are_recognized() {
    assert!(!terminal_leaf().has_tmux_panes());
    assert!(tmux_leaf(1).has_tmux_panes());
    let split = PaneNodeSnapshot::Branch(BranchSnapshot {
        direction: SplitDirection::Horizontal,
        children: vec![(PaneFlex(1.), tmux_leaf(1)), (PaneFlex(1.), tmux_leaf(2))],
    });
    assert!(split.has_tmux_panes());
}

#[test]
fn saving_while_attached_keeps_the_origin_tab_active_in_place_of_tmux_tabs() {
    // Tabs: plain, hidden origin, tmux window 0 (active), tmux window 1, plain.
    let dropped = [false, false, true, true, false];
    // The active tmux tab hands off to the origin, saved as a normal tab at index 1.
    assert_eq!(persisted_active_index(&dropped, 2), 1);
    assert_eq!(persisted_active_index(&dropped, 3), 1);
    // Kept tabs shift left past the dropped ones.
    assert_eq!(persisted_active_index(&dropped, 4), 2);
    assert_eq!(persisted_active_index(&dropped, 0), 0);
    assert_eq!(persisted_active_index(&[true], 0), 0);
}
