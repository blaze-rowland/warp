use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use command::blocking::Command;
use instant::Instant;

use warp_terminal::tmux::{CommandReply, PaneId};

use super::*;

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
        ..reply(&[])
    }
}

fn facts(command: &str, alternate_on: bool) -> PaneFacts {
    PaneFacts {
        pane_id: PaneId(3),
        alternate_on,
        session_id_option: None,
        shell_option: None,
        current_command: Some(command.to_owned()),
    }
}

/// A pane the launcher set up with `token`.
fn launcher_facts(token: &str) -> PaneFacts {
    PaneFacts {
        session_id_option: Some(derive_session_id(token, PaneId(3)).as_u64().to_string()),
        shell_option: Some("zsh".to_owned()),
        ..facts("zsh", false)
    }
}

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

// Plain-terminal mode.

#[test]
fn a_tmux_pane_is_a_plain_terminal_until_its_shell_is_integrated() {
    assert_eq!(pane_input_mode(true, false), PaneInputMode::PlainTerminal);
    assert_eq!(pane_input_mode(true, true), PaneInputMode::Warp);
}

#[test]
fn other_panes_keep_warps_input_editor() {
    assert_eq!(pane_input_mode(false, false), PaneInputMode::Warp);
    assert_eq!(pane_input_mode(false, true), PaneInputMode::Warp);
}

// Session ids.

#[test]
fn the_session_id_is_a_keyed_hash_of_the_token_and_pane() {
    let id = derive_session_id(TOKEN, PaneId(3));
    assert_eq!(id, derive_session_id(TOKEN, PaneId(3)));
    assert_ne!(id, derive_session_id(TOKEN, PaneId(4)));
    assert_ne!(id, derive_session_id("another-token", PaneId(3)));
    assert!(
        id.as_u64() < 1 << 60,
        "fits the launcher's shell arithmetic"
    );
}

/// The launcher derives the id in POSIX sh with whatever SHA-256 tool the host has; Warp must
/// derive the same number.
#[test]
fn the_launcher_shell_derives_the_same_session_id() {
    let script =
        format!("h=$(printf '%s:%s' {TOKEN} %3 | {SHA256_COMMAND} | cut -c1-15); echo $((0x$h))");
    let output = Command::new("/bin/sh").arg("-c").arg(&script).output();
    let Ok(output) = output else {
        eprintln!("skipping: no /bin/sh");
        return;
    };
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_eq!(
        printed,
        derive_session_id(TOKEN, PaneId(3)).as_u64().to_string()
    );
}

#[test]
fn only_the_derived_session_id_verifies() {
    assert_eq!(
        verified_session_id(&launcher_facts(TOKEN), TOKEN),
        Some(derive_session_id(TOKEN, PaneId(3)))
    );
    // Set up under an older token.
    assert_eq!(verified_session_id(&launcher_facts("old"), TOKEN), None);
    // A forged or missing value.
    let forged = PaneFacts {
        session_id_option: Some("12345".to_owned()),
        ..facts("zsh", false)
    };
    assert_eq!(verified_session_id(&forged, TOKEN), None);
    assert_eq!(verified_session_id(&facts("zsh", false), TOKEN), None);
}

#[test]
fn hook_tokens_are_random_hex() {
    let token = new_hook_token();
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(token, new_hook_token());
}

// Parsing what tmux says.

#[test]
fn pane_facts_parse_from_tmux_rows() {
    let rows = reply(&[
        "%1\t0\t42\tzsh\tzsh",
        "%2\t1\t\t\tvim",
        "%3\t0\t\t\t",
        "garbage",
    ]);
    assert_eq!(
        parse_pane_facts(&rows),
        vec![
            PaneFacts {
                pane_id: PaneId(1),
                alternate_on: false,
                session_id_option: Some("42".to_owned()),
                shell_option: Some("zsh".to_owned()),
                current_command: Some("zsh".to_owned()),
            },
            PaneFacts {
                pane_id: PaneId(2),
                alternate_on: true,
                session_id_option: None,
                shell_option: None,
                current_command: Some("vim".to_owned()),
            },
            PaneFacts {
                pane_id: PaneId(3),
                alternate_on: false,
                session_id_option: None,
                shell_option: None,
                current_command: None,
            },
        ]
    );
    assert!(parse_pane_facts(&error_reply()).is_empty());
}

