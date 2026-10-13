use std::time::Duration;

use super::*;
use crate::tmux::layout::layout_checksum;
use crate::tmux::parser::CONTROL_MODE_DCS;

const TIME: u64 = 1_791_761_427;
const FIRST_NUMBER: u64 = 286;

const VERSION_QUERY: &str = "display-message -p '#{version}\t#{client_name}'\n";
const FLAGS: &str = "refresh-client -f pause-after=5,wait-exit\n";
const PREFIX: &str = "show -gv prefix\n";
const PREFIX2: &str = "show -gv prefix2\n";

fn list_windows() -> String {
    format!("list-windows -F '{LIST_WINDOWS_FORMAT}'\n")
}

fn list_panes() -> String {
    format!("list-panes -s -F '{LIST_PANES_FORMAT}'\n")
}

fn list_keys() -> String {
    format!("list-keys -F '{LIST_KEYS_FORMAT}'\n")
}

fn capture(pane: u32) -> String {
    format!("capture-pane -p -e -J -S -2000 -t %{pane}\n")
}

fn capture_screen(pane: u32) -> String {
    format!("capture-pane -p -e -t %{pane}\n")
}

fn cursor(pane: u32) -> String {
    format!("display-message -p -t %{pane} '#{{cursor_x}}\t#{{cursor_y}}'\n")
}

fn continue_pane(pane: u32) -> String {
    format!("refresh-client -A '%{pane}:continue'\n")
}

fn checked(body: &str) -> String {
    format!("{:04x},{body}", layout_checksum(body))
}

fn window_line(window: u32, body: &str, name: &str) -> String {
    let layout = checked(body);
    format!("@{window}\t{window}\t1\t{layout}\t{layout}\t*\t{name}")
}

fn pane_line(pane: u32, window: u32, alternate_on: bool) -> String {
    let alt = u8::from(alternate_on);
    format!("%{pane}\t@{window}\t1\t{alt}\t0\t80\t24\t")
}

fn key(name: &str, class: KeyClass, in_root_table: bool) -> ClientKey {
    ClientKey {
        key: name.to_owned(),
        pane_id: PaneId(1),
        class,
        in_root_table,
    }
}

struct Harness {
    client: ControlClient,
    now: Duration,
    number: u64,
}

impl Harness {
    fn new() -> Self {
        Self::with_config(ClientConfig::default())
    }

    fn with_config(config: ClientConfig) -> Self {
        Self {
            client: ControlClient::new(config),
            now: Duration::from_secs(100),
            number: FIRST_NUMBER,
        }
    }

    fn feed(&mut self, text: &str) -> Vec<u8> {
        self.client.receive_bytes(text.as_bytes(), self.now)
    }

    /// The DCS, tmux's reply to the attach command itself (flag 0), and `%session-changed`.
    fn enter(&mut self) {
        let mut bytes = CONTROL_MODE_DCS.to_vec();
        bytes.extend_from_slice(
            format!(
                "%begin {TIME} 280 0\n%end {TIME} 280 0\n%window-add @1\n%sessions-changed\n\
                 %session-changed $0 main\n"
            )
            .as_bytes(),
        );
        assert!(self.client.receive_bytes(&bytes, self.now).is_empty());
    }

    fn begin(&mut self) -> u64 {
        let number = self.number;
        self.number += 1;
        self.feed(&format!("%begin {TIME} {number} 1\n"));
        number
    }

    fn end(&mut self, number: u64, lines: &[&str], error: bool) {
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        let guard = if error { "%error" } else { "%end" };
        text.push_str(&format!("{guard} {TIME} {number} 1\n"));
        self.feed(&text);
    }

    fn reply(&mut self, lines: &[&str]) {
        let number = self.begin();
        self.end(number, lines, false);
    }

    fn reply_error(&mut self, lines: &[&str]) {
        let number = self.begin();
        self.end(number, lines, true);
    }

    fn out(&mut self) -> Vec<String> {
        self.client.take_outgoing()
    }

    fn events(&mut self) -> Vec<ClientEvent> {
        self.client.take_events()
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
    }

    fn pane_output(&mut self) -> Vec<(PaneId, Vec<u8>)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ClientEvent::PaneOutput { pane_id, bytes } => Some((pane_id, bytes)),
                _ => None,
            })
            .collect()
    }

    /// Runs the handshake for `version` with one window holding `panes` (all normal screen)
    /// and answers every pane seed with an empty screen. Returns with no pending events.
    fn attached(version: &str, panes: &[u32]) -> Self {
        let mut h = Self::new();
        h.attach_with(version, panes, &[]);
        h
    }

    fn attach_with(&mut self, version: &str, panes: &[u32], alt_panes: &[u32]) {
        self.enter();
        assert_eq!(self.out(), vec![VERSION_QUERY.to_owned()]);
        self.reply(&[&format!("{version}\tclient-7")]);
        let handshake = self.out();
        assert_eq!(handshake[0], FLAGS);
        self.reply(&[]);
        let body = layout_body(panes.iter().chain(alt_panes).copied().collect());
        self.reply(&[&window_line(1, &body, "main")]);
        let pane_lines: Vec<String> = panes
            .iter()
            .map(|&p| pane_line(p, 1, false))
            .chain(alt_panes.iter().map(|&p| pane_line(p, 1, true)))
            .collect();
        let pane_refs: Vec<&str> = pane_lines.iter().map(String::as_str).collect();
        self.reply(&pane_refs);
        self.reply(&["C-b"]);
        self.reply(&["None"]);
        if handshake.contains(&list_keys()) {
            self.reply(&[
                "root\t0\tC-h\tselect-pane -L",
                "prefix\t1\tUp\tselect-pane -U",
            ]);
        }
        let seeds = self.out();
        assert_eq!(
            seeds.len(),
            2 * (panes.len() + alt_panes.len()),
            "{seeds:?}"
        );
        for _ in panes.iter().chain(alt_panes) {
            self.reply(&[""]);
            self.reply(&["0\t0"]);
        }
        let events = self.events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ClientEvent::Attached { .. })),
            "{events:?}"
        );
        assert_eq!(self.client.phase(), Phase::Attached);
    }
}

