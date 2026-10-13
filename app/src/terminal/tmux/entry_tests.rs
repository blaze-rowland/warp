use std::collections::HashMap;

use super::*;

fn no_aliases(_: &str) -> Option<String> {
    None
}

fn plan(command: &str) -> Entry {
    plan_entry(command, no_aliases, false)
}

fn converted(command: &str) -> String {
    match plan(command) {
        Entry::Convert { command, .. } => command,
        other => panic!("{command:?} was not converted: {other:?}"),
    }
}

fn assert_unchanged(command: &str) {
    let entry = plan(command);
    assert!(
        matches!(entry, Entry::Unchanged | Entry::ControlMode { .. }),
        "{command:?} should run unchanged, got {entry:?}"
    );
}

#[test]
fn session_opening_commands_gain_control_mode_after_tmux() {
    assert_eq!(converted("tmux"), "tmux -CC");
    assert_eq!(converted("tmux a"), "tmux -CC a");
    assert_eq!(converted("tmux at -t work"), "tmux -CC at -t work");
    assert_eq!(converted("tmux attach"), "tmux -CC attach");
    assert_eq!(
        converted("tmux attach-session -t x"),
        "tmux -CC attach-session -t x"
    );
    assert_eq!(converted("tmux new -s x"), "tmux -CC new -s x");
    assert_eq!(
        converted("tmux new-session -A -s x"),
        "tmux -CC new-session -A -s x"
    );
    assert_eq!(converted("  tmux a  "), "  tmux -CC a  ");
    assert_eq!(
        converted("/opt/homebrew/bin/tmux a"),
        "/opt/homebrew/bin/tmux -CC a"
    );
}

#[test]
fn global_flags_stay_in_place_after_control_mode() {
    assert_eq!(converted("tmux -L s attach"), "tmux -CC -L s attach");
    assert_eq!(
        converted("tmux -S /tmp/sock new"),
        "tmux -CC -S /tmp/sock new"
    );
    assert_eq!(
        converted("tmux -f ~/.tmux.alt.conf -u a"),
        "tmux -CC -f ~/.tmux.alt.conf -u a"
    );
    assert_eq!(converted("tmux -2u new -s x"), "tmux -CC -2u new -s x");
    assert_eq!(converted("tmux -Lwork a"), "tmux -CC -Lwork a");
}

