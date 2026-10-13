use super::*;

#[test]
#[cfg(windows)]
fn bracketed_paste_command_execution_normalizes_crlf_to_lf_for_posix_shells_on_windows() {
    let command = "curl 'https://google.com' \\\r\n  -H 'accept: application/json'";

    let bytes = bytes_to_execute_command(command, ShellType::Bash, true, false);

    let mut expected = ShellType::Bash.kill_buffer_bytes().to_vec();
    expected.extend_from_slice(escape_sequences::BRACKETED_PASTE_START);
    expected.extend_from_slice(b"curl 'https://google.com' \\\n  -H 'accept: application/json'");
    expected.extend_from_slice(escape_sequences::BRACKETED_PASTE_END);
    expected.extend_from_slice(ShellType::Bash.execute_command_bytes());

    assert_eq!(bytes, expected);
    assert!(!bytes.contains(&b'\r'));
}

#[test]
#[cfg(not(windows))]
fn bracketed_paste_command_execution_preserves_crlf_for_posix_shells_off_windows() {
    let command = "curl 'https://google.com' \\\r\n  -H 'accept: application/json'";

    let bytes = bytes_to_execute_command(command, ShellType::Bash, true, false);

    let mut expected = ShellType::Bash.kill_buffer_bytes().to_vec();
    expected.extend_from_slice(escape_sequences::BRACKETED_PASTE_START);
    expected.extend_from_slice(b"curl 'https://google.com' \\\r\n  -H 'accept: application/json'");
    expected.extend_from_slice(escape_sequences::BRACKETED_PASTE_END);
    expected.extend_from_slice(ShellType::Bash.execute_command_bytes());

    assert_eq!(bytes, expected);
    assert!(bytes.contains(&b'\r'));
}

#[test]
fn unbracketed_paste_command_execution_preserves_lf_for_posix_shells() {
    let command = "printf 'hello'\nprintf 'world'";

    let bytes = bytes_to_execute_command(command, ShellType::Bash, false, false);

    let mut expected = ShellType::Bash.kill_buffer_bytes().to_vec();
    expected.extend_from_slice(b"printf 'hello'\nprintf 'world'");
    expected.extend_from_slice(ShellType::Bash.execute_command_bytes());

    assert_eq!(bytes, expected);
    assert!(!bytes.contains(&b'\r'));
}

#[test]
fn powershell_command_execution_normalizes_linefeeds_to_carriage_returns() {
    let command = "Write-Output 'hello'\r\nWrite-Output 'world'\nWrite-Output 'again'";

    let bytes = bytes_to_execute_command(command, ShellType::PowerShell, false, false);

    let mut expected = ShellType::PowerShell.kill_buffer_bytes().to_vec();
    expected.extend_from_slice(b"Write-Output 'hello'\rWrite-Output 'world'\rWrite-Output 'again'");
    expected.extend_from_slice(ShellType::PowerShell.execute_command_bytes());

    assert_eq!(bytes, expected);
    assert!(!bytes.contains(&b'\n'));
}

#[test]
fn split_kill_buffer_write_splits_powershell_off_from_the_rest() {
    let bytes = bytes_to_execute_command("Get-ChildItem", ShellType::PowerShell, false, false);

    let (kill_buffer, rest) =
        split_kill_buffer_write(&bytes, ShellType::PowerShell).expect("PowerShell should split");

    assert_eq!(kill_buffer, ShellType::PowerShell.kill_buffer_bytes());
    let mut expected_rest = b"Get-ChildItem".to_vec();
    expected_rest.extend_from_slice(ShellType::PowerShell.execute_command_bytes());
    assert_eq!(rest, expected_rest.as_slice());
}

#[test]
fn split_kill_buffer_write_does_not_split_the_other_three_shells() {
    for shell_type in [ShellType::Zsh, ShellType::Bash, ShellType::Fish] {
        let bytes = bytes_to_execute_command("echo hi", shell_type, false, false);
        assert!(
            split_kill_buffer_write(&bytes, shell_type).is_none(),
            "expected no split for {shell_type:?}, which uses a single unambiguous control byte"
        );
    }
}

#[test]
fn split_kill_buffer_write_handles_a_command_with_no_content_gracefully() {
    let kill_buffer_only = ShellType::PowerShell.kill_buffer_bytes().to_vec();
    assert!(split_kill_buffer_write(&kill_buffer_only, ShellType::PowerShell).is_none());
}

#[test]
fn split_kill_buffer_write_returns_none_when_the_kill_buffer_is_not_the_prefix() {
    let kill_buffer = ShellType::PowerShell.kill_buffer_bytes();
    let not_prefixed = b"Get-ChildItem (no leading chord)".to_vec();
    assert!(
        not_prefixed.len() > kill_buffer.len(),
        "fixture must be longer than the chord so the length guard alone wouldn't catch it"
    );
    assert!(!not_prefixed.starts_with(kill_buffer));
    assert!(split_kill_buffer_write(&not_prefixed, ShellType::PowerShell).is_none());
}

#[test]
fn a_tmux_panes_command_clears_the_line_with_default_editing_keys() {
    // Ctrl-E (end of line) then Ctrl-U (kill to line start): keys every unmodified bash, zsh and
    // fish line editor already has, so the pane needs no Warp key binding (R10).
    for shell_type in [ShellType::Zsh, ShellType::Bash, ShellType::Fish] {
        let bytes = bytes_to_execute_command("ls", shell_type, false, true);

        let mut expected = b"\x05\x15ls".to_vec();
        expected.extend_from_slice(shell_type.execute_command_bytes());
        assert_eq!(bytes, expected, "{shell_type:?}");
        assert!(
            !bytes.contains(&escape_sequences::C0::DLE),
            "{shell_type:?} must not send Warp's Ctrl-P kill-buffer key"
        );
        assert!(
            !bytes.contains(&escape_sequences::C0::ESC),
            "{shell_type:?} must not send an ESC-prefixed Warp key"
        );
        assert!(split_kill_buffer_write(&bytes, shell_type).is_none());
    }
}

#[test]
fn a_tmux_panes_bracketed_paste_command_also_uses_the_default_clear() {
    let bytes = bytes_to_execute_command("ls", ShellType::Zsh, true, true);

    let mut expected = b"\x05\x15".to_vec();
    expected.extend_from_slice(escape_sequences::BRACKETED_PASTE_START);
    expected.extend_from_slice(b"ls");
    expected.extend_from_slice(escape_sequences::BRACKETED_PASTE_END);
    expected.extend_from_slice(ShellType::Zsh.execute_command_bytes());
    assert_eq!(bytes, expected);
}

#[test]
fn a_non_tmux_command_still_starts_with_the_kill_buffer_key() {
    for shell_type in [
        ShellType::Zsh,
        ShellType::Bash,
        ShellType::Fish,
        ShellType::PowerShell,
    ] {
        let bytes = bytes_to_execute_command("ls", shell_type, false, false);

        let mut expected = shell_type.kill_buffer_bytes().to_vec();
        expected.extend_from_slice(b"ls");
        expected.extend_from_slice(shell_type.execute_command_bytes());
        assert_eq!(bytes, expected, "{shell_type:?}");
    }
}
