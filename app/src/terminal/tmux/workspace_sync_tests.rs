use std::cell::RefCell;
use std::rc::Rc;

use warp_core::features::FeatureFlag;
use warp_terminal::local_tty::mio_channel;
use warp_terminal::local_tty::tmux_gateway::{GatewayEvent, RefusalReason, TmuxRequest};
use warp_terminal::tmux::model::{PaneRow, WindowRow};
use warp_terminal::tmux::{
    ClientEvent, CommandReply, PaneId, SessionId, SessionModel, WindowFlags, WindowId,
    layout_checksum, parse_layout_change,
};
use warp_terminal::writeable_pty::Message;
use warpui::{App, ModelHandle};

use super::*;
use crate::terminal::tmux::gateway::TmuxGateways;
use crate::test_util::settings::initialize_settings_for_tests;

fn layout(body: &str) -> String {
    format!("{:04x},{body}", layout_checksum(body))
}

fn window_row(id: u32, index: u32, name: &str, body: &str) -> WindowRow {
    WindowRow {
        id: WindowId(id),
        index,
        active: index == 0,
        layout: Some(parse_layout_change(&layout(body), None, &WindowFlags::new("")).unwrap()),
        name: name.to_owned(),
    }
}

fn pane_row(id: u32, window: u32, command: &str) -> PaneRow {
    PaneRow {
        id: PaneId(id),
        window_id: WindowId(window),
        active: true,
        alternate_on: false,
        mode: None,
        width: 80,
        height: 24,
        current_command: Some(command.to_owned()),
    }
}

/// A session named `work` with two windows: `@1` (index 0, pane %1) and `@2` (index 1, panes
/// %2 and %3).
fn two_window_model() -> SessionModel {
    let mut model = SessionModel::default();
    model.set_session(SessionId(0), "work".to_owned());
    model.replace_windows(vec![
        window_row(1, 0, "editor", "80x24,0,0,1"),
        window_row(2, 1, "logs", "80x24,0,0{40x24,0,0,2,39x24,41,0,3}"),
    ]);
    model.replace_panes(vec![
        pane_row(1, 1, "zsh"),
        pane_row(2, 2, "zsh"),
        pane_row(3, 2, "htop"),
    ]);
    model
}

fn reply_with(lines: Vec<Vec<u8>>) -> CommandReply {
    CommandReply {
        time: 0,
        number: 0,
        flags: 1,
        error: false,
        lines,
    }
}

fn open(window_id: u32, title: &str) -> OpenTab {
    OpenTab {
        window_id: WindowId(window_id),
        title: title.to_owned(),
    }
}

// Window planning.

#[test]
fn a_two_window_session_opens_two_tabs_in_window_order() {
    let ops = plan_window_sync(&two_window_model(), &[]);
    assert_eq!(
        ops,
        vec![
            WindowOp::Open {
                window_id: WindowId(1),
                title: "editor".to_owned()
            },
            WindowOp::Open {
                window_id: WindowId(2),
                title: "logs".to_owned()
            },
        ]
    );
}

#[test]
fn open_windows_get_updates_and_closed_windows_close() {
    let model = two_window_model();
    let ops = plan_window_sync(&model, &[open(1, "editor"), open(9, "gone")]);
    assert_eq!(
        ops,
        vec![
            WindowOp::Close {
                window_id: WindowId(9)
            },
            WindowOp::Update {
                window_id: WindowId(1)
            },
            WindowOp::Open {
                window_id: WindowId(2),
                title: "logs".to_owned()
            },
        ]
    );
}

#[test]
fn a_renamed_window_renames_its_tab_with_a_clean_title() {
    let mut model = two_window_model();
    model.rename_window(
        WindowId(1),
        "\u{1b}]0;evil\u{7}build\r\n\u{9b}31m".to_owned(),
    );
    let ops = plan_window_sync(&model, &[open(1, "editor"), open(2, "logs")]);
    assert!(ops.contains(&WindowOp::Rename {
        window_id: WindowId(1),
        title: "]0;evilbuild31m".to_owned()
    }));
}