#[test]
fn quoting_is_preserved() {
    assert_eq!(
        converted("tmux new -s 'my session'"),
        "tmux -CC new -s 'my session'"
    );
    assert_eq!(
        converted(r#"tmux a -t "it's \"here\"""#),
        r#"tmux -CC a -t "it's \"here\"""#
    );
    assert_eq!(
        converted(r"tmux new -s my\ session"),
        r"tmux -CC new -s my\ session"
    );
}

#[test]
fn other_subcommands_and_detached_sessions_run_unchanged() {
    for command in [
        "tmux ls",
        "tmux list-sessions",
        "tmux new -d -s bg",
        "tmux new -ds bg",
        "tmux new-session -s bg -d",
        "tmux kill-server",
        "tmux kill-session -t x",
        "tmux source ~/.tmux.conf",
        "tmux send-keys -t x ls Enter",
        "tmux -V",
        "tmux -c 'echo hi'",
        "tmux -Q a",
        "tmuxinator start x",
        "echo tmux a",
        "FOO=1 tmux a",
        "tmux -L",
        "",
        "   ",
    ] {
        assert_unchanged(command);
    }
}

#[test]
fn compound_commands_run_unchanged() {
    for command in [
        "tmux a | cat",
        "tmux a > out.txt",
        "tmux a < /dev/null",
        "tmux a 2>&1",
        "tmux a && echo done",
        "tmux a || echo failed",
        "tmux a; echo done",
        "tmux a &",
        "(tmux a)",
        "{ tmux a; }",
        "echo $(tmux ls)",
        "tmux a -t $(cat name)",
        "tmux a -t `cat name`",
        "tmux a -t \"$(cat name)\"",
        "tmux new -s x\necho hi",
        "tmux a -t 'unterminated",
        "tmux a; and echo done",
    ] {
        assert_unchanged(command);
    }
}

#[test]
fn a_second_tmux_command_after_an_escaped_semicolon_is_still_one_command() {
    assert_eq!(
        converted(r"tmux new -s x \; split-window"),
        r"tmux -CC new -s x \; split-window"
    );
}

#[test]
fn variables_and_comments_are_allowed() {
    assert_eq!(converted("tmux a -t $NAME"), "tmux -CC a -t $NAME");
    assert_eq!(converted("tmux a # work"), "tmux -CC a # work");
}

#[test]
fn control_mode_commands_are_not_rewritten_but_remember_their_socket() {
    assert_eq!(plan("tmux -CC a"), Entry::ControlMode { socket: None });
    assert_eq!(plan("tmux -C a"), Entry::ControlMode { socket: None });
    assert_eq!(
        plan("tmux -L work -CC new"),
        Entry::ControlMode {
            socket: Some(TmuxSocket::name("work"))
        }
    );
}

#[test]
fn converted_commands_report_their_socket() {
    match plan("tmux -L work a") {
        Entry::Convert { socket, .. } => assert_eq!(socket, Some(TmuxSocket::name("work"))),
        other => panic!("not converted: {other:?}"),
    }
    match plan("tmux -S ~/tmux.sock new") {
        Entry::Convert { socket, .. } => {
            let socket = socket.expect("socket");
            assert_eq!(socket.global_flags(), "-S ~/tmux.sock");
        }
        other => panic!("not converted: {other:?}"),
    }
    match plan("tmux -L'my sock' a") {
        Entry::Convert { socket, .. } => {
            assert_eq!(socket.expect("socket").global_flags(), "-L 'my sock'");
        }
        other => panic!("not converted: {other:?}"),
    }
    match plan("tmux a") {
        Entry::Convert { socket, .. } => assert_eq!(socket, None),
        other => panic!("not converted: {other:?}"),
    }
}

#[test]
fn aliases_expand_before_matching() {
    let aliases: HashMap<&str, &str> = [
        ("t", "tmux"),
        ("tw", "tmux -L work"),
        ("tl", "tmux ls"),
        ("tp", "tmux a | cat"),
    ]
    .into_iter()
    .collect();
    let lookup = |name: &str| aliases.get(name).map(|value| (*value).to_owned());

    match plan_entry("t a", lookup, false) {
        Entry::Convert { command, .. } => assert_eq!(command, "tmux -CC a"),
        other => panic!("not converted: {other:?}"),
    }
    match plan_entry("tw new -s 'x y'", lookup, false) {
        Entry::Convert { command, socket } => {
            assert_eq!(command, "tmux -CC -L work new -s 'x y'");
            assert_eq!(socket, Some(TmuxSocket::name("work")));
        }
        other => panic!("not converted: {other:?}"),
    }
    assert_eq!(plan_entry("tl", lookup, false), Entry::Unchanged);
    assert_eq!(plan_entry("tp", lookup, false), Entry::Unchanged);
    // A quoted word is not an alias.
    assert_eq!(plan_entry("'t' a", lookup, false), Entry::Unchanged);
}

#[test]
fn nested_origins_run_raw_with_a_note() {
    assert_eq!(plan_entry("tmux a", no_aliases, true), Entry::Nested);
    assert_eq!(plan_entry("tmux", no_aliases, true), Entry::Nested);
    // Commands that would not convert anyway are not flagged.
    assert_eq!(plan_entry("tmux ls", no_aliases, true), Entry::Unchanged);
    assert!(NESTED_NOTE.contains("Nested tmux is not supported natively"));
}

#[test]
fn the_rewritten_command_is_only_used_when_conversion_applies() {
    assert_eq!(
        command_to_run("tmux a", no_aliases, false, true),
        "tmux -CC a"
    );
    // Auto-convert off: unchanged, so tmux shows its raw screen.
    assert_eq!(command_to_run("tmux a", no_aliases, false, false), "tmux a");
    assert_eq!(command_to_run("tmux a", no_aliases, true, true), "tmux a");
    assert_eq!(
        command_to_run("tmux ls", no_aliases, false, true),
        "tmux ls"
    );
}

#[test]
fn sockets_render_as_global_flags_for_other_commands() {
    assert_eq!(TmuxSocket::name("work").global_flags(), "-L work");
    assert_eq!(TmuxSocket::name("my sock").global_flags(), "-L 'my sock'");
    assert_eq!(TmuxSocket::name("it's").global_flags(), r"-L 'it'\''s'");
}
