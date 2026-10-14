//! The event loop of one tmux pane (U6, KTD3, KTD12).
//!
//! A tmux pane has no PTY of its own. Its output arrives on the pane's queue from the origin
//! PTY's gateway, and everything Warp would write to a PTY goes back to tmux as a request:
//!
//! - The queue is drained and parsed on a background executor, never on the UI thread, and only
//!   the pane's own model is locked while parsing. The origin model is never touched here.
//! - Input, including whole commands from the input editor, becomes [`TmuxRequest::PaneInput`]
//!   (`send-keys -H`). Terminal queries in pane output (DA, DSR, OSC 10/11, ...) are not
//!   answered: tmux is the terminal the pane's program talks to and answers them itself, so a
//!   second answer from Warp would reach the program as typed input.
//! - A resize is reported as [`TmuxPaneEventLoopEvent::Resized`]; the window sizer turns the
//!   sizes of a window's panes into one `refresh-client -C @W:WxH`.
//! - `Shutdown` and `ChildExited` only release this view's state. Dropping a Warp pane sends
//!   them, including on window close and quit, so they never kill the tmux pane.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_channel::Receiver;
use instant::Instant;
use parking_lot::{FairMutex, Mutex};
use warp_terminal::local_tty::tmux_gateway::{PaneChunk, PaneQueueReceiver, TmuxRequest};
use warp_terminal::tmux::{PaneId, SnapshotKind};
use warpui::{Entity, ModelContext};

use super::gateway::TmuxRequestSender;
use crate::terminal::TerminalModel;
use crate::terminal::event_listener::ChannelEventListener;
use crate::terminal::model::ansi::Processor;
use crate::terminal::writeable_pty::Message;

/// Where a pane's requests go: its origin's gateway in the app, a recorder in tests.
pub trait PaneRequestSink: Send + Sync + 'static {
    /// Returns false when the request could not be delivered.
    fn send(&self, request: TmuxRequest) -> bool;
}

impl PaneRequestSink for TmuxRequestSender {
    fn send(&self, request: TmuxRequest) -> bool {
        TmuxRequestSender::send(self, request)
    }
}

/// Drops every request; for a pane whose origin is already gone.
pub struct DisconnectedSink;

impl PaneRequestSink for DisconnectedSink {
    fn send(&self, _request: TmuxRequest) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxPaneEventLoopEvent {
    /// The pane's view changed size. Window sizing happens in [`super::pane_manager`].
    Resized { columns: usize, rows: usize },
    /// The pane's queue closed: the pane or the whole session ended.
    Ended,
}

/// How a drain of the pane's queue stopped.
enum DrainEnd {
    /// The gateway closed the queue.
    Closed,
    /// The view was released; the queue was dropped unread.
    Released,
}

pub struct TmuxPaneEventLoop {
    pane_id: PaneId,
    model: Arc<FairMutex<TerminalModel>>,
    channel_event_listener: ChannelEventListener,
    requests: Arc<dyn PaneRequestSink>,
    /// Set once the view is released; checked by the drain between chunks.
    released: Arc<AtomicBool>,
    /// Closed on release, which wakes a drain waiting for the next chunk.
    release_tx: Option<async_channel::Sender<()>>,
    release_rx: Receiver<()>,
    draining: bool,
    /// A queue the gateway opened before the current one finished draining.
    next_queue: Option<PaneQueueReceiver>,
    /// When the pane last printed anything; shell integration waits for a new pane to go idle.
    last_output: Arc<Mutex<Option<Instant>>>,
}

impl Entity for TmuxPaneEventLoop {
    type Event = TmuxPaneEventLoopEvent;
}

impl TmuxPaneEventLoop {
    /// Handles `messages` from the pane's `PtyController` until it is dropped. Call
    /// [`Self::attach_queue`] once the pane's queue is available.
    pub fn new(
        pane_id: PaneId,
        model: Arc<FairMutex<TerminalModel>>,
        channel_event_listener: ChannelEventListener,
        requests: Arc<dyn PaneRequestSink>,
        messages: Receiver<Message>,
        ctx: &mut ModelContext<Self>,
    ) -> Self {
        let _ = ctx.spawn_stream_local(
            messages,
            |me, message, ctx| me.handle_message(message, ctx),
            |_, _| {},
        );
        let (release_tx, release_rx) = async_channel::bounded(1);
        Self {
            pane_id,
            model,
            channel_event_listener,
            requests,
            released: Arc::new(AtomicBool::new(false)),
            release_tx: Some(release_tx),
            release_rx,
            draining: false,
            next_queue: None,
            last_output: Arc::default(),
        }
    }

    /// When the pane last printed anything, updated as its output is parsed.
    pub fn last_output(&self) -> Arc<Mutex<Option<Instant>>> {
        self.last_output.clone()
    }

    /// Whether the view let go of this pane. Nothing more is parsed or sent once it has.
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }

