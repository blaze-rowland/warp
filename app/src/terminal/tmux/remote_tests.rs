use std::path::PathBuf;
use std::sync::Arc;

use warp_core::{HostId, SessionId};
use warp_terminal::tmux::PaneId;
use warpui::{App, ModelHandle};

use super::*;
use crate::terminal::model::session::command_executor::testing::TestCommandExecutor;
use crate::terminal::model::session::command_executor::{
    ExecuteCommandOptions,
};
use crate::terminal::model::session::{BootstrapSessionType, SessionInfo};
use crate::terminal::shell::{Shell, ShellType};
use crate::terminal::tmux::integration::{
    BundleFile, LauncherBundle, NewPanePlan, PaneFacts, derive_session_id,
    install_check_commands, install_upload_commands, new_pane_plan,
};

const HOST: &str = "box";
const ORIGIN_SESSION: u64 = 7;

fn host_id() -> HostId {
    HostId::new("box-host-id".to_owned())
}

fn origin() -> TmuxOriginId {
    TmuxOriginId::new_for_test(1)
}

fn origin_session(control_master: Option<&str>) -> OriginSession {
    OriginSession {
        session_id: SessionId::from(ORIGIN_SESSION),
        hostname: HOST.to_owned(),
        control_master: control_master.map(PathBuf::from),
    }
}

fn pairing(link: ServerLink, control_master: Option<&str>) -> RemotePairing {
    RemotePairing {
        origin: origin_session(control_master),
        link,
    }
}

fn connected() -> ServerLink {
    ServerLink::Connected { host_id: host_id() }
}

fn pane_session_info(session_id: u64, hostname: &str) -> SessionInfo {
    let mut info = SessionInfo::new_for_test();
    info.session_id = SessionId::from(session_id);
    info.hostname = hostname.to_owned();
    info.session_type = BootstrapSessionType::WarpifiedRemote;
    info
}

fn is<T: 'static>(executor: &Arc<dyn CommandExecutor>) -> bool {
    executor.as_any().downcast_ref::<T>().is_some()
}

/// A pane's sessions model with one session, registered with the remote hosts.
fn add_pane(
    app: &mut App,
    hosts: &ModelHandle<TmuxRemoteHosts>,
    session_id: u64,
    hostname: &str,
) -> ModelHandle<Sessions> {
    let sessions = app.add_model(|_| {
        let mut sessions = Sessions::new_for_test();
        sessions.set_tmux_origin(origin());
        sessions.register_session_for_test(pane_session_info(session_id, hostname));
        sessions
    });
    hosts.update(app, |hosts, ctx| hosts.register_pane(origin(), &sessions, ctx));
    sessions
}

fn pane_session(app: &App, sessions: &ModelHandle<Sessions>) -> Arc<Session> {
    sessions.read(app, |sessions, _| {
        sessions.sessions().next().cloned().expect("pane has a session")
    })
}

fn remote_host_id(session: &Session) -> Option<HostId> {
    match session.session_type() {
        SessionType::WarpifiedRemote { host_id } => host_id,
        SessionType::Local => None,
    }
}

#[test]
fn two_panes_on_the_paired_host_share_the_origin_connection_and_resolve_remote_paths() {
    App::test((), |mut app| async move {
        let hosts = app.add_singleton_model(|_| TmuxRemoteHosts::default());
        let first = add_pane(&mut app, &hosts, 101, HOST);
        let second = add_pane(&mut app, &hosts, 102, HOST);

        hosts.update(&mut app, |hosts, ctx| {
            hosts.set_pairing_for_test(origin(), pairing(connected(), Some("/tmp/cm")), ctx);
        });

        for sessions in [&first, &second] {
            let session = pane_session(&app, sessions);
            // Working directories pair with the origin's host: `LocalOrRemotePath::Remote`.
            assert_eq!(remote_host_id(&session), Some(host_id()));
            let pane = hosts.read(&app, |hosts, _| hosts.pane_remote(origin(), &session));
            assert_eq!(
                pane.executor_plan(),
                ExecutorPlan::RemoteServer {
                    session_id: SessionId::from(ORIGIN_SESSION),
                    host_id: host_id(),
                    fallback: Some(PathBuf::from("/tmp/cm")),
                }
            );
            assert_eq!(
                pane.remote_server_session(),
                Some(SessionId::from(ORIGIN_SESSION))
            );
            assert_eq!(
                pane.coding_panel_state(),
                Some(CodingPanelEnablementState::RemoteSession {
                    has_remote_server: true
                })
            );
            // No manager client in this test, so the plan falls back to the origin's socket.
            assert!(is::<RemoteCommandExecutor>(&session.command_executor()));
        }
    });
}

