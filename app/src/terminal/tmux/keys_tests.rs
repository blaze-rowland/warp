use std::time::Duration;

use instant::Instant;
use warp_terminal::tmux::{ClientKey, CommandReply, KeyBinding, KeyClass, KeyConfig, PaneId};
use warpui::keymap::{Keystroke, ReservedKeyBinding};

use super::*;
use crate::terminal::tmux::key_tables::{HOOK_MARKER, hook_slot};

const ORIGIN: TmuxOriginId = TmuxOriginId(1);
const PANE: PaneId = PaneId(1);

fn stroke(source: &str) -> Keystroke {
    Keystroke::parse(source).unwrap()
}

fn reply(lines: &[&str]) -> CommandReply {
    CommandReply {
        time: 0,
        number: 1,
        flags: 1,
        error: false,
        lines: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
    }
}

fn binding(table: &str, repeat: bool, key: &str, command: &str) -> KeyBinding {
    KeyBinding {
        table: table.into(),
        repeat,
        key: key.into(),
        command: command.into(),
    }
}

/// tmux's defaults that matter here, plus vim-tmux-navigator's `ctrl+h/j/k/l`.
fn navigator_config() -> KeyConfig {
    let navigator = |key: &str, direction: &str| {
        binding(
            "root",
            false,
            key,
            &format!("if-shell \"$is_vim\" \"send-keys {key}\" \"select-pane -{direction}\""),
        )
    };
    KeyConfig {
        prefix: Some("C-b".into()),
        prefix2: None,
        bindings: Some(vec![
            navigator("C-h", "L"),
            navigator("C-j", "D"),
            navigator("C-k", "U"),
            navigator("C-l", "R"),
            binding("prefix", true, "Up", "select-pane -U"),
            binding("prefix", false, "2", "select-window -t :=2"),
            binding("prefix", false, "%", "split-window -h"),
        ]),
    }
}

/// A core attached to a 3.4+ server with `config`, whose identity is known.
fn attached_core(config: &KeyConfig) -> KeyRouterCore {
    let mut core = KeyRouterCore::default();
    core.starting(ORIGIN);
    core.key_config_loaded(ORIGIN, config);
    assert_eq!(
        core.attached(ORIGIN),
        vec![command(IDENTITY_QUERY, CommandPurpose::Identity)]
    );
    core.on_reply(
        ORIGIN,
        CommandPurpose::Identity,
        &reply(&["client-7\tbox\t/tmp/tmux-501/default"]),
    );
    core
}

fn route(core: &KeyRouterCore, key: &str) -> KeyRoute {
    core.route(ORIGIN, PANE, &stroke(key), true, Instant::now())
}

fn tmux_key(name: &str, class: KeyClass, in_root_table: bool) -> KeyRoute {
    KeyRoute::Tmux(ClientKey {
        key: name.into(),
        pane_id: PANE,
        class,
        in_root_table,
    })
}

/// Routes `key`, and if it goes to tmux, records it as sent like the router does.
fn press(core: &mut KeyRouterCore, key: &str) -> KeyRoute {
    let route = route(core, key);
    if matches!(route, KeyRoute::Tmux(_)) {
        core.key_sent(ORIGIN, Instant::now());
    }
    route
}

fn fence_line() -> Outbound {
    command(
        "display-message -p -c \"client-7\" '#{client_key_table} #{pane_id}'",
        CommandPurpose::Fence,
    )
}

#[test]
fn ctrl_h_bound_with_if_shell_goes_to_tmux_and_skips_warp() {
    let core = attached_core(&navigator_config());
    let route = route(&core, "ctrl-h");
    // `send-keys -K -c <client> C-h`; the engine adds `-K -c "client-7"` (client_tests).
    assert_eq!(route, tmux_key("C-h", KeyClass::Navigation, true));
    // A claimed key skips Warp's keymap, so the editor's backspace on ctrl+h never fires.
    assert!(route.is_claimed());
}

#[test]
fn plain_letters_at_a_warp_prompt_stay_with_warp() {
    let core = attached_core(&navigator_config());
    for key in ["l", "s", "shift-A", "enter", "backspace"] {
        assert_eq!(route(&core, key), KeyRoute::Warp, "{key}");
    }
}