#[test]
fn a_window_without_a_layout_waits() {
    let mut model = SessionModel::default();
    model.set_session(SessionId(0), "work".to_owned());
    model.add_window(WindowId(5));
    assert!(plan_window_sync(&model, &[]).is_empty());
}

#[test]
fn new_windows_insert_by_tmux_index() {
    // Open tabs show windows with indices 0, 2 and 3 in that order.
    assert_eq!(insertion_offset(&[0, 2, 3], 1), 1);
    assert_eq!(insertion_offset(&[0, 2, 3], 4), 3);
    assert_eq!(insertion_offset(&[], 0), 0);
}

// Titles.

#[test]
fn sanitized_titles_drop_control_characters_and_fall_back() {
    assert_eq!(sanitize_title("  vim\u{0}\u{7f} main.rs "), "vim main.rs");
    assert_eq!(sanitize_title("\u{1b}\u{7}"), "");
    let long = "x".repeat(500);
    assert_eq!(sanitize_title(&long).chars().count(), MAX_TITLE_CHARS);
    let window = TmuxWindow {
        id: WindowId(3),
        index: Some(2),
        name: Some("\u{1b}".to_owned()),
        layout: None,
        active_pane: None,
    };
    assert_eq!(window_title(&window), "2");
}

#[test]
fn the_tooltip_names_the_session_and_host() {
    assert_eq!(
        session_tooltip("work", "devbox"),
        "tmux session work on devbox"
    );
}

// Session end lines (R24).

#[test]
fn end_lines_name_the_session_and_how_it_ended() {
    assert_eq!(
        end_line("work", &SessionEnd::Exited(Some("detached".to_owned()))),
        "[detached from work]"
    );
    assert_eq!(
        end_line("work", &SessionEnd::Exited(None)),
        "[exited from work]"
    );
    assert_eq!(
        end_line(
            "work",
            &SessionEnd::Exited(Some("server exited".to_owned()))
        ),
        "[server exited: work]"
    );
    assert_eq!(
        end_line(
            "work",
            &SessionEnd::AttachFailed(Some("can't find session: nope".into()))
        ),
        "[tmux: can't find session: nope]"
    );
    assert_eq!(
        end_line("work", &SessionEnd::Disconnected),
        "[lost connection to work]"
    );
}

// Hidden-tab index mapping.

#[test]
fn cmd_n_and_tmux_window_index_select_the_same_visible_tab() {
    // Tab 0 is a plain tab, tab 1 the hidden origin, tabs 2 and 3 tmux windows 0 and 1.
    let hidden = [false, true, false, false];
    assert_eq!(visible_to_raw(&hidden, 0), Some(0));
    assert_eq!(visible_to_raw(&hidden, 1), Some(2));
    assert_eq!(visible_to_raw(&hidden, 2), Some(3));
    assert_eq!(visible_to_raw(&hidden, 3), None);
    // tmux selecting window 1 activates raw tab 3, which is what cmd+3 selects.
    assert_eq!(raw_to_visible(&hidden, 3), Some(2));
    assert_eq!(
        visible_to_raw(&hidden, raw_to_visible(&hidden, 3).unwrap()),
        Some(3)
    );
    assert_eq!(raw_to_visible(&hidden, 1), None);
}

#[test]
fn next_and_previous_tab_skip_the_hidden_origin() {
    let hidden = [false, true, false, false];
    assert_eq!(step_visible(&hidden, 0, true), Some(2));
    assert_eq!(step_visible(&hidden, 2, false), Some(0));
    assert_eq!(step_visible(&hidden, 3, true), Some(0));
    assert_eq!(step_visible(&hidden, 0, false), Some(3));
    assert_eq!(last_visible(&hidden), Some(3));
    assert_eq!(last_visible(&[false, true]), Some(0));
    assert_eq!(nearest_visible(&hidden, 1), Some(2));
    assert_eq!(nearest_visible(&[false, true], 1), Some(0));
    assert_eq!(nearest_visible(&[true], 0), None);
}

// Close confirmation (R25).

