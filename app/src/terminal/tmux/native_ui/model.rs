//! [`TmuxNativeUi`]: runs the native surfaces against live sessions.
//!
//! It opens a surface once tmux expanded its formats, feeds keys to the surface's state machine,
//! and sends what the surface produced. It also turns `%message` into toasts, tracks panes in a
//! tmux mode for the mode strip, and answers the key router's watchdog when tmux UI slipped
//! through to Warp's client.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use warp_terminal::tmux::{ClientEvent, CommandReply, PaneId};
use warpui::r#async::Timer;
use warpui::keymap::Keystroke;
use warpui::{AppContext, Entity, ModelContext, SingletonEntity, WindowId};

use super::{
    CHOOSE_TREE_QUERY, Classified, ConfirmState, DEFAULT_DISPLAY_PANES_TIME, DEFAULT_DISPLAY_TIME,
    DISPLAY_TIME_QUERY, Interactive, MenuSpec, MenuSpecItem, MenuState, PanesState, PickerState,
    PromptSpec, PromptState, SurfaceOutcome, classify, default_confirm_prompt, expand_query,
    key_of, parse_display_time, parse_expansion, parse_tree_rows, prompt_inputs, prompt_labels,
    prompt_submit_commands, select_target_commands, watchdog_cancel_script, watchdog_toast_text,
};
use crate::terminal::model::session::Session;
use crate::terminal::model::session::command_executor::ExecuteCommandOptions;
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::gateway::{TmuxGatewayEvent, TmuxGateways};
use crate::terminal::tmux::key_tables::HOOK_MARKER;
use crate::terminal::tmux::keys::{TmuxKeyRouter, TmuxKeyRouterEvent};
use crate::terminal::tmux::workspace_sync::{ReplyCallback, TmuxSessions};
use crate::view_components::DismissibleToast;
use crate::workspace::{ToastStack, WorkspaceAction};

/// What a native surface shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    Confirm(ConfirmState),
    Prompt {
        state: PromptState,
        spec: PromptSpec,
    },
    Menu(MenuState),
    Picker {
        state: PickerState,
        current_session: Option<String>,
    },
    Panes(PanesState),
}

/// The surface open for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSurface {
    /// The tmux pane the binding was pressed in; formats expand for it.
    pub pane_id: PaneId,
    pub kind: SurfaceKind,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxNativeUiEvent {
    /// A surface opened, changed or closed.
    SurfaceChanged { origin: TmuxOriginId },
    /// A pane entered or left a tmux mode.
    ModeChanged { origin: TmuxOriginId },
}

/// See the module docs.
#[derive(Default)]
pub struct TmuxNativeUi {
    surfaces: HashMap<TmuxOriginId, ActiveSurface>,
    modes: HashMap<TmuxOriginId, HashMap<PaneId, String>>,
    display_time: HashMap<TmuxOriginId, Duration>,
    /// Where the watchdog runs its second tmux client: the origin pane's session, which reaches
    /// the server's host (locally or through a Warpified SSH session).
    executors: HashMap<TmuxOriginId, Arc<Session>>,
    /// Surfaces waiting on expansions; a newer request or a cancel bumps it.
    generation: u64,
}

impl Entity for TmuxNativeUi {
    type Event = TmuxNativeUiEvent;
}

impl SingletonEntity for TmuxNativeUi {}

