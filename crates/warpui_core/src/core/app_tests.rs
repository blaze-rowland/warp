//! Tests for the pre-keymap keystroke claim hook ([`View::claims_keystroke`]) and reserved key
//! bindings ([`AppContext::set_reserved_key_bindings`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::*;
use crate::elements::{DispatchEventResult, Empty, EventHandler};
use crate::event::KeyEventDetails;
use crate::keymap::macros::*;
use crate::platform::WindowStyle;
use crate::{Element, Entity, TypedActionView, ViewContext};

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestAction(&'static str);

/// Shared, cloneable record of everything that reached the view.
#[derive(Clone, Default)]
struct Record {
    /// Keymap actions dispatched to the view.
    actions: Rc<RefCell<Vec<&'static str>>>,
    /// Raw `KeyDown` keystrokes delivered to the view's element tree.
    raw_keys: Rc<RefCell<Vec<Keystroke>>>,
    /// How many times `claims_keystroke` was asked.
    claim_queries: Rc<Cell<usize>>,
}

struct ClaimingView {
    claimed: Vec<Keystroke>,
    record: Record,
}

impl Entity for ClaimingView {
    type Event = ();
}

impl View for ClaimingView {
    fn ui_name() -> &'static str {
        "ClaimingView"
    }

    fn render(&self, _: &AppContext) -> Box<dyn Element> {
        let raw_keys = self.record.raw_keys.clone();
        EventHandler::new(Empty::new().finish())
            .on_keydown(move |_, _, keystroke| {
                raw_keys.borrow_mut().push(keystroke.clone());
                DispatchEventResult::StopPropagation
            })
            .finish()
    }

    fn claims_keystroke(&self, keystroke: &Keystroke, _: &AppContext) -> bool {
        self.record
            .claim_queries
            .set(self.record.claim_queries.get() + 1);
        self.claimed.contains(keystroke)
    }
}

impl TypedActionView for ClaimingView {
    type Action = TestAction;

    fn handle_action(&mut self, action: &TestAction, _: &mut ViewContext<Self>) {
        self.record.actions.borrow_mut().push(action.0);
    }
}

fn key(s: &str) -> Keystroke {
    Keystroke::parse(s).expect("valid keystroke")
}

fn key_down(keystroke: &str, is_composing: bool) -> Event {
    let keystroke = key(keystroke);
    Event::KeyDown {
        chars: keystroke.key.clone(),
        keystroke,
        details: KeyEventDetails::default(),
        is_composing,
    }
}

/// Opens a window whose focused root view claims `claimed`, and registers `bindings`.
fn setup(app: &mut App, claimed: &[&str], bindings: Vec<FixedBinding>) -> (WindowId, Record) {
    let record = Record::default();
    let view_record = record.clone();
    let claimed = claimed.iter().map(|k| key(k)).collect();
    let (window_id, view) = app.add_window(WindowStyle::NotStealFocus, move |_| ClaimingView {
        claimed,
        record: view_record,
    });
    app.update(|ctx| ctx.register_fixed_bindings(bindings));
    view.update(app, |_, ctx| ctx.focus_self());
    (window_id, record)
}

fn press(app: &mut App, window_id: WindowId, event: Event) -> bool {
    app.update(|ctx| {
        let presenter = ctx.presenter(window_id).expect("window has a presenter");
        ctx.simulate_window_event(event, window_id, presenter)
    })
}

fn binding(keys: &str, action: &'static str) -> FixedBinding {
    FixedBinding::new(keys, TestAction(action), id!("ClaimingView"))
}

#[test]
fn claimed_keystroke_skips_keymap_and_reaches_view_raw() {
    App::test((), |mut app| async move {
        let (window_id, record) =
            setup(&mut app, &["ctrl-h"], vec![binding("ctrl-h", "backspace")]);

        assert!(press(&mut app, window_id, key_down("ctrl-h", false)));

        assert!(record.actions.borrow().is_empty(), "keymap action fired");
        assert_eq!(*record.raw_keys.borrow(), vec![key("ctrl-h")]);
    })
}

#[test]
fn non_claiming_view_leaves_dispatch_unchanged() {
    App::test((), |mut app| async move {
        let (window_id, record) = setup(&mut app, &[], vec![binding("ctrl-h", "backspace")]);

        assert!(press(&mut app, window_id, key_down("ctrl-h", false)));
        assert_eq!(*record.actions.borrow(), vec!["backspace"]);
        assert!(record.raw_keys.borrow().is_empty());

        // An unbound key still reaches the element tree.
        press(&mut app, window_id, key_down("ctrl-j", false));
        assert_eq!(*record.raw_keys.borrow(), vec![key("ctrl-j")]);
    })
}

