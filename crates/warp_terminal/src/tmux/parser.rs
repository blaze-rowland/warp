//! Incremental, byte-oriented parser for the tmux control-mode protocol (`tmux -CC`).
//!
//! The parser runs on raw PTY reads. Before tmux announces control mode with
//! [`CONTROL_MODE_DCS`], and again after `%exit`, bytes belong to the shell and are returned as
//! [`DecodeItem::Shell`]. In between, every complete line is either a command-reply line, a
//! notification, or ignorable noise; nothing in control mode is ever treated as VT bytes.
//!
//! Pane payloads (`%output`, `%extended-output`) are decoded as bytes. tmux splits pane output at
//! arbitrary byte offsets, so a UTF-8 sequence can straddle two `%output` lines; the trailing
//! incomplete sequence is carried per pane and prepended to that pane's next payload.
//!
//! Reference: <https://github.com/tmux/tmux/wiki/Control-Mode>.

use std::collections::HashMap;
use std::fmt;

use super::layout::WindowFlags;

/// DCS sequence tmux emits when a client enters control mode (`tmux -CC`).
pub const CONTROL_MODE_DCS: &[u8] = b"\x1bP1000p";

/// String terminator tmux writes after `%exit` to close the control-mode DCS.
const STRING_TERMINATOR: &[u8] = b"\x1b\\";

const MAX_CONTROL_LINE_BYTES: usize = 1_048_576;
const MAX_REPLY_PAYLOAD_LINES: usize = 10_000;
const MAX_REPLY_PAYLOAD_BYTES: usize = 1_048_576;

/// A tmux pane id (`%N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u32);

/// A tmux window id (`@N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u32);

/// A tmux session id (`$N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u32);

impl PaneId {
    /// Parses `%N`.
    pub fn parse(token: &str) -> Option<Self> {
        parse_id(token.as_bytes(), b'%').map(Self)
    }
}

impl WindowId {
    /// Parses `@N`.
    pub fn parse(token: &str) -> Option<Self> {
        parse_id(token.as_bytes(), b'@').map(Self)
    }
}

impl SessionId {
    /// Parses `$N`.
    pub fn parse(token: &str) -> Option<Self> {
        parse_id(token.as_bytes(), b'$').map(Self)
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "${}", self.0)
    }
}

/// One item from [`ControlModeParser::decode`]: shell bytes outside control mode, or a
/// control-mode event that must not be treated as VT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeItem {
    Shell(Vec<u8>),
    Control(ControlEvent),
}

/// A complete `%begin` … `%end`/`%error` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReply {
    pub time: u64,
    /// tmux's command number. Correlation with outgoing commands belongs to the caller.
    pub number: u64,
    /// `1` when the command came from this client.
    pub flags: u32,
    /// True when the block ended with `%error`.
    pub error: bool,
    /// Reply lines, without line terminators. tmux writes them unescaped, so they are raw bytes.
    pub lines: Vec<Vec<u8>>,
}

