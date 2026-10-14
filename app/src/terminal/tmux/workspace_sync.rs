//! tmux sessions as Warp tabs: the session registry, tagged tmux commands, and the pure planning
//! the workspace uses to keep tabs in sync with a session (U7, KTD4, R23–R25, R28, R29).
//!
//! - [`TmuxSessions`] follows every gateway. It knows each session's name, host and status
//!   (attached, not responding, disconnected), routes command replies to their callbacks, and
//!   re-emits what workspaces need as [`TmuxSessionEvent`]s.
//! - [`plan_window_sync`] diffs a session model against the tabs a workspace shows. Layouts within
//!   a tab diff by pane id in [`super::layout_snapshot`].
//! - The command builders here are the only place Warp actions become tmux commands. Only an
//!   explicit close sends `kill-pane` or `kill-window` (R25, KTD3).

use std::collections::{HashMap, HashSet};

use warp_terminal::local_tty::tmux_gateway::{RefusalReason, TmuxRequest};
use warp_terminal::tmux::{
    ClientEvent, CommandReply, PaneId, SessionId, SessionModel, TmuxWindow, WindowId,
    quote_argument,
};
use warpui::{AppContext, Entity, ModelContext, SingletonEntity};

use super::TmuxOriginId;
use super::gateway::{TmuxGatewayEvent, TmuxGateways};
pub use super::hidden_tabs::{
    last_visible, nearest_visible, raw_to_visible, step_visible, visible_to_raw,
};
use super::pane_manager::WINDOW_SIZE_COMMAND_TAG;
use crate::terminal::shell::ShellType;

/// Longest tab title taken from a tmux window name.
pub const MAX_TITLE_CHARS: usize = 120;

// Window planning.

/// A tab a workspace shows for one tmux window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenTab {
    pub window_id: WindowId,
    pub title: String,
}

/// One change that brings a workspace's tabs in line with a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowOp {
    /// Open a tab for a window that has a layout.
    Open { window_id: WindowId, title: String },
    /// The window is gone; remove its tab. tmux already closed it, so nothing is sent.
    Close { window_id: WindowId },
    /// Apply the window's current layout to its tab (a no-op when nothing changed).
    Update { window_id: WindowId },
    /// The window's name changed.
    Rename { window_id: WindowId, title: String },
}

/// Diffs `model` against the tabs a workspace shows for it. Closes come first, then windows in
/// tmux index order. Windows whose layout is not known yet wait for it.
pub fn plan_window_sync(model: &SessionModel, open: &[OpenTab]) -> Vec<WindowOp> {
    let mut ops: Vec<WindowOp> = open
        .iter()
        .filter(|tab| model.window(tab.window_id).is_none())
        .map(|tab| WindowOp::Close {
            window_id: tab.window_id,
        })
        .collect();
    // Popup windows (U14) show as floating panes, not tabs. A new window's name arrives just
    // after its layout, so a window waits for its name before it gets a tab.
    let mut windows: Vec<&TmuxWindow> = model
        .windows()
        .filter(|window| {
            window
                .name
                .as_deref()
                .is_none_or(|name| !super::popup::is_popup_window_name(name))
        })
        .collect();
    windows.sort_by_key(|window| (window.index.unwrap_or(u32::MAX), window.id));
    for window in windows {
        let title = window_title(window);
        match open.iter().find(|tab| tab.window_id == window.id) {
            Some(tab) => {
                ops.push(WindowOp::Update {
                    window_id: window.id,
                });
                if tab.title != title {
                    ops.push(WindowOp::Rename {
                        window_id: window.id,
                        title,
                    });
                }
            }
            None if window.layout.is_some() && window.name.is_some() => ops.push(WindowOp::Open {
                window_id: window.id,
                title,
            }),
            None => {}
        }
    }
    ops
}

/// Where a new window's tab goes among a session's tabs, given the tmux indices of the tabs
/// already open in tab order: after every tab with a lower index.
pub fn insertion_offset(open_indices: &[u32], new_index: u32) -> usize {
    open_indices
        .iter()
        .filter(|index| **index < new_index)
        .count()
}

// Titles and tooltips.

/// A window name made safe for a tab title: control characters (C0, DEL and C1, which carry
/// escape sequences) removed, trimmed and bounded.
pub fn sanitize_title(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    cleaned.trim().chars().take(MAX_TITLE_CHARS).collect()
}

