//! Tests that drive [`EventLoop`] through an in-memory PTY made of mio pipes.
//!
//! The terminal double records every [`ansi::Handler`] call, so "the model saw these bytes" is
//! checked against a direct [`ansi::Processor`] parse of the same bytes.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use instant::Instant;
use mio::unix::pipe;
use mio::{Interest, Token};
use parking_lot::{FairMutex, Mutex};
use warpui_core::color::ColorU;

use super::*;
use crate::SizeInfo;
use crate::event::{ExitReason, ObservedExitStatus};
use crate::local_tty::tmux_gateway::{
    GatewayEvent, GatewayLink, GatewaySwitch, OriginFlags, PaneChunk, PaneQueueLimits,
    PaneQueueReceiver, RefusalReason, TmuxRequest,
};
use crate::local_tty::{ChildEvent, EventedPty, EventedReadWrite, mio_channel};
use crate::model::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, LineClearMode, Mode, StandardCharset,
    TabulationClearMode,
};
use crate::model::index::VisibleRow;
use crate::model::selection::ScrollDelta;
use crate::model::{KeyboardModes, KeyboardModesApplyBehavior};
use crate::tmux::{ClientEvent, PaneId};

const WAIT: Duration = Duration::from_secs(10);
const WRITE_TOKEN: Token = Token(3);

/// The terminal model double: a trace of handler calls plus the exit reasons it received.
#[derive(Default)]
pub(super) struct RecordingTerminal {
    pub trace: Vec<String>,
    pub exits: Vec<String>,
}

impl RecordingTerminal {
    fn record(&mut self, call: String) {
        self.trace.push(call);
    }

    /// The printable characters the model received, in order.
    pub fn text(&self) -> String {
        self.trace
            .iter()
            .filter_map(|call| {
                let quoted = call.strip_prefix("input(")?.strip_suffix(')')?;
                quoted.strip_prefix('\'')?.strip_suffix('\'').map(|c| {
                    match c {
                        "\\\\" => "\\",
                        "\\'" => "'",
                        other => other,
                    }
                    .to_owned()
                })
            })
            .collect()
    }
}

impl ActiveTerminal for RecordingTerminal {
    fn exit(&mut self, reason: ExitReason) {
        self.exits.push(format!("{reason:?}"));
    }
}

/// The handler calls a direct parse of `bytes` produces, for comparison with the event loop.
pub(super) fn direct_parse_trace(chunks: &[&[u8]]) -> Vec<String> {
    let mut terminal = RecordingTerminal::default();
    let mut processor = ansi::Processor::new();
    let mut responses = Vec::new();
    for chunk in chunks {
        processor.parse_bytes(&mut terminal, chunk, &mut responses);
    }
    terminal.trace
}

/// An in-memory PTY: the loop reads what the test "shell" writes and writes what the test reads.
pub(super) struct TestPty {
    reader: pipe::Receiver,
    writer: pipe::Sender,
    child: pipe::Receiver,
    resizes: Arc<Mutex<Vec<(usize, usize)>>>,
    killed: Arc<AtomicBool>,
}

impl EventedReadWrite for TestPty {
    type Reader = pipe::Receiver;
    type Writer = pipe::Sender;

    fn register(&mut self, poll: &mio::Poll, _: Interest) -> io::Result<()> {
        poll.registry()
            .register(&mut self.reader, PTY_TOKEN, Interest::READABLE)?;
        poll.registry()
            .register(&mut self.writer, WRITE_TOKEN, Interest::WRITABLE)?;
        poll.registry()
            .register(&mut self.child, SIGNALS_TOKEN, Interest::READABLE)
    }

    fn reregister(&mut self, _: &mio::Poll, _: Interest) -> io::Result<()> {
        Ok(())
    }

    fn deregister(&mut self, poll: &mio::Poll) -> io::Result<()> {
        poll.registry().deregister(&mut self.reader)?;
        poll.registry().deregister(&mut self.writer)?;
        poll.registry().deregister(&mut self.child)
    }

    fn reader(&mut self) -> &mut pipe::Receiver {
        &mut self.reader
    }

    fn read_token(&self) -> Token {
        PTY_TOKEN
    }

    fn writer(&mut self) -> &mut pipe::Sender {
        &mut self.writer
    }

    fn write_token(&self) -> Token {
        WRITE_TOKEN
    }
}

impl EventedPty for TestPty {
    fn child_event_token(&self) -> Token {
        SIGNALS_TOKEN
    }

    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let mut code = [0u8; 1];
        match self.child.read(&mut code) {
            Ok(1) => Some(ChildEvent::Exited(ObservedExitStatus::Code(i32::from(
                code[0],
            )))),
            _ => None,
        }
    }

    fn on_resize(&mut self, size: &SizeInfo) {
        self.resizes.lock().push((size.rows(), size.columns()));
    }

    fn kill(self) -> anyhow::Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// The test's side of a running event loop.
pub(super) struct Harness {
    pub terminal: Arc<FairMutex<RecordingTerminal>>,
    pub tx: mio_channel::Sender<Message>,
    /// Writes here are PTY output (what the shell prints).
    shell_out: Option<pipe::Sender>,
    /// Reads here are PTY input (what the loop wrote to the shell).
    shell_in: pipe::Receiver,
    child: pipe::Sender,
    written: Vec<u8>,
    pub resizes: Arc<Mutex<Vec<(usize, usize)>>>,
    pub killed: Arc<AtomicBool>,
    handle: Option<JoinHandle<mio_channel::Receiver<Message>>>,
    _wakeups: async_channel::Receiver<()>,
    _events: async_channel::Receiver<crate::event::Event>,
}