/// Parsed control-mode events: one per notification in the tmux control-mode wiki, plus
/// reply framing and parser state changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// The DCS was seen; the stream is now control-mode lines.
    EnteredControlMode,
    CommandBegin {
        time: u64,
        number: u64,
        flags: u32,
    },
    CommandEnd(CommandReply),
    /// `%output` (`age_ms` is `None`) or `%extended-output` (`age_ms` is the reported age).
    PaneOutput {
        pane_id: PaneId,
        age_ms: Option<u64>,
        bytes: Vec<u8>,
    },
    Pause {
        pane_id: PaneId,
    },
    Continue {
        pane_id: PaneId,
    },
    PaneModeChanged {
        pane_id: PaneId,
    },
    /// Layout strings are left unparsed; see [`super::layout::parse_layout_change`].
    LayoutChange {
        window_id: WindowId,
        layout: String,
        visible_layout: Option<String>,
        flags: WindowFlags,
    },
    WindowAdd {
        window_id: WindowId,
    },
    WindowClose {
        window_id: WindowId,
    },
    WindowRenamed {
        window_id: WindowId,
        name: String,
    },
    UnlinkedWindowAdd {
        window_id: WindowId,
    },
    UnlinkedWindowClose {
        window_id: WindowId,
    },
    UnlinkedWindowRenamed {
        window_id: WindowId,
        name: String,
    },
    WindowPaneChanged {
        window_id: WindowId,
        pane_id: PaneId,
    },
    SessionChanged {
        session_id: SessionId,
        name: String,
    },
    ClientSessionChanged {
        client: String,
        session_id: SessionId,
        name: String,
    },
    SessionRenamed {
        session_id: SessionId,
        name: String,
    },
    SessionsChanged,
    SessionWindowChanged {
        session_id: SessionId,
        window_id: WindowId,
    },
    /// `%subscription-changed`; ids that tmux reports as `-` are `None`.
    SubscriptionChanged {
        name: String,
        session_id: Option<SessionId>,
        window_id: Option<WindowId>,
        window_index: Option<u32>,
        pane_id: Option<PaneId>,
        value: String,
    },
    ClientDetached {
        client: String,
    },
    ConfigError {
        message: String,
    },
    Message {
        message: String,
    },
    PasteBufferChanged {
        name: String,
    },
    PasteBufferDeleted {
        name: String,
    },
    Exit {
        reason: Option<String>,
    },
    /// A control line or command-reply payload exceeded its bound. The parser discards further
    /// control text until `%exit`.
    ProtocolOverflow,
}

/// The `%begin` whose `%end`/`%error` has not arrived yet.
struct OpenBlock {
    time: u64,
    number: u64,
    flags: u32,
    lines: Vec<Vec<u8>>,
    bytes: usize,
}

enum State {
    /// Shell bytes; looking for the DCS. `pending` holds at most a partial DCS prefix.
    SeekingDcs { pending: Vec<u8> },
    /// Control-mode lines. `line` holds unterminated bytes.
    Control {
        line: Vec<u8>,
        block: Option<OpenBlock>,
    },
    /// Just after `%exit`: strip the `ESC \` that closes the DCS, then return to the shell.
    AfterExit { pending: Vec<u8> },
    /// After an overflow: drop everything until `%exit`.
    DiscardUntilExit { line: Vec<u8> },
}

/// What a control line did to the parser state.
enum LineOutcome {
    Continue,
    Exited,
    Overflowed,
}

/// Incremental parser for tmux control-mode byte streams.
pub struct ControlModeParser {
    state: State,
    /// Per-pane trailing bytes of an incomplete UTF-8 sequence from the previous payload.
    utf8_carry: HashMap<PaneId, Vec<u8>>,
}

impl Default for ControlModeParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlModeParser {
    pub fn new() -> Self {
        Self {
            state: State::SeekingDcs {
                pending: Vec::new(),
            },
            utf8_carry: HashMap::new(),
        }
    }

    pub fn is_in_control_mode(&self) -> bool {
        matches!(self.state, State::Control { .. })
    }