/// A window's tab title: its sanitized name, or its index when the name is empty.
pub fn window_title(window: &TmuxWindow) -> String {
    let title = window
        .name
        .as_deref()
        .map(sanitize_title)
        .unwrap_or_default();
    if !title.is_empty() {
        return title;
    }
    window
        .index
        .map(|index| index.to_string())
        .unwrap_or_else(|| "tmux".to_owned())
}

/// The tooltip on a tmux tab (R28).
pub fn session_tooltip(session: &str, host: &str) -> String {
    format!("tmux session {session} on {host}")
}

// Session end (R24).

/// How a session ended, for the line the origin tab shows when it returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// `%exit`, with tmux's reason when it gave one.
    Exited(Option<String>),
    /// tmux exited before attaching.
    AttachFailed(Option<String>),
    /// The transport or the reply stream broke.
    Disconnected,
    /// The gateway refused the session (R6), with the reason.
    Refused(String),
}

/// The line the origin tab shows when the session ends, naming the session and how it ended.
pub fn end_line(session: &str, end: &SessionEnd) -> String {
    match end {
        SessionEnd::Exited(Some(reason)) => {
            let reason = sanitize_title(reason);
            if reason.contains("detached") {
                format!("[detached from {session}]")
            } else if reason.is_empty() {
                format!("[exited from {session}]")
            } else {
                format!("[{reason}: {session}]")
            }
        }
        SessionEnd::Exited(None) => format!("[exited from {session}]"),
        SessionEnd::AttachFailed(Some(error)) => format!("[tmux: {}]", sanitize_title(error)),
        SessionEnd::AttachFailed(None) => "[tmux: could not attach]".to_owned(),
        SessionEnd::Disconnected => format!("[lost connection to {session}]"),
        SessionEnd::Refused(reason) => format!("[tmux: {reason}]"),
    }
}

/// The line the origin pane shows when a control-mode entry stays raw (R6, R29).
pub fn refusal_line(reason: &RefusalReason) -> String {
    end_line("", &SessionEnd::Refused(reason.to_string()))
}

// Close confirmation (R25).

/// Whether `command` (a `#{pane_current_command}`) is an idle shell, which closes without asking.
pub fn is_idle_shell(command: Option<&str>) -> bool {
    let Some(command) = command.map(str::trim).filter(|c| !c.is_empty()) else {
        return true;
    };
    if ShellType::from_name(command).is_some() {
        return true;
    }
    let name = command.trim_start_matches('-');
    let name = name.rsplit('/').next().unwrap_or(name);
    matches!(
        name,
        "sh" | "dash" | "ksh" | "mksh" | "tcsh" | "csh" | "nu" | "xonsh"
    )
}

/// The commands other than idle shells in a `#{pane_current_command}` reply.
pub fn busy_commands(reply: &CommandReply) -> Vec<String> {
    if reply.error {
        return Vec::new();
    }
    reply
        .lines
        .iter()
        .map(|line| sanitize_title(&String::from_utf8_lossy(line)))
        .filter(|command| !is_idle_shell(Some(command)))
        .collect()
}

