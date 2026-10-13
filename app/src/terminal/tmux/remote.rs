//! Remote parity for tmux sessions attached over SSH (U11, KTD9).
//!
//! tmux panes never become `RemoteServerManager` sessions: deregistering a session kills its ssh
//! child and ControlMaster, so the connection stays with the hidden origin pane's own
//! remote-server session for the whole attach. This module pairs each origin with that session
//! and maps every pane on the same host to its `HostId`, so pane working directories resolve as
//! remote paths and file, git and command requests go through the host-scoped calls.
//!
//! Every tmux pane session also gets a command executor that never types into the pane: plain
//! tmux clients share it, and an in-band executor would echo generator commands to them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use remote_server::client::RemoteServerClient;
use warp_core::{HostId, SessionId};
use warpui::{AppContext, Entity, ModelContext, ModelHandle, SingletonEntity, WeakModelHandle};

use super::TmuxOriginId;
use super::gateway::{TmuxGatewayEvent, TmuxGateways};
use crate::coding_panel_enablement_state::{CodingPanelEnablementState, RemoteUnavailableReason};
use crate::remote_server::manager::{
    RemoteServerManager, RemoteServerManagerEvent, RemoteSessionState,
};
use crate::terminal::model::session::command_executor::remote_server_executor::RemoteServerCommandExecutor;
use crate::terminal::model::session::command_executor::{
    CommandExecutor, InBandCommandExecutor, NoOpCommandExecutor, RemoteCommandExecutor,
};
use crate::terminal::model::session::{Session, SessionType, Sessions};

/// The origin pane's session, read when tmux attaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginSession {
    /// The origin's remote-server session, whose connection every pane shares.
    pub session_id: SessionId,
    pub hostname: String,
    /// The SSH wrapper's ControlMaster socket, when the wrapper made the connection.
    pub control_master: Option<PathBuf>,
}

impl OriginSession {
    /// The pairing source for a remote origin session; `None` for a local one.
    pub fn from_session(session: &Session) -> Option<Self> {
        if session.is_local() {
            return None;
        }
        Some(Self {
            session_id: session.id(),
            hostname: session.hostname().to_owned(),
            control_master: session.ssh_control_master_socket().map(ToOwned::to_owned),
        })
    }
}

/// The origin's remote-server connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerLink {
    Connecting,
    Connected {
        host_id: HostId,
    },
    /// The connection dropped and the manager is retrying.
    Reconnecting {
        host_id: HostId,
    },
    /// No remote server: the helper cannot run, setup was skipped, or retries ran out (R13).
    Unavailable,
}

impl ServerLink {
    /// The link for the manager's current state of the origin's session.
    pub fn from_state(state: Option<&RemoteSessionState>) -> Self {
        match state {
            None
            | Some(RemoteSessionState::Disconnected)
            | Some(RemoteSessionState::AwaitingExitStatus { .. }) => Self::Unavailable,
            Some(RemoteSessionState::Connecting | RemoteSessionState::Initializing { .. }) => {
                Self::Connecting
            }
            Some(RemoteSessionState::Connected { host_id, .. }) => Self::Connected {
                host_id: host_id.clone(),
            },
            Some(RemoteSessionState::Reconnecting { host_id, .. }) => Self::Reconnecting {
                host_id: host_id.clone(),
            },
        }
    }

    pub fn apply(self, event: LinkEvent) -> Self {
        match event {
            LinkEvent::Connecting => Self::Connecting,
            LinkEvent::Connected(host_id) => Self::Connected { host_id },
            LinkEvent::Disconnected => Self::Unavailable,
            // The manager announces a reconnect only as the host losing its last session; a
            // final drop announces the session's disconnect first.
            LinkEvent::HostDisconnected(lost) => match self {
                Self::Connected { host_id } if host_id == lost => Self::Reconnecting { host_id },
                other => other,
            },
        }
    }
}

/// A `RemoteServerManager` event as it affects one origin's link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    Connecting,
    /// Connected or reconnected.
    Connected(HostId),
    /// Failed, disconnected for good, or deregistered.
    Disconnected,
    HostDisconnected(HostId),
}

impl LinkEvent {
    /// How `event` affects the link of the origin whose remote-server session is `origin`.
    pub fn from_manager_event(event: &RemoteServerManagerEvent, origin: SessionId) -> Option<Self> {
        match event {
            RemoteServerManagerEvent::SessionConnecting { session_id } if *session_id == origin => {
                Some(Self::Connecting)
            }
            RemoteServerManagerEvent::SessionConnected {
                session_id,
                host_id,
            }
            | RemoteServerManagerEvent::SessionReconnected {
                session_id,
                host_id,
                ..
            } if *session_id == origin => Some(Self::Connected(host_id.clone())),
            RemoteServerManagerEvent::SessionConnectionFailed { session_id, .. }
            | RemoteServerManagerEvent::SessionDisconnected { session_id, .. }
            | RemoteServerManagerEvent::SessionDeregistered { session_id }
                if *session_id == origin =>
            {
                Some(Self::Disconnected)
            }
            RemoteServerManagerEvent::HostDisconnected { host_id } => {
                Some(Self::HostDisconnected(host_id.clone()))
            }
            _ => None,
        }
    }
}

