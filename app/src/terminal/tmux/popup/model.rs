//! [`TmuxPopups`]: popup windows, the wrapper's PATH, and the hidden-popup probe for live
//! sessions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use instant::Instant;
use warp_terminal::tmux::CommandReply;
use warpui::r#async::Timer;
use warpui::{Entity, ModelContext, SingletonEntity, WindowId as UiWindowId};

use super::{
    CONFIRM_DELAY, GLOBAL_PATH_QUERY, HiddenPopupChange, HiddenPopupWatch, PopupSpec, PopupWindow,
    ProbeState, bound_popup_command, close_popup_command, hidden_popup_text, hidden_popups,
    parse_clients, parse_global_path, parse_pane_pids, parse_probe, parse_window_id,
    path_with_wrapper, popup_window_options_command, popup_windows, probe_read_commands,
    probe_start_command, set_global_path_command, wrapper_dir,
};
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::gateway::TmuxGateways;
use crate::terminal::tmux::keys::{TmuxKeyRouter, TmuxKeyRouterEvent};
use crate::terminal::tmux::native_ui::{client_name, send, show_toast_with};
use crate::terminal::tmux::workspace_sync::{SessionStatus, TmuxSessionEvent, TmuxSessions};
use crate::view_components::DismissibleToast;
use crate::workspace::ToastStack;

/// How long after starting a probe its record is first read, and between reads while the
/// background job has not written it yet.
const PROBE_READ_DELAY: Duration = Duration::from_millis(300);

/// Reads of a probe's record before it is given up.
const PROBE_READ_ATTEMPTS: u32 = 10;

/// How long the hidden-popup toast stays; it is removed earlier when the popup goes away.
const HIDDEN_POPUP_TOAST_TIME: Duration = Duration::from_secs(60);

