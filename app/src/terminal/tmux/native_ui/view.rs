//! Views for the native tmux surfaces: [`TmuxSurfaceView`] draws a session's open surface over
//! its tab, and [`render_mode_strip`] the "tmux mode" strip.
//!
//! The surface view takes focus while a surface is open and claims every key (except Warp's
//! reserved ones), so keys reach the surface's state machine rather than Warp's keymap or tmux.

use std::any::Any;

use pathfinder_color::ColorU;
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::Vector2F;
use warpui::accessibility::{AccessibilityContent, WarpA11yRole};
use warpui::elements::{
    Align, Border, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Empty, Flex,
    Hoverable, MainAxisSize, MouseStateHandle, ParentElement, Radius, Text,
};
use warpui::event::{DispatchedEvent, Event};
use warpui::keymap::Keystroke;
use warpui::{
    AfterLayoutContext, AppContext, Element, Entity, EntityId, EventContext, LayoutContext,
    PaintContext, SingletonEntity, SizeConstraint, TypedActionView, View, ViewContext, WindowId,
};

use super::model::{SurfaceKind, TmuxNativeUi};
use super::{MenuRow, mode_strip_text};
use crate::appearance::Appearance;
use crate::terminal::tmux::TmuxOriginId;
use crate::themes::theme::Fill;

/// Rows a surface can make clickable.
const CLICKABLE_ROWS: usize = 64;
const SURFACE_WIDTH: f32 = 460.;

#[derive(Clone, Debug)]
pub enum SurfaceAction {
    Key(Keystroke),
    Activate(usize),
}

/// Draws `origin`'s open native surface. See the module docs.
pub struct TmuxSurfaceView {
    origin: TmuxOriginId,
    view_id: EntityId,
    window_id: WindowId,
    rows: Vec<MouseStateHandle>,
}