/// A side-by-side layout over `panes`.
fn layout_body(panes: Vec<u32>) -> String {
    if let [pane] = panes[..] {
        return format!("80x24,0,0,{pane}");
    }
    let width = 80 / panes.len() as u32;
    let cells: Vec<String> = panes
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}x24,{},0,{p}", width - 1, i as u32 * width))
        .collect();
    format!("80x24,0,0{{{}}}", cells.join(","))
}

#[test]
fn handshake_on_3_7_queries_version_then_lists_prefixes_and_keys() {
    let mut h = Harness::new();
    h.enter();
    assert_eq!(h.client.phase(), Phase::Handshaking);
    assert_eq!(h.out(), vec![VERSION_QUERY.to_owned()]);

    h.reply(&["3.7c\tclient-7"]);
    assert_eq!(
        h.out(),
        vec![
            FLAGS.to_owned(),
            list_windows(),
            list_panes(),
            PREFIX.to_owned(),
            PREFIX2.to_owned(),
            list_keys(),
        ]
    );
    assert_eq!(h.client.client_name(), Some("client-7"));
    assert!(h.client.version().unwrap().supports_client_key_commands());

    h.reply(&[]);
    h.reply(&[&window_line(
        1,
        "80x24,0,0{40x24,0,0,1,39x24,41,0,2}",
        "main",
    )]);
    h.reply(&[&pane_line(1, 1, false), &pane_line(2, 1, false)]);
    assert_eq!(h.out(), vec![capture(1), cursor(1), capture(2), cursor(2)]);
    h.reply(&["C-b"]);
    h.reply(&["None"]);
    assert!(h.events().is_empty());
    h.reply(&[
        "root\t0\tC-h\tselect-pane -L",
        "prefix\t1\tUp\tselect-pane -U",
    ]);

    let expected_keys = KeyConfig {
        prefix: Some("C-b".into()),
        prefix2: None,
        bindings: Some(vec![
            KeyBinding {
                table: "root".into(),
                repeat: false,
                key: "C-h".into(),
                command: "select-pane -L".into(),
            },
            KeyBinding {
                table: "prefix".into(),
                repeat: true,
                key: "Up".into(),
                command: "select-pane -U".into(),
            },
        ]),
    };
    assert_eq!(
        h.events(),
        vec![
            ClientEvent::KeyConfigLoaded(expected_keys.clone()),
            ClientEvent::Attached {
                session_id: SessionId(0),
                session_name: "main".into(),
            },
        ]
    );
    assert_eq!(h.client.phase(), Phase::Attached);
    assert_eq!(h.client.key_config(), &expected_keys);
    let model = h.client.model();
    assert_eq!(
        model.window(WindowId(1)).unwrap().name.as_deref(),
        Some("main")
    );
    assert_eq!(model.panes().count(), 2);
}

#[test]
fn handshake_on_3_2_uses_pause_after_and_skips_key_table_queries() {
    let mut h = Harness::new();
    h.enter();
    h.out();
    h.reply(&["3.2a\tclient-7"]);
    let handshake = h.out();
    assert_eq!(handshake[0], FLAGS);
    assert!(handshake.iter().all(|line| !line.starts_with("list-keys")));
    h.reply(&[]);
    h.reply(&[&window_line(1, "80x24,0,0,1", "main")]);
    h.reply(&[&pane_line(1, 1, false)]);
    h.reply(&["C-a"]);
    h.reply(&["None"]);
    let events = h.events();
    assert!(matches!(
        &events[..],
        [
            ClientEvent::KeyConfigLoaded(KeyConfig { bindings: None, .. }),
            ClientEvent::Attached { .. }
        ]
    ));
    assert_eq!(h.client.key_config().prefix.as_deref(), Some("C-a"));

    assert_eq!(
        h.client
            .send_client_key(key("C-a", KeyClass::Other, true), h.now),
        Err(CommandError::ClientKeysUnsupported)
    );
    let all = h.out();
    assert!(
        all.iter().all(|line| !line.contains("send-keys -K")),
        "{all:?}"
    );
}

#[test]
fn tmux_older_than_3_2_is_unsupported_and_sends_nothing_else() {
    let mut h = Harness::new();
    h.enter();
    h.out();
    h.reply(&["3.1c\tclient-7"]);
    assert_eq!(
        h.events(),
        vec![ClientEvent::UnsupportedVersion {
            version: "3.1c".into()
        }]
    );
    assert_eq!(h.client.phase(), Phase::Unsupported);
    assert!(h.out().is_empty());
    h.feed("%output %1 hi\n");
    assert!(h.events().is_empty());
}

#[test]
fn unparseable_version_is_unsupported() {
    let mut h = Harness::new();
    h.enter();
    h.out();
    h.reply_error(&["unknown command"]);
    assert!(matches!(
        &h.events()[..],
        [ClientEvent::UnsupportedVersion { .. }]
    ));
}

