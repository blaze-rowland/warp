//! A pane group that shows one tmux window (U7, KTD4).
//!
//! tmux's layout is the source of truth: `%layout-change` applies here as a diff keyed by tmux
//! pane id, reusing existing pane models. Warp's split, close and divider-drag actions in a tmux
//! tab become tmux commands instead of local changes, and the resulting layout change brings the
//! tab up to date, so a Warp-initiated split never creates a second pane.

use std::collections::HashMap;
use std::rc::Rc;

use warp_terminal::tmux::{LayoutNode, PaneId as TmuxPaneId, WindowId};
use warpui::elements::ChildView;
use warpui::{AppContext, Element, SingletonEntity as _, ViewContext, ViewHandle};

use super::tree::{Direction, PaneData, PaneFlex, PaneNode, SplitDirection};
use super::{PaneGroup, PaneGroupAction, PaneId, TerminalPane};
use crate::banner::{Banner, BannerAction, BannerTextButton, BannerTextContent};
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::layout_snapshot::{LayoutShape, layout_shape};

/// The tmux window a pane group shows.
pub struct TmuxTab {
    pub origin: TmuxOriginId,
    pub window_id: WindowId,
    /// The sanitized window name shown as the tab title.
    pub title: String,
    /// The layout last applied, and its shape.
    layout: LayoutNode,
    shape: LayoutShape,
    not_responding_banner: ViewHandle<Banner<PaneGroupAction>>,
    disconnected_banner: ViewHandle<Banner<PaneGroupAction>>,
    /// R19's shadowed-keys notice or R27's old-tmux notice (U13).
    key_notice_banner: ViewHandle<Banner<KeyNoticeAction>>,
    /// Remote features unavailable on this session's host (U11, R13, R29).
    remote_banner: ViewHandle<Banner<PaneGroupAction>>,
    /// Native tmux prompts, menus and pickers (U14).
    #[cfg(feature = "local_tty")]
    surface_view: ViewHandle<crate::terminal::tmux::native_ui::view::TmuxSurfaceView>,
    /// The popup shown as a floating pane (U14).
    #[cfg(feature = "local_tty")]
    popup: Option<TmuxPopupPane>,
    /// The pane that had focus before a native surface or popup took it.
    focus_before_overlay: Option<PaneId>,
}

/// A tmux popup's window shown as a floating pane over the tab (U14, KTD10, KTD14).
#[cfg(feature = "local_tty")]
struct TmuxPopupPane {
    window_id: WindowId,
    pane_id: TmuxPaneId,
    title: Option<String>,
    view: ViewHandle<crate::terminal::TerminalView>,
    /// Keeps the pane's terminal model fed while the popup shows.
    _manager: warpui::ModelHandle<Box<dyn crate::terminal::TerminalManager>>,
}

/// The "Show keys" link on the shadowed-keys notice (R19, R29).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyNoticeAction {
    ShowKeys,
}

/// Shown before a tmux tab's title while a key waits on tmux (R20).
const KEY_IN_FLIGHT_MARK: &str = "\u{22ef}";

impl TmuxTab {
    /// The layout last applied to the tab.
    pub fn layout(&self) -> &LayoutNode {
        &self.layout
    }
}

impl PaneGroup {
    /// The tmux window this tab shows, if it is a tmux tab.
    pub fn tmux_tab(&self) -> Option<&TmuxTab> {
        self.tmux_tab.as_ref()
    }

    pub fn is_tmux_tab(&self) -> bool {
        self.tmux_tab.is_some()
    }

