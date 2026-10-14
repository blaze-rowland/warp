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
    let (gateway, events, origin) = gateway_with_origin(enabled);
    // Most tests play an entry Warp started; see the `unarmed` tests for the rest.
    origin.arm();
    (gateway, events)
}

fn gateway_with_origin(enabled: bool) -> (Gateway, Arc<Mutex<Vec<GatewayEvent>>>, OriginFlags) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    let (requester, _) = recording_requester();
    let origin = OriginFlags::default();
    let gateway = Gateway::new(GatewayLink {
        switch: GatewaySwitch::new(enabled),
        origin: origin.clone(),
        sink: Box::new(move |event| sink_events.lock().unwrap().push(event)),
        requester,
        client_config: ClientConfig::default(),
        queue_limits: PaneQueueLimits::default(),
    });
    (gateway, events, origin)
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
    gateway.origin.arm();
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
    // The attach never completed, so the stream is the shell's output.
    assert_eq!(gateway.take_shell_bytes(), b"\x1bP1000p");
    assert!(
        matches!(
            &events.lock().unwrap()[..],
            [
                GatewayEvent::Starting,
                GatewayEvent::Client(ClientEvent::TransportLost),
                GatewayEvent::Ended
            ]
        ),
        "{:?}",
        event_names(&events)
    );
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

const ATTACH: &[u8] = b"\x1bP1000p%begin 1 280 0\n%end 1 280 0\n%session-changed $0 main\n";
const VERSION_LINE: &[u8] = b"display-message -p '#{version}\t#{client_name}'\n";

fn event_names(events: &Arc<Mutex<Vec<GatewayEvent>>>) -> Vec<String> {
    events
        .lock()
        .unwrap()
        .iter()
        .map(|event| match event {
            GatewayEvent::Client(event) => format!("{event:?}")
                .split([' ', '{', '('])
                .next()
                .unwrap_or_default()
                .to_owned(),
            GatewayEvent::Model(_) => "Model".to_owned(),
            event => format!("{event:?}"),
        })
        .collect()
}

#[test]
fn an_unarmed_entry_writes_nothing_and_asks_first() {
    let (mut gateway, events, _) = gateway_with_origin(true);
    let mut writes = VecDeque::new();
    let mut read = b"$ cat notes\r\n".to_vec();
    read.extend_from_slice(ATTACH);
    assert_eq!(
        gateway.route(&read, &mut writes),
        Routed::Split(b"$ cat notes\r\n".to_vec())
    );
    assert_eq!(
        gateway.route(b"%output %1 more\n", &mut writes),
        Routed::Split(Vec::new())
    );
    assert!(writes.is_empty());
    assert!(!gateway.in_session());
    assert!(gateway.engaged());
    assert_eq!(gateway.next_timeout(), None);
    assert_eq!(event_names(&events), ["ConfirmPending"]);
}

#[test]
fn dismissing_an_unarmed_entry_releases_the_held_bytes_to_the_model() {
    let (mut gateway, events, _) = gateway_with_origin(true);
    let mut writes = VecDeque::new();
    gateway.route(b"\x1bP1000phello", &mut writes);
    gateway.route(b" world\x1b\\", &mut writes);
    gateway.request(TmuxRequest::ConfirmDismiss, &mut writes);
    assert!(writes.is_empty());
    assert_eq!(gateway.take_shell_bytes(), b"\x1bP1000phello world\x1b\\");
    assert_eq!(event_names(&events), ["ConfirmPending", "ConfirmCleared"]);
    assert_eq!(gateway.route(b"$ ", &mut writes), Routed::Shell);
    assert!(!gateway.in_session());
}

#[test]
fn opening_an_unarmed_entry_starts_the_session_with_the_held_bytes() {
    let (mut gateway, events, _) = gateway_with_origin(true);
    let mut writes = VecDeque::new();
    gateway.route(ATTACH, &mut writes);
    assert!(writes.is_empty());
    gateway.request(TmuxRequest::ConfirmOpen, &mut writes);
    assert!(gateway.in_session());
    assert_eq!(writes.pop_front().as_deref(), Some(VERSION_LINE));
    assert_eq!(
        event_names(&events),
        ["ConfirmPending", "ConfirmCleared", "Starting"]
    );
}

