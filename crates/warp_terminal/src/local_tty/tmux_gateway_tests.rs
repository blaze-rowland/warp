use std::sync::Mutex;

use super::*;

fn entry_in(matched: &mut usize, input: &[u8]) -> Option<Entry> {
    find_entry(matched, input)
}

#[test]
fn finds_the_dcs_in_one_read() {
    let mut matched = 0;
    assert_eq!(
        entry_in(&mut matched, b"ab\x1bP1000p%begin"),
        Some(Entry {
            start: 2,
            end: 9,
            carried: false
        })
    );
}

#[test]
fn ignores_reads_without_the_dcs() {
    let mut matched = 0;
    assert_eq!(entry_in(&mut matched, b"plain \x1b[31mred\x1bP1$q"), None);
    assert_eq!(matched, 0);
    assert_eq!(entry_in(&mut matched, b""), None);
}

#[test]
fn carries_a_partial_dcs_across_reads_without_holding_bytes() {
    let mut matched = 0;
    assert_eq!(entry_in(&mut matched, b"x\x1bP1"), None);
    assert_eq!(matched, 3);
    assert_eq!(entry_in(&mut matched, b"00"), None);
    assert_eq!(matched, 5);
    assert_eq!(
        entry_in(&mut matched, b"0p%"),
        Some(Entry {
            start: 0,
            end: 2,
            carried: true
        })
    );
    assert_eq!(matched, 0);
}

#[test]
fn a_broken_partial_match_restarts_the_search_at_the_mismatch() {
    let mut matched = 0;
    assert_eq!(entry_in(&mut matched, b"\x1bP10"), None);
    assert_eq!(
        entry_in(&mut matched, b"\x1bP1000p"),
        Some(Entry {
            start: 0,
            end: 7,
            carried: false
        })
    );
    let mut matched = 0;
    assert_eq!(entry_in(&mut matched, b"\x1bP"), None);
    assert_eq!(entry_in(&mut matched, b"0 no"), None);
    assert_eq!(matched, 0);
}

#[test]
fn a_repeated_escape_still_matches() {
    let mut matched = 0;
    assert_eq!(
        entry_in(&mut matched, b"\x1b\x1bP1000p"),
        Some(Entry {
            start: 1,
            end: 8,
            carried: false
        })
    );
}

#[test]
fn origin_flags_report_the_nested_reason() {
    let flags = OriginFlags::default();
    assert_eq!(flags.nested_reason(), None);
    flags.set_tmux_env(true);
    assert_eq!(flags.nested_reason(), Some(RefusalReason::TmuxEnvSet));
    flags.set_origin_is_tmux_pane(true);
    assert_eq!(flags.nested_reason(), Some(RefusalReason::OriginIsTmuxPane));
    flags.set_origin_is_tmux_pane(false);
    flags.set_tmux_env(false);
    assert_eq!(flags.nested_reason(), None);
    assert!(RefusalReason::TmuxEnvSet.is_nested());
    assert!(
        !RefusalReason::UnsupportedVersion {
            version: "3.1".into()
        }
        .is_nested()
    );
    assert_eq!(
        RefusalReason::UnsupportedVersion {
            version: String::new()
        }
        .to_string(),
        "Native tmux needs tmux 3.2 or newer"
    );
}

fn recording_requester() -> (GatewayRequester, Arc<Mutex<Vec<TmuxRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let out = requests.clone();
    (
        Arc::new(move |request| out.lock().unwrap().push(request)),
        requests,
    )
}

#[test]
fn a_pane_queue_pauses_past_high_water_and_resumes_at_low_water() {
    let limits = PaneQueueLimits {
        high_water: 10,
        low_water: 4,
        hard_cap: 20,
    };
    let (requester, requests) = recording_requester();
    let (sender, receiver) = pane_queue(PaneId(3), limits, requester);
    assert_eq!(
        sender.push(PaneChunk::Output(vec![b'a'; 6])),
        Pushed::Queued
    );
    assert_eq!(
        sender.push(PaneChunk::Output(vec![b'b'; 6])),
        Pushed::QueuedPause
    );
    assert_eq!(
        sender.push(PaneChunk::Output(vec![b'c'; 6])),
        Pushed::Queued
    );
    assert_eq!(
        sender.push(PaneChunk::Output(vec![b'd'; 6])),
        Pushed::Dropped { pause: false }
    );
    assert_eq!(receiver.queued_bytes(), 18);
    assert!(receiver.is_paused());

    receiver.try_recv().unwrap();
    receiver.try_recv().unwrap();
    assert!(requests.lock().unwrap().is_empty());
    receiver.try_recv().unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        vec![TmuxRequest::ResumePane(PaneId(3))]
    );
    assert!(!receiver.is_paused());
    assert_eq!(receiver.try_recv(), None);
}

#[test]
fn a_chunk_over_the_hard_cap_is_dropped_and_pauses() {
    let limits = PaneQueueLimits {
        high_water: 10,
        low_water: 4,
        hard_cap: 20,
    };
    let (requester, _) = recording_requester();
    let (sender, receiver) = pane_queue(PaneId(3), limits, requester);
    assert_eq!(
        sender.push(PaneChunk::Output(vec![0; 21])),
        Pushed::Dropped { pause: true }
    );
    assert_eq!(receiver.queued_bytes(), 0);
    drop(sender);
    assert!(receiver.is_closed());
}