/// The confirmation text for killing panes running `commands`.
pub fn close_confirmation_text(commands: &[String]) -> String {
    let names = match commands {
        [] => "A process".to_owned(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    let verb = if commands.len() > 1 { "are" } else { "is" };
    let pronoun = if commands.len() > 1 { "They" } else { "It" };
    format!("{names} {verb} still running. {pronoun} will be stopped.")
}

/// Asks before killing tmux panes or windows that run something (R25). `on_confirm` sends the
/// kill; cancelling sends nothing.
pub fn show_kill_confirmation(
    title: &str,
    text: String,
    on_confirm: impl FnOnce(&mut AppContext) + 'static,
    ctx: &mut AppContext,
) {
    show_confirmation(title, text, "Yes, close", on_confirm, ctx);
}

/// Shows a native confirmation with `confirm_label` and Cancel. `on_confirm` runs only when the
/// user confirms.
pub fn show_confirmation(
    title: &str,
    text: String,
    confirm_label: &str,
    on_confirm: impl FnOnce(&mut AppContext) + 'static,
    ctx: &mut AppContext,
) {
    use warpui::modals::{AlertDialogWithCallbacks, ModalButton};

    let dialog = AlertDialogWithCallbacks::for_app(
        title,
        text,
        vec![
            ModalButton::for_app(confirm_label.to_owned(), on_confirm),
            ModalButton::for_app("Cancel".to_owned(), |_| {}),
        ],
        |_| {},
    );
    if cfg!(target_os = "macos") {
        ctx.show_native_platform_modal(dialog);
        return;
    }
    let Some(window_id) = ctx.windows().active_window() else {
        return;
    };
    if let Some(workspace) = ctx
        .views_of_type::<crate::workspace::Workspace>(window_id)
        .and_then(|workspaces| workspaces.first().cloned())
    {
        workspace.update(ctx, |workspace, ctx| {
            workspace.show_native_modal(dialog, ctx)
        });
    }
}

// Command builders: Warp actions as tmux commands. Panes and windows Warp creates start through
// U9's launcher when it is installed (`launcher`); otherwise they start tmux's default shell.

/// Where a Warp split puts the new pane relative to the target pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitPlacement {
    Left,
    Right,
    Up,
    Down,
}

/// Splits `target` the way a Warp split would, starting in the pane's directory, through the
/// launcher at `launcher` when there is one.
pub fn split_window_command(
    target: PaneId,
    placement: SplitPlacement,
    launcher: Option<&str>,
) -> String {
    let flags = match placement {
        SplitPlacement::Right => "-h",
        SplitPlacement::Left => "-h -b",
        SplitPlacement::Down => "-v",
        SplitPlacement::Up => "-v -b",
    };
    with_launcher(
        format!("split-window {flags} -t {target} -c '#{{pane_current_path}}'"),
        launcher,
    )
}

/// Appends the launcher as the new pane's shell-command.
fn with_launcher(command: String, launcher: Option<&str>) -> String {
    match launcher {
        Some(launcher) => format!(
            "{command} {}",
            super::integration::launcher_shell_command(launcher)
        ),
        None => command,
    }
}

/// Kills a pane. Only an explicit close sends this (R25).
pub fn kill_pane_command(pane_id: PaneId) -> String {
    format!("kill-pane -t {pane_id}")
}

/// Kills a window. Only an explicit close sends this (R25).
pub fn kill_window_command(window_id: WindowId) -> String {
    format!("kill-window -t {window_id}")
}

/// A new window after `after` (the active tab's window), in its directory, through the launcher
/// at `launcher` when there is one.
pub fn new_window_command(after: Option<WindowId>, launcher: Option<&str>) -> String {
    let command = match after {
        Some(window_id) => format!("new-window -a -t {window_id} -c '#{{pane_current_path}}'"),
        None => "new-window".to_owned(),
    };
    with_launcher(command, launcher)
}

pub fn rename_window_command(window_id: WindowId, name: &str) -> String {
    format!(
        "rename-window -t {window_id} {}",
        quote_argument(&sanitize_title(name))
    )
}

pub fn select_window_command(window_id: WindowId) -> String {
    format!("select-window -t {window_id}")
}

pub fn select_pane_command(pane_id: PaneId) -> String {
    format!("select-pane -t {pane_id}")
}

/// The commands that make tmux's active window and pane the ones Warp shows (`window_id`, with
/// `pane_id` focused), as far as `model` knows. Empty when tmux already agrees, which is always
/// the case when the focus change came from tmux, so nothing is echoed back.
pub fn focus_sync_commands(
    model: &SessionModel,
    window_id: WindowId,
    pane_id: Option<PaneId>,
) -> Vec<String> {
    let Some(window) = model.window(window_id) else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    if model.active_window() != Some(window_id) {
        commands.push(select_window_command(window_id));
    }
    if let Some(pane_id) = pane_id
        && window.active_pane != Some(pane_id)
        && model
            .pane(pane_id)
            .is_some_and(|pane| pane.window_id == window_id)
    {
        commands.push(select_pane_command(pane_id));
    }
    commands
}

/// Asks for the foreground command of every pane in a window.
pub fn window_commands_query(window_id: WindowId) -> String {
    format!("list-panes -t {window_id} -F '#{{pane_current_command}}'")
}

/// Asks for a pane's foreground command.
pub fn pane_command_query(pane_id: PaneId) -> String {
    format!("display-message -p -t {pane_id} '#{{pane_current_command}}'")
}

/// The shell command Reattach runs in the origin pane.
pub fn reattach_command(session: &str) -> String {
    format!(
        "tmux -CC attach -t '{}'",
        sanitize_title(session).replace('\'', "'\\''")
    )
}

// Duplicate sessions.

/// What makes two attaches the same session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionKey {
    pub host: String,
    pub session_id: SessionId,
    pub name: String,
}