#[test]
fn plain_clients_are_the_non_control_clients() {
    let clients = reply(&["1\tclient-1", "0\t/dev/ttys004", "0\t/dev/ttys\x1b007"]);
    assert_eq!(
        plain_clients(&clients),
        Some(vec!["/dev/ttys004".to_owned(), "/dev/ttys007".to_owned()])
    );
    assert_eq!(plain_clients(&reply(&["1\tclient-1"])), Some(vec![]));
    assert_eq!(plain_clients(&error_reply()), None);
}

#[test]
fn the_token_reply_is_trusted_only_when_well_formed() {
    assert_eq!(parse_token(&reply(&[TOKEN])), Some(TOKEN.to_owned()));
    assert_eq!(parse_token(&reply(&[""])), None);
    assert_eq!(parse_token(&reply(&["a b"])), None);
    assert_eq!(parse_token(&error_reply()), None);
}

// R8's action.

#[test]
fn warpify_needs_a_known_shell_at_its_prompt() {
    assert_eq!(
        warpify_availability(Some("zsh"), false),
        WarpifyAvailability::Available(ShellType::Zsh)
    );
    assert_eq!(
        warpify_availability(Some("-bash"), false),
        WarpifyAvailability::Available(ShellType::Bash)
    );
    let WarpifyAvailability::Unavailable(reason) = warpify_availability(Some("vim"), true) else {
        panic!("vim on the alternate screen is not Warpified");
    };
    assert!(reason.contains("vim"), "{reason}");
    let WarpifyAvailability::Unavailable(reason) = warpify_availability(Some("htop"), false) else {
        panic!("htop is not a shell");
    };
    assert!(reason.contains("htop"), "{reason}");
    assert!(matches!(
        warpify_availability(Some("zsh"), true),
        WarpifyAvailability::Unavailable(_)
    ));
    assert!(matches!(
        warpify_availability(Some("pwsh"), false),
        WarpifyAvailability::Unavailable(_)
    ));
}

#[test]
fn the_warpify_confirmation_names_attached_plain_clients() {
    let none = warpify_confirmation_text(Some(&[]));
    assert!(!none.contains("client"), "{none}");
    let one = warpify_confirmation_text(Some(&["/dev/ttys004".to_owned()]));
    assert!(one.contains("/dev/ttys004"), "{one}");
    let two = warpify_confirmation_text(Some(&["a".to_owned(), "b".to_owned()]));
    assert!(two.contains("a and b"), "{two}");
    let unknown = warpify_confirmation_text(None);
    assert!(unknown.contains("may see"), "{unknown}");
}

// Panes that existed at attach.

/// Covers F1 and F3: a pane the launcher set up while Warp was away is integrated from its next
/// prompt, with no Warpify step.
#[test]
fn a_launcher_pane_from_before_attach_is_reannounced() {
    assert_eq!(
        existing_pane_plan(&launcher_facts(TOKEN), Some(TOKEN), true),
        ExistingPanePlan::Reannounce(derive_session_id(TOKEN, PaneId(3)))
    );
}

/// Covers AE3: panes that existed at attach offer "Warpify this pane".
#[test]
fn an_existing_plain_shell_pane_offers_warpify() {
    assert_eq!(
        existing_pane_plan(&facts("zsh", false), Some(TOKEN), true),
        ExistingPanePlan::Warpify(WarpifyAvailability::Available(ShellType::Zsh))
    );
}

#[test]
fn an_existing_vim_pane_shows_warpify_disabled_with_a_reason() {
    let ExistingPanePlan::Warpify(WarpifyAvailability::Unavailable(reason)) =
        existing_pane_plan(&facts("vim", true), Some(TOKEN), true)
    else {
        panic!("vim gets a disabled action");
    };
    assert!(reason.contains("vim"), "{reason}");
}