impl TmuxNativeUi {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxGateways::handle(ctx), |me, _, event, ctx| {
            me.on_gateway_event(event, ctx);
        });
        ctx.subscribe_to_model(&TmuxKeyRouter::handle(ctx), |me, _, event, ctx| {
            if let TmuxKeyRouterEvent::Stalled { origin, command } = event {
                me.on_stalled(*origin, command.as_deref(), ctx);
            }
        });
        Self::default()
    }

    /// The surface open for `origin`.
    pub fn surface(&self, origin: TmuxOriginId) -> Option<&ActiveSurface> {
        self.surfaces.get(&origin)
    }

    /// The tmux mode `pane_id` is in.
    pub fn mode(&self, origin: TmuxOriginId, pane_id: PaneId) -> Option<&str> {
        self.modes.get(&origin)?.get(&pane_id).map(String::as_str)
    }

    /// Every pane of `origin` in a mode.
    pub fn modes(&self, origin: TmuxOriginId) -> Vec<(PaneId, String)> {
        let mut modes: Vec<(PaneId, String)> = self
            .modes
            .get(&origin)
            .map(|modes| modes.iter().map(|(p, m)| (*p, m.clone())).collect())
            .unwrap_or_default();
        modes.sort();
        modes
    }

    /// The session the watchdog's second client runs through.
    pub fn set_executor(&mut self, origin: TmuxOriginId, session: Arc<Session>) {
        self.executors.insert(origin, session);
    }

    /// A key routed to tmux in `pane_id` is bound to `command`. Returns whether Warp draws it
    /// (the key must then not be sent).
    pub fn intercept(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        command: &str,
        ctx: &mut ModelContext<Self>,
    ) -> bool {
        match classify(command) {
            Classified::Native(interactive) => {
                self.open(origin, pane_id, interactive, ctx);
                true
            }
            Classified::Plain | Classified::Unsupported(_) => false,
        }
    }

    /// Runs a command a surface produced, drawing it natively if it opens tmux UI itself, so no
    /// interactive command is ever sent from Warp's client.
    pub fn run(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        line: &str,
        ctx: &mut ModelContext<Self>,
    ) {
        match classify(line) {
            Classified::Native(interactive) => self.open(origin, pane_id, interactive, ctx),
            Classified::Unsupported(name) => {
                log::info!("tmux: not running {name} from a native surface");
                show_toast(watchdog_toast_text(Some(&name)), DEFAULT_DISPLAY_TIME, ctx);
            }
            Classified::Plain => send(origin, line.to_owned(), None, ctx),
        }
    }

    /// A key pressed while `origin`'s surface has focus.
    pub fn key(
        &mut self,
        origin: TmuxOriginId,
        keystroke: &Keystroke,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(key) = key_of(keystroke) else {
            return;
        };
        let Some(surface) = self.surfaces.get_mut(&origin) else {
            return;
        };
        let outcome = match &mut surface.kind {
            SurfaceKind::Confirm(state) => state.key(&key),
            SurfaceKind::Prompt { state, .. } => state.key(&key),
            SurfaceKind::Menu(state) => state.key(&key),
            SurfaceKind::Picker { state, .. } => state.key(&key),
            SurfaceKind::Panes(state) => state.key(&key),
        };
        self.finish(origin, outcome, ctx);
    }

    /// A click on row `index` of `origin`'s menu or picker.
    pub fn activate(&mut self, origin: TmuxOriginId, index: usize, ctx: &mut ModelContext<Self>) {
        let Some(surface) = self.surfaces.get(&origin) else {
            return;
        };
        let outcome = match &surface.kind {
            SurfaceKind::Menu(state) => state.activate(index),
            SurfaceKind::Picker { state, .. } => state.activate(index),
            SurfaceKind::Confirm(_) | SurfaceKind::Prompt { .. } | SurfaceKind::Panes(_) => {
                return;
            }
        };
        self.finish(origin, outcome, ctx);
    }

    /// Closes `origin`'s surface without running anything.
    pub fn cancel(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        self.generation += 1;
        if self.surfaces.remove(&origin).is_some() {
            ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin });
        }
    }

    fn finish(
        &mut self,
        origin: TmuxOriginId,
        outcome: SurfaceOutcome,
        ctx: &mut ModelContext<Self>,
    ) {
        match outcome {
            SurfaceOutcome::Continue => {
                ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin });
            }
            SurfaceOutcome::Cancel => self.cancel(origin, ctx),
            SurfaceOutcome::Run(commands) => {
                let Some(surface) = self.surfaces.remove(&origin) else {
                    return;
                };
                ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin });
                for command in commands {
                    match &surface.kind {
                        // Menu items are formats, expanded for the pane before they run, as
                        // tmux does.
                        SurfaceKind::Menu(_) => {
                            self.expand_and_run(origin, surface.pane_id, command, ctx)
                        }
                        _ => self.run(origin, surface.pane_id, &command, ctx),
                    }
                }
            }
            SurfaceOutcome::Submit(values) => {
                let Some(surface) = self.surfaces.remove(&origin) else {
                    return;
                };
                ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin });
                if let SurfaceKind::Prompt { spec, .. } = &surface.kind {
                    let client = client_name(origin, ctx);
                    for line in prompt_submit_commands(spec, &values, client.as_deref()) {
                        send(origin, line, None, ctx);
                    }
                }
            }
            SurfaceOutcome::Select(target) => {
                let Some(surface) = self.surfaces.remove(&origin) else {
                    return;
                };
                ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin });
                if let SurfaceKind::Picker {
                    current_session, ..
                } = &surface.kind
                {
                    let client = client_name(origin, ctx);
                    for line in select_target_commands(
                        &target,
                        current_session.as_deref(),
                        client.as_deref(),
                    ) {
                        send(origin, line, None, ctx);
                    }
                }
            }
        }
    }

    fn expand_and_run(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        command: String,
        ctx: &mut ModelContext<Self>,
    ) {
        send(
            origin,
            expand_query(&command, pane_id),
            Some(Box::new(move |reply, ctx| {
                let expanded = parse_expansion(reply).unwrap_or(command);
                Self::handle(ctx).update(ctx, |me, ctx| me.run(origin, pane_id, &expanded, ctx));
            })),
            ctx,
        );
    }

    fn open(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        interactive: Interactive,
        ctx: &mut ModelContext<Self>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        match interactive {
            Interactive::Confirm(spec) => {
                let prompt = spec
                    .prompt
                    .clone()
                    .unwrap_or_else(|| default_confirm_prompt(&spec.command));
                expand_all(origin, pane_id, vec![prompt], ctx, move |mut texts, ctx| {
                    let prompt = texts.pop().unwrap_or_default();
                    let kind = SurfaceKind::Confirm(ConfirmState::new(&spec, prompt));
                    Self::show(origin, pane_id, generation, kind, ctx);
                });
            }
            Interactive::Prompt(spec) => {
                let labels = prompt_labels(&spec);
                let count = labels.len();
                let mut formats = labels;
                formats.extend(prompt_inputs(&spec, count));
                expand_all(origin, pane_id, formats, ctx, move |texts, ctx| {
                    let (labels, inputs) = texts.split_at(count.min(texts.len()));
                    let state = PromptState::new(&spec, labels.to_vec(), inputs.to_vec());
                    let kind = SurfaceKind::Prompt { state, spec };
                    Self::show(origin, pane_id, generation, kind, ctx);
                });
            }
            Interactive::Menu(spec) => {
                let mut formats = vec![spec.title.clone().unwrap_or_default()];
                formats.extend(menu_names(&spec));
                expand_all(origin, pane_id, formats, ctx, move |texts, ctx| {
                    let (title, names) = match texts.split_first() {
                        Some((title, names)) => (title.clone(), names),
                        None => (String::new(), &[][..]),
                    };
                    let state = MenuState::new(&spec, title, names);
                    Self::show(origin, pane_id, generation, SurfaceKind::Menu(state), ctx);
                });
            }
            Interactive::Choose(spec) => {
                let current_session = TmuxGateways::as_ref(ctx)
                    .model(origin)
                    .and_then(|model| model.session())
                    .map(|session| session.id.to_string());
                send(
                    origin,
                    CHOOSE_TREE_QUERY.to_owned(),
                    Some(Box::new(move |reply, ctx| {
                        let state = PickerState::new(
                            spec,
                            parse_tree_rows(reply),
                            current_session.as_deref(),
                        );
                        let kind = SurfaceKind::Picker {
                            state,
                            current_session,
                        };
                        Self::show(origin, pane_id, generation, kind, ctx);
                    })),
                    ctx,
                );
            }
            Interactive::Panes(spec) => {
                let panes = current_window_panes(origin, ctx);
                let kind = SurfaceKind::Panes(PanesState {
                    panes,
                    template: spec.template,
                });
                Self::show(origin, pane_id, generation, kind, ctx);
                let _ = ctx.spawn(
                    Timer::after(DEFAULT_DISPLAY_PANES_TIME),
                    move |me, _, ctx| {
                        if me
                            .surfaces
                            .get(&origin)
                            .is_some_and(|surface| surface.generation == generation)
                        {
                            me.cancel(origin, ctx);
                        }
                    },
                );
            }
            Interactive::Popup(spec) => {
                crate::terminal::tmux::popup::TmuxPopups::handle(ctx).update(ctx, |popups, ctx| {
                    popups.open_bound_popup(origin, &spec, ctx);
                });
            }
        }
    }

    fn show(
        origin: TmuxOriginId,
        pane_id: PaneId,
        generation: u64,
        kind: SurfaceKind,
        ctx: &mut AppContext,
    ) {
        Self::handle(ctx).update(ctx, |me, ctx| {
            if me.generation != generation {
                return;
            }
            me.surfaces.insert(
                origin,
                ActiveSurface {
                    pane_id,
                    kind,
                    generation,
                },
            );
            ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin });
        });
    }

    fn on_gateway_event(&mut self, event: &TmuxGatewayEvent, ctx: &mut ModelContext<Self>) {
        match event {
            TmuxGatewayEvent::Client { origin, event } => self.on_client_event(*origin, event, ctx),
            TmuxGatewayEvent::Ended { origin } => {
                self.surfaces.remove(origin);
                self.modes.remove(origin);
                self.display_time.remove(origin);
                self.executors.remove(origin);
                ctx.emit(TmuxNativeUiEvent::SurfaceChanged { origin: *origin });
                ctx.emit(TmuxNativeUiEvent::ModeChanged { origin: *origin });
            }
            TmuxGatewayEvent::Starting { .. }
            | TmuxGatewayEvent::Refused { .. }
            | TmuxGatewayEvent::PaneQueueReady { .. }
            | TmuxGatewayEvent::CommandCompleted { .. } => {}
        }
    }

    fn on_client_event(
        &mut self,
        origin: TmuxOriginId,
        event: &ClientEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        match event {
            ClientEvent::Attached { .. } | ClientEvent::SessionChanged { .. } => {
                send(
                    origin,
                    DISPLAY_TIME_QUERY.to_owned(),
                    Some(Box::new(move |reply, ctx| {
                        let time = parse_display_time(reply);
                        Self::handle(ctx).update(ctx, |me, _| {
                            me.display_time.insert(origin, time);
                        });
                    })),
                    ctx,
                );
            }
            ClientEvent::Message { text } => {
                if text.trim() == HOOK_MARKER {
                    return;
                }
                let text: String = text.chars().filter(|c| !c.is_control()).collect();
                let time = self
                    .display_time
                    .get(&origin)
                    .copied()
                    .unwrap_or(DEFAULT_DISPLAY_TIME);
                show_toast(text, time, ctx);
            }
            ClientEvent::PaneModeChanged { pane_id, mode } => {
                let modes = self.modes.entry(origin).or_default();
                let changed = match mode.as_deref().filter(|mode| !mode.is_empty()) {
                    Some(mode) => modes.insert(*pane_id, mode.to_owned()).as_deref() != Some(mode),
                    None => modes.remove(pane_id).is_some(),
                };
                if changed {
                    ctx.emit(TmuxNativeUiEvent::ModeChanged { origin });
                }
            }
            _ => {}
        }
    }

    /// The key router's watchdog fired. If the last key's binding opens tmux UI, it is open on
    /// Warp's client: a second client closes it, and a toast names it.
    fn on_stalled(
        &mut self,
        origin: TmuxOriginId,
        command: Option<&str>,
        ctx: &mut ModelContext<Self>,
    ) {
        let name = match command.map(classify) {
            Some(Classified::Unsupported(name)) => name,
            Some(Classified::Native(interactive)) => native_name(&interactive).to_owned(),
            Some(Classified::Plain) | None => return,
        };
        let identity = TmuxKeyRouter::as_ref(ctx).core().identity(origin).cloned();
        let (Some(identity), Some(session)) = (identity, self.executors.get(&origin).cloned())
        else {
            log::warn!("tmux: {name} is open on Warp's client and cannot be closed from here");
            show_toast(watchdog_toast_text(Some(&name)), DEFAULT_DISPLAY_TIME, ctx);
            return;
        };
        let script = watchdog_cancel_script(&identity.socket_path, &identity.client_name);
        log::info!("tmux: watchdog cancelling {name}");
        let _ = ctx.spawn(
            async move {
                session
                    .execute_command(&script, None, None, ExecuteCommandOptions::default())
                    .await
            },
            move |_, result, ctx| {
                if let Err(error) = result {
                    log::warn!("tmux: watchdog could not cancel {name}: {error}");
                }
                show_toast(watchdog_toast_text(Some(&name)), DEFAULT_DISPLAY_TIME, ctx);
            },
        );
    }
}