#[derive(Default)]
struct OriginPopups {
    popups: Vec<PopupWindow>,
    watch: HiddenPopupWatch,
    probe: ProbeState,
    /// A follow-up probe is scheduled.
    follow_up: bool,
    /// The hidden-popup toast, removed when the popup goes away.
    toast: Option<(String, UiWindowId)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxPopupsEvent {
    /// A popup window opened or closed: show or remove its floating pane.
    PopupsChanged { origin: TmuxOriginId },
}

/// See the module docs.
#[derive(Default)]
pub struct TmuxPopups {
    origins: HashMap<TmuxOriginId, OriginPopups>,
}

impl Entity for TmuxPopups {
    type Event = TmuxPopupsEvent;
}

impl SingletonEntity for TmuxPopups {}

impl TmuxPopups {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxSessions::handle(ctx), |me, _, event, ctx| {
            me.on_session_event(event, ctx);
        });
        if ctx.has_singleton_model::<TmuxKeyRouter>() {
            // A key that tmux never answers is the sign of a popup taking keys.
            ctx.subscribe_to_model(&TmuxKeyRouter::handle(ctx), |me, _, event, ctx| {
                if let TmuxKeyRouterEvent::Stalled { origin, .. } = event {
                    me.probe(*origin, ctx);
                }
            });
        }
        Self::default()
    }

    /// The popup windows of `origin`'s session, oldest first. The newest is the one shown.
    pub fn popups(&self, origin: TmuxOriginId) -> &[PopupWindow] {
        self.origins
            .get(&origin)
            .map(|state| state.popups.as_slice())
            .unwrap_or_default()
    }

    /// Runs a bound `display-popup` in a detached window (KTD10).
    pub fn open_bound_popup(
        &mut self,
        origin: TmuxOriginId,
        spec: &PopupSpec,
        ctx: &mut ModelContext<Self>,
    ) {
        send(
            origin,
            bound_popup_command(spec),
            Some(Box::new(move |reply, ctx| {
                if let Some(window_id) = parse_window_id(reply) {
                    send(origin, popup_window_options_command(&window_id), None, ctx);
                }
            })),
            ctx,
        );
    }

    /// The launcher (and the wrapper beside it) is installed on `origin`'s host: put the
    /// wrapper first on the server's global PATH, and restore PATH when Warp detaches.
    pub fn wrapper_installed(
        &mut self,
        origin: TmuxOriginId,
        launcher_path: &str,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(dir) = wrapper_dir(launcher_path) else {
            return;
        };
        send(
            origin,
            GLOBAL_PATH_QUERY.to_owned(),
            Some(Box::new(move |reply, ctx| {
                let Some(path) = parse_global_path(reply) else {
                    return;
                };
                let with_wrapper = path_with_wrapper(&path, &dir);
                if with_wrapper != path {
                    send(origin, set_global_path_command(&with_wrapper), None, ctx);
                }
                // The plugin's own wrapper directory stays: the plugin keeps it while loaded.
                let restore = set_global_path_command(
                    &crate::terminal::tmux::plugins::path_after_detach(&path),
                );
                TmuxKeyRouter::handle(ctx).update(ctx, |router, ctx| {
                    router.set_extra_detach_commands(origin, vec![restore], ctx);
                });
            })),
            ctx,
        );
    }

    /// Closes the hidden popup on Warp's client.
    pub fn close_hidden(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let Some(client) = client_name(origin, ctx) else {
            return;
        };
        send(origin, close_popup_command(&client), None, ctx);
        self.schedule_probe(origin, CONFIRM_DELAY, ctx);
    }

    fn on_session_event(&mut self, event: &TmuxSessionEvent, ctx: &mut ModelContext<Self>) {
        match event {
            TmuxSessionEvent::Attached { origin } => {
                self.sync_popups(*origin, ctx);
                self.probe(*origin, ctx);
            }
            TmuxSessionEvent::WindowsChanged { origin } => self.sync_popups(*origin, ctx),
            TmuxSessionEvent::Ended { origin, .. } => {
                if let Some(state) = self.origins.remove(origin) {
                    if state.watch.offered().is_some() {
                        TmuxKeyRouter::handle(ctx)
                            .update(ctx, |router, _| router.set_hidden_popup(*origin, false));
                    }
                    if !state.popups.is_empty() {
                        ctx.emit(TmuxPopupsEvent::PopupsChanged { origin: *origin });
                    }
                }
            }
            TmuxSessionEvent::ActiveWindowChanged { .. }
            | TmuxSessionEvent::ActivePaneChanged { .. }
            | TmuxSessionEvent::StatusChanged { .. }
            | TmuxSessionEvent::Focus { .. }
            | TmuxSessionEvent::Notice { .. }
            | TmuxSessionEvent::ReattachReady { .. }
            | TmuxSessionEvent::RepliesReady => {}
        }
    }

    fn sync_popups(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let popups = TmuxGateways::as_ref(ctx)
            .model(origin)
            .map(popup_windows)
            .unwrap_or_default();
        let state = self.origins.entry(origin).or_default();
        if state.popups != popups {
            state.popups = popups;
            ctx.emit(TmuxPopupsEvent::PopupsChanged { origin });
        }
    }

    /// Probes again after `delay`, unless a follow-up is already scheduled. While no Warp window
    /// is active the probe waits for the next tick.
    fn schedule_probe(
        &mut self,
        origin: TmuxOriginId,
        delay: Duration,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        if std::mem::replace(&mut state.follow_up, true) {
            return;
        }
        let _ = ctx.spawn(Timer::after(delay), move |me, _, ctx| {
            let Some(state) = me.origins.get_mut(&origin) else {
                return;
            };
            state.follow_up = false;
            if ctx.windows().active_window().is_none() {
                me.schedule_probe(origin, delay, ctx);
                return;
            }
            me.probe(origin, ctx);
        });
    }

    /// Looks for a hidden popup on `origin`'s server: starts the background job, then reads its
    /// record.
    fn probe(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        if TmuxSessions::as_ref(ctx).status(origin) != Some(SessionStatus::Attached) {
            return;
        }
        let Some(token) = self
            .origins
            .entry(origin)
            .or_default()
            .probe
            .start(Instant::now())
        else {
            return;
        };
        send(origin, probe_start_command(token), None, ctx);
        self.read_probe(origin, token, PROBE_READ_ATTEMPTS, ctx);
    }

    fn read_probe(
        &mut self,
        origin: TmuxOriginId,
        token: u64,
        attempts: u32,
        ctx: &mut ModelContext<Self>,
    ) {
        let _ = ctx.spawn(Timer::after(PROBE_READ_DELAY), move |_, _, ctx| {
            let replies: Rc<RefCell<Vec<CommandReply>>> = Rc::default();
            let commands = probe_read_commands();
            let count = commands.len();
            for line in commands {
                let replies = replies.clone();
                send(
                    origin,
                    line,
                    Some(Box::new(move |reply, ctx| {
                        replies.borrow_mut().push(reply.clone());
                        if replies.borrow().len() == count {
                            let replies = std::mem::take(&mut *replies.borrow_mut());
                            Self::handle(ctx).update(ctx, |me, ctx| {
                                me.on_probe_read(origin, token, attempts, &replies, ctx);
                            });
                        }
                    })),
                    ctx,
                );
            }
        });
    }

    fn on_probe_read(
        &mut self,
        origin: TmuxOriginId,
        token: u64,
        attempts: u32,
        replies: &[CommandReply],
        ctx: &mut ModelContext<Self>,
    ) {
        let [record, panes, clients] = replies else {
            return;
        };
        match parse_probe(record, token) {
            Some(children) => {
                let found =
                    hidden_popups(&children, &parse_pane_pids(panes), &parse_clients(clients));
                self.on_probed(origin, token, found, ctx);
            }
            None if attempts > 1 => self.read_probe(origin, token, attempts - 1, ctx),
            None => {
                log::debug!("tmux: the hidden-popup probe did not finish; giving up");
                if let Some(state) = self.origins.get_mut(&origin) {
                    state.probe.abandon(token);
                }
            }
        }
    }

    fn on_probed(
        &mut self,
        origin: TmuxOriginId,
        token: u64,
        found: Vec<super::ServerChild>,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        if !state.probe.finish(token) {
            return;
        }
        let found_any = !found.is_empty();
        match state.watch.on_probe(found) {
            Some(HiddenPopupChange::Found(child)) => {
                log::info!("tmux: hidden popup {} ({})", child.pid, child.command);
                TmuxKeyRouter::handle(ctx)
                    .update(ctx, |router, _| router.set_hidden_popup(origin, true));
                let toast = DismissibleToast::default(hidden_popup_text(&child))
                    .with_on_body_click(move |ctx| {
                        TmuxPopups::handle(ctx)
                            .update(ctx, |popups, ctx| popups.close_hidden(origin, ctx));
                    });
                state.toast = show_toast_with(toast, HIDDEN_POPUP_TOAST_TIME, ctx);
            }
            Some(HiddenPopupChange::Gone) => {
                TmuxKeyRouter::handle(ctx)
                    .update(ctx, |router, _| router.set_hidden_popup(origin, false));
                if let Some((identifier, window_id)) = state.toast.take() {
                    ToastStack::handle(ctx).update(ctx, |stack, ctx| {
                        stack.remove_toast_by_identifier(identifier, window_id, ctx);
                    });
                }
            }
            None => {}
        }
        let offered = state.watch.offered().is_some();
        if let Some(delay) = state.probe.next_delay(found_any, offered) {
            self.schedule_probe(origin, delay, ctx);
        }
    }
}
