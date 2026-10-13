use super::*;
use crate::terminal::tmux::entry::{TmuxSocket, plan_entry};

fn session(name: &str, windows: u32, attached: u32) -> ListedSession {
    ListedSession {
        name: name.to_owned(),
        windows,
        attached_clients: attached,
    }
}

#[test]
fn the_list_command_asks_for_name_windows_and_attached_clients() {
    assert_eq!(
        list_sessions_command(None),
        "tmux ls -F '#{session_windows} #{session_attached} #{session_name}'"
    );
    assert_eq!(
        list_sessions_command(Some(&TmuxSocket::name("work"))),
        "tmux -L work ls -F '#{session_windows} #{session_attached} #{session_name}'"
    );
}

#[test]
fn list_output_parses_into_sessions_and_keeps_spaces_in_names() {
    let stdout = "3 1 work\n1 0 my session\n\n2 0 0\nnot a row\n";
    assert_eq!(
        parse_list_sessions(stdout),
        vec![
            session("work", 3, 1),
            session("my session", 1, 0),
            session("0", 2, 0)
        ]
    );
}

#[test]
fn a_missing_server_means_no_sessions_and_other_failures_are_errors() {
    assert_eq!(
        list_outcome(true, "1 0 work\n", ""),
        ListOutcome::Sessions(vec![session("work", 1, 0)])
    );
    assert_eq!(list_outcome(true, "", ""), ListOutcome::Sessions(vec![]));
    assert_eq!(
        list_outcome(false, "", "no server running on /tmp/tmux-501/default\n"),
        ListOutcome::Sessions(vec![])
    );
    assert_eq!(
        list_outcome(
            false,
            "",
            "error connecting to /tmp/tmux-501/x (No such file or directory)\n"
        ),
        ListOutcome::Sessions(vec![])
    );
    assert_eq!(
        list_outcome(false, "", "zsh: command not found: tmux\n"),
        ListOutcome::Failed("zsh: command not found: tmux".to_owned())
    );
    assert_eq!(
        list_outcome(false, "", ""),
        ListOutcome::Failed("tmux ls failed".to_owned())
    );
}

#[test]
fn labels_name_the_window_count_and_attached_state() {
    assert_eq!(
        session_label(&session("work", 3, 1)),
        "work — 3 windows, attached"
    );
    assert_eq!(session_label(&session("solo", 1, 0)), "solo — 1 window");
    assert_eq!(
        session_label(&session("bad\x1b[31mname", 2, 0)),
        "bad[31mname — 2 windows"
    );
}

#[test]
fn the_attach_menu_lists_sessions_then_offers_a_new_one() {
    let entries = attach_menu_entries(&ListOutcome::Sessions(vec![
        session("work", 3, 1),
        session("play", 1, 0),
    ]));
    assert_eq!(
        entries,
        vec![
            MenuEntry::action(
                "work — 3 windows, attached",
                TmuxMenuAction::Attach("work".into())
            ),
            MenuEntry::action("play — 1 window", TmuxMenuAction::Attach("play".into())),
            MenuEntry::Separator,
            MenuEntry::action(NEW_SESSION_LABEL, TmuxMenuAction::NewSession),
        ]
    );
}

#[test]
fn with_no_sessions_the_menu_says_so_and_offers_new() {
    assert_eq!(
        attach_menu_entries(&ListOutcome::Sessions(vec![])),
        vec![
            MenuEntry::note("No tmux sessions"),
            MenuEntry::Separator,
            MenuEntry::action(NEW_SESSION_LABEL, TmuxMenuAction::NewSession),
        ]
    );
    assert_eq!(
        attach_menu_entries(&ListOutcome::Failed("tmux: not found".into())),
        vec![
            MenuEntry::note("Could not list tmux sessions: tmux: not found"),
            MenuEntry::Separator,
            MenuEntry::action(NEW_SESSION_LABEL, TmuxMenuAction::NewSession),
        ]
    );
}

#[test]
fn chosen_entries_become_control_mode_commands_on_the_remembered_socket() {
    assert_eq!(
        command_for(&TmuxMenuAction::NewSession, None, true),
        Some("tmux -CC new".into())
    );
    assert_eq!(
        command_for(&TmuxMenuAction::Attach("my session".into()), None, true),
        Some("tmux -CC attach -t '=my session'".into())
    );
    assert_eq!(
        command_for(
            &TmuxMenuAction::Attach("work".into()),
            Some(&TmuxSocket::name("alt")),
            true
        ),
        Some("tmux -CC -L alt attach -t =work".into())
    );
    assert_eq!(command_for(&TmuxMenuAction::ListSessions, None, true), None);
}

#[test]
fn nested_panes_get_the_plain_command_so_it_runs_raw() {
    assert_eq!(
        command_for(&TmuxMenuAction::Attach("work".into()), None, false),
        Some("tmux attach -t =work".into())
    );
    assert_eq!(
        command_for(&TmuxMenuAction::NewSession, None, false),
        Some("tmux new".into())
    );
}

#[test]
fn menu_commands_are_not_rewritten_again() {
    let command = command_for(&TmuxMenuAction::Attach("work".into()), None, true).unwrap();
    assert!(matches!(
        plan_entry(&command, |_| None, false),
        crate::terminal::tmux::entry::Entry::ControlMode { .. }
    ));
}
