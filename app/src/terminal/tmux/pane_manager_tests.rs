use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{FairMutex, Mutex};
use warp_terminal::local_tty::mio_channel;
use warp_terminal::local_tty::tmux_gateway::{
    GatewayEvent, PaneChunk, TestPaneQueueSender, TmuxRequest, test_pane_queue,
};
use warp_terminal::tmux::{
    PaneId, PaneSnapshot, SnapshotKind, WindowFlags, WindowId, WindowLayout, layout_checksum,
    parse_layout_change, send_keys_command,
};
use warpui::r#async::executor::Background;
use warpui::{App, ModelHandle};

use super::*;
use crate::auth::AuthStateProvider;
use crate::server::telemetry::context_provider::AppTelemetryContextProvider;
use crate::terminal::History;
use crate::terminal::color::{self, Colors};
use crate::terminal::event_listener::ChannelEventListener;
use crate::terminal::input::CommandExecutionSource;
use crate::terminal::model::ObfuscateSecrets;
use crate::terminal::model::bootstrap::BootstrapStage;
use crate::terminal::model::session::{SessionId, SessionInfo, Sessions};
use crate::terminal::model::test_utils::block_size;
use crate::terminal::shell::ShellType;
use crate::terminal::tmux::TmuxOriginId;
use crate::terminal::tmux::gateway::TmuxGateways;
use crate::terminal::tmux::pane_event_loop::{PaneRequestSink, TmuxPaneEventLoop};
use crate::terminal::writeable_pty::Message;
use crate::test_util::assert_eventually;
use crate::test_util::settings::initialize_settings_for_tests;

const PANE: PaneId = PaneId(3);
const SESSION: u64 = 7;

#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<Vec<TmuxRequest>>>);

impl RecordingSink {
    fn requests(&self) -> Vec<TmuxRequest> {
        self.0.lock().clone()
    }
}

impl PaneRequestSink for RecordingSink {
    fn send(&self, request: TmuxRequest) -> bool {
        self.0.lock().push(request);
        true
    }
}

struct Harness {
    model: Arc<FairMutex<TerminalModel>>,
    backend: PaneBackend,
    requests: RecordingSink,
    queue: Option<TestPaneQueueSender>,
}

impl Harness {
    fn push(&self, chunk: PaneChunk) {
        self.queue
            .as_ref()
            .expect("queue is still open")
            .push(chunk);
    }

    fn output(&self, bytes: impl AsRef<[u8]>) {
        self.push(PaneChunk::Output(bytes.as_ref().to_vec()));
    }

    /// Warp's bootstrap, as when it is sourced in the pane: the bootstrap command finishes, rc
    /// files run, then the shell reports that it is bootstrapped and shows a prompt.
    fn bootstrap(&self) {
        self.output(init_shell());
        self.output(command_finished());
        self.output(bootstrapped());
        self.output(precmd());
        // Each later prompt reports the previous command's end, then the prompt.
        self.output(command_finished());
        self.output(precmd());
    }

    fn event_loop(&self) -> &ModelHandle<TmuxPaneEventLoop> {
        &self.backend.event_loop
    }

    fn pane_inputs(&self) -> Vec<Vec<u8>> {
        self.requests
            .requests()
            .into_iter()
            .filter_map(|request| match request {
                TmuxRequest::PaneInput { pane_id, bytes } if pane_id == PANE => Some(bytes),
                _ => None,
            })
            .collect()
    }

