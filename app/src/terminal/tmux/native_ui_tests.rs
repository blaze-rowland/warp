use std::time::Duration;

use warp_terminal::tmux::{CommandReply, PaneId};
use warpui::keymap::Keystroke;

use super::*;
use crate::terminal::tmux::key_tables::TmuxKey;
use crate::terminal::tmux::popup::tests::LiveTmux;

fn reply(lines: &[&str]) -> CommandReply {
    CommandReply {
        time: 0,
        number: 1,
        flags: 1,
        error: false,
        lines: lines.iter().map(|line| line.as_bytes().to_vec()).collect(),
    }
}

fn key(name: &str) -> TmuxKey {
    TmuxKey::parse(name).unwrap_or_else(|| panic!("key {name}"))
}

fn stroke(source: &str) -> Keystroke {
    Keystroke::parse(source).unwrap()
}

/// tmux 3.7's default `prefix <` menu, as `list-keys` prints it.
const WINDOW_MENU: &str = r##"display-menu -T "#[align=centre]#{window_index}:#{window_name}" -x W -y W "#{?#{>:#{session_windows},1},,-}Swap Left" l { swap-window -t :-1 } "#{?#{>:#{session_windows},1},,-}Swap Right" r { swap-window -t :+1 } "#{?pane_marked_set,,-}Swap Marked" s { swap-window } '' Kill X { kill-window } Respawn R { respawn-window -k } "#{?pane_marked,Unmark,Mark}" m { select-pane -m } Rename n { command-prompt -F -I "#W" { rename-window -t "#{window_id}" "%%" } } '' "New After" w { new-window -a } "New At End" W { new-window }"##;

#[test]
fn bound_commands_split_as_tmux_prints_them() {
    let commands = split_commands(
        r##"select-pane -t = \; if-shell -F "#{pane_in_mode}" { send-keys -M } { copy-mode -M }"##,
    );
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0][0].text, "select-pane");
    let if_shell: Vec<&str> = commands[1].iter().map(|t| t.text.as_str()).collect();
    assert_eq!(
        if_shell,
        [
            "if-shell",
            "-F",
            "#{pane_in_mode}",
            "send-keys -M",
            "copy-mode -M"
        ]
    );
    assert!(commands[1][3].block);
    assert_eq!(commands[1][3].raw, "{ send-keys -M }");
    // Quotes and escapes.
    let tokens = &split_commands(r#"a "b \"c\" \$d" 'e "f"' g\ h"#)[0];
    let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, ["a", "b \"c\" $d", "e \"f\"", "g h"]);
    // Nested blocks and braces inside quotes.
    let tokens = &split_commands(r#"x { a "}" { b } } y"#)[0];
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[1].text, r#"a "}" { b }"#);
    assert_eq!(tokens[2].text, "y");
}

