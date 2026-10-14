use std::cell::RefCell;
use std::rc::Rc;

use warp_core::features::FeatureFlag;
use warp_terminal::local_tty::mio_channel;
use warp_terminal::local_tty::tmux_gateway::{GatewayEvent, GatewayLink, TmuxRequest};
use warp_terminal::writeable_pty::Message;
use warpui::{App, ModelHandle};

use super::*;
use crate::terminal::shell::ShellType;
use crate::terminal::tmux::gateway::TmuxGateways;
use crate::test_util::settings::initialize_settings_for_tests;

fn reply(lines: &[&str]) -> CommandReply {
    CommandReply {
        time: 0,
        number: 0,
        flags: 1,
        error: false,
        lines: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
    }
}

fn error_reply() -> CommandReply {
    CommandReply {
        error: true,
        ..reply(&["no server"])
    }
}

const CONTINUUM_ON: &str = "15\t/home/me/.tmux/plugins/tmux-resurrect/scripts/save.sh\t";
const CONTINUUM_OFF: &str = "\t\t";
const ONLY_WARP: &str = "1\tclient-7";
const PLAIN_CLIENT: &str = "0\t/dev/ttys004";

// New panes (U9, R7, R8).

#[test]
fn a_new_pane_a_plain_client_shares_is_offered_warpify_and_never_typed_into() {
    let facts = reply(&["%5\t0\t\t\tzsh"]);
    let plain = ["/dev/ttys004".to_owned()];
    let plan = |integrating, facts: Option<&CommandReply>, clients: Option<&[String]>| {
        new_pane_action(
            PaneId(5),
            integrating,
            facts,
            clients,
            Some("token"),
            false,
            true,
        )
    };
    assert_eq!(
        plan(false, Some(&facts), Some(&plain)),
        Some(NewPanePlan::Warpify(WarpifyAvailability::Available(
            ShellType::Zsh
        )))
    );
    // An unreadable client list counts as shared.
    assert!(!matches!(
        plan(false, Some(&facts), None),
        Some(NewPanePlan::TypeInit(_))
    ));
    // Warp alone gets the typed bootstrap.
    assert_eq!(
        plan(false, Some(&facts), Some(&[])),
        Some(NewPanePlan::TypeInit(ShellType::Zsh))
    );
    // A pane that integrated while its facts were read, or whose facts are missing, is left
    // alone.
    assert_eq!(plan(true, Some(&facts), Some(&[])), None);
    assert_eq!(plan(false, None, Some(&[])), None);
    assert_eq!(plan(false, Some(&error_reply()), Some(&[])), None);
}

#[test]
fn a_view_made_again_for_a_pane_is_not_a_new_pane() {
    let mut state = OriginState {
        listed: true,
        ..OriginState::default()
    };
    assert!(state.is_new_pane(PaneId(4)));
    state.seen.insert(PaneId(4));
    // The pane was zoomed out or moved, and its view is made again: it is not planned (or
    // typed into) a second time.
    assert!(!state.is_new_pane(PaneId(4)));
    state.existing.insert(PaneId(1), ExistingPanePlan::Off);
    assert!(!state.is_new_pane(PaneId(1)));
}

// The App model: held queues and the continuum autosave.

fn integration(app: &mut App) -> (ModelHandle<TmuxGateways>, ModelHandle<TmuxIntegration>) {
    initialize_settings_for_tests(app);
    let gateways = app.add_singleton_model(TmuxGateways::new);
    app.add_singleton_model(TmuxSessions::new);
    let integration = app.add_singleton_model(TmuxIntegration::new);
    (gateways, integration)
}

/// A running session on a new origin; its commands arrive on the returned receiver.
fn session(
    app: &mut App,
    gateways: &ModelHandle<TmuxGateways>,
) -> (TmuxOriginId, GatewayLink, mio_channel::Receiver<Message>) {
    let (tx, rx) = mio_channel::channel();
    let (origin, link) =
        gateways.update(app, |g, _| g.link_for_new_origin(tx, &Default::default()));
    gateways.update(app, |g, ctx| {
        g.deliver_for_test(origin, GatewayEvent::Starting, ctx)
    });
    (origin, link, rx)
}

fn commands(rx: &mio_channel::Receiver<Message>) -> Vec<(u64, String)> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|message| match message {
            Message::Tmux(TmuxRequest::Command { tag, line }) => Some((tag, line)),
            _ => None,
        })
        .collect()
}

fn answer(
    app: &mut App,
    gateways: &ModelHandle<TmuxGateways>,
    origin: TmuxOriginId,
    tag: u64,
    reply: CommandReply,
) {
    gateways.update(app, |g, ctx| {
        g.deliver_for_test(origin, GatewayEvent::CommandCompleted { tag, reply }, ctx)
    });
}

#[test]
fn without_a_token_each_held_queue_is_released_after_its_own_wait() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, integration) = integration(&mut app);
        let (origin, _link, _rx) = session(&mut app, &gateways);
        let connected = Rc::new(RefCell::new(Vec::new()));
        let hold = |app: &mut App, pane: u32| {
            let connected = connected.clone();
            integration.update(app, |me, ctx| {
                me.connect_when_token_known(
                    origin,
                    PaneId(pane),
                    Box::new(move |_| connected.borrow_mut().push(pane)),
                    ctx,
                )
            });
        };

        hold(&mut app, 1);
        warpui::r#async::Timer::after(Duration::from_secs(2)).await;
        hold(&mut app, 2);
        assert!(connected.borrow().is_empty(), "both wait for the token");

        warpui::r#async::Timer::after(Duration::from_millis(1500)).await;
        assert_eq!(
            *connected.borrow(),
            [1],
            "only the pane that waited TOKEN_WAIT is released"
        );
        warpui::r#async::Timer::after(Duration::from_secs(2)).await;
        assert_eq!(*connected.borrow(), [1, 2]);

        // Once the token is known, a pane is connected at once.
        integration.update(&mut app, |me, ctx| me.on_token(origin, None, ctx));
        hold(&mut app, 3);
        assert_eq!(*connected.borrow(), [1, 2, 3]);
    });
}