    fn text(&self) -> String {
        let model = self.model.lock();
        model
            .block_list()
            .blocks()
            .iter()
            .map(|block| {
                format!(
                    "{}\n{}",
                    block
                        .prompt_and_command_grid()
                        .contents_to_string_force_full_grid_contents(false, None),
                    block.output_to_string_force_full_grid_contents()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A pane backend fed by a test queue, with a recording request sink in place of the gateway.
/// The session's shell is registered so the input editor can run commands; hook session ids
/// stay validated (real registration comes with shell integration, U9).
fn harness(app: &mut App, shell_type: ShellType) -> Harness {
    // Only what a pane's models need; the full terminal-view setup is slow and unneeded.
    initialize_settings_for_tests(app);
    app.add_singleton_model(|_| AuthStateProvider::new_for_test());
    app.add_singleton_model(AppTelemetryContextProvider::new_context_provider);
    app.add_singleton_model(|_| History::default());
    let (wakeups_tx, _wakeups_rx) = async_channel::unbounded();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (pty_reads_tx, _pty_reads_rx) = async_broadcast::broadcast(1);
    let listener = ChannelEventListener::new(wakeups_tx, events_tx, pty_reads_tx);

    // A fresh model, as a new pane gets: no shell integration yet.
    let mut model = TerminalModel::new(
        None,
        block_size(),
        color::List::from(&Colors::default()),
        listener.clone(),
        Arc::new(Background::default()),
        false, /* should_show_bootstrap_block */
        false, /* should_show_in_band_command_blocks */
        false, /* should_show_memory_stats */
        false, /* honor_ps1 */
        false, /* is_inverted */
        ObfuscateSecrets::No,
        false, /* is_ai_ugc_telemetry_enabled */
        None,
        pane_shell_launch_state(Some(shell_type.name())),
    );
    prepare_pane_model(&mut model);
    model.register_session_id(SessionId::from(SESSION));
    let model = Arc::new(FairMutex::new(model));

    let mut sessions = Sessions::new_for_test();
    sessions.register_session_for_test(
        SessionInfo::new_for_test()
            .with_id(SESSION)
            .with_shell_type(shell_type),
    );
    let sessions = app.add_model(|_| sessions);
    let (_executor_command_tx, executor_command_rx) = async_channel::unbounded();

    let requests = RecordingSink::default();
    let backend = app.update(|ctx| {
        wire_pane_backend(
            PANE,
            model.clone(),
            listener,
            events_rx,
            sessions,
            executor_command_rx,
            Arc::new(requests.clone()),
            ctx,
        )
    });

    let (queue_tx, queue_rx) = test_pane_queue(PANE);
    backend.event_loop.update(app, |event_loop, ctx| {
        event_loop.attach_queue(queue_rx, ctx);
    });

    Harness {
        model,
        backend,
        requests,
        queue: Some(queue_tx),
    }
}

/// A harness whose queue will come from the gateways singleton: the test queue is closed, which
/// ends the first drain, so the pane can take the gateway's.
fn harness_fed_by_gateways(app: &mut App) -> Harness {
    let mut pane = harness(app, ShellType::Bash);
    drop(pane.queue.take());
    pane
}

fn hook(json: &str) -> Vec<u8> {
    let mut bytes = b"\x1bP$d".to_vec();
    bytes.extend(hex::encode(json).bytes());
    bytes.push(0x9c);
    bytes
}

fn init_shell() -> Vec<u8> {
    hook(&format!(
        r#"{{"hook":"InitShell","value":{{"session_id":{SESSION},"shell":"bash","hostname":"localhost"}}}}"#
    ))
}

fn bootstrapped() -> Vec<u8> {
    hook(&format!(
        r#"{{"hook":"Bootstrapped","value":{{"session_id":{SESSION},"shell":"bash","histfile":"","home_dir":"/home/me","path":"/bin","aliases":"","abbreviations":"","function_names":"","env_var_names":"","builtins":"","keywords":"","shell_version":"5.2"}}}}"#
    ))
}

fn precmd() -> Vec<u8> {
    hook(&format!(
        r#"{{"hook":"Precmd","value":{{"pwd":"/tmp","session_id":{SESSION}}}}}"#
    ))
}

fn preexec(command: &str) -> Vec<u8> {
    hook(&format!(
        r#"{{"hook":"Preexec","value":{{"command":"{command}","session_id":{SESSION}}}}}"#
    ))
}

fn command_finished() -> Vec<u8> {
    let next_block_id = crate::terminal::model::block::BlockId::new();
    hook(&format!(
        r#"{{"hook":"CommandFinished","value":{{"exit_code":0,"next_block_id":"{next_block_id}","session_id":{SESSION}}}}}"#
    ))
}

fn layout(body: &str) -> WindowLayout {
    let layout = format!("{:04x},{body}", layout_checksum(body));
    parse_layout_change(&layout, None, &WindowFlags::default()).unwrap()
}

#[test]
fn output_with_warp_hooks_produces_blocks() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);

        pane.bootstrap();
        // A user command.
        pane.output(preexec("ls"));
        pane.output(b"file.txt\r\n");
        pane.output(command_finished());
        pane.output(precmd());

        assert_eventually!(
            pane.model.lock().block_list().blocks().iter().any(|block| {
                block.finished()
                    && block.bootstrap_stage() == BootstrapStage::PostBootstrapPrecmd
                    && block.output_to_string().contains("file.txt")
            }),
            "a finished command block holds the output: {:?}",
            pane.model
                .lock()
                .block_list()
                .blocks()
                .iter()
                .map(|b| (b.bootstrap_stage(), b.finished(), b.output_to_string()))
                .collect::<Vec<_>>()
        );
        let model = pane.model.lock();
        assert!(model.block_list().is_bootstrapped());
        assert!(!model.is_read_only());
    });
}

#[test]
fn output_without_hooks_is_plain_scrolling_output() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);

        let lines: String = (0..200).map(|i| format!("line {i}\r\n")).collect();
        pane.output(lines.as_bytes());

        assert_eventually!(
            pane.text().contains("line 199"),
            "plain output reaches the pane: {:?}",
            pane.text()
        );
        let model = pane.model.lock();
        assert!(!model.is_read_only(), "the pane is not in an exited state");
        assert!(
            model
                .block_list()
                .active_block()
                .is_visible(&Default::default()),
            "pre-integration output is shown, not hidden as a bootstrap block"
        );
    });
}

