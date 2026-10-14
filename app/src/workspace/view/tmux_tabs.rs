//! tmux sessions in the workspace (U7, R23–R25, R28, R29).
//!
//! A session's windows open as tabs at the origin tab's position and the origin tab hides while
//! attached; it stays in `tabs` with `hidden_for_tmux` set, and every user-facing tab enumeration
//! skips it. The planning lives in [`crate::terminal::tmux::workspace_sync`] and
//! [`crate::terminal::tmux::layout_snapshot`]; this is the glue that applies it.

use std::collections::HashMap;
use std::sync::Arc;

use warp_core::features::FeatureFlag;
use warp_terminal::tmux::{SessionModel, WindowId};
use warpui::elements::{
    Border, Clipped, Container, CornerRadius, CrossAxisAlignment, Flex, Hoverable,
    ParentElement as _, Radius, Text,
};
use warpui::platform::Cursor;
use warpui::{AppContext, Element, SingletonEntity as _, ViewContext, ViewHandle};

use super::{TabConfigsMenuOpenSource, Workspace};
use crate::appearance::Appearance;
use crate::menu::{MenuItem, MenuItemFields};
use crate::pane_group::{PaneGroup, PanesLayout};
use crate::tab::TabData;
use crate::terminal::TerminalView;
use crate::terminal::model::session::command_executor::ExecuteCommandOptions;
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::gateway::TmuxGateways;
use crate::terminal::tmux::layout_snapshot::{layout_to_snapshot, tab_layout};
use crate::terminal::tmux::native_ui::TmuxNativeUi;
use crate::terminal::tmux::remote::{OriginSession, TmuxRemoteEvent, TmuxRemoteHosts};
use crate::terminal::tmux::session_menu::{
    ATTACH_LABEL, ListOutcome, MenuEntry, NEW_SESSION_LABEL, TmuxMenuAction, attach_menu_entries,
    command_for_with_launcher, list_outcome, list_sessions_command,
};
use crate::terminal::tmux::settings::TmuxSettings;
use crate::terminal::tmux::status_strip::{ELLIPSIS, build_status_strip, tab_for_window};
use crate::terminal::tmux::workspace_sync::{
    OpenTab, SessionStatus, TmuxSessionEvent, TmuxSessions, WindowOp, busy_commands,
    close_confirmation_text, insertion_offset, kill_window_command, new_window_command,
    plan_window_sync, reattach_command, rename_window_command, show_kill_confirmation,
    window_commands_query,
};
use crate::themes::theme::Fill;
use crate::ui_components::icons;
use crate::workspace::action::WorkspaceAction;
use crate::workspace::tab_settings::TabSettings;

/// The status strip's side padding, in pixels.
const STRIP_HORIZONTAL_PADDING: f32 = 8.;
/// The space between status strip entries, in pixels.
const STRIP_ENTRY_GAP: f32 = 8.;
/// A UI font character's rough width, in ems, for the status strip's character budget.
const CHAR_WIDTH_EM: f32 = 0.65;

impl Workspace {
    /// Follows tmux sessions whose origin tab is in this workspace.
    pub(super) fn subscribe_to_tmux_sessions(&mut self, ctx: &mut ViewContext<Self>) {
        if !ctx.has_singleton_model::<TmuxSessions>() {
            return;
        }
        ctx.subscribe_to_model(&TmuxSessions::handle(ctx), |me, _, event, ctx| {
            me.handle_tmux_session_event(event, ctx);
        });
        if ctx.has_singleton_model::<TmuxRemoteHosts>() {
            ctx.subscribe_to_model(&TmuxRemoteHosts::handle(ctx), |me, _, event, ctx| {
                let TmuxRemoteEvent::Changed { origin } = event;
                let active_is_session = me
                    .tabs
                    .get(me.active_tab_index)
                    .and_then(|tab| tab.pane_group.as_ref(ctx).tmux_tab().map(|t| t.origin))
                    == Some(*origin);
                if active_is_session {
                    me.update_active_session(ctx);
                }
                ctx.notify();
            });
        }
    }

