//! The tmux control-mode gateway on a local PTY (KTD2, KTD12).
//!
//! While the app's `terminal.tmux_control_mode` setting is on, the PTY event loop hands every raw
//! read to a [`Gateway`] before it takes the origin model's lock. Until tmux announces control
//! mode with `ESC P1000p`, every byte is the shell's and is passed through untouched. From the
//! DCS on, the bytes are the control stream: they drive a [`ControlClient`] on the event loop
//! thread, its command lines are written back to the same PTY, and only the shell bytes before
//! the DCS and after `%exit` reach the origin model. Pane payloads never touch the origin model:
//! they go to one bounded [`PaneQueueReceiver`] per pane, to be parsed elsewhere.
//!
//! The gateway never locks a pane model and never parses pane bytes, so the lock order stays
//! "control core, then pane model". Pane bytes are never scanned for the entry DCS, so an inner
//! program printing `ESC P1000p` cannot start a second session.
//!
//! Control-mode bytes can appear in any output (`cat` of a file, a remote host's output), so an
//! entry only becomes a session on its own when Warp just ran a tmux command on this origin and
//! armed it ([`OriginFlags::arm`]). Any other entry is held, with nothing written to the PTY,
//! until the app answers [`GatewayEvent::ConfirmPending`] (R3). A session whose attach does not
//! complete within [`HANDSHAKE_DEADLINE`] is abandoned and its bytes go to the origin model.
//!
//! The crate cannot read app settings, so the app mirrors the setting into a [`GatewaySwitch`]
//! and reports what it knows about the origin pane through [`OriginFlags`].

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use instant::Instant;

use crate::tmux::{
    CONTROL_MODE_DCS, ClientConfig, ClientEvent, ClientKey, CommandId, CommandReply, ControlClient,
    PaneId, PaneSnapshot, Phase, SessionModel,
};

/// CAN: aborts an escape sequence in the VT parser. Written to the origin model when a DCS
/// prefix it already received turns out to be the control-mode entry.
const CANCEL: u8 = 0x18;

/// How long after Warp runs a tmux command on an origin a control-mode entry there starts a
/// session without asking.
pub const ARM_WINDOW: Duration = Duration::from_secs(10);

/// How long a session may take to show that tmux attached before its bytes go back to the
/// origin model.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);

/// How long a disconnected session (desync, lost control) may wait for tmux's `%exit` before the
/// PTY goes back to the shell anyway.
const DISCONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// The most control-stream bytes held for an unconfirmed entry or an unfinished attach. Past it
/// the bytes go to the origin model as plain output.
const MAX_HELD_BYTES: usize = 1 << 20;

/// The app's `terminal.tmux_control_mode` setting (folded with the feature flag), shared by every
/// event loop. Reading it is one relaxed atomic load.
#[derive(Clone, Debug, Default)]
pub struct GatewaySwitch(Arc<AtomicBool>);

