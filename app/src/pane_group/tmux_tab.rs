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
use crate::banner::{Banner, BannerTextButton, BannerTextContent};
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
}

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
        self.set_title(&title, ctx);
        self.tmux_tab = Some(TmuxTab {
            origin,
            window_id,
            title,
            shape: layout_shape(&layout),
            layout,
            not_responding_banner,
            disconnected_banner,
        });
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
        use crate::terminal::tmux::layout_snapshot::{
            UpdateKind, branch_flexes, plan_pane_update,
        };

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
            TmuxSessions::handle(ctx).update(ctx, |sessions, ctx| {
                sessions.send_command(origin, split_window_command(target, placement), None, ctx);
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
            use crate::terminal::tmux::workspace_sync::{SessionStatus, TmuxSessions};
            let banner = match TmuxSessions::as_ref(app).status(tab.origin)? {
                SessionStatus::NotResponding => &tab.not_responding_banner,
                SessionStatus::Disconnected => &tab.disconnected_banner,
                SessionStatus::Starting | SessionStatus::Attached => return None,
            };
            Some(ChildView::new(banner).finish())
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
            .map(|tab| vec![tab.not_responding_banner.id(), tab.disconnected_banner.id()])
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
