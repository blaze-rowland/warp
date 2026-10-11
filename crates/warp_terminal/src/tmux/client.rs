//! Transport-agnostic tmux control-mode client engine.
//!
//! [`ControlClient`] consumes control-mode bytes (or already-parsed [`ControlEvent`]s), writes
//! command lines to an outgoing queue, keeps the [`SessionModel`] current, and reports what the
//! app must do as [`ClientEvent`]s. It performs no I/O and reads no clock: the caller supplies
//! `now` as monotonic time since any fixed origin, and calls [`ControlClient::tick`] by
//! [`ControlClient::next_deadline`].
//!
//! Replies are matched to commands by the command number in `%begin`, not by counting `%end`s:
//! tmux runs this client's commands in order and flags their blocks with `1`, so each flagged
//! `%begin` binds the oldest unanswered command to its number, and the `%end`/`%error` must
//! carry that number. Anything else is a desync.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use super::encode::send_keys_command;
use super::layout::{WindowLayout, parse_layout_change};
use super::model::{
    LIST_PANES_FORMAT, LIST_WINDOWS_FORMAT, SessionModel, WindowChange, parse_pane_row,
    parse_window_row,
};
use super::parser::{
    CommandReply, ControlEvent, ControlModeParser, DecodeItem, PaneId, SessionId, WindowId,
};
use super::version::TmuxVersion;

/// `list-keys -F` format (tmux 3.4+) matching [`KeyBinding`].
pub const LIST_KEYS_FORMAT: &str = "#{key_table}\t#{key_repeat}\t#{key_string}\t#{key_command}";

const VERSION_QUERY: &str = "display-message -p '#{version}\t#{client_name}'";

/// `%begin` flag tmux sets on replies to commands this client sent.
const FROM_THIS_CLIENT: u32 = 1;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// tmux pauses a pane whose output is this far behind (`pause-after`, whole seconds).
    pub pause_after: Duration,
    /// Scrollback lines requested when seeding a pane.
    pub history_lines: u32,
    /// How long a command may go unanswered before [`ClientEvent::NotResponding`].
    pub command_timeout: Duration,
    /// `send-keys -K` commands allowed in flight; later keys wait client-side.
    pub max_unacknowledged_client_keys: usize,
    /// A queued navigation key older than this is dropped (see [`KeyClass::Navigation`]).
    pub stale_navigation_key_age: Duration,
    /// Output held per pane while its snapshot is in flight. Past this the pane is resynced.
    pub max_buffered_output_bytes: usize,
    /// A capture larger than this is retaken without history.
    pub max_snapshot_bytes: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            pause_after: Duration::from_secs(5),
            history_lines: 2000,
            command_timeout: Duration::from_secs(3),
            max_unacknowledged_client_keys: 4,
            stale_navigation_key_age: Duration::from_secs(1),
            max_buffered_output_bytes: 1 << 20,
            // Below the parser's 1 MiB reply cap, so an oversized capture is retaken instead of
            // overflowing the protocol.
            max_snapshot_bytes: 768 * 1024,
        }
    }
}

/// Identifies a command sent with [`ControlClient::send_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandId(u64);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("the control client is not accepting commands")]
    Closed,
    #[error("a command must be one non-empty line")]
    NotOneLine,
    #[error("tmux is older than 3.4, which added send-keys -K")]
    ClientKeysUnsupported,
}

/// How a key that goes through tmux's key tables may be treated when the channel is slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyClass {
    /// Bound to pane or window navigation. Safe to drop when stale (R20).
    Navigation,
    Other,
}

/// A key for `send-keys -K`, evaluated by tmux against the attached client's key tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientKey {
    /// tmux key name, e.g. `C-b` or `Up`.
    pub key: String,
    /// The focused pane when the key was pressed.
    pub pane_id: PaneId,
    pub class: KeyClass,
    /// False when the client may be in a non-root key table. Such keys are never dropped.
    pub in_root_table: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// First content for a pane that existed before attach.
    Seed,
    /// Replaces the pane's content after output was lost (pause, overflow).
    Resync,
    /// The visible alternate screen of a full-screen app, with no history.
    Repaint,
}

/// A pane's content captured from tmux, followed in order by any output that arrived after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub pane_id: PaneId,
    pub kind: SnapshotKind,
    /// `capture-pane -e` lines: history first, the visible screen last.
    pub lines: Vec<Vec<u8>>,
    /// Zero-based `(x, y)` on the visible screen.
    pub cursor: Option<(u32, u32)>,
}

impl PaneSnapshot {
    /// Bytes that reproduce the snapshot when fed to a terminal the pane's size.
    pub fn to_vt_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if self.kind == SnapshotKind::Repaint {
            bytes.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        }
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                bytes.extend_from_slice(b"\r\n");
            }
            bytes.extend_from_slice(line);
        }
        if let Some((x, y)) = self.cursor {
            bytes.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        }
        bytes
    }
}

/// One row of a [`LIST_KEYS_FORMAT`] reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub table: String,
    pub repeat: bool,
    pub key: String,
    pub command: String,
}