#[test]
fn closing_panes_never_touches_the_origin_connection() {
    // A pane's own session going away (its ExitShell deregisters only that id) is not an event
    // about the origin's connection.
    let origin_session = SessionId::from(ORIGIN_SESSION);
    assert_eq!(
        LinkEvent::from_manager_event(
            &RemoteServerManagerEvent::SessionDeregistered {
                session_id: SessionId::from(101)
            },
            origin_session,
        ),
        None
    );

    App::test((), |mut app| async move {
        let hosts = app.add_singleton_model(|_| TmuxRemoteHosts::default());
        let keep = add_pane(&mut app, &hosts, 101, HOST);
        let closed = [
            add_pane(&mut app, &hosts, 102, HOST),
            add_pane(&mut app, &hosts, 103, HOST),
        ];
        hosts.update(&mut app, |hosts, ctx| {
            hosts.set_pairing_for_test(origin(), pairing(connected(), None), ctx);
        });
        drop(closed);

        hosts.update(&mut app, |hosts, ctx| {
            hosts.on_manager_event(
                &RemoteServerManagerEvent::SessionDeregistered {
                    session_id: SessionId::from(102),
                },
                ctx,
            );
        });

        hosts.read(&app, |hosts, _| {
            assert_eq!(
                hosts.pairing(origin()).map(|pairing| &pairing.link),
                Some(&connected())
            );
        });
        assert_eq!(remote_host_id(&pane_session(&app, &keep)), Some(host_id()));
    });
}

#[test]
fn helper_less_host_keeps_blocks_and_shows_the_unavailable_notice() {
    // AE4: no remote server and no wrapper socket.
    let pairing = pairing(ServerLink::Unavailable, None);
    let pane = PaneRemote::new(Some(&pairing), HOST, false);

    assert_eq!(
        pane.coding_panel_state(),
        Some(CodingPanelEnablementState::RemoteUnavailable(
            RemoteUnavailableReason::NoRemoteServer
        ))
    );
    assert_eq!(pane.executor_plan(), ExecutorPlan::None);
    let banner = banner_text(&pairing).expect("unavailable shows a banner");
    assert!(banner.contains("unavailable on box"), "{banner}");
    assert!(banner.contains("Blocks and AI still work"), "{banner}");

    // With the wrapper's ControlMaster socket, generators still run over it.
    let with_socket = PaneRemote::new(
        Some(&self::pairing(ServerLink::Unavailable, Some("/tmp/cm"))),
        HOST,
        false,
    );
    assert_eq!(
        with_socket.executor_plan(),
        ExecutorPlan::ControlMaster(PathBuf::from("/tmp/cm"))
    );
}

#[test]
fn unavailable_notice_is_dismissed_once_per_host() {
    App::test((), |mut app| async move {
        let hosts = app.add_singleton_model(|_| TmuxRemoteHosts::default());
        let other_origin = TmuxOriginId::new_for_test(2);
        hosts.update(&mut app, |hosts, ctx| {
            hosts.set_pairing_for_test(origin(), pairing(ServerLink::Unavailable, None), ctx);
            hosts.set_pairing_for_test(other_origin, pairing(ServerLink::Unavailable, None), ctx);
            assert!(hosts.banner(origin()).is_some());
            hosts.dismiss_banner(origin(), ctx);
            assert_eq!(hosts.banner(origin()), None);
            assert_eq!(hosts.banner(other_origin), None);
        });
    });
}