    /// Consumes a chunk and returns control events only; shell bytes are discarded.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<ControlEvent> {
        self.decode(bytes)
            .into_iter()
            .filter_map(|item| match item {
                DecodeItem::Control(event) => Some(event),
                DecodeItem::Shell(_) => None,
            })
            .collect()
    }

    /// Consumes a chunk, splitting shell bytes from control-mode events at the DCS and `%exit`
    /// boundaries even when they share a read.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<DecodeItem> {
        let mut items = Vec::new();
        let mut input = bytes.to_vec();
        loop {
            let state = std::mem::replace(
                &mut self.state,
                State::SeekingDcs {
                    pending: Vec::new(),
                },
            );
            let (next, leftover) = match state {
                State::SeekingDcs { pending } => self.seek_dcs(pending, input, &mut items),
                State::Control { line, block } => {
                    self.drain_control(line, block, input, &mut items)
                }
                State::AfterExit { pending } => strip_string_terminator(pending, input),
                State::DiscardUntilExit { line } => drain_discard(line, input, &mut items),
            };
            self.state = next;
            match leftover {
                Some(rest) => input = rest,
                None => return items,
            }
        }
    }

    /// Returns the next state and, if the state changed mid-chunk, the unconsumed bytes.
    fn seek_dcs(
        &mut self,
        mut pending: Vec<u8>,
        input: Vec<u8>,
        items: &mut Vec<DecodeItem>,
    ) -> (State, Option<Vec<u8>>) {
        pending.extend_from_slice(&input);
        if let Some(index) = find_subslice(&pending, CONTROL_MODE_DCS) {
            let rest = pending.split_off(index + CONTROL_MODE_DCS.len());
            pending.truncate(index);
            if !pending.is_empty() {
                items.push(DecodeItem::Shell(pending));
            }
            items.push(DecodeItem::Control(ControlEvent::EnteredControlMode));
            self.utf8_carry.clear();
            let state = State::Control {
                line: Vec::new(),
                block: None,
            };
            return (state, Some(rest));
        }
        let hold = dcs_prefix_hold(&pending);
        let held = pending.split_off(pending.len() - hold);
        if !pending.is_empty() {
            items.push(DecodeItem::Shell(pending));
        }
        (State::SeekingDcs { pending: held }, None)
    }

    fn drain_control(
        &mut self,
        mut line: Vec<u8>,
        mut block: Option<OpenBlock>,
        input: Vec<u8>,
        items: &mut Vec<DecodeItem>,
    ) -> (State, Option<Vec<u8>>) {
        line.extend_from_slice(&input);
        let mut start = 0;
        while let Some(offset) = line[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            let outcome = self.handle_line(trim_cr(&line[start..end]), &mut block, items);
            start = end + 1;
            match outcome {
                LineOutcome::Continue => {}
                LineOutcome::Exited => {
                    self.utf8_carry.clear();
                    let state = State::AfterExit {
                        pending: Vec::new(),
                    };
                    return (state, Some(line.split_off(start)));
                }
                LineOutcome::Overflowed => return self.overflow(line.split_off(start), items),
            }
        }
        line.drain(..start);
        if line.len() > MAX_CONTROL_LINE_BYTES {
            return self.overflow(line, items);
        }
        (State::Control { line, block }, None)
    }

    fn overflow(&mut self, rest: Vec<u8>, items: &mut Vec<DecodeItem>) -> (State, Option<Vec<u8>>) {
        self.utf8_carry.clear();
        items.push(DecodeItem::Control(ControlEvent::ProtocolOverflow));
        (State::DiscardUntilExit { line: Vec::new() }, Some(rest))
    }

    fn handle_line(
        &mut self,
        line: &[u8],
        block: &mut Option<OpenBlock>,
        items: &mut Vec<DecodeItem>,
    ) -> LineOutcome {
        if let Some(open) = block {
            if let Some((kind, time, number, _)) = parse_guard(line)
                && kind != GuardKind::Begin
                && time == open.time
                && number == open.number
            {
                let open = block.take().expect("block is open");
                items.push(DecodeItem::Control(ControlEvent::CommandEnd(
                    CommandReply {
                        time: open.time,
                        number: open.number,
                        flags: open.flags,
                        error: kind == GuardKind::Error,
                        lines: open.lines,
                    },
                )));
                return LineOutcome::Continue;
            }
            // Notifications can arrive inside a block (tmux 3.7 writes `%message`, `%pause` and
            // `%continue` there). Anything else is reply payload.
            if let Some(event) = self.parse_notification(line) {
                return self.emit(event, items);
            }
            open.bytes += line.len();
            open.lines.push(line.to_vec());
            if open.lines.len() > MAX_REPLY_PAYLOAD_LINES || open.bytes > MAX_REPLY_PAYLOAD_BYTES {
                return LineOutcome::Overflowed;
            }
            return LineOutcome::Continue;
        }

        if let Some((GuardKind::Begin, time, number, flags)) = parse_guard(line) {
            *block = Some(OpenBlock {
                time,
                number,
                flags,
                lines: Vec::new(),
                bytes: 0,
            });
            items.push(DecodeItem::Control(ControlEvent::CommandBegin {
                time,
                number,
                flags,
            }));
            return LineOutcome::Continue;
        }
        // Unmatched `%end`/`%error`, unknown `%` lines, and stray idle bytes are ignorable.
        match self.parse_notification(line) {
            Some(event) => self.emit(event, items),
            None => LineOutcome::Continue,
        }
    }

    fn emit(&mut self, event: ControlEvent, items: &mut Vec<DecodeItem>) -> LineOutcome {
        let exited = matches!(event, ControlEvent::Exit { .. });
        // A payload that was entirely carried (an incomplete UTF-8 prefix) produces no event.
        let empty_output =
            matches!(&event, ControlEvent::PaneOutput { bytes, .. } if bytes.is_empty());
        if !empty_output {
            items.push(DecodeItem::Control(event));
        }
        if exited {
            LineOutcome::Exited
        } else {
            LineOutcome::Continue
        }
    }

    /// Parses one notification line. Returns `None` for unknown or malformed lines.
    fn parse_notification(&mut self, line: &[u8]) -> Option<ControlEvent> {
        if line.first() != Some(&b'%') {
            return None;
        }
        let (name, rest) = split_token(line);
        let event = match name {
            b"%output" => {
                let (pane, payload) = split_token(rest);
                let pane_id = PaneId(parse_id(pane, b'%')?);
                self.pane_output(pane_id, None, payload)
            }
            b"%extended-output" => {
                let separator = find_subslice(rest, b" : ")?;
                let mut header = rest[..separator].split(|&b| b == b' ');
                let pane_id = PaneId(parse_id(header.next()?, b'%')?);
                let age = parse_number::<u64>(header.next()?)?;
                self.pane_output(pane_id, Some(age), &rest[separator + 3..])
            }
            b"%pause" => ControlEvent::Pause {
                pane_id: PaneId(parse_id(first_token(rest), b'%')?),
            },
            b"%continue" => ControlEvent::Continue {
                pane_id: PaneId(parse_id(first_token(rest), b'%')?),
            },
            b"%pane-mode-changed" => ControlEvent::PaneModeChanged {
                pane_id: PaneId(parse_id(first_token(rest), b'%')?),
            },
            b"%layout-change" => {
                let mut parts = rest.splitn(4, |&b| b == b' ');
                let window_id = WindowId(parse_id(parts.next()?, b'@')?);
                let layout = lossy(parts.next().filter(|layout| !layout.is_empty())?);
                let visible_layout = parts.next().filter(|v| !v.is_empty()).map(lossy);
                let flags = WindowFlags::new(&lossy(parts.next().unwrap_or_default()));
                ControlEvent::LayoutChange {
                    window_id,
                    layout,
                    visible_layout,
                    flags,
                }
            }
            b"%window-add" => ControlEvent::WindowAdd {
                window_id: WindowId(parse_id(first_token(rest), b'@')?),
            },
            b"%window-close" => ControlEvent::WindowClose {
                window_id: WindowId(parse_id(first_token(rest), b'@')?),
            },
            b"%window-renamed" => {
                let (window, name) = split_token(rest);
                ControlEvent::WindowRenamed {
                    window_id: WindowId(parse_id(window, b'@')?),
                    name: lossy(name),
                }
            }
            b"%unlinked-window-add" => ControlEvent::UnlinkedWindowAdd {
                window_id: WindowId(parse_id(first_token(rest), b'@')?),
            },
            b"%unlinked-window-close" => ControlEvent::UnlinkedWindowClose {
                window_id: WindowId(parse_id(first_token(rest), b'@')?),
            },
            b"%unlinked-window-renamed" => {
                let (window, name) = split_token(rest);
                ControlEvent::UnlinkedWindowRenamed {
                    window_id: WindowId(parse_id(window, b'@')?),
                    name: lossy(name),
                }
            }
            b"%window-pane-changed" => {
                let (window, rest) = split_token(rest);
                ControlEvent::WindowPaneChanged {
                    window_id: WindowId(parse_id(window, b'@')?),
                    pane_id: PaneId(parse_id(first_token(rest), b'%')?),
                }
            }
            b"%session-changed" => {
                let (session, name) = split_token(rest);
                ControlEvent::SessionChanged {
                    session_id: SessionId(parse_id(session, b'$')?),
                    name: lossy(name),
                }
            }
            b"%client-session-changed" => {
                let (client, rest) = split_token(rest);
                let (session, name) = split_token(rest);
                ControlEvent::ClientSessionChanged {
                    client: lossy(client),
                    session_id: SessionId(parse_id(session, b'$')?),
                    name: lossy(name),
                }
            }
            b"%session-renamed" => {
                let (session, name) = split_token(rest);
                ControlEvent::SessionRenamed {
                    session_id: SessionId(parse_id(session, b'$')?),
                    name: lossy(name),
                }
            }
            b"%sessions-changed" => ControlEvent::SessionsChanged,
            b"%session-window-changed" => {
                let (session, rest) = split_token(rest);
                ControlEvent::SessionWindowChanged {
                    session_id: SessionId(parse_id(session, b'$')?),
                    window_id: WindowId(parse_id(first_token(rest), b'@')?),
                }
            }
            b"%subscription-changed" => {
                let separator = find_subslice(rest, b" : ")?;
                let mut header = rest[..separator].split(|&b| b == b' ');
                let name = lossy(header.next().filter(|name| !name.is_empty())?);
                let session_id = optional_field(header.next(), |t| parse_id(t, b'$'))?;
                let window_id = optional_field(header.next(), |t| parse_id(t, b'@'))?;
                let window_index = optional_field(header.next(), parse_number::<u32>)?;
                let pane_id = optional_field(header.next(), |t| parse_id(t, b'%'))?;
                ControlEvent::SubscriptionChanged {
                    name,
                    session_id: session_id.map(SessionId),
                    window_id: window_id.map(WindowId),
                    window_index,
                    pane_id: pane_id.map(PaneId),
                    value: lossy(&rest[separator + 3..]),
                }
            }
            b"%client-detached" => ControlEvent::ClientDetached {
                client: lossy(rest),
            },
            b"%config-error" => ControlEvent::ConfigError {
                message: lossy(rest),
            },
            b"%message" => ControlEvent::Message {
                message: lossy(rest),
            },
            b"%paste-buffer-changed" => ControlEvent::PasteBufferChanged { name: lossy(rest) },
            b"%paste-buffer-deleted" => ControlEvent::PasteBufferDeleted { name: lossy(rest) },
            b"%exit" => ControlEvent::Exit {
                reason: parse_exit_reason(rest),
            },
            _ => return None,
        };
        Some(event)
    }

    /// Decodes a pane payload and applies the per-pane UTF-8 carry.
    fn pane_output(
        &mut self,
        pane_id: PaneId,
        age_ms: Option<u64>,
        payload: &[u8],
    ) -> ControlEvent {
        let mut bytes = self.utf8_carry.remove(&pane_id).unwrap_or_default();
        bytes.extend(octal_unescape(payload));
        let incomplete = incomplete_utf8_suffix_len(&bytes);
        if incomplete > 0 {
            let carry = bytes.split_off(bytes.len() - incomplete);
            self.utf8_carry.insert(pane_id, carry);
        }
        ControlEvent::PaneOutput {
            pane_id,
            age_ms,
            bytes,
        }
    }
}