#[test]
fn with_integration_off_existing_panes_stay_plain() {
    assert_eq!(
        existing_pane_plan(&launcher_facts(TOKEN), Some(TOKEN), false),
        ExistingPanePlan::Off
    );
    assert_eq!(
        existing_pane_plan(&facts("zsh", false), Some(TOKEN), false),
        ExistingPanePlan::Off
    );
}

// Panes that appear after attach.

/// Covers AE2: with a plain client attached and no plugin, a pane from that client's `prefix %`
/// gets R8's action and nothing typed.
#[test]
fn a_shared_session_never_gets_typed_into() {
    let clients = ["/dev/ttys004".to_owned()];
    assert_eq!(
        new_pane_plan(&facts("bash", false), Some(TOKEN), Some(&clients), true),
        NewPanePlan::Warpify(WarpifyAvailability::Available(ShellType::Bash))
    );
    // Unknown clients count as shared.
    assert_eq!(
        new_pane_plan(&facts("bash", false), Some(TOKEN), None, true),
        NewPanePlan::Warpify(WarpifyAvailability::Available(ShellType::Bash))
    );
}

/// With Warp as the only client, a pane created by a tmux binding gets the typed bootstrap.
#[test]
fn warp_alone_types_the_init_command_into_a_new_shell_pane() {
    assert_eq!(
        new_pane_plan(&facts("zsh", false), Some(TOKEN), Some(&[]), true),
        NewPanePlan::TypeInit(ShellType::Zsh)
    );
    // Before the token is known too: typing registers its own session id.
    assert_eq!(
        new_pane_plan(&facts("fish", false), None, Some(&[]), true),
        NewPanePlan::TypeInit(ShellType::Fish)
    );
}

/// A `split-window htop` pane is not injected.
#[test]
fn a_program_pane_is_not_injected() {
    assert!(matches!(
        new_pane_plan(&facts("htop", true), Some(TOKEN), Some(&[]), true),
        NewPanePlan::Warpify(WarpifyAvailability::Unavailable(_))
    ));
    assert!(matches!(
        new_pane_plan(&facts("zsh", true), Some(TOKEN), Some(&[]), true),
        NewPanePlan::Warpify(WarpifyAvailability::Unavailable(_))
    ));
}

#[test]
fn a_launcher_pane_is_left_to_the_launcher() {
    assert_eq!(
        new_pane_plan(&launcher_facts(TOKEN), Some(TOKEN), Some(&[]), true),
        NewPanePlan::Launcher
    );
}

/// When the launcher install fails, a Warp split starts the default shell (see
/// `workspace_sync_tests`), which carries no launcher options and so follows the typed path's
/// conditions.
#[test]
fn a_default_shell_split_follows_the_typed_path_conditions() {
    let split = facts("zsh", false);
    assert_eq!(
        new_pane_plan(&split, Some(TOKEN), Some(&[]), true),
        NewPanePlan::TypeInit(ShellType::Zsh)
    );
    assert!(matches!(
        new_pane_plan(&split, Some(TOKEN), Some(&["tty".to_owned()]), true),
        NewPanePlan::Warpify(_)
    ));
}

#[test]
fn with_integration_off_nothing_is_typed() {
    assert_eq!(
        new_pane_plan(&facts("zsh", false), Some(TOKEN), Some(&[]), false),
        NewPanePlan::Off
    );
}

// Install commands.

fn bundle() -> LauncherBundle {
    launcher_bundle(&crate::ASSETS)
}

/// The part of a `run-shell` command tmux hands to the shell.
fn run_shell_payload(command: &str) -> &str {
    command
        .strip_prefix("run-shell '")
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or_else(|| panic!("not a run-shell: {command}"))
}

#[test]
fn every_install_command_is_one_safe_tmux_line() {
    let bundle = bundle();
    let mut commands: Vec<String> = install_check_commands(&bundle).into();
    commands.extend(install_upload_commands(&bundle, "abc"));
    commands.push(ensure_token_command(TOKEN));
    commands.extend(reannounce_command(&[SessionId::from(7)]));
    for command in &commands {
        assert!(!command.contains('\n'), "one line: {command}");
        if command.starts_with("run-shell") {
            let payload = run_shell_payload(command);
            assert!(!payload.contains('\''), "no quote ends it early: {payload}");
            assert!(
                !payload.contains('#'),
                "no tmux format expansion: {payload}"
            );
            assert!(
                payload.ends_with(">/dev/null 2>&1 || true"),
                "output dropped, always succeeds: {payload}"
            );
        }
        assert!(
            command.len() < UPLOAD_CHUNK_CHARS + 512,
            "short: {}",
            command.len()
        );
    }
}