#[test]
fn running_ls_from_the_input_editor_sends_one_send_keys() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        pane.bootstrap();
        assert_eventually!(
            pane.model.lock().block_list().is_bootstrapped(),
            "the pane is bootstrapped"
        );
        // Forget what bootstrapping sent.
        pane.requests.0.lock().clear();

        let outcome = pane
            .backend
            .pty_controller
            .update(&mut app, |controller, ctx| {
                controller.write_command("ls", ShellType::Bash, CommandExecutionSource::User, ctx)
            });
        assert!(outcome.is_accepted());

        // Ctrl-E Ctrl-U (the default editor's clear-line keys, not Warp's Ctrl-P binding, which a
        // marker-only pane does not install), the command, then the shell's Enter.
        let expected = [&b"\x05\x15"[..], b"ls", b"\n"].concat();
        assert!(!expected.contains(&0x10), "no Ctrl-P");
        // The write waits for the line editor, then goes out as one pane input. Warp's own
        // writes (the input-reporting sequence, or a late bootstrap script whose text happens to
        // contain "ls") may surround it; exactly one input is the whole command.
        let command_inputs = || {
            pane.pane_inputs()
                .into_iter()
                .filter(|input| input.ends_with(b"ls\n") && input.len() <= expected.len())
                .collect::<Vec<_>>()
        };
        assert_eventually!(
            200 => !command_inputs().is_empty(),
            "the command reaches the pane: {:?}",
            pane.requests.requests()
        );
        warpui::r#async::Timer::after(std::time::Duration::from_millis(50)).await;
        let inputs = command_inputs();
        assert_eq!(
            inputs.len(),
            1,
            "one input for the whole command: {inputs:?}"
        );
        assert_eq!(inputs[0], expected);
        assert_eq!(send_keys_command(PANE, &inputs[0]).len(), 1);
    });
}

#[test]
fn keystrokes_are_forwarded_as_pane_input() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        pane.backend
            .pty_controller
            .update(&mut app, |controller, ctx| {
                controller.write_bytes(b"l".to_vec(), ctx);
                controller.write_bytes(b"s\r".to_vec(), ctx);
            });
        assert_eventually!(pane.pane_inputs().len() == 2, "both keystrokes are sent");
        assert_eq!(pane.pane_inputs(), vec![b"l".to_vec(), b"s\r".to_vec()]);
    });
}

#[test]
fn terminal_queries_in_pane_output_are_left_to_tmux() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        // DA1, DA2, CPR, OSC 11 and the kitty keyboard query, as vim or fish send at startup.
        // tmux is the terminal the program talks to and answers these itself; a second answer
        // from Warp would arrive as typed input.
        pane.output(b"\x1b[c\x1b[>c\x1b[6n\x1b]11;?\x07\x1b[?u");
        pane.output(b"after the queries\r\n");
        assert_eventually!(
            pane.text().contains("after the queries"),
            "the output is parsed: {:?}",
            pane.text()
        );
        assert!(
            pane.requests.requests().is_empty(),
            "nothing is sent back: {:?}",
            pane.requests.requests()
        );
    });
}