    /// tmux's `#{window_index}` for this tab's window, when it is a tmux tab.
    pub fn tmux_window_index(&self, ctx: &AppContext) -> Option<u32> {
        let tab = self.tmux_tab.as_ref()?;
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::gateway::TmuxGateways;
            if ctx.has_singleton_model::<TmuxGateways>() {
                return TmuxGateways::as_ref(ctx)
                    .model(tab.origin)?
                    .window(tab.window_id)?
                    .index;
            }
        }
        let _ = (tab, ctx);
        None
    }

    /// Marks this pane group as the tab for `window_id`, whose panes were built from `layout`.
    pub(crate) fn bind_tmux_window(
        &mut self,
        origin: TmuxOriginId,
        window_id: WindowId,
        title: String,
        layout: LayoutNode,
        ctx: &mut ViewContext<Self>,
    ) {
        let not_responding_banner = ctx.add_typed_action_view(|_| {
            Banner::<PaneGroupAction>::new_without_close(BannerTextContent::plain_text(
                "tmux not responding. Waiting for the server to answer…",
            ))
        });
        let disconnected_banner = ctx.add_typed_action_view(|_| {
            Banner::<PaneGroupAction>::new_with_buttons(
                BannerTextContent::plain_text(
                    "Disconnected from tmux. This tab is read-only until you reattach.",
                ),
                vec![BannerTextButton::new(
                    "Reattach".to_owned(),
                    Rc::new(|ctx, _, _| ctx.dispatch_typed_action(PaneGroupAction::TmuxReattach)),
                )],
                false,
            )
        });
        #[cfg(feature = "local_tty")]
        ctx.subscribe_to_model(
            &crate::terminal::tmux::workspace_sync::TmuxSessions::handle(ctx),
            move |_, _, event, ctx| {
                if matches!(
                    event,
                    crate::terminal::tmux::workspace_sync::TmuxSessionEvent::StatusChanged {
                        origin: changed
                    } if *changed == origin
                ) {
                    ctx.notify();
                }
            },
        );
        let key_notice_banner = ctx.add_typed_action_view(|_| {
            Banner::<KeyNoticeAction>::new_with_buttons(
                BannerTextContent::plain_text(""),
                vec![BannerTextButton::new(
                    "Show keys".to_owned(),
                    Rc::new(|ctx, _, _| {
                        ctx.dispatch_typed_action(BannerAction::Action(KeyNoticeAction::ShowKeys))
                    }),
                )],
                true,
            )
        });
        #[cfg(feature = "local_tty")]
        {
            use crate::banner::BannerEvent;
            use crate::terminal::tmux::keys::{TmuxKeyRouter, TmuxKeyRouterEvent};
            ctx.subscribe_to_view(&key_notice_banner, move |_, _, event, ctx| {
                TmuxKeyRouter::handle(ctx).update(ctx, |router, ctx| match event {
                    BannerEvent::Dismiss(_) => router.dismiss_notice(origin, ctx),
                    BannerEvent::Action(KeyNoticeAction::ShowKeys) => {
                        router.expand_notice(origin, ctx)
                    }
                });
            });
            ctx.subscribe_to_model(
                &TmuxKeyRouter::handle(ctx),
                move |me, router, event, ctx| match event {
                    TmuxKeyRouterEvent::NoticeChanged { origin: changed } if *changed == origin => {
                        me.sync_key_notice(ctx);
                    }
                    TmuxKeyRouterEvent::InFlightChanged { origin: changed }
                        if *changed == origin =>
                    {
                        let in_flight = router.as_ref(ctx).is_key_in_flight(origin);
                        if let Some(tab) = &me.tmux_tab {
                            me.custom_title = Some(if in_flight {
                                format!("{KEY_IN_FLIGHT_MARK} {}", tab.title)
                            } else {
                                tab.title.clone()
                            });
                        }
                        ctx.notify();
                    }
                    _ => {}
                },
            );
        }
        let remote_banner = ctx.add_typed_action_view(|_| {
            Banner::<PaneGroupAction>::new(BannerTextContent::plain_text(""))
        });
        #[cfg(feature = "local_tty")]
        if ctx.has_singleton_model::<crate::terminal::tmux::remote::TmuxRemoteHosts>() {
            use crate::banner::BannerEvent;
            use crate::terminal::tmux::remote::{TmuxRemoteEvent, TmuxRemoteHosts};
            ctx.subscribe_to_view(&remote_banner, move |_, _, event, ctx| {
                if let BannerEvent::Dismiss(_) = event {
                    TmuxRemoteHosts::handle(ctx)
                        .update(ctx, |hosts, ctx| hosts.dismiss_banner(origin, ctx));
                }
            });
            ctx.subscribe_to_model(&TmuxRemoteHosts::handle(ctx), move |me, _, event, ctx| {
                let TmuxRemoteEvent::Changed { origin: changed } = event;
                if *changed == origin {
                    me.sync_remote_banner(ctx);
                }
            });
        }
        #[cfg(feature = "local_tty")]
        let surface_view = {
            use crate::terminal::tmux::native_ui::view::TmuxSurfaceView;
            use crate::terminal::tmux::native_ui::{TmuxNativeUi, TmuxNativeUiEvent};
            use crate::terminal::tmux::popup::{TmuxPopups, TmuxPopupsEvent};
            ctx.subscribe_to_model(
                &TmuxNativeUi::handle(ctx),
                move |me, _, event, ctx| match event {
                    TmuxNativeUiEvent::SurfaceChanged { origin: changed } if *changed == origin => {
                        me.sync_tmux_surface(ctx);
                    }
                    TmuxNativeUiEvent::ModeChanged { origin: changed } if *changed == origin => {
                        ctx.notify();
                    }
                    _ => {}
                },
            );
            ctx.subscribe_to_model(&TmuxPopups::handle(ctx), move |me, _, event, ctx| {
                let TmuxPopupsEvent::PopupsChanged { origin: changed } = event;
                if *changed == origin {
                    me.sync_tmux_popup(ctx);
                }
            });
            ctx.add_typed_action_view(|ctx| TmuxSurfaceView::new(origin, ctx))
        };
        self.set_title(&title, ctx);
        self.tmux_tab = Some(TmuxTab {
            origin,
            window_id,
            title,
            shape: layout_shape(&layout),
            layout,
            not_responding_banner,
            disconnected_banner,
            key_notice_banner,
            remote_banner,
            #[cfg(feature = "local_tty")]
            surface_view,
            #[cfg(feature = "local_tty")]
            popup: None,
            focus_before_overlay: None,
        });
        #[cfg(feature = "local_tty")]
        {
            self.sync_key_notice(ctx);
            self.sync_remote_banner(ctx);
            self.sync_tmux_popup(ctx);
        }
    }

    /// Copies the session's remote-features notice into the tab's banner (U11, R13, R29).
    #[cfg(feature = "local_tty")]
    fn sync_remote_banner(&mut self, ctx: &mut ViewContext<Self>) {
        use crate::terminal::tmux::remote::TmuxRemoteHosts;
        let Some(tab) = &self.tmux_tab else {
            return;
        };
        if !ctx.has_singleton_model::<TmuxRemoteHosts>() {
            return;
        }
        if let Some(text) = TmuxRemoteHosts::as_ref(ctx).banner(tab.origin) {
            tab.remote_banner.update(ctx, |banner, ctx| {
                banner.set_content(BannerTextContent::plain_text(text), ctx);
            });
        }
        ctx.notify();
    }

    /// Gives the native surface focus while it is open for a pane of this tab, and gives focus
    /// back to the pane that had it when it closes (U14).
    #[cfg(feature = "local_tty")]
    fn sync_tmux_surface(&mut self, ctx: &mut ViewContext<Self>) {
        use crate::terminal::tmux::native_ui::TmuxNativeUi;
        let Some(tab) = &self.tmux_tab else {
            return;
        };
        let surface_pane = TmuxNativeUi::as_ref(ctx)
            .surface(tab.origin)
            .map(|surface| surface.pane_id);
        let mine = surface_pane.is_some_and(|pane| self.tmux_pane_ids(ctx).contains_key(&pane));
        let surface_view = tab.surface_view.clone();
        if mine {
            if !surface_view.is_focused(ctx) {
                let focused = self.focused_pane_id(ctx);
                if let Some(tab) = &mut self.tmux_tab {
                    tab.focus_before_overlay.get_or_insert(focused);
                }
                ctx.focus(&surface_view);
            }
        } else if surface_view.is_focused(ctx) {
            self.restore_overlay_focus(ctx);
        }
        ctx.notify();
    }

    /// Shows the session's newest popup as a floating pane on the tab of the session's current
    /// window, and removes it when its window closes (U14).
    #[cfg(feature = "local_tty")]
    fn sync_tmux_popup(&mut self, ctx: &mut ViewContext<Self>) {
        use crate::terminal::tmux::gateway::TmuxGateways;
        use crate::terminal::tmux::keys::TmuxKeyRouter;
        use crate::terminal::tmux::popup::TmuxPopups;
        let Some(tab) = &self.tmux_tab else {
            return;
        };
        let origin = tab.origin;
        let current = TmuxGateways::as_ref(ctx)
            .model(origin)
            .and_then(|model| model.active_window());
        let wanted = if current == Some(tab.window_id) {
            TmuxPopups::as_ref(ctx).popups(origin).last().cloned()
        } else {
            None
        };
        let shown = tab.popup.as_ref().map(|popup| popup.window_id);
        if shown == wanted.as_ref().map(|popup| popup.window_id) {
            return;
        }
        if let Some(tab) = &mut self.tmux_tab
            && tab.popup.take().is_some()
        {
            TmuxKeyRouter::handle(ctx).update(ctx, |router, _| router.set_popup_pane(origin, None));
            self.restore_overlay_focus(ctx);
        }
        let Some(wanted) = wanted else {
            ctx.notify();
            return;
        };
        let resources = super::TerminalViewResources {
            tips_completed: self.tips_completed.clone(),
            server_api: self.server_api.clone(),
            model_event_sender: self.model_event_sender.clone(),
        };
        let view_size = Self::estimated_view_bounds(ctx).size() * 0.7;
        let Ok((view, manager)) = Self::create_tmux_pane_session(
            crate::app_state::TmuxPaneSnapshot {
                origin,
                pane_id: wanted.pane_id,
                current_command: None,
            },
            resources,
            view_size,
            self.model_event_sender.clone(),
            ctx,
        ) else {
            log::warn!("tmux: could not show popup {}", wanted.window_id);
            return;
        };
        let focused = self.focused_pane_id(ctx);
        if let Some(tab) = &mut self.tmux_tab {
            tab.focus_before_overlay.get_or_insert(focused);
            tab.popup = Some(TmuxPopupPane {
                window_id: wanted.window_id,
                pane_id: wanted.pane_id,
                title: wanted.title,
                view: view.clone(),
                _manager: manager,
            });
        }
        TmuxKeyRouter::handle(ctx).update(ctx, |router, _| {
            router.set_popup_pane(origin, Some(wanted.pane_id))
        });
        ctx.focus(&view);
        ctx.notify();
    }

    /// Focuses the pane that had focus before a surface or popup took it.
    fn restore_overlay_focus(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(previous) = self
            .tmux_tab
            .as_mut()
            .and_then(|tab| tab.focus_before_overlay.take())
        else {
            return;
        };
        if let Some(view) = self.terminal_view_from_pane_id(previous, ctx) {
            ctx.focus(&view);
        } else {
            let focused = self.focused_pane_id(ctx);
            self.focus_pane_by_id(focused, ctx);
        }
    }

    /// The overlays drawn over a tmux tab's panes: the floating popup pane and the native
    /// surface (U14).
    pub(super) fn render_tmux_overlays(&self, app: &AppContext) -> Vec<Box<dyn Element>> {
        let mut overlays = Vec::new();
        let Some(tab) = &self.tmux_tab else {
            return overlays;
        };
        #[cfg(feature = "local_tty")]
        {
            use warpui::elements::{
                Align, Border, Container, CornerRadius, Flex, ParentElement, Percentage, Radius,
                Shrinkable, Text,
            };

            use crate::appearance::Appearance;
            if let Some(popup) = &tab.popup {
                let appearance = Appearance::as_ref(app);
                let theme = appearance.theme();
                let title = popup
                    .title
                    .clone()
                    .unwrap_or_else(|| "tmux popup".to_owned());
                let body = Flex::column()
                    .with_child(
                        Container::new(
                            Text::new_inline(
                                title,
                                appearance.ui_font_family(),
                                appearance.ui_font_size(),
                            )
                            .with_color(theme.nonactive_ui_text_color().into())
                            .finish(),
                        )
                        .with_horizontal_padding(8.)
                        .with_vertical_padding(4.)
                        .finish(),
                    )
                    .with_child(Shrinkable::new(1., ChildView::new(&popup.view).finish()).finish())
                    .finish();
                let frame = Container::new(body)
                    .with_background(theme.surface_2())
                    .with_border(Border::all(1.).with_border_fill(theme.accent()))
                    .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
                    .finish();
                overlays.push(Align::new(Percentage::both(0.8, 0.6, frame).finish()).finish());
            }
            if tab.surface_view.as_ref(app).is_open(app) {
                overlays.push(ChildView::new(&tab.surface_view).finish());
            }
        }
        #[cfg(not(feature = "local_tty"))]
        let _ = (tab, app);
        overlays
    }

    /// The "tmux mode" strip for a pane of this tab in copy mode or another tmux mode (U14).
    pub(super) fn render_tmux_mode_strip(&self, app: &AppContext) -> Option<Box<dyn Element>> {
        let tab = self.tmux_tab.as_ref()?;
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::native_ui::TmuxNativeUi;
            if !app.has_singleton_model::<TmuxNativeUi>() {
                return None;
            }
            let panes = self.tmux_pane_ids(app);
            let (_, mode) = TmuxNativeUi::as_ref(app)
                .modes(tab.origin)
                .into_iter()
                .find(|(pane, _)| panes.contains_key(pane))?;
            Some(crate::terminal::tmux::native_ui::view::render_mode_strip(
                &mode, app,
            ))
        }
        #[cfg(not(feature = "local_tty"))]
        {
            let _ = (tab, app);
            None
        }
    }

    /// Copies the session's key notice text into the tab's banner.
    #[cfg(feature = "local_tty")]
    fn sync_key_notice(&mut self, ctx: &mut ViewContext<Self>) {
        use crate::terminal::tmux::keys::{KeyNoticeKind, TmuxKeyRouter};
        let Some(tab) = &self.tmux_tab else {
            return;
        };
        if !ctx.has_singleton_model::<TmuxKeyRouter>() {
            return;
        }
        let router = TmuxKeyRouter::as_ref(ctx);
        let Some(text) = router.notice_text(tab.origin, ctx) else {
            ctx.notify();
            return;
        };
        // Only the collapsed shadowed-keys notice has keys left to show.
        let show_keys = router.notice(tab.origin).is_some_and(|notice| {
            matches!(notice.kind, KeyNoticeKind::Shadowed { .. }) && !notice.expanded
        });
        tab.key_notice_banner.update(ctx, |banner, ctx| {
            banner.set_content(BannerTextContent::plain_text(text), ctx);
            banner.set_action_button_label(0, if show_keys { "Show keys" } else { "" }, ctx);
        });
        ctx.notify();
    }

    /// The focused tmux pane and the views that take its keys, for routing keys to tmux (U13).
    #[cfg(feature = "local_tty")]
    pub(crate) fn tmux_key_target(
        &self,
        app: &AppContext,
    ) -> Option<crate::terminal::tmux::key_capture::KeyTarget> {
        let tab = self.tmux_tab.as_ref()?;
        // A focused popup takes the keys, as a tmux popup does (U14).
        if let Some(popup) = &tab.popup {
            let view = &popup.view;
            let input = view.as_ref(app).input();
            let editor = input.as_ref(app).editor().id();
            let target = crate::terminal::tmux::key_capture::KeyTarget {
                origin: tab.origin,
                pane_id: popup.pane_id,
                window_id: view.window_id(app),
                view_ids: vec![view.id(), input.id(), editor],
            };
            if target.is_focused(app) {
                return Some(target);
            }
        }
        let terminal = self.terminal_session_by_id(self.focused_pane_id(app))?;
        let (origin, pane_id) = terminal.tmux_pane(app)?;
        let view = terminal.terminal_view(app);
        let input = view.as_ref(app).input();
        let editor = input.as_ref(app).editor().id();
        Some(crate::terminal::tmux::key_capture::KeyTarget {
            origin,
            pane_id,
            window_id: view.window_id(app),
            view_ids: vec![view.id(), input.id(), editor],
        })
    }

    /// Shows the window's new name.
    pub(crate) fn set_tmux_title(&mut self, title: String, ctx: &mut ViewContext<Self>) {
        self.set_title(&title, ctx);
        if let Some(tab) = &mut self.tmux_tab {
            tab.title = title;
        }
    }

    /// The Warp pane showing each tmux pane in this tab.
    pub fn tmux_pane_ids(&self, ctx: &AppContext) -> HashMap<TmuxPaneId, PaneId> {
        self.pane_contents
            .iter()
            .filter_map(|(pane_id, contents)| {
                let terminal = contents.as_any().downcast_ref::<TerminalPane>()?;
                let (_, tmux_pane) = terminal.tmux_pane(ctx)?;
                Some((tmux_pane, *pane_id))
            })
            .collect()
    }

    /// The tmux pane a Warp pane shows.
    pub fn tmux_pane_of(&self, pane_id: PaneId, ctx: &AppContext) -> Option<TmuxPaneId> {
        let terminal = self.terminal_session_by_id(pane_id)?;
        terminal.tmux_pane(ctx).map(|(_, tmux_pane)| tmux_pane)
    }

    /// The focused pane's session and tmux pane, for routing keys to tmux (U13).
    pub fn focused_tmux_pane(&self, ctx: &AppContext) -> Option<(TmuxOriginId, TmuxPaneId)> {
        let terminal = self.terminal_session_by_id(self.focused_pane_id(ctx))?;
        terminal.tmux_pane(ctx)
    }

    /// Focuses the Warp pane showing `tmux_pane` (tmux's active pane changed).
    pub(crate) fn focus_tmux_pane(&mut self, tmux_pane: TmuxPaneId, ctx: &mut ViewContext<Self>) {
        if let Some(pane_id) = self.tmux_pane_ids(ctx).get(&tmux_pane).copied()
            && self.focused_pane_id(ctx) != pane_id
        {
            self.focus_pane_by_id(pane_id, ctx);
        }
    }

    /// Applies tmux's layout for this window as a diff keyed by pane id: new panes are created,
    /// gone panes are dropped without sending anything (tmux already removed them), and existing
    /// pane models are reused when panes move. Applying the same layout twice changes nothing.
    #[cfg(feature = "local_tty")]
    pub(crate) fn apply_tmux_layout(
        &mut self,
        layout: LayoutNode,
        active_pane: Option<TmuxPaneId>,
        current_command: &dyn Fn(TmuxPaneId) -> Option<String>,
        ctx: &mut ViewContext<Self>,
    ) {
        use crate::terminal::tmux::layout_snapshot::{UpdateKind, branch_flexes, plan_pane_update};

        let Some(tab) = &self.tmux_tab else {
            return;
        };
        let origin = tab.origin;
        let mut panes = self.tmux_pane_ids(ctx);
        let mut present: Vec<TmuxPaneId> = panes.keys().copied().collect();
        present.sort();
        let next_shape = layout_shape(&layout);
        let plan = plan_pane_update(Some(&tab.shape), &present, &next_shape);

        for tmux_pane in &plan.added {
            match self.create_tmux_pane(origin, *tmux_pane, current_command(*tmux_pane), ctx) {
                Some(pane_id) => {
                    panes.insert(*tmux_pane, pane_id);
                }
                None => log::warn!("tmux: could not create a Warp pane for {tmux_pane}"),
            }
        }

        match plan.kind {
            UpdateKind::Unchanged => {}
            UpdateKind::Resize => {
                if !self.panes.root.set_branch_flexes(&branch_flexes(&layout)) {
                    self.rebuild_tmux_tree(&next_shape, &panes);
                }
            }
            UpdateKind::Restructure => self.rebuild_tmux_tree(&next_shape, &panes),
        }

        let focused = self.focused_pane_id(ctx);
        let mut removed_focus = false;
        for tmux_pane in &plan.removed {
            if let Some(pane_id) = panes.remove(tmux_pane) {
                removed_focus |= pane_id == focused;
                self.drop_tmux_pane(pane_id, ctx);
            }
        }

        if let Some(tab) = &mut self.tmux_tab {
            tab.layout = layout;
            tab.shape = next_shape;
        }
        let target = active_pane
            .and_then(|pane| panes.get(&pane).copied())
            .filter(|_| removed_focus || !plan.added.is_empty());
        if let Some(target) = target {
            self.focus_pane_by_id(target, ctx);
        }
        if plan.kind != UpdateKind::Unchanged {
            self.handle_pane_count_change(ctx);
            ctx.notify();
        }
    }

    /// Rebuilds the pane tree in `shape`'s arrangement from existing panes.
    fn rebuild_tmux_tree(&mut self, shape: &LayoutShape, panes: &HashMap<TmuxPaneId, PaneId>) {
        match build_node(shape, panes) {
            Some(root) => self.panes.replace_root(root),
            None => log::warn!("tmux: no Warp panes for the new layout"),
        }
    }

    /// Creates the Warp pane for a tmux pane that appeared in the layout.
    #[cfg(feature = "local_tty")]
    fn create_tmux_pane(
        &mut self,
        origin: TmuxOriginId,
        pane_id: TmuxPaneId,
        current_command: Option<String>,
        ctx: &mut ViewContext<Self>,
    ) -> Option<PaneId> {
        let resources = super::TerminalViewResources {
            tips_completed: self.tips_completed.clone(),
            server_api: self.server_api.clone(),
            model_event_sender: self.model_event_sender.clone(),
        };
        let view_size = Self::estimated_view_bounds(ctx).size();
        let (view, manager) = Self::create_tmux_pane_session(
            crate::app_state::TmuxPaneSnapshot {
                origin,
                pane_id,
                current_command,
            },
            resources,
            view_size,
            self.model_event_sender.clone(),
            ctx,
        )
        .ok()?;
        let pane = TerminalPane::new(
            uuid::Uuid::new_v4().as_bytes().to_vec(),
            manager,
            view,
            self.model_event_sender.clone(),
            ctx,
        );
        self.init_pane(Box::new(pane), ctx)
    }

    /// Drops a Warp pane whose tmux pane is gone. Sends nothing and skips undo-close: tmux
    /// already removed the pane.
    fn drop_tmux_pane(&mut self, pane_id: PaneId, ctx: &mut ViewContext<Self>) {
        if !self.pane_contents.contains_key(&pane_id) {
            return;
        }
        self.clean_up_pane(pane_id, ctx);
        self.pane_contents.remove(&pane_id);
        if self.panes.is_pane_in_tree(pane_id) {
            self.panes.remove(pane_id);
        }
        self.remove_from_pane_history(pane_id);
    }

    /// A Warp split in a tmux tab: asks tmux for a new pane next to `base`. The pane appears
    /// when tmux reports the new layout. Returns false outside tmux tabs.
    pub(super) fn tmux_split(
        &mut self,
        base: PaneId,
        direction: Direction,
        ctx: &mut ViewContext<Self>,
    ) -> bool {
        let Some(tab) = &self.tmux_tab else {
            return false;
        };
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::workspace_sync::{
                SplitPlacement, TmuxSessions, split_window_command,
            };
            let origin = tab.origin;
            let Some(target) = self
                .tmux_pane_of(base, ctx)
                .or_else(|| tab.layout.pane_ids().first().copied())
            else {
                return true;
            };
            let placement = match direction {
                Direction::Left => SplitPlacement::Left,
                Direction::Right => SplitPlacement::Right,
                Direction::Up => SplitPlacement::Up,
                Direction::Down => SplitPlacement::Down,
            };
            let launcher = crate::terminal::tmux::integration::TmuxIntegration::as_ref(ctx)
                .launcher_path(origin, ctx);
            TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
                sessions.send_command(
                    origin,
                    split_window_command(target, placement, launcher.as_deref()),
                    None,
                    ctx,
                );
            });
        }
        #[cfg(not(feature = "local_tty"))]
        let _ = (tab, base, direction, ctx);
        true
    }

    /// Closing a split in a tmux tab kills the tmux pane, asking first when something other
    /// than an idle shell runs in it (R25). Returns false outside tmux tabs.
    pub(super) fn tmux_close_pane(&mut self, pane_id: PaneId, ctx: &mut ViewContext<Self>) -> bool {
        let Some(tab) = &self.tmux_tab else {
            return false;
        };
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::workspace_sync::{
                TmuxSessions, busy_commands, close_confirmation_text, kill_pane_command,
                pane_command_query, show_kill_confirmation,
            };
            let origin = tab.origin;
            let Some(tmux_pane) = self.tmux_pane_of(pane_id, ctx) else {
                return true;
            };
            let running_in_warp = self.tmux_pane_is_running_command(pane_id, ctx);
            TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
                sessions.send_command(
                    origin,
                    pane_command_query(tmux_pane),
                    Some(Box::new(move |reply, sessions, ctx| {
                        let busy = busy_commands(reply);
                        if busy.is_empty() && !running_in_warp {
                            sessions.send_command(origin, kill_pane_command(tmux_pane), None, ctx);
                            return;
                        }
                        show_kill_confirmation(
                            "Close pane?",
                            close_confirmation_text(&busy),
                            move |ctx| {
                                TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
                                    sessions.send_command(
                                        origin,
                                        kill_pane_command(tmux_pane),
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
        }
        #[cfg(not(feature = "local_tty"))]
        let _ = (tab, pane_id, ctx);
        true
    }

    /// Whether the Warp pane's own model shows a running command.
    pub(crate) fn tmux_pane_is_running_command(&self, pane_id: PaneId, ctx: &AppContext) -> bool {
        self.terminal_view_from_pane_id(pane_id, ctx)
            .is_some_and(|view| {
                view.as_ref(ctx)
                    .model
                    .lock()
                    .block_list()
                    .active_block()
                    .is_active_and_long_running()
            })
    }

    /// After a divider drag or keyboard resize in a tmux tab, asks tmux for the new sizes. tmux
    /// answers with a layout change that becomes the tab's geometry.
    pub(super) fn tmux_sync_divider_sizes(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(tab) = &self.tmux_tab else {
            return;
        };
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::layout_snapshot::resize_commands;
            use crate::terminal::tmux::workspace_sync::TmuxSessions;
            let origin = tab.origin;
            let commands = resize_commands(&tab.layout, &self.panes.root.branch_flexes());
            if commands.is_empty() {
                return;
            }
            TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
                for command in commands {
                    sessions.send_command(origin, command, None, ctx);
                }
            });
        }
        #[cfg(not(feature = "local_tty"))]
        let _ = (tab, ctx);
    }

    /// The tab tooltip for a tmux tab: its session and host (R28).
    pub fn tmux_tooltip(&self, ctx: &AppContext) -> Option<String> {
        let tab = self.tmux_tab.as_ref()?;
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::workspace_sync::TmuxSessions;
            if ctx.has_singleton_model::<TmuxSessions>()
                && let Some(session) = TmuxSessions::as_ref(ctx).session(tab.origin)
            {
                return Some(session.tooltip());
            }
        }
        let _ = ctx;
        Some(format!("tmux window {}", tab.title))
    }

    /// The terminal whose PTY is `origin`, i.e. where the session was started.
    pub fn tmux_origin_view(
        &self,
        origin: TmuxOriginId,
        ctx: &AppContext,
    ) -> Option<ViewHandle<crate::terminal::TerminalView>> {
        self.pane_contents.values().find_map(|contents| {
            let terminal = contents.as_any().downcast_ref::<TerminalPane>()?;
            (terminal.tmux_origin(ctx) == Some(origin)).then(|| terminal.terminal_view(ctx))
        })
    }

    /// The banner for the session's state (R29): "tmux not responding" or "Disconnected".
    pub(super) fn render_tmux_banner(&self, app: &AppContext) -> Option<Box<dyn Element>> {
        let tab = self.tmux_tab.as_ref()?;
        #[cfg(feature = "local_tty")]
        {
            use crate::terminal::tmux::keys::TmuxKeyRouter;
            use crate::terminal::tmux::workspace_sync::{SessionStatus, TmuxSessions};
            let banner = match TmuxSessions::as_ref(app).status(tab.origin)? {
                SessionStatus::NotResponding => ChildView::new(&tab.not_responding_banner).finish(),
                SessionStatus::Disconnected => ChildView::new(&tab.disconnected_banner).finish(),
                SessionStatus::Attached
                    if app
                        .has_singleton_model::<crate::terminal::tmux::remote::TmuxRemoteHosts>(
                        )
                        && crate::terminal::tmux::remote::TmuxRemoteHosts::as_ref(app)
                            .banner(tab.origin)
                            .is_some() =>
                {
                    ChildView::new(&tab.remote_banner).finish()
                }
                SessionStatus::Attached
                    if app.has_singleton_model::<TmuxKeyRouter>()
                        && TmuxKeyRouter::as_ref(app).notice(tab.origin).is_some() =>
                {
                    ChildView::new(&tab.key_notice_banner).finish()
                }
                SessionStatus::Starting | SessionStatus::Attached => return None,
            };
            Some(banner)
        }
        #[cfg(not(feature = "local_tty"))]
        {
            let _ = (tab, app);
            None
        }
    }

    /// Banner views owned by the tab, for [`warpui::View::child_view_ids`].
    pub(super) fn tmux_banner_ids(&self) -> Vec<warpui::EntityId> {
        self.tmux_tab
            .as_ref()
            .map(|tab| {
                #[allow(unused_mut)]
                let mut ids = vec![
                    tab.not_responding_banner.id(),
                    tab.disconnected_banner.id(),
                    tab.key_notice_banner.id(),
                    tab.remote_banner.id(),
                ];
                #[cfg(feature = "local_tty")]
                {
                    ids.push(tab.surface_view.id());
                    ids.extend(tab.popup.as_ref().map(|popup| popup.view.id()));
                }
                ids
            })
            .unwrap_or_default()
    }
}

/// The pane tree for `shape`, from the Warp panes in `panes`. Panes without a Warp pane are left
/// out.
fn build_node(shape: &LayoutShape, panes: &HashMap<TmuxPaneId, PaneId>) -> Option<PaneNode> {
    match shape {
        LayoutShape::Leaf(tmux_pane) => panes.get(tmux_pane).map(|id| PaneNode::Leaf(*id)),
        LayoutShape::Split {
            horizontal,
            children,
        } => {
            let nodes: Vec<(PaneFlex, PaneNode)> = children
                .iter()
                .filter_map(|(flex, child)| Some((PaneFlex(*flex), build_node(child, panes)?)))
                .collect();
            if nodes.is_empty() {
                return None;
            }
            let axis = if *horizontal {
                SplitDirection::Horizontal
            } else {
                SplitDirection::Vertical
            };
            let len = nodes.len();
            Some(PaneData::new_branch(axis, nodes, len).root)
        }
    }
}