impl GatewaySwitch {
    pub fn new(enabled: bool) -> Self {
        Self(Arc::new(AtomicBool::new(enabled)))
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

const TMUX_ENV: u8 = 1;
const ORIGIN_IS_TMUX_PANE: u8 = 1 << 1;

#[derive(Debug, Default)]
struct OriginState {
    bits: AtomicU8,
    armed_until: Mutex<Option<Instant>>,
}

/// Facts about one origin pane, set by the app: what rules out a native session (R6), and
/// whether Warp itself just started tmux there (R3).
#[derive(Clone, Debug, Default)]
pub struct OriginFlags(Arc<OriginState>);

impl OriginFlags {
    /// `$TMUX` is set in the origin shell: it already runs inside tmux.
    pub fn set_tmux_env(&self, set: bool) {
        self.set(TMUX_ENV, set);
    }

    /// The origin pane is itself a tmux pane (for example `ssh host` from a native tmux pane).
    pub fn set_origin_is_tmux_pane(&self, set: bool) {
        self.set(ORIGIN_IS_TMUX_PANE, set);
    }

    /// Why a control-mode entry on this origin must stay raw, if it must.
    pub fn nested_reason(&self) -> Option<RefusalReason> {
        let bits = self.0.bits.load(Ordering::Relaxed);
        if bits & ORIGIN_IS_TMUX_PANE != 0 {
            Some(RefusalReason::OriginIsTmuxPane)
        } else if bits & TMUX_ENV != 0 {
            Some(RefusalReason::TmuxEnvSet)
        } else {
            None
        }
    }

    /// Warp is running a tmux command here (typed and converted, or from the session menu): the
    /// next control-mode entry within [`ARM_WINDOW`] starts a session without asking.
    pub fn arm(&self) {
        *self.armed_until() = Some(Instant::now() + ARM_WINDOW);
    }

    /// Withdraws [`Self::arm`]: the command Warp ran is over, or another one started.
    pub fn disarm(&self) {
        *self.armed_until() = None;
    }

    /// Whether an entry now would start a session without asking. Consumes the arm.
    fn take_armed(&self) -> bool {
        self.armed_until()
            .take()
            .is_some_and(|until| Instant::now() < until)
    }

    fn armed_until(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.0
            .armed_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A handle that does not keep the origin alive.
    pub fn downgrade(&self) -> WeakOriginFlags {
        WeakOriginFlags(Arc::downgrade(&self.0))
    }

    fn set(&self, bit: u8, set: bool) {
        if set {
            self.0.bits.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.0.bits.fetch_and(!bit, Ordering::Relaxed);
        }
    }
}

/// [`OriginFlags`] that do not keep the origin alive; the gateway holds the strong handle.
#[derive(Clone, Debug)]
pub struct WeakOriginFlags(std::sync::Weak<OriginState>);

impl WeakOriginFlags {
    /// `None` once the origin's event loop has ended.
    pub fn upgrade(&self) -> Option<OriginFlags> {
        self.0.upgrade().map(OriginFlags)
    }
}

/// Why a tmux control-mode entry did not become a native session (R6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// `$TMUX` is set in the origin shell.
    TmuxEnvSet,
    /// The origin pane is itself a tmux pane.
    OriginIsTmuxPane,
    /// The server is older than tmux 3.2, or its version is unreadable.
    UnsupportedVersion { version: String },
}

impl RefusalReason {
    pub fn is_nested(&self) -> bool {
        matches!(self, Self::TmuxEnvSet | Self::OriginIsTmuxPane)
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TmuxEnvSet | Self::OriginIsTmuxPane => {
                f.write_str("Nested tmux is not supported natively")
            }
            Self::UnsupportedVersion { version } if version.is_empty() => {
                f.write_str("Native tmux needs tmux 3.2 or newer")
            }
            Self::UnsupportedVersion { version } => {
                write!(f, "Native tmux needs tmux 3.2 or newer (found {version})")
            }
        }
    }
}

/// Requests from the app to a running session, delivered through the event loop's channel as
/// [`crate::writeable_pty::Message::Tmux`]. Requests with no session running are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxRequest {
    /// Literal input for a pane (`send-keys -H`).
    PaneInput { pane_id: PaneId, bytes: Vec<u8> },
    /// A key evaluated against tmux's key tables (`send-keys -K`).
    ClientKey(ClientKey),
    /// One tmux command line; its reply arrives as [`GatewayEvent::CommandCompleted`] with `tag`.
    Command { tag: u64, line: String },
    /// A pane's queue drained below its low-water mark; resume and resync it.
    ResumePane(PaneId),
    /// Detach the control client (the final bare newline).
    Detach,
    /// The user accepted [`GatewayEvent::ConfirmPending`]: start the session.
    ConfirmOpen,
    /// The user declined [`GatewayEvent::ConfirmPending`]: the held bytes go to the origin model
    /// as plain output and the PTY stays the shell's. Nothing is written, since the stream may
    /// not be tmux; a real `tmux -CC` left running there ends with `detach` or Ctrl-C.
    ConfirmDismiss,
}

