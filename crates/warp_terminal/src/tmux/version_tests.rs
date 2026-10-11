use super::*;

fn gates(input: &str) -> (bool, bool) {
    let version = TmuxVersion::parse(input).unwrap_or_else(|| panic!("{input} did not parse"));
    (
        version.meets_native_floor(),
        version.supports_client_key_commands(),
    )
}

#[test]
fn release_versions_parse_to_the_right_gates() {
    assert_eq!(gates("3.2"), (true, false));
    assert_eq!(gates("3.2a"), (true, false));
    assert_eq!(gates("3.3a"), (true, false));
    assert_eq!(gates("3.4"), (true, true));
    assert_eq!(gates("3.7c"), (true, true));
    assert_eq!(gates("4.0"), (true, true));
    assert_eq!(gates("3.1c"), (false, false));
    assert_eq!(gates("2.9"), (false, false));
    assert_eq!(gates("1.8"), (false, false));
}

#[test]
fn tmux_dash_v_output_is_accepted() {
    assert_eq!(
        TmuxVersion::parse("tmux 3.7c\n"),
        Some(TmuxVersion::Release {
            major: 3,
            minor: 7,
            letter: Some('c')
        })
    );
    assert_eq!(gates("tmux 3.4"), (true, true));
}

#[test]
fn development_and_platform_builds_parse() {
    assert_eq!(
        TmuxVersion::parse("next-3.8"),
        Some(TmuxVersion::Next { major: 3, minor: 8 })
    );
    assert_eq!(gates("tmux next-3.8"), (true, true));
    assert_eq!(gates("next-3.3"), (true, false));
    assert_eq!(TmuxVersion::parse("master"), Some(TmuxVersion::Master));
    assert_eq!(gates("tmux master"), (true, true));
    assert_eq!(
        TmuxVersion::parse("openbsd-7.5"),
        Some(TmuxVersion::Platform {
            name: "openbsd".into(),
            version: "7.5".into()
        })
    );
    assert_eq!(gates("tmux openbsd-7.5"), (true, true));
}

#[test]
fn release_candidates_count_as_their_release() {
    assert_eq!(gates("3.4-rc"), (true, true));
    assert_eq!(gates("3.3-rc2"), (true, false));
}

#[test]
fn garbage_does_not_parse() {
    for input in ["", "tmux", "abc", "3", "3.", ".4", "next-", "next-x"] {
        assert_eq!(TmuxVersion::parse(input), None, "{input:?}");
    }
}