#[test]
fn claim_is_scoped_to_the_claimed_keystrokes() {
    App::test((), |mut app| async move {
        let (window_id, record) = setup(
            &mut app,
            &["ctrl-h"],
            vec![binding("ctrl-h", "backspace"), binding("ctrl-l", "clear")],
        );

        press(&mut app, window_id, key_down("ctrl-l", false));
        assert_eq!(*record.actions.borrow(), vec!["clear"]);
    })
}

#[test]
fn reserved_named_binding_fires_even_when_claimed() {
    App::test((), |mut app| async move {
        let (window_id, record) = setup(&mut app, &["cmd-p"], vec![]);
        app.update(|ctx| {
            ctx.register_editable_bindings([EditableBinding::new(
                "test:command_palette",
                "Command palette",
                TestAction("palette"),
            )
            .with_key_binding("cmd-p")]);
            ctx.set_reserved_key_bindings([ReservedKeyBinding::Named(
                "test:command_palette".into(),
            )]);
        });

        assert!(press(&mut app, window_id, key_down("cmd-p", false)));
        assert_eq!(*record.actions.borrow(), vec!["palette"]);
        assert!(record.raw_keys.borrow().is_empty());
        assert_eq!(
            record.claim_queries.get(),
            0,
            "reserved keys are never offered"
        );
    })
}

#[test]
fn reserved_named_binding_follows_rebinding() {
    App::test((), |mut app| async move {
        let (window_id, record) = setup(&mut app, &["cmd-p", "cmd-shift-P"], vec![]);
        app.update(|ctx| {
            ctx.register_editable_bindings([EditableBinding::new(
                "test:command_palette",
                "Command palette",
                TestAction("palette"),
            )
            .with_key_binding("cmd-p")]);
            ctx.set_reserved_key_bindings([ReservedKeyBinding::Named(
                "test:command_palette".into(),
            )]);
            ctx.set_custom_trigger(
                "test:command_palette".into(),
                Trigger::Keystrokes(vec![key("cmd-shift-P")]),
            );
        });

        press(&mut app, window_id, key_down("cmd-shift-P", false));
        assert_eq!(*record.actions.borrow(), vec!["palette"]);

        // The old keystroke is no longer reserved, so the view may claim it.
        press(&mut app, window_id, key_down("cmd-p", false));
        assert_eq!(*record.raw_keys.borrow(), vec![key("cmd-p")]);
    })
}

#[test]
fn reserved_literal_keystroke_is_never_claimed() {
    App::test((), |mut app| async move {
        let (window_id, record) = setup(&mut app, &["ctrl-shift-escape"], vec![]);
        app.update(|ctx| {
            ctx.set_reserved_key_bindings([ReservedKeyBinding::Keystroke(key(
                "ctrl-shift-escape",
            ))]);
        });

        assert!(!app.read(|ctx| ctx.is_keystroke_reserved(&key("ctrl-h"))));
        assert!(app.read(|ctx| ctx.is_keystroke_reserved(&key("ctrl-shift-escape"))));
        assert_eq!(
            app.read(|ctx| ctx.claiming_view_for_keystroke(
                window_id,
                &key("ctrl-shift-escape"),
                false
            )),
            None
        );
        assert_eq!(record.claim_queries.get(), 0);
    })
}

#[test]
fn reserved_custom_action_resolves_through_its_platform_keystroke() {
    App::test((), |mut app| async move {
        const QUIT: CustomTag = 7;
        let (window_id, _record) = setup(&mut app, &["cmd-q", "cmd-n"], vec![]);
        app.update(|ctx| {
            ctx.register_default_keystroke_triggers_for_custom_actions(|tag| {
                (tag == QUIT).then(|| key("cmd-q"))
            });
            ctx.register_fixed_bindings([FixedBinding::custom(
                QUIT,
                TestAction("quit"),
                "Quit",
                id!("ClaimingView"),
            )]);
            ctx.set_reserved_key_bindings([ReservedKeyBinding::CustomAction(QUIT)]);
        });

        let claimer = |app: &App, k: &str| {
            app.read(|ctx| ctx.claiming_view_for_keystroke(window_id, &key(k), false))
        };
        assert_eq!(claimer(&app, "cmd-q"), None);
        assert!(claimer(&app, "cmd-n").is_some());
    })
}

#[test]
fn reserved_named_custom_action_binding_resolves_through_its_platform_keystroke() {
    App::test((), |mut app| async move {
        const SETTINGS: CustomTag = 9;
        let (window_id, _record) = setup(&mut app, &["cmd-,"], vec![]);
        app.update(|ctx| {
            ctx.register_default_keystroke_triggers_for_custom_actions(|tag| {
                (tag == SETTINGS).then(|| key("cmd-,"))
            });
            ctx.register_editable_bindings([EditableBinding::new(
                "test:settings",
                "Settings",
                TestAction("settings"),
            )
            .with_custom_action(SETTINGS)]);
            ctx.set_reserved_key_bindings([ReservedKeyBinding::Named("test:settings".into())]);
        });

        assert!(app.read(|ctx| ctx.is_keystroke_reserved(&key("cmd-,"))));
        assert_eq!(
            app.read(|ctx| ctx.claiming_view_for_keystroke(window_id, &key("cmd-,"), false)),
            None
        );
    })
}