impl Harness {
    /// Builds the loop; `configure` may adjust it before it is spawned.
    pub fn spawn_with(
        configure: impl FnOnce(
            EventLoop<TestPty, RecordingTerminal>,
            &mio_channel::Sender<Message>,
        ) -> EventLoop<TestPty, RecordingTerminal>,
    ) -> Self {
        let (shell_out, reader) = pipe::new().expect("pipe");
        let (writer, mut shell_in) = pipe::new().expect("pipe");
        let (child, child_rx) = pipe::new().expect("pipe");
        shell_out.set_nonblocking(false).expect("blocking");
        shell_in.set_nonblocking(true).expect("nonblocking");
        child.set_nonblocking(false).expect("blocking");
        let _ = &mut shell_in;

        let resizes = Arc::new(Mutex::new(Vec::new()));
        let killed = Arc::new(AtomicBool::new(false));
        let pty = TestPty {
            reader,
            writer,
            child: child_rx,
            resizes: resizes.clone(),
            killed: killed.clone(),
        };

        let (wakeups_tx, wakeups) = async_channel::unbounded();
        let (events_tx, events) = async_channel::unbounded::<crate::event::Event>();
        let (pty_reads_tx, _) = async_broadcast::broadcast(16);
        let listener = ChannelEventListener::new(wakeups_tx, events_tx, pty_reads_tx);

        let terminal = Arc::new(FairMutex::new(RecordingTerminal::default()));
        let (tx, rx) = mio_channel::channel();
        let event_loop = configure(EventLoop::new(terminal.clone(), listener, pty, rx), &tx);
        let handle = event_loop.spawn();
        Self {
            terminal,
            tx,
            shell_out: Some(shell_out),
            shell_in,
            child,
            written: Vec::new(),
            resizes,
            killed,
            handle: Some(handle),
            _wakeups: wakeups,
            _events: events,
        }
    }

    pub fn spawn() -> Self {
        Self::spawn_with(|event_loop, _| event_loop)
    }

    /// The shell prints `bytes`.
    pub fn shell_prints(&mut self, bytes: &[u8]) {
        self.shell_out
            .as_mut()
            .expect("shell output is open")
            .write_all(bytes)
            .expect("write to pty");
    }

    /// Closes the shell's side of the PTY output.
    pub fn close_shell_output(&mut self) {
        self.shell_out = None;
    }

    pub fn child_exits(&mut self, code: u8) {
        self.child.write_all(&[code]).expect("signal child exit");
    }

    pub fn send(&self, message: Message) {
        self.tx.send(message).expect("event loop is running");
    }

    /// Waits until `done` holds for the terminal double.
    pub fn wait_for_terminal(&self, what: &str, done: impl Fn(&RecordingTerminal) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            if done(&self.terminal.lock()) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Everything the loop has written to the PTY so far, waiting until it contains `needle`.
    pub fn wait_for_written(&mut self, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        loop {
            self.drain_written();
            if contains(&self.written, needle) {
                return self.written.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {:?} in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&self.written)
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Takes what the loop has written to the PTY so far.
    pub fn take_written(&mut self) -> Vec<u8> {
        self.drain_written();
        std::mem::take(&mut self.written)
    }

    fn drain_written(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            match self.shell_in.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => self.written.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => panic!("reading pty input: {err}"),
            }
        }
    }

    /// Waits for the loop thread to finish.
    pub fn join(&mut self) {
        let handle = self.handle.take().expect("not joined yet");
        handle.join().expect("event loop thread");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.tx.send(Message::Shutdown);
            let _ = handle.join();
        }
    }
}

pub(super) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// A `tmux -CC` session as the PTY delivers it: a prompt, the DCS, control lines, `%exit`, the
/// string terminator, and the next prompt.
pub(super) fn recorded_session() -> Vec<u8> {
    let mut bytes = b"$ tmux -CC\r\n".to_vec();
    bytes.extend_from_slice(b"\x1bP1000p");
    bytes.extend_from_slice(
        b"%begin 1791761427 280 0\n%end 1791761427 280 0\n%window-add @1\n\
          %sessions-changed\n%session-changed $0 main\n%output %1 hello\\015\\012\n%exit\n",
    );
    bytes.extend_from_slice(b"\x1b\\$ done\r\n");
    bytes
}

// Characterization of today's behavior. These pass unchanged with the tmux gateway off.

#[test]
fn recorded_tmux_session_reaches_the_model_unchanged() {
    let session = recorded_session();
    let mut h = Harness::spawn();
    h.shell_prints(&session);
    let expected = direct_parse_trace(&[&session]);
    h.wait_for_terminal("the whole session", |t| t.trace.len() >= expected.len());
    assert_eq!(h.terminal.lock().trace, expected);
    // Today the VT parser swallows the control stream as DCS payload.
    let text = h.terminal.lock().text();
    assert_eq!(text, "$ tmux -CC$ done");
}

#[test]
fn a_session_split_across_reads_reaches_the_model_unchanged() {
    let session = recorded_session();
    let mut h = Harness::spawn();
    // Split inside the DCS so a detector would have to carry state across reads.
    let (first, rest) = session.split_at(b"$ tmux -CC\r\n\x1bP10".len());
    h.shell_prints(first);
    h.wait_for_terminal("the prompt", |t| t.text().contains("$ tmux -CC"));
    h.shell_prints(rest);
    let expected = direct_parse_trace(&[first, rest]);
    h.wait_for_terminal("the whole session", |t| t.trace.len() >= expected.len());
    assert_eq!(h.terminal.lock().trace, expected);
}

#[test]
fn input_messages_are_written_to_the_pty_in_order() {
    let mut h = Harness::spawn();
    h.send(Message::Input(b"ls".as_slice().into()));
    h.send(Message::Input(b" -la\n".to_vec().into()));
    assert_eq!(h.wait_for_written(b"ls -la\n"), b"ls -la\n");
}

#[test]
fn terminal_responses_are_written_back_to_the_pty() {
    let mut h = Harness::spawn();
    h.shell_prints(b"\x1b[5n");
    h.wait_for_written(b"<device_status>");
}

#[test]
fn resize_messages_reach_the_pty() {
    let mut h = Harness::spawn();
    h.send(Message::Resize(SizeInfo::new_without_font_metrics(30, 100)));
    h.send(Message::Input(b"x".as_slice().into()));
    h.wait_for_written(b"x");
    assert_eq!(*h.resizes.lock(), vec![(30, 100)]);
}

