use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};

use settings::Setting;
use warp_core::features::FeatureFlag;
use warp_terminal::local_tty::mio_channel;
use warp_terminal::local_tty::tmux_gateway::{Gateway, GatewayEvent, RefusalReason, TmuxRequest};
use warp_terminal::tmux::ClientEvent;
use warpui::{App, ModelHandle, SingletonEntity};

use super::{TmuxGatewayEvent, TmuxGateways, starts_inside_tmux};
use crate::terminal::tmux::settings::TmuxSettings;
use crate::test_util::settings::initialize_settings_for_tests;

fn set_control_mode(app: &mut App, value: bool) {
    TmuxSettings::handle(app).update(app, |settings, ctx| {
        settings.tmux_control_mode.set_value(value, ctx).unwrap();
    });
}

fn gateways(app: &mut App) -> ModelHandle<TmuxGateways> {
    initialize_settings_for_tests(app);
    app.add_singleton_model(TmuxGateways::new)
}

fn env(tmux: &str) -> HashMap<OsString, OsString> {
    HashMap::from([(OsString::from("TMUX"), OsString::from(tmux))])
}

#[test]
fn the_switch_follows_the_master_setting_and_the_flag() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        let (tx, _rx) = mio_channel::channel();
        let (_, link) = gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        assert!(!link.switch.is_enabled());

        set_control_mode(&mut app, true);
        assert!(link.switch.is_enabled());
        gateways.read(&app, |g, _| assert!(g.is_enabled()));

        set_control_mode(&mut app, false);
        assert!(!link.switch.is_enabled());
    });
}

#[test]
fn the_switch_stays_off_without_the_feature_flag() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(false);
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        set_control_mode(&mut app, true);
        gateways.read(&app, |g, _| assert!(!g.is_enabled()));
    });
}

#[test]
fn tmux_in_the_shell_environment_marks_the_origin_nested() {
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        let (tx, _rx) = mio_channel::channel();
        let (nested, nested_link) = gateways.update(&mut app, |g, _| {
            g.link_for_new_origin(tx.clone(), &env("/tmp/tmux-501/default,4242,0"))
        });
        let (plain, plain_link) =
            gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        assert_ne!(nested, plain);
        assert_eq!(
            nested_link.origin.nested_reason(),
            Some(RefusalReason::TmuxEnvSet)
        );
        assert_eq!(plain_link.origin.nested_reason(), None);

        // The app can mark an origin that is itself a tmux pane.
        gateways.read(&app, |g, _| {
            g.origin_flags(plain).unwrap().set_origin_is_tmux_pane(true)
        });
        assert_eq!(
            plain_link.origin.nested_reason(),
            Some(RefusalReason::OriginIsTmuxPane)
        );
    });
}

#[test]
fn only_the_spawn_environment_counts_as_inside_tmux() {
    let set = Some(OsStr::new("/tmp/tmux-501/default,1,0"));
    let empty = Some(OsStr::new(""));
    assert!(starts_inside_tmux(set));
    assert!(!starts_inside_tmux(empty));
    // Warp's own TMUX (when launched from a tmux pane) is stripped from shells, so it no longer
    // makes a fresh shell look nested.
    assert!(!starts_inside_tmux(None));
}

#[test]
fn origins_are_forgotten_when_their_event_loop_ends() {
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        let (tx, _rx) = mio_channel::channel();
        let (first, link) =
            gateways.update(&mut app, |g, _| g.link_for_new_origin(tx.clone(), &env("")));
        drop(link);
        gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        gateways.read(&app, |g, _| {
            assert!(g.origin_flags(first).is_none());
            assert!(g.request_sender(first).is_none());
        });
    });
}

