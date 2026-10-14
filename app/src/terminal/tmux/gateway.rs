//! App-side owner of the tmux control-mode gateways (U5, KTD2, KTD12).
//!
//! Each local PTY event loop runs a [`Gateway`](warp_terminal::local_tty::tmux_gateway::Gateway)
//! that splits the control stream from shell bytes on the PTY thread. This singleton is the app's
//! end of every gateway:
//!
//! - It mirrors [`TmuxSettings::control_mode_enabled`] into the [`GatewaySwitch`] every event loop
//!   reads, so turning the setting off makes the PTY loops behave as they do without tmux support.
//! - It hands each new origin PTY a [`GatewayLink`] and keeps a weak handle to that origin's
//!   [`OriginFlags`], so callers can report nesting the PTY thread cannot see (R6).
//! - It receives every [`GatewayEvent`] on the main thread, logs it, holds pane queues until the
//!   pane backend takes them, and re-emits the rest as [`TmuxGatewayEvent`]s for the workspace.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use warp_terminal::local_tty::mio_channel;
use warp_terminal::local_tty::tmux_gateway::{
    GatewayEvent, GatewayLink, GatewaySwitch, OriginFlags, PaneQueueLimits, PaneQueueReceiver,
    RefusalReason, TmuxRequest, WeakOriginFlags,
};
use warp_terminal::tmux::{ClientConfig, ClientEvent, CommandReply, PaneId, SessionModel};
use warp_terminal::writeable_pty::Message;
use warpui::{Entity, ModelContext, SingletonEntity};

pub use super::TmuxOriginId;
use super::settings::TmuxSettings;

/// What a gateway reported, on the main thread. Pane bytes are not here: take them with
/// [`TmuxGateways::take_pane_queue`] after [`TmuxGatewayEvent::PaneQueueReady`].
#[derive(Clone, Debug)]
pub enum TmuxGatewayEvent {
    /// Control-mode bytes appeared on `origin` without Warp starting tmux there. Nothing is sent
    /// until the user answers with [`TmuxGateways::answer_confirmation`] (R3).
    ConfirmPending { origin: TmuxOriginId },
    /// `origin` no longer waits for that answer.
    ConfirmCleared { origin: TmuxOriginId },
    /// tmux entered control mode on `origin`; the handshake is running.
    Starting { origin: TmuxOriginId },
    /// A control-mode entry stays raw, with the reason to show (R6, AE5).
    Refused {
        origin: TmuxOriginId,
        reason: RefusalReason,
    },
    /// An engine event (attach, layout, exit, transport loss, ...).
    Client {
        origin: TmuxOriginId,
        event: ClientEvent,
    },
    /// Bytes for `pane_id` are waiting in its queue.
    PaneQueueReady {
        origin: TmuxOriginId,
        pane_id: PaneId,
    },
    /// The reply to a [`TmuxRequest::Command`] sent with `tag`.
    CommandCompleted {
        origin: TmuxOriginId,
        tag: u64,
        reply: CommandReply,
    },
    /// The control stream ended; the origin PTY belongs to its shell again.
    Ended { origin: TmuxOriginId },
}

/// The tag on commands sent just before a detach. Nothing waits for their replies.
pub const DETACH_COMMAND_TAG: u64 = u64::MAX - 1;

/// Sends requests to one origin's gateway through its PTY event loop.
#[derive(Clone)]
pub struct TmuxRequestSender(mio_channel::Sender<Message>);

impl TmuxRequestSender {
    /// Returns false when the origin's event loop is gone.
    pub fn send(&self, request: TmuxRequest) -> bool {
        self.0.send(Message::Tmux(request)).is_ok()
    }
}

struct Origin {
    flags: WeakOriginFlags,
    requests: TmuxRequestSender,
    pane_queues: HashMap<PaneId, PaneQueueReceiver>,
    /// Panes whose view was dropped with the queue it was reading (a zoomed-out or moved pane).
    released_panes: HashSet<PaneId>,
    /// The engine's session model as of its last report, while a session runs.
    model: Option<SessionModel>,
    /// Between `Starting` and `Ended`.
    in_session: bool,
    /// Sent before a detach, e.g. to remove the key router's hooks (U13, KTD13).
    detach_commands: Vec<String>,
}

/// The app's end of every PTY's tmux gateway. See the module docs.
pub struct TmuxGateways {
    switch: GatewaySwitch,
    next_origin: u64,
    events_tx: async_channel::Sender<(TmuxOriginId, GatewayEvent)>,
    origins: HashMap<TmuxOriginId, Origin>,
}

impl Entity for TmuxGateways {
    type Event = TmuxGatewayEvent;
}

impl SingletonEntity for TmuxGateways {}

