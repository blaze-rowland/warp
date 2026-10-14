//! Claims tmux-bound keys before Warp's keymap (U13, KTD6) and hands them to the key router.
//!
//! The pane group of a tmux tab answers [`warpui::View::claims_keystroke`] with
//! [`claims_keystroke`], so a claimed key skips Warp's keymap and reaches the element tree as a
//! plain `KeyDown`. [`TmuxKeyCapture`] wraps the pane group's content and takes that `KeyDown`
//! before any pane sees it (elements normally dispatch children first), so neither the input
//! editor nor the terminal grid acts on it. It then dispatches [`ROUTE_KEY_ACTION`], which sends
//! the key through tmux. Popup keys (U14) are claimed but left to the popup pane's terminal,
//! which writes them as typed input.

use std::any::Any;
use std::collections::HashSet;

use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::Vector2F;
use warp_terminal::tmux::{ClientKey, PaneId};
use warpui::event::{DispatchedEvent, Event};
use warpui::keymap::Keystroke;
use warpui::{
    AfterLayoutContext, AppContext, Element, EntityId, EventContext, LayoutContext, PaintContext,
    SingletonEntity as _, SizeConstraint, ViewContext, WindowId,
};

use super::TmuxOriginId;
use super::keys::{KeyRoute, TmuxKeyRouter, escape_hatch_keystroke};
use super::native_ui::TmuxNativeUi;
use crate::pane_group::PaneGroup;
use crate::util::bindings::trigger_to_keystroke;

/// Dispatched on the pane group with a [`RoutedKey`].
pub const ROUTE_KEY_ACTION: &str = "tmux_keys:route_key";
/// Dispatched on the pane group with the session's [`TmuxOriginId`].
pub const ESCAPE_HATCH_ACTION: &str = "tmux_keys:escape_hatch";

/// The focused tmux pane of a pane group and the views whose focus means "typing into it".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyTarget {
    pub origin: TmuxOriginId,
    pub pane_id: PaneId,
    pub window_id: WindowId,
    /// The terminal view, its input view and its editor.
    pub view_ids: Vec<EntityId>,
}

impl KeyTarget {
    /// Whether keys are going to this pane: its terminal or input editor has focus, not a find
    /// bar, menu or palette.
    pub fn is_focused(&self, app: &AppContext) -> bool {
        app.focused_view_id(self.window_id)
            .is_some_and(|focused| self.view_ids.contains(&focused))
    }
}

/// A claimed key on its way to tmux.
#[derive(Clone, Debug)]
pub struct RoutedKey {
    pub origin: TmuxOriginId,
    pub key: ClientKey,
}

/// Where a keystroke goes for a pane group whose focused pane is `target`.
pub fn route_for(target: Option<&KeyTarget>, keystroke: &Keystroke, app: &AppContext) -> KeyRoute {
    let Some(target) = target else {
        return KeyRoute::Warp;
    };
    if !app.has_singleton_model::<TmuxKeyRouter>() || !target.is_focused(app) {
        return KeyRoute::Warp;
    }
    TmuxKeyRouter::as_ref(app).route(target.origin, target.pane_id, keystroke, app)
}

/// [`warpui::View::claims_keystroke`] for a pane group.
pub fn claims_keystroke(
    target: Option<&KeyTarget>,
    keystroke: &Keystroke,
    app: &AppContext,
) -> bool {
    route_for(target, keystroke, app).is_claimed()
}

/// Wraps a pane group's content so claimed keys go to tmux. A no-op outside tmux tabs.
pub fn wrap(child: Box<dyn Element>, target: Option<KeyTarget>) -> Box<dyn Element> {
    match target {
        Some(target) => TmuxKeyCapture { child, target }.finish(),
        None => child,
    }
}

/// See the module docs.
pub struct TmuxKeyCapture {
    child: Box<dyn Element>,
    target: KeyTarget,
}