/// The attached server's key configuration, as read at attach.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyConfig {
    pub prefix: Option<String>,
    pub prefix2: Option<String>,
    /// `None` below tmux 3.4, which cannot list keys with formats.
    pub bindings: Option<Vec<KeyBinding>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// The handshake finished; [`ControlClient::model`] holds the session.
    Attached {
        session_id: SessionId,
        session_name: String,
    },
    /// The server is older than the native floor or its version is unreadable (R6).
    UnsupportedVersion {
        version: String,
    },
    KeyConfigLoaded(KeyConfig),
    /// The client switched sessions; the model was reloaded and panes are being reseeded.
    SessionChanged {
        session_id: SessionId,
        name: String,
    },
    SessionRenamed {
        name: String,
    },
    WindowAdded {
        window_id: WindowId,
    },
    WindowClosed {
        window_id: WindowId,
    },
    WindowRenamed {
        window_id: WindowId,
        name: String,
    },
    LayoutChanged {
        window_id: WindowId,
        layout: WindowLayout,
    },
    ActiveWindowChanged {
        window_id: WindowId,
    },
    ActivePaneChanged {
        window_id: WindowId,
        pane_id: PaneId,
    },
    PaneSnapshot(PaneSnapshot),
    PaneOutput {
        pane_id: PaneId,
        bytes: Vec<u8>,
    },
    PaneModeChanged {
        pane_id: PaneId,
        mode: Option<String>,
    },
    Message {
        text: String,
    },
    ConfigError {
        text: String,
    },
    CommandCompleted {
        id: CommandId,
        reply: CommandReply,
    },
    /// A stale navigation key was dropped instead of sent.
    ClientKeyDropped {
        key: ClientKey,
    },
    /// The oldest outstanding command has gone unanswered past the timeout.
    NotResponding {
        command: String,
    },
    /// Replies are flowing again after [`ClientEvent::NotResponding`].
    Responding,
    Exited {
        reason: Option<String>,
    },
    /// tmux exited before attaching to a session.
    AttachFailed {
        error: Option<String>,
    },
    /// The transport ended without `%exit`.
    TransportLost,
    /// The reply stream no longer matches what was sent; pane output is no longer delivered.
    Desync {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for tmux to enter control mode.
    Connecting,
    Handshaking,
    Attached,
    /// tmux is too old for native sessions; only `%exit` and detach are handled.
    Unsupported,
    Exited,
    /// Desynced or the transport was lost.
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Handshake,
    /// After switching sessions: replace everything and reseed.
    Reload,
    /// After window adds or an unparseable layout: report differences.
    Refresh,
}

#[derive(Debug)]
enum Purpose {
    Version,
    ClientFlags,
    ListWindows(ListKind),
    ListPanes(ListKind),
    Prefix,
    Prefix2,
    ListKeys,
    Capture { pane_id: PaneId, generation: u64 },
    Cursor { pane_id: PaneId, generation: u64 },
    PaneMode(PaneId),
    PaneFlow,
    ClientKey,
    PaneInput,
    User(CommandId),
}

#[derive(Debug)]
struct Sent {
    purpose: Purpose,
    line: String,
    sent_at: Duration,
    /// Bound when this command's `%begin` arrives.
    number: Option<u64>,
}

/// A snapshot in flight for one pane.
///
/// tmux writes pane output it read before running `capture-pane` ahead of the reply, so that
/// output is already in the capture and is dropped. Output after the capture's `%begin` is newer
/// and is held until the snapshot is delivered.
struct PaneSync {
    generation: u64,
    kind: SnapshotKind,
    with_history: bool,
    begun: bool,
    lines: Option<Vec<Vec<u8>>>,
    held: Vec<Vec<u8>>,
    held_bytes: usize,
    overflowed: bool,
}