#[test]
fn ime_composition_is_never_claimed() {
    App::test((), |mut app| async move {
        let (window_id, record) =
            setup(&mut app, &["ctrl-h"], vec![binding("ctrl-h", "backspace")]);

        press(&mut app, window_id, key_down("ctrl-h", true));

        assert_eq!(
            record.claim_queries.get(),
            0,
            "claim asked during composition"
        );
        // Composition keeps today's behavior: it goes through normal keymap dispatch.
        assert_eq!(*record.actions.borrow(), vec!["backspace"]);
    })
}

#[test]
fn claim_during_pending_chord_resets_pending_state() {
    App::test((), |mut app| async move {
        let (window_id, record) = setup(
            &mut app,
            &["ctrl-h"],
            vec![
                binding("ctrl-k ctrl-x", "chord"),
                binding("ctrl-x", "single"),
                binding("ctrl-h", "backspace"),
            ],
        );

        // Starts the two-key chord.
        assert!(press(&mut app, window_id, key_down("ctrl-k", false)));
        // Claimed by the view, which must drop the pending `ctrl-k`.
        press(&mut app, window_id, key_down("ctrl-h", false));
        // Matched fresh: the single-key binding fires, not the chord.
        press(&mut app, window_id, key_down("ctrl-x", false));

        assert_eq!(*record.actions.borrow(), vec!["single"]);
        assert_eq!(*record.raw_keys.borrow(), vec![key("ctrl-h")]);
    })
}

#[test]
fn hook_is_skipped_while_key_bindings_are_disabled() {
    App::test((), |mut app| async move {
        let (window_id, record) =
            setup(&mut app, &["ctrl-h"], vec![binding("ctrl-h", "backspace")]);
        app.update(|ctx| ctx.disable_key_bindings(window_id));

        press(&mut app, window_id, key_down("ctrl-h", false));

        assert_eq!(record.claim_queries.get(), 0, "claim asked while recording");
        assert_eq!(
            app.read(|ctx| ctx.claiming_view_for_keystroke(window_id, &key("ctrl-h"), false)),
            None
        );
        // The recorder still gets the raw key, as before.
        assert_eq!(*record.raw_keys.borrow(), vec![key("ctrl-h")]);
        assert!(record.actions.borrow().is_empty());
    })
}

#[test]
fn claim_query_walks_up_the_responder_chain() {
    struct Leaf;
    impl Entity for Leaf {
        type Event = ();
    }
    impl View for Leaf {
        fn ui_name() -> &'static str {
            "Leaf"
        }
        fn render(&self, _: &AppContext) -> Box<dyn Element> {
            Empty::new().finish()
        }
    }
    impl TypedActionView for Leaf {
        type Action = ();
    }

    App::test((), |mut app| async move {
        let record = Record::default();
        let view_record = record.clone();
        let (window_id, root) = app.add_window(WindowStyle::NotStealFocus, move |_| ClaimingView {
            claimed: vec![key("ctrl-h")],
            record: view_record,
        });
        let leaf = root.update(&mut app, |_, ctx| ctx.add_typed_action_view(|_| Leaf));
        leaf.update(&mut app, |_, ctx| ctx.focus_self());

        assert_eq!(
            app.read(|ctx| ctx.claiming_view_for_keystroke(window_id, &key("ctrl-h"), false)),
            Some(root.id())
        );
        assert_eq!(
            app.read(|ctx| ctx.claiming_view_for_keystroke(window_id, &key("ctrl-j"), false)),
            None
        );
    })
}

#[test]
fn reserved_chord_reserves_its_first_keystroke() {
    App::test((), |mut app| async move {
        let (window_id, _record) = setup(&mut app, &["ctrl-b", "ctrl-x"], vec![]);
        app.update(|ctx| {
            ctx.register_editable_bindings([EditableBinding::new(
                "test:escape_hatch",
                "Escape hatch",
                TestAction("escape"),
            )
            .with_key_binding("ctrl-b ctrl-x")]);
            ctx.set_reserved_key_bindings([ReservedKeyBinding::Named("test:escape_hatch".into())]);
        });

        let claimer = |app: &App, k: &str| {
            app.read(|ctx| ctx.claiming_view_for_keystroke(window_id, &key(k), false))
        };
        assert_eq!(claimer(&app, "ctrl-b"), None);
        assert!(claimer(&app, "ctrl-x").is_some());
    })
}
