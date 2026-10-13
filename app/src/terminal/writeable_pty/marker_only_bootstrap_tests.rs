//! Live checks that Warp's marker-only bootstrap (the one tmux panes source through the launcher)
//! leaves the user's line editor alone (R10, KTD8): no Warp key bindings, no widget overrides,
//! and the clear-line keys Warp sends to a tmux pane work in the unmodified editor.
//!
//! Each test starts its own tmux server on an isolated socket with `-f /dev/null` and a temporary
//! $HOME, and kills it afterwards; the user's tmux is never touched. A test is skipped when tmux
//! 3.2+ or its shell is missing.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use command::blocking::Command;
use instant::Instant;

use crate::terminal::tmux::integration::{
    HOOK_TOKEN_OPTION, install_locally, launcher_bundle, launcher_shell_command, new_hook_token,
};

/// The bytes Warp writes ahead of a command in a tmux pane: Ctrl-E, Ctrl-U.
const TMUX_CLEAR_LINE_HEX: [&str; 2] = ["05", "15"];

struct Shell {
    name: &'static str,
    prompt: &'static str,
    /// Starts the shell without Warp, as a plain tmux user would have it.
    plain_command: &'static str,
    /// Writes every key binding (and the widget/option state the bootstrap could change) to
    /// `~/out-$1`, atomically.
    dump_bindings: &'static str,
    /// Switches the running shell to vi insert mode, when the shell has one.
    vi_mode: Option<&'static str>,
}

const ZSH: Shell = Shell {
    name: "zsh",
    prompt: "fz> ",
    plain_command: "exec zsh -i",
    dump_bindings: "{ bindkey -LM emacs; bindkey -LM viins; bindkey -LM vicmd; bindkey -lL main; \
                    zle -lL bracketed-paste; [[ -o sharehistory ]] && echo sharehistory; } \
                    > ~/tmp-LABEL 2>&1; mv ~/tmp-LABEL ~/out-LABEL",
    vi_mode: Some("bindkey -v"),
};

const BASH: Shell = Shell {
    name: "bash",
    prompt: "fb> ",
    plain_command: "exec bash -i",
    dump_bindings: "{ bind -p; bind -X; bind -s; bind -m vi-insert -p; bind -m vi-insert -X; \
                    bind -m vi-command -p; bind -m vi-command -X; } > ~/tmp-LABEL 2>&1; \
                    mv ~/tmp-LABEL ~/out-LABEL",
    vi_mode: Some("set -o vi"),
};

const FISH: Shell = Shell {
    name: "fish",
    prompt: "ff> ",
    plain_command: "exec fish -i",
    dump_bindings: "begin; bind; bind -M insert; echo $fish_key_bindings; end > ~/tmp-LABEL 2>&1; \
                    mv ~/tmp-LABEL ~/out-LABEL",
    vi_mode: Some("fish_vi_key_bindings"),
};

struct LiveTmux {
    socket: String,
    home: tempfile::TempDir,
}