    /// Whether a queue is being drained.
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// Starts draining `queue` on a background executor, or after the current queue closes. A
    /// queue that arrives after release is dropped.
    pub fn attach_queue(&mut self, queue: PaneQueueReceiver, ctx: &mut ModelContext<Self>) {
        if self.is_released() {
            return;
        }
        if self.draining {
            // The gateway closed the old queue before opening this one; let the old drain finish.
            self.next_queue = Some(queue);
            return;
        }
        self.draining = true;
        let drain = drain(
            queue,
            self.model.clone(),
            self.channel_event_listener.clone(),
            self.released.clone(),
            self.release_rx.clone(),
            self.last_output.clone(),
        );
        let _ = ctx.spawn(drain, |me, end, ctx| {
            me.draining = false;
            match (end, me.next_queue.take()) {
                (DrainEnd::Closed, Some(next)) => me.attach_queue(next, ctx),
                (DrainEnd::Closed, None) => {
                    log::debug!("tmux: pane {} queue closed", me.pane_id);
                    ctx.emit(TmuxPaneEventLoopEvent::Ended);
                }
                (DrainEnd::Released, _) => {}
            }
        });
    }

    /// Handles one message from the pane's `PtyController`.
    pub fn handle_message(&mut self, message: Message, ctx: &mut ModelContext<Self>) {
        match message {
            Message::Input(bytes) => {
                if self.is_released() || bytes.is_empty() {
                    return;
                }
                let request = TmuxRequest::PaneInput {
                    pane_id: self.pane_id,
                    bytes: bytes.into_owned(),
                };
                if !self.requests.send(request) {
                    log::debug!(
                        "tmux: pane {} input dropped; its session is gone",
                        self.pane_id
                    );
                }
            }
            Message::Resize(size_info) => {
                if !self.is_released() {
                    ctx.emit(TmuxPaneEventLoopEvent::Resized {
                        columns: size_info.columns,
                        rows: size_info.rows,
                    });
                }
            }
            Message::Shutdown | Message::ChildExited => self.release(),
            // Session-level requests go through the origin PTY's own event loop.
            Message::Tmux(request) => {
                log::warn!(
                    "tmux: pane {} ignoring a {} request",
                    self.pane_id,
                    request.name()
                );
            }
        }
    }

    /// Lets go of the view's state. Sends nothing to tmux: only an explicit close kills a pane.
    fn release(&mut self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        log::debug!("tmux: pane {} view released", self.pane_id);
        self.release_tx = None;
        self.next_queue = None;
    }
}

/// Drains `queue` into the pane's model until it closes or the view is released.
async fn drain(
    queue: PaneQueueReceiver,
    model: Arc<FairMutex<TerminalModel>>,
    channel_event_listener: ChannelEventListener,
    released: Arc<AtomicBool>,
    release_rx: Receiver<()>,
    last_output: Arc<Mutex<Option<Instant>>>,
) -> DrainEnd {
    enum Next {
        Chunk(PaneChunk),
        Closed,
        Released,
    }

    let mut parser = Processor::new();
    loop {
        let next = futures_lite::future::or(
            async {
                match queue.recv().await {
                    Some(chunk) => Next::Chunk(chunk),
                    None => Next::Closed,
                }
            },
            async {
                // Nothing is ever sent; this resolves when the sender is dropped on release.
                let _ = release_rx.recv().await;
                Next::Released
            },
        )
        .await;
        let chunk = match next {
            Next::Chunk(chunk) => chunk,
            Next::Closed => return DrainEnd::Closed,
            Next::Released => return DrainEnd::Released,
        };
        if released.load(Ordering::Acquire) {
            return DrainEnd::Released;
        }
        apply_chunk(&mut parser, &model, chunk);
        *last_output.lock() = Some(Instant::now());
        channel_event_listener.send_wakeup_event();
    }
}

/// Applies one chunk to the pane's model, holding only that model's lock. The parser's replies
/// to terminal queries are dropped; tmux already answered them (see the module docs).
fn apply_chunk(parser: &mut Processor, model: &FairMutex<TerminalModel>, chunk: PaneChunk) {
    let mut model = model.lock();
    match chunk {
        PaneChunk::Output(bytes) => parser.parse_bytes(&mut *model, &bytes, &mut std::io::sink()),
        PaneChunk::Snapshot(snapshot) => {
            // Whatever the parser was in the middle of is stale now.
            *parser = Processor::new();
            // A repaint redraws only the alternate screen and leaves the blocks alone; seeds
            // and resyncs replace the pane's content.
            if snapshot.kind != SnapshotKind::Repaint {
                model.clear_blocks();
            }
            // Captured text holds no queries worth answering; drop anything it provokes.
            parser.parse_bytes(&mut *model, &snapshot.to_vt_bytes(), &mut std::io::sink());
        }
    }
}