#[test]
fn prefix_alone_goes_to_tmux() {
    let core = attached_core(&navigator_config());
    assert_eq!(
        route(&core, "ctrl-b"),
        tmux_key("C-b", KeyClass::Other, true)
    );
}

#[test]
fn prefix_then_2_sends_two_client_keys_in_order() {
    let mut core = attached_core(&navigator_config());
    let first = press(&mut core, "ctrl-b");
    let second = press(&mut core, "2");
    assert_eq!(first, tmux_key("C-b", KeyClass::Other, true));
    // The client is in the prefix table: `2` is evaluated there, never by Warp.
    assert_eq!(second, tmux_key("2", KeyClass::Other, false));
    // tmux then switches windows; `%session-window-changed` focuses tab 2 through U7's
    // ActiveWindowChanged handling (workspace_sync).
}

#[test]
fn an_unbound_key_inside_an_unfenced_window_still_goes_to_tmux() {
    let mut core = attached_core(&navigator_config());
    press(&mut core, "ctrl-b");
    // `x` is in no table; tmux decides (it may be in a `switch-client -T` table).
    assert_eq!(route(&core, "x"), tmux_key("x", KeyClass::Other, false));
    assert_eq!(route(&core, "f5"), tmux_key("F5", KeyClass::Other, false));
}

#[test]
fn the_window_closes_only_after_the_key_is_acknowledged_and_a_fence_reports_root() {
    let mut core = attached_core(&navigator_config());
    press(&mut core, "ctrl-h");
    assert!(core.is_window_open(ORIGIN));
    // Typed right after ctrl+h: tmux delivers it to whichever pane is active once ctrl+h ran.
    assert_eq!(press(&mut core, "l"), tmux_key("l", KeyClass::Other, false));
    // The fence waits until every key is acknowledged.
    assert_eq!(core.key_resolved(ORIGIN, Instant::now()), vec![]);
    assert_eq!(press(&mut core, "s"), tmux_key("s", KeyClass::Other, false));
    assert_eq!(core.key_resolved(ORIGIN, Instant::now()), vec![]);
    assert_eq!(
        core.key_resolved(ORIGIN, Instant::now()),
        vec![fence_line()]
    );
    assert!(core.is_window_open(ORIGIN));
    assert_eq!(
        core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["root %0"])),
        vec![]
    );
    assert!(!core.is_window_open(ORIGIN));
    assert_eq!(route(&core, "l"), KeyRoute::Warp);
}

#[test]
fn a_fence_overtaken_by_newer_keys_is_ignored_and_retaken() {
    let mut core = attached_core(&navigator_config());
    press(&mut core, "ctrl-b");
    assert_eq!(
        core.key_resolved(ORIGIN, Instant::now()),
        vec![fence_line()]
    );
    press(&mut core, "2");
    // The fence ran before `2`; even a root answer cannot close the window.
    assert_eq!(
        core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["root %1"])),
        vec![]
    );
    assert!(core.is_window_open(ORIGIN));
    assert_eq!(
        core.key_resolved(ORIGIN, Instant::now()),
        vec![fence_line()]
    );
    core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["root %1"]));
    assert!(!core.is_window_open(ORIGIN));
}

#[test]
fn prefix_up_up_with_repeat_stays_in_tmux_for_both_ups() {
    let mut core = attached_core(&navigator_config());
    press(&mut core, "ctrl-b");
    core.key_resolved(ORIGIN, Instant::now());
    core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["prefix %2"]));
    // `bind -r Up`: tmux stays in the prefix table for repeat-time.
    assert_eq!(
        press(&mut core, "up"),
        tmux_key("Up", KeyClass::Other, false)
    );
    assert_eq!(
        core.key_resolved(ORIGIN, Instant::now()),
        vec![fence_line()]
    );
    core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["prefix %1"]));
    assert!(core.is_window_open(ORIGIN));
    assert_eq!(
        press(&mut core, "up"),
        tmux_key("Up", KeyClass::Other, false)
    );
    core.key_resolved(ORIGIN, Instant::now());
    core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["prefix %0"]));
    // Still repeating: the router asks again after the repeat time.
    assert!(core.awaiting_refence(ORIGIN));
    assert_eq!(core.refence(ORIGIN), vec![fence_line()]);
    assert!(!core.awaiting_refence(ORIGIN));
    assert_eq!(core.refence(ORIGIN), vec![]);
    core.on_reply(ORIGIN, CommandPurpose::Fence, &reply(&["root %0"]));
    assert!(!core.is_window_open(ORIGIN));
    assert!(!core.awaiting_refence(ORIGIN));
}