#[test]
fn child_exit_reports_the_exit_status_and_does_not_kill() {
    let mut h = Harness::spawn();
    h.shell_prints(b"bye\r\n");
    h.wait_for_terminal("output", |t| t.text().contains("bye"));
    h.child_exits(3);
    h.join();
    assert_eq!(
        h.terminal.lock().exits,
        vec![format!(
            "{:?}",
            ExitReason::ShellProcessExited {
                status: ObservedExitStatus::Code(3)
            }
        )]
    );
    assert!(!h.killed.load(Ordering::SeqCst));
}

#[test]
fn shutdown_kills_the_pty_and_reports_a_disconnect() {
    let mut h = Harness::spawn();
    h.send(Message::Shutdown);
    h.join();
    assert_eq!(
        h.terminal.lock().exits,
        vec![format!("{:?}", ExitReason::PtyDisconnected)]
    );
    assert!(h.killed.load(Ordering::SeqCst));
}

#[test]
fn closing_the_pty_output_keeps_the_loop_running() {
    let mut h = Harness::spawn();
    h.shell_prints(b"last words");
    h.wait_for_terminal("output", |t| t.text().contains("last words"));
    h.close_shell_output();
    h.send(Message::Input(b"still here".as_slice().into()));
    h.wait_for_written(b"still here");
    assert!(h.terminal.lock().exits.is_empty());
}

// The tmux gateway (U5).

const TIME: u64 = 1_791_761_427;

fn checked(body: &str) -> String {
    format!("{:04x},{body}", crate::tmux::layout_checksum(body))
}

/// Plays tmux on the far side of the PTY: answers every command line the loop writes.
struct GatewayHarness {
    h: Harness,
    switch: GatewaySwitch,
    origin: OriginFlags,
    events: Arc<Mutex<Vec<GatewayEvent>>>,
    version: &'static str,
    number: u64,
    /// Bytes written to the PTY that have not been answered yet.
    unanswered: Vec<u8>,
    /// Every command line answered so far.
    commands: Vec<String>,
    /// Payload for commands that start with a key, instead of the default reply.
    replies: Vec<(String, Vec<String>)>,
}

impl GatewayHarness {
    fn spawn(enabled: bool) -> Self {
        Self::spawn_with(enabled, PaneQueueLimits::default())
    }

    fn spawn_with(enabled: bool, queue_limits: PaneQueueLimits) -> Self {
        let switch = GatewaySwitch::new(enabled);
        let origin = OriginFlags::default();
        let events = Arc::new(Mutex::new(Vec::new()));
        let link_switch = switch.clone();
        let link_origin = origin.clone();
        let sink_events = events.clone();
        let h = Harness::spawn_with(move |event_loop, tx| {
            let tx = tx.clone();
            event_loop.with_tmux_gateway(GatewayLink {
                switch: link_switch,
                origin: link_origin,
                sink: Box::new(move |event| sink_events.lock().push(event)),
                requester: Arc::new(move |request| {
                    let _ = tx.send(Message::Tmux(request));
                }),
                client_config: crate::tmux::ClientConfig::default(),
                queue_limits,
            })
        });
        Self {
            h,
            switch,
            origin,
            events,
            version: "3.2",
            number: 300,
            unanswered: Vec::new(),
            commands: Vec::new(),
            replies: Vec::new(),
        }
    }

    /// Warp runs `tmux -CC` (arming the origin), then the prompt, the DCS, tmux's reply to the
    /// attach command itself, and `%session-changed`.
    fn enter(&mut self) {
        self.origin.arm();
        self.enter_unarmed();
    }

    /// [`Self::enter`] for a control-mode start Warp did not run.
    fn enter_unarmed(&mut self) {
        let mut bytes = b"$ tmux -CC\r\n\x1bP1000p".to_vec();
        bytes.extend_from_slice(
            format!(
                "%begin {TIME} 280 0\n%end {TIME} 280 0\n%window-add @1\n%sessions-changed\n\
                 %session-changed $0 main\n"
            )
            .as_bytes(),
        );
        self.h.shell_prints(&bytes);
    }

    fn reply_for(&self, command: &str) -> Vec<String> {
        if let Some((_, reply)) = self
            .replies
            .iter()
            .find(|(prefix, _)| command.starts_with(prefix.as_str()))
        {
            return reply.clone();
        }
        if command.starts_with("display-message -p '#{version}") {
            vec![format!("{}\tclient-7", self.version)]
        } else if command.starts_with("list-windows") {
            let layout = checked("80x24,0,0,1");
            vec![format!("@1\t1\t1\t{layout}\t{layout}\t*\tmain")]
        } else if command.starts_with("list-panes") {
            vec!["%1\t@1\t1\t0\t0\t80\t24\t".to_owned()]
        } else if command == "show -gv prefix" {
            vec!["C-b".to_owned()]
        } else if command == "show -gv prefix2" {
            vec!["None".to_owned()]
        } else if command.starts_with("display-message -p -t") {
            vec!["0\t0".to_owned()]
        } else if command.starts_with("capture-pane") {
            vec![String::new()]
        } else {
            Vec::new()
        }
    }