#[test]
fn a_closed_queue_does_not_request_a_resume() {
    let limits = PaneQueueLimits {
        high_water: 1,
        low_water: 0,
        hard_cap: 20,
    };
    let (requester, requests) = recording_requester();
    let (sender, receiver) = pane_queue(PaneId(3), limits, requester);
    assert_eq!(
        sender.push(PaneChunk::Output(vec![0; 2])),
        Pushed::QueuedPause
    );
    drop(sender);
    assert!(receiver.try_recv().is_some());
    assert!(requests.lock().unwrap().is_empty());
}

fn gateway(enabled: bool) -> (Gateway, Arc<Mutex<Vec<GatewayEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    let (requester, _) = recording_requester();
    let gateway = Gateway::new(GatewayLink {
        switch: GatewaySwitch::new(enabled),
        origin: OriginFlags::default(),
        sink: Box::new(move |event| sink_events.lock().unwrap().push(event)),
        requester,
        client_config: ClientConfig::default(),
        queue_limits: PaneQueueLimits::default(),
    });
    (gateway, events)
}

#[test]
fn a_disabled_gateway_is_not_engaged_and_passes_everything_through() {
    let (mut gateway, events) = gateway(false);
    assert!(!gateway.engaged());
    let mut writes = VecDeque::new();
    assert_eq!(gateway.route(b"\x1bP1000p", &mut writes), Routed::Shell);
    assert!(writes.is_empty());
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn entry_splits_shell_bytes_and_writes_the_version_query_once_the_attach_succeeds() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    assert_eq!(
        gateway.route(b"prompt\x1bP1000p%begin 1 280 0\n", &mut writes),
        Routed::Split(b"prompt".to_vec())
    );
    assert!(gateway.in_session());
    assert!(writes.is_empty());
    gateway.route(b"%end 1 280 0\n%sessions-changed\n", &mut writes);
    assert_eq!(
        writes.pop_front().as_deref(),
        Some(b"display-message -p '#{version}\t#{client_name}'\n".as_slice())
    );
    assert!(matches!(
        &events.lock().unwrap()[..],
        [GatewayEvent::Starting]
    ));
    assert!(gateway.next_timeout().is_some());
}

#[test]
fn exit_and_shell_bytes_in_the_same_read_end_the_session() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    assert_eq!(
        gateway.route(b"\x1bP1000p%exit\n\x1b\\$ \x1bP1", &mut writes),
        Routed::Split(b"$ \x1bP1".to_vec())
    );
    assert!(!gateway.in_session());
    assert!(matches!(
        &events.lock().unwrap()[..],
        [
            GatewayEvent::Starting,
            GatewayEvent::Client(ClientEvent::AttachFailed { .. }),
            GatewayEvent::Ended
        ]
    ));
    // The held-back partial DCS went to the model and the detector carries on from it.
    assert!(matches!(
        gateway.route(b"000p", &mut writes),
        Routed::Split(model) if model == [CANCEL]
    ));
    assert!(gateway.in_session());
}

#[test]
fn transport_closed_ends_a_session_and_is_a_no_op_without_one() {
    let (mut gateway, events) = gateway(true);
    gateway.transport_closed();
    assert!(events.lock().unwrap().is_empty());
    let mut writes = VecDeque::new();
    gateway.route(b"\x1bP1000p", &mut writes);
    gateway.transport_closed();
    assert!(!gateway.in_session());
    assert!(matches!(
        &events.lock().unwrap()[..],
        [
            GatewayEvent::Starting,
            GatewayEvent::Client(ClientEvent::TransportLost),
            GatewayEvent::Ended
        ]
    ));
}

#[test]
fn weak_origin_flags_share_state_until_the_origin_is_gone() {
    let flags = OriginFlags::default();
    let weak = flags.downgrade();
    weak.upgrade().unwrap().set_origin_is_tmux_pane(true);
    assert_eq!(flags.nested_reason(), Some(RefusalReason::OriginIsTmuxPane));
    drop(flags);
    assert!(weak.upgrade().is_none());
}

#[test]
fn a_test_pane_queue_delivers_in_order_and_closes_when_the_sender_drops() {
    let (sender, queue) = test_pane_queue(PaneId(4));
    assert_eq!(queue.pane_id(), PaneId(4));
    sender.push(PaneChunk::Output(b"one".to_vec()));
    sender.push(PaneChunk::Output(b"two".to_vec()));
    assert_eq!(queue.queued_bytes(), 6);
    assert_eq!(queue.try_recv(), Some(PaneChunk::Output(b"one".to_vec())));
    drop(sender);
    assert_eq!(queue.try_recv(), Some(PaneChunk::Output(b"two".to_vec())));
    assert_eq!(queue.try_recv(), None);
    assert!(queue.is_closed());
    assert_eq!(queue.queued_bytes(), 0);
}
