use warp_terminal::tmux::PaneId;

use super::*;

const PANE: PaneId = PaneId(3);
const LAUNCHER: &str = "/home/u/.warp/tmux/warp-shell.sh";

fn rewrite(command: &str) -> Option<String> {
    launcher_binding(command, PANE, Some(LAUNCHER), true).map(|binding| binding.line)
}

#[test]
fn a_split_keeps_its_flags_targets_the_focused_pane_and_starts_the_launcher() {
    assert_eq!(
        rewrite("split-window -h -c \"#{pane_current_path}\"").as_deref(),
        Some(
            "split-window -t %3 -h -c \"#{pane_current_path}\" '/home/u/.warp/tmux/warp-shell.sh'"
        )
    );
    assert_eq!(
        launcher_binding("splitw -v", PANE, Some(LAUNCHER), true),
        Some(LauncherBinding {
            name: "split-window",
            line: "splitw -t %3 -v '/home/u/.warp/tmux/warp-shell.sh'".to_owned(),
        })
    );
    // Flags `list-keys` groups and quotes stay exactly as printed.
    assert_eq!(
        rewrite("split-window -Pbh -F \"#{pane_id}\" -e FOO=1 -l \"30%\"").as_deref(),
        Some(
            "split-window -t %3 -Pbh -F \"#{pane_id}\" -e FOO=1 -l \"30%\" \
             '/home/u/.warp/tmux/warp-shell.sh'"
        )
    );
    // A binding's own target wins.
    assert_eq!(
        rewrite("split-window -fv -t {bottom} -p 20").as_deref(),
        Some("split-window -fv -t {bottom} -p 20 '/home/u/.warp/tmux/warp-shell.sh'")
    );
    assert_eq!(
        rewrite("split-window -dZ --").as_deref(),
        Some("split-window -t %3 -dZ -- '/home/u/.warp/tmux/warp-shell.sh'")
    );
}

#[test]
fn a_new_window_keeps_its_flags_and_starts_the_launcher() {
    assert_eq!(
        rewrite("new-window -c '#{pane_current_path}'").as_deref(),
        Some("new-window -c '#{pane_current_path}' '/home/u/.warp/tmux/warp-shell.sh'")
    );
    assert_eq!(
        launcher_binding("new-window -n foo", PANE, Some(LAUNCHER), true),
        Some(LauncherBinding {
            name: "new-window",
            line: "new-window -n foo '/home/u/.warp/tmux/warp-shell.sh'".to_owned(),
        })
    );
    // tmux refuses a pane as `new-window`'s target, so none is added.
    assert_eq!(
        rewrite("neww -a -c \"#{pane_current_path}\"").as_deref(),
        Some("neww -a -c \"#{pane_current_path}\" '/home/u/.warp/tmux/warp-shell.sh'")
    );
    assert_eq!(
        rewrite("new-window -dk -t :5 -e A=b").as_deref(),
        Some("new-window -dk -t :5 -e A=b '/home/u/.warp/tmux/warp-shell.sh'")
    );
}

#[test]
fn a_binding_that_runs_its_own_command_stays_with_tmux() {
    for command in [
        "split-window -h htop",
        "split-window -h \"htop -d 1\"",
        "new-window -n logs tail -f log",
        "new-window -- sh",
    ] {
        assert_eq!(rewrite(command), None, "{command}");
    }
}

#[test]
fn compound_and_other_commands_stay_with_tmux() {
    for command in [
        "split-window -h \\; select-pane -L",
        "split-window -h ; select-pane -L",
        "select-pane -L \\; split-window -h",
        "if-shell -F \"#{pane_at_left}\" \"split-window -h\" \"split-window -v\"",
        "run-shell \"tmux split-window -h\"",
        "split-window { send-keys x }",
        "select-pane -L",
        "kill-pane",
        "",
    ] {
        assert_eq!(rewrite(command), None, "{command}");
    }
}

#[test]
fn unknown_flags_stay_with_tmux() {
    for command in [
        // `-I` forwards stdin instead of starting a shell.
        "split-window -I",
        "split-window -hk",
        "split-window -m hi",
        "new-window -x",
    ] {
        assert_eq!(rewrite(command), None, "{command}");
    }
}

#[test]
fn without_the_launcher_or_with_integration_off_the_key_goes_to_tmux() {
    let command = "split-window -h -c \"#{pane_current_path}\"";
    assert_eq!(launcher_binding(command, PANE, None, true), None);
    assert_eq!(launcher_binding(command, PANE, Some(LAUNCHER), false), None);
    assert_eq!(launcher_binding("new-window", PANE, Some(""), true), None);
}