#[test]
fn source_file_rebinding_the_prefix_refreshes_the_mirror() {
    let mut core = attached_core(&navigator_config());
    assert_eq!(route(&core, "ctrl-a"), KeyRoute::Warp);
    let refresh = core.refresh(ORIGIN);
    assert_eq!(
        refresh,
        vec![
            command(PREFIX_QUERY, CommandPurpose::Prefix),
            command(key_tables::list_keys_command(), CommandPurpose::ListKeys),
        ]
    );
    // A hook message during the refresh is coalesced into one more refresh.
    assert_eq!(core.on_message(ORIGIN, HOOK_MARKER), vec![]);
    core.on_reply(ORIGIN, CommandPurpose::Prefix, &reply(&["C-a\tNone"]));
    let again = core.on_reply(
        ORIGIN,
        CommandPurpose::ListKeys,
        &reply(&["prefix\t0\t1\tselect-window -t :=1"]),
    );
    assert_eq!(again, refresh);
    assert_eq!(
        route(&core, "ctrl-a"),
        tmux_key("C-a", KeyClass::Other, true)
    );
    assert_eq!(route(&core, "ctrl-b"), KeyRoute::Warp);
    assert_eq!(route(&core, "ctrl-h"), KeyRoute::Warp);
}

#[test]
fn warps_hook_message_and_config_errors_refresh_the_mirror() {
    let mut core = attached_core(&navigator_config());
    assert_eq!(core.on_message(ORIGIN, "some other message"), vec![]);
    assert_eq!(core.on_message(ORIGIN, HOOK_MARKER).len(), 2);
}

#[test]
fn a_3_2_server_never_routes_through_tmux_and_says_so_once() {
    let config = KeyConfig {
        prefix: Some("C-b".into()),
        prefix2: None,
        bindings: None,
    };
    let mut core = attached_core(&config);
    for key in ["ctrl-b", "2", "ctrl-h", "a"] {
        // Warp's path: the editor at a prompt, otherwise `send-keys -H`.
        assert_eq!(route(&core, key), KeyRoute::Warp, "{key}");
    }
    assert_eq!(core.refresh(ORIGIN), vec![]);
    let memory = core.check_old_tmux_notice(ORIGIN, &[]).unwrap();
    assert_eq!(memory, "old-tmux:box:/tmp/tmux-501/default");
    let notice = core.notice(ORIGIN).unwrap();
    assert!(notice.text(None).contains("tmux 3.4 or newer on box"));
    // Remembered per server: a later attach shows nothing.
    let mut again = attached_core(&config);
    assert_eq!(again.check_old_tmux_notice(ORIGIN, &[memory]), None);
    assert_eq!(again.notice(ORIGIN), None);
}

#[test]
fn paste_is_reserved_and_never_routed_through_tmux() {
    let reserved = reserved_key_bindings(Some(stroke("ctrl-shift-|")));
    for name in ["terminal:paste", "workspace:toggle_command_palette"] {
        assert!(
            reserved.contains(&ReservedKeyBinding::Named(name.into())),
            "{name}"
        );
    }
    assert!(reserved.contains(&ReservedKeyBinding::CustomAction(
        CustomAction::Paste.into()
    )));
    assert!(reserved.contains(&ReservedKeyBinding::Keystroke(stroke("ctrl-shift-|"))));
    // cmd-v never reaches tmux's key tables, even inside a window.
    let mut core = attached_core(&navigator_config());
    press(&mut core, "ctrl-b");
    assert_eq!(route(&core, "cmd-v"), KeyRoute::Warp);
}