impl TmuxGateways {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let switch = GatewaySwitch::new(TmuxSettings::as_ref(ctx).control_mode_enabled());
        ctx.subscribe_to_model(&TmuxSettings::handle(ctx), |me, _settings, _event, ctx| {
            me.sync_switch(ctx);
        });
        let (events_tx, events_rx) = async_channel::unbounded();
        let _ = ctx.spawn_stream_local(
            events_rx,
            |me, (origin, event), ctx| me.on_gateway_event(origin, event, ctx),
            |_, _| {},
        );
        Self {
            switch,
            next_origin: 0,
            events_tx,
            origins: HashMap::new(),
        }
    }

    /// Whether the gateways are on (the feature flag and the master setting).
    pub fn is_enabled(&self) -> bool {
        self.switch.is_enabled()
    }

    /// A link for a new origin PTY event loop. `requests` feeds that loop; `env_vars` are the
    /// variables the shell is spawned with, used to tell whether it starts inside tmux.
    pub fn link_for_new_origin(
        &mut self,
        requests: mio_channel::Sender<Message>,
        env_vars: &HashMap<OsString, OsString>,
    ) -> (TmuxOriginId, GatewayLink) {
        self.origins
            .retain(|_, origin| origin.flags.upgrade().is_some());

        let origin_id = TmuxOriginId(self.next_origin);
        self.next_origin += 1;

        let flags = OriginFlags::default();
        // Warp strips TMUX from the shells it starts, so only an explicit spawn value counts.
        flags.set_tmux_env(starts_inside_tmux(
            env_vars.get(OsStr::new("TMUX")).map(OsString::as_os_str),
        ));

        let events_tx = self.events_tx.clone();
        let sink = Box::new(move |event| {
            // Unbounded, so this only fails once the app is shutting down.
            let _ = events_tx.try_send((origin_id, event));
        });
        let requester_tx = requests.clone();
        let requester = Arc::new(move |request| {
            let _ = requester_tx.send(Message::Tmux(request));
        });

        self.origins.insert(
            origin_id,
            Origin {
                flags: flags.downgrade(),
                requests: TmuxRequestSender(requests),
                pane_queues: HashMap::new(),
                released_panes: HashSet::new(),
                model: None,
                in_session: false,
                detach_commands: Vec::new(),
            },
        );
        let link = GatewayLink {
            switch: self.switch.clone(),
            origin: flags,
            sink,
            requester,
            client_config: ClientConfig::default(),
            queue_limits: PaneQueueLimits::default(),
        };
        (origin_id, link)
    }

    /// The origin's nesting flags, while its event loop runs. Set
    /// [`OriginFlags::set_origin_is_tmux_pane`] when the origin is itself a tmux pane.
    pub fn origin_flags(&self, origin: TmuxOriginId) -> Option<OriginFlags> {
        self.origins.get(&origin)?.flags.upgrade()
    }

    /// Sends input, keys, commands, or a detach to the origin's session.
    pub fn request_sender(&self, origin: TmuxOriginId) -> Option<TmuxRequestSender> {
        Some(self.origins.get(&origin)?.requests.clone())
    }

    /// Takes a pane's byte queue after [`TmuxGatewayEvent::PaneQueueReady`]. The pane backend
    /// owns it from then on and must drain it off the UI thread.
    pub fn take_pane_queue(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
    ) -> Option<PaneQueueReceiver> {
        let origin = self.origins.get_mut(&origin)?;
        let queue = origin.pane_queues.remove(&pane_id)?;
        origin.released_panes.remove(&pane_id);
        Some(queue)
    }

    /// A pane's view was dropped while tmux keeps the pane, taking the queue it was reading.
    pub fn pane_view_released(&mut self, origin: TmuxOriginId, pane_id: PaneId) {
        if let Some(origin) = self.origins.get_mut(&origin)
            && origin.in_session
        {
            origin.released_panes.insert(pane_id);
        }
    }

    /// A new view for a pane found no queue waiting. If an earlier view took the pane's queue
    /// with it, asks the gateway for a new queue that starts with a snapshot.
    pub fn request_queue_for_returning_pane(&mut self, origin: TmuxOriginId, pane_id: PaneId) {
        if let Some(origin) = self.origins.get_mut(&origin)
            && origin.released_panes.remove(&pane_id)
        {
            origin.requests.send(TmuxRequest::ResyncPane(pane_id));
        }
    }

    /// The session model of `origin`'s running session, as the engine last reported it. Tabs are
    /// built from it when [`ClientEvent::Attached`] arrives.
    pub fn model(&self, origin: TmuxOriginId) -> Option<&SessionModel> {
        self.origins.get(&origin)?.model.as_ref()
    }

    /// Whether a control-mode session runs on `origin`.
    pub fn is_in_session(&self, origin: TmuxOriginId) -> bool {
        self.origins
            .get(&origin)
            .is_some_and(|origin| origin.in_session)
    }

    /// Every origin with a running session.
    pub fn origins_in_session(&self) -> Vec<TmuxOriginId> {
        let mut origins: Vec<_> = self
            .origins
            .iter()
            .filter(|(_, origin)| origin.in_session)
            .map(|(id, _)| *id)
            .collect();
        origins.sort();
        origins
    }

    /// Detaches every running session without killing anything (quit, R25; the master setting
    /// turning off, R22).
    pub fn detach_all(&self) {
        for origin in self.origins.values().filter(|origin| origin.in_session) {
            Self::detach_origin(origin);
        }
    }

    /// Detaches `origin`'s session, after the commands registered with
    /// [`Self::set_detach_commands`].
    pub fn detach(&self, origin: TmuxOriginId) {
        if let Some(origin) = self.origins.get(&origin) {
            Self::detach_origin(origin);
        }
    }

    /// Opens (`open`) or declines a control-mode start that [`TmuxGatewayEvent::ConfirmPending`]
    /// asked about. Declining shows the held bytes as plain output.
    pub fn answer_confirmation(&self, origin: TmuxOriginId, open: bool) {
        if let Some(origin) = self.origins.get(&origin) {
            origin.requests.send(if open {
                TmuxRequest::ConfirmOpen
            } else {
                TmuxRequest::ConfirmDismiss
            });
        }
    }

    /// Commands to send just before `origin` detaches, replacing earlier ones.
    pub fn set_detach_commands(&mut self, origin: TmuxOriginId, commands: Vec<String>) {
        if let Some(origin) = self.origins.get_mut(&origin) {
            origin.detach_commands = commands;
        }
    }

    fn detach_origin(origin: &Origin) {
        for line in &origin.detach_commands {
            origin.requests.send(TmuxRequest::Command {
                tag: DETACH_COMMAND_TAG,
                line: line.clone(),
            });
        }
        origin.requests.send(TmuxRequest::Detach);
    }

    fn sync_switch(&mut self, ctx: &mut ModelContext<Self>) {
        let enabled = TmuxSettings::as_ref(ctx).control_mode_enabled();
        if enabled != self.switch.is_enabled() {
            log::info!("tmux: control mode gateways {}", on_off(enabled));
            self.switch.set_enabled(enabled);
            if !enabled {
                // Turning the feature off restores today's behavior: no native sessions.
                self.detach_all();
            }
        }
    }

    /// Delivers `event` as if `origin`'s gateway had sent it.
    #[cfg(test)]
    pub fn deliver_for_test(
        &mut self,
        origin: TmuxOriginId,
        event: GatewayEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        self.on_gateway_event(origin, event, ctx);
    }

    fn on_gateway_event(
        &mut self,
        origin: TmuxOriginId,
        event: GatewayEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let event = match event {
            GatewayEvent::ConfirmPending => {
                log::info!("tmux: control mode on origin {origin:?} waits for the user");
                TmuxGatewayEvent::ConfirmPending { origin }
            }
            GatewayEvent::ConfirmCleared => TmuxGatewayEvent::ConfirmCleared { origin },
            GatewayEvent::Starting => {
                log::info!("tmux: control mode starting on origin {origin:?}");
                if let Some(entry) = self.origins.get_mut(&origin) {
                    entry.in_session = true;
                    entry.model = None;
                }
                TmuxGatewayEvent::Starting { origin }
            }
            GatewayEvent::Refused(reason) => {
                log::info!("tmux: staying raw on origin {origin:?}: {reason}");
                TmuxGatewayEvent::Refused { origin, reason }
            }
            GatewayEvent::Model(model) => {
                if let Some(entry) = self.origins.get_mut(&origin) {
                    entry.model = Some(*model);
                }
                return;
            }
            GatewayEvent::Client(event) => {
                log::debug!("tmux: origin {origin:?}: {event:?}");
                TmuxGatewayEvent::Client { origin, event }
            }
            GatewayEvent::PaneQueueOpened { queue } => {
                let pane_id = queue.pane_id();
                let Some(entry) = self.origins.get_mut(&origin) else {
                    return;
                };
                entry.pane_queues.insert(pane_id, queue);
                TmuxGatewayEvent::PaneQueueReady { origin, pane_id }
            }
            GatewayEvent::CommandCompleted { tag, reply } => {
                TmuxGatewayEvent::CommandCompleted { origin, tag, reply }
            }
            GatewayEvent::Ended => {
                log::info!("tmux: control mode ended on origin {origin:?}");
                if let Some(entry) = self.origins.get_mut(&origin) {
                    entry.pane_queues.clear();
                    entry.released_panes.clear();
                    entry.in_session = false;
                    entry.model = None;
                    entry.detach_commands.clear();
                }
                TmuxGatewayEvent::Ended { origin }
            }
        };
        ctx.emit(event);
    }
}

/// Whether a shell spawned with `spawn_value` for `$TMUX` (or inheriting `inherited`) starts
/// inside tmux.
fn starts_inside_tmux(spawn_value: Option<&OsStr>) -> bool {
    spawn_value.is_some_and(|value| !value.is_empty())
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