/// What the gateway reports to the app.
#[derive(Debug)]
pub enum GatewayEvent {
    /// Control-mode bytes appeared on an origin Warp did not just start tmux on. They are held
    /// and nothing is written until the app sends [`TmuxRequest::ConfirmOpen`] or
    /// [`TmuxRequest::ConfirmDismiss`].
    ConfirmPending,
    /// A [`Self::ConfirmPending`] entry is no longer waiting: dismissed, or its held bytes went
    /// to the origin model because there were too many or the setting turned off.
    ConfirmCleared,
    /// tmux entered control mode on this origin; the handshake is running.
    Starting,
    /// A control-mode entry stays raw (R6). For nested tmux nothing was diverted; for an old
    /// server the control client is being detached and the shell gets the PTY back.
    Refused(RefusalReason),
    /// An engine event. Pane bytes and command replies are delivered separately.
    Client(ClientEvent),
    /// The engine's session model after a batch of events that changed it. Sent before those
    /// events, so the app can build tabs from it when `Attached` arrives (the handshake emits no
    /// window events).
    Model(Box<SessionModel>),
    /// The first bytes for a pane arrived; read them from `queue`.
    PaneQueueOpened { queue: PaneQueueReceiver },
    /// The reply to a [`TmuxRequest::Command`].
    CommandCompleted { tag: u64, reply: CommandReply },
    /// The control stream ended (`%exit`, desync recovery, transport loss, or an attach that
    /// never completed) and the PTY belongs to the shell again. Every pane queue is closed.
    Ended,
}

/// Receives [`GatewayEvent`]s on the event loop thread. Must not block.
pub type GatewayEventSink = Box<dyn FnMut(GatewayEvent) + Send>;

/// Delivers [`TmuxRequest`]s to the gateway's event loop (from any thread).
pub type GatewayRequester = Arc<dyn Fn(TmuxRequest) + Send + Sync>;

/// Bounds for one pane's queue, in bytes (KTD12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneQueueLimits {
    /// Past this the pane is paused (`refresh-client -A '%N:pause'`).
    pub high_water: usize,
    /// A paused pane drained to this is resumed and resynced.
    pub low_water: usize,
    /// Chunks that would take the queue past this are dropped; the resync on resume repairs the
    /// pane.
    pub hard_cap: usize,
}

impl Default for PaneQueueLimits {
    fn default() -> Self {
        Self {
            high_water: 1 << 20,
            low_water: 256 << 10,
            hard_cap: 8 << 20,
        }
    }
}

/// One unit of pane content, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneChunk {
    /// Raw pane output for the pane's VT parser. Never scanned for control-mode entry.
    Output(Vec<u8>),
    /// The pane's content from tmux; replaces what the pane shows.
    Snapshot(PaneSnapshot),
}

impl PaneChunk {
    fn byte_len(&self) -> usize {
        match self {
            Self::Output(bytes) => bytes.len(),
            Self::Snapshot(snapshot) => snapshot.lines.iter().map(|line| line.len() + 2).sum(),
        }
    }
}

#[derive(Debug, Default)]
struct QueueShared {
    bytes: AtomicUsize,
    paused: AtomicBool,
}

/// What [`PaneQueueSender::push`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pushed {
    Queued,
    /// Queued, and the queue just went past its high-water mark: pause the pane.
    QueuedPause,
    /// Dropped at the hard cap. `pause` is true when the pane is not paused yet.
    Dropped {
        pause: bool,
    },
}

struct PaneQueueSender {
    tx: async_channel::Sender<PaneChunk>,
    shared: Arc<QueueShared>,
    limits: PaneQueueLimits,
}

impl PaneQueueSender {
    fn push(&self, chunk: PaneChunk) -> Pushed {
        let len = chunk.byte_len();
        let queued = self.shared.bytes.load(Ordering::Acquire);
        if queued.saturating_add(len) > self.limits.hard_cap {
            let pause = !self.shared.paused.swap(true, Ordering::AcqRel);
            return Pushed::Dropped { pause };
        }
        self.shared.bytes.fetch_add(len, Ordering::AcqRel);
        if self.tx.try_send(chunk).is_err() {
            // The reader is gone; nobody will drain this.
            self.shared.bytes.fetch_sub(len, Ordering::AcqRel);
            return Pushed::Queued;
        }
        if queued + len > self.limits.high_water && !self.shared.paused.swap(true, Ordering::AcqRel)
        {
            Pushed::QueuedPause
        } else {
            Pushed::Queued
        }
    }
}

/// The reading end of one pane's byte queue. Parse what it yields off the UI thread (U6).
///
/// Taking chunks below the low-water mark resumes a pane the gateway paused, which also resyncs
/// it with a fresh [`PaneChunk::Snapshot`]. The queue closes when the pane or session ends.
pub struct PaneQueueReceiver {
    pane_id: PaneId,
    rx: async_channel::Receiver<PaneChunk>,
    shared: Arc<QueueShared>,
    limits: PaneQueueLimits,
    requester: GatewayRequester,
}

