//! [`TmuxIntegration`]: runs U9's plans against live sessions.
//!
//! On each attach it makes sure the server has a hook token, installs the launcher on the
//! server's host, and lists the session's panes. Each tmux pane's model gets its derived session
//! id registered before its output is parsed. Panes that existed at attach are planned with
//! [`existing_pane_plan`]; panes that appear later wait until they first go idle and are planned
//! with [`new_pane_plan`], which never types into a pane while Warp's tmux plugin is loaded.
//!
//! It also runs tmux-continuum's autosave while Warp is the only client (KTD11; see
//! [`crate::terminal::tmux::plugins`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use instant::Instant;
use parking_lot::{FairMutex, Mutex};
use warp_terminal::tmux::{CommandReply, PaneId};
use warpui::r#async::Timer;
use warpui::{AppContext, Entity, ModelContext, SingletonEntity, WeakViewHandle};

use super::{
    ExistingPanePlan, LIST_CLIENTS_QUERY, LauncherBundle, NewPanePlan, WarpifyAvailability,
    derive_session_id, ensure_token_command, existing_pane_plan, install_check_commands,
    install_upload_commands, launcher_bundle, new_hook_token, new_pane_plan, pane_facts_query,
    parse_launcher_path, parse_pane_facts, parse_token, plain_clients, read_token_query,
    reannounce_command, session_panes_query,
};
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::plugins::{
    AUTOSAVE_TICK, CONTINUUM_QUERY, autosave_due, parse_continuum, parse_plugin, plugin_query,
};
use crate::terminal::tmux::popup::TmuxPopups;
use crate::terminal::tmux::settings::TmuxSettings;
use crate::terminal::tmux::workspace_sync::{TmuxSessionEvent, TmuxSessions};
use crate::terminal::{TerminalModel, TerminalView};

/// How long a new pane's output must pause before it counts as idle.
const IDLE_QUIET: Duration = Duration::from_millis(500);
/// A new pane that has printed nothing by then counts as idle anyway.
const IDLE_FALLBACK: Duration = Duration::from_secs(5);
/// How often a new pane is checked for idleness.
const IDLE_POLL: Duration = Duration::from_millis(200);
/// A new pane that is still busy by then is planned anyway.
const IDLE_GIVE_UP: Duration = Duration::from_secs(15);
/// How long pane output waits for the token before being parsed without it.
const TOKEN_WAIT: Duration = Duration::from_secs(3);

/// When a pane last printed anything; the pane's event loop updates it.
pub type LastOutput = Arc<Mutex<Option<Instant>>>;

type ConnectQueue = Box<dyn FnOnce(&mut AppContext)>;

struct PaneEntry {
    model: Weak<FairMutex<TerminalModel>>,
    view: WeakViewHandle<TerminalView>,
    last_output: LastOutput,
    created: Instant,
}

#[derive(Default)]
struct OriginState {
    /// The token query was answered (with or without a token).
    token_known: bool,
    token: Option<String>,
    launcher_path: Option<String>,
    /// The attach-time pane listing was answered.
    listed: bool,
    /// What the listing planned for each pane that existed at attach. A pane whose view is
    /// created after the listing still counts as existing, so it is never typed into.
    existing: HashMap<PaneId, ExistingPanePlan>,
    panes: HashMap<PaneId, PaneEntry>,
    /// Pane queues held until the token is known, so no hook is parsed before its id is
    /// registered.
    held_queues: Vec<ConnectQueue>,
    /// New panes being watched for their first idle moment.
    watching: HashSet<PaneId>,
    /// Warp's tmux plugin was loaded at attach (re-read before each new pane is planned).
    plugin: bool,
    /// The running continuum autosave loop, by generation; a new attach starts a new one.
    autosave: Option<u64>,
}

/// See the module docs.
pub struct TmuxIntegration {
    bundle: Option<LauncherBundle>,
    origins: HashMap<TmuxOriginId, OriginState>,
    /// The launcher installed on this machine, once tried.
    local_launcher: Option<Option<String>>,
    /// The last autosave loop generation handed out.
    autosave_generation: u64,
}