#[test]
fn the_bundle_holds_the_launcher_and_every_shells_bootstrap() {
    let bundle = bundle();
    let paths: Vec<&str> = bundle.files().iter().map(|file| file.path).collect();
    assert_eq!(
        paths,
        [
            LAUNCHER_FILE,
            "zsh/.zshenv",
            "init.zsh",
            "init.bash",
            "init.fish",
            "bootstrap.zsh",
            "bootstrap.bash",
            "bootstrap.fish",
        ]
    );
    assert_eq!(bundle.version().len(), 16);
    for file in bundle
        .files()
        .iter()
        .filter(|f| f.path.starts_with("bootstrap."))
    {
        let text = String::from_utf8_lossy(&file.contents);
        assert!(
            text.contains("WARP_TMUX_MARKER_ONLY"),
            "{} has marker-only mode",
            file.path
        );
    }
}

#[test]
fn the_launcher_path_must_be_absolute_and_plain() {
    let ok = "/home/me/.cache/warp/tmux/0123/warp-shell.sh";
    assert_eq!(parse_launcher_path(&reply(&[ok])), Some(ok.to_owned()));
    assert_eq!(parse_launcher_path(&reply(&[""])), None);
    assert_eq!(
        parse_launcher_path(&reply(&["relative/warp-shell.sh"])),
        None
    );
    assert_eq!(parse_launcher_path(&reply(&["/a'b/warp-shell.sh"])), None);
    assert_eq!(parse_launcher_path(&error_reply()), None);
}

#[test]
fn a_local_install_verifies_and_is_reused() {
    let home = tempfile::tempdir().unwrap();
    let bundle = bundle();
    let path = install_locally(&bundle, home.path()).unwrap();
    assert!(path.ends_with(LAUNCHER_FILE));
    assert!(path.starts_with(home.path().join(INSTALL_ROOT)));
    // A second install finds it in place.
    assert_eq!(install_locally(&bundle, home.path()).unwrap(), path);
    // A tampered install is replaced... unless it is in place, in which case it fails closed.
    std::fs::write(path.parent().unwrap().join("init.zsh"), b"echo tampered").unwrap();
    assert!(install_locally(&bundle, home.path()).is_err());
}

// Live tests against a real tmux on an isolated socket. Each starts its own server with
// `-f /dev/null` and a temporary $HOME, and kills it afterwards; the user's tmux is never touched.

struct LiveTmux {
    socket: String,
    home: tempfile::TempDir,
}