impl fmt::Debug for PaneQueueReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaneQueueReceiver")
            .field("pane_id", &self.pane_id)
            .field("queued_bytes", &self.queued_bytes())
            .finish()
    }
}

impl PaneQueueReceiver {
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// Bytes waiting in the queue.
    pub fn queued_bytes(&self) -> usize {
        self.shared.bytes.load(Ordering::Acquire)
    }

    /// Whether the gateway has paused the pane because this queue backed up.
    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Acquire)
    }

    /// Waits for the next chunk. `None` once the queue is closed and empty.
    pub async fn recv(&self) -> Option<PaneChunk> {
        let chunk = self.rx.recv().await.ok()?;
        self.taken(&chunk);
        Some(chunk)
    }

    /// The next chunk, if one is waiting.
    pub fn try_recv(&self) -> Option<PaneChunk> {
        let chunk = self.rx.try_recv().ok()?;
        self.taken(&chunk);
        Some(chunk)
    }

    /// Whether the gateway has closed this queue.
    pub fn is_closed(&self) -> bool {
        self.rx.is_closed()
    }

    fn taken(&self, chunk: &PaneChunk) {
        let len = chunk.byte_len();
        let before = self.shared.bytes.fetch_sub(len, Ordering::AcqRel);
        if before.saturating_sub(len) <= self.limits.low_water
            && self.shared.paused.swap(false, Ordering::AcqRel)
            && !self.rx.is_closed()
        {
            (self.requester)(TmuxRequest::ResumePane(self.pane_id));
        }
    }
}

fn pane_queue(
    pane_id: PaneId,
    limits: PaneQueueLimits,
    requester: GatewayRequester,
) -> (PaneQueueSender, PaneQueueReceiver) {
    let (tx, rx) = async_channel::unbounded();
    let shared = Arc::new(QueueShared::default());
    (
        PaneQueueSender {
            tx,
            shared: shared.clone(),
            limits,
        },
        PaneQueueReceiver {
            pane_id,
            rx,
            shared,
            limits,
            requester,
        },
    )
}

/// The writing end of a queue made by [`test_pane_queue`].
#[cfg(any(test, feature = "test-util"))]
pub struct TestPaneQueueSender(PaneQueueSender);

#[cfg(any(test, feature = "test-util"))]
impl TestPaneQueueSender {
    /// Queues `chunk` as the gateway would. Dropping the sender closes the queue.
    pub fn push(&self, chunk: PaneChunk) {
        self.0.push(chunk);
    }
}

/// A pane queue with no gateway behind it, for testing pane backends. Resume requests are
/// dropped.
#[cfg(any(test, feature = "test-util"))]
pub fn test_pane_queue(pane_id: PaneId) -> (TestPaneQueueSender, PaneQueueReceiver) {
    let (sender, receiver) = pane_queue(pane_id, PaneQueueLimits::default(), Arc::new(|_| {}));
    (TestPaneQueueSender(sender), receiver)
}

/// Everything the event loop needs to run a gateway on one origin PTY.
pub struct GatewayLink {
    pub switch: GatewaySwitch,
    pub origin: OriginFlags,
    pub sink: GatewayEventSink,
    /// Feeds [`TmuxRequest`]s back into this PTY's event loop.
    pub requester: GatewayRequester,
    pub client_config: ClientConfig,
    pub queue_limits: PaneQueueLimits,
}

/// How [`Gateway::route`] split a read.
#[derive(Debug, PartialEq, Eq)]
pub enum Routed {
    /// The whole read belongs to the origin model, unchanged.
    Shell,
    /// Only these bytes belong to the origin model (possibly none).
    Split(Vec<u8>),
}

struct Session {
    client: ControlClient,
    queues: HashMap<PaneId, PaneQueueSender>,
    tags: HashMap<CommandId, u64>,
    /// The server was refused; its `AttachFailed` on `%exit` is expected and not reported.
    refused: bool,
    started_at: Duration,
    /// Every byte of the stream, from the DCS, until the attach is confirmed. If the attach never
    /// completes, the stream was not a working tmux and these go to the origin model.
    prelude: Option<Vec<u8>>,
    /// When the client disconnected (desync, lost control) while tmux may still be running.
    disconnected_at: Option<Duration>,
}