#[test]
fn commands_must_be_one_non_empty_line() {
    let mut h = Harness::attached("3.7c", &[1]);
    assert_eq!(
        h.client.send_command("list-windows\nkill-server", h.now),
        Err(CommandError::NotOneLine)
    );
    assert_eq!(
        h.client.send_command("a\rb", h.now),
        Err(CommandError::NotOneLine)
    );
    assert_eq!(
        h.client.send_command("   ", h.now),
        Err(CommandError::NotOneLine)
    );
    assert!(h.out().is_empty());
    assert_eq!(
        h.client
            .send_client_key(key("C-b\n", KeyClass::Other, true), h.now),
        Err(CommandError::NotOneLine)
    );
    h.client.send_command("list-sessions", h.now).unwrap();
    h.client
        .send_pane_input(PaneId(1), &[b'\n'; 300], h.now)
        .unwrap();
    for line in h.out() {
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert!(line.trim().len() > 1);
    }
}

#[test]
fn paste_over_128_bytes_then_list_windows_routes_the_reply_by_command_number() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client
        .send_pane_input(PaneId(1), &[b'x'; 300], h.now)
        .unwrap();
    let id = h.client.send_command("list-windows", h.now).unwrap();
    let out = h.out();
    assert_eq!(out.len(), 4);
    assert!(out[..3].iter().all(|l| l.starts_with("send-keys -t %1 -H")));
    assert_eq!(out[3], "list-windows\n");

    // A block from another source (flag 0) does not consume a reply slot.
    h.feed(&format!("%begin {TIME} 900 0\nnoise\n%end {TIME} 900 0\n"));
    h.reply(&[]);
    h.reply(&[]);
    assert!(h.events().is_empty());
    h.reply(&[]);
    assert!(h.events().is_empty());
    h.number += 40;
    h.reply(&["@1: main"]);
    let events = h.events();
    let [ClientEvent::CommandCompleted { id: got, reply }] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(*got, id);
    assert_eq!(reply.lines, vec![b"@1: main".to_vec()]);
    assert!(!reply.error);
}

#[test]
fn user_command_errors_are_delivered_with_the_error_flag() {
    let mut h = Harness::attached("3.7c", &[1]);
    let id = h.client.send_command("bogus", h.now).unwrap();
    h.reply_error(&["parse error: unknown command: bogus"]);
    let events = h.events();
    let [ClientEvent::CommandCompleted { id: got, reply }] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(*got, id);
    assert!(reply.error);
}

#[test]
fn pause_continues_and_resyncs_that_pane_while_other_panes_flow() {
    let mut h = Harness::attached("3.7c", &[1, 2, 3]);
    h.feed("%pause %2\n");
    assert_eq!(h.out(), vec![continue_pane(2), capture(2), cursor(2)]);
    h.feed("%extended-output %1 12 : one\n%output %3 three\n");
    assert_eq!(
        h.pane_output(),
        vec![(PaneId(1), b"one".to_vec()), (PaneId(3), b"three".to_vec())]
    );

    h.reply(&[]);
    h.feed("%continue %2\n");
    assert!(
        h.out().is_empty(),
        "tmux's %continue for our resume must not resync twice"
    );
    h.reply(&["$ ls", "file"]);
    h.reply(&["0\t2"]);
    let events = h.events();
    assert_eq!(
        events,
        vec![ClientEvent::PaneSnapshot(PaneSnapshot {
            pane_id: PaneId(2),
            kind: SnapshotKind::Resync,
            lines: vec![b"$ ls".to_vec(), b"file".to_vec()],
            cursor: Some((0, 2)),
        })]
    );
    h.feed("%output %2 after\n");
    assert_eq!(h.pane_output(), vec![(PaneId(2), b"after".to_vec())]);
}

#[test]
fn a_continue_that_warp_did_not_request_resyncs_the_pane() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%continue %1\n");
    assert_eq!(h.out(), vec![capture(1), cursor(1)]);
}

#[test]
fn pause_and_resume_requested_by_the_app() {
    let mut h = Harness::attached("3.7c", &[1, 3]);
    h.client.pause_pane(PaneId(3), h.now);
    assert_eq!(h.out(), vec!["refresh-client -A '%3:pause'\n".to_owned()]);
    h.reply(&[]);
    h.client.resume_pane(PaneId(3), h.now);
    assert_eq!(h.out(), vec![continue_pane(3), capture(3), cursor(3)]);
}

#[test]
fn output_during_a_seed_is_applied_after_it_in_order_with_no_duplicate_at_the_seam() {
    let mut h = Harness::new();
    h.enter();
    h.out();
    h.reply(&["3.7c\tclient-7"]);
    h.out();
    h.reply(&[]);
    h.reply(&[&window_line(1, "80x24,0,0,4", "main")]);
    // Output before the pane list is covered by the capture that follows it.
    h.feed("%output %4 L0\\015\\012\n");
    h.reply(&[&pane_line(4, 1, false)]);
    assert_eq!(h.out(), vec![capture(4), cursor(4)]);
    h.reply(&["C-b"]);
    h.reply(&["None"]);
    h.reply(&[]);
    h.events();

    // tmux writes output it read before running `capture-pane` ahead of the reply, so that
    // output is already in the capture (recorded from tmux 3.7c).
    h.feed("%output %4 L1\\015\\012\n");
    let capture_number = h.begin();
    h.end(capture_number, &["L0", "L1", ""], false);
    // Output after the reply is newer than the capture.
    h.feed("%output %4 L2\\015\\012\n");
    h.feed("%output %4 L3\\015\\012\n");
    assert!(h.events().is_empty());
    h.reply(&["0\t2"]);

    let events = h.events();
    assert_eq!(
        events,
        vec![
            ClientEvent::PaneSnapshot(PaneSnapshot {
                pane_id: PaneId(4),
                kind: SnapshotKind::Seed,
                lines: vec![b"L0".to_vec(), b"L1".to_vec(), b"".to_vec()],
                cursor: Some((0, 2)),
            }),
            ClientEvent::PaneOutput {
                pane_id: PaneId(4),
                bytes: b"L2\r\n".to_vec(),
            },
            ClientEvent::PaneOutput {
                pane_id: PaneId(4),
                bytes: b"L3\r\n".to_vec(),
            },
        ]
    );
    h.feed("%output %4 L4\n");
    assert_eq!(h.pane_output(), vec![(PaneId(4), b"L4".to_vec())]);
}

