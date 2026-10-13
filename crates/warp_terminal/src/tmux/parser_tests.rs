use super::*;

const FIXTURE_3_2: &[u8] = include_bytes!("fixtures/tmux-3.2-handbuilt.cc");
const FIXTURE_3_4: &[u8] = include_bytes!("fixtures/tmux-3.4-handbuilt.cc");
const FIXTURE_3_7_SESSION: &[u8] = include_bytes!("fixtures/tmux-3.7c-session.cc");
const FIXTURE_3_7_NOTIFICATIONS: &[u8] = include_bytes!("fixtures/tmux-3.7c-notifications.cc");

/// Escape bytes the way tmux's `control_write_output` does: only bytes below 0x20 and `\` are
/// written as three-digit octal. Bytes >= 0x7f (including UTF-8) are sent raw.
fn tmux_escape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in bytes {
        if b < b' ' || b == b'\\' {
            out.extend_from_slice(format!("\\{b:03o}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

fn entered(rest: &[u8]) -> Vec<u8> {
    let mut bytes = CONTROL_MODE_DCS.to_vec();
    bytes.extend_from_slice(rest);
    bytes
}

/// Parse `rest` after the DCS and return the control events after `EnteredControlMode`.
fn events_after_entry(rest: &[u8]) -> Vec<ControlEvent> {
    let mut parser = ControlModeParser::new();
    let mut events = parser.push(&entered(rest));
    assert_eq!(events.first(), Some(&ControlEvent::EnteredControlMode));
    events.remove(0);
    events
}

fn output(pane: u32, bytes: &[u8]) -> ControlEvent {
    ControlEvent::PaneOutput {
        pane_id: PaneId(pane),
        age_ms: None,
        bytes: bytes.to_vec(),
    }
}

/// Feed `input` in chunks of `chunk` bytes and coalesce adjacent shell items, so the result is
/// comparable regardless of how reads were split.
fn decode_in_chunks(input: &[u8], chunk: usize) -> Vec<DecodeItem> {
    let mut parser = ControlModeParser::new();
    let mut items: Vec<DecodeItem> = Vec::new();
    for piece in input.chunks(chunk) {
        for item in parser.decode(piece) {
            match (items.last_mut(), item) {
                (Some(DecodeItem::Shell(prev)), DecodeItem::Shell(next)) => {
                    prev.extend_from_slice(&next)
                }
                (_, item) => items.push(item),
            }
        }
    }
    items
}

fn shell_bytes(items: &[DecodeItem]) -> Vec<u8> {
    items
        .iter()
        .filter_map(|item| match item {
            DecodeItem::Shell(bytes) => Some(bytes.clone()),
            DecodeItem::Control(_) => None,
        })
        .flatten()
        .collect()
}

fn controls(items: &[DecodeItem]) -> Vec<ControlEvent> {
    items
        .iter()
        .filter_map(|item| match item {
            DecodeItem::Control(event) => Some(event.clone()),
            DecodeItem::Shell(_) => None,
        })
        .collect()
}

/// Compact, order-preserving summary of an event for fixture assertions.
fn summary(event: &ControlEvent) -> String {
    use ControlEvent::*;
    match event {
        EnteredControlMode => "enter".into(),
        CommandBegin { number, .. } => format!("begin {number}"),
        CommandEnd(reply) => format!(
            "{} {} [{}]",
            if reply.error { "error" } else { "end" },
            reply.number,
            reply.lines.len()
        ),
        PaneOutput {
            pane_id,
            age_ms: None,
            ..
        } => format!("output {pane_id}"),
        PaneOutput {
            pane_id,
            age_ms: Some(_),
            ..
        } => format!("extended-output {pane_id}"),
        Pause { pane_id } => format!("pause {pane_id}"),
        Continue { pane_id } => format!("continue {pane_id}"),
        PaneModeChanged { pane_id } => format!("pane-mode-changed {pane_id}"),
        LayoutChange {
            window_id, flags, ..
        } => format!("layout {window_id} {}", flags.as_str()),
        WindowAdd { window_id } => format!("window-add {window_id}"),
        WindowClose { window_id } => format!("window-close {window_id}"),
        WindowRenamed { window_id, name } => format!("window-renamed {window_id} {name}"),
        UnlinkedWindowAdd { window_id } => format!("unlinked-window-add {window_id}"),
        UnlinkedWindowClose { window_id } => format!("unlinked-window-close {window_id}"),
        UnlinkedWindowRenamed { window_id, name } => {
            format!("unlinked-window-renamed {window_id} {name}")
        }
        WindowPaneChanged { window_id, pane_id } => {
            format!("window-pane-changed {window_id} {pane_id}")
        }
        SessionChanged { session_id, name } => format!("session-changed {session_id} {name}"),
        ClientSessionChanged {
            client,
            session_id,
            name,
        } => format!("client-session-changed {client} {session_id} {name}"),
        SessionRenamed { session_id, name } => format!("session-renamed {session_id} {name}"),
        SessionsChanged => "sessions-changed".into(),
        SessionWindowChanged {
            session_id,
            window_id,
        } => format!("session-window-changed {session_id} {window_id}"),
        SubscriptionChanged { name, value, .. } => format!("subscription-changed {name}={value}"),
        ClientDetached { client } => format!("client-detached {client}"),
        ConfigError { message } => format!("config-error {message}"),
        Message { message } => format!("message {message}"),
        PasteBufferChanged { name } => format!("paste-buffer-changed {name}"),
        PasteBufferDeleted { name } => format!("paste-buffer-deleted {name}"),
        Exit { reason: None } => "exit".into(),
        Exit {
            reason: Some(reason),
        } => format!("exit {reason}"),
        ProtocolOverflow => "overflow".into(),
        ControlLost => "control-lost".into(),
    }
}

fn summaries(items: &[DecodeItem]) -> Vec<String> {
    controls(items).iter().map(summary).collect()
}

/// Replays a fixture whole and at several chunkings; all must agree. Returns the whole-read items.
fn replay_fixture(fixture: &[u8]) -> Vec<DecodeItem> {
    let whole = decode_in_chunks(fixture, fixture.len());
    for chunk in [1, 2, 3, 7, 64] {
        assert_eq!(
            decode_in_chunks(fixture, chunk),
            whole,
            "chunk size {chunk} changed the decoded items"
        );
    }
    whole
}

fn pane_output(items: &[DecodeItem], pane: u32) -> Vec<u8> {
    controls(items)
        .into_iter()
        .filter_map(|event| match event {
            ControlEvent::PaneOutput { pane_id, bytes, .. } if pane_id == PaneId(pane) => {
                Some(bytes)
            }
            _ => None,
        })
        .flatten()
        .collect()
}

// --- DCS entry and exit -------------------------------------------------------------------------

#[test]
fn dcs_enters_control_mode() {
    let mut parser = ControlModeParser::new();
    assert!(!parser.is_in_control_mode());
    assert_eq!(
        parser.push(CONTROL_MODE_DCS),
        vec![ControlEvent::EnteredControlMode]
    );
    assert!(parser.is_in_control_mode());
}

#[test]
fn dcs_split_at_every_offset_is_detected_exactly_once() {
    let mut input = b"prompt$ tmux -CC\r\n".to_vec();
    input.extend_from_slice(CONTROL_MODE_DCS);
    input.extend_from_slice(b"%output %0 hi\r\n");
    let dcs_start = input.len() - CONTROL_MODE_DCS.len() - b"%output %0 hi\r\n".len();
    // Every single split point, and every pair of split points inside or around the DCS.
    let mut splits: Vec<Vec<usize>> = (0..=input.len()).map(|i| vec![i]).collect();
    for i in dcs_start..=dcs_start + CONTROL_MODE_DCS.len() {
        for j in i..=dcs_start + CONTROL_MODE_DCS.len() {
            splits.push(vec![i, j]);
        }
    }
    for points in splits {
        let mut parser = ControlModeParser::new();
        let mut items = Vec::new();
        let mut start = 0;
        for &point in points.iter().chain(std::iter::once(&input.len())) {
            items.extend(parser.decode(&input[start..point]));
            start = point;
        }
        let entries = items
            .iter()
            .filter(|item| matches!(item, DecodeItem::Control(ControlEvent::EnteredControlMode)))
            .count();
        assert_eq!(entries, 1, "splits {points:?}");
        assert_eq!(
            shell_bytes(&items),
            b"prompt$ tmux -CC\r\n",
            "splits {points:?}"
        );
        assert_eq!(
            controls(&items).last(),
            Some(&output(0, b"hi")),
            "splits {points:?}"
        );
    }
}

#[test]
fn partial_dcs_prefix_that_does_not_complete_is_released_as_shell() {
    let mut parser = ControlModeParser::new();
    assert!(parser.decode(b"ab\x1bP10").iter().all(|item| match item {
        DecodeItem::Shell(bytes) => bytes == b"ab",
        DecodeItem::Control(_) => false,
    }));
    let items = parser.decode(b"x");
    assert_eq!(items, vec![DecodeItem::Shell(b"\x1bP10x".to_vec())]);
    assert!(!parser.is_in_control_mode());
}

#[test]
fn shell_then_dcs_then_output_in_one_buffer() {
    let mut parser = ControlModeParser::new();
    let mut bytes = b"prompt$ ".to_vec();
    bytes.extend_from_slice(CONTROL_MODE_DCS);
    bytes.extend_from_slice(b"%output %0 hi\n");
    assert_eq!(
        parser.decode(&bytes),
        vec![
            DecodeItem::Shell(b"prompt$ ".to_vec()),
            DecodeItem::Control(ControlEvent::EnteredControlMode),
            DecodeItem::Control(output(0, b"hi")),
        ]
    );
}

#[test]
fn exit_and_string_terminator_in_one_read_return_trailing_shell_bytes() {
    let mut parser = ControlModeParser::new();
    let items = parser.decode(&entered(b"%output %0 x\r\n%exit\r\n\x1b\\$ prompt"));
    assert_eq!(
        items,
        vec![
            DecodeItem::Control(ControlEvent::EnteredControlMode),
            DecodeItem::Control(output(0, b"x")),
            DecodeItem::Control(ControlEvent::Exit { reason: None }),
            DecodeItem::Shell(b"$ prompt".to_vec()),
        ]
    );
    assert!(!parser.is_in_control_mode());
}

#[test]
fn string_terminator_split_across_reads_is_not_leaked_to_the_shell() {
    let input = entered(b"%exit\r\n\x1b\\$ ");
    for split in 0..=input.len() {
        let mut parser = ControlModeParser::new();
        let mut items = parser.decode(&input[..split]);
        items.extend(parser.decode(&input[split..]));
        assert_eq!(shell_bytes(&items), b"$ ", "split at {split}");
    }
}

#[test]
fn exit_without_string_terminator_still_returns_shell_bytes() {
    let mut parser = ControlModeParser::new();
    let items = parser.decode(&entered(b"%exit\n$ "));
    assert_eq!(shell_bytes(&items), b"$ ");
}

#[test]
fn exit_reason_is_reported() {
    assert_eq!(
        events_after_entry(b"%exit server exited\n"),
        vec![ControlEvent::Exit {
            reason: Some("server exited".into())
        }]
    );
}

#[test]
fn control_mode_can_be_entered_again_after_exit() {
    let mut parser = ControlModeParser::new();
    let mut input = entered(b"%exit\r\n\x1b\\$ ");
    input.extend_from_slice(&entered(b"%output %1 again\n"));
    let items = parser.decode(&input);
    assert_eq!(
        summaries(&items),
        vec!["enter", "exit", "enter", "output %1"]
    );
    assert_eq!(shell_bytes(&items), b"$ ");
    assert!(parser.is_in_control_mode());
}

// --- %output payloads ---------------------------------------------------------------------------

#[test]
fn utf8_split_across_reads_inside_one_line_is_not_dropped() {
    let mut parser = ControlModeParser::new();
    let mut events = parser.push(&entered(b"%output %3 caf\xc3"));
    events.extend(parser.push(b"\xa9\n"));
    assert_eq!(
        events,
        vec![
            ControlEvent::EnteredControlMode,
            output(3, "café".as_bytes())
        ]
    );
}

#[test]
fn octal_escaped_utf8_split_mid_escape_decodes() {
    let mut parser = ControlModeParser::new();
    let mut events = parser.push(&entered(b"%output %3 caf\\30"));
    events.extend(parser.push(b"3\\251\n"));
    assert_eq!(
        events,
        vec![
            ControlEvent::EnteredControlMode,
            output(3, "café".as_bytes())
        ]
    );
}

#[test]
fn utf8_split_across_output_lines_is_carried_per_pane() {
    let events =
        events_after_entry(b"%output %3 caf\xc3\n%output %4 x\n%output %3 \xa9!\n%output %4 y\n");
    assert_eq!(
        events,
        vec![
            output(3, b"caf"),
            output(4, b"x"),
            output(3, "é!".as_bytes()),
            output(4, b"y"),
        ]
    );
}

#[test]
fn utf8_carry_applies_to_extended_output() {
    // The first payload is only an incomplete UTF-8 prefix, so it is carried and produces no event.
    assert_eq!(
        events_after_entry(b"%extended-output %0 10 : \xe2\x82\n%extended-output %0 12 : \xac\n"),
        vec![ControlEvent::PaneOutput {
            pane_id: PaneId(0),
            age_ms: Some(12),
            bytes: "€".as_bytes().to_vec(),
        }]
    );
}

#[test]
fn invalid_utf8_payload_bytes_pass_through() {
    assert_eq!(
        events_after_entry(b"%output %0 \xff\xfe\n%output %0 a\xc3\n%output %0 b\n"),
        vec![
            output(0, b"\xff\xfe"),
            output(0, b"a"),
            // The carried lead byte did not continue; it is emitted unchanged, never dropped.
            output(0, b"\xc3b"),
        ]
    );
}

#[test]
fn output_decodes_backslash_and_escape() {
    assert_eq!(
        events_after_entry(b"%output %0 a\\134b\\033[1m\n"),
        vec![output(0, b"a\\b\x1b[1m")]
    );
}

#[test]
fn tmux_escaped_payload_round_trips_every_byte() {
    let payload: Vec<u8> = (0..=255u8).collect();
    let escaped = tmux_escape(&payload);
    // tmux leaves bytes >= 0x7f raw.
    assert!(escaped.ends_with(&(0x7fu8..=0xff).collect::<Vec<_>>()));
    let mut line = b"%output %12 ".to_vec();
    line.extend_from_slice(&escaped);
    line.push(b'\n');
    let events = events_after_entry(&line);
    let bytes: Vec<u8> = events
        .iter()
        .flat_map(|event| match event {
            ControlEvent::PaneOutput { bytes, .. } => bytes.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(bytes, payload);
}

#[test]
fn octal_unescape_leaves_incomplete_or_invalid_escapes_literal() {
    assert_eq!(octal_unescape(b"abc"), b"abc");
    assert_eq!(octal_unescape(b"\\033"), b"\x1b");
    assert_eq!(octal_unescape(b"x\\134"), b"x\\");
    assert_eq!(octal_unescape(b"\\03"), b"\\03");
    assert_eq!(octal_unescape(b"\\9aa"), b"\\9aa");
    assert_eq!(octal_unescape(b"\\"), b"\\");
}

#[test]
fn incomplete_line_is_held_until_newline() {
    let mut parser = ControlModeParser::new();
    assert_eq!(
        parser.push(&entered(b"%output %0 hel")),
        vec![ControlEvent::EnteredControlMode]
    );
    assert_eq!(parser.push(b"lo\r\n"), vec![output(0, b"hello")]);
}

// --- Reply framing ------------------------------------------------------------------------------

#[test]
fn end_line_that_does_not_match_the_open_begin_is_payload() {
    let events = events_after_entry(
        b"%begin 100 2 1\n%end not really\n%end 100 3 1\n%error 99 2 1\nreal\n%end 100 2 1\n",
    );
    assert_eq!(
        events,
        vec![
            ControlEvent::CommandBegin {
                time: 100,
                number: 2,
                flags: 1
            },
            ControlEvent::CommandEnd(CommandReply {
                time: 100,
                number: 2,
                flags: 1,
                error: false,
                lines: vec![
                    b"%end not really".to_vec(),
                    b"%end 100 3 1".to_vec(),
                    b"%error 99 2 1".to_vec(),
                    b"real".to_vec(),
                ],
            }),
        ]
    );
}

#[test]
fn error_closes_the_matching_block() {
    assert_eq!(
        events_after_entry(b"%begin 9 8 1\nparse error: unknown command: x\n%error 9 8 1\n"),
        vec![
            ControlEvent::CommandBegin {
                time: 9,
                number: 8,
                flags: 1
            },
            ControlEvent::CommandEnd(CommandReply {
                time: 9,
                number: 8,
                flags: 1,
                error: true,
                lines: vec![b"parse error: unknown command: x".to_vec()],
            }),
        ]
    );
}

#[test]
fn notification_inside_a_block_is_delivered_and_the_reply_stays_intact() {
    // tmux 3.7 writes `%message`, `%pause` and `%continue` inside the issuing command's block
    // (see fixtures/tmux-3.7c-notifications.cc).
    assert_eq!(
        events_after_entry(b"%begin 1 2 1\nfirst\n%message hi\nsecond\n%end 1 2 1\n"),
        vec![
            ControlEvent::CommandBegin {
                time: 1,
                number: 2,
                flags: 1
            },
            ControlEvent::Message {
                message: "hi".into()
            },
            ControlEvent::CommandEnd(CommandReply {
                time: 1,
                number: 2,
                flags: 1,
                error: false,
                lines: vec![b"first".to_vec(), b"second".to_vec()],
            }),
        ]
    );
}

#[test]
fn reply_lines_that_look_like_pane_ids_stay_payload() {
    let events = events_after_entry(b"%begin 1 2 1\n%0 40\n%1 39\n%end 1 2 1\n");
    assert_eq!(
        events.last(),
        Some(&ControlEvent::CommandEnd(CommandReply {
            time: 1,
            number: 2,
            flags: 1,
            error: false,
            lines: vec![b"%0 40".to_vec(), b"%1 39".to_vec()],
        }))
    );
}

#[test]
fn reply_payload_keeps_non_utf8_bytes() {
    let events = events_after_entry(b"%begin 1 2 1\nab\xffcd\n%end 1 2 1\n");
    assert!(matches!(
        events.last(),
        Some(ControlEvent::CommandEnd(reply)) if reply.lines == vec![b"ab\xffcd".to_vec()]
    ));
}

#[test]
fn notifications_between_blocks_are_tolerated() {
    assert_eq!(
        events_after_entry(
            b"%begin 1 1 1\n%end 1 1 1\n%sessions-changed\n%begin 1 2 1\nx\n%end 1 2 1\n"
        )
        .iter()
        .map(summary)
        .collect::<Vec<_>>(),
        vec![
            "begin 1",
            "end 1 [0]",
            "sessions-changed",
            "begin 2",
            "end 2 [1]"
        ]
    );
}

#[test]
fn unmatched_end_unknown_notifications_and_stray_bytes_are_ignored() {
    assert_eq!(
        events_after_entry(
            b"\n\r\n%end 1 2 1\nidle garbage\n%future-notification a b c\n%window-add nope\n%output 3 missing-percent\n%output %0 x\n"
        ),
        vec![output(0, b"x")]
    );
}

// --- Notifications ------------------------------------------------------------------------------

#[test]
fn every_wiki_notification_is_parsed() {
    use ControlEvent::*;
    let cases: Vec<(&[u8], ControlEvent)> = vec![
        (b"%output %1 hi", output(1, b"hi")),
        (
            b"%extended-output %1 250 : hi",
            PaneOutput {
                pane_id: PaneId(1),
                age_ms: Some(250),
                bytes: b"hi".to_vec(),
            },
        ),
        (
            b"%extended-output %1 250 future args : a : b",
            PaneOutput {
                pane_id: PaneId(1),
                age_ms: Some(250),
                bytes: b"a : b".to_vec(),
            },
        ),
        (b"%pause %2", Pause { pane_id: PaneId(2) }),
        (b"%continue %2", Continue { pane_id: PaneId(2) }),
        (
            b"%pane-mode-changed %2",
            PaneModeChanged { pane_id: PaneId(2) },
        ),
        (
            b"%layout-change @1 b25e,80x24,0,0,1 b25e,80x24,0,0,1 *Z",
            LayoutChange {
                window_id: WindowId(1),
                layout: "b25e,80x24,0,0,1".into(),
                visible_layout: Some("b25e,80x24,0,0,1".into()),
                flags: WindowFlags::new("*Z"),
            },
        ),
        (
            b"%layout-change @1 b25e,80x24,0,0,1",
            LayoutChange {
                window_id: WindowId(1),
                layout: "b25e,80x24,0,0,1".into(),
                visible_layout: None,
                flags: WindowFlags::new(""),
            },
        ),
        (
            b"%window-add @3",
            WindowAdd {
                window_id: WindowId(3),
            },
        ),
        (
            b"%window-close @3",
            WindowClose {
                window_id: WindowId(3),
            },
        ),
        (
            b"%window-renamed @3 my window",
            WindowRenamed {
                window_id: WindowId(3),
                name: "my window".into(),
            },
        ),
        (
            b"%unlinked-window-add @4",
            UnlinkedWindowAdd {
                window_id: WindowId(4),
            },
        ),
        (
            b"%unlinked-window-close @4",
            UnlinkedWindowClose {
                window_id: WindowId(4),
            },
        ),
        (
            b"%unlinked-window-renamed @4 other",
            UnlinkedWindowRenamed {
                window_id: WindowId(4),
                name: "other".into(),
            },
        ),
        (
            b"%window-pane-changed @4 %7",
            WindowPaneChanged {
                window_id: WindowId(4),
                pane_id: PaneId(7),
            },
        ),
        (
            b"%session-changed $2 main work",
            SessionChanged {
                session_id: SessionId(2),
                name: "main work".into(),
            },
        ),
        (
            b"%client-session-changed /dev/ttys003 $2 main",
            ClientSessionChanged {
                client: "/dev/ttys003".into(),
                session_id: SessionId(2),
                name: "main".into(),
            },
        ),
        (
            b"%session-renamed $2 renamed",
            SessionRenamed {
                session_id: SessionId(2),
                name: "renamed".into(),
            },
        ),
        (b"%sessions-changed", SessionsChanged),
        (
            b"%session-window-changed $2 @4",
            SessionWindowChanged {
                session_id: SessionId(2),
                window_id: WindowId(4),
            },
        ),
        (
            b"%subscription-changed cmd $2 @4 1 %7 : vim : x",
            SubscriptionChanged {
                name: "cmd".into(),
                session_id: Some(SessionId(2)),
                window_id: Some(WindowId(4)),
                window_index: Some(1),
                pane_id: Some(PaneId(7)),
                value: "vim : x".into(),
            },
        ),
        (
            b"%subscription-changed pn $0 - - - : bash",
            SubscriptionChanged {
                name: "pn".into(),
                session_id: Some(SessionId(0)),
                window_id: None,
                window_index: None,
                pane_id: None,
                value: "bash".into(),
            },
        ),
        (
            b"%client-detached /dev/ttys003",
            ClientDetached {
                client: "/dev/ttys003".into(),
            },
        ),
        (
            b"%config-error /etc/tmux.conf:1: unknown command: x",
            ConfigError {
                message: "/etc/tmux.conf:1: unknown command: x".into(),
            },
        ),
        (
            b"%message hello there",
            Message {
                message: "hello there".into(),
            },
        ),
        (
            b"%paste-buffer-changed buffer0",
            PasteBufferChanged {
                name: "buffer0".into(),
            },
        ),
        (
            b"%paste-buffer-deleted buffer0",
            PasteBufferDeleted {
                name: "buffer0".into(),
            },
        ),
        (b"%exit", Exit { reason: None }),
    ];
    for (line, expected) in cases {
        let mut input = line.to_vec();
        input.extend_from_slice(b"\r\n");
        assert_eq!(
            events_after_entry(&input),
            vec![expected],
            "line {:?}",
            String::from_utf8_lossy(line)
        );
    }
}

// --- Bounds -------------------------------------------------------------------------------------

#[test]
fn oversized_control_line_discards_until_exit() {
    let mut parser = ControlModeParser::new();
    parser.decode(CONTROL_MODE_DCS);
    let items = parser.decode(&vec![b'x'; 1_048_577]);
    assert_eq!(summaries(&items), vec!["overflow"]);
    assert!(!parser.is_in_control_mode());
    let items = parser.decode(b"%output %0 leaked\n%layout-change @0 80x24,0,0,0\n%exit\n$ ");
    assert_eq!(summaries(&items), vec!["exit"]);
    assert_eq!(shell_bytes(&items), b"$ ");
}

#[test]
fn oversized_reply_payload_discards_until_exit() {
    let mut parser = ControlModeParser::new();
    parser.decode(CONTROL_MODE_DCS);
    let mut bytes = b"%begin 1 1 1\n".to_vec();
    for _ in 0..10_001 {
        bytes.extend_from_slice(b"line\n");
    }
    let items = parser.decode(&bytes);
    assert_eq!(summaries(&items), vec!["begin 1", "overflow"]);
    assert!(parser.decode(b"%output %0 leaked\n").is_empty());
}

// --- Fixture replays ----------------------------------------------------------------------------

#[test]
fn fixture_tmux_3_7c_session_replays() {
    let items = replay_fixture(FIXTURE_3_7_SESSION);
    assert_eq!(
        summaries(&items),
        vec![
            "enter",
            "begin 280",
            "end 280 [0]",
            "window-add @0",
            "sessions-changed",
            "session-changed $0 0",
            "window-renamed @0 tmux",
            "output %0",
            "window-renamed @0 bash",
            "begin 288",
            "end 288 [0]",
            "begin 289",
            "end 289 [0]",
            "window-renamed @0 renamed",
            "begin 291",
            "end 291 [0]",
            "output %0",
            "output %0",
            "output %0",
            "output %0",
            "output %0",
            "output %0",
            "output %0",
            "output %0",
            "begin 292",
            "end 292 [0]",
            "output %0",
            "output %0",
            "begin 293",
            "end 293 [0]",
            "window-pane-changed @0 %1",
            "layout @0 *",
            "output %0",
            "output %1",
            "begin 296",
            "end 296 [2]",
            "begin 297",
            "end 297 [0]",
            "layout @0 *Z",
            "output %1",
            "begin 299",
            "end 299 [0]",
            "session-window-changed $0 @1",
            "window-add @1",
            "window-renamed @1 kernel_task",
            "output %2",
            "window-renamed @1 bash",
            "begin 304",
            "end 304 [0]",
            "session-window-changed $0 @0",
            "unlinked-window-close @1",
            "begin 307",
            "error 307 [1]",
            "begin 308",
            "end 308 [0]",
            "exit",
        ]
    );
    // The trailing `ESC \` after `%exit` is protocol, not shell output.
    assert!(shell_bytes(&items).is_empty());
    let pane0 = pane_output(&items, 0);
    let needle = "café\x1b[1m\\\r\n".as_bytes();
    assert!(pane0.windows(needle.len()).any(|w| w == needle));
}

#[test]
fn fixture_tmux_3_7c_notifications_replays() {
    let items = replay_fixture(FIXTURE_3_7_NOTIFICATIONS);
    assert_eq!(
        summaries(&items),
        vec![
            "enter",
            "begin 280",
            "end 280 [0]",
            "window-add @0",
            "sessions-changed",
            "session-changed $0 0",
            "output %0",
            "window-renamed @0 bash",
            "begin 287",
            "end 287 [0]",
            "begin 288",
            "end 288 [0]",
            "begin 289",
            "end 289 [0]",
            "begin 290",
            "end 290 [0]",
            "session-renamed $0 work",
            "subscription-changed pn=bash",
            "begin 292",
            "end 292 [0]",
            "paste-buffer-changed warpbuf",
            "begin 294",
            "end 294 [0]",
            "paste-buffer-deleted warpbuf",
            "begin 296",
            "end 296 [0]",
            "pane-mode-changed %0",
            "window-renamed @0 [tmux]",
            "begin 299",
            "end 299 [0]",
            "pane-mode-changed %0",
            "window-renamed @0 bash",
            "begin 302",
            "message hello-msg",
            "end 302 [0]",
            "begin 303",
            "end 303 [0]",
            "extended-output %0",
            "extended-output %0",
            "begin 304",
            "pause %0",
            "end 304 [0]",
            "begin 305",
            "end 305 [0]",
            "begin 306",
            "continue %0",
            "end 306 [0]",
            "begin 307",
            "end 307 [0]",
            "sessions-changed",
            "exit",
        ]
    );
    assert_eq!(pane_output(&items, 0), b"\x1b[?1034h$ echo hi\r\nhi\r\n$ ");
}

#[test]
fn fixture_tmux_3_2_handbuilt_replays() {
    let items = replay_fixture(FIXTURE_3_2);
    assert_eq!(
        summaries(&items),
        vec![
            "enter",
            "begin 1",
            "end 1 [0]",
            "window-add @0",
            "sessions-changed",
            "session-changed $0 0",
            "output %0",
            "begin 8",
            "end 8 [0]",
            "window-pane-changed @0 %1",
            "layout @0 *",
            "output %1",
            "begin 9",
            "end 9 [1]",
            "client-detached /dev/ttys001",
            "exit",
        ]
    );
    assert_eq!(pane_output(&items, 1), b"\x1b[1mhi\\\r\n");
    assert_eq!(shell_bytes(&items), b"$ ");
}

#[test]
fn fixture_tmux_3_4_handbuilt_replays() {
    let items = replay_fixture(FIXTURE_3_4);
    assert_eq!(
        summaries(&items),
        vec![
            "enter",
            "begin 1",
            "end 1 [0]",
            "config-error /etc/tmux.conf:3: unknown command: bogus",
            "window-add @0",
            "session-changed $0 main",
            "begin 5",
            "pause %0",
            "end 5 [0]",
            "extended-output %0",
            "continue %0",
            "begin 6",
            "end 6 [1]",
            "subscription-changed cmd=vim",
            "unlinked-window-add @3",
            "unlinked-window-renamed @3 scratch",
            "client-session-changed /dev/ttys002 $1 other",
            "exit server exited",
        ]
    );
    assert_eq!(pane_output(&items, 0), "café\r\n".as_bytes());
    assert!(shell_bytes(&items).is_empty());
}

#[test]
fn notification_lines_tmux_never_interleaves_stay_reply_payload() {
    let body = b"%exit\n%window-close @1\n%layout-change @1 x\n%session-changed $1 x\n";
    let mut input = b"%begin 1 2 1\n".to_vec();
    input.extend_from_slice(body);
    input.extend_from_slice(b"%end 1 2 1\n");
    let mut parser = ControlModeParser::new();
    let events = parser.push(&entered(&input));
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(
        events[2],
        ControlEvent::CommandEnd(CommandReply {
            time: 1,
            number: 2,
            flags: 1,
            error: false,
            lines: body
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .map(<[u8]>::to_vec)
                .collect(),
        })
    );
    assert!(parser.is_in_control_mode());
}

#[test]
fn every_line_of_an_expected_capture_reply_is_payload() {
    let mut parser = ControlModeParser::new();
    parser.push(CONTROL_MODE_DCS);
    parser.expect_reply(true);
    parser.expect_reply(false);
    let events = parser.push(
        b"%begin 1 2 0\n%message theirs\n%end 1 2 0\n\
          %begin 1 3 1\n%output %1 x\n%message pane text\n%pause %1\n%end 1 3 1\n\
          %begin 1 4 1\n%message mine\n%end 1 4 1\n",
    );
    let summaries: Vec<_> = events.iter().map(summary).collect();
    assert_eq!(
        summaries,
        [
            "begin 2",
            "message theirs",
            "end 2 [0]",
            "begin 3",
            "end 3 [3]",
            "begin 4",
            "message mine",
            "end 4 [0]"
        ]
    );
}

#[test]
fn shell_lines_where_only_control_lines_can_be_hand_the_stream_back_to_the_shell() {
    let mut parser = ControlModeParser::new();
    let items = parser.decode(&entered(
        b"%session-changed $0 main\nConnection to host closed.\r\nbye\nlast\nmore",
    ));
    assert_eq!(
        items[items.len() - 2..],
        [
            DecodeItem::Control(ControlEvent::ControlLost),
            DecodeItem::Shell(b"Connection to host closed.\r\nbye\nlast\nmore".to_vec()),
        ]
    );
    assert!(parser.shell_pending().is_some());
}

#[test]
fn a_prompt_with_escapes_hands_the_stream_back_without_a_newline() {
    let mut parser = ControlModeParser::new();
    let items = parser.decode(&entered(b"%session-changed $0 main\n\x1b]133;A\x07$ "));
    assert_eq!(
        items[items.len() - 2..],
        [
            DecodeItem::Control(ControlEvent::ControlLost),
            DecodeItem::Shell(b"\x1b]133;A\x07$ ".to_vec()),
        ]
    );
}

#[test]
fn control_lines_reset_the_stray_line_count_and_block_payload_is_never_stray() {
    let mut parser = ControlModeParser::new();
    let events = parser.push(&entered(
        b"one\ntwo\n%sessions-changed\nthree\nfour\n\
          %begin 1 2 1\na\nb\nc\n\x1b[31md\n%end 1 2 1\n",
    ));
    assert!(!events.contains(&ControlEvent::ControlLost), "{events:?}");
    assert!(parser.is_in_control_mode());
}
