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

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;

use warp_terminal::local_tty::mio_channel;
use warp_terminal::local_tty::tmux_gateway::{
    GatewayEvent, GatewayLink, GatewaySwitch, OriginFlags, PaneQueueLimits, PaneQueueReceiver,
    RefusalReason, TmuxRequest, WeakOriginFlags,
};
use warp_terminal::tmux::{ClientConfig, ClientEvent, CommandReply, PaneId};
use warp_terminal::writeable_pty::Message;
use warpui::{Entity, ModelContext, SingletonEntity};

use super::settings::TmuxSettings;

/// Identifies one origin PTY event loop. A respawned shell gets a new id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxOriginId(u64);

/// What a gateway reported, on the main thread. Pane bytes are not here: take them with
/// [`TmuxGateways::take_pane_queue`] after [`TmuxGatewayEvent::PaneQueueReady`].
#[derive(Clone, Debug)]
pub enum TmuxGatewayEvent {
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
        flags.set_tmux_env(starts_inside_tmux(
            env_vars.get(OsStr::new("TMUX")).map(OsString::as_os_str),
            std::env::var_os("TMUX").as_deref(),
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
        self.origins.get_mut(&origin)?.pane_queues.remove(&pane_id)
    }

    fn sync_switch(&mut self, ctx: &mut ModelContext<Self>) {
        let enabled = TmuxSettings::as_ref(ctx).control_mode_enabled();
        if enabled != self.switch.is_enabled() {
            log::info!("tmux: control mode gateways {}", on_off(enabled));
            self.switch.set_enabled(enabled);
        }
    }

    fn on_gateway_event(
        &mut self,
        origin: TmuxOriginId,
        event: GatewayEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let event = match event {
            GatewayEvent::Starting => {
                log::info!("tmux: control mode starting on origin {origin:?}");
                TmuxGatewayEvent::Starting { origin }
            }
            GatewayEvent::Refused(reason) => {
                log::info!("tmux: staying raw on origin {origin:?}: {reason}");
                TmuxGatewayEvent::Refused { origin, reason }
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
                }
                TmuxGatewayEvent::Ended { origin }
            }
        };
        ctx.emit(event);
    }
}

/// Whether a shell spawned with `spawn_value` for `$TMUX` (or inheriting `inherited`) starts
/// inside tmux.
fn starts_inside_tmux(spawn_value: Option<&OsStr>, inherited: Option<&OsStr>) -> bool {
    spawn_value
        .or(inherited)
        .is_some_and(|value| !value.is_empty())
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