/// One remote origin and its connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePairing {
    pub origin: OriginSession,
    pub link: ServerLink,
}

/// What a pane's session gets from its origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneRemote {
    /// Local tmux and a local pane: the usual rules apply.
    Unpaired,
    /// The pane runs on the host its origin is paired with.
    Paired(RemotePairing),
    /// The pane's shell runs on another host than the paired one, e.g. after `ssh` inside it.
    OtherHost,
}

impl PaneRemote {
    /// `pane_hostname` and `pane_is_local` describe the pane's integrated session. OSC 7 from a
    /// host other than this machine is dropped by the parser, so the session's host is the
    /// pane's host signal.
    pub fn new(pairing: Option<&RemotePairing>, pane_hostname: &str, pane_is_local: bool) -> Self {
        match pairing {
            Some(pairing) if pairing.origin.hostname == pane_hostname => {
                Self::Paired(pairing.clone())
            }
            Some(_) => Self::OtherHost,
            None if pane_is_local => Self::Unpaired,
            None => Self::OtherHost,
        }
    }

    /// The host this pane's working directories resolve against. `Some(None)` clears it.
    pub fn host_id(&self) -> Option<Option<HostId>> {
        match self {
            Self::Paired(RemotePairing {
                link: ServerLink::Connected { host_id },
                ..
            }) => Some(Some(host_id.clone())),
            Self::Paired(_) => Some(None),
            Self::Unpaired | Self::OtherHost => None,
        }
    }

    /// The registered remote-server session that serves this pane's session-scoped requests.
    pub fn remote_server_session(&self) -> Option<SessionId> {
        match self {
            Self::Paired(RemotePairing {
                origin,
                link: ServerLink::Connecting | ServerLink::Connected { .. },
            }) => Some(origin.session_id),
            Self::Paired(_) | Self::Unpaired | Self::OtherHost => None,
        }
    }

    /// The file explorer and code review state; `None` keeps the usual session-based state.
    pub(crate) fn coding_panel_state(&self) -> Option<CodingPanelEnablementState> {
        match self {
            Self::Unpaired => None,
            Self::OtherHost => Some(CodingPanelEnablementState::RemoteUnavailable(
                RemoteUnavailableReason::PaneOnOtherHost,
            )),
            Self::Paired(pairing) => Some(match pairing.link {
                ServerLink::Connecting | ServerLink::Connected { .. } => {
                    CodingPanelEnablementState::RemoteSession {
                        has_remote_server: true,
                    }
                }
                ServerLink::Reconnecting { .. } => CodingPanelEnablementState::RemoteUnavailable(
                    RemoteUnavailableReason::Reconnecting,
                ),
                ServerLink::Unavailable => CodingPanelEnablementState::RemoteUnavailable(
                    RemoteUnavailableReason::NoRemoteServer,
                ),
            }),
        }
    }

    pub fn executor_plan(&self) -> ExecutorPlan {
        match self {
            Self::Unpaired | Self::OtherHost => ExecutorPlan::Default,
            Self::Paired(RemotePairing { origin, link }) => match link {
                ServerLink::Connected { host_id } => ExecutorPlan::RemoteServer {
                    session_id: origin.session_id,
                    host_id: host_id.clone(),
                    fallback: origin.control_master.clone(),
                },
                ServerLink::Connecting
                | ServerLink::Reconnecting { .. }
                | ServerLink::Unavailable => match &origin.control_master {
                    Some(socket) => ExecutorPlan::ControlMaster(socket.clone()),
                    None => ExecutorPlan::None,
                },
            },
        }
    }
}

/// Which command executor a tmux pane session gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutorPlan {
    /// The executor Warp picks for any session, unless that one types into the pane.
    Default,
    /// The origin's remote-server connection; `fallback` if its client is gone.
    RemoteServer {
        session_id: SessionId,
        host_id: HostId,
        fallback: Option<PathBuf>,
    },
    /// A fresh ssh per command over the origin SSH session's ControlMaster socket.
    ControlMaster(PathBuf),
    /// No executor: completions and generators that need one are unavailable (R13).
    None,
}

