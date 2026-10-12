//! The terminal manager behind each tmux pane (U6, KTD3).
//!
//! A tmux pane is an ordinary Warp terminal: a [`TerminalModel`] with a block list and the input
//! editor. Like the shared-session viewer it is fed bytes from elsewhere rather than from a
//! PTY; like `remote_tty` its `PtyController` writes into a channel. The pane's
//! [`TmuxPaneEventLoop`] connects both ends to the session's gateway.
//!
//! [`TmuxWindowSizer`] turns pane sizes into one `refresh-client -C @W:WxH` per tmux window.

use std::any::Any;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::mpsc::SyncSender;

use async_channel::{Receiver, Sender};
use parking_lot::FairMutex;
use pathfinder_geometry::vector::Vector2F;
use warp_terminal::local_tty::tmux_gateway::TmuxRequest;
use warp_terminal::tmux::{
    ClientEvent, LayoutNode, PaneId, WindowId, WindowLayout, refresh_client_window_size_command,
};
use warpui::{
    AppContext, Entity, ModelContext, ModelHandle, SingletonEntity, ViewHandle,
    WindowId as UiWindowId,
};

use super::TmuxOriginId;
use super::gateway::{TmuxGatewayEvent, TmuxGateways};
use super::pane_event_loop::{
    DisconnectedSink, PaneRequestSink, TmuxPaneEventLoop, TmuxPaneEventLoopEvent,
};
use crate::ai::blocklist::InputConfig;
use crate::app_state::TmuxPaneSnapshot;
use crate::context_chips::prompt_type::PromptType;
use crate::pane_group::TerminalViewResources;
use crate::pane_group::pane::DetachType;
use crate::persistence::ModelEvent;
use crate::terminal::event_listener::ChannelEventListener;
use crate::terminal::model::session::{ExecutorCommandEvent, Sessions};
use crate::terminal::model_events::ModelEventDispatcher;
use crate::terminal::shell::{ShellName, ShellType};
use crate::terminal::terminal_manager::{self, BlockSpacing};
use crate::terminal::writeable_pty::terminal_manager_util::{
    init_pty_controller_model, wire_up_pty_controller_with_surface,
};
use crate::terminal::writeable_pty::{self, Message};
use crate::terminal::{ShellLaunchState, TerminalManager, TerminalModel, TerminalView};

type PtyController = writeable_pty::PtyController<Sender<Message>>;

/// The tag on every `refresh-client -C` the sizer sends. Nothing waits for these replies.
pub const WINDOW_SIZE_COMMAND_TAG: u64 = u64::MAX;

/// The terminal manager for one tmux pane. See the module docs.
pub struct TmuxPaneTerminalManager {
    model: Arc<FairMutex<TerminalModel>>,
    origin: TmuxOriginId,
    pane_id: PaneId,
    event_loop: ModelHandle<TmuxPaneEventLoop>,
    // Strong references so the framework keeps these models alive with the pane.
    _pty_controller: ModelHandle<PtyController>,
    _model_events: ModelHandle<ModelEventDispatcher>,
    _view: ViewHandle<TerminalView>,
}

pub struct TmuxPaneTerminalManagerInit {
    pub manager: ModelHandle<Box<dyn TerminalManager>>,
    pub view: ViewHandle<TerminalView>,
}