impl Entity for TmuxIntegration {
    type Event = ();
}

impl SingletonEntity for TmuxIntegration {}

impl TmuxIntegration {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxSessions::handle(ctx), |me, _, event, ctx| {
            me.on_session_event(event, ctx);
        });
        Self {
            bundle: None,
            origins: HashMap::new(),
            local_launcher: None,
            autosave_generation: 0,
        }
    }

    /// The launcher installed on this machine, for sessions Warp creates here before attaching
    /// (`tmux new`). Installs it on first use; `None` when that fails or integration is off.
    pub fn local_launcher(&mut self, ctx: &AppContext) -> Option<String> {
        if !TmuxSettings::as_ref(ctx).integrate_panes_enabled() {
            return None;
        }
        if let Some(cached) = &self.local_launcher {
            return cached.clone();
        }
        let home = dirs::home_dir();
        let bundle = self.bundle();
        let installed = home.and_then(|home| match super::install_locally(bundle, &home) {
            Ok(path) => Some(path.to_string_lossy().into_owned()),
            Err(error) => {
                log::warn!("tmux: could not install the launcher locally: {error}");
                None
            }
        });
        self.local_launcher = Some(installed.clone());
        installed
    }

    /// The launcher to start Warp-created panes with, once it is installed on `origin`'s host.
    /// `None` when integration is off or the install failed: panes start the default shell.
    pub fn launcher_path(&self, origin: TmuxOriginId, ctx: &AppContext) -> Option<String> {
        if !TmuxSettings::as_ref(ctx).integrate_panes_enabled() {
            return None;
        }
        self.origins.get(&origin)?.launcher_path.clone()
    }

    /// The launcher bundle, built once.
    pub fn bundle(&mut self) -> &LauncherBundle {
        self.bundle
            .get_or_insert_with(|| launcher_bundle(&crate::ASSETS))
    }

    /// Takes a new pane's model. Its derived session id is registered as soon as the token is
    /// known, and `connect_queue` (which starts parsing its output) runs only after that.
    #[allow(clippy::too_many_arguments)]
    pub fn pane_created(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        model: &Arc<FairMutex<TerminalModel>>,
        view: WeakViewHandle<TerminalView>,
        last_output: LastOutput,
        connect_queue: ConnectQueue,
        ctx: &mut ModelContext<Self>,
    ) {
        if !TmuxSettings::as_ref(ctx).integrate_panes_enabled() {
            connect_queue(ctx);
            return;
        }
        let state = self.origins.entry(origin).or_default();
        let is_new = state.listed && !state.existing.contains_key(&pane_id);
        let warpify = match state.existing.get(&pane_id) {
            Some(ExistingPanePlan::Warpify(availability)) => Some(availability.clone()),
            _ => None,
        };
        state.panes.insert(
            pane_id,
            PaneEntry {
                model: Arc::downgrade(model),
                view,
                last_output,
                created: Instant::now(),
            },
        );
        if let Some(token) = &state.token {
            model
                .lock()
                .register_session_id(derive_session_id(token, pane_id));
        }
        if state.token_known {
            connect_queue(ctx);
        } else {
            state.held_queues.push(connect_queue);
            let _ = ctx.spawn(Timer::after(TOKEN_WAIT), move |me, _, ctx| {
                me.release_queues(origin, ctx);
            });
        }
        if is_new {
            self.watch_new_pane(origin, pane_id, ctx);
        } else if let Some(availability) = warpify
            && let Some(view) = self
                .origins
                .get(&origin)
                .and_then(|state| state.panes.get(&pane_id))
                .and_then(|entry| entry.view.upgrade(ctx))
        {
            view.update(ctx, |view, ctx| {
                view.set_tmux_warpify(Some(availability), ctx)
            });
        }
    }

    /// Forgets a pane whose view is gone.
    pub fn pane_dropped(&mut self, origin: TmuxOriginId, pane_id: PaneId) {
        if let Some(state) = self.origins.get_mut(&origin) {
            state.panes.remove(&pane_id);
            state.watching.remove(&pane_id);
        }
    }

    fn on_session_event(&mut self, event: &TmuxSessionEvent, ctx: &mut ModelContext<Self>) {
        match event {
            TmuxSessionEvent::Attached { origin } => self.on_attached(*origin, ctx),
            TmuxSessionEvent::Ended { origin, .. } => {
                self.origins.remove(origin);
            }
            TmuxSessionEvent::WindowsChanged { .. }
            | TmuxSessionEvent::ActiveWindowChanged { .. }
            | TmuxSessionEvent::ActivePaneChanged { .. }
            | TmuxSessionEvent::StatusChanged { .. }
            | TmuxSessionEvent::Focus { .. }
            | TmuxSessionEvent::Notice { .. }
            | TmuxSessionEvent::ReattachReady { .. } => {}
        }
    }

    /// Token, then launcher install, then the pane listing. Commands run in order on the
    /// server, so each reply arrives after the previous step's.
    fn on_attached(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let state = self.origins.entry(origin).or_default();
        // A session switch lists the new session's panes again.
        state.listed = false;
        state.existing.clear();
        state.autosave = None;
        self.start_autosave(origin, ctx);
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        if !TmuxSettings::as_ref(ctx).integrate_panes_enabled() {
            state.token_known = true;
            self.release_queues(origin, ctx);
            return;
        }
        let token = new_hook_token();
        send(origin, ensure_token_command(&token), None, ctx);
        send(
            origin,
            read_token_query(),
            Some(Box::new(move |reply, ctx| {
                let token = parse_token(reply);
                Self::handle(ctx).update(ctx, |me, ctx| me.on_token(origin, token, ctx));
            })),
            ctx,
        );
        send(
            origin,
            plugin_query(),
            Some(Box::new(move |reply, ctx| {
                let plugin = parse_plugin(reply);
                Self::handle(ctx).update(ctx, |me, _| {
                    if let Some(state) = me.origins.get_mut(&origin) {
                        state.plugin = plugin;
                    }
                });
            })),
            ctx,
        );
        let [check, read] = install_check_commands(self.bundle());
        send(origin, check, None, ctx);
        send(
            origin,
            read,
            Some(Box::new(move |reply, ctx| {
                let path = parse_launcher_path(reply);
                Self::handle(ctx).update(ctx, |me, ctx| me.on_install_checked(origin, path, ctx));
            })),
            ctx,
        );
    }

    fn on_token(
        &mut self,
        origin: TmuxOriginId,
        token: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        if token.is_none() {
            log::warn!("tmux: no hook token on origin {origin:?}; panes stay plain");
        }
        state.token_known = true;
        state.token = token;
        if let Some(token) = &state.token {
            for (pane_id, entry) in &state.panes {
                if let Some(model) = entry.model.upgrade() {
                    model
                        .lock()
                        .register_session_id(derive_session_id(token, *pane_id));
                }
            }
        }
        self.release_queues(origin, ctx);
    }

    fn release_queues(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        for connect in std::mem::take(&mut state.held_queues) {
            connect(ctx);
        }
    }

    fn on_install_checked(
        &mut self,
        origin: TmuxOriginId,
        path: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        if path.is_some() {
            self.on_installed(origin, path, ctx);
            return;
        }
        let staging = new_hook_token();
        let mut commands = install_upload_commands(self.bundle(), &staging);
        let read = commands.pop().expect("the upload ends with a read");
        for command in commands {
            send(origin, command, None, ctx);
        }
        send(
            origin,
            read,
            Some(Box::new(move |reply, ctx| {
                let path = parse_launcher_path(reply);
                Self::handle(ctx).update(ctx, |me, ctx| me.on_installed(origin, path, ctx));
            })),
            ctx,
        );
    }

    fn on_installed(
        &mut self,
        origin: TmuxOriginId,
        path: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        match &path {
            Some(path) => log::info!("tmux: launcher installed at {path}"),
            None => log::warn!(
                "tmux: could not install the launcher on origin {origin:?}; Warp's panes start \
                 the default shell"
            ),
        }
        state.launcher_path = path.clone();
        // KTD14: the `tmux` wrapper beside the launcher goes first on the server's PATH.
        if let Some(path) = path
            && ctx.has_singleton_model::<TmuxPopups>()
        {
            TmuxPopups::handle(ctx).update(ctx, |popups, ctx| {
                popups.wrapper_installed(origin, &path, ctx);
            });
        }
        send(
            origin,
            session_panes_query(),
            Some(Box::new(move |reply, ctx| {
                let reply = reply.clone();
                Self::handle(ctx).update(ctx, |me, ctx| me.on_listed(origin, &reply, ctx));
            })),
            ctx,
        );
    }

    fn on_listed(
        &mut self,
        origin: TmuxOriginId,
        reply: &CommandReply,
        ctx: &mut ModelContext<Self>,
    ) {
        let integrate = TmuxSettings::as_ref(ctx).integrate_panes_enabled();
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        state.listed = true;
        let mut reannounce = Vec::new();
        for pane in parse_pane_facts(reply) {
            let plan = existing_pane_plan(&pane, state.token.as_deref(), integrate);
            state.existing.insert(pane.pane_id, plan.clone());
            match plan {
                ExistingPanePlan::Off => {}
                ExistingPanePlan::Reannounce(session_id) => reannounce.push(session_id),
                ExistingPanePlan::Warpify(availability) => {
                    if let Some(view) = state
                        .panes
                        .get(&pane.pane_id)
                        .and_then(|entry| entry.view.upgrade(ctx))
                    {
                        view.update(ctx, |view, ctx| {
                            view.set_tmux_warpify(Some(availability), ctx)
                        });
                    }
                }
            }
        }
        if let Some(command) = reannounce_command(&reannounce) {
            send(origin, command, None, ctx);
        }
    }

    /// Polls a new pane until it is integrated or first goes idle, then plans it.
    fn watch_new_pane(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        state.watching.insert(pane_id);
        let _ = ctx.spawn(Timer::after(IDLE_POLL), move |me, _, ctx| {
            me.check_new_pane(origin, pane_id, ctx);
        });
    }

    fn check_new_pane(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(state) = self.origins.get_mut(&origin) else {
            return;
        };
        if !state.watching.contains(&pane_id) {
            return;
        }
        let Some(entry) = state.panes.get(&pane_id) else {
            state.watching.remove(&pane_id);
            return;
        };
        let Some(model) = entry.model.upgrade() else {
            state.watching.remove(&pane_id);
            return;
        };
        let integrating = {
            let model = model.lock();
            model.block_list().is_bootstrapped() || model.pending_session_id().is_some()
        };
        if integrating {
            state.watching.remove(&pane_id);
            return;
        }
        let age = entry.created.elapsed();
        let quiet = entry
            .last_output
            .lock()
            .map(|last| last.elapsed() >= IDLE_QUIET);
        let idle = match quiet {
            Some(quiet) => quiet || age >= IDLE_GIVE_UP,
            None => age >= IDLE_FALLBACK,
        };
        if !idle {
            let _ = ctx.spawn(Timer::after(IDLE_POLL), move |me, _, ctx| {
                me.check_new_pane(origin, pane_id, ctx);
            });
            return;
        }
        state.watching.remove(&pane_id);
        self.plan_new_pane(origin, pane_id, ctx);
    }

    /// Reads the pane and the attached clients, then applies [`new_pane_plan`].
    fn plan_new_pane(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        ctx: &mut ModelContext<Self>,
    ) {
        let facts: Arc<Mutex<Option<CommandReply>>> = Arc::default();
        let facts_slot = facts.clone();
        send(
            origin,
            pane_facts_query(pane_id),
            Some(Box::new(move |reply, _| {
                *facts_slot.lock() = Some(reply.clone());
            })),
            ctx,
        );
        // The plugin may have been loaded since attach (`tmux source ~/.tmux.conf`).
        send(
            origin,
            plugin_query(),
            Some(Box::new(move |reply, ctx| {
                let plugin = parse_plugin(reply);
                Self::handle(ctx).update(ctx, |me, _| {
                    if let Some(state) = me.origins.get_mut(&origin) {
                        state.plugin = plugin;
                    }
                });
            })),
            ctx,
        );
        send(
            origin,
            LIST_CLIENTS_QUERY.to_owned(),
            Some(Box::new(move |reply, ctx| {
                let clients = plain_clients(reply);
                let facts = facts.lock().take();
                Self::handle(ctx).update(ctx, |me, ctx| {
                    me.apply_new_pane_plan(origin, pane_id, facts, clients, ctx);
                });
            })),
            ctx,
        );
    }

    fn apply_new_pane_plan(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        facts: Option<CommandReply>,
        clients: Option<Vec<String>>,
        ctx: &mut ModelContext<Self>,
    ) {
        let integrate = TmuxSettings::as_ref(ctx).integrate_panes_enabled();
        let Some(state) = self.origins.get(&origin) else {
            return;
        };
        let Some(entry) = state.panes.get(&pane_id) else {
            return;
        };
        let Some(view) = entry.view.upgrade(ctx) else {
            return;
        };
        if entry.model.upgrade().is_some_and(|model| {
            let model = model.lock();
            model.block_list().is_bootstrapped() || model.pending_session_id().is_some()
        }) {
            return;
        }
        let Some(pane) = facts
            .as_ref()
            .map(parse_pane_facts)
            .and_then(|panes| panes.into_iter().find(|pane| pane.pane_id == pane_id))
        else {
            return;
        };
        let plan = new_pane_plan(
            &pane,
            state.token.as_deref(),
            clients.as_deref(),
            state.plugin,
            integrate,
        );
        log::debug!("tmux: new pane {pane_id} on {origin:?}: {plan:?}");
        match plan {
            NewPanePlan::Off | NewPanePlan::Launcher => {}
            NewPanePlan::TypeInit(shell) => {
                view.update(ctx, |view, ctx| view.tmux_type_init(shell, ctx));
            }
            NewPanePlan::Warpify(availability) => {
                view.update(ctx, |view, ctx| {
                    view.set_tmux_warpify(Some(availability), ctx)
                });
            }
        }
    }

    /// Starts continuum's autosave for `origin` if continuum is set up on its server (KTD11).
    fn start_autosave(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        self.autosave_generation += 1;
        let generation = self.autosave_generation;
        send(
            origin,
            CONTINUUM_QUERY.to_owned(),
            Some(Box::new(move |reply, ctx| {
                let configured = parse_continuum(reply).is_some();
                Self::handle(ctx).update(ctx, |me, ctx| {
                    let Some(state) = me.origins.get_mut(&origin) else {
                        return;
                    };
                    if configured && state.autosave.is_none() {
                        log::info!("tmux: running continuum's autosave on {origin:?}");
                        state.autosave = Some(generation);
                        me.schedule_autosave(origin, generation, ctx);
                    }
                });
            })),
            ctx,
        );
    }

    fn schedule_autosave(
        &mut self,
        origin: TmuxOriginId,
        generation: u64,
        ctx: &mut ModelContext<Self>,
    ) {
        let _ = ctx.spawn(Timer::after(AUTOSAVE_TICK), move |me, _, ctx| {
            me.autosave_tick(origin, generation, ctx);
        });
    }

    /// One autosave tick: re-reads continuum's settings and the clients, and runs the save when
    /// it is due and no plain client is attached. Stops when the session ended, a new attach
    /// started another loop, or continuum was turned off.
    fn autosave_tick(
        &mut self,
        origin: TmuxOriginId,
        generation: u64,
        ctx: &mut ModelContext<Self>,
    ) {
        let running = self
            .origins
            .get(&origin)
            .is_some_and(|state| state.autosave == Some(generation));
        if !running {
            return;
        }
        let config: Arc<Mutex<Option<CommandReply>>> = Arc::default();
        let config_slot = config.clone();
        send(
            origin,
            CONTINUUM_QUERY.to_owned(),
            Some(Box::new(move |reply, _| {
                *config_slot.lock() = Some(reply.clone());
            })),
            ctx,
        );
        send(
            origin,
            LIST_CLIENTS_QUERY.to_owned(),
            Some(Box::new(move |reply, ctx| {
                let clients = plain_clients(reply);
                let autosave = config.lock().take().as_ref().and_then(parse_continuum);
                Self::handle(ctx).update(ctx, |me, ctx| {
                    let Some(state) = me.origins.get_mut(&origin) else {
                        return;
                    };
                    if state.autosave != Some(generation) {
                        return;
                    }
                    let Some(autosave) = autosave else {
                        log::info!("tmux: continuum is off on {origin:?}; autosave stopped");
                        state.autosave = None;
                        return;
                    };
                    if autosave_due(clients.as_deref()) {
                        send(origin, autosave.save_command(), None, ctx);
                    }
                    me.schedule_autosave(origin, generation, ctx);
                });
            })),
            ctx,
        );
    }

    /// "Warpify this pane": reads the pane and the attached clients, then asks before typing.
    pub fn warpify(
        &mut self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        view: WeakViewHandle<TerminalView>,
        ctx: &mut ModelContext<Self>,
    ) {
        let facts: Arc<Mutex<Option<CommandReply>>> = Arc::default();
        let facts_slot = facts.clone();
        send(
            origin,
            pane_facts_query(pane_id),
            Some(Box::new(move |reply, _| {
                *facts_slot.lock() = Some(reply.clone());
            })),
            ctx,
        );
        send(
            origin,
            LIST_CLIENTS_QUERY.to_owned(),
            Some(Box::new(move |reply, ctx| {
                let clients = plain_clients(reply);
                let pane = facts
                    .lock()
                    .take()
                    .map(|reply| parse_pane_facts(&reply))
                    .and_then(|panes| panes.into_iter().find(|pane| pane.pane_id == pane_id));
                let Some(pane) = pane else {
                    return;
                };
                let availability =
                    super::warpify_availability(pane.current_command.as_deref(), pane.alternate_on);
                let Some(view) = view.upgrade(ctx) else {
                    return;
                };
                match availability {
                    WarpifyAvailability::Available(shell) => {
                        let text = super::warpify_confirmation_text(clients.as_deref());
                        let weak = view.downgrade();
                        confirm_warpify(
                            text,
                            move |ctx| {
                                if let Some(view) = weak.upgrade(ctx) {
                                    view.update(ctx, |view, ctx| view.tmux_type_init(shell, ctx));
                                }
                            },
                            ctx,
                        );
                    }
                    unavailable @ WarpifyAvailability::Unavailable(_) => {
                        view.update(ctx, |view, ctx| {
                            view.set_tmux_warpify(Some(unavailable), ctx)
                        });
                    }
                }
            })),
            ctx,
        );
    }
}

type Reply = Box<dyn FnOnce(&CommandReply, &mut AppContext)>;

/// Sends one command through the session registry, which routes its reply.
fn send(origin: TmuxOriginId, line: String, on_reply: Option<Reply>, ctx: &mut AppContext) {
    TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
        let on_reply = on_reply.map(|on_reply| {
            Box::new(
                move |reply: &CommandReply,
                      _: &mut TmuxSessions,
                      ctx: &mut ModelContext<TmuxSessions>| on_reply(reply, ctx),
            ) as crate::terminal::tmux::workspace_sync::ReplyCallback
        });
        if sessions.send_command(origin, line, on_reply, ctx).is_none() {
            log::debug!("tmux: integration command dropped; origin {origin:?} is gone");
        }
    });
}

/// Asks before "Warpify this pane" types into the pane (R8).
fn confirm_warpify(
    text: String,
    on_confirm: impl FnOnce(&mut AppContext) + 'static,
    ctx: &mut AppContext,
) {
    crate::terminal::tmux::workspace_sync::show_confirmation(
        "Warpify this pane?",
        text,
        "Warpify",
        on_confirm,
        ctx,
    );
}