    /// Answers whatever the loop has written. A bare newline (detach) gets `%exit`.
    fn answer(&mut self) {
        let written = self.h.take_written();
        self.unanswered.extend_from_slice(&written);
        let mut out = Vec::new();
        while let Some(newline) = self.unanswered.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.unanswered.drain(..=newline).collect();
            let command = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if command.is_empty() {
                out.extend_from_slice(b"%exit\n\x1b\\");
                continue;
            }
            let number = self.number;
            self.number += 1;
            out.extend_from_slice(format!("%begin {TIME} {number} 1\n").as_bytes());
            for reply_line in self.reply_for(&command) {
                out.extend_from_slice(reply_line.as_bytes());
                out.push(b'\n');
            }
            out.extend_from_slice(format!("%end {TIME} {number} 1\n").as_bytes());
            self.commands.push(command);
        }
        if !out.is_empty() {
            self.h.shell_prints(&out);
        }
    }

    /// Answers commands until `done` holds for the gateway's events.
    fn answer_until(&mut self, what: &str, done: impl Fn(&[GatewayEvent]) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.answer();
            if done(&self.events.lock()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events: {:?}; commands: {:?}",
                self.events.lock(),
                self.commands
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn attach(&mut self) {
        self.enter();
        self.answer_until("attach", |events| {
            events
                .iter()
                .any(|e| matches!(e, GatewayEvent::Client(ClientEvent::Attached { .. })))
        });
        // The pane's seed (capture and cursor) is answered too.
        self.answer_until("the seed", |events| {
            events.iter().any(|e| {
                matches!(e, GatewayEvent::PaneQueueOpened { queue } if queue.pane_id() == PaneId(1))
            })
        });
    }

    fn has_event(&self, matches: impl Fn(&GatewayEvent) -> bool) -> bool {
        self.events.lock().iter().any(matches)
    }

    fn count_events(&self, matches: impl Fn(&GatewayEvent) -> bool) -> usize {
        self.events.lock().iter().filter(|e| matches(e)).count()
    }

    fn take_queue(&self, pane_id: PaneId) -> PaneQueueReceiver {
        let mut events = self.events.lock();
        let index = events
            .iter()
            .position(|e| matches!(e, GatewayEvent::PaneQueueOpened { queue } if queue.pane_id() == pane_id))
            .expect("pane queue opened");
        match events.remove(index) {
            GatewayEvent::PaneQueueOpened { queue } => queue,
            _ => unreachable!(),
        }
    }

    /// Waits for pane output and returns it, skipping snapshots.
    fn pane_output(queue: &PaneQueueReceiver, len: usize) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::new();
        while out.len() < len {
            match queue.try_recv() {
                Some(PaneChunk::Output(bytes)) => out.extend(bytes),
                Some(PaneChunk::Snapshot(_)) => {}
                None => {
                    assert!(Instant::now() < deadline, "timed out; got {out:?}");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        out
    }
}

#[test]
fn flag_off_a_linked_loop_passes_the_session_to_the_model_unchanged() {
    let session = recorded_session();
    let mut g = GatewayHarness::spawn(false);
    g.h.shell_prints(&session);
    let expected = direct_parse_trace(&[&session]);
    g.h.wait_for_terminal("the whole session", |t| t.trace.len() >= expected.len());
    assert_eq!(g.h.terminal.lock().trace, expected);
    assert!(g.events.lock().is_empty());
    assert!(g.h.take_written().is_empty());
}

#[test]
fn flag_on_diverts_the_control_stream_and_the_model_gets_only_shell_bytes() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    assert_eq!(
        g.commands[0],
        "display-message -p '#{version}\t#{client_name}'"
    );
    let queue = g.take_queue(PaneId(1));
    g.h.shell_prints(b"%output %1 hello\\015\\012\n");
    assert_eq!(GatewayHarness::pane_output(&queue, 7), b"hello\r\n");

    g.h.shell_prints(b"%exit\n\x1b\\$ done\r\n");
    g.h.wait_for_terminal("the next prompt", |t| t.text().contains("$ done"));
    assert_eq!(g.h.terminal.lock().text(), "$ tmux -CC$ done");
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Client(ClientEvent::Exited { .. }))));
    g.answer_until("the end", |events| {
        events.iter().any(|e| matches!(e, GatewayEvent::Ended))
    });
    assert!(queue.is_closed());
    assert!(
        !g.h.terminal
            .lock()
            .trace
            .iter()
            .any(|call| call.contains('%'))
    );
}

#[test]
fn the_entry_dcs_split_across_reads_is_cancelled_in_the_model() {
    let mut g = GatewayHarness::spawn(true);
    g.origin.arm();
    g.h.shell_prints(b"$ tmux -CC\r\n\x1bP10");
    g.h.wait_for_terminal("the prompt", |t| t.text().contains("$ tmux -CC"));
    g.h.shell_prints(b"00p");
    g.h.shell_prints(
        format!("%begin {TIME} 280 0\n%end {TIME} 280 0\n%session-changed $0 main\n").as_bytes(),
    );
    g.answer_until("attach", |events| {
        events
            .iter()
            .any(|e| matches!(e, GatewayEvent::Client(ClientEvent::Attached { .. })))
    });
    g.h.shell_prints(b"%exit\n\x1b\\after");
    g.h.wait_for_terminal("the shell", |t| t.text().ends_with("after"));
    assert_eq!(g.h.terminal.lock().text(), "$ tmux -CCafter");
}

#[test]
fn tmux_env_in_the_origin_shell_stays_raw_with_the_nested_note() {
    let session = recorded_session();
    let mut g = GatewayHarness::spawn(true);
    g.origin.set_tmux_env(true);
    g.h.shell_prints(&session);
    let expected = direct_parse_trace(&[&session]);
    g.h.wait_for_terminal("the whole session", |t| t.trace.len() >= expected.len());
    assert_eq!(g.h.terminal.lock().trace, expected);
    let events = g.events.lock();
    assert!(
        matches!(&events[..], [GatewayEvent::Refused(reason)] if *reason == RefusalReason::TmuxEnvSet),
        "{events:?}"
    );
    assert_eq!(
        RefusalReason::TmuxEnvSet.to_string(),
        "Nested tmux is not supported natively"
    );
    drop(events);
    assert!(g.h.take_written().is_empty());
}

#[test]
fn an_origin_that_is_a_tmux_pane_stays_raw() {
    let mut g = GatewayHarness::spawn(true);
    g.origin.set_origin_is_tmux_pane(true);
    g.enter();
    g.h.wait_for_terminal("the prompt", |t| t.text().contains("$ tmux -CC"));
    let deadline = Instant::now() + WAIT;
    while !g.has_event(|e| matches!(e, GatewayEvent::Refused(_))) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(g.has_event(
        |e| matches!(e, GatewayEvent::Refused(r) if *r == RefusalReason::OriginIsTmuxPane)
    ));
    assert!(!g.has_event(|e| matches!(e, GatewayEvent::Starting)));
}