/// Another origin already attached to the session `key` names.
pub fn find_duplicate(
    sessions: &[(TmuxOriginId, SessionKey)],
    origin: TmuxOriginId,
    key: &SessionKey,
) -> Option<TmuxOriginId> {
    sessions
        .iter()
        .find(|(other, other_key)| *other != origin && other_key == key)
        .map(|(other, _)| *other)
}

// The session registry.

/// Where a session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    /// The handshake is running.
    Starting,
    Attached,
    /// A command has gone unanswered for 3 seconds (KTD13).
    NotResponding,
    /// The transport or reply stream broke; tabs stay, read-only, until reattached or closed.
    Disconnected,
}

/// What the registry knows about one origin's session.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub name: String,
    pub session_id: Option<SessionId>,
    /// The host the session runs on, once the workspace reports it.
    pub host: Option<String>,
    pub status: SessionStatus,
    end: Option<SessionEnd>,
    /// This attach duplicated a session another origin already shows.
    duplicate_of: Option<TmuxOriginId>,
}

impl SessionInfo {
    fn new() -> Self {
        Self {
            name: String::new(),
            session_id: None,
            host: None,
            status: SessionStatus::Starting,
            end: None,
            duplicate_of: None,
        }
    }

    /// The tab tooltip (R28).
    pub fn tooltip(&self) -> String {
        session_tooltip(&self.name, self.host.as_deref().unwrap_or("this computer"))
    }

    pub fn is_duplicate(&self) -> bool {
        self.duplicate_of.is_some()
    }
}

/// What workspaces act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxSessionEvent {
    /// The session attached (or switched sessions): build its tabs from the model.
    Attached { origin: TmuxOriginId },
    /// Windows were added, closed, renamed or re-laid out.
    WindowsChanged { origin: TmuxOriginId },
    ActiveWindowChanged {
        origin: TmuxOriginId,
        window_id: WindowId,
    },
    ActivePaneChanged {
        origin: TmuxOriginId,
        window_id: WindowId,
        pane_id: PaneId,
    },
    /// Status, name or host changed: redraw banners and tooltips.
    StatusChanged { origin: TmuxOriginId },
    /// The session is over: remove its tabs and bring back the origin tab with `line`.
    Ended {
        origin: TmuxOriginId,
        line: Option<String>,
    },
    /// Focus this already-attached session's tabs.
    Focus { origin: TmuxOriginId },
    /// Show `line` in the origin pane; no session started (R6, R29).
    Notice { origin: TmuxOriginId, line: String },
    /// A Reattach that waited for the old control client to end can run now.
    ReattachReady { origin: TmuxOriginId },
    /// Replies for commands sent with [`TmuxSessions::send_command_app`] are waiting for
    /// [`TmuxReplyRunner`], which runs them once the registry's update has finished.
    RepliesReady,
}

/// Called with a tagged command's reply. It runs inside the registry's update, so it gets the
/// registry to send follow-up commands with.
pub type ReplyCallback =
    Box<dyn FnOnce(&CommandReply, &mut TmuxSessions, &mut ModelContext<TmuxSessions>)>;

/// Called with a tagged command's reply outside the registry's update, so it may update any model,
/// the registry included (for example to send a follow-up through another model's helper).
pub type AppReplyCallback = Box<dyn FnOnce(&CommandReply, &mut AppContext)>;

enum PendingReply {
    /// Runs inside the registry's update with the registry in hand.
    Registry(ReplyCallback),
    /// Runs after the registry's update, from [`TmuxReplyRunner`].
    App(AppReplyCallback),
}

/// Every tmux session in this Warp instance. See the module docs.
pub struct TmuxSessions {
    sessions: HashMap<TmuxOriginId, SessionInfo>,
    pending: HashMap<(TmuxOriginId, u64), PendingReply>,
    /// App replies whose commands completed, waiting for [`TmuxReplyRunner`].
    ready_replies: Vec<(CommandReply, AppReplyCallback)>,
    next_tag: u64,
    /// Disconnected sessions whose Reattach waits for the old control client to end.
    reattach_after_end: HashSet<TmuxOriginId>,
}