/// `bind | split-window -h -c "#{pane_current_path}"` on a real tmux (isolated socket
/// `warp-bind-<pid>-split`), with Warp's control client attached: `prefix` goes through
/// `send-keys -K`, `|` resolves in the prefix table to the split, Warp sends the split with the
/// launcher instead, and tmux goes back to the root table. The new pane starts the launcher, in
/// the focused pane's directory. Skipped without tmux 3.4 or newer.
#[cfg(feature = "local_tty")]
#[test]
fn live_prefix_pipe_splits_through_the_launcher() {
    use std::time::Duration;

    use instant::Instant;
    use warp_terminal::tmux::{CommandReply, KeyConfig};
    use warpui::keymap::Keystroke;

    use crate::terminal::tmux::TmuxOriginId;
    use crate::terminal::tmux::key_tables::{
        PREFIX_QUERY, list_keys_command, parse_list_keys_reply, parse_prefix_reply,
    };
    use crate::terminal::tmux::keys::{KeyRoute, KeyRouterCore, Outbound};
    use crate::terminal::tmux::popup::tests::LiveTmux;

    const ORIGIN: TmuxOriginId = TmuxOriginId(1);
    const WAIT: Duration = Duration::from_secs(5);

    fn answer(tmux: &mut LiveTmux, line: &str) -> CommandReply {
        tmux.control(line, WAIT)
            .unwrap_or_else(|| panic!("{line} answers"))
    }

    /// Runs what the router produced the way `TmuxKeyRouter::execute` does, feeding replies
    /// back. Returns every line written.
    fn drive(tmux: &mut LiveTmux, core: &mut KeyRouterCore, out: Vec<Outbound>) -> Vec<String> {
        let mut written = Vec::new();
        let mut queue: std::collections::VecDeque<Outbound> = out.into();
        while let Some(outbound) = queue.pop_front() {
            match outbound {
                Outbound::Key(key) => {
                    core.note_key(ORIGIN, &key);
                    let client = tmux.client_name.clone();
                    let line = format!(
                        "send-keys -K -c {} {}",
                        warp_terminal::tmux::quote_argument(&client),
                        warp_terminal::tmux::quote_argument(&key.key)
                    );
                    core.key_sent(ORIGIN, Instant::now());
                    answer(tmux, &line);
                    written.push(line);
                    queue.extend(core.key_resolved(ORIGIN, Instant::now()));
                }
                Outbound::Command { line, purpose } => {
                    let reply = answer(tmux, &line);
                    written.push(line);
                    queue.extend(core.on_reply(ORIGIN, purpose, &reply));
                }
            }
        }
        written
    }

    let Some(mut tmux) =
        LiveTmux::start_on_socket(format!("warp-bind-{}-split", std::process::id()))
    else {
        return;
    };
    let launcher = tmux.home().join("launcher.sh");
    std::fs::write(
        &launcher,
        "#!/bin/sh\ntmux -S \"${TMUX%%,*}\" set-option -p @warp_test launched\nexec /bin/sh\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let launcher = launcher.to_string_lossy().into_owned();
    let cwd = tmux.home().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = std::fs::canonicalize(cwd)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    tmux.run(&[
        "bind-key",
        "|",
        "split-window",
        "-h",
        "-c",
        "#{pane_current_path}",
    ]);
    let pane = PaneId::parse(&tmux.stdout(&["display-message", "-p", "#{pane_id}"])).unwrap();
    tmux.type_line(&pane.to_string(), &format!("cd '{cwd}'"));
    tmux.wait_for("the pane to change directory", || {
        tmux.stdout(&[
            "display-message",
            "-p",
            "-t",
            &pane.to_string(),
            "#{pane_current_path}",
        ]) == cwd
    });

    // The router, attached as Warp attaches.
    let mut core = KeyRouterCore::default();
    core.starting(ORIGIN);
    let (prefix, prefix2) = parse_prefix_reply(&answer(&mut tmux, PREFIX_QUERY));
    let bindings = parse_list_keys_reply(&answer(&mut tmux, &list_keys_command()));
    core.key_config_loaded(
        ORIGIN,
        &KeyConfig {
            prefix,
            prefix2,
            bindings: Some(bindings),
        },
    );
    let out = core.attached(ORIGIN);
    let mut written = drive(&mut tmux, &mut core, out);

    let route = |core: &KeyRouterCore, key: &str| match core.route(
        ORIGIN,
        pane,
        &Keystroke::parse(key).unwrap(),
        true,
        Instant::now(),
    ) {
        KeyRoute::Tmux(key) => key,
        route => panic!("{key} routed to {route:?}"),
    };
    let prefix = route(&core, "ctrl-b");
    written.extend(drive(&mut tmux, &mut core, vec![Outbound::Key(prefix)]));
    let pipe = route(&core, "|");
    let bound = core
        .bound_command(ORIGIN, &pipe)
        .map(|bound| bound.command.clone())
        .expect("| is bound in the prefix table");
    let binding = launcher_binding(&bound, pane, Some(&launcher), true)
        .unwrap_or_else(|| panic!("{bound} starts the default shell"));
    assert_eq!(binding.name, "split-window");
    let reply = answer(&mut tmux, &binding.line);
    assert!(!reply.error, "{}: {:?}", binding.line, reply.lines);
    written.push(binding.line.clone());
    let out = core.intercepted(ORIGIN);
    written.extend(drive(&mut tmux, &mut core, out));

    assert!(
        !written.iter().any(|line| line.contains("\"|\"")),
        "| never reaches tmux as a key: {written:#?}"
    );
    assert!(
        written.contains(&format!(
            "switch-client -c {} -T root",
            warp_terminal::tmux::quote_argument(&tmux.client_name)
        )),
        "{written:#?}"
    );
    assert!(!core.is_window_open(ORIGIN), "{written:#?}");
    assert_eq!(
        tmux.stdout(&["display-message", "-p", "#{client_key_table}"]),
        "root"
    );
    let new_pane = || -> Option<(String, String, String)> {
        tmux.stdout(&[
            "list-panes",
            "-F",
            "#{pane_id}\t#{pane_start_command}\t#{pane_current_path}\t#{@warp_test}",
        ])
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            (fields.first() != Some(&pane.to_string().as_str())).then(|| {
                (
                    fields.get(1).copied().unwrap_or_default().to_owned(),
                    fields.get(2).copied().unwrap_or_default().to_owned(),
                    fields.get(3).copied().unwrap_or_default().to_owned(),
                )
            })
        })
        .next()
    };
    tmux.wait_for("the launcher to run in the new pane", || {
        new_pane().is_some_and(|(_, _, marker)| marker == "launched")
    });
    let (start_command, path, _) = new_pane().unwrap();
    assert!(
        start_command.contains(&launcher),
        "start command {start_command:?}"
    );
    assert_eq!(path, cwd);
}
