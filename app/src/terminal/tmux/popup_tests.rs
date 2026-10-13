use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use command::blocking::Command;
use instant::Instant;
use warp_terminal::tmux::{CommandReply, PaneId};

use super::*;
use crate::terminal::tmux::native_ui::{Classified, Interactive, classify};

fn reply(lines: &[&str]) -> CommandReply {
    CommandReply {
        time: 0,
        number: 1,
        flags: 1,
        error: false,
        lines: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
    }
}

#[test]
fn popup_windows_are_recognized_by_name() {
    assert!(is_popup_window_name("warp-popup"));
    assert!(is_popup_window_name("warp-popup:Pick a file"));
    assert!(!is_popup_window_name("warp-popups"));
    assert!(!is_popup_window_name("vim"));
    assert_eq!(popup_title("warp-popup:Pick"), Some("Pick".to_owned()));
    assert_eq!(popup_title("warp-popup"), None);
    assert_eq!(popup_window_name(Some(" t ")), "warp-popup:t");
    assert_eq!(popup_window_name(None), "warp-popup");
}

/// Covers AE9: a binding that runs `display-popup -E 'fzf'` becomes a detached window running
/// the same command, named so Warp shows it as a floating pane.
#[test]
fn a_bound_popup_runs_in_a_detached_named_window() {
    let Classified::Native(Interactive::Popup(spec)) = classify("display-popup -E fzf") else {
        panic!("display-popup is drawn natively");
    };
    assert_eq!(
        bound_popup_command(&spec),
        "new-window -d -P -F '#{window_id}' -n \"warp-popup\" fzf"
    );
    let Classified::Native(Interactive::Popup(spec)) =
        classify("display-popup -E -d \"#{pane_current_path}\" -e A=1 -T Files \"htop -d 5\"")
    else {
        panic!("display-popup is drawn natively");
    };
    assert_eq!(
        bound_popup_command(&spec),
        "new-window -d -P -F '#{window_id}' -n \"warp-popup:Files\" -c \"#{pane_current_path}\" \
         -e A=1 \"htop -d 5\""
    );
    assert_eq!(parse_window_id(&reply(&["@12"])), Some("@12".to_owned()));
    assert_eq!(parse_window_id(&reply(&["oops"])), None);
    // Closing a popup has nothing to draw; tmux runs it.
    assert_eq!(classify("display-popup -C"), Classified::Plain);
}

#[test]
fn the_wrapper_dir_goes_first_on_path_once() {
    let dir = "/home/me/.cache/warp/tmux/0123456789abcdef/bin";
    let old = "/home/me/.cache/warp/tmux/fedcba9876543210/bin";
    assert!(is_wrapper_dir(dir));
    assert!(is_wrapper_dir(&format!("{dir}/")));
    assert!(!is_wrapper_dir("/usr/local/bin"));
    assert!(!is_wrapper_dir("/home/me/.cache/warp/other/bin"));
    assert_eq!(
        path_with_wrapper("/usr/bin:/bin", dir),
        format!("{dir}:/usr/bin:/bin")
    );
    // A crashed Warp's (or another version's) wrapper is replaced, not stacked.
    assert_eq!(
        path_with_wrapper(&format!("{old}:/usr/bin:{dir}"), dir),
        format!("{dir}:/usr/bin")
    );
    assert_eq!(
        path_without_wrapper(&format!("{dir}:/usr/bin:/bin")),
        "/usr/bin:/bin"
    );
    assert_eq!(
        wrapper_dir("/home/me/.cache/warp/tmux/0123456789abcdef/warp-shell.sh"),
        Some(dir.to_owned())
    );
    assert_eq!(
        parse_global_path(&reply(&["PATH=/usr/bin:/bin"])),
        Some("/usr/bin:/bin".to_owned())
    );
    assert_eq!(parse_global_path(&reply(&["-PATH"])), None);
    assert_eq!(
        set_global_path_command("/a b:/c$d"),
        "set-environment -g PATH \"/a b:/c\\$d\""
    );
}

#[test]
fn the_probe_is_one_safe_tmux_line_each() {
    let commands = probe_commands();
    let script = commands[0]
        .strip_prefix("run-shell '")
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("run-shell");
    assert!(!script.contains('#') && !script.contains('\''), "{script}");
    for command in &commands {
        assert!(!command.contains('\n'), "{command}");
    }
}