impl TmuxKeyCapture {
    fn capture(&self, event: &DispatchedEvent, ctx: &mut EventContext, app: &AppContext) -> bool {
        let Event::KeyDown {
            keystroke,
            is_composing: false,
            ..
        } = event.raw_event()
        else {
            return false;
        };
        if !self.target.is_focused(app) {
            return false;
        }
        if escape_hatch_keystroke(app).as_ref() == Some(keystroke) {
            ctx.dispatch_action(ESCAPE_HATCH_ACTION, self.target.origin);
            return true;
        }
        // Reserved keys reach here only when no Warp binding took them; they stay with Warp.
        if app.is_keystroke_reserved(keystroke) {
            return false;
        }
        match route_for(Some(&self.target), keystroke, app) {
            KeyRoute::Tmux(key) => {
                ctx.dispatch_action(
                    ROUTE_KEY_ACTION,
                    RoutedKey {
                        origin: self.target.origin,
                        key,
                    },
                );
                true
            }
            // Popup keys go on to the popup pane's terminal as typed input.
            KeyRoute::Popup | KeyRoute::Warp => false,
        }
    }
}

impl Element for TmuxKeyCapture {
    fn layout(
        &mut self,
        constraint: SizeConstraint,
        ctx: &mut LayoutContext,
        app: &AppContext,
    ) -> Vector2F {
        self.child.layout(constraint, ctx, app)
    }

    fn after_layout(&mut self, ctx: &mut AfterLayoutContext, app: &AppContext) {
        self.child.after_layout(ctx, app);
    }

    fn paint(&mut self, origin: Vector2F, ctx: &mut PaintContext, app: &AppContext) {
        self.child.paint(origin, ctx, app);
    }

    fn size(&self) -> Option<Vector2F> {
        self.child.size()
    }

    fn origin(&self) -> Option<warpui::elements::Point> {
        self.child.origin()
    }

    fn bounds(&self) -> Option<RectF> {
        self.child.bounds()
    }

    fn parent_data(&self) -> Option<&dyn Any> {
        self.child.parent_data()
    }

    fn dispatch_event(
        &mut self,
        event: &DispatchedEvent,
        ctx: &mut EventContext,
        app: &AppContext,
    ) -> bool {
        // Before the child: the panes must never see a key that goes to tmux.
        self.capture(event, ctx, app) || self.child.dispatch_event(event, ctx, app)
    }
}

/// Registers the pane-group actions [`TmuxKeyCapture`] dispatches.
pub fn init(app: &mut AppContext) {
    app.add_action(
        ROUTE_KEY_ACTION,
        |pane_group: &mut PaneGroup, routed: &RoutedKey, ctx: &mut ViewContext<PaneGroup>| {
            route_key(pane_group, routed, ctx);
            true
        },
    );
    app.add_action(
        ESCAPE_HATCH_ACTION,
        |_: &mut PaneGroup, _: &TmuxOriginId, ctx: &mut ViewContext<PaneGroup>| {
            TmuxKeyRouter::handle(ctx).update(ctx, |router, _| router.escape_hatch_pressed());
            true
        },
    );
}

fn route_key(pane_group: &PaneGroup, routed: &RoutedKey, ctx: &mut ViewContext<PaneGroup>) {
    let origin = routed.origin;
    let router = TmuxKeyRouter::handle(ctx);
    // U14, KTD10: a binding that opens tmux UI is drawn by Warp instead of reaching tmux.
    if let Some(command) = router.as_ref(ctx).bound_command(origin, &routed.key)
        && ctx.has_singleton_model::<TmuxNativeUi>()
        && TmuxNativeUi::handle(ctx).update(ctx, |ui, ctx| {
            ui.intercept(origin, routed.key.pane_id, &command, ctx)
        })
    {
        router.update(ctx, |router, ctx| router.key_intercepted(origin, ctx));
        return;
    }
    router.update(ctx, |router, ctx| {
        router.send_client_key(origin, routed.key.clone(), ctx);
    });
    // The first key tmux takes is when its bindings start shadowing Warp's (R19).
    if !router.as_ref(ctx).core().needs_shadow_check(origin) {
        return;
    }
    let Some(target) = pane_group.tmux_key_target(ctx) else {
        return;
    };
    // Reading key bindings walks the focused view's ancestors, which include this pane group,
    // so it cannot run while this action holds the pane group. The router runs it afterwards.
    router.update(ctx, |_, ctx| {
        let _ = ctx.spawn(async {}, move |router, _, ctx| {
            let used: HashSet<Keystroke> = target
                .view_ids
                .iter()
                .flat_map(|view_id| ctx.key_bindings_for_view(target.window_id, *view_id))
                .filter_map(|binding| trigger_to_keystroke(binding.trigger))
                .collect();
            router.check_shadow_notice(origin, |stroke| used.contains(stroke), ctx);
        });
    });
}
