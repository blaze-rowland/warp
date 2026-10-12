use super::*;

#[test]
fn send_keys_encodes_hex_for_the_target_pane() {
    assert_eq!(
        send_keys_command(PaneId(3), b"A\n"),
        vec!["send-keys -t %3 -H 41 0a\n".to_owned()]
    );
}

#[test]
fn empty_input_sends_no_commands() {
    assert!(send_keys_command(PaneId(3), b"").is_empty());
}

#[test]
fn a_300_byte_paste_is_three_commands_one_per_line() {
    let paste: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
    let commands = send_keys_command(PaneId(7), &paste);
    assert_eq!(commands.len(), 3);
    let mut decoded = Vec::new();
    for command in &commands {
        // Exactly one command line per element, so a reply correlator can count one each.
        assert!(command.ends_with('\n'));
        assert_eq!(command.matches('\n').count(), 1);
        let hex = command
            .strip_prefix("send-keys -t %7 -H ")
            .unwrap()
            .trim_end();
        decoded.extend(hex.split(' ').map(|b| u8::from_str_radix(b, 16).unwrap()));
    }
    assert_eq!(decoded, paste);
    let sizes: Vec<usize> = commands
        .iter()
        .map(|c| c.trim_end().split(' ').count() - 4)
        .collect();
    assert_eq!(sizes, vec![128, 128, 44]);
}

#[test]
fn refresh_client_size_is_one_command() {
    assert_eq!(
        refresh_client_size_command(120, 40),
        "refresh-client -C 120x40\n"
    );
}

#[test]
fn refresh_window_size_targets_one_window() {
    assert_eq!(
        refresh_client_window_size_command(WindowId(3), 200, 50),
        "refresh-client -C @3:200x50\n"
    );
}