impl TmuxPaneTerminalManager {
    /// Creates the terminal for `snapshot`'s pane. Output starts flowing once the pane's queue is
    /// ready, which may be before or after this is called.
    pub fn create_model(
        snapshot: TmuxPaneSnapshot,
        resources: TerminalViewResources,
        initial_size: Vector2F,
        model_event_sender: Option<SyncSender<ModelEvent>>,
        window_id: UiWindowId,
        ctx: &mut AppContext,
    ) -> TmuxPaneTerminalManagerInit {
        let TmuxPaneSnapshot {
            origin, pane_id, ..
        } = snapshot;

        let (wakeups_tx, wakeups_rx) = async_channel::unbounded();
        let (events_tx, events_rx) = async_channel::unbounded();
        let (executor_command_tx, executor_command_rx) = async_channel::unbounded();
        // Nothing consumes raw pane bytes; `async_broadcast` needs a capacity of at least 1.
        let (pty_reads_tx, _pty_reads_rx) = async_broadcast::broadcast(1);
        let channel_event_listener = ChannelEventListener::new(wakeups_tx, events_tx, pty_reads_tx);

        let model = create_pane_model(&snapshot, initial_size, channel_event_listener.clone(), ctx);
        let size_info = *model.block_list().size();
        let colors = model.colors();
        let model = Arc::new(FairMutex::new(model));

        let sessions: ModelHandle<Sessions> =
            ctx.add_model(|ctx| Sessions::new(executor_command_tx, ctx));

        let requests: Arc<dyn PaneRequestSink> =
            match TmuxGateways::as_ref(ctx).request_sender(origin) {
                Some(sender) => Arc::new(sender),
                None => {
                    log::warn!("tmux: pane {pane_id} created for a closed origin {origin:?}");
                    Arc::new(DisconnectedSink)
                }
            };
        TmuxWindowSizer::handle(ctx).update(ctx, |sizer, _| {
            sizer.register_origin(origin, requests.clone());
        });

        let backend = wire_pane_backend(
            pane_id,
            model.clone(),
            channel_event_listener,
            events_rx,
            sessions.clone(),
            executor_command_rx,
            requests,
            ctx,
        );
        connect_resizes(origin, pane_id, &backend.event_loop, ctx);
        connect_queue(origin, pane_id, &backend.event_loop, ctx);

        let prompt_type =
            ctx.add_model(|ctx| PromptType::new_dynamic_from_sessions(sessions.clone(), ctx));
        let view_model = model.clone();
        let model_events = backend.model_events.clone();
        let view_sessions = sessions.clone();
        let view_model_event_sender = model_event_sender.clone();
        let view = ctx.add_typed_action_view(window_id, |ctx| {
            TerminalView::new(
                resources,
                wakeups_rx,
                model_events,
                view_model,
                view_sessions,
                size_info,
                colors,
                view_model_event_sender,
                prompt_type,
                None::<InputConfig>,
                None, // No conversation restoration for tmux panes.
                None, // No raw pane reads to broadcast.
                false,
                ctx,
            )
        });

        wire_up_pty_controller_with_surface(
            &backend.pty_controller,
            &view,
            model.clone(),
            sessions,
            model_event_sender,
            ctx,
        );

        let manager = Self {
            model,
            origin,
            pane_id,
            event_loop: backend.event_loop,
            _pty_controller: backend.pty_controller,
            _model_events: backend.model_events,
            _view: view.clone(),
        };
        let manager = ctx.add_model(|_| {
            let manager: Box<dyn TerminalManager> = Box::new(manager);
            manager
        });
        TmuxPaneTerminalManagerInit { manager, view }
    }

    pub fn origin(&self) -> TmuxOriginId {
        self.origin
    }

    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }
}

impl TerminalManager for TmuxPaneTerminalManager {
    fn model(&self) -> Arc<FairMutex<TerminalModel>> {
        self.model.clone()
    }