#[test]
fn hidden_popups_are_terminal_children_that_are_not_panes() {
    let children = parse_probe(&reply(&["100 /usr/local/bin/fzf,200 -zsh,300 bash,"]));
    assert_eq!(
        children,
        vec![
            ServerChild {
                pid: 100,
                command: "fzf".into()
            },
            ServerChild {
                pid: 200,
                command: "-zsh".into()
            },
            ServerChild {
                pid: 300,
                command: "bash".into()
            },
        ]
    );
    let panes = parse_pane_pids(&reply(&["200", "300"]));
    assert_eq!(panes, HashSet::from([200, 300]));
    let control_latest = parse_clients(&reply(&["0\t10\t/dev/ttys001", "1\t20\tclient-9"]));
    let found = hidden_popups(&children, &panes, &control_latest);
    assert_eq!(found.iter().map(|c| c.pid).collect::<Vec<_>>(), vec![100]);
    // A popup that landed on a plain client is drawn there.
    let plain_latest = parse_clients(&reply(&["0\t30\t/dev/ttys001", "1\t20\tclient-9"]));
    assert!(hidden_popups(&children, &panes, &plain_latest).is_empty());
    // With only control clients, any popup is hidden.
    assert_eq!(hidden_popups(&children, &panes, &[]).len(), 1);
}

#[test]
fn a_hidden_popup_is_offered_after_two_probes_and_cleared_when_gone() {
    let fzf = ServerChild {
        pid: 100,
        command: "fzf".into(),
    };
    let mut watch = HiddenPopupWatch::default();
    assert_eq!(watch.on_probe(vec![fzf.clone()]), None);
    assert_eq!(
        watch.on_probe(vec![fzf.clone()]),
        Some(HiddenPopupChange::Found(fzf.clone()))
    );
    assert_eq!(watch.on_probe(vec![fzf.clone()]), None);
    assert_eq!(watch.offered(), Some(&fzf));
    assert_eq!(watch.on_probe(Vec::new()), Some(HiddenPopupChange::Gone));
    assert_eq!(watch.on_probe(Vec::new()), None);
    // A popup that closed by itself before the second probe never raises the offer.
    assert_eq!(watch.on_probe(vec![fzf]), None);
    assert_eq!(watch.on_probe(Vec::new()), None);
    assert!(
        hidden_popup_text(&ServerChild {
            pid: 1,
            command: "fzf".into()
        })
        .contains("(fzf)")
    );
}

#[test]
fn close_commands_target_warps_client() {
    assert_eq!(
        close_popup_command("client-9"),
        "display-popup -C -c \"client-9\""
    );
    assert_eq!(
        popup_window_options_command("@3"),
        "set-option -w -t @3 remain-on-exit off ; \
         if-shell -F -t @3 \"#{pane_dead}\" \"kill-window -t @3\""
    );
}

// Live tests against a real tmux on an isolated socket (`warp-u14-<pid>-<name>`, `-f /dev/null`,
// a temporary $HOME) with a control client attached, as Warp is. The server is killed and its
// socket removed afterwards; the user's tmux is never touched.

pub(crate) struct LiveTmux {
    pub socket: String,
    pub socket_path: String,
    pub client_name: String,
    dir: tempfile::TempDir,
    control: Option<(Child, ChildStdin)>,
    lines: Arc<Mutex<Vec<String>>>,
    /// Commands sent on the control client; their replies arrive in order.
    sent: usize,
}