enum State {
    /// Shell bytes. `matched` DCS bytes at the end of earlier reads already went to the model.
    Seeking {
        matched: usize,
    },
    /// An unarmed entry waits for the app; `held` is the stream from the DCS on.
    Confirming {
        held: Vec<u8>,
    },
    Session(Box<Session>),
}

/// The control-mode gateway for one origin PTY. Runs on that PTY's event loop thread.
pub struct Gateway {
    switch: GatewaySwitch,
    origin: OriginFlags,
    sink: GatewayEventSink,
    requester: GatewayRequester,
    client_config: ClientConfig,
    queue_limits: PaneQueueLimits,
    clock: Instant,
    state: State,
    /// Bytes for the origin model produced outside [`Gateway::route`]; see
    /// [`Gateway::take_shell_bytes`].
    shell_out: Vec<u8>,
}

impl Gateway {
    pub fn new(link: GatewayLink) -> Self {
        Self {
            switch: link.switch,
            origin: link.origin,
            sink: link.sink,
            requester: link.requester,
            client_config: link.client_config,
            queue_limits: link.queue_limits,
            clock: Instant::now(),
            state: State::Seeking { matched: 0 },
            shell_out: Vec::new(),
        }
    }

    /// Whether reads must go through [`Self::route`]: the switch is on, or an entry that started
    /// while it was on is still held or running.
    #[inline]
    pub fn engaged(&self) -> bool {
        self.switch.is_enabled() || !matches!(self.state, State::Seeking { .. })
    }