#[test]
fn a_stale_tagged_hook_is_removed_and_ours_installed_on_attach() {
    let mut core = KeyRouterCore::default();
    core.starting(ORIGIN);
    core.key_config_loaded(ORIGIN, &navigator_config());
    core.attached(ORIGIN);
    let scan = core.on_reply(
        ORIGIN,
        CommandPurpose::Identity,
        &reply(&["client-7\tbox\t/tmp/tmux-501/default"]),
    );
    assert_eq!(
        scan,
        vec![
            command(LIST_CLIENTS_COMMAND, CommandPurpose::ListClients),
            command("show-hooks -g after-bind-key", CommandPurpose::ShowHooks),
            command("show-hooks -g after-unbind-key", CommandPurpose::ShowHooks),
            command("show-hooks -g after-set-option", CommandPurpose::ShowHooks),
        ]
    );
    assert_eq!(
        core.on_reply(ORIGIN, CommandPurpose::ListClients, &reply(&["client-7"])),
        vec![]
    );
    let crashed = format!("after-bind-key[4321] display-message -c client-3 {HOOK_MARKER}");
    core.on_reply(ORIGIN, CommandPurpose::ShowHooks, &reply(&[&crashed]));
    core.on_reply(
        ORIGIN,
        CommandPurpose::ShowHooks,
        &reply(&["after-unbind-key"]),
    );
    let out = core.on_reply(ORIGIN, CommandPurpose::ShowHooks, &reply(&[]));
    let lines: Vec<&str> = out
        .iter()
        .map(|out| match out {
            Outbound::Command { line, .. } => line.as_str(),
            Outbound::Key(_) => panic!("unexpected key"),
        })
        .collect();
    let slot = hook_slot("client-7");
    assert_eq!(lines[0], "set-hook -gu after-bind-key[4321]");
    assert_eq!(
        lines[1],
        format!(
            "set-hook -g after-bind-key[{slot}] 'display-message -c \"client-7\" {HOOK_MARKER}'"
        )
    );
    assert_eq!(lines.len(), 4);
    // Removed again when Warp detaches.
    assert_eq!(
        core.detach_commands(ORIGIN)[0],
        format!("set-hook -gu after-bind-key[{slot}]")
    );
    // A second attach (session switch) does not scan again.
    assert_eq!(core.attached(ORIGIN), vec![]);
}

#[test]
fn the_shadowed_keys_notice_lists_ctrl_k_once_per_config() {
    let mut core = attached_core(&navigator_config());
    let warp = [stroke("ctrl-k"), stroke("cmd-k"), stroke("ctrl-r")];
    let uses = |stroke: &Keystroke| warp.contains(stroke);
    assert!(core.needs_shadow_check(ORIGIN));
    let memory = core.check_shadow_notice(ORIGIN, &[], uses).unwrap();
    assert!(memory.starts_with("shadowed:box:/tmp/tmux-501/default:"));
    let mut notice = core.notice(ORIGIN).unwrap().clone();
    assert_eq!(
        notice.kind,
        KeyNoticeKind::Shadowed {
            keys: vec![TmuxKey::parse("C-k").unwrap()]
        }
    );
    assert!(notice.text(Some("ctrl-shift-|")).contains("1 key"));
    notice.expanded = true;
    let expanded = notice.text(Some("ctrl-shift-|"));
    assert!(expanded.contains("ctrl+k"), "{expanded}");
    assert!(expanded.contains("always stay with Warp"), "{expanded}");

    // Not again for the same config in this session...
    assert!(!core.needs_shadow_check(ORIGIN));
    assert_eq!(core.check_shadow_notice(ORIGIN, &[], uses), None);
    // ...nor after a restart.
    let mut restarted = attached_core(&navigator_config());
    assert_eq!(
        restarted.check_shadow_notice(ORIGIN, std::slice::from_ref(&memory), uses),
        None
    );
    assert_eq!(restarted.notice(ORIGIN), None);
    // Dismissing hides it.
    assert!(core.dismiss_notice(ORIGIN));
    assert_eq!(core.notice(ORIGIN), None);
}