#[test]
fn a_snapshot_replaces_what_the_pane_showed() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        pane.output(b"stale output\r\n");
        assert_eventually!(pane.text().contains("stale output"), "output arrives");

        pane.push(PaneChunk::Snapshot(PaneSnapshot {
            pane_id: PANE,
            kind: SnapshotKind::Resync,
            lines: vec![b"fresh".to_vec(), b"screen".to_vec()],
            cursor: Some((0, 1)),
        }));

        assert_eventually!(
            pane.text().contains("fresh"),
            "the snapshot is applied: {:?}",
            pane.text()
        );
        assert!(!pane.text().contains("stale output"), "{:?}", pane.text());
        assert!(pane.requests.requests().is_empty());
    });
}

#[test]
fn a_pane_resize_sends_one_refresh_client_for_its_window() {
    App::test((), |mut app| async move {
        let sizer = app.add_singleton_model(|_| TmuxWindowSizer::new_for_test());
        let origin = TmuxOriginId::new_for_test(1);
        let window_requests = RecordingSink::default();
        sizer.update(&mut app, |sizer, _| {
            sizer.register_origin(origin, Arc::new(window_requests.clone()));
            // Two panes side by side in @2: %3 and %4, 80 + 1 + 79 columns.
            sizer.set_layout(
                origin,
                WindowId(2),
                layout("160x24,0,0{80x24,0,0,3,79x24,81,0,4}"),
            );
        });

        let pane = harness(&mut app, ShellType::Bash);
        app.update(|ctx| connect_resizes(origin, PANE, &pane.backend.event_loop, ctx));

        let mut size = *pane.model.lock().block_list().size();
        size.columns = 100;
        size.rows = 30;
        pane.backend.pty_controller.update(&mut app, |_, _| {});
        pane.backend.event_loop.update(&mut app, |event_loop, ctx| {
            event_loop.handle_message(Message::Resize(size), ctx);
        });
        sizer.update(&mut app, |sizer, ctx| {
            // The other pane resizes in the same layout pass.
            sizer.pane_resized(origin, PaneId(4), 99, 30, ctx);
        });

        assert_eventually!(
            !window_requests.requests().is_empty(),
            "the window size is sent"
        );
        assert_eq!(
            window_requests.requests(),
            vec![TmuxRequest::Command {
                tag: WINDOW_SIZE_COMMAND_TAG,
                line: "refresh-client -C @2:200x30".to_owned(),
            }]
        );
        // Nothing went to the pane itself.
        assert!(pane.requests.requests().is_empty());

        // The same size again sends nothing more.
        sizer.update(&mut app, |sizer, ctx| {
            sizer.pane_resized(origin, PaneId(4), 99, 30, ctx);
        });
        sizer.update(&mut app, |sizer, _| sizer.flush());
        assert_eq!(window_requests.requests().len(), 1);
    });
}

#[test]
fn a_size_matching_the_layout_sends_nothing() {
    App::test((), |mut app| async move {
        let sizer = app.add_singleton_model(|_| TmuxWindowSizer::new_for_test());
        let origin = TmuxOriginId::new_for_test(1);
        let window_requests = RecordingSink::default();
        sizer.update(&mut app, |sizer, ctx| {
            sizer.register_origin(origin, Arc::new(window_requests.clone()));
            sizer.set_layout(origin, WindowId(2), layout("80x24,0,0,3"));
            sizer.pane_resized(origin, PaneId(3), 80, 24, ctx);
            sizer.pane_resized(origin, PaneId(9), 50, 50, ctx);
            sizer.flush();
        });
        assert!(window_requests.requests().is_empty());
    });
}

#[test]
fn dropping_the_pane_view_sends_no_tmux_command() {
    App::test((), |mut app| async move {
        let mut pane = harness(&mut app, ShellType::Bash);
        pane.output(b"before\r\n");
        assert_eventually!(pane.text().contains("before"), "output arrives");

        // Closing a Warp pane shuts its PTY down; for tmux that only releases the view.
        pane.backend
            .pty_controller
            .update(&mut app, |controller, ctx| {
                controller.shutdown_pty(ctx);
            });
        assert_eventually!(
            pane.event_loop()
                .read(&app, |event_loop, _| event_loop.is_released()),
            "the view state is released"
        );
        pane.event_loop().update(&mut app, |event_loop, ctx| {
            event_loop.handle_message(Message::ChildExited, ctx);
            event_loop.handle_message(Message::Input(b"late".to_vec().into()), ctx);
        });

        // Output after release is discarded without touching the model.
        pane.output(b"after\r\n");
        pane.output(b"\x1b[c");
        drop(pane.queue.take());
        assert_eventually!(
            pane.event_loop()
                .read(&app, |event_loop, _| !event_loop.is_draining()),
            "the drain stops"
        );
        assert!(!pane.text().contains("after"));
        assert!(
            pane.requests.requests().is_empty(),
            "no kill-pane or any other request: {:?}",
            pane.requests.requests()
        );
    });
}