#[test]
fn buffered_output_over_the_cap_resyncs_instead_of_truncating() {
    let mut h = Harness::with_config(ClientConfig {
        max_buffered_output_bytes: 8,
        ..ClientConfig::default()
    });
    h.attach_with("3.7c", &[1], &[]);
    h.feed("%pause %1\n");
    h.out();
    h.reply(&[]);
    h.reply(&["old"]);
    h.feed("%output %1 0123456789\n");
    h.reply(&["0\t0"]);
    assert!(h.events().is_empty());
    assert_eq!(h.out(), vec![capture(1), cursor(1)]);
    h.reply(&["new"]);
    h.reply(&["3\t0"]);
    assert_eq!(
        h.events(),
        vec![ClientEvent::PaneSnapshot(PaneSnapshot {
            pane_id: PaneId(1),
            kind: SnapshotKind::Resync,
            lines: vec![b"new".to_vec()],
            cursor: Some((3, 0)),
        })]
    );
}

#[test]
fn an_oversized_capture_is_retaken_without_history() {
    let mut h = Harness::with_config(ClientConfig {
        max_snapshot_bytes: 10,
        ..ClientConfig::default()
    });
    h.attach_with("3.7c", &[1], &[]);
    h.feed("%continue %1\n");
    assert_eq!(h.out(), vec![capture(1), cursor(1)]);
    h.reply(&["0123456789", "0123456789"]);
    h.reply(&["0\t0"]);
    assert!(h.events().is_empty());
    assert_eq!(h.out(), vec![capture_screen(1), cursor(1)]);
    h.reply(&["short"]);
    h.reply(&["5\t0"]);
    assert!(matches!(
        &h.events()[..],
        [ClientEvent::PaneSnapshot(PaneSnapshot {
            kind: SnapshotKind::Resync,
            ..
        })]
    ));
}

#[test]
fn alt_screen_panes_are_repainted_instead_of_seeded() {
    let mut h = Harness::new();
    h.enter();
    h.out();
    h.reply(&["3.7c\tclient-7"]);
    h.out();
    h.reply(&[]);
    h.reply(&[&window_line(
        1,
        "80x24,0,0{40x24,0,0,1,39x24,41,0,2}",
        "main",
    )]);
    h.reply(&[&pane_line(1, 1, false), &pane_line(2, 1, true)]);
    assert_eq!(
        h.out(),
        vec![capture(1), cursor(1), capture_screen(2), cursor(2)]
    );
    h.reply(&["C-b"]);
    h.reply(&["None"]);
    h.reply(&[]);
    h.events();
    h.reply(&["$ "]);
    h.reply(&["2\t0"]);
    h.reply(&["top"]);
    h.reply(&["0\t0"]);
    let events = h.events();
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::PaneSnapshot(s) => Some((s.pane_id, s.kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            (PaneId(1), SnapshotKind::Seed),
            (PaneId(2), SnapshotKind::Repaint)
        ]
    );
}

#[test]
fn snapshots_render_to_vt_bytes() {
    let seed = PaneSnapshot {
        pane_id: PaneId(1),
        kind: SnapshotKind::Seed,
        lines: vec![b"a".to_vec(), b"b".to_vec()],
        cursor: Some((1, 0)),
    };
    assert_eq!(seed.to_vt_bytes(), b"a\r\nb\x1b[1;2H".to_vec());
    let repaint = PaneSnapshot {
        kind: SnapshotKind::Repaint,
        cursor: None,
        ..seed
    };
    assert_eq!(
        repaint.to_vt_bytes(),
        b"\x1b[?1049h\x1b[H\x1b[2Ja\r\nb".to_vec()
    );
}

#[test]
fn eof_without_exit_is_transport_lost() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client.transport_closed();
    assert_eq!(h.events(), vec![ClientEvent::TransportLost]);
    assert_eq!(h.client.phase(), Phase::Disconnected);
    h.client.transport_closed();
    assert!(h.events().is_empty());
}

#[test]
fn exit_after_attach_is_distinct_from_eof_and_acknowledges_wait_exit() {
    let mut h = Harness::attached("3.7c", &[1]);
    let shell = h.feed("%exit detached\n\x1b\\$ ");
    assert_eq!(shell, b"$ ".to_vec());
    assert_eq!(
        h.events(),
        vec![ClientEvent::Exited {
            reason: Some("detached".into())
        }]
    );
    assert_eq!(h.out(), vec!["\n".to_owned()]);
    assert_eq!(h.client.phase(), Phase::Exited);
    h.client.transport_closed();
    assert!(h.events().is_empty());
    assert_eq!(
        h.client.send_command("ls", h.now),
        Err(CommandError::Closed)
    );
}

#[test]
fn exit_before_session_changed_is_attach_failed_with_the_preceding_error() {
    let mut h = Harness::new();
    let mut bytes = CONTROL_MODE_DCS.to_vec();
    bytes.extend_from_slice(
        format!("%begin {TIME} 280 0\nno sessions\n%error {TIME} 280 0\n").as_bytes(),
    );
    h.client.receive_bytes(&bytes, h.now);
    // The attach failed, so the handshake never starts: tmux exits and anything written now would
    // reach the shell.
    assert!(h.out().is_empty());
    h.feed("%exit\n");
    assert_eq!(
        h.events(),
        vec![ClientEvent::AttachFailed {
            error: Some("no sessions".into())
        }]
    );
    // tmux was never asked to wait for an acknowledgement, so nothing is written to the shell.
    assert!(h.out().is_empty());
}