enum QueuedInput {
    Key { key: ClientKey, queued_at: Duration },
    Pane(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowList {
    Idle,
    Queued,
    Running,
}

pub struct ControlClient {
    config: ClientConfig,
    parser: ControlModeParser,
    phase: Phase,
    model: SessionModel,
    version: Option<TmuxVersion>,
    client_name: Option<String>,
    key_config: KeyConfig,
    handshake_remaining: usize,
    /// Pane output is dropped until the pane list is in: every listed pane is captured next.
    panes_listed: bool,
    wait_exit: bool,
    /// Set once the final bare newline is queued; nothing is written after it.
    closed: bool,
    transport_lost: bool,
    last_error: Option<String>,
    sent: VecDeque<Sent>,
    last_number: Option<u64>,
    syncs: HashMap<PaneId, PaneSync>,
    next_generation: u64,
    paused_by_app: HashSet<PaneId>,
    window_list: WindowList,
    refresh_again: bool,
    pending_session_change: Option<(SessionId, String)>,
    input_queue: VecDeque<QueuedInput>,
    unacknowledged_keys: usize,
    not_responding: bool,
    next_command_id: u64,
    outgoing: Vec<String>,
    events: Vec<ClientEvent>,
}

impl ControlClient {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            parser: ControlModeParser::new(),
            phase: Phase::Connecting,
            model: SessionModel::default(),
            version: None,
            client_name: None,
            key_config: KeyConfig::default(),
            handshake_remaining: 0,
            panes_listed: false,
            wait_exit: false,
            closed: false,
            transport_lost: false,
            last_error: None,
            sent: VecDeque::new(),
            last_number: None,
            syncs: HashMap::new(),
            next_generation: 0,
            paused_by_app: HashSet::new(),
            window_list: WindowList::Idle,
            refresh_again: false,
            pending_session_change: None,
            input_queue: VecDeque::new(),
            unacknowledged_keys: 0,
            not_responding: false,
            next_command_id: 0,
            outgoing: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn model(&self) -> &SessionModel {
        &self.model
    }

    pub fn version(&self) -> Option<&TmuxVersion> {
        self.version.as_ref()
    }

    /// The control client's tmux name, used as `-c` for client-scoped commands.
    pub fn client_name(&self) -> Option<&str> {
        self.client_name.as_deref()
    }

    pub fn key_config(&self) -> &KeyConfig {
        &self.key_config
    }

    /// Parses a transport read. Returns bytes outside control mode (before the DCS and after
    /// `%exit`), which belong to the shell.
    pub fn receive_bytes(&mut self, bytes: &[u8], now: Duration) -> Vec<u8> {
        let mut shell = Vec::new();
        for item in self.parser.decode(bytes) {
            match item {
                DecodeItem::Shell(bytes) => shell.extend(bytes),
                DecodeItem::Control(event) => self.handle_event(event, now),
            }
        }
        shell
    }

    /// Applies one parsed event, for callers that run their own [`ControlModeParser`].
    pub fn handle_event(&mut self, event: ControlEvent, now: Duration) {
        match event {
            ControlEvent::EnteredControlMode => {
                if self.phase == Phase::Connecting {
                    self.phase = Phase::Handshaking;
                    self.write(VERSION_QUERY.to_owned(), Purpose::Version, now);
                }
                return;
            }
            ControlEvent::Exit { reason } => {
                self.on_exit(reason);
                return;
            }
            ControlEvent::ProtocolOverflow => {
                self.desync("a control line or reply exceeded the parser's bounds".into());
                return;
            }
            _ => {}
        }
        if !matches!(self.phase, Phase::Handshaking | Phase::Attached) {
            return;
        }
        match event {
            ControlEvent::CommandBegin { number, flags, .. } => self.on_begin(number, flags),
            ControlEvent::CommandEnd(reply) => self.on_end(reply, now),
            ControlEvent::PaneOutput { pane_id, bytes, .. } => self.on_output(pane_id, bytes),
            ControlEvent::Pause { pane_id } => self.on_pause(pane_id, now),
            ControlEvent::Continue { pane_id } => self.on_continue(pane_id, now),
            ControlEvent::PaneModeChanged { pane_id } => self.write(
                format!("display-message -p -t {pane_id} '#{{pane_mode}}'"),
                Purpose::PaneMode(pane_id),
                now,
            ),
            ControlEvent::LayoutChange {
                window_id,
                layout,
                visible_layout,
                flags,
            } => match parse_layout_change(&layout, visible_layout.as_deref(), &flags) {
                Ok(layout) => self.on_layout(window_id, layout),
                Err(error) => {
                    log::warn!("tmux: unparseable layout for {window_id}: {error}");
                    self.request_window_refresh(now);
                }
            },
            ControlEvent::WindowAdd { window_id } => {
                if self.model.add_window(window_id) {
                    self.emit_attached(ClientEvent::WindowAdded { window_id });
                }
                self.request_window_refresh(now);
            }
            ControlEvent::WindowClose { window_id } => {
                if let Some(window) = self.model.close_window(window_id) {
                    if let Some(layout) = window.layout {
                        for pane_id in layout.layout.pane_ids() {
                            self.syncs.remove(&pane_id);
                        }
                    }
                    self.emit_attached(ClientEvent::WindowClosed { window_id });
                }
            }
            ControlEvent::WindowRenamed { window_id, name } => {
                if self.model.rename_window(window_id, name.clone()) {
                    self.emit_attached(ClientEvent::WindowRenamed { window_id, name });
                }
            }
            ControlEvent::WindowPaneChanged { window_id, pane_id } => {
                self.model.set_active_pane(window_id, pane_id);
                self.emit_attached(ClientEvent::ActivePaneChanged { window_id, pane_id });
            }
            ControlEvent::SessionChanged { session_id, name } => {
                self.on_session_changed(session_id, name, now)
            }
            ControlEvent::SessionRenamed { session_id, name } => {
                if self.model.rename_session(session_id, name.clone()) {
                    self.emit_attached(ClientEvent::SessionRenamed { name });
                }
            }
            ControlEvent::SessionWindowChanged {
                session_id,
                window_id,
            } => {
                if self.model.session().is_some_and(|s| s.id == session_id) {
                    self.model.set_active_window(window_id);
                    self.emit_attached(ClientEvent::ActiveWindowChanged { window_id });
                }
            }
            ControlEvent::Message { message } => {
                self.events.push(ClientEvent::Message { text: message })
            }
            ControlEvent::ConfigError { message } => {
                self.events.push(ClientEvent::ConfigError { text: message })
            }
            ControlEvent::EnteredControlMode
            | ControlEvent::Exit { .. }
            | ControlEvent::ProtocolOverflow
            | ControlEvent::UnlinkedWindowAdd { .. }
            | ControlEvent::UnlinkedWindowClose { .. }
            | ControlEvent::UnlinkedWindowRenamed { .. }
            | ControlEvent::ClientSessionChanged { .. }
            | ControlEvent::SessionsChanged
            | ControlEvent::SubscriptionChanged { .. }
            | ControlEvent::ClientDetached { .. }
            | ControlEvent::PasteBufferChanged { .. }
            | ControlEvent::PasteBufferDeleted { .. } => {}
        }
    }

    /// The transport hit EOF or failed.
    pub fn transport_closed(&mut self) {
        self.transport_lost = true;
        if matches!(self.phase, Phase::Exited | Phase::Disconnected) {
            return;
        }
        self.phase = Phase::Disconnected;
        self.reset_flow();
        self.events.push(ClientEvent::TransportLost);
    }

    /// Checks the command timeout.
    pub fn tick(&mut self, now: Duration) {
        if self.not_responding {
            return;
        }
        if let Some(oldest) = self.sent.front()
            && now >= oldest.sent_at + self.config.command_timeout
        {
            self.not_responding = true;
            let command = oldest.line.clone();
            self.events.push(ClientEvent::NotResponding { command });
        }
    }

    /// When [`Self::tick`] next has work to do.
    pub fn next_deadline(&self) -> Option<Duration> {
        if self.not_responding {
            return None;
        }
        self.sent
            .front()
            .map(|oldest| oldest.sent_at + self.config.command_timeout)
    }

    /// Command lines to write, in order. Each ends in exactly one `\n`.
    pub fn take_outgoing(&mut self) -> Vec<String> {
        std::mem::take(&mut self.outgoing)
    }

    pub fn take_events(&mut self) -> Vec<ClientEvent> {
        std::mem::take(&mut self.events)
    }

    /// Sends one tmux command line; its reply arrives as [`ClientEvent::CommandCompleted`].
    pub fn send_command(&mut self, line: &str, now: Duration) -> Result<CommandId, CommandError> {
        self.check_open()?;
        check_one_line(line)?;
        let id = CommandId(self.next_command_id);
        self.next_command_id += 1;
        self.write(line.to_owned(), Purpose::User(id), now);
        Ok(id)
    }

    /// Sends literal input to a pane, in order behind any queued keys.
    pub fn send_pane_input(
        &mut self,
        pane_id: PaneId,
        bytes: &[u8],
        now: Duration,
    ) -> Result<(), CommandError> {
        self.check_open()?;
        let lines = send_keys_command(pane_id, bytes)
            .into_iter()
            .map(|line| line.trim_end_matches('\n').to_owned())
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            self.input_queue.push_back(QueuedInput::Pane(lines));
            self.pump_input(now);
        }
        Ok(())
    }

    /// Sends a key through tmux's key tables (`send-keys -K`, tmux 3.4+), subject to the
    /// unacknowledged-key cap.
    pub fn send_client_key(&mut self, key: ClientKey, now: Duration) -> Result<(), CommandError> {
        self.check_open()?;
        if !self
            .version
            .as_ref()
            .is_some_and(TmuxVersion::supports_client_key_commands)
            || self.client_name.is_none()
        {
            return Err(CommandError::ClientKeysUnsupported);
        }
        check_one_line(&key.key)?;
        self.input_queue.push_back(QueuedInput::Key {
            key,
            queued_at: now,
        });
        self.pump_input(now);
        Ok(())
    }

    /// Asks tmux to stop sending a pane's output (a backlogged pane queue, KTD12).
    pub fn pause_pane(&mut self, pane_id: PaneId, now: Duration) {
        if self.check_open().is_err() {
            return;
        }
        self.paused_by_app.insert(pane_id);
        self.write(
            format!("refresh-client -A '{pane_id}:pause'"),
            Purpose::PaneFlow,
            now,
        );
    }

    /// Resumes a paused pane and resyncs its content.
    pub fn resume_pane(&mut self, pane_id: PaneId, now: Duration) {
        if self.check_open().is_err() {
            return;
        }
        self.paused_by_app.remove(&pane_id);
        self.continue_and_resync(pane_id, now);
    }

    /// Detaches by writing the final bare newline. Nothing is written after it.
    pub fn detach(&mut self) {
        if self.closed
            || self.transport_lost
            || matches!(self.phase, Phase::Connecting | Phase::Exited)
        {
            return;
        }
        self.closed = true;
        self.outgoing.push("\n".to_owned());
    }

    /// `send-keys -K` commands sent and not yet answered.
    pub fn unacknowledged_client_keys(&self) -> usize {
        self.unacknowledged_keys
    }

    /// Keys and pane input waiting behind the unacknowledged-key cap.
    pub fn queued_input(&self) -> usize {
        self.input_queue.len()
    }

    fn check_open(&self) -> Result<(), CommandError> {
        if self.closed || !matches!(self.phase, Phase::Handshaking | Phase::Attached) {
            Err(CommandError::Closed)
        } else {
            Ok(())
        }
    }

    fn write(&mut self, line: String, purpose: Purpose, now: Duration) {
        debug_assert!(check_one_line(&line).is_ok(), "{line:?}");
        if self.closed {
            return;
        }
        self.outgoing.push(format!("{line}\n"));
        self.sent.push_back(Sent {
            purpose,
            line,
            sent_at: now,
            number: None,
        });
    }

    fn emit_attached(&mut self, event: ClientEvent) {
        if self.phase == Phase::Attached {
            self.events.push(event);
        }
    }

    fn desync(&mut self, reason: String) {
        if matches!(self.phase, Phase::Exited | Phase::Disconnected) {
            return;
        }
        log::warn!("tmux control channel desynced: {reason}");
        self.phase = Phase::Disconnected;
        self.reset_flow();
        self.events.push(ClientEvent::Desync { reason });
    }

    fn reset_flow(&mut self) {
        self.sent.clear();
        self.syncs.clear();
        self.input_queue.clear();
        self.unacknowledged_keys = 0;
        self.not_responding = false;
    }

    fn on_exit(&mut self, reason: Option<String>) {
        if matches!(self.phase, Phase::Exited | Phase::Disconnected) {
            return;
        }
        if self.wait_exit && !self.closed {
            self.closed = true;
            self.outgoing.push("\n".to_owned());
        }
        let event = if self.model.session().is_none() {
            ClientEvent::AttachFailed {
                error: self.last_error.take().or(reason),
            }
        } else {
            ClientEvent::Exited { reason }
        };
        self.phase = Phase::Exited;
        self.reset_flow();
        self.events.push(event);
    }

    fn on_begin(&mut self, number: u64, flags: u32) {
        if flags & FROM_THIS_CLIENT == 0 {
            return;
        }
        let Some(sent) = self.sent.front_mut() else {
            return self.desync(format!("reply {number} with no command outstanding"));
        };
        if let Some(open) = sent.number {
            return self.desync(format!("reply {number} began inside reply {open}"));
        }
        if let Some(last) = self.last_number
            && number <= last
        {
            return self.desync(format!("reply {number} is not after reply {last}"));
        }
        sent.number = Some(number);
        self.last_number = Some(number);
        match sent.purpose {
            Purpose::Capture {
                pane_id,
                generation,
            } => {
                if let Some(sync) = self.syncs.get_mut(&pane_id)
                    && sync.generation == generation
                {
                    sync.begun = true;
                }
            }
            Purpose::ListWindows(_) => self.window_list = WindowList::Running,
            _ => {}
        }
    }

    fn on_end(&mut self, reply: CommandReply, now: Duration) {
        if reply.flags & FROM_THIS_CLIENT == 0 {
            if reply.error {
                self.last_error = Some(reply_text(&reply));
            }
            return;
        }
        if self.sent.front().and_then(|sent| sent.number) != Some(reply.number) {
            return self.desync(format!("reply {} ended without its %begin", reply.number));
        }
        let Some(sent) = self.sent.pop_front() else {
            return;
        };
        // Capture payloads are pane text, which may legitimately look like anything.
        let carries_pane_text = matches!(sent.purpose, Purpose::Capture { .. });
        if !carries_pane_text && reply.lines.iter().any(|line| is_guard_line(line)) {
            return self.desync(format!("reply {} contains a nested block", reply.number));
        }
        self.dispatch(sent.purpose, reply, now);
        if self.not_responding
            && self
                .sent
                .front()
                .is_none_or(|oldest| now < oldest.sent_at + self.config.command_timeout)
        {
            self.not_responding = false;
            self.events.push(ClientEvent::Responding);
        }
    }

    fn dispatch(&mut self, purpose: Purpose, reply: CommandReply, now: Duration) {
        match purpose {
            Purpose::Version => self.on_version(reply, now),
            Purpose::ClientFlags => {
                self.wait_exit = !reply.error;
                self.handshake_step_done();
            }
            Purpose::ListWindows(kind) => self.on_list_windows(kind, reply),
            Purpose::ListPanes(kind) => self.on_list_panes(kind, reply, now),
            Purpose::Prefix => {
                self.key_config.prefix = key_option(&reply);
                self.handshake_step_done();
            }
            Purpose::Prefix2 => {
                self.key_config.prefix2 = key_option(&reply);
                self.handshake_step_done();
            }
            Purpose::ListKeys => {
                let bindings = (!reply.error).then(|| {
                    reply
                        .lines
                        .iter()
                        .filter_map(|l| parse_binding(l))
                        .collect()
                });
                self.key_config.bindings = Some(bindings.unwrap_or_default());
                self.handshake_step_done();
            }
            Purpose::Capture {
                pane_id,
                generation,
            } => self.on_capture(pane_id, generation, reply, now),
            Purpose::Cursor {
                pane_id,
                generation,
            } => self.on_cursor(pane_id, generation, reply, now),
            Purpose::PaneMode(pane_id) => {
                if !reply.error {
                    let mode = reply_text(&reply);
                    let mode = (!mode.is_empty()).then_some(mode);
                    self.model.set_pane_mode(pane_id, mode.clone());
                    self.emit_attached(ClientEvent::PaneModeChanged { pane_id, mode });
                }
            }
            Purpose::ClientKey => {
                self.unacknowledged_keys = self.unacknowledged_keys.saturating_sub(1);
                self.pump_input(now);
            }
            Purpose::PaneFlow | Purpose::PaneInput => {}
            Purpose::User(id) => self
                .events
                .push(ClientEvent::CommandCompleted { id, reply }),
        }
    }

    fn on_version(&mut self, reply: CommandReply, now: Duration) {
        let text = reply_text(&reply);
        let mut fields = text.splitn(2, '\t');
        let raw_version = fields.next().unwrap_or_default().to_owned();
        let version = (!reply.error)
            .then(|| TmuxVersion::parse(&raw_version))
            .flatten()
            .filter(TmuxVersion::meets_native_floor);
        let Some(version) = version else {
            self.phase = Phase::Unsupported;
            self.events.push(ClientEvent::UnsupportedVersion {
                version: raw_version,
            });
            return;
        };
        self.client_name = fields.next().map(str::to_owned).filter(|n| !n.is_empty());
        let list_keys = version.supports_client_key_commands();
        self.version = Some(version);

        let pause_after = self.config.pause_after.as_secs().max(1);
        self.write(
            format!("refresh-client -f pause-after={pause_after},wait-exit"),
            Purpose::ClientFlags,
            now,
        );
        self.send_lists(ListKind::Handshake, now);
        self.write("show -gv prefix".into(), Purpose::Prefix, now);
        self.write("show -gv prefix2".into(), Purpose::Prefix2, now);
        self.handshake_remaining = 5;
        if list_keys {
            self.write(
                format!("list-keys -F '{LIST_KEYS_FORMAT}'"),
                Purpose::ListKeys,
                now,
            );
            self.handshake_remaining += 1;
        }
    }

    fn handshake_step_done(&mut self) {
        self.handshake_remaining = self.handshake_remaining.saturating_sub(1);
        self.maybe_finish_attach();
    }

    fn maybe_finish_attach(&mut self) {
        if self.phase != Phase::Handshaking
            || self.version.is_none()
            || self.handshake_remaining > 0
        {
            return;
        }
        let Some(session) = self.model.session().cloned() else {
            return;
        };
        self.phase = Phase::Attached;
        self.events
            .push(ClientEvent::KeyConfigLoaded(self.key_config.clone()));
        self.events.push(ClientEvent::Attached {
            session_id: session.id,
            session_name: session.name,
        });
    }

    fn send_lists(&mut self, kind: ListKind, now: Duration) {
        self.write(
            format!("list-windows -F '{LIST_WINDOWS_FORMAT}'"),
            Purpose::ListWindows(kind),
            now,
        );
        self.write(
            format!("list-panes -s -F '{LIST_PANES_FORMAT}'"),
            Purpose::ListPanes(kind),
            now,
        );
        self.window_list = WindowList::Queued;
    }

    /// Coalesces window refreshes: a list that has not started yet covers new changes.
    fn request_window_refresh(&mut self, now: Duration) {
        match self.window_list {
            WindowList::Queued => {}
            WindowList::Running => self.refresh_again = true,
            WindowList::Idle if self.phase == Phase::Attached => {
                self.send_lists(ListKind::Refresh, now)
            }
            WindowList::Idle => {}
        }
    }

    fn on_list_windows(&mut self, kind: ListKind, reply: CommandReply) {
        let rows = if reply.error {
            Vec::new()
        } else {
            reply
                .lines
                .iter()
                .filter_map(|line| parse_window_row(line))
                .collect()
        };
        let changes = self.model.replace_windows(rows);
        match kind {
            ListKind::Handshake => self.handshake_step_done(),
            ListKind::Reload => {}
            ListKind::Refresh => {
                for change in changes {
                    let event = match change {
                        WindowChange::Added(window_id) => ClientEvent::WindowAdded { window_id },
                        WindowChange::Closed(window_id) => ClientEvent::WindowClosed { window_id },
                        WindowChange::Renamed(window_id, name) => {
                            ClientEvent::WindowRenamed { window_id, name }
                        }
                        WindowChange::LayoutChanged(window_id) => {
                            let Some(layout) = self
                                .model
                                .window(window_id)
                                .and_then(|window| window.layout.clone())
                            else {
                                continue;
                            };
                            ClientEvent::LayoutChanged { window_id, layout }
                        }
                    };
                    self.emit_attached(event);
                }
            }
        }
    }

    fn on_list_panes(&mut self, kind: ListKind, reply: CommandReply, now: Duration) {
        let rows: Vec<_> = if reply.error {
            Vec::new()
        } else {
            reply
                .lines
                .iter()
                .filter_map(|line| parse_pane_row(line))
                .collect()
        };
        let seeds: Vec<_> = rows.iter().map(|row| (row.id, row.alternate_on)).collect();
        self.model.replace_panes(rows);
        self.window_list = WindowList::Idle;
        if kind != ListKind::Refresh {
            self.panes_listed = true;
            for (pane_id, alternate_on) in seeds {
                let kind = if alternate_on {
                    SnapshotKind::Repaint
                } else {
                    SnapshotKind::Seed
                };
                self.start_sync(pane_id, kind, true, now);
            }
        }
        match kind {
            ListKind::Handshake => self.handshake_step_done(),
            ListKind::Reload => {
                if let Some((session_id, name)) = self.pending_session_change.take() {
                    self.emit_attached(ClientEvent::SessionChanged { session_id, name });
                }
            }
            ListKind::Refresh => {}
        }
        if std::mem::take(&mut self.refresh_again) {
            self.request_window_refresh(now);
        }
    }

    fn on_session_changed(&mut self, session_id: SessionId, name: String, now: Duration) {
        let switched = self.model.set_session(session_id, name.clone());
        if switched && self.phase == Phase::Attached {
            self.panes_listed = false;
            self.syncs.clear();
            self.pending_session_change = Some((session_id, name));
            self.send_lists(ListKind::Reload, now);
        }
        self.maybe_finish_attach();
    }

    fn on_layout(&mut self, window_id: WindowId, layout: WindowLayout) {
        let added = self.model.add_window(window_id);
        let delta = self.model.apply_layout(window_id, layout.clone());
        for pane_id in delta.removed {
            self.syncs.remove(&pane_id);
        }
        if added {
            self.emit_attached(ClientEvent::WindowAdded { window_id });
        }
        self.emit_attached(ClientEvent::LayoutChanged { window_id, layout });
    }

    fn on_output(&mut self, pane_id: PaneId, bytes: Vec<u8>) {
        if !self.panes_listed {
            return;
        }
        let Some(sync) = self.syncs.get_mut(&pane_id) else {
            self.events.push(ClientEvent::PaneOutput { pane_id, bytes });
            return;
        };
        if !sync.begun || sync.overflowed {
            return;
        }
        sync.held_bytes += bytes.len();
        if sync.held_bytes > self.config.max_buffered_output_bytes {
            sync.overflowed = true;
            sync.held = Vec::new();
        } else {
            sync.held.push(bytes);
        }
    }

    fn on_pause(&mut self, pane_id: PaneId, now: Duration) {
        if !self.paused_by_app.contains(&pane_id) {
            self.continue_and_resync(pane_id, now);
        }
    }

    fn on_continue(&mut self, pane_id: PaneId, now: Duration) {
        if self.panes_listed && !self.syncs.contains_key(&pane_id) {
            let kind = self.resync_kind(pane_id);
            self.start_sync(pane_id, kind, true, now);
        }
    }

    fn continue_and_resync(&mut self, pane_id: PaneId, now: Duration) {
        self.write(
            format!("refresh-client -A '{pane_id}:continue'"),
            Purpose::PaneFlow,
            now,
        );
        if !self.panes_listed {
            return;
        }
        let kind = match self.syncs.get(&pane_id) {
            Some(sync) => sync.kind,
            None => self.resync_kind(pane_id),
        };
        self.start_sync(pane_id, kind, true, now);
    }

    fn resync_kind(&self, pane_id: PaneId) -> SnapshotKind {
        if self
            .model
            .pane(pane_id)
            .is_some_and(|pane| pane.alternate_on)
        {
            SnapshotKind::Repaint
        } else {
            SnapshotKind::Resync
        }
    }

    fn start_sync(
        &mut self,
        pane_id: PaneId,
        kind: SnapshotKind,
        with_history: bool,
        now: Duration,
    ) {
        let generation = self.next_generation;
        self.next_generation += 1;
        let history = self.config.history_lines;
        let with_history = with_history && history > 0 && kind != SnapshotKind::Repaint;
        self.syncs.insert(
            pane_id,
            PaneSync {
                generation,
                kind,
                with_history,
                begun: false,
                lines: None,
                held: Vec::new(),
                held_bytes: 0,
                overflowed: false,
            },
        );
        let capture = if with_history {
            format!("capture-pane -p -e -J -S -{history} -t {pane_id}")
        } else {
            format!("capture-pane -p -e -t {pane_id}")
        };
        self.write(
            capture,
            Purpose::Capture {
                pane_id,
                generation,
            },
            now,
        );
        self.write(
            format!("display-message -p -t {pane_id} '#{{cursor_x}}\t#{{cursor_y}}'"),
            Purpose::Cursor {
                pane_id,
                generation,
            },
            now,
        );
    }

    fn on_capture(&mut self, pane_id: PaneId, generation: u64, reply: CommandReply, now: Duration) {
        let Some(sync) = self.syncs.get_mut(&pane_id) else {
            return;
        };
        if sync.generation != generation {
            return;
        }
        if reply.error {
            // The pane is gone; hand over whatever arrived for it.
            if let Some(sync) = self.syncs.remove(&pane_id) {
                for bytes in sync.held {
                    self.events.push(ClientEvent::PaneOutput { pane_id, bytes });
                }
            }
            return;
        }
        let size: usize = reply.lines.iter().map(|line| line.len() + 1).sum();
        if size > self.config.max_snapshot_bytes && sync.with_history {
            let kind = sync.kind;
            self.start_sync(pane_id, kind, false, now);
            return;
        }
        sync.lines = Some(reply.lines);
    }

    fn on_cursor(&mut self, pane_id: PaneId, generation: u64, reply: CommandReply, now: Duration) {
        let Some(sync) = self.syncs.get(&pane_id) else {
            return;
        };
        if sync.generation != generation || sync.lines.is_none() {
            return;
        }
        if sync.overflowed {
            let (kind, with_history) = (sync.kind, sync.with_history);
            self.start_sync(pane_id, kind, with_history, now);
            return;
        }
        let Some(sync) = self.syncs.remove(&pane_id) else {
            return;
        };
        let cursor = (!reply.error).then(|| parse_cursor(&reply)).flatten();
        self.events.push(ClientEvent::PaneSnapshot(PaneSnapshot {
            pane_id,
            kind: sync.kind,
            lines: sync.lines.unwrap_or_default(),
            cursor,
        }));
        for bytes in sync.held {
            self.events.push(ClientEvent::PaneOutput { pane_id, bytes });
        }
    }

    fn pump_input(&mut self, now: Duration) {
        while let Some(front) = self.input_queue.front() {
            if matches!(front, QueuedInput::Key { .. })
                && self.unacknowledged_keys >= self.config.max_unacknowledged_client_keys
            {
                break;
            }
            let Some(input) = self.input_queue.pop_front() else {
                break;
            };
            match input {
                QueuedInput::Key { key, queued_at } => {
                    let stale =
                        now.saturating_sub(queued_at) > self.config.stale_navigation_key_age;
                    if stale && self.droppable(&key) {
                        self.events.push(ClientEvent::ClientKeyDropped { key });
                        continue;
                    }
                    let client = self.client_name.as_deref().unwrap_or_default();
                    let line = format!("send-keys -K -c {} {}", quote(client), quote(&key.key));
                    self.write(line, Purpose::ClientKey, now);
                    self.unacknowledged_keys += 1;
                }
                QueuedInput::Pane(lines) => {
                    for line in lines {
                        self.write(line, Purpose::PaneInput, now);
                    }
                }
            }
        }
    }

    /// Only root-table navigation keys for a pane outside any mode may be dropped (KTD13).
    fn droppable(&self, key: &ClientKey) -> bool {
        key.class == KeyClass::Navigation
            && key.in_root_table
            && self
                .model
                .pane(key.pane_id)
                .is_none_or(|pane| pane.mode.is_none())
    }
}

fn check_one_line(line: &str) -> Result<(), CommandError> {
    if line.trim().is_empty() || line.contains(['\n', '\r']) {
        Err(CommandError::NotOneLine)
    } else {
        Ok(())
    }
}

/// Double-quotes an argument for tmux's command parser.
fn quote(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        if matches!(c, '"' | '\\' | '$') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

fn reply_text(reply: &CommandReply) -> String {
    reply
        .lines
        .iter()
        .map(|line| String::from_utf8_lossy(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// `show -gv prefix` prints `None` for an unset key.
fn key_option(reply: &CommandReply) -> Option<String> {
    let value = reply_text(reply);
    (!reply.error && !value.is_empty() && value != "None").then_some(value)
}

fn parse_binding(line: &[u8]) -> Option<KeyBinding> {
    let line = String::from_utf8_lossy(line);
    let mut fields = line.splitn(4, '\t');
    Some(KeyBinding {
        table: fields.next()?.to_owned(),
        repeat: fields.next()? == "1",
        key: fields.next()?.to_owned(),
        command: fields.next()?.to_owned(),
    })
}

fn parse_cursor(reply: &CommandReply) -> Option<(u32, u32)> {
    let text = reply_text(reply);
    let (x, y) = text.split_once('\t')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

/// A `%begin`/`%end`/`%error` line with numeric fields, which cannot appear in a well-formed
/// reply payload.
fn is_guard_line(line: &[u8]) -> bool {
    let mut fields = line.split(|&b| b == b' ');
    let is_guard = matches!(fields.next(), Some(b"%begin" | b"%end" | b"%error"));
    let numeric: Vec<_> = fields.collect();
    is_guard
        && (2..=3).contains(&numeric.len())
        && numeric
            .iter()
            .all(|f| !f.is_empty() && f.iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
