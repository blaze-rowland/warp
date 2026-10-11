use super::*;
use crate::tmux::layout::layout_checksum;

fn checked(body: &str) -> String {
    format!("{:04x},{body}", layout_checksum(body))
}

fn layout(body: &str) -> WindowLayout {
    let layout = checked(body);
    parse_layout_change(&layout, None, &WindowFlags::default()).unwrap()
}

fn window_row(id: u32, index: u32, active: bool, body: &str, name: &str) -> WindowRow {
    WindowRow {
        id: WindowId(id),
        index,
        active,
        layout: Some(layout(body)),
        name: name.to_owned(),
    }
}

fn pane_row(id: u32, window: u32, active: bool) -> PaneRow {
    PaneRow {
        id: PaneId(id),
        window_id: WindowId(window),
        active,
        alternate_on: false,
        mode: None,
        width: 80,
        height: 24,
    }
}

#[test]
fn parses_a_window_row_recorded_from_tmux_3_7c() {
    let row = parse_window_row(b"@0\t0\t1\tb25d,80x24,0,0,0\tb25d,80x24,0,0,0\t*\tsleep").unwrap();
    assert_eq!(row.id, WindowId(0));
    assert_eq!(row.index, 0);
    assert!(row.active);
    assert_eq!(row.name, "sleep");
    let layout = row.layout.unwrap();
    assert_eq!(layout.layout.pane_ids(), vec![PaneId(0)]);
    assert!(layout.flags.is_active());
}

#[test]
fn window_names_keep_tabs_and_spaces() {
    let line = format!(
        "@3\t2\t0\t{}\t{}\t-\tmy\tname here",
        checked("80x24,0,0,7"),
        checked("80x24,0,0,7")
    );
    let row = parse_window_row(line.as_bytes()).unwrap();
    assert_eq!(row.name, "my\tname here");
    assert!(!row.active);
}

#[test]
fn a_bad_layout_keeps_the_row_without_a_layout() {
    let row = parse_window_row(b"@1\t1\t0\t0000,80x24,0,0,1\t0000,80x24,0,0,1\t-\tx").unwrap();
    assert_eq!(row.layout, None);
}

#[test]
fn rejects_malformed_window_rows() {
    assert_eq!(parse_window_row(b""), None);
    assert_eq!(parse_window_row(b"1\t0\t1\tx\tx\t*\tname"), None);
    assert_eq!(parse_window_row(b"@1\tzero\t1\tx\tx\t*\tname"), None);
    assert_eq!(parse_window_row(b"@1\t0\t1"), None);
}

#[test]
fn parses_pane_rows_recorded_from_tmux_3_7c() {
    let row = parse_pane_row(b"%0\t@0\t1\t0\t0\t80\t24\t").unwrap();
    assert_eq!(row, pane_row(0, 0, true));

    let row = parse_pane_row(b"%5\t@2\t0\t1\t1\t40\t12\tcopy-mode").unwrap();
    assert_eq!(row.id, PaneId(5));
    assert_eq!(row.window_id, WindowId(2));
    assert!(row.alternate_on);
    assert_eq!(row.mode.as_deref(), Some("copy-mode"));
    assert_eq!((row.width, row.height), (40, 12));
}

#[test]
fn rejects_malformed_pane_rows() {
    assert_eq!(parse_pane_row(b"%0\t@0\t1"), None);
    assert_eq!(parse_pane_row(b"0\t@0\t1\t0\t0\t80\t24\t"), None);
    assert_eq!(parse_pane_row(b"%0\t@0\t1\t0\t0\twide\t24\t"), None);
}

#[test]
fn switching_sessions_clears_windows_and_panes() {
    let mut model = SessionModel::default();
    assert!(!model.set_session(SessionId(0), "main".into()));
    model.apply_layout(WindowId(1), layout("80x24,0,0,1"));
    assert!(!model.set_session(SessionId(0), "main".into()));
    assert!(model.window(WindowId(1)).is_some());

    assert!(model.set_session(SessionId(4), "other".into()));
    assert_eq!(model.session().unwrap().name, "other");
    assert_eq!(model.windows().count(), 0);
    assert_eq!(model.panes().count(), 0);
}

#[test]
fn renames_only_the_attached_session() {
    let mut model = SessionModel::default();
    model.set_session(SessionId(0), "main".into());
    assert!(!model.rename_session(SessionId(9), "nope".into()));
    assert!(model.rename_session(SessionId(0), "work".into()));
    assert_eq!(model.session().unwrap().name, "work");
}