    fn on_view_detached(&self, detach_type: DetachType, app: &mut AppContext) {
        // Hidden-for-close and moved panes may come back; keep draining for them. Closing
        // releases the view only. Killing the tmux pane is an explicit action (R25).
        if matches!(detach_type, DetachType::Closed) {
            self.event_loop.update(app, |event_loop, ctx| {
                event_loop.handle_message(Message::Shutdown, ctx);
            });
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The shell a pane runs, from tmux's `#{pane_current_command}`. The gateway's own shell says
/// nothing about the pane's. A pane running anything else waits for shell integration to say.
pub fn pane_shell_launch_state(current_command: Option<&str>) -> ShellLaunchState {
    match current_command.and_then(ShellType::from_name) {
        Some(shell_type) => ShellLaunchState::ShellSpawned {
            available_shell: None,
            display_name: ShellName::LessDescriptive(shell_type.name().to_owned()),
            shell_type,
        },
        None => ShellLaunchState::DeterminingShell {
            available_shell: None,
            display_name: ShellName::MoreDescriptive(
                current_command.unwrap_or_default().to_owned(),
            ),
        },
    }
}

/// Settings every pane model gets: output from before shell integration starts is shown as
/// plain scrolling output instead of being hidden as Warp's own bootstrap.
pub(super) fn prepare_pane_model(model: &mut TerminalModel) {
    model.block_list_mut().set_show_bootstrap_block(true);
}

/// Creates the [`TerminalModel`] for a pane.
pub(super) fn create_pane_model(
    snapshot: &TmuxPaneSnapshot,
    initial_size: Vector2F,
    channel_event_listener: ChannelEventListener,
    ctx: &mut AppContext,
) -> TerminalModel {
    let mut model = terminal_manager::create_terminal_model(
        None, /* startup_directory */
        None, /* restored_blocks */
        initial_size,
        channel_event_listener,
        pane_shell_launch_state(snapshot.current_command.as_deref()),
        BlockSpacing::for_gui(ctx),
        ctx,
    );
    prepare_pane_model(&mut model);
    model
}

/// The models behind a pane's view.
pub(super) struct PaneBackend {
    pub(super) model_events: ModelHandle<ModelEventDispatcher>,
    pub(super) pty_controller: ModelHandle<PtyController>,
    pub(super) event_loop: ModelHandle<TmuxPaneEventLoop>,
}

/// Wires a pane's model to a `PtyController` and a [`TmuxPaneEventLoop`] that sends to
/// `requests`.
#[allow(clippy::too_many_arguments)]
pub(super) fn wire_pane_backend(
    pane_id: PaneId,
    model: Arc<FairMutex<TerminalModel>>,
    channel_event_listener: ChannelEventListener,
    model_events_rx: Receiver<crate::terminal::event::Event>,
    sessions: ModelHandle<Sessions>,
    executor_command_rx: Receiver<ExecutorCommandEvent>,
    requests: Arc<dyn PaneRequestSink>,
    ctx: &mut AppContext,
) -> PaneBackend {
    let model_events =
        ctx.add_model(|ctx| ModelEventDispatcher::new(model_events_rx, sessions.clone(), ctx));
    let (messages_tx, messages_rx) = async_channel::unbounded();
    let event_loop_model = model.clone();
    let event_loop = ctx.add_model(|ctx| {
        TmuxPaneEventLoop::new(
            pane_id,
            event_loop_model,
            channel_event_listener,
            requests,
            messages_rx,
            ctx,
        )
    });
    let pty_controller = init_pty_controller_model(
        messages_tx,
        executor_command_rx,
        model_events.clone(),
        sessions,
        model,
        ctx,
    );
    PaneBackend {
        model_events,
        pty_controller,
        event_loop,
    }
}

/// Reports the pane's view size to the [`TmuxWindowSizer`].
pub(super) fn connect_resizes(
    origin: TmuxOriginId,
    pane_id: PaneId,
    event_loop: &ModelHandle<TmuxPaneEventLoop>,
    ctx: &mut AppContext,
) {
    ctx.subscribe_to_model(event_loop, move |_, event, ctx| {
        if let TmuxPaneEventLoopEvent::Resized { columns, rows } = *event {
            TmuxWindowSizer::handle(ctx).update(ctx, |sizer, ctx| {
                sizer.pane_resized(origin, pane_id, columns, rows, ctx);
            });
        }
    });
}

/// Hands the pane's queue to its event loop now if the gateway has it, and whenever the gateway
/// opens a new one.
pub(super) fn connect_queue(
    origin: TmuxOriginId,
    pane_id: PaneId,
    event_loop: &ModelHandle<TmuxPaneEventLoop>,
    ctx: &mut AppContext,
) {
    let gateways = TmuxGateways::handle(ctx);
    let attach = move |event_loop: &ModelHandle<TmuxPaneEventLoop>, ctx: &mut AppContext| {
        let queue = TmuxGateways::handle(ctx)
            .update(ctx, |gateways, _| gateways.take_pane_queue(origin, pane_id));
        if let Some(queue) = queue {
            event_loop.update(ctx, |event_loop, ctx| event_loop.attach_queue(queue, ctx));
        }
    };
    attach(event_loop, ctx);
    let event_loop = event_loop.downgrade();
    ctx.subscribe_to_model(&gateways, move |_, event, ctx| {
        let TmuxGatewayEvent::PaneQueueReady {
            origin: ready_origin,
            pane_id: ready_pane,
        } = event
        else {
            return;
        };
        if (*ready_origin, *ready_pane) != (origin, pane_id) {
            return;
        }
        if let Some(event_loop) = event_loop.upgrade(ctx)
            && !event_loop.as_ref(ctx).is_released()
        {
            attach(&event_loop, ctx);
        }
    });
}

/// One tmux window as the sizer sees it.
struct SizedWindow {
    layout: WindowLayout,
    /// The size last sent since tmux last reported this window's layout.
    sent: Option<(usize, usize)>,
}

/// Sizes tmux windows to the Warp tabs that show them. Each pane reports its view size; once the
/// current layout pass is done the sizer sends one `refresh-client -C @W:WxH` per window whose
/// size changed. tmux's layout stays the source of truth for how panes split the window.
pub struct TmuxWindowSizer {
    origins: HashMap<TmuxOriginId, Arc<dyn PaneRequestSink>>,
    windows: HashMap<(TmuxOriginId, WindowId), SizedWindow>,
    pane_sizes: HashMap<(TmuxOriginId, PaneId), (usize, usize)>,
    dirty: BTreeSet<(TmuxOriginId, WindowId)>,
    flush_scheduled: bool,
}

impl Entity for TmuxWindowSizer {
    type Event = ();
}

impl SingletonEntity for TmuxWindowSizer {}

impl TmuxWindowSizer {
    /// Follows every session's layouts through [`TmuxGateways`].
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxGateways::handle(ctx), |me, _, event, _| {
            me.on_gateway_event(event);
        });
        Self::empty()
    }

    #[cfg(test)]
    pub(super) fn new_for_test() -> Self {
        Self::empty()
    }

    fn empty() -> Self {
        Self {
            origins: HashMap::new(),
            windows: HashMap::new(),
            pane_sizes: HashMap::new(),
            dirty: BTreeSet::new(),
            flush_scheduled: false,
        }
    }

    /// Where `origin`'s window sizes are sent.
    pub fn register_origin(&mut self, origin: TmuxOriginId, requests: Arc<dyn PaneRequestSink>) {
        self.origins.insert(origin, requests);
    }

    /// Records tmux's current layout for a window.
    pub fn set_layout(&mut self, origin: TmuxOriginId, window_id: WindowId, layout: WindowLayout) {
        let panes = layout.layout.pane_ids();
        if let Some(previous) = self.windows.get(&(origin, window_id)) {
            for pane_id in previous.layout.layout.pane_ids() {
                if !panes.contains(&pane_id) {
                    self.pane_sizes.remove(&(origin, pane_id));
                }
            }
        }
        // tmux reported the window anew; a size sent before it no longer counts as current.
        self.windows
            .insert((origin, window_id), SizedWindow { layout, sent: None });
    }

    /// Forgets a closed window.
    pub fn remove_window(&mut self, origin: TmuxOriginId, window_id: WindowId) {
        if let Some(window) = self.windows.remove(&(origin, window_id)) {
            for pane_id in window.layout.layout.pane_ids() {
                self.pane_sizes.remove(&(origin, pane_id));
            }
        }
        self.dirty.remove(&(origin, window_id));
    }

    /// Forgets everything about an origin whose session ended.
    pub fn remove_origin(&mut self, origin: TmuxOriginId) {
        self.origins.remove(&origin);
        self.windows.retain(|(o, _), _| *o != origin);
        self.pane_sizes.retain(|(o, _), _| *o != origin);
        self.dirty.retain(|(o, _)| *o != origin);
    }

    /// Records a pane's view size; its window's size goes out after the current layout pass.
    pub fn pane_resized(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        columns: usize,
        rows: usize,
        ctx: &mut ModelContext<Self>,
    ) {
        if columns == 0 || rows == 0 {
            return;
        }
        let Some(window_id) = self.window_of(origin, pane_id) else {
            log::debug!("tmux: pane {pane_id} resized before its window's layout is known");
            return;
        };
        self.pane_sizes.insert((origin, pane_id), (columns, rows));
        self.dirty.insert((origin, window_id));
        if !self.flush_scheduled {
            // Every pane of a resized tab reports in the same layout pass; send once after it.
            self.flush_scheduled = true;
            let _ = ctx.spawn(async {}, |me, _, _| me.flush());
        }
    }

    /// Sends the size of every window whose size changed.
    pub fn flush(&mut self) {
        self.flush_scheduled = false;
        for (origin, window_id) in std::mem::take(&mut self.dirty) {
            let Some(window) = self.windows.get_mut(&(origin, window_id)) else {
                continue;
            };
            let pane_sizes = &self.pane_sizes;
            let size = node_size(&window.layout.visible_layout, &|pane_id| {
                pane_sizes.get(&(origin, *pane_id)).copied()
            });
            let current = node_size(&window.layout.visible_layout, &|_| None);
            if size == current || window.sent == Some(size) {
                continue;
            }
            let Some(requests) = self.origins.get(&origin) else {
                continue;
            };
            let (columns, rows) = size;
            let line = refresh_client_window_size_command(window_id, columns, rows);
            window.sent = Some(size);
            requests.send(TmuxRequest::Command {
                tag: WINDOW_SIZE_COMMAND_TAG,
                line: line.trim_end().to_owned(),
            });
        }
    }

    fn window_of(&self, origin: TmuxOriginId, pane_id: PaneId) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|((o, _), window)| {
                *o == origin && window.layout.visible_layout.pane_ids().contains(&pane_id)
            })
            .map(|((_, window_id), _)| *window_id)
    }

    fn on_gateway_event(&mut self, event: &TmuxGatewayEvent) {
        match event {
            TmuxGatewayEvent::Client { origin, event } => self.on_client_event(*origin, event),
            TmuxGatewayEvent::Ended { origin } => self.remove_origin(*origin),
            TmuxGatewayEvent::Starting { .. }
            | TmuxGatewayEvent::Refused { .. }
            | TmuxGatewayEvent::PaneQueueReady { .. }
            | TmuxGatewayEvent::CommandCompleted { .. } => {}
        }
    }

    fn on_client_event(&mut self, origin: TmuxOriginId, event: &ClientEvent) {
        match event {
            ClientEvent::LayoutChanged { window_id, layout } => {
                self.set_layout(origin, *window_id, layout.clone());
            }
            ClientEvent::WindowClosed { window_id } => self.remove_window(origin, *window_id),
            ClientEvent::SessionChanged { .. } => {
                let windows: Vec<_> = self
                    .windows
                    .keys()
                    .filter(|(o, _)| *o == origin)
                    .map(|(_, window_id)| *window_id)
                    .collect();
                for window_id in windows {
                    self.remove_window(origin, window_id);
                }
            }
            ClientEvent::Attached { .. }
            | ClientEvent::UnsupportedVersion { .. }
            | ClientEvent::KeyConfigLoaded(_)
            | ClientEvent::SessionRenamed { .. }
            | ClientEvent::WindowAdded { .. }
            | ClientEvent::WindowRenamed { .. }
            | ClientEvent::ActiveWindowChanged { .. }
            | ClientEvent::ActivePaneChanged { .. }
            | ClientEvent::PaneSnapshot(_)
            | ClientEvent::PaneOutput { .. }
            | ClientEvent::PaneModeChanged { .. }
            | ClientEvent::Message { .. }
            | ClientEvent::ConfigError { .. }
            | ClientEvent::CommandCompleted { .. }
            | ClientEvent::ClientKeyDropped { .. }
            | ClientEvent::NotResponding { .. }
            | ClientEvent::Responding
            | ClientEvent::Exited { .. }
            | ClientEvent::AttachFailed { .. }
            | ClientEvent::TransportLost
            | ClientEvent::Desync { .. } => {}
        }
    }
}

/// Columns and rows of `node` with each pane at `pane_size` (tmux's size when `None`), plus
/// tmux's one-cell borders between siblings.
fn node_size(
    node: &LayoutNode,
    pane_size: &impl Fn(&PaneId) -> Option<(usize, usize)>,
) -> (usize, usize) {
    match node {
        LayoutNode::Leaf(leaf) => {
            pane_size(&leaf.pane_id).unwrap_or((leaf.width as usize, leaf.height as usize))
        }
        LayoutNode::Split {
            horizontal,
            children,
            ..
        } => {
            let sizes = children.iter().map(|child| node_size(child, pane_size));
            let borders = children.len().saturating_sub(1);
            if *horizontal {
                sizes.fold((borders, 0), |(width, height), (w, h)| {
                    (width + w, height.max(h))
                })
            } else {
                sizes.fold((0, borders), |(width, height), (w, h)| {
                    (width.max(w), height + h)
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "pane_manager_tests.rs"]
mod tests;