#[test]
fn with_tmux_precedence_off_only_the_prefix_and_its_keys_go_to_tmux() {
    let mut core = attached_core(&navigator_config());
    let now = Instant::now();
    assert_eq!(
        core.route(ORIGIN, PANE, &stroke("ctrl-h"), false, now),
        KeyRoute::Warp
    );
    assert_eq!(
        core.route(ORIGIN, PANE, &stroke("ctrl-b"), false, now),
        tmux_key("C-b", KeyClass::Other, true)
    );
    core.key_sent(ORIGIN, now);
    assert_eq!(
        core.route(ORIGIN, PANE, &stroke("2"), false, now),
        tmux_key("2", KeyClass::Other, false)
    );
}

#[test]
fn a_focused_popup_pane_gets_ctrl_j_instead_of_the_navigator() {
    let mut core = attached_core(&navigator_config());
    assert_eq!(
        route(&core, "ctrl-j"),
        tmux_key("C-j", KeyClass::Navigation, true)
    );
    core.set_popup_pane(ORIGIN, Some(PANE));
    let popup = route(&core, "ctrl-j");
    assert_eq!(popup, KeyRoute::Popup);
    assert!(popup.is_claimed());
    // Other panes of the session are unaffected.
    assert_eq!(
        core.route(ORIGIN, PaneId(2), &stroke("ctrl-j"), true, Instant::now()),
        KeyRoute::Tmux(ClientKey {
            key: "C-j".into(),
            pane_id: PaneId(2),
            class: KeyClass::Navigation,
            in_root_table: true,
        })
    );
    core.set_popup_pane(ORIGIN, None);
    assert_ne!(route(&core, "ctrl-j"), KeyRoute::Popup);
}

#[test]
fn the_escape_hatch_keeps_the_next_key_with_warp_briefly() {
    let mut core = attached_core(&navigator_config());
    let now = Instant::now();
    core.bypass_until(now + ESCAPE_HATCH_WINDOW);
    assert_eq!(
        core.route(ORIGIN, PANE, &stroke("ctrl-k"), true, now),
        KeyRoute::Warp
    );
    assert_eq!(
        core.route(
            ORIGIN,
            PANE,
            &stroke("ctrl-k"),
            true,
            now + ESCAPE_HATCH_WINDOW + Duration::from_millis(1)
        ),
        tmux_key("C-k", KeyClass::Navigation, true)
    );
}

#[test]
fn a_key_waiting_on_tmux_is_reported_in_flight_until_acknowledged() {
    let mut core = attached_core(&navigator_config());
    assert_eq!(core.in_flight_since(ORIGIN), None);
    let sent = Instant::now();
    core.key_sent(ORIGIN, sent);
    assert_eq!(core.in_flight_since(ORIGIN), Some(sent));
    core.key_resolved(ORIGIN, Instant::now());
    assert_eq!(core.in_flight_since(ORIGIN), None);
}

#[test]
fn keys_stay_with_warp_before_the_mirror_loads_and_after_the_session_ends() {
    let mut core = KeyRouterCore::default();
    assert_eq!(route(&core, "ctrl-b"), KeyRoute::Warp);
    core.starting(ORIGIN);
    assert_eq!(route(&core, "ctrl-b"), KeyRoute::Warp);
    core.key_config_loaded(ORIGIN, &navigator_config());
    assert_ne!(route(&core, "ctrl-b"), KeyRoute::Warp);
    core.ended(ORIGIN);
    assert_eq!(route(&core, "ctrl-b"), KeyRoute::Warp);
}

