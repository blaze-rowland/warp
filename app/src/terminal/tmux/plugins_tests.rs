use std::path::Path;
use std::time::Duration;

use instant::Instant;
use warp_terminal::tmux::{CommandReply, PaneId};

use super::*;
use crate::terminal::shell::ShellType;
use crate::terminal::tmux::integration::tests::{
    LiveTmux, assert_no_debris, attach_plain_client, hook_names, hooks, reply, split_args, which,
};
use crate::terminal::tmux::integration::{
    ExistingPanePlan, LIST_CLIENTS_QUERY, NewPanePlan, PaneFacts, WarpifyAvailability,
    existing_pane_plan, launcher_bundle, new_pane_plan, pane_facts_query, parse_pane_facts,
    parse_token, plain_clients, read_token_query, reannounce_command, session_panes_query,
    verified_session_id,
};
use crate::terminal::tmux::popup::{path_with_wrapper, path_without_wrapper};

fn error_reply() -> CommandReply {
    CommandReply {
        error: true,
        ..reply(&[])
    }
}

// The plugin.

#[test]
fn the_install_line_loads_the_plugin_from_the_stable_path() {
    assert_eq!(
        plugin_install_line(),
        "run-shell '~/.cache/warp/tmux/current/warp.tmux 2>/dev/null || true'"
    );
    let text = plugin_install_text();
    assert!(text.contains(&plugin_install_line()), "{text}");
    assert!(text.contains("~/.tmux.conf"), "{text}");
    assert!(text.contains("SSH"), "covers remote hosts: {text}");
}

#[test]
fn the_plugin_ships_in_the_launcher_bundle() {
    let bundle = launcher_bundle(&crate::ASSETS);
    let plugin = bundle
        .files()
        .iter()
        .find(|file| file.path == PLUGIN_FILE)
        .expect("warp.tmux in the bundle");
    assert!(plugin.executable);
    let text = String::from_utf8_lossy(&plugin.contents);
    assert!(
        text.starts_with("#!/bin/sh"),
        "runnable by run-shell and TPM"
    );
    for option in [
        PLUGIN_OPTION,
        SAVED_DEFAULT_COMMAND_OPTION,
        "@warp-hook-token",
    ] {
        assert!(text.contains(option), "{option}");
    }
}

#[test]
fn the_plugin_option_is_read_from_its_reply() {
    assert!(parse_plugin(&reply(&["1"])));
    assert!(!parse_plugin(&reply(&[""])));
    assert!(!parse_plugin(&reply(&[])));
    assert!(!parse_plugin(&error_reply()));
    assert_eq!(plugin_query(), "show-options -gqv @warp-plugin");
}

/// With the plugin loaded, a new shell pane that did not integrate is never typed into, even
/// with Warp alone: tmux starts panes through the launcher itself.
#[test]
fn with_the_plugin_loaded_the_typed_path_is_skipped() {
    let pane = PaneFacts {
        pane_id: PaneId(3),
        alternate_on: false,
        session_id_option: None,
        shell_option: None,
        current_command: Some("zsh".to_owned()),
    };
    let token = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        new_pane_plan(&pane, Some(token), Some(&[]), true, true),
        NewPanePlan::Warpify(WarpifyAvailability::Available(ShellType::Zsh))
    );
    assert_eq!(
        new_pane_plan(&pane, Some(token), Some(&[]), false, true),
        NewPanePlan::TypeInit(ShellType::Zsh)
    );
}

#[test]
fn detaching_keeps_the_plugins_wrapper_dir_on_path() {
    let plugin = "/home/me/.cache/warp/tmux/current/bin";
    let warp = "/home/me/.cache/warp/tmux/0123456789abcdef/bin";
    assert_eq!(
        path_after_detach(&format!("{warp}:{plugin}:/usr/bin:/bin")),
        format!("{plugin}:/usr/bin:/bin")
    );
    assert_eq!(
        path_after_detach(&format!("{warp}:/usr/bin")),
        "/usr/bin".to_owned()
    );
}

// Continuum.

#[test]
fn without_continuum_no_autosave_runs() {
    assert_eq!(parse_continuum(&reply(&["\t\t00"])), None);
    assert_eq!(parse_continuum(&reply(&[])), None);
    assert_eq!(parse_continuum(&error_reply()), None);
}