#[test]
fn interactive_commands_are_classified() {
    assert_eq!(classify("split-window -h"), Classified::Plain);
    // `display-message` reaches tmux and comes back as `%message`, which becomes a toast.
    assert_eq!(classify("display-message"), Classified::Plain);
    assert_eq!(classify("display-message hello"), Classified::Plain);
    // Inside a sequence Warp cannot draw it in place; the watchdog covers it.
    assert_eq!(
        classify(r#"select-pane -t 0 \; display-menu -T x a a "kill-pane""#),
        Classified::Unsupported("display-menu".to_owned())
    );
    assert!(matches!(
        classify("choose-tree -Zw"),
        Classified::Native(Interactive::Choose(ChooseSpec {
            windows_collapsed: true,
            sessions_collapsed: false
        }))
    ));
    assert!(matches!(
        classify("choose-tree -Zs"),
        Classified::Native(Interactive::Choose(ChooseSpec {
            sessions_collapsed: true,
            ..
        }))
    ));
    assert!(matches!(
        classify("display-panes"),
        Classified::Native(Interactive::Panes(PanesSpec { template: None }))
    ));
    assert!(matches!(
        classify("popup -E fzf"),
        Classified::Native(Interactive::Popup(_))
    ));
}

/// Covers AE9: `prefix x` is `confirm-before kill-pane`. `n` sends nothing; `y` runs it.
#[test]
fn confirm_before_runs_its_command_only_on_y() {
    let Classified::Native(Interactive::Confirm(spec)) =
        classify(r#"confirm-before -p "kill-pane #P? (y/n)" kill-pane"#)
    else {
        panic!("confirm-before is drawn natively");
    };
    assert_eq!(spec.prompt.as_deref(), Some("kill-pane #P? (y/n)"));
    assert_eq!(spec.command, "kill-pane");
    let state = ConfirmState::new(&spec, "kill-pane 0? (y/n)".to_owned());
    assert_eq!(state.key(&key("n")), SurfaceOutcome::Cancel);
    assert_eq!(state.key(&key("Escape")), SurfaceOutcome::Cancel);
    assert_eq!(state.key(&key("Enter")), SurfaceOutcome::Cancel);
    assert_eq!(
        state.key(&key("y")),
        SurfaceOutcome::Run(vec!["kill-pane".to_owned()])
    );
    // `-y` makes Enter confirm; `-c` changes the confirm key; a block is its inner command.
    let Classified::Native(Interactive::Confirm(spec)) =
        classify("confirm-before -y -c Y { kill-window ; new-window }")
    else {
        panic!();
    };
    let state = ConfirmState::new(&spec, String::new());
    assert_eq!(
        state.key(&key("Enter")),
        SurfaceOutcome::Run(vec!["kill-window ; new-window".to_owned()])
    );
    assert_eq!(state.key(&key("y")), SurfaceOutcome::Cancel);
    assert!(matches!(state.key(&key("Y")), SurfaceOutcome::Run(_)));
    assert_eq!(
        default_confirm_prompt("kill-session -t x"),
        "Confirm 'kill-session'? (y/n)"
    );
}

/// `prefix ,` shows a prompt seeded with the window name; tmux substitutes the template.
#[test]
fn command_prompt_is_seeded_and_submitted_through_a_background_prompt() {
    let Classified::Native(Interactive::Prompt(spec)) =
        classify(r##"command-prompt -I "#W" { rename-window "%%" }"##)
    else {
        panic!("command-prompt is drawn natively");
    };
    assert_eq!(spec.inputs.as_deref(), Some("#W"));
    assert_eq!(
        spec.template_raw.as_deref(),
        Some(r#"{ rename-window "%%" }"#)
    );
    let labels = prompt_labels(&spec);
    assert_eq!(labels, ["(rename-window)"]);
    assert_eq!(prompt_inputs(&spec, 1), ["#W"]);
    let mut state = PromptState::new(&spec, labels, vec!["old".to_owned()]);
    for name in ["BSpace", "BSpace", "BSpace", "n", "e", "w", "Space", "1"] {
        assert_eq!(state.key(&key(name)), SurfaceOutcome::Continue);
    }
    assert_eq!(
        state.key(&key("Enter")),
        SurfaceOutcome::Submit(vec!["new 1".to_owned()])
    );
    assert_eq!(
        prompt_submit_commands(&spec, &["new 1".to_owned()], Some("client-9")),
        [
            r#"command-prompt -b -p "warp" { rename-window "%%" }"#,
            r#"send-keys -K -c "client-9" -l "new 1""#,
            r#"send-keys -K -c "client-9" Enter"#,
        ]
    );
}

#[test]
fn a_prompt_has_one_field_per_p_entry() {
    let Classified::Native(Interactive::Prompt(spec)) =
        classify(r#"command-prompt -p "name,index" -I ",3" "new-window -n '%1' -t %2""#)
    else {
        panic!();
    };
    let labels = prompt_labels(&spec);
    assert_eq!(labels, ["name", "index"]);
    assert_eq!(prompt_inputs(&spec, 2), ["", "3"]);
    let mut state = PromptState::new(&spec, labels, vec![String::new(), "3".to_owned()]);
    state.key(&key("a"));
    assert_eq!(state.key(&key("Enter")), SurfaceOutcome::Continue);
    assert_eq!(state.active, 1);
    state.key(&key("4"));
    assert_eq!(
        state.key(&key("Enter")),
        SurfaceOutcome::Submit(vec!["a".to_owned(), "34".to_owned()])
    );
    let commands = prompt_submit_commands(&spec, &["a".to_owned(), String::new()], None);
    assert_eq!(
        commands,
        [
            r#"command-prompt -b -p "warp,warp" "new-window -n '%1' -t %2""#,
            r#"send-keys -K -l "a""#,
            "send-keys -K Enter",
            "send-keys -K Enter",
        ]
    );
    // Escape cancels without sending anything.
    assert_eq!(state.key(&key("Escape")), SurfaceOutcome::Cancel);
    // `prefix :` has no template: tmux runs the text as a command.
    let Classified::Native(Interactive::Prompt(spec)) = classify("command-prompt") else {
        panic!();
    };
    assert_eq!(prompt_labels(&spec), [":"]);
    assert_eq!(
        prompt_submit_commands(&spec, &["kill-pane".to_owned()], Some("c"))[0],
        r#"command-prompt -b -p "warp""#
    );
}

#[test]
fn single_key_prompts_take_the_next_key() {
    let Classified::Native(Interactive::Prompt(spec)) =
        classify(r#"command-prompt -k -p key { list-keys -1N "%%" }"#)
    else {
        panic!();
    };
    let mut state = PromptState::new(&spec, vec!["key".to_owned()], vec![String::new()]);
    assert_eq!(
        state.key(&key("C-x")),
        SurfaceOutcome::Submit(vec!["C-x".to_owned()])
    );
    assert_eq!(
        prompt_submit_commands(&spec, &["C-x".to_owned()], Some("c")),
        [
            r#"command-prompt -b -k -p "warp" { list-keys -1N "%%" }"#,
            r#"send-keys -K -c "c" "C-x""#,
        ]
    );
}

/// A `display-menu` with three items shows three entries, and picking one runs its command.
#[test]
fn a_menu_shows_its_items_and_runs_the_chosen_one() {
    let Classified::Native(Interactive::Menu(spec)) = classify(
        r#"display-menu -T "Pick" "One" 1 "rename-window one" "Two" 2 { rename-window two } "Three" 3 "rename-window three""#,
    ) else {
        panic!("display-menu is drawn natively");
    };
    assert_eq!(spec.items.len(), 3);
    let names = ["One".to_owned(), "Two".to_owned(), "Three".to_owned()];
    let mut menu = MenuState::new(&spec, "Pick".to_owned(), &names);
    assert_eq!(menu.rows.len(), 3);
    assert_eq!(menu.selected, Some(0));
    // The item's own key.
    assert_eq!(
        menu.clone().key(&key("2")),
        SurfaceOutcome::Run(vec!["rename-window two".to_owned()])
    );
    // Arrows and Enter.
    assert_eq!(menu.key(&key("Down")), SurfaceOutcome::Continue);
    assert_eq!(menu.key(&key("Down")), SurfaceOutcome::Continue);
    assert_eq!(
        menu.key(&key("Enter")),
        SurfaceOutcome::Run(vec!["rename-window three".to_owned()])
    );
    assert_eq!(menu.key(&key("Down")), SurfaceOutcome::Continue);
    assert_eq!(menu.selected, Some(0), "wraps around");
    assert_eq!(menu.key(&key("Escape")), SurfaceOutcome::Cancel);
    assert_eq!(
        menu.activate(1),
        SurfaceOutcome::Run(vec!["rename-window two".to_owned()])
    );
}

#[test]
fn the_default_window_menu_hides_disables_and_separates_like_tmux() {
    let Classified::Native(Interactive::Menu(spec)) = classify(WINDOW_MENU) else {
        panic!("display-menu is drawn natively");
    };
    assert_eq!(
        spec.title.as_deref(),
        Some("#[align=centre]#{window_index}:#{window_name}")
    );
    // Expanded as tmux would with one window: the swaps are disabled.
    let names: Vec<String> = [
        "-Swap Left",
        "-Swap Right",
        "-Swap Marked",
        "Kill",
        "Respawn",
        "Mark",
        "Rename",
        "New After",
        "New At End",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let mut menu = MenuState::new(&spec, strip_style("#[align=centre]0:zsh"), &names);
    assert_eq!(menu.title, "0:zsh");
    assert_eq!(menu.rows.len(), 11, "{:?}", menu.rows);
    assert!(
        matches!(&menu.rows[0], MenuRow::Item { enabled: false, label, .. } if label == "Swap Left")
    );
    assert_eq!(menu.rows[3], MenuRow::Separator);
    // The first enabled item is highlighted; disabled items' keys do nothing.
    assert!(
        matches!(&menu.rows[menu.selected.unwrap()], MenuRow::Item { label, .. } if label == "Kill")
    );
    assert_eq!(menu.key(&key("l")), SurfaceOutcome::Continue);
    // Rename opens a prompt: its command is itself interactive and is drawn natively in turn.
    let SurfaceOutcome::Run(commands) = menu.key(&key("n")) else {
        panic!("rename runs");
    };
    assert!(matches!(
        classify(&commands[0]),
        Classified::Native(Interactive::Prompt(_))
    ));
    // Items whose name expands empty are hidden, and so are separators left dangling.
    let names = vec![String::new(); 9];
    assert!(MenuState::new(&spec, String::new(), &names).rows.is_empty());
}

fn tree() -> Vec<TreeRow> {
    parse_tree_rows(&reply(&[
        "$0\twork\t@0\t0\tzsh\t%0\t0\tzsh\t1\t1",
        "$0\twork\t@1\t1\tlogs\t%1\t0\ttail\t1\t0",
        "$0\twork\t@1\t1\tlogs\t%2\t1\tvim\t0\t0",
        "$0\twork\t@2\t2\twarp-popup\t%5\t0\tfzf\t1\t0",
        "$1\tother\t@3\t0\tsh\t%3\t0\tsh\t1\t1",
    ]))
}

/// `prefix w` shows sessions with their windows and panes, and choosing a pane in another
/// window focuses it.
#[test]
fn the_picker_lists_sessions_windows_and_panes() {
    let rows = tree();
    assert_eq!(rows.len(), 4, "popup windows are left out");
    let Classified::Native(Interactive::Choose(spec)) = classify("choose-tree -Zw") else {
        panic!();
    };
    let mut picker = PickerState::new(spec, rows.clone(), Some("$0"));
    let labels: Vec<String> = picker.rows().iter().map(|row| row.label.clone()).collect();
    assert_eq!(
        labels,
        [
            "work (2 windows)",
            "0: zsh",
            "1: logs",
            "other (1 window)",
            "0: sh"
        ]
    );
    // The current window is highlighted.
    assert_eq!(picker.rows()[picker.selected].label, "0: zsh");
    // Expand window 1 and choose its second pane.
    picker.key(&key("Down"));
    assert_eq!(picker.key(&key("Right")), SurfaceOutcome::Continue);
    let labels: Vec<String> = picker.rows().iter().map(|row| row.label.clone()).collect();
    assert_eq!(&labels[3..5], ["0: tail", "1: vim"]);
    picker.key(&key("Down"));
    picker.key(&key("Down"));
    let SurfaceOutcome::Select(target) = picker.key(&key("Enter")) else {
        panic!("Enter chooses");
    };
    assert_eq!(
        target,
        TreeTarget::Pane {
            session_id: "$0".into(),
            window_id: "@1".into(),
            pane_id: "%2".into()
        }
    );
    assert_eq!(
        select_target_commands(&target, Some("$0"), Some("c")),
        [r#"select-window -t "@1""#, r#"select-pane -t "%2""#]
    );
    // Another session also switches the client.
    let target = TreeTarget::Session {
        session_id: "$1".into(),
    };
    assert_eq!(
        select_target_commands(&target, Some("$0"), Some("c")),
        [r#"switch-client -c "c" -t "\$1""#]
    );
    assert_eq!(picker.key(&key("Escape")), SurfaceOutcome::Cancel);
    // Without -w every window shows its panes; with -s sessions start collapsed.
    let full = PickerState::new(
        ChooseSpec {
            sessions_collapsed: false,
            windows_collapsed: false,
        },
        rows.clone(),
        Some("$0"),
    );
    assert_eq!(full.rows().len(), 7);
    let sessions = PickerState::new(
        ChooseSpec {
            sessions_collapsed: true,
            windows_collapsed: false,
        },
        rows,
        Some("$0"),
    );
    assert_eq!(sessions.rows().len(), 2);
}

#[test]
fn display_panes_chooses_by_number() {
    let state = PanesState {
        panes: vec![PaneId(4), PaneId(7)],
        template: None,
    };
    assert_eq!(
        state.key(&key("1")),
        SurfaceOutcome::Run(vec!["select-pane -t %7".to_owned()])
    );
    assert_eq!(state.key(&key("5")), SurfaceOutcome::Cancel);
    assert_eq!(state.key(&key("Escape")), SurfaceOutcome::Cancel);
    let swap = PanesState {
        panes: vec![PaneId(4)],
        template: Some("swap-pane -t %%".to_owned()),
    };
    assert_eq!(
        swap.key(&key("0")),
        SurfaceOutcome::Run(vec!["swap-pane -t %4".to_owned()])
    );
}

#[test]
fn formats_go_to_tmux_quoted_and_come_back_unstyled() {
    assert_eq!(
        expand_query("kill-pane #P? \"$x\"", PaneId(3)),
        r#"display-message -p -t %3 "kill-pane #P? \"\$x\"""#
    );
    assert_eq!(
        parse_expansion(&reply(&["#[align=centre]0:zsh#[default]"])),
        Some("0:zsh".to_owned())
    );
    assert_eq!(parse_expansion(&reply(&[])), Some(String::new()));
    assert_eq!(strip_style("#[fg=red]a#[bold]b"), "ab");
}

/// `display-message hello` comes back as `%message`; the toast lasts `display-time`.
#[test]
fn toasts_last_display_time() {
    assert_eq!(
        parse_display_time(&reply(&["4000"])),
        Duration::from_secs(4)
    );
    assert_eq!(parse_display_time(&reply(&["0"])), MIN_TOAST_TIME);
    assert_eq!(parse_display_time(&reply(&["junk"])), DEFAULT_DISPLAY_TIME);
}

#[test]
fn the_mode_strip_says_how_to_leave() {
    assert_eq!(
        mode_strip_text("copy-mode"),
        "tmux copy-mode: press q to exit"
    );
    assert_eq!(
        mode_strip_text("clock-mode"),
        "tmux clock-mode: press q or Escape to exit"
    );
}

#[test]
fn the_watchdog_closes_tmux_ui_from_a_second_client() {
    assert_eq!(
        watchdog_cancel_script("/tmp/tmux-501/default", "client-9"),
        "tmux -S /tmp/tmux-501/default send-keys -K -c client-9 Escape 2>/dev/null; \
         tmux -S /tmp/tmux-501/default display-popup -C -c client-9 2>/dev/null; true"
    );
    assert!(watchdog_toast_text(Some("display-menu")).contains("display-menu"));
    assert_eq!(
        reset_key_table_command(Some("client-9")),
        r#"switch-client -c "client-9" -T root"#
    );
}

#[test]
fn surfaces_take_warp_keystrokes() {
    assert_eq!(key_of(&stroke("y")), Some(key("y")));
    assert_eq!(key_of(&stroke("enter")), Some(key("Enter")));
    assert_eq!(key_of(&stroke("shift-A")), Some(key("A")));
}

// Live tests against a real tmux with Warp's control client attached; see `popup::tests`.

fn binding(tmux: &mut LiveTmux, table: &str, key: &str) -> String {
    let reply = tmux
        .control(
            &format!("list-keys -T {table} -F '#{{key_string}}\t#{{key_command}}'"),
            Duration::from_secs(5),
        )
        .expect("list-keys");
    reply
        .lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .find_map(|line| {
            let (bound, command) = line.split_once('\t')?;
            (bound == key).then(|| command.to_owned())
        })
        .unwrap_or_else(|| panic!("{table} {key} is bound"))
}

fn expand(tmux: &mut LiveTmux, format: &str, pane: PaneId) -> String {
    let reply = tmux
        .control(&expand_query(format, pane), Duration::from_secs(5))
        .expect("display-message");
    parse_expansion(&reply).expect("expands")
}

fn send(tmux: &mut LiveTmux, line: &str) {
    let reply = tmux
        .control(line, Duration::from_secs(5))
        .unwrap_or_else(|| panic!("{line} answers"));
    assert!(!reply.error, "{line}: {:?}", reply.lines);
}

fn active_pane(tmux: &LiveTmux) -> PaneId {
    PaneId::parse(&tmux.stdout(&["display-message", "-p", "#{pane_id}"])).unwrap()
}

/// Covers AE9 live: the default `prefix x` confirm. `n` sends nothing and the pane stays; `y`
/// runs the inner command from Warp's client.
#[test]
fn live_confirm_before_kills_only_on_y() {
    let Some(mut tmux) = LiveTmux::start("confirm") else {
        return;
    };
    tmux.run(&["split-window", "-d", "-t", "s:0"]);
    let pane = active_pane(&tmux);
    let Classified::Native(Interactive::Confirm(spec)) =
        classify(&binding(&mut tmux, "prefix", "x"))
    else {
        panic!("prefix x is a confirm");
    };
    let prompt = expand(&mut tmux, spec.prompt.as_deref().unwrap(), pane);
    assert_eq!(prompt, format!("kill-pane {}? (y/n)", 0));
    let state = ConfirmState::new(&spec, prompt);
    assert_eq!(state.key(&key("n")), SurfaceOutcome::Cancel);
    assert_eq!(
        tmux.stdout(&["display-message", "-p", "#{window_panes}"]),
        "2"
    );
    let SurfaceOutcome::Run(commands) = state.key(&key("y")) else {
        panic!("y confirms");
    };
    for command in commands {
        send(&mut tmux, &command);
    }
    assert_eq!(
        tmux.stdout(&["display-message", "-p", "#{window_panes}"]),
        "1"
    );
}

/// `prefix ,` shows a rename prompt seeded with the current name, and submitting renames the
/// window: tmux substitutes the template in a background prompt fed with `send-keys -K`.
#[test]
fn live_the_rename_prompt_renames_the_window() {
    let Some(mut tmux) = LiveTmux::start("rename") else {
        return;
    };
    tmux.run(&["rename-window", "-t", "s:0", "before"]);
    let pane = active_pane(&tmux);
    let Classified::Native(Interactive::Prompt(spec)) =
        classify(&binding(&mut tmux, "prefix", ","))
    else {
        panic!("prefix , is a prompt");
    };
    let labels = prompt_labels(&spec);
    let inputs: Vec<String> = prompt_inputs(&spec, labels.len())
        .iter()
        .map(|input| expand(&mut tmux, input, pane))
        .collect();
    assert_eq!(inputs, ["before"]);
    let mut state = PromptState::new(&spec, labels, inputs);
    for _ in 0.."before".len() {
        state.key(&key("BSpace"));
    }
    for c in ["a", "f", "t", "e", "r", "Space", "\""] {
        state.key(&key(c));
    }
    let SurfaceOutcome::Submit(values) = state.key(&key("Enter")) else {
        panic!("Enter submits");
    };
    let client = tmux.client_name.clone();
    for line in prompt_submit_commands(&spec, &values, Some(&client)) {
        send(&mut tmux, &line);
    }
    tmux.wait_for("the rename", || {
        tmux.stdout(&["display-message", "-p", "-t", "s:0", "#{window_name}"]) == "after \""
    });
    // The control client is still answering.
    send(&mut tmux, "display-message -p alive");
}

/// A `display-menu` with three items shows three entries, and picking one runs its command.
#[test]
fn live_a_three_item_menu_runs_the_picked_item() {
    let Some(mut tmux) = LiveTmux::start("menu") else {
        return;
    };
    tmux.run(&[
        "bind-key",
        "-n",
        "F7",
        "display-menu",
        "-T",
        "#{session_name}",
        "One",
        "1",
        "rename-window one",
        "Two",
        "2",
        "rename-window two",
        "#{?#{==:1,1},Three,}",
        "3",
        "rename-window three",
    ]);
    let pane = active_pane(&tmux);
    let Classified::Native(Interactive::Menu(spec)) = classify(&binding(&mut tmux, "root", "F7"))
    else {
        panic!("F7 is a menu");
    };
    let title = expand(&mut tmux, spec.title.as_deref().unwrap(), pane);
    let names: Vec<String> = spec
        .items
        .iter()
        .filter_map(|item| match item {
            MenuSpecItem::Item { name, .. } => Some(name.clone()),
            MenuSpecItem::Separator => None,
        })
        .map(|name| expand(&mut tmux, &name, pane))
        .collect();
    let mut menu = MenuState::new(&spec, title, &names);
    assert_eq!(menu.title, "s");
    let labels: Vec<String> = menu
        .rows
        .iter()
        .filter_map(|row| match row {
            MenuRow::Item { label, .. } => Some(label.clone()),
            MenuRow::Separator => None,
        })
        .collect();
    assert_eq!(labels, ["One", "Two", "Three"]);
    let SurfaceOutcome::Run(commands) = menu.key(&key("3")) else {
        panic!("3 picks Three");
    };
    for command in commands {
        let command = expand(&mut tmux, &command, pane);
        send(&mut tmux, &command);
    }
    assert_eq!(
        tmux.stdout(&["display-message", "-p", "-t", "s:0", "#{window_name}"]),
        "three"
    );
}

/// `prefix w` lists sessions, windows and panes; choosing a pane in another window focuses it.
#[test]
fn live_the_picker_focuses_a_pane_in_another_window() {
    let Some(mut tmux) = LiveTmux::start("tree") else {
        return;
    };
    let shell = tmux.shell_command();
    tmux.run(&["new-window", "-d", "-t", "s:1", "-n", "logs", &shell]);
    let target_pane = tmux.stdout(&[
        "split-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        "s:1",
        &shell,
    ]);
    let Classified::Native(Interactive::Choose(spec)) =
        classify(&binding(&mut tmux, "prefix", "w"))
    else {
        panic!("prefix w is choose-tree");
    };
    let reply = tmux
        .control(CHOOSE_TREE_QUERY, Duration::from_secs(5))
        .unwrap();
    let session = tmux.stdout(&["display-message", "-p", "#{session_id}"]);
    let mut picker = PickerState::new(spec, parse_tree_rows(&reply), Some(&session));
    // Find the window row for `logs`, expand it, and choose its second pane.
    let index = picker
        .rows()
        .iter()
        .position(|row| row.label.ends_with("logs"))
        .unwrap();
    while picker.selected != index {
        picker.key(&key("Down"));
    }
    picker.key(&key("Right"));
    picker.key(&key("Down"));
    picker.key(&key("Down"));
    let SurfaceOutcome::Select(target) = picker.key(&key("Enter")) else {
        panic!("Enter chooses");
    };
    let client = tmux.client_name.clone();
    for line in select_target_commands(&target, Some(&session), Some(&client)) {
        send(&mut tmux, &line);
    }
    assert_eq!(
        tmux.stdout(&[
            "display-message",
            "-p",
            "-t",
            "s:",
            "#{window_name} #{pane_id}"
        ]),
        format!("logs {target_pane}")
    );
}

/// A prompt that reaches tmux unintercepted stalls Warp's client; the watchdog's second client
/// cancels it, and later keys and commands still work. `display-message` comes back as
/// `%message`, and copy mode as `%pane-mode-changed`.
#[test]
fn live_the_watchdog_unblocks_a_stalled_client() {
    let Some(mut tmux) = LiveTmux::start("watchdog") else {
        return;
    };
    tmux.run(&[
        "bind-key",
        "-n",
        "F5",
        "confirm-before",
        "-p",
        "really?",
        "rename-window confirmed",
    ]);
    tmux.run(&["bind-key", "-n", "F6", "display-message", "hello there"]);
    let client = tmux.client_name.clone();
    send(&mut tmux, &format!("send-keys -K -c {client} F5"));
    assert!(
        tmux.control("display-message -p fence", Duration::from_millis(1500))
            .is_none(),
        "the confirm stalls the client's queue"
    );
    let fence = tmux.last_index();
    let status = command::blocking::Command::new("/bin/sh")
        .arg("-c")
        .arg(watchdog_cancel_script(&tmux.socket_path, &client))
        .status()
        .unwrap();
    assert!(status.success());
    let reply = tmux
        .reply(fence, Duration::from_secs(5))
        .expect("fence answers");
    assert_eq!(String::from_utf8_lossy(&reply.lines[0]), "fence");
    assert_ne!(
        tmux.stdout(&["display-message", "-p", "-t", "s:0", "#{window_name}"]),
        "confirmed",
        "cancelled, not confirmed"
    );
    // Later keys work: F6 shows a message, which Warp turns into a toast.
    send(&mut tmux, &format!("send-keys -K -c {client} F6"));
    tmux.wait_for("%message", || {
        tmux.notifications()
            .iter()
            .any(|line| line == "%message hello there")
    });
    // Copy mode reports a mode change; `q` through tmux leaves it.
    send(&mut tmux, "copy-mode");
    tmux.wait_for("copy mode", || {
        tmux.stdout(&["display-message", "-p", "#{pane_mode}"]) == "copy-mode"
    });
    assert!(
        tmux.notifications()
            .iter()
            .any(|line| line.starts_with("%pane-mode-changed"))
    );
    send(&mut tmux, &format!("send-keys -K -c {client} q"));
    tmux.wait_for("copy mode to end", || {
        tmux.stdout(&["display-message", "-p", "#{pane_mode}"])
            .is_empty()
    });
}