impl Entity for TmuxSessions {
    type Event = TmuxSessionEvent;
}

impl SingletonEntity for TmuxSessions {}

impl TmuxSessions {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxGateways::handle(ctx), |me, _, event, ctx| {
            me.on_gateway_event(event, ctx);
        });
        Self {
            sessions: HashMap::new(),
            pending: HashMap::new(),
            ready_replies: Vec::new(),
            next_tag: 1,
            reattach_after_end: HashSet::new(),
        }
    }

    /// Whether Reattach can run `tmux -CC attach` in `origin`'s pane now. While the old control
    /// client still holds the PTY, a command typed there would go to tmux, so this detaches it
    /// and emits [`TmuxSessionEvent::ReattachReady`] once it has ended.
    pub fn request_reattach(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) -> bool {
        let gateways = TmuxGateways::as_ref(ctx);
        if !gateways.is_in_session(origin) {
            return true;
        }
        self.reattach_after_end.insert(origin);
        gateways.detach(origin);
        false
    }

    pub fn session(&self, origin: TmuxOriginId) -> Option<&SessionInfo> {
        self.sessions.get(&origin)
    }

    pub fn status(&self, origin: TmuxOriginId) -> Option<SessionStatus> {
        self.sessions.get(&origin).map(|session| session.status)
    }

    /// Makes tmux's active window and pane match what Warp shows for `origin`, so tmux bindings
    /// act on the pane the user is looking at.
    pub fn sync_focus(
        &mut self,
        origin: TmuxOriginId,
        window_id: WindowId,
        pane_id: Option<PaneId>,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(model) = TmuxGateways::as_ref(ctx).model(origin) else {
            return;
        };
        for line in focus_sync_commands(model, window_id, pane_id) {
            self.send_command(origin, line, None, ctx);
        }
    }

    /// Sends one command line to `origin`'s session. `on_reply` runs once with its reply.
    /// Returns the tag, or `None` when the origin is gone.
    pub fn send_command(
        &mut self,
        origin: TmuxOriginId,
        line: String,
        on_reply: Option<ReplyCallback>,
        ctx: &mut ModelContext<Self>,
    ) -> Option<u64> {
        let sender = TmuxGateways::as_ref(ctx).request_sender(origin)?;
        // A `source-file` may rebind keys; refresh the key mirror once it ran (R17).
        let on_reply = if super::key_tables::is_source_file_command(&line)
            && ctx.has_singleton_model::<super::keys::TmuxKeyRouter>()
        {
            let inner = on_reply;
            Some(Box::new(
                move |reply: &CommandReply,
                      sessions: &mut TmuxSessions,
                      ctx: &mut ModelContext<TmuxSessions>| {
                    super::keys::TmuxKeyRouter::handle(ctx)
                        .update(ctx, |router, ctx| router.refresh(origin, ctx));
                    if let Some(inner) = inner {
                        inner(reply, sessions, ctx);
                    }
                },
            ) as ReplyCallback)
        } else {
            on_reply
        };
        let tag = self.next_tag;
        self.next_tag += 1;
        if self.next_tag == WINDOW_SIZE_COMMAND_TAG {
            self.next_tag = 1;
        }
        if !sender.send(TmuxRequest::Command { tag, line }) {
            return None;
        }
        if let Some(on_reply) = on_reply {
            self.pending
                .insert((origin, tag), PendingReply::Registry(on_reply));
        }
        Some(tag)
    }

    /// Like [`Self::send_command`], but `on_reply` runs after the registry's update instead of
    /// inside it. Models outside the registry use this: their reply handlers often send follow-up
    /// commands through the registry, which would be a circular update if they ran inside it.
    pub fn send_command_app(
        &mut self,
        origin: TmuxOriginId,
        line: String,
        on_reply: Option<AppReplyCallback>,
        ctx: &mut ModelContext<Self>,
    ) -> Option<u64> {
        let tag = self.send_command(origin, line, None, ctx)?;
        if let Some(on_reply) = on_reply {
            self.pending
                .insert((origin, tag), PendingReply::App(on_reply));
        }
        Some(tag)
    }

    /// Takes the app replies that are ready to run.
    fn take_ready_replies(&mut self) -> Vec<(CommandReply, AppReplyCallback)> {
        std::mem::take(&mut self.ready_replies)
    }

    /// Records the host a session runs on and checks whether another origin already shows the
    /// same session. Returns that origin; the caller detaches this one and the registry asks
    /// workspaces to focus the existing tabs.
    pub fn register_host(
        &mut self,
        origin: TmuxOriginId,
        host: String,
        ctx: &mut ModelContext<Self>,
    ) -> Option<TmuxOriginId> {
        let session = self.sessions.get_mut(&origin)?;
        session.host = Some(host.clone());
        let key = SessionKey {
            host,
            session_id: session.session_id?,
            name: session.name.clone(),
        };
        let others: Vec<(TmuxOriginId, SessionKey)> = self
            .sessions
            .iter()
            .filter(|(_, other)| {
                other.duplicate_of.is_none()
                    && matches!(
                        other.status,
                        SessionStatus::Attached | SessionStatus::NotResponding
                    )
            })
            .filter_map(|(other, info)| {
                Some((
                    *other,
                    SessionKey {
                        host: info.host.clone()?,
                        session_id: info.session_id?,
                        name: info.name.clone(),
                    },
                ))
            })
            .collect();
        let duplicate = find_duplicate(&others, origin, &key);
        if let Some(existing) = duplicate {
            if let Some(session) = self.sessions.get_mut(&origin) {
                session.duplicate_of = Some(existing);
            }
            ctx.emit(TmuxSessionEvent::Focus { origin: existing });
        }
        ctx.emit(TmuxSessionEvent::StatusChanged { origin });
        duplicate
    }

    /// Sends a detach to `origin`'s session.
    pub fn detach(&self, origin: TmuxOriginId, ctx: &AppContext) {
        TmuxGateways::as_ref(ctx).detach(origin);
    }

    /// Forgets a disconnected session whose tabs were closed.
    pub fn forget(&mut self, origin: TmuxOriginId) {
        if self
            .sessions
            .get(&origin)
            .is_some_and(|session| session.status == SessionStatus::Disconnected)
        {
            self.sessions.remove(&origin);
            self.reattach_after_end.remove(&origin);
        }
    }

    fn on_gateway_event(&mut self, event: &TmuxGatewayEvent, ctx: &mut ModelContext<Self>) {
        match event {
            TmuxGatewayEvent::Starting { origin } => {
                let session = self
                    .sessions
                    .entry(*origin)
                    .or_insert_with(SessionInfo::new);
                session.status = SessionStatus::Starting;
                session.end = None;
                session.duplicate_of = None;
            }
            TmuxGatewayEvent::Client { origin, event } => {
                self.on_client_event(*origin, event, ctx);
            }
            TmuxGatewayEvent::CommandCompleted { origin, tag, reply } => {
                match self.pending.remove(&(*origin, *tag)) {
                    Some(PendingReply::Registry(callback)) => callback(reply, self, ctx),
                    Some(PendingReply::App(callback)) => {
                        self.ready_replies.push((reply.clone(), callback));
                        ctx.emit(TmuxSessionEvent::RepliesReady);
                    }
                    None => {}
                }
            }
            TmuxGatewayEvent::Ended { origin } => self.on_ended(*origin, ctx),
            TmuxGatewayEvent::Refused { origin, reason } => {
                match self.sessions.get_mut(origin) {
                    // A refused handshake detaches; its end line gives the reason, not "detached".
                    Some(session) => session.end = Some(SessionEnd::Refused(reason.to_string())),
                    None => ctx.emit(TmuxSessionEvent::Notice {
                        origin: *origin,
                        line: refusal_line(reason),
                    }),
                }
            }
            TmuxGatewayEvent::PaneQueueReady { .. }
            | TmuxGatewayEvent::ConfirmPending { .. }
            | TmuxGatewayEvent::ConfirmCleared { .. } => {}
        }
    }

    fn on_client_event(
        &mut self,
        origin: TmuxOriginId,
        event: &ClientEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(session) = self.sessions.get_mut(&origin) else {
            return;
        };
        match event {
            ClientEvent::Attached {
                session_id,
                session_name,
            }
            | ClientEvent::SessionChanged {
                session_id,
                name: session_name,
            } => {
                session.session_id = Some(*session_id);
                session.name = sanitize_title(session_name);
                session.status = SessionStatus::Attached;
                session.end = None;
                ctx.emit(TmuxSessionEvent::Attached { origin });
            }
            ClientEvent::SessionRenamed { name } => {
                session.name = sanitize_title(name);
                ctx.emit(TmuxSessionEvent::StatusChanged { origin });
            }
            ClientEvent::WindowAdded { .. }
            | ClientEvent::WindowClosed { .. }
            | ClientEvent::WindowRenamed { .. }
            | ClientEvent::LayoutChanged { .. } => {
                ctx.emit(TmuxSessionEvent::WindowsChanged { origin });
            }
            ClientEvent::ActiveWindowChanged { window_id } => {
                ctx.emit(TmuxSessionEvent::ActiveWindowChanged {
                    origin,
                    window_id: *window_id,
                });
            }
            ClientEvent::ActivePaneChanged { window_id, pane_id } => {
                ctx.emit(TmuxSessionEvent::ActivePaneChanged {
                    origin,
                    window_id: *window_id,
                    pane_id: *pane_id,
                });
            }
            ClientEvent::NotResponding { .. } => {
                if session.status == SessionStatus::Attached {
                    session.status = SessionStatus::NotResponding;
                    ctx.emit(TmuxSessionEvent::StatusChanged { origin });
                }
            }
            ClientEvent::Responding => {
                if session.status == SessionStatus::NotResponding {
                    session.status = SessionStatus::Attached;
                    ctx.emit(TmuxSessionEvent::StatusChanged { origin });
                }
            }
            ClientEvent::Exited { .. } | ClientEvent::AttachFailed { .. }
                if matches!(session.end, Some(SessionEnd::Refused(_))) => {}
            ClientEvent::Exited { reason } => {
                session.end = Some(SessionEnd::Exited(reason.clone()));
            }
            ClientEvent::AttachFailed { error } => {
                session.end = Some(SessionEnd::AttachFailed(error.clone()));
            }
            ClientEvent::TransportLost | ClientEvent::Desync { .. } => {
                // Only an attached session has tabs to keep under a Disconnected banner.
                if session.status == SessionStatus::Starting {
                    session.end = Some(SessionEnd::Disconnected);
                } else {
                    session.status = SessionStatus::Disconnected;
                    ctx.emit(TmuxSessionEvent::StatusChanged { origin });
                }
            }
            ClientEvent::UnsupportedVersion { .. }
            | ClientEvent::KeyConfigLoaded(_)
            | ClientEvent::PaneSnapshot(_)
            | ClientEvent::PaneOutput { .. }
            | ClientEvent::PaneModeChanged { .. }
            | ClientEvent::Message { .. }
            | ClientEvent::ConfigError { .. }
            | ClientEvent::CommandCompleted { .. }
            | ClientEvent::ClientKeyAcknowledged
            | ClientEvent::ClientKeyDropped { .. } => {}
        }
    }

    fn on_ended(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        self.pending.retain(|(o, _), _| *o != origin);
        let Some(session) = self.sessions.get(&origin) else {
            return;
        };
        if session.status == SessionStatus::Disconnected {
            // The tabs stay under the Disconnected banner until Reattach or close.
            ctx.emit(TmuxSessionEvent::StatusChanged { origin });
            if self.reattach_after_end.remove(&origin) {
                ctx.emit(TmuxSessionEvent::ReattachReady { origin });
            }
            return;
        }
        let Some(session) = self.sessions.remove(&origin) else {
            return;
        };
        let line = if session.duplicate_of.is_some() {
            Some(format!("[{} is already open in another tab]", session.name))
        } else {
            session.end.as_ref().map(|end| end_line(&session.name, end))
        };
        ctx.emit(TmuxSessionEvent::Ended { origin, line });
    }
}

#[cfg(test)]
#[path = "workspace_sync_tests.rs"]
mod tests;

/// Runs replies for [`TmuxSessions::send_command_app`] once the registry's update has finished, so
/// a reply handler can send follow-up commands through the registry without a circular update.
pub struct TmuxReplyRunner;

impl Entity for TmuxReplyRunner {
    type Event = ();
}

impl SingletonEntity for TmuxReplyRunner {}

impl TmuxReplyRunner {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxSessions::handle(ctx), |_, sessions, event, ctx| {
            if matches!(event, TmuxSessionEvent::RepliesReady) {
                let ready = sessions.update(ctx, |sessions, _| sessions.take_ready_replies());
                for (reply, callback) in ready {
                    callback(&reply, ctx);
                }
            }
        });
        Self
    }
}