impl TmuxSurfaceView {
    pub fn new(origin: TmuxOriginId, ctx: &mut ViewContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxNativeUi::handle(ctx), move |_, _, _, ctx| ctx.notify());
        Self {
            origin,
            view_id: ctx.view_id(),
            window_id: ctx.window_id(),
            rows: (0..CLICKABLE_ROWS).map(|_| Default::default()).collect(),
        }
    }

    /// Whether a surface is open for this view's session.
    pub fn is_open(&self, app: &AppContext) -> bool {
        TmuxNativeUi::as_ref(app).surface(self.origin).is_some()
    }

    fn text(appearance: &Appearance, text: String, color: ColorU) -> Box<dyn Element> {
        Text::new_inline(
            text,
            appearance.ui_font_family(),
            appearance.ui_font_size() * 1.1,
        )
        .with_color(color)
        .finish()
    }

    fn row(
        &self,
        index: usize,
        appearance: &Appearance,
        child: Box<dyn Element>,
        selected: bool,
        clickable: bool,
    ) -> Box<dyn Element> {
        let theme = appearance.theme();
        let background = if selected {
            theme.accent()
        } else {
            Fill::Solid(ColorU::transparent_black())
        };
        let row = Container::new(child)
            .with_horizontal_padding(10.)
            .with_vertical_padding(3.)
            .with_background(background)
            .finish();
        match self.rows.get(index) {
            Some(state) if clickable => Hoverable::new(state.clone(), |_| row)
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(SurfaceAction::Activate(index))
                })
                .finish(),
            _ => row,
        }
    }

    fn render_kind(&self, kind: &SurfaceKind, appearance: &Appearance) -> Vec<Box<dyn Element>> {
        let theme = appearance.theme();
        let text_color: ColorU = theme.active_ui_text_color().into();
        let dim: ColorU = theme.nonactive_ui_text_color().into();
        let mut children = Vec::new();
        match kind {
            SurfaceKind::Confirm(state) => {
                children.push(Self::text(appearance, state.prompt.clone(), text_color));
                children.push(Self::text(
                    appearance,
                    if state.default_yes {
                        "y or Enter to confirm, any other key to cancel".to_owned()
                    } else {
                        "y to confirm, any other key to cancel".to_owned()
                    },
                    dim,
                ));
            }
            SurfaceKind::Prompt { state, .. } => {
                for (index, field) in state.fields.iter().enumerate() {
                    let active = index == state.active;
                    let caret = if active { "\u{258f}" } else { "" };
                    let line = Flex::row()
                        .with_cross_axis_alignment(CrossAxisAlignment::Center)
                        .with_child(Self::text(appearance, format!("{} ", field.label), dim))
                        .with_child(Self::text(
                            appearance,
                            format!("{}{caret}", field.value),
                            text_color,
                        ))
                        .finish();
                    children.push(self.row(usize::MAX, appearance, line, active, false));
                }
                children.push(Self::text(
                    appearance,
                    "Enter to submit, Escape to cancel".to_owned(),
                    dim,
                ));
            }
            SurfaceKind::Menu(state) => {
                if !state.title.trim().is_empty() {
                    children.push(Self::text(appearance, state.title.trim().to_owned(), dim));
                }
                for (index, row) in state.rows.iter().enumerate() {
                    match row {
                        MenuRow::Separator => children.push(
                            Container::new(
                                ConstrainedBox::new(Empty::new().finish())
                                    .with_height(1.)
                                    .finish(),
                            )
                            .with_vertical_margin(3.)
                            .with_background(theme.outline())
                            .finish(),
                        ),
                        MenuRow::Item {
                            label,
                            key,
                            enabled,
                            ..
                        } => {
                            let color = if *enabled { text_color } else { dim };
                            let key = key.as_ref().map(|key| key.name()).unwrap_or_default();
                            let line = Flex::row()
                                .with_main_axis_size(MainAxisSize::Max)
                                .with_child(Self::text(appearance, label.clone(), color))
                                .with_child(
                                    Container::new(Self::text(appearance, key, dim))
                                        .with_margin_left(16.)
                                        .finish(),
                                )
                                .finish();
                            children.push(self.row(
                                index,
                                appearance,
                                line,
                                state.selected == Some(index),
                                *enabled,
                            ));
                        }
                    }
                }
            }
            SurfaceKind::Picker { state, .. } => {
                children.push(Self::text(appearance, state.title.clone(), dim));
                for (index, row) in state.rows().iter().enumerate() {
                    let marker = match (row.expandable, row.expanded) {
                        (true, true) => "- ",
                        (true, false) => "+ ",
                        (false, _) => "  ",
                    };
                    let label = format!("{}{marker}{}", "    ".repeat(row.depth), row.label);
                    let line = Self::text(appearance, label, text_color);
                    children.push(self.row(index, appearance, line, index == state.selected, true));
                }
            }
            SurfaceKind::Panes(state) => {
                children.push(Self::text(
                    appearance,
                    "Press a pane's number to select it".to_owned(),
                    dim,
                ));
                for (index, pane) in state.panes.iter().enumerate() {
                    children.push(Self::text(
                        appearance,
                        format!("{index}   {pane}"),
                        text_color,
                    ));
                }
            }
        }
        children
    }
}

impl Entity for TmuxSurfaceView {
    type Event = ();
}

impl TypedActionView for TmuxSurfaceView {
    type Action = SurfaceAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        let origin = self.origin;
        TmuxNativeUi::handle(ctx).update(ctx, |ui, ctx| match action {
            SurfaceAction::Key(keystroke) => ui.key(origin, keystroke, ctx),
            SurfaceAction::Activate(index) => ui.activate(origin, *index, ctx),
        });
    }
}