fn native_name(interactive: &Interactive) -> &'static str {
    match interactive {
        Interactive::Confirm(_) => "confirm-before",
        Interactive::Prompt(_) => "command-prompt",
        Interactive::Choose(_) => "choose-tree",
        Interactive::Menu(_) => "display-menu",
        Interactive::Popup(_) => "display-popup",
        Interactive::Panes(_) => "display-panes",
    }
}

fn menu_names(spec: &MenuSpec) -> Vec<String> {
    spec.items
        .iter()
        .filter_map(|item| match item {
            MenuSpecItem::Item { name, .. } => Some(name.clone()),
            MenuSpecItem::Separator => None,
        })
        .collect()
}

/// The panes of the session's current window, in tmux's index order (layout order).
fn current_window_panes(origin: TmuxOriginId, app: &AppContext) -> Vec<PaneId> {
    let Some(model) = TmuxGateways::as_ref(app).model(origin) else {
        return Vec::new();
    };
    model
        .active_window()
        .and_then(|window_id| model.window(window_id))
        .and_then(|window| window.layout.as_ref())
        .map(|layout| layout.layout.pane_ids())
        .unwrap_or_default()
}

/// The name of Warp's control client for `origin`.
pub(crate) fn client_name(origin: TmuxOriginId, app: &AppContext) -> Option<String> {
    TmuxKeyRouter::as_ref(app)
        .core()
        .identity(origin)
        .map(|identity| identity.client_name.clone())
}