#[test]
fn detach_writes_one_bare_newline_last() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client.detach();
    h.client.detach();
    assert_eq!(h.out(), vec!["\n".to_owned()]);
    assert_eq!(
        h.client.send_command("ls", h.now),
        Err(CommandError::Closed)
    );
    h.feed("%exit\n");
    assert!(h.out().is_empty());
    assert!(matches!(
        &h.events()[..],
        [ClientEvent::Exited { reason: None }]
    ));
}

#[test]
fn a_burst_of_window_adds_issues_one_refresh_and_no_per_window_queries() {
    let mut h = Harness::attached("3.7c", &[1]);
    let mut burst = String::new();
    for window in 2..302 {
        burst.push_str(&format!("%window-add @{window}\n"));
    }
    h.feed(&burst);
    let added = h
        .events()
        .into_iter()
        .filter(|e| matches!(e, ClientEvent::WindowAdded { .. }))
        .count();
    assert_eq!(added, 300);
    assert_eq!(h.out(), vec![list_windows(), list_panes()]);

    // Adds that arrive before the refresh runs are covered by it.
    h.feed("%window-add @302\n");
    let rows: Vec<String> = (1..303)
        .map(|w| window_line(w, &format!("80x24,0,0,{}", w + 100), &format!("w{w}")))
        .collect();
    let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    h.reply(&row_refs);
    h.reply(&[]);
    assert!(h.out().is_empty());
    let renamed = h
        .events()
        .into_iter()
        .filter(|e| matches!(e, ClientEvent::WindowRenamed { .. }))
        .count();
    assert_eq!(renamed, 302);
    assert_eq!(h.client.model().windows().count(), 302);
}

#[test]
fn a_window_add_after_the_refresh_starts_queues_one_more_refresh() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%window-add @2\n");
    assert_eq!(h.out().len(), 2);
    h.reply(&[]);
    h.feed("%window-add @3\n%window-add @4\n");
    h.reply(&[]);
    assert_eq!(h.out(), vec![list_windows(), list_panes()]);
}

#[test]
fn notifications_become_typed_events() {
    let mut h = Harness::attached("3.7c", &[1]);
    let split = checked("80x24,0,0{40x24,0,0,1,39x24,41,0,5}");
    h.feed(&format!(
        "%layout-change @1 {split} {split} *\n\
         %window-renamed @1 editor\n\
         %window-pane-changed @1 %5\n\
         %session-window-changed $0 @1\n\
         %session-window-changed $9 @7\n\
         %session-renamed $0 work\n\
         %message hello there\n\
         %config-error bad line\n\
         %unlinked-window-add @40\n\
         %window-close @1\n"
    ));
    let events = h.events();
    assert!(
        matches!(&events[0], ClientEvent::LayoutChanged { window_id: WindowId(1), layout }
        if layout.layout.pane_ids() == vec![PaneId(1), PaneId(5)])
    );
    assert_eq!(
        events[1..],
        [
            ClientEvent::WindowRenamed {
                window_id: WindowId(1),
                name: "editor".into()
            },
            ClientEvent::ActivePaneChanged {
                window_id: WindowId(1),
                pane_id: PaneId(5)
            },
            ClientEvent::ActiveWindowChanged {
                window_id: WindowId(1)
            },
            ClientEvent::SessionRenamed {
                name: "work".into()
            },
            ClientEvent::Message {
                text: "hello there".into()
            },
            ClientEvent::ConfigError {
                text: "bad line".into()
            },
            ClientEvent::WindowClosed {
                window_id: WindowId(1)
            },
        ]
    );
    assert!(h.client.model().window(WindowId(1)).is_none());
    assert!(h.client.model().window(WindowId(40)).is_none());
    assert!(h.out().is_empty());
}

#[test]
fn a_layout_that_does_not_parse_refreshes_the_windows() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%layout-change @1 0000,80x24,0,0,1 0000,80x24,0,0,1 *\n");
    assert!(h.events().is_empty());
    assert_eq!(h.out(), vec![list_windows(), list_panes()]);
}

#[test]
fn pane_mode_changes_are_queried_and_reported() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%pane-mode-changed %1\n");
    assert_eq!(
        h.out(),
        vec!["display-message -p -t %1 '#{pane_mode}'\n".to_owned()]
    );
    h.reply(&["copy-mode"]);
    assert_eq!(
        h.events(),
        vec![ClientEvent::PaneModeChanged {
            pane_id: PaneId(1),
            mode: Some("copy-mode".into())
        }]
    );
    assert_eq!(
        h.client.model().pane(PaneId(1)).unwrap().mode.as_deref(),
        Some("copy-mode")
    );
}

#[test]
fn switching_sessions_reloads_and_reseeds() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%session-changed $3 other\n");
    assert_eq!(h.out(), vec![list_windows(), list_panes()]);
    h.reply(&[&window_line(8, "80x24,0,0,9", "o")]);
    h.reply(&[&pane_line(9, 8, false)]);
    assert_eq!(
        h.events(),
        vec![ClientEvent::SessionChanged {
            session_id: SessionId(3),
            name: "other".into()
        }]
    );
    assert_eq!(h.out(), vec![capture(9), cursor(9)]);
    assert!(h.client.model().window(WindowId(1)).is_none());
    assert!(h.client.model().pane(PaneId(9)).is_some());
}