    fn handle_tmux_session_event(&mut self, event: &TmuxSessionEvent, ctx: &mut ViewContext<Self>) {
        match event {
            TmuxSessionEvent::Attached { origin } => self.on_tmux_attached(*origin, ctx),
            TmuxSessionEvent::WindowsChanged { origin } => {
                if self.tmux_session_shows_tabs(*origin, ctx) {
                    self.sync_tmux_session(*origin, false, ctx);
                }
                // The status strip lists the session's windows.
                ctx.notify();
            }
            TmuxSessionEvent::ActiveWindowChanged { origin, window_id } => {
                // Follow tmux's window switches only while the user is in this session.
                let active_is_session = self
                    .tabs
                    .get(self.active_tab_index)
                    .and_then(|tab| tab.pane_group.as_ref(ctx).tmux_tab().map(|t| t.origin))
                    == Some(*origin);
                if active_is_session
                    && let Some(index) = self.tmux_tab_index(*origin, *window_id, ctx)
                {
                    self.activate_tab(index, ctx);
                }
            }
            TmuxSessionEvent::ActivePaneChanged {
                origin,
                window_id,
                pane_id,
            } => {
                if let Some(index) = self.tmux_tab_index(*origin, *window_id, ctx) {
                    let pane_group = self.tabs[index].pane_group.clone();
                    pane_group.update(ctx, |pane_group, ctx| {
                        pane_group.focus_tmux_pane(*pane_id, ctx)
                    });
                }
            }
            TmuxSessionEvent::StatusChanged { .. } => ctx.notify(),
            TmuxSessionEvent::Ended { origin, line } => {
                self.end_tmux_session(*origin, line.as_deref(), ctx)
            }
            TmuxSessionEvent::Focus { origin } => self.focus_tmux_session(*origin, ctx),
            TmuxSessionEvent::Notice { origin, line } => {
                self.write_tmux_origin_line(*origin, line, ctx)
            }
            TmuxSessionEvent::ReattachReady { origin } => self.tmux_reattach(*origin, ctx),
            TmuxSessionEvent::RepliesReady => {}
        }
    }