fn strip_string_terminator(mut pending: Vec<u8>, input: Vec<u8>) -> (State, Option<Vec<u8>>) {
    pending.extend_from_slice(&input);
    if pending.starts_with(STRING_TERMINATOR) {
        pending.drain(..STRING_TERMINATOR.len());
    } else if pending.is_empty() || STRING_TERMINATOR.starts_with(&pending) {
        return (State::AfterExit { pending }, None);
    }
    let state = State::SeekingDcs {
        pending: Vec::new(),
    };
    (state, Some(pending))
}

fn drain_discard(
    mut line: Vec<u8>,
    input: Vec<u8>,
    items: &mut Vec<DecodeItem>,
) -> (State, Option<Vec<u8>>) {
    line.extend_from_slice(&input);
    let mut start = 0;
    while let Some(offset) = line[start..].iter().position(|&b| b == b'\n') {
        let end = start + offset;
        let text = trim_cr(&line[start..end]);
        start = end + 1;
        let (name, rest) = split_token(text);
        if name == b"%exit" {
            items.push(DecodeItem::Control(ControlEvent::Exit {
                reason: parse_exit_reason(rest),
            }));
            let state = State::AfterExit {
                pending: Vec::new(),
            };
            return (state, Some(line.split_off(start)));
        }
    }
    line.drain(..start);
    if line.len() > MAX_CONTROL_LINE_BYTES {
        line.clear();
    }
    (State::DiscardUntilExit { line }, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardKind {
    Begin,
    End,
    Error,
}

/// Parses `%begin|%end|%error <time> <number> [<flags>]`, requiring every field to be numeric.
fn parse_guard(line: &[u8]) -> Option<(GuardKind, u64, u64, u32)> {
    let (name, rest) = split_token(line);
    let kind = match name {
        b"%begin" => GuardKind::Begin,
        b"%end" => GuardKind::End,
        b"%error" => GuardKind::Error,
        _ => return None,
    };
    let mut fields = rest.split(|&b| b == b' ');
    let time = parse_number(fields.next()?)?;
    let number = parse_number(fields.next()?)?;
    let flags = match fields.next() {
        Some(flags) => parse_number(flags)?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    Some((kind, time, number, flags))
}

fn parse_exit_reason(rest: &[u8]) -> Option<String> {
    let reason = lossy(rest);
    let reason = reason.trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

/// Decodes tmux control-mode octal escapes (`\ooo`). tmux escapes bytes below 0x20 and `\`;
/// any other byte, and any backslash not followed by three octal digits, passes through.
pub fn octal_unescape(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'\\'
            && let Some(&[a, b, c]) = input.get(i + 1..i + 4)
            && [a, b, c].iter().all(|d| (b'0'..=b'7').contains(d))
        {
            out.push(((a - b'0') << 6) | ((b - b'0') << 3) | (c - b'0'));
            i += 4;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// Length of a trailing UTF-8 sequence that is a valid prefix but not yet complete.
fn incomplete_utf8_suffix_len(bytes: &[u8]) -> usize {
    for i in (bytes.len().saturating_sub(3)..bytes.len()).rev() {
        let byte = bytes[i];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        let have = bytes.len() - i;
        return if have < needed { have } else { 0 };
    }
    0
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Splits at the first space: `(token, rest)`. `rest` is empty when there is no space.
fn split_token(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|&b| b == b' ') {
        Some(index) => (&bytes[..index], &bytes[index + 1..]),
        None => (bytes, &[]),
    }
}

fn first_token(bytes: &[u8]) -> &[u8] {
    split_token(bytes).0
}

fn parse_number<T: std::str::FromStr>(token: &[u8]) -> Option<T> {
    if token.is_empty() || !token.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(token).ok()?.parse().ok()
}

fn parse_id(token: &[u8], sigil: u8) -> Option<u32> {
    parse_number(token.strip_prefix(&[sigil])?)
}

/// Parses a `%subscription-changed` header field: `-` is `None`, anything unparseable fails.
fn optional_field<T>(
    token: Option<&[u8]>,
    parse: impl Fn(&[u8]) -> Option<T>,
) -> Option<Option<T>> {
    match token? {
        b"-" => Some(None),
        token => parse(token).map(Some),
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Number of trailing bytes that could be the start of a DCS split across reads.
fn dcs_prefix_hold(pending: &[u8]) -> usize {
    (1..CONTROL_MODE_DCS.len())
        .rev()
        .find(|&len| pending.ends_with(&CONTROL_MODE_DCS[..len]))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