#[test]
fn an_inner_pane_printing_the_entry_dcs_does_not_start_a_second_session() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    let queue = g.take_queue(PaneId(1));
    g.h.shell_prints(b"%output %1 \\033P1000p%begin\n");
    assert_eq!(GatewayHarness::pane_output(&queue, 13), b"\x1bP1000p%begin");
    g.answer();
    assert_eq!(g.count_events(|e| matches!(e, GatewayEvent::Starting)), 1);
    assert!(!g.has_event(|e| matches!(e, GatewayEvent::Client(ClientEvent::Desync { .. }))));
    assert_eq!(g.h.terminal.lock().text(), "$ tmux -CC");
}

#[test]
fn a_3_1_server_detaches_and_reports_the_version_reason() {
    let mut g = GatewayHarness::spawn(true);
    g.version = "3.1";
    g.enter();
    g.answer_until("the refusal and the end", |events| {
        events.iter().any(|e| matches!(e, GatewayEvent::Ended))
    });
    let reason = RefusalReason::UnsupportedVersion {
        version: "3.1".into(),
    };
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Refused(r) if *r == reason)));
    assert_eq!(
        reason.to_string(),
        "Native tmux needs tmux 3.2 or newer (found 3.1)"
    );
    assert!(!g.has_event(|e| matches!(
        e,
        GatewayEvent::Client(ClientEvent::AttachFailed { .. } | ClientEvent::Attached { .. })
    )));
    // The version query was the only command; the detach is the bare newline.
    assert_eq!(g.commands.len(), 1);
    g.h.shell_prints(b"$ ");
    g.h.wait_for_terminal("the shell", |t| t.text() == "$ tmux -CC$ ");
}

#[test]
fn child_exit_while_attached_marks_the_transport_lost() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    let queue = g.take_queue(PaneId(1));
    g.h.child_exits(255);
    g.h.join();
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Client(ClientEvent::TransportLost))));
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Ended)));
    assert!(queue.is_closed());
    assert_eq!(
        g.h.terminal.lock().exits,
        vec![format!(
            "{:?}",
            ExitReason::ShellProcessExited {
                status: ObservedExitStatus::Code(255)
            }
        )]
    );
}

#[test]
fn pty_eof_while_attached_marks_the_transport_lost() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    g.h.close_shell_output();
    let deadline = Instant::now() + WAIT;
    while !g.has_event(|e| matches!(e, GatewayEvent::Client(ClientEvent::TransportLost))) {
        assert!(Instant::now() < deadline, "{:?}", g.events.lock());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Ended)));
}

#[test]
fn while_attached_control_lines_never_take_the_origin_model_lock() {
    let mut g = GatewayHarness::spawn(true);
    g.h.shell_prints(b"$ tmux -CC\r\n");
    g.h.wait_for_terminal("the prompt", |t| t.text() == "$ tmux -CC");
    // Hold the origin model's lock for the whole attach. The loop would block on it at the first
    // control line if it tried to take it.
    let terminal = g.h.terminal.clone();
    let guard = terminal.lock();
    g.origin.arm();
    let mut bytes = b"\x1bP1000p".to_vec();
    bytes.extend_from_slice(
        format!("%begin {TIME} 280 0\n%end {TIME} 280 0\n%session-changed $0 main\n").as_bytes(),
    );
    g.h.shell_prints(&bytes);
    g.answer_until("attach", |events| {
        events
            .iter()
            .any(|e| matches!(e, GatewayEvent::Client(ClientEvent::Attached { .. })))
    });
    let queue = {
        g.answer_until("the seed", |events| {
            events
                .iter()
                .any(|e| matches!(e, GatewayEvent::PaneQueueOpened { .. }))
        });
        g.take_queue(PaneId(1))
    };
    g.h.shell_prints(b"%output %1 x\n%window-renamed @1 build\n");
    assert_eq!(GatewayHarness::pane_output(&queue, 1), b"x");
    drop(guard);
    assert_eq!(g.h.terminal.lock().text(), "$ tmux -CC");
}

#[test]
fn turning_the_switch_off_mid_session_keeps_the_session_until_exit() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    g.switch.set_enabled(false);
    g.h.shell_prints(b"%window-renamed @1 build\n%exit\n\x1b\\$ ");
    g.h.wait_for_terminal("the shell", |t| t.text() == "$ tmux -CC$ ");
    // With the switch off, the next DCS is the model's again.
    g.h.shell_prints(b"\x1bP1000p%exit\n\x1b\\!");
    g.h.wait_for_terminal("the raw dcs", |t| t.text().ends_with('!'));
    assert_eq!(g.count_events(|e| matches!(e, GatewayEvent::Starting)), 1);
}

#[test]
fn a_backed_up_pane_queue_pauses_the_pane_and_resumes_when_drained() {
    let limits = PaneQueueLimits {
        high_water: 8,
        low_water: 2,
        hard_cap: 64,
    };
    let mut g = GatewayHarness::spawn_with(true, limits);
    g.attach();
    let queue = g.take_queue(PaneId(1));
    while queue.try_recv().is_some() {}
    g.h.shell_prints(b"%output %1 0123456789\n");
    g.answer();
    let written = g.h.wait_for_written(b"refresh-client -A '%1:pause'\n");
    assert!(contains(&written, b"refresh-client -A '%1:pause'\n"));
    assert!(queue.is_paused());
    assert_eq!(
        queue.try_recv(),
        Some(PaneChunk::Output(b"0123456789".to_vec()))
    );
    assert!(!queue.is_paused());
    g.h.wait_for_written(b"refresh-client -A '%1:continue'\n");
}

#[test]
fn commands_from_the_app_get_their_replies_by_tag() {
    let mut g = GatewayHarness::spawn(true);
    g.replies.push((
        "list-sessions".to_owned(),
        vec!["main: 1 windows".to_owned()],
    ));
    g.attach();
    g.h.send(Message::Tmux(TmuxRequest::Command {
        tag: 42,
        line: "list-sessions".to_owned(),
    }));
    g.answer_until("the reply", |events| {
        events
            .iter()
            .any(|e| matches!(e, GatewayEvent::CommandCompleted { tag: 42, .. }))
    });
    let events = g.events.lock();
    let reply = events
        .iter()
        .find_map(|e| match e {
            GatewayEvent::CommandCompleted { reply, .. } => Some(reply),
            _ => None,
        })
        .unwrap();
    assert_eq!(reply.lines, vec![b"main: 1 windows".to_vec()]);
}