    /// Bytes for the origin model that a request or a timer released. Parse them before the next
    /// read.
    pub fn take_shell_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shell_out)
    }

    pub fn in_session(&self) -> bool {
        matches!(self.state, State::Session(_))
    }

    /// Splits one raw PTY read. Command lines for the PTY are appended to `writes`.
    pub fn route(&mut self, input: &[u8], writes: &mut VecDeque<Cow<'static, [u8]>>) -> Routed {
        match &mut self.state {
            State::Seeking { matched } => {
                if !self.switch.is_enabled() {
                    *matched = 0;
                    return Routed::Shell;
                }
                let Some(entry) = find_entry(matched, input) else {
                    return Routed::Shell;
                };
                if let Some(reason) = self.origin.nested_reason() {
                    log::info!("tmux: control mode on a nested origin stays raw: {reason}");
                    (self.sink)(GatewayEvent::Refused(reason));
                    return Routed::Shell;
                }
                let mut model = input[..entry.start].to_vec();
                if entry.carried {
                    model.push(CANCEL);
                }
                let mut control = CONTROL_MODE_DCS.to_vec();
                control.extend_from_slice(&input[entry.end..]);
                if self.origin.take_armed() {
                    self.start_session(&control, &mut model, writes);
                } else {
                    log::info!("tmux: control mode on an origin Warp did not start; asking first");
                    self.state = State::Confirming { held: control };
                    (self.sink)(GatewayEvent::ConfirmPending);
                    self.check_held();
                    model.append(&mut self.shell_out);
                }
                Routed::Split(model)
            }
            State::Confirming { held } => {
                held.extend_from_slice(input);
                self.check_held();
                Routed::Split(std::mem::take(&mut self.shell_out))
            }
            State::Session(_) => {
                let mut model = Vec::new();
                self.feed_session(input, &mut model, writes);
                Routed::Split(model)
            }
        }
    }

    /// Releases held bytes to the origin model when there are too many or the setting is off.
    fn check_held(&mut self) {
        let State::Confirming { held } = &self.state else {
            return;
        };
        if held.len() > MAX_HELD_BYTES || !self.switch.is_enabled() {
            log::info!("tmux: no answer for a held control-mode entry; showing it as output");
            self.release_held();
        }
    }

    fn release_held(&mut self) {
        if let Some(held) = self.take_held() {
            self.shell_out.extend_from_slice(&held);
            (self.sink)(GatewayEvent::ConfirmCleared);
        }
    }

    /// Leaves [`State::Confirming`], returning what it held.
    fn take_held(&mut self) -> Option<Vec<u8>> {
        let State::Confirming { held } = &mut self.state else {
            return None;
        };
        let held = std::mem::take(held);
        self.state = State::Seeking { matched: 0 };
        Some(held)
    }

    fn start_session(
        &mut self,
        control: &[u8],
        model: &mut Vec<u8>,
        writes: &mut VecDeque<Cow<'static, [u8]>>,
    ) {
        log::info!("tmux: control mode entered; starting the gateway session");
        self.state = State::Session(Box::new(Session {
            client: ControlClient::new(self.client_config.clone()),
            queues: HashMap::new(),
            tags: HashMap::new(),
            refused: false,
            started_at: self.now(),
            prelude: Some(Vec::new()),
            disconnected_at: None,
        }));
        (self.sink)(GatewayEvent::Starting);
        self.feed_session(control, model, writes);
    }

    /// Applies a request from the app.
    pub fn request(&mut self, request: TmuxRequest, writes: &mut VecDeque<Cow<'static, [u8]>>) {
        let now = self.now();
        match request {
            TmuxRequest::ConfirmOpen => {
                if let Some(held) = self.take_held() {
                    (self.sink)(GatewayEvent::ConfirmCleared);
                    let mut model = Vec::new();
                    self.start_session(&held, &mut model, writes);
                    self.shell_out.append(&mut model);
                }
                return;
            }
            TmuxRequest::ConfirmDismiss => {
                self.release_held();
                return;
            }
            TmuxRequest::PaneInput { .. }
            | TmuxRequest::ClientKey(_)
            | TmuxRequest::Command { .. }
            | TmuxRequest::ResumePane(_)
            | TmuxRequest::Detach => {}
        }
        let State::Session(session) = &mut self.state else {
            log::debug!("tmux: dropping {request:?} with no session running");
            return;
        };
        let client = &mut session.client;
        let result = match request {
            TmuxRequest::PaneInput { pane_id, bytes } => {
                client.send_pane_input(pane_id, &bytes, now)
            }
            TmuxRequest::ClientKey(key) => client.send_client_key(key, now),
            TmuxRequest::Command { tag, line } => client.send_command(&line, now).map(|id| {
                session.tags.insert(id, tag);
            }),
            TmuxRequest::ResumePane(pane_id) => {
                client.resume_pane(pane_id, now);
                Ok(())
            }
            TmuxRequest::Detach => {
                client.detach();
                Ok(())
            }
            TmuxRequest::ConfirmOpen | TmuxRequest::ConfirmDismiss => Ok(()),
        };
        if let Err(error) = result {
            log::warn!("tmux: request rejected: {error}");
        }
        self.pump(writes);
    }

    /// Time until [`Self::tick`] has work, if a session is running.
    pub fn next_timeout(&self) -> Option<Duration> {
        let State::Session(session) = &self.state else {
            return None;
        };
        let deadline = [
            session.client.next_deadline(),
            session
                .prelude
                .as_ref()
                .map(|_| session.started_at + HANDSHAKE_DEADLINE),
            session.disconnected_at.map(|at| at + DISCONNECT_DEADLINE),
        ]
        .into_iter()
        .flatten()
        .min()?;
        Some(deadline.saturating_sub(self.now()))
    }

    /// Runs the engine's timers, and gives the PTY back to the shell when an attach never
    /// completed or a disconnected tmux never exited.
    pub fn tick(&mut self, writes: &mut VecDeque<Cow<'static, [u8]>>) {
        let now = self.now();
        let State::Session(session) = &mut self.state else {
            return;
        };
        session.client.tick(now);
        self.pump(writes);
        let State::Session(session) = &mut self.state else {
            return;
        };
        if session.prelude.is_some() && now >= session.started_at + HANDSHAKE_DEADLINE {
            log::info!("tmux: the attach did not complete in time; showing the stream as output");
            self.abandon();
        } else if session
            .disconnected_at
            .is_some_and(|at| now >= at + DISCONNECT_DEADLINE)
        {
            log::info!("tmux: a disconnected session did not exit; the shell gets the PTY back");
            self.end_session();
        }
    }

    /// Ends a session whose attach never completed: everything it read goes to the origin model.
    fn abandon(&mut self) {
        if let State::Session(session) = &mut self.state
            && let Some(prelude) = session.prelude.take()
        {
            self.shell_out.extend_from_slice(&prelude);
        }
        self.end_session();
    }

    /// The PTY hit EOF, failed, or its child exited (KTD13). Ends any session.
    pub fn transport_closed(&mut self) {
        self.release_held();
        let State::Session(session) = &mut self.state else {
            return;
        };
        if let Some(prelude) = session.prelude.take() {
            self.shell_out.extend_from_slice(&prelude);
        }
        session.client.transport_closed();
        let mut writes = VecDeque::new();
        self.pump(&mut writes);
        self.end_session();
    }

    fn now(&self) -> Duration {
        self.clock.elapsed()
    }

    /// Moves the gateway's clock forward, for deadline tests.
    #[cfg(test)]
    fn advance_clock(&mut self, by: Duration) {
        self.clock = self.clock.checked_sub(by).expect("clock in range");
    }

    fn feed_session(
        &mut self,
        input: &[u8],
        model: &mut Vec<u8>,
        writes: &mut VecDeque<Cow<'static, [u8]>>,
    ) {
        let now = self.now();
        let State::Session(session) = &mut self.state else {
            return;
        };
        if let Some(prelude) = &mut session.prelude {
            prelude.extend_from_slice(input);
        }
        let shell = session.client.receive_bytes(input, now);
        if session.client.attach_confirmed() {
            session.prelude = None;
        }
        let abandoned = session.prelude.as_ref().is_some_and(|prelude| {
            prelude.len() > MAX_HELD_BYTES || session.client.transport_lost()
        });
        if abandoned {
            log::info!("tmux: the control stream was not a working tmux; showing it as output");
            let State::Session(session) = &mut self.state else {
                return;
            };
            model.extend(session.prelude.take().unwrap_or_default());
            self.end_session();
            return;
        }
        model.extend(shell);
        self.pump(writes);
        let State::Session(session) = &mut self.state else {
            return;
        };
        if session.disconnected_at.is_none() && session.client.phase() == Phase::Disconnected {
            session.disconnected_at = Some(now);
        }
        if let Some(pending) = session.client.shell_pending() {
            // `pending` is a partial DCS the parser held back; it goes to the model now and the
            // detector carries on from it.
            let pending = pending.to_vec();
            model.extend_from_slice(&pending);
            self.end_session();
            self.state = State::Seeking {
                matched: pending.len(),
            };
        }
    }

    /// Moves the engine's output to `writes`, pane bytes to their queues, and events to the sink.
    fn pump(&mut self, writes: &mut VecDeque<Cow<'static, [u8]>>) {
        let now = self.now();
        let State::Session(session) = &mut self.state else {
            return;
        };
        let mut prune = false;
        let events = session.client.take_events();
        if events.iter().any(changes_model) {
            (self.sink)(GatewayEvent::Model(Box::new(
                session.client.model().clone(),
            )));
        }
        for event in events {
            match event {
                ClientEvent::PaneOutput { pane_id, bytes } => {
                    Self::enqueue(
                        session,
                        &mut self.sink,
                        &self.requester,
                        self.queue_limits,
                        pane_id,
                        PaneChunk::Output(bytes),
                        now,
                    );
                }
                ClientEvent::PaneSnapshot(snapshot) => {
                    let pane_id = snapshot.pane_id;
                    Self::enqueue(
                        session,
                        &mut self.sink,
                        &self.requester,
                        self.queue_limits,
                        pane_id,
                        PaneChunk::Snapshot(snapshot),
                        now,
                    );
                }
                ClientEvent::UnsupportedVersion { version } => {
                    log::info!("tmux: server version {version:?} is unsupported; detaching");
                    session.refused = true;
                    session.client.detach();
                    (self.sink)(GatewayEvent::Refused(RefusalReason::UnsupportedVersion {
                        version,
                    }));
                }
                ClientEvent::AttachFailed { .. } if session.refused => {}
                ClientEvent::CommandCompleted { id, reply } => {
                    if let Some(tag) = session.tags.remove(&id) {
                        (self.sink)(GatewayEvent::CommandCompleted { tag, reply });
                    }
                }
                event => {
                    prune |= matches!(
                        event,
                        ClientEvent::WindowClosed { .. }
                            | ClientEvent::LayoutChanged { .. }
                            | ClientEvent::SessionChanged { .. }
                    );
                    if matches!(
                        event,
                        ClientEvent::Exited { .. }
                            | ClientEvent::AttachFailed { .. }
                            | ClientEvent::TransportLost
                            | ClientEvent::Desync { .. }
                    ) {
                        log::info!("tmux: {event:?}");
                    }
                    (self.sink)(GatewayEvent::Client(event));
                }
            }
        }
        if prune {
            let model = session.client.model();
            session
                .queues
                .retain(|pane_id, _| model.pane(*pane_id).is_some());
        }
        if matches!(session.client.phase(), Phase::Exited | Phase::Disconnected) {
            // Nothing more will be delivered to panes; close their queues now.
            session.queues.clear();
        }
        writes.extend(
            session
                .client
                .take_outgoing()
                .into_iter()
                .map(|line| Cow::Owned(line.into_bytes())),
        );
    }

    fn enqueue(
        session: &mut Session,
        sink: &mut GatewayEventSink,
        requester: &GatewayRequester,
        limits: PaneQueueLimits,
        pane_id: PaneId,
        chunk: PaneChunk,
        now: Duration,
    ) {
        let queue = session.queues.entry(pane_id).or_insert_with(|| {
            let (sender, queue) = pane_queue(pane_id, limits, requester.clone());
            sink(GatewayEvent::PaneQueueOpened { queue });
            sender
        });
        match queue.push(chunk) {
            Pushed::Queued | Pushed::Dropped { pause: false } => {}
            Pushed::QueuedPause | Pushed::Dropped { pause: true } => {
                log::debug!("tmux: pane {pane_id} queue is backed up; pausing it");
                session.client.pause_pane(pane_id, now);
            }
        }
    }

    fn end_session(&mut self) {
        if let State::Session(_) = std::mem::replace(&mut self.state, State::Seeking { matched: 0 })
        {
            log::info!("tmux: control stream ended; the PTY belongs to the shell again");
            (self.sink)(GatewayEvent::Ended);
        }
    }
}