    /// Tells tmux which window and pane the user is looking at in the active tab, so tmux
    /// bindings act on them. Changes that came from tmux find tmux already agreeing and send
    /// nothing.
    pub(super) fn sync_tmux_focus(&mut self, ctx: &mut ViewContext<Self>) {
        if !ctx.has_singleton_model::<TmuxSessions>() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active_tab_index) else {
            return;
        };
        let pane_group = tab.pane_group.as_ref(ctx);
        let Some(tmux_tab) = pane_group.tmux_tab() else {
            return;
        };
        let (origin, window_id) = (tmux_tab.origin, tmux_tab.window_id);
        let pane_id = pane_group
            .focused_tmux_pane(ctx)
            .filter(|(pane_origin, _)| *pane_origin == origin)
            .map(|(_, pane_id)| pane_id);
        if !self.tmux_session_shows_tabs(origin, ctx) {
            return;
        }
        TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
            sessions.sync_focus(origin, window_id, pane_id, ctx);
        });
    }

    fn tmux_session_shows_tabs(&self, origin: TmuxOriginId, ctx: &ViewContext<Self>) -> bool {
        TmuxSessions::as_ref(ctx)
            .session(origin)
            .is_some_and(|session| {
                !session.is_duplicate()
                    && matches!(
                        session.status,
                        SessionStatus::Attached | SessionStatus::NotResponding
                    )
            })
    }

    /// The tab whose shell started `origin`'s session.
    pub(crate) fn tmux_origin_tab_index(
        &self,
        origin: TmuxOriginId,
        ctx: &ViewContext<Self>,
    ) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            let pane_group = tab.pane_group.as_ref(ctx);
            !pane_group.is_tmux_tab() && pane_group.tmux_origin_view(origin, ctx).is_some()
        })
    }

    /// `origin`'s tmux tabs in tab order, with the window each shows.
    fn tmux_tabs_of(
        &self,
        origin: TmuxOriginId,
        ctx: &ViewContext<Self>,
    ) -> Vec<(usize, WindowId)> {
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let tmux = tab.pane_group.as_ref(ctx).tmux_tab()?;
                (tmux.origin == origin).then_some((index, tmux.window_id))
            })
            .collect()
    }

    fn tmux_tab_index(
        &self,
        origin: TmuxOriginId,
        window_id: WindowId,
        ctx: &ViewContext<Self>,
    ) -> Option<usize> {
        tab_for_window(
            self.tabs.iter().enumerate().map(|(index, tab)| {
                let shown = tab
                    .pane_group
                    .as_ref(ctx)
                    .tmux_tab()
                    .map(|tmux| (tmux.origin, tmux.window_id));
                (index, shown)
            }),
            origin,
            window_id,
        )
    }

    /// Activates the tab showing `origin`'s `window_id`, as a click in the tmux status strip
    /// asks. Tab activation tells tmux about the switch, like cmd+N.
    pub(super) fn activate_tmux_window(
        &mut self,
        origin: TmuxOriginId,
        window_id: WindowId,
        ctx: &mut ViewContext<Self>,
    ) {
        if let Some(index) = self.tmux_tab_index(origin, window_id, ctx) {
            self.activate_tab(index, ctx);
        }
    }

    /// The tmux-style status strip under the active tmux tab: `[session]` and the session's
    /// windows, current one highlighted. Shown only while the vertical tabs panel, which lists
    /// the windows otherwise, is closed.
    pub(super) fn render_tmux_status_strip(&self, app: &AppContext) -> Option<Box<dyn Element>> {
        if !TmuxSettings::as_ref(app).control_mode_enabled()
            || (FeatureFlag::VerticalTabs.is_enabled()
                && *TabSettings::as_ref(app).use_vertical_tabs
                && self.vertical_tabs_panel_open)
            || !app.has_singleton_model::<TmuxGateways>()
        {
            return None;
        }
        let tmux = self
            .tabs
            .get(self.active_tab_index)?
            .pane_group
            .as_ref(app)
            .tmux_tab()?;
        let (origin, current) = (tmux.origin, tmux.window_id);
        let model = TmuxGateways::as_ref(app).model(origin)?;
        let session = app
            .has_singleton_model::<TmuxSessions>()
            .then(|| TmuxSessions::as_ref(app).session(origin))
            .flatten()
            .map(|session| session.name.clone())
            .filter(|name| !name.is_empty())
            .or_else(|| model.session().map(|session| session.name.clone()))
            .unwrap_or_default();

        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let font_family = appearance.ui_font_family();
        let font_size = appearance.ui_font_size();
        // A rough character budget for the window width; anything past it is clipped.
        let budget = app
            .window_bounds(&self.window_id)
            .map(|bounds| {
                ((bounds.width() - 2. * STRIP_HORIZONTAL_PADDING) / (font_size * CHAR_WIDTH_EM))
                    .max(0.) as usize
            })
            .unwrap_or(usize::MAX);
        let strip = build_status_strip(&session, model.windows(), current, budget);

        let text = |label: String, color: Fill| {
            Text::new_inline(label, font_family, font_size)
                .with_color(color.into())
                .finish()
        };
        let mut row = Flex::row().with_cross_axis_alignment(CrossAxisAlignment::Center);
        row.add_child(
            Container::new(text(
                format!("[{}]", strip.session),
                theme.active_ui_text_color(),
            ))
            .with_margin_right(STRIP_ENTRY_GAP)
            .finish(),
        );
        if strip.hidden_before {
            row.add_child(
                Container::new(text(ELLIPSIS.to_string(), theme.nonactive_ui_text_color()))
                    .with_margin_right(STRIP_ENTRY_GAP)
                    .finish(),
            );
        }
        let mut mouse_states = self.tmux_status_strip_mouse_states.borrow_mut();
        mouse_states.retain(|window_id, _| model.window(*window_id).is_some());
        for (position, entry) in strip.entries.iter().enumerate() {
            let Some((origin, window_id)) = strip.click_target(origin, position) else {
                continue;
            };
            let mouse_state = mouse_states.entry(window_id).or_default().clone();
            let label = entry.label.clone();
            let is_current = entry.current;
            let item = Hoverable::new(mouse_state, |state| {
                let (background, color) = if is_current {
                    (Some(theme.accent_overlay()), theme.active_ui_text_color())
                } else if state.is_hovered() {
                    (
                        Some(theme.surface_overlay_1()),
                        theme.active_ui_text_color(),
                    )
                } else {
                    (None, theme.nonactive_ui_text_color())
                };
                let mut container = Container::new(text(label, color))
                    .with_horizontal_padding(4.)
                    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)));
                if let Some(background) = background {
                    container = container.with_background(background);
                }
                container.finish()
            })
            .on_click(move |ctx, _, _| {
                ctx.dispatch_typed_action(WorkspaceAction::ActivateTmuxWindow {
                    origin,
                    window_id,
                });
            })
            .with_cursor(Cursor::PointingHand)
            .finish();
            row.add_child(
                Container::new(item)
                    .with_margin_right(STRIP_ENTRY_GAP)
                    .finish(),
            );
        }
        if strip.hidden_after {
            row.add_child(text(ELLIPSIS.to_string(), theme.nonactive_ui_text_color()));
        }

        Some(
            Container::new(Clipped::new(row.finish()).finish())
                .with_horizontal_padding(STRIP_HORIZONTAL_PADDING)
                .with_vertical_padding(2.)
                .with_background(theme.surface_2())
                .with_border(Border::top(1.).with_border_fill(theme.outline()))
                .finish(),
        )
    }

    fn on_tmux_attached(&mut self, origin: TmuxOriginId, ctx: &mut ViewContext<Self>) {
        let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx) else {
            return;
        };
        let origin_session = self.tabs[origin_index]
            .pane_group
            .as_ref(ctx)
            .tmux_origin_view(origin, ctx)
            .and_then(|view| view.as_ref(ctx).active_session().as_ref(ctx).session(ctx));
        if ctx.has_singleton_model::<TmuxRemoteHosts>() {
            let pairing = origin_session
                .as_deref()
                .and_then(OriginSession::from_session);
            TmuxRemoteHosts::handle(ctx).update(ctx, |hosts, ctx| {
                hosts.pair(origin, pairing, ctx);
            });
        }
        let host = origin_session
            .map(|session| session.hostname().to_owned())
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| "this computer".to_owned());
        // The watchdog's second tmux client runs through the origin pane's session, which
        // reaches the server's host (U14).
        if let Some(session) = self.tabs[origin_index]
            .pane_group
            .as_ref(ctx)
            .tmux_origin_view(origin, ctx)
            .and_then(|view| view.as_ref(ctx).active_session().as_ref(ctx).session(ctx))
        {
            TmuxNativeUi::handle(ctx).update(ctx, |ui, _| ui.set_executor(origin, session));
        }
        let duplicate = TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
            sessions.register_host(origin, host, ctx)
        });
        if duplicate.is_some() {
            // Step 5: the session is already open here; its tabs get focus and this attach
            // detaches without opening a second set of tabs.
            TmuxSessions::as_ref(ctx).detach(origin, ctx);
            return;
        }
        self.sync_tmux_session(origin, true, ctx);
    }

    /// Brings `origin`'s tabs in line with its session model (KTD4). On attach the first tab
    /// takes the origin tab's place and the origin tab hides (R23).
    fn sync_tmux_session(
        &mut self,
        origin: TmuxOriginId,
        attaching: bool,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(model) = TmuxGateways::as_ref(ctx).model(origin).cloned() else {
            return;
        };
        if self.tmux_origin_tab_index(origin, ctx).is_none() {
            return;
        }
        let open: Vec<OpenTab> = self
            .tmux_tabs_of(origin, ctx)
            .into_iter()
            .map(|(index, window_id)| OpenTab {
                window_id,
                title: self.tabs[index]
                    .pane_group
                    .as_ref(ctx)
                    .tmux_tab()
                    .map(|tab| tab.title.clone())
                    .unwrap_or_default(),
            })
            .collect();
        let origin_was_active = self
            .tmux_origin_tab_index(origin, ctx)
            .is_some_and(|index| index == self.active_tab_index);

        for op in plan_window_sync(&model, &open) {
            match op {
                WindowOp::Close { window_id } => {
                    if let Some(index) = self.tmux_tab_index(origin, window_id, ctx) {
                        // tmux already closed the window: nothing is sent, nothing is undoable.
                        self.remove_tab(index, false, true, ctx);
                    }
                }
                WindowOp::Update { window_id } => {
                    let Some(index) = self.tmux_tab_index(origin, window_id, ctx) else {
                        continue;
                    };
                    let Some(window) = model.window(window_id) else {
                        continue;
                    };
                    let Some(layout) = window.layout.as_ref() else {
                        continue;
                    };
                    let layout = tab_layout(layout).clone();
                    let active_pane = window.active_pane;
                    let commands = current_commands(&model);
                    let pane_group = self.tabs[index].pane_group.clone();
                    pane_group.update(ctx, |pane_group, ctx| {
                        pane_group.apply_tmux_layout(
                            layout,
                            active_pane,
                            &|pane| commands.get(&pane).cloned(),
                            ctx,
                        );
                    });
                }
                WindowOp::Rename { window_id, title } => {
                    if let Some(index) = self.tmux_tab_index(origin, window_id, ctx) {
                        let pane_group = self.tabs[index].pane_group.clone();
                        pane_group
                            .update(ctx, |pane_group, ctx| pane_group.set_tmux_title(title, ctx));
                    }
                }
                WindowOp::Open { window_id, title } => {
                    self.open_tmux_tab(origin, &model, window_id, title, ctx);
                }
            }
        }

        let has_tabs = !self.tmux_tabs_of(origin, ctx).is_empty();
        if let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx)
            && has_tabs
            && !self.tabs[origin_index].hidden_for_tmux
        {
            self.tabs[origin_index].hidden_for_tmux = true;
        }
        if attaching && (origin_was_active || self.tabs[self.active_tab_index].hidden_for_tmux) {
            let target = model
                .active_window()
                .and_then(|window_id| self.tmux_tab_index(origin, window_id, ctx))
                .or_else(|| {
                    self.tmux_tabs_of(origin, ctx)
                        .first()
                        .map(|(index, _)| *index)
                });
            if let Some(index) = target {
                self.activate_tab(index, ctx);
            }
        } else if self.tabs[self.active_tab_index].hidden_for_tmux {
            let index = self.visible_tab_index(self.active_tab_index);
            self.activate_tab(index, ctx);
        }
        ctx.notify();
    }

    /// Opens the tab for `window_id` among `origin`'s tabs, ordered by tmux window index.
    fn open_tmux_tab(
        &mut self,
        origin: TmuxOriginId,
        model: &SessionModel,
        window_id: WindowId,
        title: String,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(window) = model.window(window_id) else {
            return;
        };
        let Some(layout) = window.layout.as_ref() else {
            return;
        };
        let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx) else {
            return;
        };
        let existing = self.tmux_tabs_of(origin, ctx);
        let indices: Vec<u32> = existing
            .iter()
            .map(|(_, id)| model.window(*id).and_then(|w| w.index).unwrap_or(u32::MAX))
            .collect();
        let offset = insertion_offset(&indices, window.index.unwrap_or(u32::MAX));
        let insert_at = match existing.get(offset) {
            Some((index, _)) => *index,
            None => existing
                .last()
                .map(|(index, _)| index + 1)
                .unwrap_or(origin_index + 1),
        };

        let layout = tab_layout(layout).clone();
        let commands = current_commands(model);
        let snapshot = layout_to_snapshot(origin, &layout, window.active_pane, &|pane| {
            commands.get(&pane).cloned()
        });
        let tips_completed = self.tips_completed.clone();
        let banner = self
            .user_default_shell_unsupported_banner_model_handle
            .clone();
        let server_api = self.server_api.clone();
        let model_event_sender = self.model_event_sender.clone();
        let pane_group: ViewHandle<PaneGroup> = ctx.add_typed_action_view(|ctx| {
            let mut pane_group = PaneGroup::new_with_panes_layout(
                tips_completed,
                banner,
                server_api,
                PanesLayout::Snapshot(Box::new(snapshot)),
                Arc::new(HashMap::new()),
                model_event_sender,
                ctx,
            );
            pane_group.bind_tmux_window(origin, window_id, title, layout, ctx);
            pane_group
        });
        ctx.subscribe_to_view(&pane_group, move |me, pane_group, event, ctx| {
            me.handle_file_tree_event(pane_group, event, ctx)
        });

        let mut tab = TabData::new(pane_group);
        tab.group_id = self.tabs[origin_index].group_id;
        let insert_at = insert_at.min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
        self.tab_mru_order
            .push(self.tabs[insert_at].pane_group.id());
        if insert_at <= self.active_tab_index && self.tabs.len() > 1 {
            self.active_tab_index += 1;
        }
    }

    /// The session ended: its tabs go away and the origin tab comes back where it was, with a
    /// line naming the session and how it ended (R24).
    fn end_tmux_session(
        &mut self,
        origin: TmuxOriginId,
        line: Option<&str>,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx) else {
            return;
        };
        let was_in_session = self.tabs[self.active_tab_index].hidden_for_tmux
            || self
                .tabs
                .get(self.active_tab_index)
                .and_then(|tab| tab.pane_group.as_ref(ctx).tmux_tab().map(|t| t.origin))
                == Some(origin)
            || origin_index == self.active_tab_index;
        self.tabs[origin_index].hidden_for_tmux = false;
        for (index, _) in self.tmux_tabs_of(origin, ctx).into_iter().rev() {
            self.remove_tab(index, false, true, ctx);
        }
        if let Some(line) = line {
            self.write_tmux_origin_line(origin, line, ctx);
        }
        if was_in_session && let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx) {
            self.activate_tab(origin_index, ctx);
        }
        ctx.notify();
    }

    /// Writes `line` into the origin pane as terminal output (R24, R29).
    fn write_tmux_origin_line(
        &mut self,
        origin: TmuxOriginId,
        line: &str,
        ctx: &mut ViewContext<Self>,
    ) {
        let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx) else {
            return;
        };
        let Some(view) = self.tabs[origin_index]
            .pane_group
            .as_ref(ctx)
            .tmux_origin_view(origin, ctx)
        else {
            return;
        };
        view.update(ctx, |view, ctx| view.write_tmux_note(line, ctx));
    }

    /// Focuses an already-attached session's tabs (step 5).
    fn focus_tmux_session(&mut self, origin: TmuxOriginId, ctx: &mut ViewContext<Self>) {
        let tabs = self.tmux_tabs_of(origin, ctx);
        let Some((first, _)) = tabs.first().copied() else {
            return;
        };
        let active_window = TmuxGateways::as_ref(ctx)
            .model(origin)
            .and_then(SessionModel::active_window);
        let index = active_window
            .and_then(|window_id| tabs.iter().find(|(_, w)| *w == window_id))
            .map(|(index, _)| *index)
            .unwrap_or(first);
        self.activate_tab(index, ctx);
        ctx.windows().show_window_and_focus_app(ctx.window_id());
    }

    /// The tmux tab at `index`'s session and window.
    fn tmux_tab_at(
        &self,
        index: usize,
        ctx: &ViewContext<Self>,
    ) -> Option<(TmuxOriginId, WindowId)> {
        let tab = self.tabs.get(index)?.pane_group.as_ref(ctx).tmux_tab()?;
        Some((tab.origin, tab.window_id))
    }

    /// cmd+T or `+` in a tmux tab creates a tmux window (R28). Returns false outside tmux tabs.
    pub(super) fn tmux_new_window(&mut self, ctx: &mut ViewContext<Self>) -> bool {
        let Some((origin, window_id)) = self.tmux_tab_at(self.active_tab_index, ctx) else {
            return false;
        };
        let launcher = crate::terminal::tmux::integration::TmuxIntegration::as_ref(ctx)
            .launcher_path(origin, ctx);
        TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
            sessions.send_command(
                origin,
                new_window_command(Some(window_id), launcher.as_deref()),
                None,
                ctx,
            );
        });
        true
    }

    /// Closing a tmux tab kills its window, asking first when something other than an idle
    /// shell runs in it (R25). The tab goes away when tmux reports the window closed; nothing
    /// goes on the undo-close stack. Returns false for other tabs.
    pub(super) fn tmux_close_tab(&mut self, index: usize, ctx: &mut ViewContext<Self>) -> bool {
        let Some((origin, window_id)) = self.tmux_tab_at(index, ctx) else {
            return false;
        };
        if TmuxSessions::as_ref(ctx).status(origin) == Some(SessionStatus::Disconnected) {
            // Nothing to kill: the session is unreachable. Closing just removes the tab.
            self.remove_tab(index, false, true, ctx);
            if self.tmux_tabs_of(origin, ctx).is_empty() {
                TmuxSessions::handle(ctx).update(ctx, |sessions, _| sessions.forget(origin));
                self.end_tmux_session(origin, None, ctx);
            }
            return true;
        }
        let pane_group = self.tabs[index].pane_group.clone();
        let running_in_warp = {
            let pane_group = pane_group.as_ref(ctx);
            pane_group
                .pane_ids()
                .any(|pane_id| pane_group.tmux_pane_is_running_command(pane_id, ctx))
        };
        TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
            sessions.send_command(
                origin,
                window_commands_query(window_id),
                Some(Box::new(move |reply, sessions, ctx| {
                    let busy = busy_commands(reply);
                    if busy.is_empty() && !running_in_warp {
                        sessions.send_command(origin, kill_window_command(window_id), None, ctx);
                        return;
                    }
                    show_kill_confirmation(
                        "Close tab?",
                        close_confirmation_text(&busy),
                        move |ctx| {
                            TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
                                sessions.send_command(
                                    origin,
                                    kill_window_command(window_id),
                                    None,
                                    ctx,
                                );
                            });
                        },
                        ctx,
                    );
                })),
                ctx,
            );
        });
        true
    }

    /// Renaming a tmux tab renames its window; `%window-renamed` updates the title. Returns
    /// false for other tabs.
    pub(super) fn tmux_rename_tab(
        &mut self,
        index: usize,
        name: &str,
        ctx: &mut ViewContext<Self>,
    ) -> bool {
        let Some((origin, window_id)) = self.tmux_tab_at(index, ctx) else {
            return false;
        };
        TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
            sessions.send_command(origin, rename_window_command(window_id, name), None, ctx);
        });
        true
    }

    /// Reattach from the Disconnected banner: reruns `tmux -CC attach -t <session>` in the
    /// origin pane, once the old control client no longer holds it.
    pub(super) fn tmux_reattach(&mut self, origin: TmuxOriginId, ctx: &mut ViewContext<Self>) {
        let Some(name) = TmuxSessions::as_ref(ctx)
            .session(origin)
            .map(|session| session.name.clone())
        else {
            return;
        };
        let Some(origin_index) = self.tmux_origin_tab_index(origin, ctx) else {
            return;
        };
        let Some(view) = self.tabs[origin_index]
            .pane_group
            .as_ref(ctx)
            .tmux_origin_view(origin, ctx)
        else {
            return;
        };
        if !TmuxSessions::handle(ctx)
            .update(ctx, |sessions, ctx| sessions.request_reattach(origin, ctx))
        {
            return;
        }
        let command = reattach_command(&name);
        view.update(ctx, |view, ctx| {
            view.execute_command_or_set_pending(&command, ctx)
        });
    }
}