#[test]
fn pane_input_and_detach_requests_are_written_to_the_pty() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    g.h.send(Message::Tmux(TmuxRequest::PaneInput {
        pane_id: PaneId(1),
        bytes: b"ls\r".to_vec(),
    }));
    let deadline = Instant::now() + WAIT;
    while !g
        .commands
        .iter()
        .any(|c| c == "send-keys -t %1 -H 6c 73 0d")
    {
        assert!(Instant::now() < deadline, "{:?}", g.commands);
        g.answer();
        std::thread::sleep(Duration::from_millis(2));
    }
    g.h.send(Message::Tmux(TmuxRequest::Detach));
    g.answer_until("the end", |events| {
        events.iter().any(|e| matches!(e, GatewayEvent::Ended))
    });
}

#[test]
fn requests_without_a_session_are_dropped() {
    let mut g = GatewayHarness::spawn(true);
    g.h.send(Message::Tmux(TmuxRequest::Detach));
    g.h.send(Message::Input(b"x".as_slice().into()));
    assert_eq!(g.h.wait_for_written(b"x"), b"x");
    assert!(g.events.lock().is_empty());
}

impl ansi::Handler for RecordingTerminal {
    fn set_title(&mut self, p1: Option<String>) {
        self.record(format!("set_title({p1:?})"));
    }

    fn set_cursor_style(&mut self, p1: Option<CursorStyle>) {
        self.record(format!("set_cursor_style({p1:?})"));
    }

    fn set_cursor_shape(&mut self, p1: CursorShape) {
        self.record(format!("set_cursor_shape({p1:?})"));
    }

    fn input(&mut self, p1: char) {
        self.record(format!("input({p1:?})"));
    }

    fn goto(&mut self, p1: VisibleRow, p2: usize) {
        self.record(format!("goto({p1:?}, {p2:?})"));
    }

    fn goto_line(&mut self, p1: VisibleRow) {
        self.record(format!("goto_line({p1:?})"));
    }

    fn goto_col(&mut self, p1: usize) {
        self.record(format!("goto_col({p1:?})"));
    }

    fn insert_blank(&mut self, p1: usize) {
        self.record(format!("insert_blank({p1:?})"));
    }

    fn move_up(&mut self, p1: usize) {
        self.record(format!("move_up({p1:?})"));
    }

    fn move_down(&mut self, p1: usize) {
        self.record(format!("move_down({p1:?})"));
    }

    fn identify_terminal<W: io::Write>(&mut self, w1: &mut W, p2: Option<char>) {
        self.record(format!("identify_terminal({p2:?})"));
        let _ = w1.write_all(b"<identify_terminal>");
    }

    fn report_xtversion<W: io::Write>(&mut self, w1: &mut W) {
        self.record("report_xtversion".to_owned());
        let _ = w1.write_all(b"<report_xtversion>");
    }

    fn device_status<W: io::Write>(&mut self, w1: &mut W, p2: usize) {
        self.record(format!("device_status({p2:?})"));
        let _ = w1.write_all(b"<device_status>");
    }

    fn move_forward(&mut self, p1: usize) {
        self.record(format!("move_forward({p1:?})"));
    }

    fn move_backward(&mut self, p1: usize) {
        self.record(format!("move_backward({p1:?})"));
    }

    fn move_down_and_cr(&mut self, p1: usize) {
        self.record(format!("move_down_and_cr({p1:?})"));
    }

    fn move_up_and_cr(&mut self, p1: usize) {
        self.record(format!("move_up_and_cr({p1:?})"));
    }

    fn put_tab(&mut self, p1: u16) {
        self.record(format!("put_tab({p1:?})"));
    }

    fn backspace(&mut self) {
        self.record("backspace".to_owned());
    }

    fn carriage_return(&mut self) {
        self.record("carriage_return".to_owned());
    }

    fn linefeed(&mut self) -> ScrollDelta {
        self.record("linefeed".to_owned());
        ScrollDelta::zero()
    }

    fn bell(&mut self) {
        self.record("bell".to_owned());
    }

    fn substitute(&mut self) {
        self.record("substitute".to_owned());
    }

    fn newline(&mut self) {
        self.record("newline".to_owned());
    }

    fn set_horizontal_tabstop(&mut self) {
        self.record("set_horizontal_tabstop".to_owned());
    }

    fn scroll_up(&mut self, p1: usize) -> ScrollDelta {
        self.record(format!("scroll_up({p1:?})"));
        ScrollDelta::zero()
    }

    fn scroll_down(&mut self, p1: usize) -> ScrollDelta {
        self.record(format!("scroll_down({p1:?})"));
        ScrollDelta::zero()
    }

    fn insert_blank_lines(&mut self, p1: usize) -> ScrollDelta {
        self.record(format!("insert_blank_lines({p1:?})"));
        ScrollDelta::zero()
    }

    fn delete_lines(&mut self, p1: usize) -> ScrollDelta {
        self.record(format!("delete_lines({p1:?})"));
        ScrollDelta::zero()
    }

    fn erase_chars(&mut self, p1: usize) {
        self.record(format!("erase_chars({p1:?})"));
    }

    fn delete_chars(&mut self, p1: usize) {
        self.record(format!("delete_chars({p1:?})"));
    }

    fn move_backward_tabs(&mut self, p1: u16) {
        self.record(format!("move_backward_tabs({p1:?})"));
    }

    fn move_forward_tabs(&mut self, p1: u16) {
        self.record(format!("move_forward_tabs({p1:?})"));
    }

    fn save_cursor_position(&mut self) {
        self.record("save_cursor_position".to_owned());
    }

    fn restore_cursor_position(&mut self) {
        self.record("restore_cursor_position".to_owned());
    }

