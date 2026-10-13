#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodingPanelEnablementState {
    Enabled,
    /// An SSH command has been detected at preexec time but the remote
    /// session has not finished bootstrapping yet. The file tree should
    /// show a loading state immediately to avoid flickering the stale
    /// local tree.
    PendingRemoteSession,
    /// The active session is on a remote host.
    ///
    /// `has_remote_server` is `true` when the session is registered with
    /// `RemoteServerManager` (i.e. Auto SSH Warpification / mode 1). When
    /// `true`, remote repo metadata may arrive and the file tree should show
    /// a loading state. When `false` (tmux or subshell SSH), no data will
    /// arrive and the file tree should show a disabled message.
    RemoteSession {
        has_remote_server: bool,
    },
    UnsupportedSession,
    /// The session is remote but its files can't be shown; the reason says why.
    RemoteUnavailable(RemoteUnavailableReason),
    Disabled,
}

/// Why a remote tmux pane's file explorer and code review are unavailable (R13, KTD9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteUnavailableReason {
    /// Warp's SSH extension can't run on the host.
    NoRemoteServer,
    /// The SSH extension's connection dropped and is being retried.
    Reconnecting,
    /// The pane's shell runs on a different host than its tmux session's connection.
    PaneOnOtherHost,
}

impl RemoteUnavailableReason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::NoRemoteServer => {
                "The file explorer and code review are unavailable on this host because Warp's SSH extension can't run there."
            }
            Self::Reconnecting => "Reconnecting to the remote host…",
            Self::PaneOnOtherHost => {
                "The file explorer is unavailable for this pane because it's on a different host than its tmux session."
            }
        }
    }
}

impl CodingPanelEnablementState {
    pub(crate) fn from_session_env(
        is_enabled: bool,
        is_remote: bool,
        is_unsupported_session: bool,
        has_remote_server: bool,
    ) -> Self {
        if is_remote {
            Self::RemoteSession { has_remote_server }
        } else if is_unsupported_session {
            Self::UnsupportedSession
        } else if is_enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

#[cfg(test)]
#[path = "coding_panel_enablement_state_tests.rs"]
mod tests;