/// Whether `event` reports a change to the session model the app mirrors.
fn changes_model(event: &ClientEvent) -> bool {
    matches!(
        event,
        ClientEvent::Attached { .. }
            | ClientEvent::SessionChanged { .. }
            | ClientEvent::SessionRenamed { .. }
            | ClientEvent::WindowAdded { .. }
            | ClientEvent::WindowClosed { .. }
            | ClientEvent::WindowRenamed { .. }
            | ClientEvent::LayoutChanged { .. }
            | ClientEvent::ActiveWindowChanged { .. }
            | ClientEvent::ActivePaneChanged { .. }
            | ClientEvent::PaneModeChanged { .. }
    )
}

/// Where the entry DCS sits in a read.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// Index of the DCS's first byte in this read (0 when it began in an earlier read).
    start: usize,
    /// Index just past the DCS.
    end: usize,
    /// Part of the DCS was in earlier reads, which the model already received.
    carried: bool,
}

/// Finds [`CONTROL_MODE_DCS`] in `input`, continuing a partial match of `matched` bytes from
/// earlier reads. Bytes are never held back: a partial match at the end of a read is recorded in
/// `matched` and the read still goes to the model.
fn find_entry(matched: &mut usize, input: &[u8]) -> Option<Entry> {
    let dcs = CONTROL_MODE_DCS;
    let mut index = 0;
    if *matched > 0 {
        let carried = *matched;
        let mut m = carried;
        while index < input.len() && m < dcs.len() && input[index] == dcs[m] {
            index += 1;
            m += 1;
        }
        if m == dcs.len() {
            *matched = 0;
            return Some(Entry {
                start: 0,
                end: index,
                carried: true,
            });
        }
        if index == input.len() {
            *matched = m;
            return None;
        }
        *matched = 0;
    }
    while let Some(offset) = input[index..].iter().position(|&b| b == dcs[0]) {
        let start = index + offset;
        let mut end = start;
        let mut m = 0;
        while end < input.len() && m < dcs.len() && input[end] == dcs[m] {
            end += 1;
            m += 1;
        }
        if m == dcs.len() {
            return Some(Entry {
                start,
                end,
                carried: false,
            });
        }
        if end == input.len() {
            *matched = m;
            return None;
        }
        index = start + 1;
    }
    None
}

#[cfg(test)]
#[path = "tmux_gateway_tests.rs"]
mod tests;