#[test]
fn an_armed_entry_starts_without_asking_and_the_arm_is_used_up() {
    let (mut gateway, events, origin) = gateway_with_origin(true);
    origin.arm();
    let mut writes = VecDeque::new();
    gateway.route(ATTACH, &mut writes);
    assert!(gateway.in_session());
    assert_eq!(writes.pop_front().as_deref(), Some(VERSION_LINE));
    gateway.route(b"%exit\n\x1b\\", &mut writes);
    assert!(!gateway.in_session());
    gateway.route(ATTACH, &mut writes);
    assert!(!gateway.in_session());
    assert_eq!(event_names(&events).last().unwrap(), "ConfirmPending");
}

#[test]
fn a_disarmed_origin_asks() {
    let (mut gateway, events, origin) = gateway_with_origin(true);
    origin.arm();
    origin.disarm();
    gateway.route(ATTACH, &mut VecDeque::new());
    assert_eq!(event_names(&events), ["ConfirmPending"]);
}

#[test]
fn an_attach_that_does_not_complete_in_time_is_abandoned_to_the_model() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    assert_eq!(
        gateway.route(b"$ \x1bP1000p%begin 1 280 0\n", &mut writes),
        Routed::Split(b"$ ".to_vec())
    );
    assert!(
        gateway
            .next_timeout()
            .is_some_and(|t| t <= HANDSHAKE_DEADLINE)
    );
    gateway.tick(&mut writes);
    assert!(gateway.in_session());
    gateway.advance_clock(HANDSHAKE_DEADLINE);
    gateway.tick(&mut writes);
    assert!(!gateway.in_session());
    assert!(writes.is_empty());
    assert_eq!(gateway.take_shell_bytes(), b"\x1bP1000p%begin 1 280 0\n");
    assert_eq!(event_names(&events), ["Starting", "Ended"]);
}

#[test]
fn plain_text_after_an_entry_is_given_back_before_anything_is_written() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    assert_eq!(
        gateway.route(b"\x1bP1000pnot\ntmux\nat all\n$ ", &mut writes),
        Routed::Split(b"\x1bP1000pnot\ntmux\nat all\n$ ".to_vec())
    );
    assert!(!gateway.in_session());
    assert!(writes.is_empty());
    assert_eq!(event_names(&events), ["Starting", "Ended"]);
}

#[test]
fn lost_control_while_attached_ends_the_session_and_shell_bytes_flow_again() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    gateway.route(ATTACH, &mut writes);
    assert_eq!(writes.len(), 1);
    let dropped = b"Connection to host closed.\r\n\x1b]133;A\x07$ ";
    assert_eq!(
        gateway.route(dropped, &mut writes),
        Routed::Split(dropped.to_vec())
    );
    assert!(!gateway.in_session());
    assert_eq!(writes.len(), 1, "nothing more is written: {writes:?}");
    let names = event_names(&events);
    assert!(names.contains(&"TransportLost".to_owned()), "{names:?}");
    assert_eq!(names.last().unwrap(), "Ended");
    assert_eq!(gateway.route(b"ls\r\n", &mut writes), Routed::Shell);
}

/// A control line past the parser's bound, which desyncs the session.
fn overlong_control_line() -> Vec<u8> {
    let mut line = b"%message ".to_vec();
    line.resize(2 << 20, b'a');
    line
}

#[test]
fn a_desync_detaches_and_the_session_ends_on_exit() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    gateway.route(ATTACH, &mut writes);
    writes.clear();
    gateway.route(&overlong_control_line(), &mut writes);
    assert_eq!(writes.pop_front().as_deref(), Some(b"\n".as_slice()));
    assert!(gateway.in_session());
    assert!(event_names(&events).contains(&"Desync".to_owned()));
    assert_eq!(
        gateway.route(b"%exit\n\x1b\\$ ", &mut writes),
        Routed::Split(b"$ ".to_vec())
    );
    assert!(!gateway.in_session());
    assert_eq!(event_names(&events).last().unwrap(), "Ended");
}