#[test]
fn a_closed_queue_ends_the_pane_cleanly() {
    App::test((), |mut app| async move {
        let mut pane = harness(&mut app, ShellType::Bash);
        let ended = Arc::new(Mutex::new(false));
        let ended_flag = ended.clone();
        app.update(|ctx| {
            ctx.subscribe_to_model(pane.event_loop(), move |_, event, _| {
                if matches!(event, TmuxPaneEventLoopEvent::Ended) {
                    *ended_flag.lock() = true;
                }
            });
        });

        pane.output(b"last words\r\n");
        drop(pane.queue.take());

        assert_eventually!(*ended.lock(), "the pane reports that it ended");
        assert!(pane.text().contains("last words"));
        assert!(
            !pane
                .event_loop()
                .read(&app, |event_loop, _| event_loop.is_draining())
        );
        assert!(pane.requests.requests().is_empty());
    });
}

#[test]
fn the_shell_type_comes_from_the_pane_current_command() {
    let shell_type = |command: Option<&str>| match pane_shell_launch_state(command) {
        ShellLaunchState::ShellSpawned { shell_type, .. } => Some(shell_type),
        ShellLaunchState::DeterminingShell { .. } => None,
    };
    assert_eq!(shell_type(Some("fish")), Some(ShellType::Fish));
    assert_eq!(shell_type(Some("-zsh")), Some(ShellType::Zsh));
    assert_eq!(shell_type(Some("bash")), Some(ShellType::Bash));
    // A pane running something else has no shell type yet; integration decides later.
    assert_eq!(shell_type(Some("vim")), None);
    assert_eq!(shell_type(None), None);
}

#[test]
fn a_queue_the_gateway_opens_later_is_attached() {
    App::test((), |mut app| async move {
        let mut pane = harness_fed_by_gateways(&mut app);
        assert_eventually!(
            !pane
                .event_loop()
                .read(&app, |event_loop, _| event_loop.is_draining()),
            "the test queue is done"
        );

        let gateways = app.add_singleton_model(TmuxGateways::new);
        let (requests_tx, _requests_rx) = mio_channel::channel();
        let (origin, mut link) = gateways.update(&mut app, |g, _| {
            g.link_for_new_origin(requests_tx, &HashMap::new())
        });
        app.update(|ctx| connect_queue(origin, PANE, &pane.backend.event_loop.clone(), ctx));

        // Another pane's queue is not ours.
        let (_other_tx, other_rx) = test_pane_queue(PaneId(99));
        (link.sink)(GatewayEvent::PaneQueueOpened { queue: other_rx });
        let (queue_tx, queue_rx) = test_pane_queue(PANE);
        (link.sink)(GatewayEvent::PaneQueueOpened { queue: queue_rx });
        pane.queue = Some(queue_tx);
        pane.output(b"from the gateway\r\n");

        assert_eventually!(
            pane.text().contains("from the gateway"),
            "the gateway's queue feeds the pane: {:?}",
            pane.text()
        );
        // The other pane's queue stays with the gateways for its own pane.
        assert!(
            gateways
                .update(&mut app, |g, _| g.take_pane_queue(origin, PaneId(99)))
                .is_some()
        );
    });
}

#[test]
fn a_queue_ready_before_the_pane_is_attached_at_once() {
    App::test((), |mut app| async move {
        let mut pane = harness_fed_by_gateways(&mut app);
        assert_eventually!(
            !pane
                .event_loop()
                .read(&app, |event_loop, _| event_loop.is_draining()),
            "the test queue is done"
        );

        let gateways = app.add_singleton_model(TmuxGateways::new);
        let (requests_tx, _requests_rx) = mio_channel::channel();
        let (origin, mut link) = gateways.update(&mut app, |g, _| {
            g.link_for_new_origin(requests_tx, &HashMap::new())
        });
        let (queue_tx, queue_rx) = test_pane_queue(PANE);
        (link.sink)(GatewayEvent::PaneQueueOpened { queue: queue_rx });
        pane.queue = Some(queue_tx);
        pane.output(b"early bytes\r\n");
        assert_eventually!(
            gateways.read(&app, |g, _| g.request_sender(origin).is_some()),
            "the origin is linked"
        );
        // Let the gateways receive the queue on the main thread.
        warpui::r#async::Timer::after(std::time::Duration::from_millis(20)).await;

        app.update(|ctx| connect_queue(origin, PANE, &pane.backend.event_loop.clone(), ctx));
        assert_eventually!(
            pane.text().contains("early bytes"),
            "the waiting queue feeds the pane: {:?}",
            pane.text()
        );
    });
}