#[test]
fn a_fifth_client_key_waits_until_one_of_the_first_four_is_acknowledged() {
    let mut h = Harness::attached("3.7c", &[1]);
    for name in ["a", "b", "c", "d", "e"] {
        h.client
            .send_client_key(key(name, KeyClass::Other, true), h.now)
            .unwrap();
    }
    let out = h.out();
    assert_eq!(
        out,
        ["a", "b", "c", "d"]
            .map(|k| format!("send-keys -K -c \"client-7\" \"{k}\"\n"))
            .to_vec()
    );
    assert_eq!(h.client.unacknowledged_client_keys(), 4);
    assert_eq!(h.client.queued_input(), 1);
    h.reply(&[]);
    assert_eq!(
        h.out(),
        vec!["send-keys -K -c \"client-7\" \"e\"\n".to_owned()]
    );
    assert_eq!(h.client.unacknowledged_client_keys(), 4);
    assert_eq!(h.client.queued_input(), 0);
}

#[test]
fn key_names_are_quoted_for_tmux() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client
        .send_client_key(key("\"$\\", KeyClass::Other, true), h.now)
        .unwrap();
    assert_eq!(
        h.out(),
        vec!["send-keys -K -c \"client-7\" \"\\\"\\$\\\\\"\n".to_owned()]
    );
}

#[test]
fn a_stale_queued_navigation_key_is_dropped_but_one_sent_in_the_prefix_table_is_not() {
    let mut h = Harness::attached("3.7c", &[1]);
    for name in ["a", "b", "c", "d"] {
        h.client
            .send_client_key(key(name, KeyClass::Other, true), h.now)
            .unwrap();
    }
    h.out();
    let stale_nav = key("C-h", KeyClass::Navigation, true);
    let prefix_table_nav = key("Up", KeyClass::Navigation, false);
    let stale_other = key("x", KeyClass::Other, true);
    for k in [&stale_nav, &prefix_table_nav, &stale_other] {
        h.client.send_client_key(k.clone(), h.now).unwrap();
    }
    h.advance(Duration::from_millis(1500));
    h.reply(&[]);
    assert_eq!(
        h.events(),
        vec![
            ClientEvent::ClientKeyAcknowledged,
            ClientEvent::ClientKeyDropped { key: stale_nav }
        ]
    );
    assert_eq!(
        h.out(),
        vec!["send-keys -K -c \"client-7\" \"Up\"\n".to_owned()]
    );
    h.reply(&[]);
    assert_eq!(
        h.out(),
        vec!["send-keys -K -c \"client-7\" \"x\"\n".to_owned()]
    );
}

#[test]
fn a_fresh_queued_navigation_key_is_not_dropped() {
    let mut h = Harness::attached("3.7c", &[1]);
    for name in ["a", "b", "c", "d", "C-h"] {
        h.client
            .send_client_key(key(name, KeyClass::Navigation, true), h.now)
            .unwrap();
    }
    h.out();
    h.advance(Duration::from_millis(900));
    h.reply(&[]);
    assert_eq!(h.out().len(), 1);
    assert_eq!(h.events(), vec![ClientEvent::ClientKeyAcknowledged]);
}

#[test]
fn a_stale_navigation_key_for_a_pane_in_a_mode_is_not_dropped() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%pane-mode-changed %1\n");
    h.reply(&["copy-mode"]);
    h.events();
    for name in ["a", "b", "c", "d", "C-h"] {
        h.client
            .send_client_key(key(name, KeyClass::Navigation, true), h.now)
            .unwrap();
    }
    h.out();
    h.advance(Duration::from_secs(2));
    h.reply(&[]);
    assert_eq!(h.events(), vec![ClientEvent::ClientKeyAcknowledged]);
    assert_eq!(
        h.out(),
        vec!["send-keys -K -c \"client-7\" \"C-h\"\n".to_owned()]
    );
}

#[test]
fn pane_input_keeps_its_place_behind_queued_keys() {
    let mut h = Harness::attached("3.7c", &[1]);
    for name in ["a", "b", "c", "d", "e"] {
        h.client
            .send_client_key(key(name, KeyClass::Other, true), h.now)
            .unwrap();
    }
    h.out();
    h.client.send_pane_input(PaneId(1), b"hi", h.now).unwrap();
    assert!(h.out().is_empty());
    h.reply(&[]);
    assert_eq!(
        h.out(),
        vec![
            "send-keys -K -c \"client-7\" \"e\"\n".to_owned(),
            "send-keys -t %1 -H 68 69\n".to_owned(),
        ]
    );
}

#[test]
fn a_command_unanswered_for_3_seconds_raises_not_responding_and_a_late_reply_clears_it() {
    let mut h = Harness::attached("3.7c", &[1]);
    assert_eq!(h.client.next_deadline(), None);
    let sent_at = h.now;
    h.client
        .send_command("run-shell 'sleep 10'", h.now)
        .unwrap();
    assert_eq!(
        h.client.next_deadline(),
        Some(sent_at + Duration::from_secs(3))
    );
    h.advance(Duration::from_millis(2900));
    h.client.tick(h.now);
    assert!(h.events().is_empty());
    h.advance(Duration::from_millis(200));
    h.client.tick(h.now);
    assert_eq!(
        h.events(),
        vec![ClientEvent::NotResponding {
            command: "run-shell 'sleep 10'".into()
        }]
    );
    h.client.tick(h.now);
    assert!(h.events().is_empty());
    assert_eq!(h.client.next_deadline(), None);

    h.advance(Duration::from_secs(5));
    h.reply(&[]);
    let events = h.events();
    assert_eq!(events.last(), Some(&ClientEvent::Responding));
}