#[test]
fn only_processes_other_than_an_idle_shell_need_confirmation() {
    assert!(is_idle_shell(Some("zsh")));
    assert!(is_idle_shell(Some("-bash")));
    assert!(is_idle_shell(Some("sh")));
    assert!(is_idle_shell(None));
    assert!(!is_idle_shell(Some("htop")));

    let reply = reply_with(vec![b"zsh".to_vec(), b"htop".to_vec(), b"".to_vec()]);
    assert_eq!(busy_commands(&reply), vec!["htop".to_owned()]);
    let idle = reply_with(vec![b"fish".to_vec()]);
    assert!(busy_commands(&idle).is_empty());
    assert_eq!(
        close_confirmation_text(&["htop".to_owned(), "vim".to_owned()]),
        "htop and vim are still running. They will be stopped."
    );
}

// Command builders.

#[test]
fn warp_actions_become_tmux_commands() {
    assert_eq!(
        split_window_command(PaneId(4), SplitPlacement::Right),
        "split-window -h -t %4 -c '#{pane_current_path}'"
    );
    assert_eq!(
        split_window_command(PaneId(4), SplitPlacement::Left),
        "split-window -h -b -t %4 -c '#{pane_current_path}'"
    );
    assert_eq!(
        split_window_command(PaneId(4), SplitPlacement::Down),
        "split-window -v -t %4 -c '#{pane_current_path}'"
    );
    assert_eq!(
        split_window_command(PaneId(4), SplitPlacement::Up),
        "split-window -v -b -t %4 -c '#{pane_current_path}'"
    );
    assert_eq!(kill_pane_command(PaneId(4)), "kill-pane -t %4");
    assert_eq!(kill_window_command(WindowId(2)), "kill-window -t @2");
    assert_eq!(
        new_window_command(Some(WindowId(2))),
        "new-window -a -t @2 -c '#{pane_current_path}'"
    );
    assert_eq!(new_window_command(None), "new-window");
    assert_eq!(
        rename_window_command(WindowId(2), "a \"b\" $HOME\\x;\n"),
        "rename-window -t @2 \"a \\\"b\\\" \\$HOME\\\\x;\""
    );
    assert_eq!(select_window_command(WindowId(2)), "select-window -t @2");
    assert_eq!(select_pane_command(PaneId(3)), "select-pane -t %3");
    assert_eq!(
        window_commands_query(WindowId(2)),
        "list-panes -t @2 -F '#{pane_current_command}'"
    );
    assert_eq!(
        pane_command_query(PaneId(3)),
        "display-message -p -t %3 '#{pane_current_command}'"
    );
}

#[test]
fn the_reattach_command_names_the_session() {
    assert_eq!(reattach_command("work"), "tmux -CC attach -t 'work'");
    assert_eq!(reattach_command("it's"), "tmux -CC attach -t 'it'\\''s'");
}

// Duplicate sessions (step 5).

#[test]
fn a_session_already_attached_on_the_same_host_is_found() {
    let existing = SessionKey {
        host: "devbox".to_owned(),
        session_id: SessionId(0),
        name: "work".to_owned(),
    };
    let mut sessions = vec![(TmuxOriginId(1), existing.clone())];
    assert_eq!(
        find_duplicate(&sessions, TmuxOriginId(2), &existing),
        Some(TmuxOriginId(1))
    );
    // The same origin is never its own duplicate; another host is a different server.
    assert_eq!(find_duplicate(&sessions, TmuxOriginId(1), &existing), None);
    let other_host = SessionKey {
        host: "laptop".to_owned(),
        ..existing.clone()
    };
    assert_eq!(
        find_duplicate(&sessions, TmuxOriginId(2), &other_host),
        None
    );
    sessions.clear();
    assert_eq!(find_duplicate(&sessions, TmuxOriginId(2), &existing), None);
}

// The session registry and command tags, on the gateway's events.

fn sessions(app: &mut App) -> (ModelHandle<TmuxGateways>, ModelHandle<TmuxSessions>) {
    initialize_settings_for_tests(app);
    let gateways = app.add_singleton_model(TmuxGateways::new);
    let sessions = app.add_singleton_model(TmuxSessions::new);
    (gateways, sessions)
}