#[test]
fn continuum_hooked_into_the_status_line_saves_every_15_minutes_by_default() {
    assert_eq!(
        parse_continuum(&reply(&["\t/p/tmux-resurrect/scripts/save.sh\t10"])),
        Some(ContinuumAutosave {
            interval: Duration::from_secs(15 * 60),
            save_script: Some("/p/tmux-resurrect/scripts/save.sh".to_owned()),
        })
    );
}

#[test]
fn continuums_interval_is_in_minutes_and_zero_is_off() {
    assert_eq!(
        parse_continuum(&reply(&["5\t\t00"])),
        Some(ContinuumAutosave {
            interval: Duration::from_secs(5 * 60),
            save_script: None,
        })
    );
    assert_eq!(parse_continuum(&reply(&["0\t/s.sh\t01"])), None);
    assert_eq!(parse_continuum(&reply(&["soon\t/s.sh\t01"])), None);
}

#[test]
fn an_unsafe_save_script_path_falls_back_to_the_default() {
    for path in ["relative/save.sh", "/a'b/save.sh", "/a$(x)/save.sh", "/a#b"] {
        let autosave = parse_continuum(&reply(&[&format!("5\t{path}\t00")])).unwrap();
        assert_eq!(autosave.save_script, None, "{path}");
    }
}

#[test]
fn the_save_command_is_one_safe_run_shell() {
    for save_script in [None, Some("/home/me/save.sh".to_owned())] {
        let command = ContinuumAutosave {
            interval: Duration::from_secs(300),
            save_script,
        }
        .save_command();
        let payload = command
            .strip_prefix("run-shell '")
            .and_then(|rest| rest.strip_suffix('\''))
            .unwrap_or_else(|| panic!("not a run-shell: {command}"));
        assert!(!command.contains('\n'));
        assert!(!payload.contains('\''), "{payload}");
        assert!(!payload.contains('#'), "{payload}");
        assert!(payload.contains("+ 300 ))"), "{payload}");
        assert!(
            payload.contains("quiet & }"),
            "the save runs in the background: {payload}"
        );
    }
}

#[test]
fn warp_saves_only_when_it_is_the_only_client() {
    assert!(autosave_due(Some(&[])));
    assert!(!autosave_due(Some(&["/dev/ttys004".to_owned()])));
    // An unreadable client list counts as a plain client being attached.
    assert!(!autosave_due(None));
}

// Live tests: isolated servers (`tmux -L warp-u10-<pid>-<name> -f /dev/null`, temporary $HOME).

fn start(name: &str) -> Option<LiveTmux> {
    LiveTmux::start_with_prefix("warp-u10", name)
}

fn option(tmux: &LiveTmux, name: &str) -> String {
    tmux.stdout(&["show-options", "-gqv", name])
        .trim_end_matches('\n')
        .to_owned()
}

fn global_path(tmux: &LiveTmux) -> String {
    tmux.stdout(&["show-environment", "-g", "PATH"])
        .trim()
        .trim_start_matches("PATH=")
        .to_owned()
}

fn current_dir(tmux: &LiveTmux) -> String {
    format!("{}/.cache/warp/tmux/current", tmux.home().display())
}

fn load_plugin(tmux: &LiveTmux) {
    tmux.run_lines(&[plugin_install_line()]);
}

fn unload_plugin(tmux: &LiveTmux) {
    tmux.run_lines(&[format!(
        "run-shell '~/.cache/warp/tmux/current/{PLUGIN_FILE} unload'"
    )]);
}

/// A new pane with no command (as `prefix %`, a script or resurrect makes one): tmux starts it
/// with `default-command`.
fn default_pane(tmux: &LiveTmux, args: &[&str]) -> PaneId {
    let mut command = args.to_vec();
    command.extend(["-P", "-F", "#{pane_id}"]);
    let pane = tmux.stdout(&command);
    PaneId::parse(pane.trim()).unwrap_or_else(|| panic!("pane id from {pane:?}"))
}