#[test]
fn a_malformed_begin_block_raises_desync_and_stops_pane_output() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client.send_command("list-windows", h.now).unwrap();
    h.feed(&format!(
        "%begin {TIME} 900 1\n%begin {TIME} 901 1\nx\n%end {TIME} 901 1\n%end {TIME} 900 1\n"
    ));
    let events = h.events();
    assert!(
        matches!(&events[..], [ClientEvent::Desync { .. }]),
        "{events:?}"
    );
    assert_eq!(h.client.phase(), Phase::Disconnected);
    h.feed("%output %1 garbage\n");
    assert!(h.events().is_empty());
    assert_eq!(
        h.client.send_command("ls", h.now),
        Err(CommandError::Closed)
    );
}

#[test]
fn a_reply_with_no_outstanding_command_is_a_desync() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.reply(&["surprise"]);
    assert!(matches!(&h.events()[..], [ClientEvent::Desync { .. }]));
}

#[test]
fn a_reply_whose_end_does_not_match_its_begin_is_a_desync() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client.send_command("ls", h.now).unwrap();
    h.client.handle_event(
        ControlEvent::CommandBegin {
            time: TIME,
            number: 950,
            flags: 1,
        },
        h.now,
    );
    h.client.handle_event(
        ControlEvent::CommandEnd(CommandReply {
            time: TIME,
            number: 951,
            flags: 1,
            error: false,
            lines: Vec::new(),
        }),
        h.now,
    );
    assert!(matches!(&h.events()[..], [ClientEvent::Desync { .. }]));
}

#[test]
fn a_parser_overflow_is_a_desync() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client.handle_event(ControlEvent::ProtocolOverflow, h.now);
    assert!(matches!(&h.events()[..], [ClientEvent::Desync { .. }]));
    assert_eq!(h.client.phase(), Phase::Disconnected);
}

#[test]
fn output_for_panes_created_after_attach_flows_without_a_seed() {
    let mut h = Harness::attached("3.7c", &[1]);
    let split = checked("80x24,0,0{40x24,0,0,1,39x24,41,0,6}");
    h.feed(&format!(
        "%layout-change @1 {split} {split} *\n%output %6 new\n"
    ));
    assert_eq!(h.pane_output(), vec![(PaneId(6), b"new".to_vec())]);
    assert!(h.out().is_empty());
}

/// Attaches to a real tmux on an isolated socket, lists windows and detaches.
#[test]
fn live_attach_list_and_detach() {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::mpsc;

    use command::blocking::Command;
    use instant::Instant;

    let Some(version) = Command::new("tmux")
        .arg("-V")
        .output()
        .ok()
        .and_then(|out| TmuxVersion::parse(&String::from_utf8_lossy(&out.stdout)))
    else {
        eprintln!("skipping: tmux not found");
        return;
    };
    if !version.supports_client_key_commands() {
        eprintln!("skipping: tmux is older than 3.4");
        return;
    }

    let socket = format!("warp-u3-{}", std::process::id());
    struct KillServer {
        socket: String,
        path: Option<String>,
    }
    impl Drop for KillServer {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.socket, "kill-server"])
                .output();
            if let Some(path) = &self.path {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let mut cleanup = KillServer {
        socket: socket.clone(),
        path: None,
    };

    let mut child = Command::new("tmux")
        .args(["-L", &socket, "-f", "/dev/null", "-C", "new-session"])
        .args(["-x", "80", "-y", "24", "sh -c 'echo warp-live; sleep 30'"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tmux");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let start = Instant::now();
    let mut client = ControlClient::new(ClientConfig::default());
    // `-C` (unlike `-CC`) does not write the DCS; supply it so the engine starts its handshake.
    client.receive_bytes(CONTROL_MODE_DCS, start.elapsed());
    let mut list_id = None;
    let mut socket_path_id = None;
    let mut listed = false;
    let mut exited = false;
    let mut eof = false;
    while start.elapsed() < Duration::from_secs(10) && !eof {
        for line in client.take_outgoing() {
            stdin.write_all(line.as_bytes()).unwrap();
        }
        stdin.flush().unwrap();
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(bytes)) => {
                client.receive_bytes(&bytes, start.elapsed());
            }
            Ok(None) => {
                eof = true;
                client.transport_closed();
            }
            Err(_) => client.tick(start.elapsed()),
        }
        for event in client.take_events() {
            match event {
                ClientEvent::Attached { .. } => {
                    let now = start.elapsed();
                    let query = "display-message -p '#{socket_path}'";
                    socket_path_id = Some(client.send_command(query, now).unwrap());
                    list_id = Some(client.send_command("list-windows", now).unwrap());
                }
                ClientEvent::CommandCompleted { id, reply } if Some(id) == socket_path_id => {
                    let path = reply.lines.first().map(|l| String::from_utf8_lossy(l));
                    cleanup.path = path
                        .filter(|path| path.contains(&socket))
                        .map(|path| path.into_owned());
                }
                ClientEvent::CommandCompleted { id, reply } if Some(id) == list_id => {
                    assert!(!reply.error);
                    assert_eq!(reply.lines.len(), 1);
                    listed = true;
                    client.detach();
                }
                ClientEvent::Exited { .. } => exited = true,
                ClientEvent::Desync { reason } => panic!("desync: {reason}"),
                ClientEvent::AttachFailed { error } => panic!("attach failed: {error:?}"),
                _ => {}
            }
        }
        if exited {
            for line in client.take_outgoing() {
                let _ = stdin.write_all(line.as_bytes());
            }
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(listed, "list-windows reply never arrived");
    assert!(exited, "tmux never reported %exit after detach");
    assert_eq!(client.model().panes().count(), 1);
}

#[test]
fn each_answered_client_key_is_reported_so_the_app_can_fence_it() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.events();
    h.client
        .send_client_key(key("C-b", KeyClass::Other, true), h.now)
        .unwrap();
    h.out();
    assert!(h.events().is_empty());
    h.reply(&[]);
    assert_eq!(h.events(), vec![ClientEvent::ClientKeyAcknowledged]);
}

#[test]
fn list_keys_lines_parse_into_bindings() {
    assert_eq!(
        parse_key_binding_line(b"root\t0\tC-h\tif-shell -F x \"send-keys C-h\" \"select-pane -L\""),
        Some(KeyBinding {
            table: "root".into(),
            repeat: false,
            key: "C-h".into(),
            command: "if-shell -F x \"send-keys C-h\" \"select-pane -L\"".into(),
        })
    );
    assert_eq!(parse_key_binding_line(b"garbage"), None);
}

#[test]
fn a_failed_new_session_writes_nothing_and_reports_the_error() {
    let mut h = Harness::new();
    let mut bytes = CONTROL_MODE_DCS.to_vec();
    bytes.extend_from_slice(format!("%begin {TIME} 284 0\n").as_bytes());
    h.client.receive_bytes(&bytes, h.now);
    assert!(h.out().is_empty());
    h.feed(&format!("duplicate session: test\n%error {TIME} 284 0\n"));
    assert!(h.out().is_empty());
    assert_eq!(
        h.client.send_command("list-sessions", h.now),
        Err(CommandError::Closed)
    );
    h.feed("%exit\n");
    assert!(h.out().is_empty());
    assert_eq!(
        h.events(),
        vec![ClientEvent::AttachFailed {
            error: Some("duplicate session: test".into())
        }]
    );
    assert_eq!(h.client.phase(), Phase::Exited);
}

#[test]
fn the_handshake_starts_only_after_the_attach_command_succeeds() {
    let mut h = Harness::new();
    let mut bytes = CONTROL_MODE_DCS.to_vec();
    bytes.extend_from_slice(format!("%begin {TIME} 286 0\n").as_bytes());
    h.client.receive_bytes(&bytes, h.now);
    assert!(h.out().is_empty());
    h.feed(&format!("%end {TIME} 286 0\n"));
    assert_eq!(h.out(), vec![VERSION_QUERY.to_owned()]);
    h.feed("%window-add @1\n%sessions-changed\n%session-changed $1 other\n");
    assert!(h.out().is_empty());
}

#[test]
fn without_an_attach_reply_the_handshake_starts_on_session_changed() {
    let mut h = Harness::new();
    let mut bytes = CONTROL_MODE_DCS.to_vec();
    bytes.extend_from_slice(b"%window-add @1\n");
    h.client.receive_bytes(&bytes, h.now);
    assert!(h.out().is_empty());
    h.feed("%session-changed $0 main\n");
    assert_eq!(h.out(), vec![VERSION_QUERY.to_owned()]);
}

#[test]
fn a_desync_detaches_so_tmux_exits_and_the_shell_gets_the_pty_back() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed(&format!("%begin {TIME} 9999 1\n"));
    assert_eq!(h.out(), vec!["\n".to_owned()]);
    assert!(
        h.events()
            .iter()
            .any(|e| matches!(e, ClientEvent::Desync { .. }))
    );
    assert_eq!(h.client.phase(), Phase::Disconnected);
    assert!(h.client.shell_pending().is_none());
    assert_eq!(
        h.feed(&format!("%end {TIME} 9999 1\n%exit\n\x1b\\$ ")),
        b"$ "
    );
    assert!(h.client.shell_pending().is_some());
}