#[test]
fn gateway_events_reach_subscribers_on_the_main_thread() {
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        let (tx, rx) = mio_channel::channel();
        let (origin, link) = gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        link.switch.set_enabled(true);

        let (events_tx, events_rx) = async_channel::unbounded();
        app.update(|ctx| {
            ctx.subscribe_to_model(&gateways, move |_, event: &TmuxGatewayEvent, _| {
                let _ = events_tx.try_send(event.clone());
            });
        });

        // Drive a real gateway the way the PTY thread would.
        link.origin.arm();
        let mut gateway = Gateway::new(link);
        let mut writes = VecDeque::new();
        gateway.route(b"\x1bP1000p", &mut writes);
        gateway.transport_closed();

        let mut received = Vec::new();
        while received.len() < 3 {
            received.push(events_rx.recv().await.expect("gateway event"));
        }
        assert!(matches!(
            &received[..],
            [
                TmuxGatewayEvent::Starting { origin: a },
                TmuxGatewayEvent::Client {
                    origin: b,
                    event: ClientEvent::TransportLost
                },
                TmuxGatewayEvent::Ended { origin: c },
            ] if *a == origin && *b == origin && *c == origin
        ));

        // Requests for the origin go to its event loop's channel.
        let sender = gateways
            .read(&app, |g, _| g.request_sender(origin))
            .unwrap();
        assert!(sender.send(TmuxRequest::Detach));
        assert!(matches!(
            rx.try_recv(),
            Ok(warp_terminal::writeable_pty::Message::Tmux(_))
        ));
    });
}

#[test]
fn turning_the_master_setting_off_detaches_every_running_session() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        set_control_mode(&mut app, true);
        let (tx, rx) = mio_channel::channel();
        let (origin, _link) = gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        let (idle_tx, idle_rx) = mio_channel::channel();
        let _idle = gateways.update(&mut app, |g, _| g.link_for_new_origin(idle_tx, &env("")));
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
        });
        gateways.read(&app, |g, _| {
            assert!(g.is_in_session(origin));
            assert_eq!(g.origins_in_session(), vec![origin]);
        });

        set_control_mode(&mut app, false);
        assert!(matches!(
            rx.try_recv(),
            Ok(warp_terminal::writeable_pty::Message::Tmux(
                TmuxRequest::Detach
            ))
        ));
        // An origin without a session gets nothing.
        assert!(idle_rx.try_recv().is_err());
    });
}

#[test]
fn the_session_model_is_kept_while_the_session_runs() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        let (tx, _rx) = mio_channel::channel();
        let (origin, _link) = gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        let mut model = warp_terminal::tmux::SessionModel::default();
        model.set_session(warp_terminal::tmux::SessionId(0), "work".to_owned());
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
            g.deliver_for_test(origin, GatewayEvent::Model(Box::new(model)), ctx);
        });
        gateways.read(&app, |g, _| {
            let name = g
                .model(origin)
                .and_then(|m| m.session())
                .map(|s| s.name.clone());
            assert_eq!(name.as_deref(), Some("work"));
        });
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(origin, GatewayEvent::Ended, ctx)
        });
        gateways.read(&app, |g, _| {
            assert!(g.model(origin).is_none());
            assert!(!g.is_in_session(origin));
        });
    });
}

#[test]
fn an_unarmed_entry_asks_and_the_answer_goes_to_the_origin() {
    App::test((), |mut app| async move {
        let gateways = gateways(&mut app);
        let (tx, rx) = mio_channel::channel();
        let (origin, link) = gateways.update(&mut app, |g, _| g.link_for_new_origin(tx, &env("")));
        link.switch.set_enabled(true);
        let (events_tx, events_rx) = async_channel::unbounded();
        app.update(|ctx| {
            ctx.subscribe_to_model(&gateways, move |_, event: &TmuxGatewayEvent, _| {
                let _ = events_tx.try_send(event.clone());
            });
        });

        let mut gateway = Gateway::new(link);
        let mut writes = VecDeque::new();
        gateway.route(b"\x1bP1000p", &mut writes);
        assert!(writes.is_empty());
        assert!(matches!(
            events_rx.recv().await.expect("gateway event"),
            TmuxGatewayEvent::ConfirmPending { origin: o } if o == origin
        ));
        gateways.read(&app, |g, _| assert!(!g.is_in_session(origin)));

        gateways.read(&app, |g, _| g.answer_confirmation(origin, false));
        assert!(matches!(
            rx.try_recv(),
            Ok(warp_terminal::writeable_pty::Message::Tmux(
                TmuxRequest::ConfirmDismiss
            ))
        ));
        gateways.read(&app, |g, _| g.answer_confirmation(origin, true));
        assert!(matches!(
            rx.try_recv(),
            Ok(warp_terminal::writeable_pty::Message::Tmux(
                TmuxRequest::ConfirmOpen
            ))
        ));
    });
}