#[test]
fn a_view_recreated_for_a_dropped_pane_asks_tmux_for_its_content() {
    App::test((), |mut app| async move {
        let pane = harness_fed_by_gateways(&mut app);
        let gateways = app.add_singleton_model(TmuxGateways::new);
        let (requests_tx, requests_rx) = mio_channel::channel();
        let (origin, _link) = gateways.update(&mut app, |g, ctx| {
            let (origin, link) = g.link_for_new_origin(requests_tx, &HashMap::new());
            g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
            (origin, link)
        });
        let resyncs = move || {
            std::iter::from_fn(|| requests_rx.try_recv().ok())
                .filter(|message| {
                    matches!(message, Message::Tmux(TmuxRequest::ResyncPane(pane)) if *pane == PANE)
                })
                .count()
        };

        // A brand-new pane waits for its first output; nothing is asked.
        app.update(|ctx| connect_queue(origin, PANE, &pane.backend.event_loop.clone(), ctx));
        assert_eq!(resyncs(), 0);

        // The pane's view was dropped (zoomed out, or moved to another tab) and comes back: the
        // old queue went with the old view, so the gateway is asked for a new one.
        gateways.update(&mut app, |g, _| g.pane_view_released(origin, PANE));
        app.update(|ctx| connect_queue(origin, PANE, &pane.backend.event_loop.clone(), ctx));
        assert_eq!(resyncs(), 1);
        // Asked once per drop.
        app.update(|ctx| connect_queue(origin, PANE, &pane.backend.event_loop.clone(), ctx));
        assert_eq!(resyncs(), 0);
    });
}

#[test]
fn a_dropped_pane_whose_queue_was_reopened_takes_it_without_asking() {
    App::test((), |mut app| async move {
        let mut pane = harness_fed_by_gateways(&mut app);
        assert_eventually!(
            !pane
                .event_loop()
                .read(&app, |event_loop, _| event_loop.is_draining()),
            "the test queue is done"
        );
        let gateways = app.add_singleton_model(TmuxGateways::new);
        let (requests_tx, requests_rx) = mio_channel::channel();
        let (origin, mut link) = gateways.update(&mut app, |g, ctx| {
            let (origin, link) = g.link_for_new_origin(requests_tx, &HashMap::new());
            g.deliver_for_test(origin, GatewayEvent::Starting, ctx);
            (origin, link)
        });
        gateways.update(&mut app, |g, _| g.pane_view_released(origin, PANE));
        // Output while the pane was hidden reopened its queue (with a snapshot) in the gateway.
        let (queue_tx, queue_rx) = test_pane_queue(PANE);
        (link.sink)(GatewayEvent::PaneQueueOpened { queue: queue_rx });
        pane.queue = Some(queue_tx);
        pane.output(b"while hidden\r\n");
        warpui::r#async::Timer::after(std::time::Duration::from_millis(20)).await;

        app.update(|ctx| connect_queue(origin, PANE, &pane.backend.event_loop.clone(), ctx));
        assert_eventually!(
            pane.text().contains("while hidden"),
            "the reopened queue feeds the new view: {:?}",
            pane.text()
        );
        assert!(requests_rx.try_recv().is_err(), "nothing else is asked");
    });
}

#[test]
fn a_queue_reopened_while_the_old_one_drains_is_kept() {
    App::test((), |mut app| async move {
        let mut pane = harness(&mut app, ShellType::Bash);
        let (next_tx, next_rx) = test_pane_queue(PANE);
        pane.event_loop().update(&mut app, |event_loop, ctx| {
            event_loop.attach_queue(next_rx, ctx);
        });
        drop(pane.queue.take());
        next_tx.push(PaneChunk::Output(b"after reopening\r\n".to_vec()));

        assert_eventually!(
            pane.text().contains("after reopening"),
            "the second queue feeds the pane: {:?}",
            pane.text()
        );
        assert!(
            pane.event_loop()
                .read(&app, |event_loop, _| event_loop.is_draining())
        );
    });
}