/// The executor for `plan`. Never an [`InBandCommandExecutor`]. `default` is the executor Warp
/// picked for the session; `client` finds the remote-server client for a session and host.
pub fn resolve_executor(
    plan: ExecutorPlan,
    default: Arc<dyn CommandExecutor>,
    client: impl FnOnce(SessionId, &HostId) -> Option<Arc<RemoteServerClient>>,
) -> Arc<dyn CommandExecutor> {
    let control_master = |socket: Option<PathBuf>| -> Arc<dyn CommandExecutor> {
        match socket {
            Some(socket) => Arc::new(RemoteCommandExecutor::new(socket, None)),
            None => Arc::new(NoOpCommandExecutor::new()),
        }
    };
    match plan {
        ExecutorPlan::Default if is_in_band(default.as_ref()) => {
            Arc::new(NoOpCommandExecutor::new())
        }
        ExecutorPlan::Default => default,
        ExecutorPlan::RemoteServer {
            session_id,
            host_id,
            fallback,
        } => match client(session_id, &host_id) {
            Some(client) => Arc::new(RemoteServerCommandExecutor::new(session_id, client)),
            None => control_master(fallback),
        },
        ExecutorPlan::ControlMaster(socket) => control_master(Some(socket)),
        ExecutorPlan::None => control_master(None),
    }
}

fn is_in_band(executor: &dyn CommandExecutor) -> bool {
    executor
        .as_any()
        .downcast_ref::<InBandCommandExecutor>()
        .is_some()
}