fn token(tmux: &LiveTmux) -> String {
    parse_token(&tmux.reply(&split_args(&read_token_query()))).expect("token")
}

#[test]
fn live_the_plugin_sets_default_command_and_unload_restores_it() {
    let Some(tmux) = start("load") else {
        return;
    };
    let launcher = tmux.install().expect("installed");
    // Each install points `current` at itself.
    let current = current_dir(&tmux);
    assert_eq!(
        std::fs::canonicalize(format!("{current}/warp-shell.sh")).unwrap(),
        std::fs::canonicalize(&launcher).unwrap()
    );

    let user_command = "exec /bin/sh -l";
    tmux.run(&["set-option", "-g", "default-command", user_command]);
    let stale = format!("{}/.cache/warp/tmux/old/bin", tmux.home().display());
    // The plugin's own `tmux` calls need tmux on the server's PATH, as on any real server.
    let tmux_dir = which("tmux")
        .and_then(|tmux| Some(Path::new(&tmux).parent()?.to_string_lossy().into_owned()))
        .unwrap();
    let path = format!("{stale}:{tmux_dir}:/usr/bin:/bin");
    tmux.run(&["set-environment", "-g", "PATH", &path]);

    load_plugin(&tmux);
    assert_eq!(option(&tmux, PLUGIN_OPTION), "1");
    assert_eq!(option(&tmux, SAVED_DEFAULT_COMMAND_OPTION), user_command);
    let default_command = option(&tmux, "default-command");
    assert!(
        default_command.contains(&format!("{current}/warp-shell.sh")),
        "the stable path: {default_command}"
    );
    assert!(!option(&tmux, "@warp-hook-token").is_empty());
    // The wrapper goes first on PATH, as Warp's own helper would put it.
    assert_eq!(
        global_path(&tmux),
        path_with_wrapper(&path, &format!("{current}/bin"))
    );

    // Loading again (`tmux source ~/.tmux.conf`) keeps the user's command for unload.
    load_plugin(&tmux);
    assert_eq!(option(&tmux, SAVED_DEFAULT_COMMAND_OPTION), user_command);

    unload_plugin(&tmux);
    assert_eq!(option(&tmux, "default-command"), user_command);
    assert_eq!(option(&tmux, PLUGIN_OPTION), "");
    assert_eq!(option(&tmux, SAVED_DEFAULT_COMMAND_OPTION), "");
    assert_eq!(global_path(&tmux), path_without_wrapper(&path));

    // Without a default-command of the user's own, unload leaves tmux's default.
    tmux.run(&["set-option", "-gu", "default-command"]);
    load_plugin(&tmux);
    assert!(option(&tmux, "default-command").contains("warp-shell.sh"));
    unload_plugin(&tmux);
    assert_eq!(option(&tmux, "default-command"), "");
}

#[test]
fn live_without_an_install_the_plugin_does_nothing() {
    let Some(tmux) = start("absent") else {
        return;
    };
    tmux.run(&["set-option", "-g", "default-command", "exec /bin/sh"]);
    load_plugin(&tmux);
    assert_eq!(option(&tmux, PLUGIN_OPTION), "");
    assert_eq!(option(&tmux, "default-command"), "exec /bin/sh");
}