// Shell integration (U9).

fn init_subshell_shell() -> Vec<u8> {
    hook(&format!(
        r#"{{"hook":"InitShell","value":{{"session_id":{SESSION},"shell":"bash","hostname":"localhost","is_subshell":true}}}}"#
    ))
}

/// What the launcher prints while a pane starts, before its first prompt: `InitShell` (it
/// sourced the bootstrap itself), the bootstrap's own prompt hooks, `Bootstrapped`, then the
/// first prompt.
fn launcher_startup(pane: &Harness) {
    pane.output(init_shell());
    pane.output(command_finished());
    pane.output(precmd());
    pane.output(bootstrapped());
    pane.output(command_finished());
    pane.output(precmd());
}

#[test]
fn warp_writes_nothing_to_a_launcher_pane_before_its_first_prompt() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        launcher_startup(&pane);
        assert_eventually!(
            pane.model.lock().block_list().is_bootstrapped(),
            "the launcher's hooks integrate the pane"
        );
        // Give any write the InitShell or Bootstrapped events would cause time to arrive.
        warpui::r#async::Timer::after(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            pane.pane_inputs(),
            Vec::<Vec<u8>>::new(),
            "no bootstrap script, prompt switch or other bytes"
        );
    });
}

#[test]
fn the_typed_path_still_gets_the_bootstrap_body() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        // Warp typed the init-subshell command; the shell answers with a subshell InitShell.
        pane.output(init_subshell_shell());
        assert_eventually!(
            !pane.pane_inputs().is_empty(),
            "Warp writes the bootstrap body after a subshell InitShell"
        );
    });
}

#[test]
fn a_forged_hook_printed_by_a_program_produces_no_block() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        // `cat` of a file holding Warp hooks with an id nobody registered.
        let forged = |json: &str| hook(&json.replace(&SESSION.to_string(), "99999"));
        pane.output(forged(&format!(
            r#"{{"hook":"InitShell","value":{{"session_id":{SESSION},"shell":"bash","hostname":"x"}}}}"#
        )));
        pane.output(forged(&format!(
            r#"{{"hook":"Bootstrapped","value":{{"session_id":{SESSION},"shell":"bash","histfile":"","home_dir":"/","path":"/bin","aliases":"","abbreviations":"","function_names":"","env_var_names":"","builtins":"","keywords":"","shell_version":"5"}}}}"#
        )));
        pane.output(forged(&format!(
            r#"{{"hook":"Precmd","value":{{"pwd":"/tmp","session_id":{SESSION}}}}}"#
        )));
        pane.output(b"after the forged hooks\r\n");
        assert_eventually!(
            pane.text().contains("after the forged hooks"),
            "plain output still arrives"
        );
        let model = pane.model.lock();
        assert!(
            !model.block_list().is_bootstrapped(),
            "no integration from a forged hook"
        );
        assert!(model.pending_session_id().is_none());
    });
}

/// Covers F1: a launcher pane bootstrapped before Warp attached shows its screen as plain output
/// and gets blocks from its next prompt, when it announces itself again; nothing is typed.
#[test]
fn a_launcher_pane_from_before_attach_gets_blocks_from_its_next_prompt() {
    App::test((), |mut app| async move {
        let pane = harness(&mut app, ShellType::Bash);
        pane.push(PaneChunk::Snapshot(PaneSnapshot {
            pane_id: PANE,
            kind: SnapshotKind::Seed,
            lines: vec![b"u9b> make".to_vec(), b"done".to_vec(), b"u9b> ".to_vec()],
            cursor: Some((5, 2)),
        }));
        assert_eventually!(pane.text().contains("done"), "the seeded screen shows");
        assert!(!pane.model.lock().block_list().is_bootstrapped());

        // The user runs a command; at the next prompt the launcher reannounces.
        pane.output(b"ls\r\nfile.txt\r\n");
        pane.output(command_finished());
        pane.output(init_shell());
        pane.output(bootstrapped());
        pane.output(command_finished());
        pane.output(precmd());
        assert_eventually!(
            pane.model.lock().block_list().is_bootstrapped(),
            "blocks from the next prompt"
        );
        warpui::r#async::Timer::after(std::time::Duration::from_millis(100)).await;
        assert_eq!(pane.pane_inputs(), Vec::<Vec<u8>>::new(), "nothing typed");
    });
}