#[test]
fn pane_on_another_host_says_the_explorer_is_unavailable_for_it() {
    let pairing = pairing(connected(), Some("/tmp/cm"));
    let pane = PaneRemote::new(Some(&pairing), "elsewhere", false);

    assert_eq!(pane, PaneRemote::OtherHost);
    let state = pane.coding_panel_state();
    assert_eq!(
        state,
        Some(CodingPanelEnablementState::RemoteUnavailable(
            RemoteUnavailableReason::PaneOnOtherHost
        ))
    );
    assert!(
        RemoteUnavailableReason::PaneOnOtherHost
            .message()
            .contains("unavailable for this pane")
    );
    // Never the paired host's filesystem, socket or connection.
    assert_eq!(pane.host_id(), None);
    assert_eq!(pane.remote_server_session(), None);
    assert_eq!(pane.executor_plan(), ExecutorPlan::Default);
}

#[test]
fn dropped_connection_shows_reconnecting_and_recovers() {
    let origin_session = SessionId::from(ORIGIN_SESSION);
    let event = |event| LinkEvent::from_manager_event(&event, origin_session).unwrap();

    // The manager enters its reconnect flow by announcing that the host lost its last session.
    let link = connected().apply(event(RemoteServerManagerEvent::HostDisconnected {
        host_id: host_id(),
    }));
    assert_eq!(link, ServerLink::Reconnecting { host_id: host_id() });
    let reconnecting = PaneRemote::new(Some(&pairing(link.clone(), None)), HOST, false);
    assert_eq!(
        reconnecting.coding_panel_state(),
        Some(CodingPanelEnablementState::RemoteUnavailable(
            RemoteUnavailableReason::Reconnecting
        ))
    );
    assert_eq!(reconnecting.host_id(), Some(None));
    assert!(
        banner_text(&pairing(link.clone(), None))
            .unwrap()
            .starts_with("Reconnecting")
    );

    // Another host going away does not touch this one.
    assert_eq!(
        connected().apply(LinkEvent::HostDisconnected(HostId::new("other".to_owned()))),
        connected()
    );

    // Recovers on reconnect.
    assert_eq!(
        link.clone().apply(LinkEvent::Connected(host_id())),
        connected()
    );

    // Falls back to R13's notice once retries run out.
    let exhausted = link.apply(event(RemoteServerManagerEvent::SessionDisconnected {
        session_id: origin_session,
        host_id: host_id(),
        exit_status: None,
        was_reconnect_attempt: true,
    }));
    assert_eq!(exhausted, ServerLink::Unavailable);

    // A final drop announces the session first; the host event that follows keeps it there.
    assert_eq!(
        ServerLink::Unavailable.apply(LinkEvent::HostDisconnected(host_id())),
        ServerLink::Unavailable
    );
}

#[test]
fn panes_follow_the_link_through_a_reconnect() {
    App::test((), |mut app| async move {
        let hosts = app.add_singleton_model(|_| TmuxRemoteHosts::default());
        let pane = add_pane(&mut app, &hosts, 101, HOST);
        hosts.update(&mut app, |hosts, ctx| {
            hosts.set_pairing_for_test(origin(), pairing(connected(), None), ctx);
        });
        assert_eq!(remote_host_id(&pane_session(&app, &pane)), Some(host_id()));

        hosts.update(&mut app, |hosts, ctx| {
            hosts.on_manager_event(
                &RemoteServerManagerEvent::HostDisconnected { host_id: host_id() },
                ctx,
            );
            assert!(hosts.banner(origin()).unwrap().starts_with("Reconnecting"));
        });
        assert_eq!(remote_host_id(&pane_session(&app, &pane)), None);

        hosts.update(&mut app, |hosts, ctx| {
            hosts.on_manager_event(
                &RemoteServerManagerEvent::SessionConnected {
                    session_id: SessionId::from(ORIGIN_SESSION),
                    host_id: host_id(),
                },
                ctx,
            );
            assert_eq!(hosts.banner(origin()), None);
        });
        assert_eq!(remote_host_id(&pane_session(&app, &pane)), Some(host_id()));
    });
}

