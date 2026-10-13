use warp_terminal::tmux::{CommandReply, KeyBinding, KeyClass, KeyConfig, PaneId};
use warpui::keymap::Keystroke;

use super::*;

fn stroke(source: &str) -> Keystroke {
    Keystroke::parse(source).unwrap()
}

fn binding(table: &str, repeat: bool, key: &str, command: &str) -> KeyBinding {
    KeyBinding {
        table: table.into(),
        repeat,
        key: key.into(),
        command: command.into(),
    }
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

const NAVIGATOR_C_H: &str = "if-shell -F \"#{@is_vim}\" { send-keys C-h } { select-pane -L }";

fn navigator_tables() -> KeyTables {
    KeyTables::from_parts(
        Some("C-b"),
        None,
        vec![
            binding("root", false, "C-h", NAVIGATOR_C_H),
            binding("root", false, "C-k", "select-pane -U"),
            binding("root", false, "M-Left", "previous-window"),
            binding("prefix", true, "Up", "select-pane -U"),
            binding("prefix", false, "2", "select-window -t :=2"),
            binding("prefix", false, "%", "split-window -h"),
            binding("root", false, "MouseDown1Pane", "select-pane -t ="),
        ],
    )
}

#[test]
fn warp_keystrokes_encode_to_tmux_key_names() {
    let cases = [
        ("ctrl-h", Some("C-h")),
        ("ctrl-b", Some("C-b")),
        ("a", Some("a")),
        ("shift-A", Some("A")),
        ("2", Some("2")),
        ("%", Some("%")),
        ("up", Some("Up")),
        ("alt-left", Some("M-Left")),
        ("shift-up", Some("S-Up")),
        ("ctrl-alt-shift-up", Some("C-M-S-Up")),
        ("escape", Some("Escape")),
        ("enter", Some("Enter")),
        ("tab", Some("Tab")),
        ("shift-tab", Some("BTab")),
        ("backspace", Some("BSpace")),
        ("delete", Some("DC")),
        ("pageup", Some("PPage")),
        ("pagedown", Some("NPage")),
        ("f5", Some("F5")),
        ("ctrl-f12", Some("C-F12")),
        ("f13", None),
        ("space", Some("Space")),
        ("ctrl-space", Some("C-Space")),
        ("meta-x", Some("M-x")),
        ("cmd-k", None),
        ("ctrl-shift-A", Some("C-S-a")),
    ];
    for (warp, tmux) in cases {
        assert_eq!(
            TmuxKey::from_keystroke(&stroke(warp))
                .map(|key| key.name())
                .as_deref(),
            tmux,
            "{warp}"
        );
    }
}

#[test]
fn tmux_spellings_of_one_key_compare_equal() {
    let pairs = [
        ("C-h", "C-h"),
        ("C-H", "C-S-h"),
        ("^B", "C-b"),
        ("PageUp", "PPage"),
        ("PgDn", "NPage"),
        ("S-Tab", "BTab"),
        ("\\;", ";"),
        ("M-S-Up", "M-S-Up"),
        ("space", "Space"),
    ];
    for (a, b) in pairs {
        assert_eq!(TmuxKey::parse(a), TmuxKey::parse(b), "{a} vs {b}");
        assert!(TmuxKey::parse(a).is_some(), "{a}");
    }
    assert_eq!(TmuxKey::parse("MouseDown1Pane"), None);
    assert_eq!(TmuxKey::parse("F13"), None);
}

#[test]
fn tmux_keys_map_back_to_the_warp_keystroke() {
    for warp in [
        "ctrl-k",
        "ctrl-h",
        "alt-left",
        "shift-up",
        "f5",
        "shift-tab",
        "escape",
        "shift-A",
        "%",
    ] {
        let key = TmuxKey::from_keystroke(&stroke(warp)).unwrap();
        let back = key.to_keystroke().unwrap();
        assert_eq!(TmuxKey::from_keystroke(&back), Some(key), "{warp}");
    }
    assert_eq!(
        TmuxKey::parse("C-k").unwrap().to_keystroke(),
        Some(stroke("ctrl-k"))
    );
    assert_eq!(TmuxKey::parse("C-k").unwrap().display(), "ctrl+k");
}

#[test]
fn the_mirror_knows_prefixes_root_bindings_and_navigation() {
    let tables = navigator_tables();
    let key = |name| TmuxKey::parse(name).unwrap();
    assert!(tables.is_prefix(&key("C-b")));
    assert!(!tables.is_prefix(&key("C-a")));
    assert!(tables.root_binding(&key("C-h")).is_some());
    assert!(tables.root_binding(&key("Up")).is_none());
    assert!(tables.binding("prefix", &key("Up")).unwrap().repeat);
    assert_eq!(tables.root_key_class(&key("C-h")), KeyClass::Navigation);
    assert_eq!(tables.root_key_class(&key("C-k")), KeyClass::Navigation);
    assert_eq!(tables.root_key_class(&key("M-Left")), KeyClass::Navigation);
    assert_eq!(tables.root_key_class(&key("C-b")), KeyClass::Other);
}

#[test]
fn navigation_commands_include_if_shell_wrapping_select_pane() {
    assert!(is_navigation_command("select-pane -L"));
    assert!(is_navigation_command("selectw -t 2"));
    assert!(is_navigation_command(NAVIGATOR_C_H));
    assert!(is_navigation_command(
        "if-shell \"$is_vim\" \"send-keys C-j\" \"select-pane -D\""
    ));
    assert!(!is_navigation_command("if-shell true \"send-keys x\""));
    assert!(!is_navigation_command("split-window -h"));
    assert!(!is_navigation_command("send-keys C-h"));
}

#[test]
fn a_mirror_needs_tmux_3_4_bindings() {
    let below_3_4 = KeyConfig {
        prefix: Some("C-b".into()),
        prefix2: None,
        bindings: None,
    };
    assert_eq!(KeyTables::from_config(&below_3_4), None);
    let config = KeyConfig {
        prefix: Some("C-a".into()),
        prefix2: Some("C-b".into()),
        bindings: Some(vec![]),
    };
    let tables = KeyTables::from_config(&config).unwrap();
    assert!(tables.is_prefix(&TmuxKey::parse("C-a").unwrap()));
    assert!(tables.is_prefix(&TmuxKey::parse("C-b").unwrap()));
}

#[test]
fn the_config_hash_follows_bindings_and_prefix_but_not_order() {
    let a = navigator_tables();
    let mut rows = vec![
        binding("prefix", false, "%", "split-window -h"),
        binding("root", false, "MouseDown1Pane", "select-pane -t ="),
        binding("prefix", false, "2", "select-window -t :=2"),
        binding("prefix", true, "Up", "select-pane -U"),
        binding("root", false, "M-Left", "previous-window"),
        binding("root", false, "C-k", "select-pane -U"),
        binding("root", false, "C-h", NAVIGATOR_C_H),
    ];
    let b = KeyTables::from_parts(Some("C-b"), None, rows.clone());
    assert_eq!(a.config_hash(), b.config_hash());
    let c = KeyTables::from_parts(Some("C-a"), None, rows.clone());
    assert_ne!(a.config_hash(), c.config_hash());
    rows.pop();
    let d = KeyTables::from_parts(Some("C-b"), None, rows);
    assert_ne!(a.config_hash(), d.config_hash());
}

#[test]
fn shadowed_keys_are_root_keys_and_prefixes_warp_also_uses() {
    let tables = navigator_tables();
    let warp = [stroke("ctrl-k"), stroke("ctrl-b"), stroke("cmd-p")];
    let shadowed: Vec<String> = tables
        .shadowed_keys(|stroke| warp.contains(stroke))
        .iter()
        .map(TmuxKey::display)
        .collect();
    assert_eq!(shadowed, vec!["ctrl+b", "ctrl+k"]);
}

#[test]
fn refresh_replies_parse() {
    assert_eq!(
        parse_prefix_reply(&reply(&["C-a\tNone"])),
        (Some("C-a".into()), None)
    );
    assert_eq!(
        parse_list_keys_reply(&reply(&["root\t0\tC-h\tselect-pane -L", "junk"])),
        vec![binding("root", false, "C-h", "select-pane -L")]
    );
    assert!(list_keys_command().starts_with("list-keys -F '"));
    assert!(is_source_file_command("source-file ~/.tmux.conf"));
    assert!(is_source_file_command("source ~/.tmux.conf"));
    assert!(!is_source_file_command("list-keys"));
    assert_eq!(
        parse_identity_reply(&reply(&["client-7\tbox\t/tmp/tmux-501/default"])),
        Some(ServerIdentity {
            client_name: "client-7".into(),
            host: "box".into(),
            socket_path: "/tmp/tmux-501/default".into(),
        })
    );
}

#[test]
fn the_fence_asks_for_the_client_key_table_and_pane() {
    assert_eq!(
        fence_command("client-7"),
        "display-message -p -c \"client-7\" '#{client_key_table} #{pane_id}'"
    );
    assert_eq!(
        parse_fence_reply(&reply(&["prefix %3"])),
        Some(("prefix".into(), Some(PaneId(3))))
    );
    assert_eq!(
        parse_fence_reply(&reply(&["root %0"])),
        Some(("root".into(), Some(PaneId(0))))
    );
}

#[test]
fn hooks_are_tagged_installed_at_a_fixed_slot_and_removed_there() {
    let install = install_hook_commands("client-7");
    let remove = remove_hook_commands("client-7");
    let slot = hook_slot("client-7");
    assert_eq!(install.len(), HOOK_NAMES.len());
    assert_eq!(
        install[0],
        format!(
            "set-hook -g after-bind-key[{slot}] 'display-message -c \"client-7\" {HOOK_MARKER}'"
        )
    );
    assert_eq!(remove[0], format!("set-hook -gu after-bind-key[{slot}]"));
    assert_ne!(hook_slot("client-7"), hook_slot("client-8"));
}

#[test]
fn a_stale_tagged_hook_from_a_crashed_warp_is_removed_on_attach() {
    let lines = vec![
        "after-bind-key[0] run-shell 'echo user hook'".to_owned(),
        format!("after-bind-key[4100] display-message -c client-1 {HOOK_MARKER}"),
        format!("after-bind-key[4200] display-message -c client-2 {HOOK_MARKER}"),
        format!("after-unbind-key[4100] display-message -c client-1 {HOOK_MARKER}"),
    ];
    let live = vec!["client-2".to_owned(), "client-9".to_owned()];
    assert_eq!(
        stale_hook_removals(&lines, &live),
        vec![
            "set-hook -gu after-bind-key[4100]".to_owned(),
            "set-hook -gu after-unbind-key[4100]".to_owned(),
        ]
    );
}