impl View for TmuxSurfaceView {
    fn ui_name() -> &'static str {
        "TmuxSurfaceView"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let Some(surface) = TmuxNativeUi::as_ref(app).surface(self.origin) else {
            return Empty::new().finish();
        };
        let appearance = Appearance::as_ref(app);
        let theme = appearance.theme();
        let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);
        for child in self.render_kind(&surface.kind, appearance) {
            column.add_child(Container::new(child).with_vertical_margin(2.).finish());
        }
        let panel = Container::new(column.finish())
            .with_uniform_padding(12.)
            .with_background(theme.surface_2())
            .with_border(Border::all(1.).with_border_fill(theme.outline()))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
            .finish();
        let panel = ConstrainedBox::new(panel)
            .with_max_width(SURFACE_WIDTH)
            .finish();
        KeyCatcher {
            child: Align::new(panel).finish(),
            view_id: self.view_id,
            window_id: self.window_id,
        }
        .finish()
    }

    fn claims_keystroke(&self, _keystroke: &Keystroke, app: &AppContext) -> bool {
        self.is_open(app)
    }

    fn accessibility_contents(&self, app: &AppContext) -> Option<AccessibilityContent> {
        let surface = TmuxNativeUi::as_ref(app).surface(self.origin)?;
        let (value, help, role) = match &surface.kind {
            SurfaceKind::Confirm(state) => (
                format!("tmux confirmation: {}", state.prompt),
                "Press y to confirm. Any other key cancels.".to_owned(),
                WarpA11yRole::PopoverRole,
            ),
            SurfaceKind::Prompt { state, .. } => {
                let field = state.fields.get(state.active);
                (
                    format!(
                        "tmux prompt: {} {}",
                        field.map(|field| field.label.as_str()).unwrap_or_default(),
                        field.map(|field| field.value.as_str()).unwrap_or_default()
                    ),
                    "Type a value. Enter submits, Escape cancels.".to_owned(),
                    WarpA11yRole::TextfieldRole,
                )
            }
            SurfaceKind::Menu(state) => {
                let selected = state
                    .selected
                    .and_then(|index| match state.rows.get(index) {
                        Some(MenuRow::Item { label, .. }) => Some(label.clone()),
                        _ => None,
                    });
                (
                    format!(
                        "tmux menu {}: {}",
                        state.title.trim(),
                        selected.unwrap_or_default()
                    ),
                    "Arrow keys move, Enter chooses, an item's key runs it, Escape closes."
                        .to_owned(),
                    WarpA11yRole::MenuRole,
                )
            }
            SurfaceKind::Picker { state, .. } => {
                let rows = state.rows();
                (
                    format!(
                        "tmux session picker: {}",
                        rows.get(state.selected)
                            .map(|row| row.label.clone())
                            .unwrap_or_default()
                    ),
                    "Arrow keys move, Right expands, Left collapses, Enter chooses, Escape \
                     closes."
                        .to_owned(),
                    WarpA11yRole::ListRole,
                )
            }
            SurfaceKind::Panes(state) => (
                format!("tmux pane numbers: {} panes", state.panes.len()),
                "Press a pane's number to select it.".to_owned(),
                WarpA11yRole::PopoverRole,
            ),
        };
        Some(AccessibilityContent::new(value, help, role))
    }
}

/// The strip at the top of a tab while a pane is in a tmux mode.
pub fn render_mode_strip(mode: &str, app: &AppContext) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();
    Container::new(
        Text::new_inline(
            mode_strip_text(mode),
            appearance.ui_font_family(),
            appearance.ui_font_size(),
        )
        .with_color(theme.active_ui_text_color().into())
        .finish(),
    )
    .with_horizontal_padding(16.)
    .with_vertical_padding(4.)
    .with_background(theme.surface_3())
    .finish()
}

/// Sends `KeyDown`s to the surface view while it has focus.
struct KeyCatcher {
    child: Box<dyn Element>,
    view_id: EntityId,
    window_id: WindowId,
}

impl Element for KeyCatcher {
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
        if let Event::KeyDown {
            keystroke,
            is_composing: false,
            ..
        } = event.raw_event()
            && app.focused_view_id(self.window_id) == Some(self.view_id)
            && !app.is_keystroke_reserved(keystroke)
        {
            ctx.dispatch_typed_action(SurfaceAction::Key(keystroke.clone()));
            return true;
        }
        self.child.dispatch_event(event, ctx, app)
    }
}