#[test]
fn a_disconnected_session_that_never_exits_gives_the_pty_back() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    gateway.route(ATTACH, &mut writes);
    gateway.route(&overlong_control_line(), &mut writes);
    assert!(gateway.next_timeout().is_some());
    gateway.advance_clock(Duration::from_secs(5));
    gateway.tick(&mut writes);
    assert!(!gateway.in_session());
    assert_eq!(event_names(&events).last().unwrap(), "Ended");
}

#[test]
fn confirm_requests_without_a_held_entry_leave_a_running_session_alone() {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    gateway.route(ATTACH, &mut writes);
    gateway.request(TmuxRequest::ConfirmOpen, &mut writes);
    gateway.request(TmuxRequest::ConfirmDismiss, &mut writes);
    gateway.transport_closed();
    assert!(!gateway.in_session());
    assert!(gateway.take_shell_bytes().is_empty());
    assert_eq!(event_names(&events), ["Starting", "TransportLost", "Ended"]);
}

/// tmux's answer to one command line Warp wrote, for a session with one window holding `panes`.
fn tmux_reply(line: &str, panes: &[u32]) -> Vec<String> {
    let checked = |body: &str| format!("{:04x},{body}", crate::tmux::layout_checksum(body));
    if line.starts_with("display-message -p '#{version}") {
        vec!["3.7c\tclient-7".to_owned()]
    } else if line.starts_with("list-windows") {
        let body = match panes {
            [pane] => format!("80x24,0,0,{pane}"),
            panes => format!(
                "80x24,0,0{{{}}}",
                panes
                    .iter()
                    .enumerate()
                    .map(|(i, pane)| format!("39x24,{},0,{pane}", i * 40))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        };
        let layout = checked(&body);
        vec![format!("@1\t1\t1\t{layout}\t{layout}\t*\tmain")]
    } else if line.starts_with("list-panes") {
        panes
            .iter()
            .map(|pane| format!("%{pane}\t@1\t1\t0\t0\t80\t24\t\t"))
            .collect()
    } else if line.starts_with("show -gv prefix2") {
        vec!["None".to_owned()]
    } else if line.starts_with("show -gv prefix") {
        vec!["C-b".to_owned()]
    } else if line.starts_with("capture-pane") {
        vec!["fresh".to_owned()]
    } else if line.starts_with("display-message -p -t") {
        vec!["0\t0".to_owned()]
    } else {
        Vec::new()
    }
}

/// Answers everything in `writes` as tmux would, until Warp writes nothing more.
fn answer_all(gateway: &mut Gateway, writes: &mut VecDeque<Cow<'static, [u8]>>, panes: &[u32]) {
    // tmux numbers each command it runs, across the whole session.
    static NEXT_NUMBER: AtomicUsize = AtomicUsize::new(300);
    while let Some(line) = writes.pop_front() {
        let number = NEXT_NUMBER.fetch_add(1, Ordering::Relaxed);
        let line = String::from_utf8_lossy(&line).into_owned();
        let mut text = format!("%begin 1 {number} 1\n");
        for reply in tmux_reply(&line, panes) {
            text.push_str(&reply);
            text.push('\n');
        }
        text.push_str(&format!("%end 1 {number} 1\n"));
        gateway.route(text.as_bytes(), writes);
    }
}

/// A gateway attached to a session with one window holding `panes`, every seed answered.
fn attached_gateway(panes: &[u32]) -> (Gateway, Arc<Mutex<Vec<GatewayEvent>>>) {
    let (mut gateway, events) = gateway(true);
    let mut writes = VecDeque::new();
    gateway.route(
        b"\x1bP1000p%begin 1 280 0\n%end 1 280 0\n%window-add @1\n%sessions-changed\n\
          %session-changed $0 main\n",
        &mut writes,
    );
    answer_all(&mut gateway, &mut writes, panes);
    assert!(
        event_names(&events).iter().any(|name| name == "Attached"),
        "{:?}",
        event_names(&events)
    );
    (gateway, events)
}

/// Takes the queues opened so far, by pane.
fn opened_queues(events: &Arc<Mutex<Vec<GatewayEvent>>>) -> Vec<PaneQueueReceiver> {
    let mut events = events.lock().unwrap();
    let (opened, rest): (Vec<_>, Vec<_>) = events
        .drain(..)
        .partition(|event| matches!(event, GatewayEvent::PaneQueueOpened { .. }));
    *events = rest;
    opened
        .into_iter()
        .filter_map(|event| match event {
            GatewayEvent::PaneQueueOpened { queue } => Some(queue),
            _ => None,
        })
        .collect()
}

fn drain(queue: &PaneQueueReceiver) -> Vec<PaneChunk> {
    std::iter::from_fn(|| queue.try_recv()).collect()
}

#[test]
fn a_queue_whose_reader_is_gone_is_reopened_with_a_resync() {
    let (mut gateway, events) = attached_gateway(&[1, 2]);
    let queues = opened_queues(&events);
    assert_eq!(queues.len(), 2);
    // The view showing %1 is dropped (zoomed out, or moved to another tab).
    drop(queues.into_iter().find(|queue| queue.pane_id() == PaneId(1)));

    let mut writes = VecDeque::new();
    gateway.route(b"%output %1 hidden\n", &mut writes);
    let reopened = opened_queues(&events);
    assert_eq!(
        reopened.iter().map(PaneQueueReceiver::pane_id).collect::<Vec<_>>(),
        [PaneId(1)],
        "the pane's next output opens a new queue"
    );
    assert!(
        writes
            .iter()
            .any(|line| line.starts_with(b"capture-pane -p -e -J -S -2000 -t %1")),
        "the new queue gets a resync: {writes:?}"
    );
    answer_all(&mut gateway, &mut writes, &[1, 2]);
    let chunks = drain(&reopened[0]);
    assert_eq!(chunks[0], PaneChunk::Output(b"hidden".to_vec()));
    assert!(
        matches!(&chunks[1], PaneChunk::Snapshot(snapshot) if snapshot.lines == [b"fresh".to_vec()]),
        "{chunks:?}"
    );

    // The reopened queue is live: more output goes to it without another resync.
    gateway.route(b"%output %1 more\n", &mut writes);
    assert!(writes.is_empty());
    assert!(opened_queues(&events).is_empty());
    assert_eq!(drain(&reopened[0]), [PaneChunk::Output(b"more".to_vec())]);
}

#[test]
fn a_resync_request_replaces_the_queue_and_sends_a_snapshot() {
    let (mut gateway, events) = attached_gateway(&[1]);
    // An old view still holds the queue while a new view for the same pane asks for content.
    let old = opened_queues(&events).pop().unwrap();
    let mut writes = VecDeque::new();
    gateway.request(TmuxRequest::ResyncPane(PaneId(1)), &mut writes);
    assert!(old.is_closed(), "the old view's queue is closed");
    assert!(opened_queues(&events).is_empty());
    answer_all(&mut gateway, &mut writes, &[1]);

    let new = opened_queues(&events);
    assert_eq!(new.len(), 1);
    assert!(
        matches!(&drain(&new[0])[..], [PaneChunk::Snapshot(snapshot)] if snapshot.lines == [b"fresh".to_vec()])
    );
    gateway.route(b"%output %1 after\n", &mut writes);
    assert_eq!(drain(&new[0]), [PaneChunk::Output(b"after".to_vec())]);

    // A pane tmux does not know is left alone.
    gateway.request(TmuxRequest::ResyncPane(PaneId(9)), &mut writes);
    assert!(writes.is_empty());
}