#[test]
fn helper_less_pane_sends_no_generator_bytes_through_send_keys() {
    let (executor_command_tx, executor_command_rx) = async_channel::unbounded();
    let (cancel_tx, _cancel_rx) = async_channel::unbounded();
    let in_band: Arc<dyn CommandExecutor> =
        Arc::new(InBandCommandExecutor::new(executor_command_tx, cancel_tx));

    // The default for a remote pane without the SSH wrapper is in-band; tmux panes never keep it.
    let pane = PaneRemote::new(Some(&pairing(ServerLink::Unavailable, None)), HOST, false);
    let executor = resolve_executor(pane.executor_plan(), in_band.clone(), |_, _| None);
    assert!(is::<NoOpCommandExecutor>(&executor));

    let shell = Shell::new(ShellType::Bash, None, None, Default::default(), None);
    let _ = futures::executor::block_on(executor.execute_command(
        "compgen -c",
        &shell,
        None,
        None,
        ExecuteCommandOptions::default(),
    ));
    assert!(matches!(
        executor_command_rx.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));

    // The same holds for a pane on another host and for local tmux panes.
    for pane in [
        PaneRemote::OtherHost,
        PaneRemote::new(None, "this-mac", true),
    ] {
        let executor = resolve_executor(pane.executor_plan(), in_band.clone(), |_, _| None);
        assert!(!is::<InBandCommandExecutor>(&executor));
    }
}

#[test]
fn local_tmux_panes_keep_their_non_typing_executor() {
    let local: Arc<dyn CommandExecutor> = Arc::new(TestCommandExecutor {});
    let pane = PaneRemote::new(None, "this-mac", true);
    assert_eq!(pane, PaneRemote::Unpaired);
    assert_eq!(pane.coding_panel_state(), None);
    let executor = resolve_executor(pane.executor_plan(), local.clone(), |_, _| None);
    assert!(Arc::ptr_eq(&executor, &local));
}

#[test]
fn helper_less_host_gets_the_launcher_over_the_control_channel() {
    // AE4: the launcher install runs as tmux commands on the tmux server's host, so it needs
    // neither the SSH extension nor the plugin.
    let bundle = LauncherBundle::new(vec![BundleFile {
        path: "launcher.sh",
        contents: b"echo warp".to_vec(),
        executable: true,
    }]);
    let commands: Vec<String> = install_check_commands(&bundle)
        .into_iter()
        .chain(install_upload_commands(&bundle, "1"))
        .collect();
    assert!(
        commands
            .iter()
            .all(|line| line.starts_with("run-shell ") || line.starts_with("show-options ")),
        "{commands:?}"
    );

    // A Warp split started through the launcher verifies and shows blocks.
    let token = "0123456789abcdef0123456789abcdef";
    let pane = PaneFacts {
        pane_id: PaneId(4),
        alternate_on: false,
        session_id_option: Some(derive_session_id(token, PaneId(4)).as_u64().to_string()),
        shell_option: Some("bash".to_owned()),
        current_command: Some("bash".to_owned()),
    };
    assert_eq!(
        new_pane_plan(&pane, Some(token), Some(&[]), false, true),
        NewPanePlan::Launcher
    );

    // Its session still never types generator commands.
    let unavailable = pairing(ServerLink::Unavailable, None);
    assert_eq!(
        PaneRemote::new(Some(&unavailable), HOST, false).executor_plan(),
        ExecutorPlan::None
    );
}

#[test]
fn initial_link_follows_the_managers_state() {
    assert_eq!(ServerLink::from_state(None), ServerLink::Unavailable);
    assert_eq!(
        ServerLink::from_state(Some(&RemoteSessionState::Connecting)),
        ServerLink::Connecting
    );
    assert_eq!(
        ServerLink::from_state(Some(&RemoteSessionState::Disconnected)),
        ServerLink::Unavailable
    );
}

#[test]
fn origin_pairing_reads_only_remote_sessions() {
    let mut info = pane_session_info(ORIGIN_SESSION, HOST);
    info.is_ssh_wrapper_session =
        crate::terminal::model::session::IsSSHWrapperSession::Yes {
            socket_path: PathBuf::from("/tmp/cm"),
            external_control_master: false,
        };
    let remote = Session::new(info, Arc::new(TestCommandExecutor {}));
    assert_eq!(
        OriginSession::from_session(&remote),
        Some(origin_session(Some("/tmp/cm")))
    );

    let local = Session::new(SessionInfo::new_for_test(), Arc::new(TestCommandExecutor {}));
    assert_eq!(OriginSession::from_session(&local), None);
}