#[test]
fn the_token_releases_every_held_queue_and_a_late_timer_does_nothing() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, integration) = integration(&mut app);
        let (origin, _link, _rx) = session(&mut app, &gateways);
        let connected = Rc::new(RefCell::new(Vec::new()));
        for pane in [1, 2] {
            let connected = connected.clone();
            integration.update(&mut app, |me, ctx| {
                me.connect_when_token_known(
                    origin,
                    PaneId(pane),
                    Box::new(move |_| connected.borrow_mut().push(pane)),
                    ctx,
                )
            });
        }
        integration.update(&mut app, |me, ctx| {
            me.on_token(origin, Some("token".to_owned()), ctx)
        });
        let mut released = connected.borrow().clone();
        released.sort();
        assert_eq!(released, [1, 2]);
        warpui::r#async::Timer::after(TOKEN_WAIT + Duration::from_millis(200)).await;
        assert_eq!(connected.borrow().len(), 2, "each queue is connected once");
    });
}

/// Runs one autosave tick of loop `generation` and answers its two queries. Empty when the tick
/// sent nothing.
#[allow(clippy::too_many_arguments)]
fn tick(
    app: &mut App,
    gateways: &ModelHandle<TmuxGateways>,
    integration: &ModelHandle<TmuxIntegration>,
    origin: TmuxOriginId,
    rx: &mio_channel::Receiver<Message>,
    generation: u64,
    config: CommandReply,
    clients: CommandReply,
) -> Vec<String> {
    integration.update(app, |me, ctx| me.autosave_tick(origin, generation, ctx));
    let sent = commands(rx);
    if sent.is_empty() {
        return Vec::new();
    }
    let [(config_tag, config_line), (clients_tag, clients_line)] = &sent[..] else {
        panic!("a tick reads the settings and the clients: {sent:?}");
    };
    assert_eq!(config_line, CONTINUUM_QUERY);
    assert_eq!(clients_line, LIST_CLIENTS_QUERY);
    answer(app, gateways, origin, *config_tag, config);
    answer(app, gateways, origin, *clients_tag, clients);
    vec!["answered".to_owned()]
}

/// The commands sent once the replies' follow-up work ran.
async fn sent_after(rx: &mio_channel::Receiver<Message>) -> Vec<String> {
    warpui::r#async::Timer::after(Duration::from_millis(50)).await;
    commands(rx).into_iter().map(|(_, line)| line).collect()
}

#[test]
fn the_autosave_saves_only_while_warp_is_the_only_client_and_survives_failed_replies() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, integration) = integration(&mut app);
        let (origin, _link, rx) = session(&mut app, &gateways);
        integration.update(&mut app, |me, _| {
            me.origins.entry(origin).or_default().autosave = Some(7);
        });
        let running = |app: &App| {
            integration.read(app, |me, _| {
                me.origins.get(&origin).and_then(|state| state.autosave)
            })
        };
        let run_tick = |app: &mut App, generation, config, clients| {
            tick(
                app,
                &gateways,
                &integration,
                origin,
                &rx,
                generation,
                config,
                clients,
            )
        };

        // A plain client is attached: its status line runs continuum, so Warp does not.
        run_tick(
            &mut app,
            7,
            reply(&[CONTINUUM_ON]),
            reply(&[ONLY_WARP, PLAIN_CLIENT]),
        );
        let sent = sent_after(&rx).await;
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(running(&app), Some(7));

        // Warp alone: the save runs.
        run_tick(&mut app, 7, reply(&[CONTINUUM_ON]), reply(&[ONLY_WARP]));
        let sent = sent_after(&rx).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].starts_with("run-shell"), "{sent:?}");

        // A failed reply skips the tick without ending the loop.
        run_tick(&mut app, 7, error_reply(), reply(&[ONLY_WARP]));
        assert!(sent_after(&rx).await.is_empty());
        assert_eq!(running(&app), Some(7));

        // A stale loop sends nothing at all.
        assert!(run_tick(&mut app, 6, reply(&[CONTINUUM_ON]), reply(&[ONLY_WARP])).is_empty());

        // Continuum turned off ends the loop.
        run_tick(&mut app, 7, reply(&[CONTINUUM_OFF]), reply(&[ONLY_WARP]));
        assert!(sent_after(&rx).await.is_empty());
        assert_eq!(running(&app), None);
        assert!(run_tick(&mut app, 7, reply(&[CONTINUUM_ON]), reply(&[ONLY_WARP])).is_empty());
    });
}

#[test]
fn autosave_steps() {
    assert_eq!(
        autosave_step(Some(&reply(&[CONTINUUM_OFF])), &reply(&[ONLY_WARP])),
        AutosaveStep::Stop
    );
    assert_eq!(
        autosave_step(None, &reply(&[ONLY_WARP])),
        AutosaveStep::Wait,
        "a lost reply skips the tick"
    );
    assert_eq!(
        autosave_step(Some(&error_reply()), &reply(&[ONLY_WARP])),
        AutosaveStep::Wait
    );
    assert_eq!(
        autosave_step(Some(&reply(&[CONTINUUM_ON])), &error_reply()),
        AutosaveStep::Wait,
        "an unreadable client list counts as a plain client"
    );
    assert!(matches!(
        autosave_step(Some(&reply(&[CONTINUUM_ON])), &reply(&[ONLY_WARP])),
        AutosaveStep::Save(_)
    ));
}