// The session menu (R2).
impl Workspace {
    /// New and Attach for the new-session menu, while control mode is on and the active pane can
    /// run them (not a tmux pane).
    pub(super) fn tmux_new_session_menu_items(
        &self,
        ctx: &mut ViewContext<Self>,
    ) -> Option<Vec<MenuItem<WorkspaceAction>>> {
        if !TmuxSettings::as_ref(ctx).control_mode_enabled() {
            return None;
        }
        self.active_session_view(ctx)?
            .as_ref(ctx)
            .tmux_menu_target(ctx)?;
        Some(vec![
            MenuItemFields::new(NEW_SESSION_LABEL)
                .with_on_select_action(WorkspaceAction::TmuxSessionMenu(TmuxMenuAction::NewSession))
                .with_icon(icons::Icon::Terminal)
                .into_item(),
            MenuItemFields::new(ATTACH_LABEL)
                .with_on_select_action(WorkspaceAction::TmuxSessionMenu(
                    TmuxMenuAction::ListSessions,
                ))
                .with_icon(icons::Icon::Terminal)
                .into_item(),
        ])
    }

    /// Runs a session menu entry against the active pane: lists its host's sessions, or runs the
    /// chosen tmux command in it.
    pub(super) fn handle_tmux_menu_action(
        &mut self,
        action: TmuxMenuAction,
        ctx: &mut ViewContext<Self>,
    ) {
        if !TmuxSettings::as_ref(ctx).control_mode_enabled() {
            return;
        }
        let Some(view) = self.active_session_view(ctx) else {
            return;
        };
        let Some(target) = view.as_ref(ctx).tmux_menu_target(ctx) else {
            return;
        };
        // A session created here starts its first pane through U9's launcher (KTD7).
        let launcher = if target.session.is_local() {
            TerminalView::local_tmux_launcher(ctx)
        } else {
            None
        };
        let Some(command) = command_for_with_launcher(
            &action,
            target.socket.as_ref(),
            !target.nested,
            launcher.as_deref(),
        ) else {
            // Listing runs through the session's executor, so a Warpified SSH pane lists the
            // remote host's sessions without typing into the pane.
            let list = list_sessions_command(target.socket.as_ref());
            let session = target.session;
            let _ = ctx.spawn(
                async move {
                    session
                        .execute_command(&list, None, None, ExecuteCommandOptions::default())
                        .await
                },
                |me, result, ctx| {
                    let outcome = match result {
                        Ok(output) => list_outcome(
                            output.success(),
                            &String::from_utf8_lossy(&output.stdout),
                            &String::from_utf8_lossy(&output.stderr),
                        ),
                        Err(error) => ListOutcome::Failed(error.to_string()),
                    };
                    me.open_tmux_attach_menu(&outcome, ctx);
                },
            );
            return;
        };
        view.update(ctx, |view, ctx| {
            view.execute_command_or_set_pending(&command, ctx)
        });
    }

    fn open_tmux_attach_menu(&mut self, outcome: &ListOutcome, ctx: &mut ViewContext<Self>) {
        let items = attach_menu_entries(outcome)
            .into_iter()
            .map(|entry| match entry {
                MenuEntry::Item {
                    label,
                    action: Some(action),
                } => MenuItemFields::new(label)
                    .with_on_select_action(WorkspaceAction::TmuxSessionMenu(action))
                    .into_item(),
                MenuEntry::Item {
                    label,
                    action: None,
                } => MenuItemFields::new(label).with_disabled(true).into_item(),
                MenuEntry::Separator => MenuItem::Separator,
            })
            .collect();
        let anchor = self.keyboard_new_session_menu_anchor(ctx);
        self.open_new_session_menu_with(anchor, items, TabConfigsMenuOpenSource::Pointer, ctx);
    }
}

/// Each pane's `#{pane_current_command}` in the model.
fn current_commands(model: &SessionModel) -> HashMap<warp_terminal::tmux::PaneId, String> {
    model
        .panes()
        .filter_map(|pane| Some((pane.id, pane.current_command.clone()?)))
        .collect()
}