#[test]
fn window_add_rename_and_close() {
    let mut model = SessionModel::default();
    assert!(model.add_window(WindowId(2)));
    assert!(!model.add_window(WindowId(2)));
    assert!(model.rename_window(WindowId(2), "logs".into()));
    assert!(!model.rename_window(WindowId(2), "logs".into()));
    assert!(!model.rename_window(WindowId(9), "missing".into()));

    model.apply_layout(WindowId(2), layout("80x24,0,0{40x24,0,0,3,39x24,41,0,4}"));
    assert!(model.pane(PaneId(3)).is_some());
    let closed = model.close_window(WindowId(2)).unwrap();
    assert_eq!(closed.name.as_deref(), Some("logs"));
    assert!(model.pane(PaneId(3)).is_none());
    assert!(model.pane(PaneId(4)).is_none());
    assert!(model.close_window(WindowId(2)).is_none());
}

#[test]
fn layout_changes_report_added_and_removed_panes() {
    let mut model = SessionModel::default();
    let delta = model.apply_layout(WindowId(1), layout("80x24,0,0,1"));
    assert_eq!(delta.added, vec![PaneId(1)]);
    assert!(delta.removed.is_empty());

    let delta = model.apply_layout(WindowId(1), layout("80x24,0,0[80x12,0,0,1,80x11,0,13,2]"));
    assert_eq!(delta.added, vec![PaneId(2)]);
    assert!(delta.removed.is_empty());
    let pane = model.pane(PaneId(2)).unwrap();
    assert_eq!(
        (pane.window_id, pane.width, pane.height),
        (WindowId(1), 80, 11)
    );

    let delta = model.apply_layout(WindowId(1), layout("80x24,0,0,2"));
    assert!(delta.added.is_empty());
    assert_eq!(delta.removed, vec![PaneId(1)]);
    assert!(model.pane(PaneId(1)).is_none());
}

#[test]
fn a_pane_moving_windows_is_not_removed_from_its_new_window() {
    let mut model = SessionModel::default();
    model.apply_layout(WindowId(1), layout("80x24,0,0[80x12,0,0,1,80x11,0,13,2]"));
    // `break-pane`: tmux reports the new window first, then the old one without the pane.
    model.apply_layout(WindowId(2), layout("80x24,0,0,2"));
    let delta = model.apply_layout(WindowId(1), layout("80x24,0,0,1"));
    assert!(delta.removed.is_empty());
    assert_eq!(model.pane(PaneId(2)).unwrap().window_id, WindowId(2));
}

#[test]
fn active_window_and_pane_and_mode() {
    let mut model = SessionModel::default();
    model.apply_layout(WindowId(1), layout("80x24,0,0{40x24,0,0,1,39x24,41,0,2}"));
    model.set_active_window(WindowId(1));
    model.set_active_pane(WindowId(1), PaneId(2));
    assert_eq!(model.active_window(), Some(WindowId(1)));
    assert_eq!(
        model.window(WindowId(1)).unwrap().active_pane,
        Some(PaneId(2))
    );

    model.set_pane_mode(PaneId(2), Some("copy-mode".into()));
    assert_eq!(
        model.pane(PaneId(2)).unwrap().mode.as_deref(),
        Some("copy-mode")
    );
    model.set_pane_mode(PaneId(2), None);
    assert_eq!(model.pane(PaneId(2)).unwrap().mode, None);
}

#[test]
fn replacing_windows_reports_each_difference() {
    let mut model = SessionModel::default();
    model.add_window(WindowId(1));
    model.rename_window(WindowId(1), "old".into());
    model.add_window(WindowId(2));

    let changes = model.replace_windows(vec![
        window_row(1, 0, false, "80x24,0,0,1", "new"),
        window_row(3, 1, true, "80x24,0,0,5", "fresh"),
    ]);
    assert_eq!(
        changes,
        vec![
            WindowChange::Closed(WindowId(2)),
            WindowChange::Renamed(WindowId(1), "new".into()),
            WindowChange::LayoutChanged(WindowId(1)),
            WindowChange::Added(WindowId(3)),
        ]
    );
    assert_eq!(model.active_window(), Some(WindowId(3)));
    assert_eq!(model.window(WindowId(3)).unwrap().index, Some(1));
    assert!(model.pane(PaneId(5)).is_some());

    // The same rows again change nothing.
    let changes = model.replace_windows(vec![
        window_row(1, 0, false, "80x24,0,0,1", "new"),
        window_row(3, 1, true, "80x24,0,0,5", "fresh"),
    ]);
    assert!(changes.is_empty());
}

#[test]
fn replacing_panes_sets_state_and_active_panes() {
    let mut model = SessionModel::default();
    model.replace_windows(vec![window_row(
        1,
        0,
        true,
        "80x24,0,0{40x24,0,0,1,39x24,41,0,2}",
        "w",
    )]);
    let mut alt = pane_row(2, 1, true);
    alt.alternate_on = true;
    model.replace_panes(vec![pane_row(1, 1, false), alt]);
    assert!(model.pane(PaneId(2)).unwrap().alternate_on);
    assert!(!model.pane(PaneId(1)).unwrap().alternate_on);
    assert_eq!(
        model.window(WindowId(1)).unwrap().active_pane,
        Some(PaneId(2))
    );
    assert_eq!(model.panes().count(), 2);
}