#[test]
fn a_desync_after_the_transport_is_gone_writes_nothing() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.client.transport_closed();
    h.client.desync("late".into());
    assert!(h.out().is_empty());
}

#[test]
fn shell_output_on_the_control_stream_is_transport_lost_and_flows_to_the_shell() {
    let mut h = Harness::attached("3.7c", &[1]);
    let dropped = "Connection to host closed.\r\n\x1b]133;A\x07$ ";
    assert_eq!(h.feed(dropped), dropped.as_bytes());
    assert_eq!(h.events(), vec![ClientEvent::TransportLost]);
    assert!(h.client.transport_lost());
    assert!(h.client.shell_pending().is_some());
    h.client.detach();
    assert!(h.out().is_empty());
}

#[test]
fn capture_text_that_looks_like_notifications_stays_pane_text() {
    let mut h = Harness::attached("3.7c", &[1]);
    h.feed("%pause %1\n");
    assert_eq!(h.out(), vec![continue_pane(1), capture(1), cursor(1)]);
    h.reply(&[]);
    h.reply(&["%exit", "%window-close @1", "%output %1 x", "%message hi"]);
    h.reply(&["0\t0"]);
    assert_eq!(
        h.events(),
        vec![ClientEvent::PaneSnapshot(PaneSnapshot {
            pane_id: PaneId(1),
            kind: SnapshotKind::Resync,
            lines: vec![
                b"%exit".to_vec(),
                b"%window-close @1".to_vec(),
                b"%output %1 x".to_vec(),
                b"%message hi".to_vec(),
            ],
            cursor: Some((0, 0)),
        })]
    );
    assert_eq!(h.client.phase(), Phase::Attached);
}

#[test]
fn quoted_values_cannot_expand_in_a_shell() {
    let value = "a`touch /nonexistent/x`$(id)${HOME}\"\\b";
    let quoted = quote_argument(value);
    assert_eq!(
        quoted,
        "\"a\\`touch /nonexistent/x\\`\\$(id)\\${HOME}\\\"\\\\b\""
    );
    let output = command::blocking::Command::new("sh")
        .arg("-c")
        .arg(format!("printf %s {quoted}"))
        .output()
        .expect("run sh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), value);
}