/// Drives the router core and the real control client against tmux on an isolated socket:
/// hooks refresh the mirror (R17), `ctrl+h` bound with `if-shell` moves focus and the `ls` typed
/// right after it lands in the newly focused pane (the fence), and `prefix Up Up` with
/// `bind -r Up` moves twice. Skipped without tmux 3.4+.
#[test]
fn live_routing_through_tmux_key_tables() {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::mpsc;

    use command::blocking::Command;
    use warp_terminal::tmux::{
        CONTROL_MODE_DCS, ClientConfig, ClientEvent, CommandId, ControlClient, TmuxVersion,
    };

    let Some(version) = Command::new("tmux")
        .arg("-V")
        .output()
        .ok()
        .and_then(|out| TmuxVersion::parse(&String::from_utf8_lossy(&out.stdout)))
    else {
        eprintln!("skipping: tmux not found");
        return;
    };
    if !version.supports_client_key_commands() {
        eprintln!("skipping: tmux is older than 3.4");
        return;
    }

    let socket = format!("warp-u13-{}", std::process::id());
    struct KillServer(String);
    impl Drop for KillServer {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["-L", &self.0, "kill-server"])
                .output();
        }
    }
    let _cleanup = KillServer(socket.clone());

    let mut child = Command::new("tmux")
        .args(["-L", &socket, "-f", "/dev/null", "-C", "new-session"])
        .args(["-x", "80", "-y", "30", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tmux");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
                Ok(n) => {
                    if tx.send(Some(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    #[derive(Debug, PartialEq)]
    enum Step {
        WaitHooks,
        WaitMirror,
        CtrlH,
        CheckLs,
        PrefixUpUp,
        CheckFocus,
        Done,
    }

    let start = Instant::now();
    let elapsed = || start.elapsed();
    let mut client = ControlClient::new(ClientConfig::default());
    client.receive_bytes(CONTROL_MODE_DCS, elapsed());
    let mut core = KeyRouterCore::default();
    core.starting(ORIGIN);
    let mut purposes: HashMap<CommandId, CommandPurpose> = HashMap::new();
    let mut checks: HashMap<CommandId, &str> = HashMap::new();
    let mut written: Vec<String> = Vec::new();
    let mut step = Step::WaitHooks;
    let mut step_started = Instant::now();
    let mut ls_pane_text = String::new();
    let mut other_pane_text = String::new();
    let mut focused_after_up_up = String::new();

    fn execute(
        client: &mut ControlClient,
        core: &mut KeyRouterCore,
        purposes: &mut HashMap<CommandId, CommandPurpose>,
        out: Vec<Outbound>,
        now: Duration,
    ) {
        for outbound in out {
            match outbound {
                Outbound::Key(key) => {
                    client.send_client_key(key, now).unwrap();
                    core.key_sent(ORIGIN, Instant::now());
                }
                Outbound::Command { line, purpose } => {
                    let id = client.send_command(&line, now).unwrap();
                    purposes.insert(id, purpose);
                }
            }
        }
    }
    let press = |core: &KeyRouterCore, key: &str| -> Vec<Outbound> {
        match core.route(ORIGIN, PaneId(0), &stroke(key), true, Instant::now()) {
            KeyRoute::Tmux(key) => vec![Outbound::Key(key)],
            route => panic!("{key} routed to {route:?}"),
        }
    };

    while start.elapsed() < Duration::from_secs(20) && step != Step::Done {
        for line in client.take_outgoing() {
            stdin.write_all(line.as_bytes()).unwrap();
            written.push(line);
        }
        stdin.flush().unwrap();
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Some(bytes)) => {
                client.receive_bytes(&bytes, elapsed());
            }
            Ok(None) => panic!("tmux exited"),
            Err(_) => client.tick(elapsed()),
        }
        for event in client.take_events() {
            let out = match event {
                ClientEvent::KeyConfigLoaded(config) => {
                    core.key_config_loaded(ORIGIN, &config);
                    vec![]
                }
                ClientEvent::Attached { .. } => core.attached(ORIGIN),
                ClientEvent::Message { text } => core.on_message(ORIGIN, &text),
                ClientEvent::ClientKeyAcknowledged | ClientEvent::ClientKeyDropped { .. } => {
                    core.key_resolved(ORIGIN, Instant::now())
                }
                ClientEvent::CommandCompleted { id, reply } => {
                    if let Some(purpose) = purposes.remove(&id) {
                        core.on_reply(ORIGIN, purpose, &reply)
                    } else {
                        let text = reply
                            .lines
                            .iter()
                            .map(|l| String::from_utf8_lossy(l).into_owned())
                            .collect::<Vec<_>>()
                            .join("\n");
                        match checks.remove(&id) {
                            Some("ls_pane") => ls_pane_text = text,
                            Some("other_pane") => other_pane_text = text,
                            Some("focus") => focused_after_up_up = text,
                            _ => {}
                        }
                        vec![]
                    }
                }
                ClientEvent::Desync { reason } => panic!("desync: {reason}"),
                ClientEvent::AttachFailed { error } => panic!("attach failed: {error:?}"),
                _ => vec![],
            };
            execute(&mut client, &mut core, &mut purposes, out, elapsed());
        }

        let now = elapsed();
        let idle = purposes.is_empty() && client.unacknowledged_client_keys() == 0;
        match step {
            Step::WaitHooks if !core.detach_commands(ORIGIN).is_empty() && idle => {
                for line in [
                    "split-window -v cat",
                    "split-window -v cat",
                    "bind -r Up select-pane -U",
                    "bind -n C-h if-shell 'sleep 0.3' 'select-pane -U' 'send-keys C-h'",
                ] {
                    client.send_command(line, now).unwrap();
                }
                step = Step::WaitMirror;
            }
            Step::WaitMirror => {
                let ready = core.tables(ORIGIN).is_some_and(|tables| {
                    tables
                        .root_binding(&TmuxKey::parse("C-h").unwrap())
                        .is_some()
                        && tables
                            .binding("prefix", &TmuxKey::parse("Up").unwrap())
                            .is_some_and(|bound| bound.repeat)
                });
                if ready && idle {
                    // Focus is on %2, the bottom pane. ctrl+h moves to %1 after 0.3 s.
                    let mut out = press(&core, "ctrl-h");
                    execute(&mut client, &mut core, &mut purposes, out, now);
                    out = press(&core, "l");
                    out.extend(press(&core, "s"));
                    execute(&mut client, &mut core, &mut purposes, out, now);
                    step = Step::CtrlH;
                    step_started = Instant::now();
                }
            }
            Step::CtrlH if !core.is_window_open(ORIGIN) && idle => {
                let id = client.send_command("capture-pane -p -t %1", now).unwrap();
                checks.insert(id, "ls_pane");
                let id = client.send_command("capture-pane -p -t %2", now).unwrap();
                checks.insert(id, "other_pane");
                step = Step::CheckLs;
            }
            Step::CheckLs if checks.is_empty() => {
                client.send_command("select-pane -t %2", now).unwrap();
                for key in ["ctrl-b", "up", "up"] {
                    let out = press(&core, key);
                    execute(&mut client, &mut core, &mut purposes, out, now);
                }
                step = Step::PrefixUpUp;
                step_started = Instant::now();
            }
            Step::PrefixUpUp
                if core.awaiting_refence(ORIGIN)
                    && idle
                    && step_started.elapsed() > Duration::from_millis(600) =>
            {
                // The router's REFENCE_DELAY timer.
                let out = core.refence(ORIGIN);
                execute(&mut client, &mut core, &mut purposes, out, now);
                step_started = Instant::now();
            }
            Step::PrefixUpUp
                if !core.is_window_open(ORIGIN)
                    && idle
                    && step_started.elapsed() > Duration::from_millis(600) =>
            {
                let id = client
                    .send_command("display-message -p '#{pane_id}'", now)
                    .unwrap();
                checks.insert(id, "focus");
                step = Step::CheckFocus;
            }
            Step::CheckFocus if checks.is_empty() => step = Step::Done,
            _ => {}
        }
    }
    let _ = step_started;
    for line in core.detach_commands(ORIGIN) {
        let _ = stdin.write_all(format!("{line}\n").as_bytes());
    }
    client.detach();
    for line in client.take_outgoing() {
        let _ = stdin.write_all(line.as_bytes());
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    if let Some(identity) = core.identity(ORIGIN)
        && identity.socket_path.contains(&socket)
    {
        let _ = std::fs::remove_file(&identity.socket_path);
    }

    assert_eq!(step, Step::Done, "timed out; wrote {written:#?}");
    let client_name = &core.identity(ORIGIN).unwrap().client_name;
    assert!(
        written.contains(&format!("send-keys -K -c \"{client_name}\" \"C-h\"\n")),
        "{written:#?}"
    );
    assert!(
        written.iter().any(|line| line.contains("client_key_table")),
        "{written:#?}"
    );
    assert!(ls_pane_text.contains("ls"), "pane %1: {ls_pane_text:?}");
    assert!(
        !other_pane_text.contains("ls"),
        "pane %2: {other_pane_text:?}"
    );
    assert_eq!(focused_after_up_up.trim(), "%0");
}