impl LiveTmux {
    fn start(name: &str) -> Option<Self> {
        let version = Command::new("tmux").arg("-V").output().ok()?;
        let version = String::from_utf8_lossy(&version.stdout).to_string();
        let number: f32 = version
            .split_whitespace()
            .nth(1)?
            .trim_end_matches(char::is_alphabetic)
            .parse()
            .ok()?;
        if number < 3.2 {
            eprintln!("skipping: {version}");
            return None;
        }
        let tmux = Self {
            socket: format!("warp-fix-{}-{name}", std::process::id()),
            home: tempfile::tempdir().ok()?,
        };
        let home = tmux.home.path();
        // A user widget for bracketed-paste, which the bootstrap must not replace.
        std::fs::write(
            home.join(".zshrc"),
            "PS1='fz> '\nHISTFILE=~/.zsh_history\nHISTSIZE=100\nSAVEHIST=100\n\
             my_paste() { zle .bracketed-paste }\nzle -N bracketed-paste my_paste\n",
        )
        .ok()?;
        std::fs::write(home.join(".bashrc"), "PS1='fb> '\n").ok()?;
        std::fs::write(
            home.join(".bash_profile"),
            "[ -f ~/.bashrc ] && . ~/.bashrc\n",
        )
        .ok()?;
        std::fs::create_dir_all(home.join(".config/fish")).ok()?;
        std::fs::write(
            home.join(".config/fish/config.fish"),
            "function fish_prompt; echo -n 'ff> '; end\nset -g fish_greeting\n",
        )
        .ok()?;
        // The first session keeps the server alive.
        tmux.run(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "120",
            "-y",
            "30",
            "sleep 600",
        ]);
        Some(tmux)
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .env("HOME", self.home.path())
            .env_remove("TMUX")
            .env_remove("ZDOTDIR")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("tmux runs")
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stdout).to_string()
    }

    /// Starts `command` in a new window, with its raw output captured. Returns the pane id and
    /// the capture file.
    fn new_pane(&self, command: &str, label: &str) -> (String, PathBuf) {
        let pane = self
            .stdout(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "s",
                &format!("sleep 0.5; {command}"),
            ])
            .trim()
            .to_owned();
        assert!(pane.starts_with('%'), "a pane id: {pane:?}");
        let raw = self.home().join(format!("raw-{label}"));
        self.run(&[
            "pipe-pane",
            "-o",
            "-t",
            &pane,
            &format!("cat >> '{}'", raw.display()),
        ]);
        (pane, raw)
    }

    fn view(&self, pane: &str) -> String {
        self.stdout(&["capture-pane", "-p", "-t", pane])
    }

    /// The last non-blank line on the pane's screen.
    fn last_line(&self, pane: &str) -> String {
        self.view(pane)
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    }

    fn type_text(&self, pane: &str, text: &str) {
        self.run(&["send-keys", "-t", pane, "-l", text]);
    }

    fn keys(&self, pane: &str, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", pane];
        args.extend_from_slice(keys);
        self.run(&args);
    }

    fn hex(&self, pane: &str, bytes: &[&str]) {
        let mut args = vec!["send-keys", "-t", pane, "-H"];
        args.extend_from_slice(bytes);
        self.run(&args);
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}");
    }

    fn wait_for_prompt(&self, pane: &str, prompt: &str) {
        self.wait_for(&format!("the prompt in {pane}"), || {
            self.last_line(pane) == prompt.trim_end()
        });
    }

    /// Runs `shell`'s binding dump in `pane` and returns it.
    fn dump_bindings(&self, shell: &Shell, pane: &str, label: &str) -> String {
        self.type_text(pane, &shell.dump_bindings.replace("LABEL", label));
        self.keys(pane, &["Enter"]);
        let out = self.home().join(format!("out-{label}"));
        self.wait_for(&format!("the {label} binding dump"), || out.exists());
        std::fs::read_to_string(out).unwrap()
    }
}

impl Drop for LiveTmux {
    fn drop(&mut self) {
        let socket = self.stdout(&["display-message", "-p", "#{socket_path}"]);
        let _ = self.command().arg("kill-server").output();
        let socket = socket.trim();
        if socket.contains("warp-fix-") {
            let _ = std::fs::remove_file(socket);
        }
    }
}

fn which(program: &str) -> Option<String> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("command -v {program}"))
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!path.is_empty()).then_some(path)
}