/// F3 with the plugin: the server dies (a reboot, `tmux kill-server`), a new server loads the
/// plugin from the config, and the panes resurrect restores start through the launcher. When
/// Warp attaches, each verifies and is reannounced, so it shows blocks from its next prompt.
#[test]
fn live_restored_panes_carry_verified_integration() {
    let Some(tmux) = start("restore") else {
        return;
    };
    let Some(bash) = which("bash") else {
        return;
    };
    tmux.install().expect("installed");
    tmux.run(&["kill-server"]);

    // The new server, as its config runs: default shell, then the plugin.
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
    tmux.run(&["set-option", "-g", "default-shell", &bash]);
    load_plugin(&tmux);
    // resurrect restores windows and panes with `new-window` / `split-window` and no command
    // unless the pane ran a program it restores.
    let first = default_pane(&tmux, &["new-window", "-d", "-t", "s"]);
    let second = default_pane(&tmux, &["split-window", "-d", "-t", &first.to_string()]);

    let token = token(&tmux);
    for pane in [first, second] {
        tmux.wait_for("the restored shell's prompt", || {
            tmux.plain_view(pane).contains("u9b>")
        });
        // tmux shows `default-command` as the pane's start command: the launcher's path, never
        // its contents.
        let start_command = tmux.stdout(&[
            "display-message",
            "-p",
            "-t",
            &pane.to_string(),
            "#{pane_start_command}",
        ]);
        assert!(
            !start_command.contains("WARP_SESSION_ID"),
            "{start_command}"
        );
    }
    let panes = parse_pane_facts(&tmux.reply(&split_args(&session_panes_query())));
    for pane in [first, second] {
        let facts = panes.iter().find(|facts| facts.pane_id == pane).unwrap();
        assert!(
            matches!(
                existing_pane_plan(facts, Some(&token), true),
                ExistingPanePlan::Reannounce(_)
            ),
            "{facts:?}"
        );
        assert_eq!(facts.shell_option.as_deref(), Some("bash"));
    }

    // If Warp's files go away, panes still get a shell.
    std::fs::remove_file(current_dir(&tmux)).unwrap();
    let pane = default_pane(&tmux, &["new-window", "-d", "-t", "s"]);
    tmux.wait_for("the fallback shell's prompt", || {
        tmux.plain_view(pane).contains("u9b>")
    });
}

/// Covers AE2 with the plugin: a coworker attached from a plain terminal presses `prefix %`. The
/// split starts through the launcher, so Warp shows blocks (its session id verifies and Warp
/// leaves it to the launcher), and the coworker sees their normal prompt and nothing else.
#[test]
fn live_a_plain_clients_split_gets_integration_with_the_plugin() {
    let Some(tmux) = start("ae2") else {
        return;
    };
    let Some(bash) = which("bash") else {
        return;
    };
    tmux.install().expect("installed");
    tmux.run(&["set-option", "-g", "default-shell", &bash]);
    load_plugin(&tmux);
    let Some(mut plain) = attach_plain_client(&tmux) else {
        eprintln!("skipping: no `script` to give a plain client a terminal");
        return;
    };
    tmux.wait_for("the plain client", || {
        plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)))
            .is_some_and(|clients| !clients.is_empty())
    });

    let pane = default_pane(&tmux, &["split-window", "-h", "-t", "s"]);
    tmux.wait_for("the prompt", || tmux.plain_view(pane).contains("u9b>"));
    let view = tmux.plain_view(pane);
    assert_eq!(view.trim(), "u9b>", "a normal prompt:\n{view}");
    assert_no_debris(&view);

    let token = token(&tmux);
    let clients = plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)));
    let facts = parse_pane_facts(&tmux.reply(&split_args(&pane_facts_query(pane))));
    let plugin = parse_plugin(&tmux.reply(&split_args(&plugin_query())));
    assert!(plugin);
    assert_eq!(
        new_pane_plan(&facts[0], Some(&token), clients.as_deref(), plugin, true),
        NewPanePlan::Launcher
    );

    // Warp attaching asks the pane to announce itself; at its next prompt it sends Warp's hooks
    // with the verified session id, and the plain client still sees only the shell.
    let session_id = verified_session_id(&facts[0], &token).expect("verified");
    let raw = tmux.home().join("raw-ae2");
    tmux.run(&[
        "pipe-pane",
        "-o",
        "-t",
        &pane.to_string(),
        &format!("cat >> '{}'", raw.display()),
    ]);
    tmux.run_lines(&[reannounce_command(&[session_id]).unwrap()]);
    tmux.run(&["send-keys", "-t", &pane.to_string(), "true", "Enter"]);
    tmux.wait_for("Bootstrapped", || {
        hook_names(&hooks(&std::fs::read(&raw).unwrap_or_default()))
            .contains(&"Bootstrapped".to_owned())
    });
    for hook in hooks(&std::fs::read(&raw).unwrap()) {
        assert_eq!(
            hook["value"]["session_id"].as_u64(),
            Some(session_id.as_u64()),
            "{hook}"
        );
    }
    assert_no_debris(&tmux.plain_view(pane));
    let _ = plain.kill();
    let _ = plain.wait();
}