    fn clear_line(&mut self, p1: LineClearMode) {
        self.record(format!("clear_line({p1:?})"));
    }

    fn clear_screen(&mut self, p1: ClearMode) {
        self.record(format!("clear_screen({p1:?})"));
    }

    fn clear_tabs(&mut self, p1: TabulationClearMode) {
        self.record(format!("clear_tabs({p1:?})"));
    }

    fn reset_state(&mut self) {
        self.record("reset_state".to_owned());
    }

    fn reverse_index(&mut self) -> ScrollDelta {
        self.record("reverse_index".to_owned());
        ScrollDelta::zero()
    }

    fn terminal_attribute(&mut self, p1: Attr) {
        self.record(format!("terminal_attribute({p1:?})"));
    }

    fn set_mode(&mut self, p1: Mode) {
        self.record(format!("set_mode({p1:?})"));
    }

    fn unset_mode(&mut self, p1: Mode) {
        self.record(format!("unset_mode({p1:?})"));
    }

    fn set_keyboard_enhancement_flags(
        &mut self,
        p1: KeyboardModes,
        p2: KeyboardModesApplyBehavior,
    ) {
        self.record(format!("set_keyboard_enhancement_flags({p1:?}, {p2:?})"));
    }

    fn push_keyboard_enhancement_flags(&mut self, p1: KeyboardModes) {
        self.record(format!("push_keyboard_enhancement_flags({p1:?})"));
    }

    fn pop_keyboard_enhancement_flags(&mut self, p1: u16) {
        self.record(format!("pop_keyboard_enhancement_flags({p1:?})"));
    }

    fn query_keyboard_enhancement_flags<W: io::Write>(&mut self, w1: &mut W) {
        self.record("query_keyboard_enhancement_flags".to_owned());
        let _ = w1.write_all(b"<query_keyboard_enhancement_flags>");
    }

    fn set_scrolling_region(&mut self, p1: usize, p2: Option<usize>) {
        self.record(format!("set_scrolling_region({p1:?}, {p2:?})"));
    }

    fn set_keypad_application_mode(&mut self) {
        self.record("set_keypad_application_mode".to_owned());
    }

    fn unset_keypad_application_mode(&mut self) {
        self.record("unset_keypad_application_mode".to_owned());
    }

    fn set_active_charset(&mut self, p1: CharsetIndex) {
        self.record(format!("set_active_charset({p1:?})"));
    }

    fn configure_charset(&mut self, p1: CharsetIndex, p2: StandardCharset) {
        self.record(format!("configure_charset({p1:?}, {p2:?})"));
    }

    fn set_color(&mut self, p1: usize, p2: ColorU) {
        self.record(format!("set_color({p1:?}, {p2:?})"));
    }

    fn dynamic_color_sequence<W: io::Write>(&mut self, w1: &mut W, p2: u8, p3: usize, p4: &str) {
        self.record(format!("dynamic_color_sequence({p2:?}, {p3:?}, {p4:?})"));
        let _ = w1.write_all(b"<dynamic_color_sequence>");
    }

    fn reset_color(&mut self, p1: usize) {
        self.record(format!("reset_color({p1:?})"));
    }

    fn clipboard_store(&mut self, p1: u8, p2: &[u8]) {
        self.record(format!("clipboard_store({p1:?}, {p2:?})"));
    }

    fn clipboard_load(&mut self, p1: u8, p2: &str) {
        self.record(format!("clipboard_load({p1:?}, {p2:?})"));
    }

    fn decaln(&mut self) {
        self.record("decaln".to_owned());
    }

    fn push_title(&mut self) {
        self.record("push_title".to_owned());
    }

    fn pop_title(&mut self) {
        self.record("pop_title".to_owned());
    }

    fn text_area_size_pixels<W: io::Write>(&mut self, w1: &mut W) {
        self.record("text_area_size_pixels".to_owned());
        let _ = w1.write_all(b"<text_area_size_pixels>");
    }

    fn text_area_size_chars<W: io::Write>(&mut self, w1: &mut W) {
        self.record("text_area_size_chars".to_owned());
        let _ = w1.write_all(b"<text_area_size_chars>");
    }
}

#[test]
fn the_session_model_reaches_the_app_before_attached() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    let events = g.events.lock();
    let model = events
        .iter()
        .position(|e| matches!(e, GatewayEvent::Model(model) if model.windows().count() == 1))
        .expect("a model snapshot with the session's window");
    let attached = events
        .iter()
        .position(|e| matches!(e, GatewayEvent::Client(ClientEvent::Attached { .. })))
        .expect("attached");
    assert!(model < attached, "{events:?}");
}

#[test]
fn shutdown_while_attached_flushes_a_detach_first() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    g.answer();
    // Everything written so far was answered; nothing else is pending.
    let _ = g.h.take_written();
    g.h.send(Message::Shutdown);
    g.h.join();
    // The control client's detach is the bare newline (KTD3, R25: quit and window close detach).
    assert_eq!(g.h.take_written(), b"\n");
}

#[test]
fn shutdown_without_a_session_writes_nothing() {
    let mut g = GatewayHarness::spawn(true);
    g.h.send(Message::Shutdown);
    g.h.join();
    assert!(g.h.take_written().is_empty());
}

