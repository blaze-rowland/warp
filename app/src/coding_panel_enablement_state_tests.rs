use super::{CodingPanelEnablementState, RemoteUnavailableReason};

#[test]
fn remote_sessions_report_whether_a_remote_server_backs_them() {
    assert_eq!(
        CodingPanelEnablementState::from_session_env(true, true, false, true),
        CodingPanelEnablementState::RemoteSession {
            has_remote_server: true
        }
    );
    assert_eq!(
        CodingPanelEnablementState::from_session_env(true, true, false, false),
        CodingPanelEnablementState::RemoteSession {
            has_remote_server: false
        }
    );
}

#[test]
fn remote_unavailable_reasons_name_the_pane_or_host() {
    assert!(
        RemoteUnavailableReason::PaneOnOtherHost
            .message()
            .contains("unavailable for this pane")
    );
    assert!(
        RemoteUnavailableReason::NoRemoteServer
            .message()
            .contains("unavailable on this host")
    );
    assert!(
        RemoteUnavailableReason::Reconnecting
            .message()
            .starts_with("Reconnecting")
    );
}