/// A fake resurrect save script that counts its runs and records its argument.
fn fake_save_script(tmux: &LiveTmux) -> (String, std::path::PathBuf) {
    let log = tmux.home().join("saves.log");
    let script = tmux.home().join("save.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$1\" >> '{}'\n", log.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (script.to_string_lossy().into_owned(), log)
}

fn saves(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn continuum(tmux: &LiveTmux) -> Option<ContinuumAutosave> {
    // `display-message` needs a session to expand in; the CLI picks the server's only one.
    let output = tmux.run(&[
        "display-message",
        "-p",
        CONTINUUM_QUERY
            .strip_prefix("display-message -p '")
            .and_then(|rest| rest.strip_suffix('\''))
            .unwrap(),
    ]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    parse_continuum(&reply(&text.lines().collect::<Vec<_>>()))
}

/// The query reads continuum's settings from a real server: no continuum, then continuum hooked
/// into the status line at its default interval, then an explicit interval.
#[test]
fn live_the_continuum_query_finds_continuum() {
    let Some(tmux) = start("cq") else {
        return;
    };
    assert_eq!(continuum(&tmux), None);
    tmux.run(&[
        "set-option",
        "-g",
        "status-right",
        "#(/p/tmux-continuum/scripts/continuum_save.sh)%H:%M",
    ]);
    tmux.run(&[
        "set-option",
        "-g",
        "@resurrect-save-script-path",
        "/p/save.sh",
    ]);
    assert_eq!(
        continuum(&tmux),
        Some(ContinuumAutosave {
            interval: Duration::from_secs(15 * 60),
            save_script: Some("/p/save.sh".to_owned()),
        })
    );
    tmux.run(&["set-option", "-g", "@continuum-save-interval", "1"]);
    assert_eq!(continuum(&tmux).unwrap().interval, Duration::from_secs(60));
    tmux.run(&["set-option", "-g", "@continuum-save-interval", "0"]);
    assert_eq!(continuum(&tmux), None);
}

/// With continuum configured and only Warp attached, a save runs once per interval, `quiet` as
/// continuum runs it, and it records continuum's last-save time. With a plain client attached
/// Warp runs none.
#[test]
fn live_the_autosave_runs_once_per_interval() {
    let Some(tmux) = start("save") else {
        return;
    };
    let (script, log) = fake_save_script(&tmux);
    let autosave = ContinuumAutosave {
        interval: Duration::from_secs(2),
        save_script: Some(script),
    };
    // Three ticks in quick succession: one save.
    for _ in 0..3 {
        tmux.run_lines(&[autosave.save_command()]);
    }
    tmux.wait_for("the save", || !saves(&log).is_empty());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(saves(&log), ["quiet"]);
    assert!(
        option(&tmux, "@continuum-save-last-timestamp")
            .parse::<u64>()
            .is_ok()
    );
    // A tick after the interval saves again.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    tmux.run_lines(&[autosave.save_command()]);
    tmux.wait_for("the second save", || saves(&log).len() == 2);

    // A plain client's status line drives continuum: Warp does not save.
    let Some(mut plain) = attach_plain_client(&tmux) else {
        return;
    };
    tmux.wait_for("the plain client", || {
        plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)))
            .is_some_and(|clients| !clients.is_empty())
    });
    let clients = plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)));
    assert!(!autosave_due(clients.as_deref()));
    let _ = plain.kill();
    let _ = plain.wait();
    tmux.wait_for("the plain client to leave", || {
        plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY)))
            .is_some_and(|clients| clients.is_empty())
    });
    assert!(autosave_due(
        plain_clients(&tmux.reply(&split_args(LIST_CLIENTS_QUERY))).as_deref()
    ));
}

/// A tick never runs a missing save script (continuum configured, resurrect not installed).
#[test]
fn live_no_save_runs_without_resurrect() {
    let Some(tmux) = start("nores") else {
        return;
    };
    let autosave = ContinuumAutosave {
        interval: Duration::from_secs(1),
        save_script: None,
    };
    tmux.run_lines(&[autosave.save_command()]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(option(&tmux, "@continuum-save-last-timestamp"), "");
}