fn wait_for_event(g: &GatewayHarness, what: &str, matches: impl Fn(&GatewayEvent) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !g.has_event(&matches) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            g.events.lock()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn an_unarmed_entry_writes_nothing_until_the_user_opens_it() {
    let mut g = GatewayHarness::spawn(true);
    g.enter_unarmed();
    wait_for_event(&g, "the confirmation", |e| {
        matches!(e, GatewayEvent::ConfirmPending)
    });
    g.h.wait_for_terminal("the prompt", |t| t.text() == "$ tmux -CC");
    std::thread::sleep(Duration::from_millis(50));
    assert!(g.h.take_written().is_empty());
    assert!(!g.has_event(|e| matches!(e, GatewayEvent::Starting)));

    g.h.send(Message::Tmux(TmuxRequest::ConfirmOpen));
    g.answer_until("attach", |events| {
        events
            .iter()
            .any(|e| matches!(e, GatewayEvent::Client(ClientEvent::Attached { .. })))
    });
    assert_eq!(g.h.terminal.lock().text(), "$ tmux -CC");
}

#[test]
fn dismissing_an_unarmed_entry_shows_the_held_bytes_as_output() {
    let mut g = GatewayHarness::spawn(true);
    let cat = b"$ cat notes\r\n\x1bP1000phidden\x1b\\ shown";
    g.h.shell_prints(cat);
    wait_for_event(&g, "the confirmation", |e| {
        matches!(e, GatewayEvent::ConfirmPending)
    });
    g.h.send(Message::Tmux(TmuxRequest::ConfirmDismiss));
    g.h.wait_for_terminal("the released bytes", |t| t.text().ends_with("shown"));
    let expected = direct_parse_trace(&[cat]);
    assert_eq!(g.h.terminal.lock().trace, expected);
    g.h.shell_prints(b"!");
    g.h.wait_for_terminal("more shell output", |t| t.text().ends_with('!'));
    assert!(g.h.take_written().is_empty());
}

#[test]
fn a_dropped_connection_while_attached_gives_the_origin_back_to_its_shell() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    let written = g.h.take_written().len();
    g.h.shell_prints(b"Connection to host closed.\r\n\x1b]133;A\x07$ ");
    wait_for_event(&g, "the end", |e| matches!(e, GatewayEvent::Ended));
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Client(ClientEvent::TransportLost))));
    g.h.shell_prints(b"ls\r\n");
    g.h.wait_for_terminal("the shell", |t| t.text().ends_with("ls"));
    assert_eq!(
        g.h.terminal.lock().text(),
        "$ tmux -CCConnection to host closed.$ ls"
    );
    let _ = written;
    assert!(g.h.take_written().is_empty());
}

#[test]
fn a_desync_detaches_and_the_shell_gets_the_pty_back() {
    let mut g = GatewayHarness::spawn(true);
    g.attach();
    g.answer();
    g.h.shell_prints(format!("%begin {TIME} 9999 1\n%end {TIME} 9999 1\n").as_bytes());
    g.answer_until("the end", |events| {
        events.iter().any(|e| matches!(e, GatewayEvent::Ended))
    });
    assert!(g.has_event(|e| matches!(e, GatewayEvent::Client(ClientEvent::Desync { .. }))));
    g.h.shell_prints(b"$ ");
    g.h.wait_for_terminal("the shell", |t| t.text() == "$ tmux -CC$ ");
}

/// A PTY whose output is a fixed stream that never pauses until it is used up.
struct FloodPty {
    reader: FloodReader,
    writer: SharedWriter,
}

struct FloodReader {
    data: Vec<u8>,
    position: usize,
}

impl Read for FloodReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position == self.data.len() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = buf.len().min(16 << 10).min(self.data.len() - self.position);
        buf[..n].copy_from_slice(&self.data[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl EventedReadWrite for FloodPty {
    type Reader = FloodReader;
    type Writer = SharedWriter;

    fn register(&mut self, _: &mio::Poll, _: Interest) -> io::Result<()> {
        Ok(())
    }

    fn reregister(&mut self, _: &mio::Poll, _: Interest) -> io::Result<()> {
        Ok(())
    }

    fn deregister(&mut self, _: &mio::Poll) -> io::Result<()> {
        Ok(())
    }

    fn reader(&mut self) -> &mut FloodReader {
        &mut self.reader
    }

    fn read_token(&self) -> Token {
        PTY_TOKEN
    }

    fn writer(&mut self) -> &mut SharedWriter {
        &mut self.writer
    }

    fn write_token(&self) -> Token {
        WRITE_TOKEN
    }
}

impl EventedPty for FloodPty {
    fn child_event_token(&self) -> Token {
        SIGNALS_TOKEN
    }

    fn next_child_event(&mut self) -> Option<ChildEvent> {
        None
    }

    fn on_resize(&mut self, _: &SizeInfo) {}

    fn kill(self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn a_control_stream_flood_yields_so_queued_writes_go_out() {
    let mut data =
        format!("\x1bP1000p%begin {TIME} 280 0\n%end {TIME} 280 0\n%session-changed $0 main\n")
            .into_bytes();
    while data.len() < 4 << 20 {
        data.extend_from_slice(
            b"%output %1 yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\\015\\012\n",
        );
    }
    let total = data.len();
    let writer = SharedWriter::default();
    let pty = FloodPty {
        reader: FloodReader { data, position: 0 },
        writer: writer.clone(),
    };
    let (wakeups_tx, _wakeups) = async_channel::unbounded();
    let (events_tx, _events) = async_channel::unbounded::<crate::event::Event>();
    let (pty_reads_tx, _) = async_broadcast::broadcast(16);
    let listener = ChannelEventListener::new(wakeups_tx, events_tx, pty_reads_tx);
    let terminal = Arc::new(FairMutex::new(RecordingTerminal::default()));
    let (_tx, rx) = mio_channel::channel();
    let origin = OriginFlags::default();
    origin.arm();
    let mut event_loop =
        EventLoop::new(terminal, listener, pty, rx).with_tmux_gateway(GatewayLink {
            switch: GatewaySwitch::new(true),
            origin,
            sink: Box::new(|_| {}),
            requester: Arc::new(|_| {}),
            client_config: crate::tmux::ClientConfig::default(),
            queue_limits: PaneQueueLimits::default(),
        });

    let mut state = State::default();
    let mut buf = vec![0u8; READ_BUFFER_SIZE];
    let mut can_read = true;
    event_loop
        .pty_read(&mut state, &mut buf, &mut can_read)
        .expect("read");
    let consumed = event_loop.pty.reader.position;
    assert!(
        consumed < total,
        "the read only returned once the flood ended ({consumed} of {total} bytes)"
    );
    assert!(can_read);
    assert!(state.needs_write());
    let mut can_write = true;
    event_loop
        .pty_write(&mut state, &mut can_write)
        .expect("write");
    assert!(contains(
        &writer.0.lock(),
        b"display-message -p '#{version}"
    ));
}
