//! [`TmuxPopups`]: popup windows, the wrapper's PATH, and the hidden-popup probe for live
//! sessions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use warp_terminal::tmux::CommandReply;
use warpui::r#async::Timer;
use warpui::{Entity, ModelContext, SingletonEntity, WindowId as UiWindowId};

use super::{
    GLOBAL_PATH_QUERY, HiddenPopupChange, HiddenPopupWatch, PopupSpec, PopupWindow,
    bound_popup_command, close_popup_command, hidden_popup_text, hidden_popups, parse_clients,
    parse_global_path, parse_pane_pids, parse_probe, parse_window_id, path_with_wrapper,
    path_without_wrapper, popup_window_options_command, popup_windows, probe_commands,
    set_global_path_command, wrapper_dir,
};
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::gateway::TmuxGateways;
use crate::terminal::tmux::keys::TmuxKeyRouter;
use crate::terminal::tmux::native_ui::{client_name, send, show_toast_with};
use crate::terminal::tmux::workspace_sync::{SessionStatus, TmuxSessionEvent, TmuxSessions};
use crate::view_components::DismissibleToast;
use crate::workspace::ToastStack;

/// How often an attached session is probed for a hidden popup.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// How long the hidden-popup toast stays; it is removed earlier when the popup goes away.
const HIDDEN_POPUP_TOAST_TIME: Duration = Duration::from_secs(60);

#[derive(Default)]
struct OriginPopups {
    popups: Vec<PopupWindow>,
    watch: HiddenPopupWatch,
    probing: bool,
    polling: bool,
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
                let restore = set_global_path_command(&path_without_wrapper(&path));
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
    }

    fn on_session_event(&mut self, event: &TmuxSessionEvent, ctx: &mut ModelContext<Self>) {
        match event {
            TmuxSessionEvent::Attached { origin } => {
                self.sync_popups(*origin, ctx);
                let state = self.origins.entry(*origin).or_default();
                if !state.polling {
                    state.polling = true;
                    self.schedule_probe(*origin, ctx);
                }
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
            | TmuxSessionEvent::Notice { .. } => {}
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

    fn schedule_probe(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let _ = ctx.spawn(Timer::after(PROBE_INTERVAL), move |me, _, ctx| {
            if !me.origins.contains_key(&origin) {
                return;
            }
            me.probe(origin, ctx);
            me.schedule_probe(origin, ctx);
        });
    }

    fn probe(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        if TmuxSessions::as_ref(ctx).status(origin) != Some(SessionStatus::Attached) {
            return;
        }
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        if state.probing {
            return;
        }
        state.probing = true;
        let replies: Rc<RefCell<Vec<CommandReply>>> = Rc::default();
        let commands = probe_commands();
        let count = commands.len();
        for (index, line) in commands.into_iter().enumerate() {
            let replies = replies.clone();
            send(
                origin,
                line,
                Some(Box::new(move |reply, ctx| {
                    replies.borrow_mut().push(reply.clone());
                    if index + 1 == count {
                        let replies = std::mem::take(&mut *replies.borrow_mut());
                        Self::handle(ctx).update(ctx, |me, ctx| {
                            me.on_probed(origin, &replies, ctx);
                        });
                    }
                })),
                ctx,
            );
        }
    }

    fn on_probed(
        &mut self,
        origin: TmuxOriginId,
        replies: &[CommandReply],
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        state.probing = false;
        // The first reply is the `run-shell`'s, which carries nothing.
        let [_, probe, panes, clients] = replies else {
            return;
        };
        let found = hidden_popups(
            &parse_probe(probe),
            &parse_pane_pids(panes),
            &parse_clients(clients),
        );
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
    }
}