fn record_events(
    app: &mut App,
    sessions: &ModelHandle<TmuxSessions>,
) -> Rc<RefCell<Vec<TmuxSessionEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    app.update(|ctx| {
        ctx.subscribe_to_model(sessions, move |_, event, _| {
            recorded.borrow_mut().push(event.clone());
        });
    });
    events
}

fn attach(
    app: &mut App,
    gateways: &ModelHandle<TmuxGateways>,
) -> (TmuxOriginId, mio_channel::Receiver<Message>) {
    let (tx, rx) = mio_channel::channel();
    let (origin, _link) =
        gateways.update(app, |g, _| g.link_for_new_origin(tx, &Default::default()));
    gateways.update(app, |g, ctx| {
        g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
        g.deliver_for_test(
            origin,
            GatewayEvent::Model(Box::new(two_window_model())),
            ctx,
        );
        g.deliver_for_test(
            origin,
            GatewayEvent::Client(ClientEvent::Attached {
                session_id: SessionId(0),
                session_name: "work".to_owned(),
            }),
            ctx,
        );
    });
    (origin, rx)
}

#[test]
fn attaching_registers_the_session_and_asks_workspaces_to_build_tabs() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let events = record_events(&mut app, &sessions);
        let (origin, _rx) = attach(&mut app, &gateways);
        sessions.read(&app, |s, _| {
            let info = s.session(origin).expect("registered");
            assert_eq!(info.name, "work");
            assert_eq!(info.status, SessionStatus::Attached);
        });
        assert!(
            events
                .borrow()
                .contains(&TmuxSessionEvent::Attached { origin })
        );
        gateways.read(&app, |g, _| {
            assert_eq!(g.model(origin).map(|m| m.windows().count()), Some(2));
        });
    });
}

#[test]
fn not_responding_and_transport_loss_change_the_status() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let (origin, _rx) = attach(&mut app, &gateways);
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(
                origin,
                GatewayEvent::Client(ClientEvent::NotResponding {
                    command: "list-windows".to_owned(),
                }),
                ctx,
            );
        });
        sessions.read(&app, |s, _| {
            assert_eq!(s.status(origin), Some(SessionStatus::NotResponding));
        });
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(origin, GatewayEvent::Client(ClientEvent::Responding), ctx);
        });
        sessions.read(&app, |s, _| {
            assert_eq!(s.status(origin), Some(SessionStatus::Attached));
        });
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(
                origin,
                GatewayEvent::Client(ClientEvent::TransportLost),
                ctx,
            );
            g.deliver_for_test(origin, GatewayEvent::Ended, ctx);
        });
        // The tabs stay, read-only, under a Disconnected banner.
        sessions.read(&app, |s, _| {
            assert_eq!(s.status(origin), Some(SessionStatus::Disconnected));
        });
    });
}

#[test]
fn exit_ends_the_session_with_a_line_and_removes_it() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let events = record_events(&mut app, &sessions);
        let (origin, _rx) = attach(&mut app, &gateways);
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(
                origin,
                GatewayEvent::Client(ClientEvent::Exited { reason: None }),
                ctx,
            );
            g.deliver_for_test(origin, GatewayEvent::Ended, ctx);
        });
        assert!(events.borrow().contains(&TmuxSessionEvent::Ended {
            origin,
            line: Some("[exited from work]".to_owned()),
        }));
        sessions.read(&app, |s, _| assert!(s.session(origin).is_none()));
    });
}

#[test]
fn command_replies_reach_their_callback_once() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let (origin, rx) = attach(&mut app, &gateways);
        let replies = Rc::new(RefCell::new(Vec::new()));
        let recorded = replies.clone();
        let tag = sessions
            .update(&mut app, |s, ctx| {
                s.send_command(
                    origin,
                    "list-panes -t @2 -F '#{pane_current_command}'".to_owned(),
                    Some(Box::new(move |reply: &CommandReply, _sessions: &mut TmuxSessions, _ctx: &mut warpui::ModelContext<TmuxSessions>| {
                        recorded.borrow_mut().push(reply.lines.clone());
                    })),
                    ctx,
                )
            })
            .expect("sent");
        match rx.try_recv() {
            Ok(Message::Tmux(TmuxRequest::Command { tag: sent, line })) => {
                assert_eq!(sent, tag);
                assert_eq!(line, "list-panes -t @2 -F '#{pane_current_command}'");
            }
            other => panic!("unexpected {other:?}"),
        }
        let reply = reply_with(vec![b"htop".to_vec()]);
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(
                origin,
                GatewayEvent::CommandCompleted {
                    tag,
                    reply: reply.clone(),
                },
                ctx,
            );
            g.deliver_for_test(origin, GatewayEvent::CommandCompleted { tag, reply }, ctx);
        });
        assert_eq!(*replies.borrow(), vec![vec![b"htop".to_vec()]]);
    });
}