impl LiveTmux {
    /// `None` (the test is skipped) without tmux 3.2 or newer.
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
            socket: format!("warp-u9-{}-{name}", std::process::id()),
            home: tempfile::tempdir().ok()?,
        };
        std::fs::write(tmux.home.path().join(".zshrc"), "PS1='u9z> '\n").ok()?;
        std::fs::write(tmux.home.path().join(".bashrc"), "PS1='u9b> '\n").ok()?;
        std::fs::write(
            tmux.home.path().join(".bash_profile"),
            "[ -f ~/.bashrc ] && . ~/.bashrc\n",
        )
        .ok()?;
        std::fs::create_dir_all(tmux.home.path().join(".config/fish")).ok()?;
        std::fs::write(
            tmux.home.path().join(".config/fish/config.fish"),
            "function fish_prompt; echo -n 'u9f> '; end\nset -g fish_greeting\n",
        )
        .ok()?;
        // The first session keeps the server alive.
        tmux.run(&[
            "new-session",
            "-d",
            "-s",
            "s",
            "-x",
            "100",
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
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("tmux runs")
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.run(args).stdout).to_string()
    }

    fn reply(&self, args: &[&str]) -> CommandReply {
        let output = self.run(args);
        CommandReply {
            error: !output.status.success(),
            ..reply(
                &String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .collect::<Vec<_>>(),
            )
        }
    }

    /// Runs command lines as Warp sends them over the control channel. `source-file` parses
    /// them with the same command parser.
    fn run_lines(&self, lines: &[String]) {
        let file = self.home.path().join("commands.tmux");
        std::fs::write(&file, lines.join("\n")).unwrap();
        let output = self.run(&["source-file", file.to_str().unwrap()]);
        assert!(
            output.status.success(),
            "source-file failed ({:?}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Installs the launcher the way Warp does over the control channel. Returns its path.
    fn install(&self) -> Option<String> {
        let bundle = bundle();
        let [check, read] = install_check_commands(&bundle);
        self.run_lines(&[check]);
        if let Some(path) = parse_launcher_path(&self.reply(&split_args(&read))) {
            return Some(path);
        }
        let mut upload = install_upload_commands(&bundle, "test");
        let read = upload.pop().unwrap();
        self.run_lines(&upload);
        parse_launcher_path(&self.reply(&split_args(&read)))
    }

    fn set_token(&self) -> String {
        let token = new_hook_token();
        self.run_lines(&[ensure_token_command(&token)]);
        parse_token(&self.reply(&split_args(&read_token_query()))).expect("token set")
    }

    /// Starts `shell` in a new window through the launcher, with its raw output captured.
    /// Returns the pane and the capture file.
    fn launcher_pane(&self, shell: &str, launcher: &str) -> (PaneId, PathBuf) {
        self.run(&["set-option", "-g", "default-shell", shell]);
        let start = format!("sleep 0.5; exec {}", launcher_shell_command(launcher));
        let pane = self.stdout(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "s",
            &start,
        ]);
        let pane = PaneId::parse(pane.trim()).expect("new pane id");
        let raw = self.home.path().join(format!("raw-{}", pane.0));
        self.run(&[
            "pipe-pane",
            "-o",
            "-t",
            &pane.to_string(),
            &format!("cat >> '{}'", raw.display()),
        ]);
        (pane, raw)
    }

    /// What a plain client sees in `pane`: its text, without escape sequences.
    fn plain_view(&self, pane: PaneId) -> String {
        self.stdout(&["capture-pane", "-p", "-t", &pane.to_string()])
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
}

impl Drop for LiveTmux {
    fn drop(&mut self) {
        let socket = self.stdout(&["display-message", "-p", "#{socket_path}"]);
        let _ = self.command().arg("kill-server").output();
        let socket = socket.trim();
        if socket.contains("warp-u9-") {
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// Splits a single-line tmux command built here into arguments for the tmux CLI. The commands
/// split this way quote only single words.
fn split_args(line: &str) -> Vec<&str> {
    line.split(' ').map(|arg| arg.trim_matches('\'')).collect()
}

/// The JSON of each Warp hook in raw pane output.
fn hooks(raw: &[u8]) -> Vec<serde_json::Value> {
    let mut hooks = Vec::new();
    let mut rest = raw;
    while let Some(start) = find(rest, b"\x1bP$d") {
        let body = &rest[start + 4..];
        let Some(end) = find(body, b"\x1b\\") else {
            break;
        };
        if let Ok(bytes) = hex::decode(&body[..end])
            && let Ok(json) = serde_json::from_slice(&bytes)
        {
            hooks.push(json);
        }
        rest = &body[end..];
    }
    hooks
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn hook_names(hooks: &[serde_json::Value]) -> Vec<String> {
    hooks
        .iter()
        .filter_map(|hook| hook["hook"].as_str().map(str::to_owned))
        .collect()
}

/// Text Warp's integration must never put on a plain client's screen.
fn assert_no_debris(view: &str) {
    for debris in [
        "9278",
        "P$",
        "WARP",
        "warp_",
        "InitShell",
        "Bootstrapped",
        "eval",
        "EOM",
    ] {
        assert!(
            !view.contains(debris),
            "plain view shows {debris:?}:\n{view}"
        );
    }
}

#[test]
fn live_the_launcher_installs_over_the_control_channel() {
    let Some(tmux) = LiveTmux::start("install") else {
        return;
    };
    let path = tmux.install().expect("installed");
    let bundle = bundle();
    assert!(
        path.starts_with(&format!("{}/", tmux.home().display()))
            || path.contains(&bundle.install_dir()),
        "{path}"
    );
    assert!(Path::new(&path).exists());
    // The files on disk are the bundle's.
    let dir = Path::new(&path).parent().unwrap();
    for file in bundle.files() {
        assert_eq!(
            std::fs::read(dir.join(file.path)).unwrap(),
            file.contents,
            "{}",
            file.path
        );
    }
    // A second attach finds the install in place without uploading.
    tmux.run(&["set-option", "-gu", &bundle.launcher_option()]);
    let [check, read] = install_check_commands(&bundle);
    tmux.run_lines(&[check]);
    assert_eq!(
        parse_launcher_path(&tmux.reply(&split_args(&read))),
        Some(path)
    );
}

#[test]
fn live_an_unwritable_home_fails_the_install() {
    let Some(tmux) = LiveTmux::start("fail") else {
        return;
    };
    // `.cache` is a file, so nothing can be created under it.
    std::fs::write(tmux.home().join(".cache"), b"").unwrap();
    assert_eq!(tmux.install(), None);
}

/// A launcher pane: its shell is integrated without Warp writing a byte, it emits hooks with the
/// derived session id, and a plain client sees the user's prompt and nothing else.
fn launcher_pane_is_integrated_and_invisible(shell: &str, prompt: &str) {
    let name = Path::new(shell).file_name().unwrap().to_str().unwrap();
    let Some(tmux) = LiveTmux::start(name) else {
        return;
    };
    let token = tmux.set_token();
    let launcher = tmux.install().expect("installed");
    let (pane, raw) = tmux.launcher_pane(shell, &launcher);

    tmux.wait_for("the user's prompt", || {
        tmux.plain_view(pane).contains(prompt)
    });
    let view = tmux.plain_view(pane);
    assert_no_debris(&view);
    assert_eq!(view.trim(), prompt.trim(), "only the prompt:\n{view}");

    // The pane records its session id, and it verifies against the token.
    let rows = tmux.reply(&split_args(&pane_facts_query(pane)));
    let facts = parse_pane_facts(&rows);
    let facts = facts.first().expect("pane facts");
    let session_id = verified_session_id(facts, &token).expect("verified id");
    assert_eq!(facts.shell_option.as_deref(), Some(name));

    // The shell announced itself with that id: InitShell, then Bootstrapped, then prompts.
    let names_so_far = || hook_names(&self::hooks(&std::fs::read(&raw).unwrap_or_default()));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !names_so_far().contains(&"Bootstrapped".to_owned()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        names_so_far().contains(&"Bootstrapped".to_owned()),
        "hooks so far: {:?}; raw bytes: {}; view:\n{}",
        names_so_far(),
        std::fs::read(&raw).map(|raw| raw.len()).unwrap_or_default(),
        tmux.plain_view(pane)
    );
    tmux.wait_for("Bootstrapped", || {
        hook_names(&hooks(&std::fs::read(&raw).unwrap_or_default()))
            .contains(&"Bootstrapped".to_owned())
    });
    let hooks = hooks(&std::fs::read(&raw).unwrap());
    let names = hook_names(&hooks);
    assert_eq!(
        names.first().map(String::as_str),
        Some("InitShell"),
        "{names:?}"
    );
    assert!(names.contains(&"Precmd".to_owned()), "{names:?}");
    for hook in &hooks {
        assert_eq!(
            hook["value"]["session_id"].as_u64(),
            Some(session_id.as_u64()),
            "{hook}"
        );
    }

    // A command typed by a plain client runs normally and leaves no debris.
    tmux.run(&[
        "send-keys",
        "-t",
        &pane.to_string(),
        "echo hi-from-plain",
        "Enter",
    ]);
    tmux.wait_for("the echo", || {
        tmux.plain_view(pane).matches(prompt.trim()).count() >= 2
    });
    let view = tmux.plain_view(pane);
    assert!(view.contains("hi-from-plain"), "{view}");
    assert_no_debris(&view);

    // Warp attaching later asks the pane to announce itself again (F1): at its next prompt it
    // sends InitShell and Bootstrapped once more.
    let before = hook_names(&self::hooks(&std::fs::read(&raw).unwrap()))
        .iter()
        .filter(|name| *name == "Bootstrapped")
        .count();
    tmux.run_lines(&[reannounce_command(&[session_id]).unwrap()]);
    tmux.run(&["send-keys", "-t", &pane.to_string(), "true", "Enter"]);
    tmux.wait_for("the reannouncement", || {
        hook_names(&self::hooks(&std::fs::read(&raw).unwrap_or_default()))
            .iter()
            .filter(|name| *name == "Bootstrapped")
            .count()
            > before
    });
    assert_no_debris(&tmux.plain_view(pane));
}

#[test]
fn live_a_zsh_launcher_pane_is_integrated_and_invisible_to_plain_clients() {
    let Some(zsh) = which("zsh") else {
        eprintln!("skipping: no zsh");
        return;
    };
    launcher_pane_is_integrated_and_invisible(&zsh, "u9z>");
}

#[test]
fn live_a_bash_launcher_pane_is_integrated_and_invisible_to_plain_clients() {
    let Some(bash) = which("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    launcher_pane_is_integrated_and_invisible(&bash, "u9b>");
}

#[test]
fn live_a_fish_launcher_pane_is_integrated_and_invisible_to_plain_clients() {
    let Some(fish) = which("fish") else {
        eprintln!("skipping: no fish");
        return;
    };
    launcher_pane_is_integrated_and_invisible(&fish, "u9f>");
}

/// Covers AE2 live: a coworker attached from a plain terminal presses `prefix %`. Their screen
/// shows a normal shell, `list-clients` reports them, so Warp plans R8's action and types
/// nothing.
#[test]
fn live_a_plain_clients_split_gets_warpify_and_no_typing() {
    let Some(tmux) = LiveTmux::start("ae2") else {
        return;
    };
    let Some(bash) = which("bash") else {
        return;
    };
    let token = tmux.set_token();
    tmux.run(&["set-option", "-g", "default-shell", &bash]);
    // A plain client on a pseudo-terminal, as a coworker's terminal would be.
    let Some(mut plain) = attach_plain_client(&tmux) else {
        eprintln!("skipping: no `script` to give a plain client a terminal");
        return;
    };
    tmux.wait_for("the plain client", || {
        plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)))
            .is_some_and(|clients| !clients.is_empty())
    });
    // `prefix %` runs split-window from the plain client with tmux's default shell.
    let pane = tmux.stdout(&["split-window", "-h", "-P", "-F", "#{pane_id}", "-t", "s"]);
    let pane = PaneId::parse(pane.trim()).unwrap();
    tmux.wait_for("the prompt", || tmux.plain_view(pane).contains("u9b>"));

    let clients = plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)));
    let facts = parse_pane_facts(&tmux.reply(&split_args(&pane_facts_query(pane))));
    let plan = new_pane_plan(&facts[0], Some(&token), clients.as_deref(), true);
    assert_eq!(
        plan,
        NewPanePlan::Warpify(WarpifyAvailability::Available(ShellType::Bash))
    );
    let view = tmux.plain_view(pane);
    assert_eq!(
        view.trim(),
        "u9b>",
        "a normal shell, nothing injected:\n{view}"
    );
    let _ = plain.kill();
    let _ = plain.wait();
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

/// `tmux attach` on a pseudo-terminal from `script` (BSD or util-linux syntax).
fn attach_plain_client(tmux: &LiveTmux) -> Option<std::process::Child> {
    let attach = format!("tmux -L {} -f /dev/null attach -t s", tmux.socket);
    let mut command = Command::new("script");
    if cfg!(target_os = "macos") {
        command.args(["-q", "/dev/null", "/bin/sh", "-c", &attach]);
    } else {
        command.args(["-qfc", &attach, "/dev/null"]);
    }
    command
        .env("HOME", tmux.home())
        .env("TERM", "xterm-256color")
        .env_remove("TMUX")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}