type Reply = Box<dyn FnOnce(&CommandReply, &mut AppContext)>;

/// Sends one command through the session registry, which routes its reply.
pub(crate) fn send(
    origin: TmuxOriginId,
    line: String,
    on_reply: Option<Reply>,
    ctx: &mut AppContext,
) {
    TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
        let on_reply = on_reply.map(|on_reply| {
            Box::new(
                move |reply: &CommandReply,
                      _: &mut TmuxSessions,
                      ctx: &mut ModelContext<TmuxSessions>| on_reply(reply, ctx),
            ) as ReplyCallback
        });
        if sessions.send_command(origin, line, on_reply, ctx).is_none() {
            log::debug!("tmux: native UI command dropped; origin {origin:?} is gone");
        }
    });
}

/// Expands every format for `pane_id`, then calls `done` with the results in order.
fn expand_all(
    origin: TmuxOriginId,
    pane_id: PaneId,
    formats: Vec<String>,
    ctx: &mut AppContext,
    done: impl FnOnce(Vec<String>, &mut AppContext) + 'static,
) {
    if formats.is_empty() {
        done(Vec::new(), ctx);
        return;
    }
    let results: Rc<RefCell<Vec<String>>> = Rc::default();
    let count = formats.len();
    let mut done = Some(done);
    for (index, format) in formats.into_iter().enumerate() {
        let results = results.clone();
        let last = index + 1 == count;
        let done = if last { done.take() } else { None };
        send(
            origin,
            expand_query(&format, pane_id),
            Some(Box::new(move |reply, ctx| {
                results
                    .borrow_mut()
                    .push(parse_expansion(reply).unwrap_or(format));
                if let Some(done) = done {
                    let texts = std::mem::take(&mut *results.borrow_mut());
                    done(texts, ctx);
                }
            })),
            ctx,
        );
    }
}

/// Shows a toast in the active window for `duration` (at least tmux's `display-time`).
pub(crate) fn show_toast(text: String, duration: Duration, ctx: &mut AppContext) {
    let _ = show_toast_with(DismissibleToast::default(text), duration, ctx);
}

/// Shows `toast` in the active window and removes it after `duration`. Returns its identifier
/// and window, to remove it earlier.
pub(crate) fn show_toast_with(
    toast: DismissibleToast<WorkspaceAction>,
    duration: Duration,
    ctx: &mut AppContext,
) -> Option<(String, WindowId)> {
    let window_id = ctx.windows().active_window()?;
    if !ctx.has_singleton_model::<ToastStack>() {
        return None;
    }
    let identifier = format!("tmux-toast-{}", uuid::Uuid::new_v4());
    let toast = toast.with_object_id(identifier.clone());
    let removal = identifier.clone();
    ToastStack::handle(ctx).update(ctx, |stack, ctx| {
        stack.add_persistent_toast(toast, window_id, ctx);
        let _ = ctx.spawn(Timer::after(duration), move |stack, _, ctx| {
            stack.remove_toast_by_identifier(removal, window_id, ctx);
        });
    });
    Some((identifier, window_id))
}