// Refusals and attach failures as lines in the origin pane (R6, R29, AE5, AE7).

fn link(
    app: &mut App,
    gateways: &ModelHandle<TmuxGateways>,
) -> (TmuxOriginId, mio_channel::Receiver<Message>) {
    let (tx, rx) = mio_channel::channel();
    let (origin, _link) =
        gateways.update(app, |g, _| g.link_for_new_origin(tx, &Default::default()));
    (origin, rx)
}

#[test]
fn an_attach_failure_ends_with_tmux_error_and_never_builds_tabs() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let events = record_events(&mut app, &sessions);
        let (origin, _rx) = link(&mut app, &gateways);
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
            g.deliver_for_test(
                origin,
                GatewayEvent::Client(ClientEvent::AttachFailed {
                    error: Some("can't find session: nope".to_owned()),
                }),
                ctx,
            );
            g.deliver_for_test(origin, GatewayEvent::Ended, ctx);
        });
        assert_eq!(
            *events.borrow(),
            vec![TmuxSessionEvent::Ended {
                origin,
                line: Some("[tmux: can't find session: nope]".to_owned()),
            }]
        );
        sessions.read(&app, |s, _| assert!(s.session(origin).is_none()));
    });
}

#[test]
fn an_unsupported_version_ends_with_the_reason_instead_of_a_detach_line() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let events = record_events(&mut app, &sessions);
        let (origin, _rx) = link(&mut app, &gateways);
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
            g.deliver_for_test(
                origin,
                GatewayEvent::Refused(RefusalReason::UnsupportedVersion {
                    version: "3.1".to_owned(),
                }),
                ctx,
            );
            g.deliver_for_test(
                origin,
                GatewayEvent::Client(ClientEvent::Exited {
                    reason: Some("detached".to_owned()),
                }),
                ctx,
            );
            g.deliver_for_test(origin, GatewayEvent::Ended, ctx);
        });
        assert_eq!(
            *events.borrow(),
            vec![TmuxSessionEvent::Ended {
                origin,
                line: Some("[tmux: Native tmux needs tmux 3.2 or newer (found 3.1)]".to_owned()),
            }]
        );
    });
}

#[test]
fn a_nested_refusal_shows_a_line_without_a_session() {
    let _flag = FeatureFlag::TmuxControlMode.override_enabled(true);
    App::test((), |mut app| async move {
        let (gateways, sessions) = sessions(&mut app);
        let events = record_events(&mut app, &sessions);
        let (origin, _rx) = link(&mut app, &gateways);
        gateways.update(&mut app, |g, ctx| {
            g.deliver_for_test(
                origin,
                GatewayEvent::Refused(RefusalReason::TmuxEnvSet),
                ctx,
            );
        });
        assert_eq!(
            *events.borrow(),
            vec![TmuxSessionEvent::Notice {
                origin,
                line: "[tmux: Nested tmux is not supported natively]".to_owned(),
            }]
        );
        sessions.read(&app, |s, _| assert!(s.session(origin).is_none()));
    });
}

#[test]
fn refusal_lines_name_the_reason() {
    assert_eq!(
        refusal_line(&RefusalReason::UnsupportedVersion {
            version: String::new()
        }),
        "[tmux: Native tmux needs tmux 3.2 or newer]"
    );
    assert_eq!(
        end_line(
            "",
            &SessionEnd::Refused("Nested tmux is not supported natively".to_owned())
        ),
        "[tmux: Nested tmux is not supported natively]"
    );
}