/// R13's banner on the session's tabs (R29).
pub fn banner_text(pairing: &RemotePairing) -> Option<String> {
    let host = &pairing.origin.hostname;
    match pairing.link {
        ServerLink::Connecting | ServerLink::Connected { .. } => None,
        ServerLink::Reconnecting { .. } => Some(format!(
            "Reconnecting to Warp's SSH extension on {host}. The file explorer and code review \
             are paused until it is back."
        )),
        ServerLink::Unavailable => Some(format!(
            "The file explorer and code review are unavailable on {host} because Warp's SSH \
             extension can't run there. Blocks and AI still work."
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxRemoteEvent {
    /// `origin`'s link changed: refresh its banner and the coding panel.
    Changed { origin: TmuxOriginId },
}

/// Every remote tmux session's pairing. See the module docs.
#[derive(Default)]
pub struct TmuxRemoteHosts {
    pairings: HashMap<TmuxOriginId, RemotePairing>,
    panes: HashMap<TmuxOriginId, Vec<WeakModelHandle<Sessions>>>,
    /// Hosts whose unavailable notice the user dismissed; it shows once per host.
    dismissed_hosts: HashSet<String>,
}

impl Entity for TmuxRemoteHosts {
    type Event = TmuxRemoteEvent;
}

impl SingletonEntity for TmuxRemoteHosts {}

impl TmuxRemoteHosts {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        if ctx.has_singleton_model::<RemoteServerManager>() {
            ctx.subscribe_to_model(&RemoteServerManager::handle(ctx), |me, _, event, ctx| {
                me.on_manager_event(event, ctx);
            });
        }
        if ctx.has_singleton_model::<TmuxGateways>() {
            ctx.subscribe_to_model(&TmuxGateways::handle(ctx), |me, _, event, ctx| {
                if let TmuxGatewayEvent::Ended { origin } = event {
                    me.forget(*origin, ctx);
                }
            });
        }
        Self::default()
    }

    /// Pairs `origin` with its origin pane's session (KTD9). A local origin unpairs.
    pub fn pair(
        &mut self,
        origin: TmuxOriginId,
        session: Option<OriginSession>,
        ctx: &mut ModelContext<Self>,
    ) {
        let pairing = session.map(|origin_session| {
            let link = if ctx.has_singleton_model::<RemoteServerManager>() {
                ServerLink::from_state(
                    RemoteServerManager::as_ref(ctx).session(origin_session.session_id),
                )
            } else {
                ServerLink::Unavailable
            };
            RemotePairing {
                origin: origin_session,
                link,
            }
        });
        let changed = match pairing {
            Some(pairing) => self.pairings.insert(origin, pairing.clone()) != Some(pairing),
            None => self.pairings.remove(&origin).is_some(),
        };
        if changed {
            self.refresh_panes(origin, ctx);
            ctx.emit(TmuxRemoteEvent::Changed { origin });
        }
    }

    pub fn pairing(&self, origin: TmuxOriginId) -> Option<&RemotePairing> {
        self.pairings.get(&origin)
    }

    /// Follows a pane's sessions so their executor and host follow the link.
    pub fn register_pane(
        &mut self,
        origin: TmuxOriginId,
        sessions: &ModelHandle<Sessions>,
        ctx: &AppContext,
    ) {
        let panes = self.panes.entry(origin).or_default();
        panes.retain(|weak| weak.upgrade(ctx).is_some());
        panes.push(sessions.downgrade());
    }

    pub fn pane_remote(&self, origin: TmuxOriginId, session: &Session) -> PaneRemote {
        PaneRemote::new(
            self.pairings.get(&origin),
            session.hostname(),
            session.is_local(),
        )
    }

    /// The text of `origin`'s banner, unless dismissed for its host.
    pub fn banner(&self, origin: TmuxOriginId) -> Option<String> {
        let pairing = self.pairings.get(&origin)?;
        if pairing.link == ServerLink::Unavailable
            && self.dismissed_hosts.contains(&pairing.origin.hostname)
        {
            return None;
        }
        banner_text(pairing)
    }

    /// Hides the unavailable notice for `origin`'s host. Reconnecting is not dismissible.
    pub fn dismiss_banner(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let Some(pairing) = self.pairings.get(&origin) else {
            return;
        };
        if pairing.link != ServerLink::Unavailable {
            return;
        }
        let host = pairing.origin.hostname.clone();
        if !self.dismissed_hosts.insert(host.clone()) {
            return;
        }
        let affected: Vec<TmuxOriginId> = self
            .pairings
            .iter()
            .filter(|(_, pairing)| pairing.origin.hostname == host)
            .map(|(origin, _)| *origin)
            .collect();
        for origin in affected {
            ctx.emit(TmuxRemoteEvent::Changed { origin });
        }
    }

    #[cfg(test)]
    fn set_pairing_for_test(
        &mut self,
        origin: TmuxOriginId,
        pairing: RemotePairing,
        ctx: &mut ModelContext<Self>,
    ) {
        self.pairings.insert(origin, pairing);
        self.refresh_panes(origin, ctx);
        ctx.emit(TmuxRemoteEvent::Changed { origin });
    }

    fn forget(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        self.panes.remove(&origin);
        if self.pairings.remove(&origin).is_some() {
            ctx.emit(TmuxRemoteEvent::Changed { origin });
        }
    }

    fn on_manager_event(&mut self, event: &RemoteServerManagerEvent, ctx: &mut ModelContext<Self>) {
        let mut changed = Vec::new();
        for (origin, pairing) in &mut self.pairings {
            let Some(link_event) = LinkEvent::from_manager_event(event, pairing.origin.session_id)
            else {
                continue;
            };
            let next = pairing.link.clone().apply(link_event);
            if next != pairing.link {
                pairing.link = next;
                changed.push(*origin);
            }
        }
        for origin in changed {
            self.refresh_panes(origin, ctx);
            ctx.emit(TmuxRemoteEvent::Changed { origin });
        }
    }

    fn refresh_panes(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let Some(panes) = self.panes.get_mut(&origin) else {
            return;
        };
        let mut live = Vec::new();
        panes.retain(|weak| match weak.upgrade(ctx) {
            Some(sessions) => {
                live.push(sessions);
                true
            }
            None => false,
        });
        for sessions in live {
            let pane_sessions: Vec<Arc<Session>> =
                sessions.as_ref(ctx).sessions().cloned().collect();
            for session in pane_sessions {
                apply_pane_remote(&self.pane_remote(origin, &session), &session, ctx);
            }
        }
    }
}

/// Gives a newly bootstrapped tmux pane session its executor and remote host.
pub fn adopt_pane_session(origin: TmuxOriginId, session: &Session, ctx: &AppContext) {
    let pane = if ctx.has_singleton_model::<TmuxRemoteHosts>() {
        TmuxRemoteHosts::as_ref(ctx).pane_remote(origin, session)
    } else {
        PaneRemote::new(None, session.hostname(), session.is_local())
    };
    apply_pane_remote(&pane, session, ctx);
}

fn apply_pane_remote(pane: &PaneRemote, session: &Session, ctx: &AppContext) {
    let executor = resolve_executor(
        pane.executor_plan(),
        session.command_executor(),
        |session_id, host_id| {
            if !ctx.has_singleton_model::<RemoteServerManager>() {
                return None;
            }
            let manager = RemoteServerManager::as_ref(ctx);
            manager
                .client_for_session(session_id)
                .or_else(|| manager.client_for_host(host_id))
                .cloned()
        },
    );
    session.set_command_executor(executor);
    if let Some(host_id) = pane.host_id()
        && matches!(session.session_type(), SessionType::WarpifiedRemote { .. })
    {
        session.set_remote_host_id(host_id);
    }
}

/// The pane state for a session of a tmux pane in `origin`'s session.
pub fn pane_remote_for_view(
    origin: TmuxOriginId,
    session: &Session,
    ctx: &AppContext,
) -> Option<PaneRemote> {
    if !ctx.has_singleton_model::<TmuxRemoteHosts>() {
        return None;
    }
    Some(TmuxRemoteHosts::as_ref(ctx).pane_remote(origin, session))
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