/// The names of the Warp hooks (DCS-encoded JSON) in raw pane output.
fn hook_names(raw: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = raw;
    while let Some(start) = find(rest, b"\x1bP$d") {
        let body = &rest[start + 4..];
        let Some(end) = find(body, b"\x1b\\") else {
            break;
        };
        if let Ok(bytes) = hex::decode(&body[..end])
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(name) = json["hook"].as_str()
        {
            names.push(name.to_owned());
        }
        rest = &body[end..];
    }
    names
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A marker-only launcher pane for `shell`: it still emits Warp's hooks, its key bindings are
/// exactly a plain shell's, a typed Ctrl-P recalls history, and the clear Warp sends before a
/// command empties a partly typed line in both emacs and vi insert mode.
fn marker_only_bootstrap_leaves_the_line_editor_alone(shell: &Shell) {
    let Some(path) = which(shell.name) else {
        eprintln!("skipping: no {}", shell.name);
        return;
    };
    let Some(tmux) = LiveTmux::start(shell.name) else {
        return;
    };
    let token = new_hook_token();
    tmux.run(&["set-option", "-g", HOOK_TOKEN_OPTION, &token]);
    tmux.run(&["set-option", "-g", "default-shell", &path]);
    let launcher = install_locally(&launcher_bundle(&crate::ASSETS), tmux.home())
        .expect("the launcher installs");
    let launcher = launcher_shell_command(launcher.to_str().unwrap());

    let (warp, raw) = tmux.new_pane(&format!("exec {launcher}"), "warp");
    let (plain, _) = tmux.new_pane(shell.plain_command, "plain");
    tmux.wait_for_prompt(&warp, shell.prompt);
    tmux.wait_for_prompt(&plain, shell.prompt);

    // The launcher pane is integrated: it announced itself and reports its prompts.
    tmux.wait_for("Warp's hooks", || {
        let names = hook_names(&std::fs::read(&raw).unwrap_or_default());
        names.iter().any(|name| name == "Bootstrapped") && names.iter().any(|name| name == "Precmd")
    });

    // Every binding (Ctrl-P, ESC-i, ESC-p, ESC-w, vi keymaps, widget overrides) matches a shell
    // Warp never touched. The dump runs after at least one Warp precmd, which is where the
    // bindings used to be (re)installed.
    let warp_bindings = tmux.dump_bindings(shell, &warp, "warp");
    let plain_bindings = tmux.dump_bindings(shell, &plain, "plain");
    assert!(!plain_bindings.trim().is_empty(), "the dump ran");
    assert_eq!(
        warp_bindings, plain_bindings,
        "a marker-only pane's bindings differ from a plain {}'s",
        shell.name
    );
    tmux.wait_for_prompt(&warp, shell.prompt);

    // A plain client's Ctrl-P walks history.
    tmux.type_text(&warp, "echo recall-me");
    tmux.keys(&warp, &["Enter"]);
    tmux.wait_for("the echo", || {
        tmux.view(&warp)
            .lines()
            .any(|line| line.trim() == "recall-me")
    });
    tmux.wait_for_prompt(&warp, shell.prompt);
    tmux.keys(&warp, &["C-p"]);
    let recalled = format!("{}echo recall-me", shell.prompt);
    tmux.wait_for("Ctrl-P to recall the last command", || {
        tmux.last_line(&warp) == recalled
    });
    tmux.keys(&warp, &["C-c"]);
    tmux.wait_for_prompt(&warp, shell.prompt);

    // Warp's clear, then a command: the partly typed text is gone, in emacs and vi insert mode.
    let mut modes = vec![("emacs", None)];
    if let Some(vi_mode) = shell.vi_mode {
        modes.push(("vi", Some(vi_mode)));
    }
    for (mode, switch) in modes {
        if let Some(switch) = switch {
            tmux.type_text(&warp, switch);
            tmux.keys(&warp, &["Enter"]);
            tmux.wait_for_prompt(&warp, shell.prompt);
        }
        tmux.type_text(&warp, "partial-junk");
        tmux.wait_for("the partial text", || {
            tmux.last_line(&warp).ends_with("partial-junk")
        });
        tmux.hex(&warp, &TMUX_CLEAR_LINE_HEX);
        let marker = format!("cleared-{mode}");
        tmux.type_text(&warp, &format!("echo {marker}"));
        tmux.keys(&warp, &["Enter"]);
        tmux.wait_for(&format!("the {mode} command"), || {
            tmux.view(&warp).lines().any(|line| line.trim() == marker)
        });
        let view = tmux.view(&warp);
        assert!(
            !view.contains("partial-junkecho") && !view.contains("partial-junk echo"),
            "{mode}: the partial text was not cleared:\n{view}"
        );
        tmux.wait_for_prompt(&warp, shell.prompt);
    }
}

#[test]
fn live_marker_only_bootstrap_leaves_zsh_line_editor_alone() {
    marker_only_bootstrap_leaves_the_line_editor_alone(&ZSH);
}

#[test]
fn live_marker_only_bootstrap_leaves_bash_line_editor_alone() {
    marker_only_bootstrap_leaves_the_line_editor_alone(&BASH);
}

#[test]
fn live_marker_only_bootstrap_leaves_fish_line_editor_alone() {
    marker_only_bootstrap_leaves_the_line_editor_alone(&FISH);
}