impl LiveTmux {
    /// `None` (the test is skipped) without tmux 3.4 or newer.
    pub fn start(name: &str) -> Option<Self> {
        let version = Command::new("tmux").arg("-V").output().ok()?;
        let version = String::from_utf8_lossy(&version.stdout).to_string();
        let number: f32 = version
            .split_whitespace()
            .nth(1)?
            .trim_end_matches(char::is_alphabetic)
            .parse()
            .ok()?;
        if number < 3.4 {
            eprintln!("skipping: {version}");
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let mut tmux = Self {
            socket: format!("warp-u14-{}-{name}", std::process::id()),
            socket_path: String::new(),
            client_name: String::new(),
            dir,
            control: None,
            lines: Arc::default(),
            sent: 0,
        };
        let shell = tmux.shell_command();
        let started = tmux.run(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "100",
            "-y",
            "30",
            &shell,
        ]);
        assert!(started.status.success(), "tmux starts");
        tmux.socket_path = tmux.stdout(&["display-message", "-p", "#{socket_path}"]);
        let mut child = tmux
            .command()
            .args(["-C", "attach", "-t", "s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let lines = tmux.lines.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                lines.lock().unwrap().push(line);
            }
        });
        tmux.control = Some((child, stdin));
        let name = tmux
            .control(
                "display-message -p '#{client_name}'",
                Duration::from_secs(5),
            )
            .expect("control client answers");
        tmux.client_name = String::from_utf8_lossy(&name.lines[0]).trim().to_owned();
        Some(tmux)
    }

    /// The pane shell: a plain `sh` with a fixed prompt, in the temporary home.
    pub fn shell_command(&self) -> String {
        format!("env HOME='{}' PS1='u14$ ' /bin/sh", self.home().display())
    }

    pub fn home(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .env("HOME", self.home())
            .env_remove("TMUX")
            .stdin(Stdio::null());
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("tmux runs")
    }

    pub fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stdout)
            .trim()
            .to_owned()
    }

    /// Sends `line` on the control client, as Warp does, and waits up to `timeout` for its
    /// reply. `None` on timeout; a later reply still belongs to this command.
    pub fn control(&mut self, line: &str, timeout: Duration) -> Option<CommandReply> {
        let (_, stdin) = self.control.as_mut()?;
        writeln!(stdin, "{line}").ok()?;
        stdin.flush().ok()?;
        self.sent += 1;
        self.reply(self.sent - 1, timeout)
    }

    /// The index of the last command sent on the control client.
    pub fn last_index(&self) -> usize {
        self.sent.saturating_sub(1)
    }

    /// Waits for the reply to the `index`th command sent on the control client.
    pub fn reply(&self, index: usize, timeout: Duration) -> Option<CommandReply> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(reply) = self.find_reply(index) {
                return Some(reply);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn find_reply(&self, index: usize) -> Option<CommandReply> {
        let lines = self.lines.lock().unwrap();
        let mut seen = 0;
        let mut current: Option<(String, Vec<Vec<u8>>)> = None;
        for line in lines.iter() {
            if let Some((guard, body)) = current.as_mut() {
                let ended = line.starts_with("%end ") || line.starts_with("%error ");
                if ended
                    && line
                        .split_whitespace()
                        .skip(1)
                        .collect::<Vec<_>>()
                        .join(" ")
                        == *guard
                {
                    if seen == index {
                        return Some(CommandReply {
                            time: 0,
                            number: 0,
                            flags: 1,
                            error: line.starts_with("%error"),
                            lines: std::mem::take(body),
                        });
                    }
                    seen += 1;
                    current = None;
                } else {
                    body.push(line.as_bytes().to_vec());
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("%begin ")
                && rest.ends_with(" 1")
            {
                current = Some((rest.to_owned(), Vec::new()));
            }
        }
        None
    }

    /// Every notification line the control client received.
    pub fn notifications(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with('%') && !line.starts_with("%output"))
            .cloned()
            .collect()
    }

    /// [`Self::wait_for`] for checks that talk to the control client.
    pub fn poll(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }

    pub fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }

    /// Types a shell command into `pane` (as a plain client would) and presses Enter.
    pub fn type_line(&self, pane: &str, line: &str) {
        self.run(&["send-keys", "-t", pane, "-l", line]);
        self.run(&["send-keys", "-t", pane, "Enter"]);
    }

    /// The wrapper, installed in the temporary home as Warp installs it beside the launcher.
    pub fn install_wrapper(&self) -> PathBuf {
        let bin = self.home().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let wrapper = bin.join("tmux");
        std::fs::write(
            &wrapper,
            include_bytes!("../../../assets/bundled/tmux/tmux").as_slice(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    /// `(window name, pane id)` of every window.
    pub fn windows(&self) -> Vec<(String, String)> {
        self.stdout(&["list-windows", "-a", "-F", "#{window_name}\t#{pane_id}"])
            .lines()
            .filter_map(|line| {
                let (name, pane) = line.split_once('\t')?;
                Some((name.to_owned(), pane.to_owned()))
            })
            .collect()
    }

    pub fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.home().join(name)).unwrap_or_default()
    }

    /// Runs the hidden-popup probe over the control client, as Warp does.
    pub fn probe(&mut self) -> Vec<ServerChild> {
        let replies: Vec<CommandReply> = probe_commands()
            .iter()
            .map(|line| {
                self.control(line, Duration::from_secs(5))
                    .expect("probe answers")
            })
            .collect();
        hidden_popups(
            &parse_probe(&replies[1]),
            &parse_pane_pids(&replies[2]),
            &parse_clients(&replies[3]),
        )
    }
}

impl Drop for LiveTmux {
    fn drop(&mut self) {
        if let Some((mut child, _)) = self.control.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.command().arg("kill-server").output();
        if self.socket_path.contains("warp-u14-") {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

pub(crate) fn which(program: &str) -> Option<String> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!path.is_empty()).then_some(path)
}

/// A script running `fzf-tmux -p` opens a floating Warp pane (a `warp-popup` window), blocks
/// until a selection, and returns the selection.
#[test]
fn live_fzf_tmux_through_the_wrapper_returns_the_selection() {
    if which("fzf").is_none() || which("fzf-tmux").is_none() {
        eprintln!("skipping: fzf is not installed");
        return;
    }
    let Some(mut tmux) = LiveTmux::start("fzf") else {
        return;
    };
    let bin = tmux.install_wrapper();
    let out = tmux.home().join("out");
    tmux.type_line(
        "s:0",
        &format!(
            "export PATH='{}':\"$PATH\"; printf 'alpha\\nbeta\\n' | fzf-tmux -p 80%,60% > '{}'; \
             echo rc=$? >> '{}'",
            bin.display(),
            out.display(),
            out.display()
        ),
    );
    let mut popup_pane = String::new();
    tmux.wait_for("the popup window", || {
        match tmux
            .windows()
            .into_iter()
            .find(|(name, _)| is_popup_window_name(name))
        {
            Some((_, pane)) => {
                popup_pane = pane;
                true
            }
            None => false,
        }
    });
    // The script is still blocked: nothing written yet.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(tmux.file("out"), "");
    // The popup window is a pane, so the probe does not report it as hidden.
    assert!(tmux.probe().is_empty());
    // Keys reach the popup's pane as typed input, as Warp sends them to a focused popup.
    tmux.run(&["send-keys", "-t", &popup_pane, "-l", "bet"]);
    std::thread::sleep(Duration::from_millis(300));
    tmux.run(&["send-keys", "-t", &popup_pane, "Enter"]);
    tmux.wait_for("the selection", || tmux.file("out").contains("rc="));
    assert_eq!(tmux.file("out"), "beta\nrc=0\n");
    tmux.wait_for("the popup window to close", || {
        !tmux
            .windows()
            .iter()
            .any(|(name, _)| is_popup_window_name(name))
    });
}

/// The wrapper blocks until the popup's command exits, passes its exit status through, and
/// leaves no window behind.
#[test]
fn live_the_wrapper_passes_the_popup_exit_status_through() {
    let Some(tmux) = LiveTmux::start("status") else {
        return;
    };
    let bin = tmux.install_wrapper();
    tmux.type_line(
        "s:0",
        &format!(
            "export PATH='{}':\"$PATH\"; tmux display-popup -E -d /tmp -T Hi 'pwd > \"$HOME/pwd\"; \
             sleep 1; exit 3'; echo rc=$? > \"$HOME/rc\"",
            bin.display()
        ),
    );
    tmux.wait_for("the titled popup window", || {
        tmux.windows()
            .iter()
            .any(|(name, _)| name == "warp-popup:Hi")
    });
    assert_eq!(
        tmux.file("rc"),
        "",
        "the caller blocks while the popup runs"
    );
    tmux.wait_for("the caller", || !tmux.file("rc").is_empty());
    assert_eq!(tmux.file("rc"), "rc=3\n");
    assert!(
        tmux.file("pwd").trim().ends_with("/tmp"),
        "{}",
        tmux.file("pwd")
    );
    assert!(
        !tmux
            .windows()
            .iter()
            .any(|(name, _)| is_popup_window_name(name))
    );
    // Everything else reaches the real tmux.
    tmux.type_line("s:0", "tmux rename-window wrapped; echo ok > \"$HOME/ok\"");
    tmux.wait_for("the plain command", || !tmux.file("ok").is_empty());
    assert!(tmux.windows().iter().any(|(name, _)| name == "wrapped"));
}

/// A popup started by calling tmux by absolute path is detected as hidden and offered for
/// closing, and keys sent meanwhile (as typed input, `send-keys -H`) are not swallowed.
#[test]
fn live_an_absolute_path_popup_is_detected_and_keys_are_not_swallowed() {
    let Some(mut tmux) = LiveTmux::start("hidden") else {
        return;
    };
    let real = which("tmux").expect("tmux");
    let typed = tmux.home().join("typed");
    let other = tmux.stdout(&[
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "s:0",
        &format!("cat > '{}'", typed.display()),
    ]);
    assert!(tmux.probe().is_empty(), "no popup yet");
    tmux.type_line(
        "s:0.0",
        &format!("'{real}' display-popup -E 'sleep 30'; echo closed > \"$HOME/closed\""),
    );
    let mut watch = HiddenPopupWatch::default();
    let mut found = None;
    tmux.poll("the hidden popup", |tmux| {
        if let Some(HiddenPopupChange::Found(child)) = watch.on_probe(tmux.probe()) {
            found = Some(child);
        }
        found.is_some()
    });
    let found = found.unwrap();
    assert!(
        found.command.contains("sh") || found.command.contains("sleep"),
        "{found:?}"
    );
    // With the popup open, Warp sends typed input with `send-keys -H` to the pane, which the
    // popup cannot take.
    let reply = tmux
        .control(
            &format!("send-keys -t {other} -H 68 69 0a"),
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(!reply.error);
    tmux.wait_for("typed input", || tmux.file("typed") == "hi\n");
    // "Close hidden popup".
    let client = tmux.client_name.clone();
    let reply = tmux
        .control(&close_popup_command(&client), Duration::from_secs(5))
        .unwrap();
    assert!(!reply.error);
    tmux.wait_for("the script to continue", || {
        tmux.file("closed") == "closed\n"
    });
    tmux.poll("the popup to be gone", |tmux| {
        watch.on_probe(tmux.probe()) == Some(HiddenPopupChange::Gone)
    });
    let _ = PaneId::parse(&other);
}

/// A bound popup whose command exits before Warp sets the window's options leaves no window,
/// even with `remain-on-exit` on.
#[test]
fn live_a_popup_that_exits_at_once_leaves_no_window() {
    let Some(mut tmux) = LiveTmux::start("instant") else {
        return;
    };
    tmux.run(&["set-option", "-g", "remain-on-exit", "on"]);
    let Classified::Native(Interactive::Popup(spec)) = classify("display-popup -E true") else {
        panic!("display-popup is drawn natively");
    };
    let reply = tmux
        .control(&bound_popup_command(&spec), Duration::from_secs(5))
        .unwrap();
    let window = parse_window_id(&reply).expect("window id");
    std::thread::sleep(Duration::from_millis(300));
    let reply = tmux
        .control(
            &popup_window_options_command(&window),
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(!reply.error, "{:?}", reply.lines);
    tmux.wait_for("the dead popup window to go", || {
        !tmux
            .windows()
            .iter()
            .any(|(name, _)| is_popup_window_name(name))
    });
}

/// Covers AE9: a `display-popup -E 'fzf'` binding opens a floating pane (a `warp-popup` window
/// running fzf), and the temporary window is gone after fzf exits.
#[test]
fn live_a_bound_popup_window_closes_when_its_command_exits() {
    let Some(mut tmux) = LiveTmux::start("bound") else {
        return;
    };
    let program = if which("fzf").is_some() {
        "fzf"
    } else {
        "'sh -c \"read x\"'"
    };
    let Classified::Native(Interactive::Popup(spec)) =
        classify(&format!("display-popup -E {program}"))
    else {
        panic!("display-popup is drawn natively");
    };
    tmux.run(&["set-option", "-g", "remain-on-exit", "on"]);
    let current = tmux.stdout(&["display-message", "-p", "#{window_id}"]);
    let reply = tmux
        .control(&bound_popup_command(&spec), Duration::from_secs(5))
        .expect("new-window answers");
    let window = parse_window_id(&reply).expect("window id");
    let reply = tmux
        .control(
            &popup_window_options_command(&window),
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(!reply.error);
    let pane = tmux.stdout(&["display-message", "-p", "-t", &window, "#{pane_id}"]);
    assert!(
        tmux.windows()
            .iter()
            .any(|(name, id)| is_popup_window_name(name) && *id == pane)
    );
    // The window stays detached: the session's current window did not change.
    assert_eq!(
        tmux.stdout(&["display-message", "-p", "#{window_id}"]),
        current
    );
    tmux.wait_for("the popup's program to start", || {
        !tmux
            .stdout(&["capture-pane", "-p", "-t", &pane])
            .trim()
            .is_empty()
    });
    // C-c quits fzf; Enter answers the `read` fallback.
    let quit = if program == "fzf" { "C-c" } else { "Enter" };
    tmux.run(&["send-keys", "-t", &pane, quit]);
    tmux.wait_for("the popup window to close", || {
        !tmux
            .windows()
            .iter()
            .any(|(name, _)| is_popup_window_name(name))
    });
}
