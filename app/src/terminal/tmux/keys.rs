//! tmux key routing (U13, KTD5, KTD13).
//!
//! Inside a tmux tab, keys follow the attached server's bindings. tmux does the evaluation: a key
//! goes to `send-keys -K -c <client>` (tmux 3.4+) when it is the mirrored prefix or `prefix2`,
//! matches a root-table binding, or arrives inside an unfenced `-K` window. Every other key stays
//! with Warp, which sends it to the input editor at a Warp prompt and otherwise to the pane as
//! `send-keys -H`.
//!
//! Control mode never reports key-table changes, so after each `-K` key every key keeps going
//! through `-K` until that key is acknowledged and a fence (`display-message -p -c <client>
//! '#{client_key_table} #{pane_id}'`, sent after the acknowledgement so it runs after the key's
//! bound commands) reports the client back in `root`. That covers `bind -r` repeat,
//! `switch-client -T` tables, and a binding that moves focus under the next keystroke.
//!
//! [`KeyRouterCore`] is the pure state machine; [`TmuxKeyRouter`] connects it to the gateways,
//! settings and notices. The claim itself (which keys skip Warp's keymap) lives in
//! [`super::key_capture`].

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use instant::Instant;
use settings::Setting as _;

use warp_terminal::local_tty::tmux_gateway::TmuxRequest;
use warp_terminal::tmux::{
    ClientEvent, ClientKey, CommandReply, KeyBinding, KeyClass, KeyConfig, PaneId,
};
use warpui::r#async::Timer;
use warpui::keymap::{Keystroke, ReservedKeyBinding};
use warpui::{AppContext, Entity, ModelContext, SingletonEntity};

use super::TmuxOriginId;
use super::gateway::{TmuxGatewayEvent, TmuxGateways};
use super::key_tables::{
    self, Bound, IDENTITY_QUERY, KeyTables, LIST_CLIENTS_COMMAND, PREFIX_QUERY, ServerIdentity,
    TmuxKey,
};
use super::settings::TmuxSettings;
use crate::util::bindings::CustomAction;

/// How long a `-K` key may wait for its acknowledgement before the tab shows a spinner (R20).
pub const IN_FLIGHT_INDICATOR_DELAY: Duration = Duration::from_millis(150);

/// How long after a fence found the client in a non-root table to ask again. tmux's default
/// `repeat-time` is 500 ms.
pub const REFENCE_DELAY: Duration = Duration::from_millis(500);

/// How long the escape-hatch chord keeps the next key with Warp.
pub const ESCAPE_HATCH_WINDOW: Duration = Duration::from_millis(1500);

/// Where one keystroke in a tmux pane goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyRoute {
    /// Warp's keymap, then the input editor or the pane (`send-keys -H`).
    Warp,
    /// The focused pane is a popup (U14): the key skips Warp's keymap and goes to the popup's
    /// program as typed input.
    Popup,
    /// tmux's key tables (`send-keys -K`).
    Tmux(ClientKey),
}

impl KeyRoute {
    /// Whether the key skips Warp's keymap.
    pub fn is_claimed(&self) -> bool {
        !matches!(self, KeyRoute::Warp)
    }
}

/// What a reply is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandPurpose {
    Identity,
    Prefix,
    ListKeys,
    Fence,
    ListClients,
    ShowHooks,
    /// Nothing waits for the reply.
    Fire,
}

/// Something the core asks the router to send, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outbound {
    Key(ClientKey),
    Command {
        line: String,
        purpose: CommandPurpose,
    },
}

fn command(line: impl Into<String>, purpose: CommandPurpose) -> Outbound {
    Outbound::Command {
        line: line.into(),
        purpose,
    }
}

/// The KTD5 window: open from a `-K` key until a fence sent after every key was acknowledged
/// reports the client in the root table.
#[derive(Clone, Debug, Default)]
struct KeyWindow {
    open: bool,
    sent: u64,
    resolved: u64,
    /// The number of keys sent when the fence in flight was sent.
    fence: Option<u64>,
    /// Counts fences sent, so the watchdog can tell whether the one it watches was answered.
    fences_sent: u64,
    /// When the oldest unacknowledged key was sent.
    in_flight_since: Option<Instant>,
}

impl KeyWindow {
    fn key_sent(&mut self, now: Instant) {
        self.open = true;
        if self.sent == self.resolved {
            self.in_flight_since = Some(now);
        }
        self.sent += 1;
    }

    /// Returns whether to send a fence now.
    fn key_resolved(&mut self, now: Instant) -> bool {
        self.resolved = (self.resolved + 1).min(self.sent);
        if self.resolved < self.sent {
            self.in_flight_since = Some(now);
            return false;
        }
        self.in_flight_since = None;
        if self.fence.is_some() {
            // The fence in flight is stale; its answer sends another.
            return false;
        }
        self.fence = Some(self.sent);
        self.fences_sent += 1;
        true
    }

    /// Returns whether to send another fence now.
    fn fence_answered(&mut self, table: Option<&str>) -> bool {
        let Some(fenced_at) = self.fence.take() else {
            return false;
        };
        if fenced_at == self.sent {
            if table == Some("root") {
                self.open = false;
            }
            // Still in a non-root table: the next key goes through `-K` and fences again.
            return false;
        }
        // Keys were sent after this fence. Fence again once they are all acknowledged.
        if self.resolved == self.sent {
            self.fence = Some(self.sent);
            self.fences_sent += 1;
            return true;
        }
        false
    }

    /// Starts a fence now if every key is acknowledged and none is in flight.
    fn fence_now(&mut self) -> bool {
        if !self.open || self.fence.is_some() || self.resolved != self.sent {
            return false;
        }
        self.fence = Some(self.sent);
        self.fences_sent += 1;
        true
    }
}

#[derive(Clone, Debug, Default)]
struct Refresh {
    running: bool,
    again: bool,
    prefix: Option<(Option<String>, Option<String>)>,
    bindings: Option<Vec<KeyBinding>>,
}

#[derive(Clone, Debug, Default)]
struct HookScan {
    clients: Vec<String>,
    lines: Vec<String>,
    remaining: usize,
}

/// A key notice on the session's tabs (R19, R27, R29).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyNoticeKind {
    /// The server is older than tmux 3.4 (R27).
    OldTmux { host: String },
    /// tmux bindings take keys Warp also uses (R19).
    Shadowed { keys: Vec<TmuxKey> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyNotice {
    pub kind: KeyNoticeKind,
    /// "Show keys" was clicked.
    pub expanded: bool,
}

impl KeyNotice {
    /// The banner text. `escape_hatch` names the configurable reserved chord, if set.
    pub fn text(&self, escape_hatch: Option<&str>) -> String {
        match &self.kind {
            KeyNoticeKind::OldTmux { host } => {
                let host = if host.is_empty() { "this host" } else { host };
                format!(
                    "tmux key bindings need tmux 3.4 or newer on {host}. Until then, keys go to \
                     the pane as typed input."
                )
            }
            KeyNoticeKind::Shadowed { keys } => {
                let reserved = match escape_hatch {
                    Some(chord) => format!(
                        "Warp's reserved keys (command palette, quit, new window, settings, and \
                         {chord}) always stay with Warp."
                    ),
                    None => "Warp's reserved keys (command palette, quit, new window, settings) \
                             always stay with Warp."
                        .to_owned(),
                };
                let count = keys.len();
                let noun = if count == 1 { "key" } else { "keys" };
                if self.expanded {
                    let list = keys
                        .iter()
                        .map(TmuxKey::display)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("In tmux tabs, tmux handles {list}. {reserved}")
                } else {
                    format!(
                        "Your tmux bindings take {count} {noun} that Warp also uses in tmux tabs. \
                         {reserved}"
                    )
                }
            }
        }
    }
}

/// Key state for one session.
#[derive(Clone, Debug, Default)]
struct OriginKeys {
    /// `None` until the handshake reported the key configuration.
    supports_client_keys: Option<bool>,
    tables: Option<KeyTables>,
    identity: Option<ServerIdentity>,
    window: KeyWindow,
    /// The key table tmux is believed to be in for the next key while the window is open,
    /// from the keys sent and the last fence. `None` when unknown.
    table_guess: Option<String>,
    /// The bound command of the last `-K` key, for the watchdog (U14).
    last_bound: Option<String>,
    popup_pane: Option<PaneId>,
    /// Panes in a tmux mode (copy mode, choose mode), whose keys all go through tmux (U14).
    mode_panes: HashSet<PaneId>,
    /// A popup tmux opened where Warp cannot show it takes `-K` keys, so none are sent (U14).
    hidden_popup: bool,
    refresh: Refresh,
    hook_scan: Option<HookScan>,
    hooks_installed: bool,
    notice: Option<KeyNotice>,
    /// The config hash last compared with Warp's keymap.
    shadow_checked: Option<u64>,
}

/// The pure key-routing state for every session. See the module docs.
#[derive(Debug, Default)]
pub struct KeyRouterCore {
    origins: HashMap<TmuxOriginId, OriginKeys>,
    bypass_until: Option<Instant>,
}

impl KeyRouterCore {
    /// A session is starting on `origin`; any earlier state is dropped.
    pub fn starting(&mut self, origin: TmuxOriginId) {
        self.origins.insert(origin, OriginKeys::default());
    }

    pub fn ended(&mut self, origin: TmuxOriginId) {
        self.origins.remove(&origin);
    }

    /// The handshake's key configuration. Below tmux 3.4 `bindings` is `None` and keys are never
    /// routed through tmux (R27).
    pub fn key_config_loaded(&mut self, origin: TmuxOriginId, config: &KeyConfig) {
        let keys = self.origins.entry(origin).or_default();
        keys.supports_client_keys = Some(config.bindings.is_some());
        keys.tables = KeyTables::from_config(config);
    }

    /// The session attached: learn the client's name and the server's identity.
    pub fn attached(&mut self, origin: TmuxOriginId) -> Vec<Outbound> {
        let keys = self.origins.entry(origin).or_default();
        keys.window = KeyWindow::default();
        if keys.identity.is_some() {
            return Vec::new();
        }
        vec![command(IDENTITY_QUERY, CommandPurpose::Identity)]
    }

    pub fn supports_client_keys(&self, origin: TmuxOriginId) -> Option<bool> {
        self.origins.get(&origin)?.supports_client_keys
    }

    pub fn tables(&self, origin: TmuxOriginId) -> Option<&KeyTables> {
        self.origins.get(&origin)?.tables.as_ref()
    }

    pub fn identity(&self, origin: TmuxOriginId) -> Option<&ServerIdentity> {
        self.origins.get(&origin)?.identity.as_ref()
    }

    /// Re-reads the prefixes and bindings. Coalesces requests while one is running.
    pub fn refresh(&mut self, origin: TmuxOriginId) -> Vec<Outbound> {
        let Some(keys) = self.origins.get_mut(&origin) else {
            return Vec::new();
        };
        if keys.supports_client_keys != Some(true) {
            return Vec::new();
        }
        if keys.refresh.running {
            keys.refresh.again = true;
            return Vec::new();
        }
        keys.refresh = Refresh {
            running: true,
            ..Default::default()
        };
        vec![
            command(PREFIX_QUERY, CommandPurpose::Prefix),
            command(key_tables::list_keys_command(), CommandPurpose::ListKeys),
        ]
    }

    /// A `%message`. Warp's hooks show [`key_tables::HOOK_MARKER`] when bindings change.
    pub fn on_message(&mut self, origin: TmuxOriginId, text: &str) -> Vec<Outbound> {
        if text.trim() == key_tables::HOOK_MARKER {
            self.refresh(origin)
        } else {
            Vec::new()
        }
    }

    pub fn on_reply(
        &mut self,
        origin: TmuxOriginId,
        purpose: CommandPurpose,
        reply: &CommandReply,
    ) -> Vec<Outbound> {
        let Some(keys) = self.origins.get_mut(&origin) else {
            return Vec::new();
        };
        match purpose {
            CommandPurpose::Identity => {
                keys.identity = key_tables::parse_identity_reply(reply);
                if keys.supports_client_keys != Some(true)
                    || keys.identity.is_none()
                    || keys.hooks_installed
                {
                    return Vec::new();
                }
                let show = key_tables::show_hooks_commands();
                keys.hook_scan = Some(HookScan {
                    remaining: show.len() + 1,
                    ..Default::default()
                });
                std::iter::once(command(LIST_CLIENTS_COMMAND, CommandPurpose::ListClients))
                    .chain(
                        show.into_iter()
                            .map(|line| command(line, CommandPurpose::ShowHooks)),
                    )
                    .collect()
            }
            CommandPurpose::ListClients | CommandPurpose::ShowHooks => {
                let Some(scan) = keys.hook_scan.as_mut() else {
                    return Vec::new();
                };
                let lines = key_tables::parse_lines(reply);
                if purpose == CommandPurpose::ListClients {
                    scan.clients = lines;
                } else {
                    scan.lines.extend(lines);
                }
                scan.remaining = scan.remaining.saturating_sub(1);
                if scan.remaining > 0 {
                    return Vec::new();
                }
                let Some(scan) = keys.hook_scan.take() else {
                    return Vec::new();
                };
                let Some(identity) = keys.identity.as_ref() else {
                    return Vec::new();
                };
                keys.hooks_installed = true;
                key_tables::stale_hook_removals(&scan.lines, &scan.clients)
                    .into_iter()
                    .chain(key_tables::install_hook_commands(&identity.client_name))
                    .map(|line| command(line, CommandPurpose::Fire))
                    .collect()
            }
            CommandPurpose::Prefix | CommandPurpose::ListKeys => {
                if !keys.refresh.running {
                    return Vec::new();
                }
                if purpose == CommandPurpose::Prefix {
                    keys.refresh.prefix = Some(key_tables::parse_prefix_reply(reply));
                } else {
                    keys.refresh.bindings = Some(key_tables::parse_list_keys_reply(reply));
                }
                let (Some((prefix, prefix2)), Some(bindings)) =
                    (&keys.refresh.prefix, &keys.refresh.bindings)
                else {
                    return Vec::new();
                };
                keys.tables = Some(KeyTables::from_parts(
                    prefix.as_deref(),
                    prefix2.as_deref(),
                    bindings.clone(),
                ));
                let again = keys.refresh.again;
                keys.refresh = Refresh::default();
                if again {
                    self.refresh(origin)
                } else {
                    Vec::new()
                }
            }
            CommandPurpose::Fence => {
                let table = key_tables::parse_fence_reply(reply).map(|(table, _)| table);
                if keys.window.fence == Some(keys.window.sent) {
                    keys.table_guess = table.clone();
                }
                if keys.window.fence_answered(table.as_deref()) {
                    vec![Self::fence(keys)]
                } else {
                    Vec::new()
                }
            }
            CommandPurpose::Fire => Vec::new(),
        }
    }

    fn fence(keys: &OriginKeys) -> Outbound {
        let line = match &keys.identity {
            Some(identity) => key_tables::fence_command(&identity.client_name),
            // Without `-c`, tmux answers for the client that sent the command: this one.
            None => "display-message -p '#{client_key_table} #{pane_id}'".to_owned(),
        };
        command(line, CommandPurpose::Fence)
    }

    /// Where `keystroke`, pressed in tmux pane `pane_id`, goes. `take_precedence` is R18's
    /// setting: when off, only the prefix and the keys after it go to tmux.
    pub fn route(
        &self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        keystroke: &Keystroke,
        take_precedence: bool,
        now: Instant,
    ) -> KeyRoute {
        let Some(keys) = self.origins.get(&origin) else {
            return KeyRoute::Warp;
        };
        if keys.popup_pane == Some(pane_id) {
            return KeyRoute::Popup;
        }
        if self.bypass_until.is_some_and(|until| now < until) {
            return KeyRoute::Warp;
        }
        if keys.supports_client_keys != Some(true) || keys.hidden_popup {
            return KeyRoute::Warp;
        }
        let Some(key) = TmuxKey::from_keystroke(keystroke) else {
            return KeyRoute::Warp;
        };
        let client_key = |class, in_root_table| {
            KeyRoute::Tmux(ClientKey {
                key: key.name(),
                pane_id,
                class,
                in_root_table,
            })
        };
        if keys.window.open || keys.mode_panes.contains(&pane_id) {
            // tmux may be in a non-root table or a pane mode: it decides, bound or not.
            return client_key(KeyClass::Other, false);
        }
        let Some(tables) = &keys.tables else {
            return KeyRoute::Warp;
        };
        if tables.is_prefix(&key) {
            return client_key(KeyClass::Other, true);
        }
        if take_precedence && tables.root_binding(&key).is_some() {
            return client_key(tables.root_key_class(&key), true);
        }
        KeyRoute::Warp
    }

    /// Records a `-K` key handed to the gateway.
    pub fn key_sent(&mut self, origin: TmuxOriginId, now: Instant) {
        if let Some(keys) = self.origins.get_mut(&origin) {
            keys.window.key_sent(now);
        }
    }

    /// The command `key` is bound to in the table tmux is believed to be in: the root table
    /// outside the window, the guessed table inside it. `None` when unbound or unknown.
    pub fn bound_command(&self, origin: TmuxOriginId, key: &ClientKey) -> Option<&Bound> {
        let keys = self.origins.get(&origin)?;
        let tables = keys.tables.as_ref()?;
        let key = TmuxKey::parse(&key.key)?;
        let table = if keys.window.open {
            keys.table_guess.as_deref()?
        } else {
            "root"
        };
        tables.binding(table, &key)
    }

    /// Updates the table guess for a `-K` key about to be sent: the prefix enters `prefix`, a
    /// `switch-client -T` binding enters its table, a repeatable binding stays, and anything
    /// else returns to `root`.
    pub fn note_key(&mut self, origin: TmuxOriginId, key: &ClientKey) {
        let bound = self.bound_command(origin, key).cloned();
        let Some(keys) = self.origins.get_mut(&origin) else {
            return;
        };
        let current = if keys.window.open {
            keys.table_guess.clone()
        } else {
            Some("root".to_owned())
        };
        let parsed = TmuxKey::parse(&key.key);
        let is_prefix = parsed
            .as_ref()
            .zip(keys.tables.as_ref())
            .is_some_and(|(parsed, tables)| tables.is_prefix(parsed));
        keys.last_bound = bound.as_ref().map(|bound| bound.command.clone());
        keys.table_guess = match (current.as_deref(), bound) {
            (Some("root"), _) if is_prefix => Some("prefix".to_owned()),
            (_, Some(bound)) => match key_tables::switched_table(&bound.command) {
                Some(table) => Some(table),
                None if bound.repeat => current,
                None => Some("root".to_owned()),
            },
            (Some(_), None) => Some("root".to_owned()),
            (None, None) => None,
        };
    }

    /// The bound command of the last `-K` key sent.
    pub fn last_bound(&self, origin: TmuxOriginId) -> Option<&str> {
        self.origins.get(&origin)?.last_bound.as_deref()
    }

    /// Warp drew the UI a key's binding opens instead of sending the key (U14, KTD10). If tmux
    /// was in another table (the key followed the prefix), it is put back in `root`, and a
    /// fence closes the window.
    pub fn intercepted(&mut self, origin: TmuxOriginId) -> Vec<Outbound> {
        let Some(keys) = self.origins.get_mut(&origin) else {
            return Vec::new();
        };
        if !keys.window.open {
            return Vec::new();
        }
        keys.table_guess = Some("root".to_owned());
        let reset = match &keys.identity {
            Some(identity) => {
                super::native_ui::reset_key_table_command(Some(&identity.client_name))
            }
            None => super::native_ui::reset_key_table_command(None),
        };
        let mut out = vec![command(reset, CommandPurpose::Fire)];
        if keys.window.fence_now() {
            out.push(Self::fence(keys));
        }
        out
    }

    /// Records whether a pane is in a tmux mode.
    pub fn set_pane_mode(&mut self, origin: TmuxOriginId, pane_id: PaneId, in_mode: bool) {
        if let Some(keys) = self.origins.get_mut(&origin) {
            if in_mode {
                keys.mode_panes.insert(pane_id);
            } else {
                keys.mode_panes.remove(&pane_id);
            }
        }
    }

    /// Records whether a hidden popup is open on the session's client.
    pub fn set_hidden_popup(&mut self, origin: TmuxOriginId, hidden: bool) {
        if let Some(keys) = self.origins.get_mut(&origin) {
            keys.hidden_popup = hidden;
        }
    }

    /// The number of fences sent so far, and whether one is in flight.
    pub fn fence_state(&self, origin: TmuxOriginId) -> Option<(u64, bool)> {
        let keys = self.origins.get(&origin)?;
        Some((keys.window.fences_sent, keys.window.fence.is_some()))
    }

    /// tmux acknowledged a `-K` key, or the engine dropped a stale one.
    pub fn key_resolved(&mut self, origin: TmuxOriginId, now: Instant) -> Vec<Outbound> {
        let Some(keys) = self.origins.get_mut(&origin) else {
            return Vec::new();
        };
        if keys.window.key_resolved(now) {
            vec![Self::fence(keys)]
        } else {
            Vec::new()
        }
    }

    /// Whether the window is open with every key acknowledged and no fence in flight: the last
    /// fence found the client in a non-root table (prefix, repeat or `switch-client -T`). tmux
    /// leaves it on a timeout without telling Warp, so the router fences again later.
    pub fn awaiting_refence(&self, origin: TmuxOriginId) -> bool {
        self.origins.get(&origin).is_some_and(|keys| {
            keys.window.open
                && keys.window.fence.is_none()
                && keys.window.resolved == keys.window.sent
        })
    }

    /// Fences again if [`Self::awaiting_refence`] still holds.
    pub fn refence(&mut self, origin: TmuxOriginId) -> Vec<Outbound> {
        if !self.awaiting_refence(origin) {
            return Vec::new();
        }
        let Some(keys) = self.origins.get_mut(&origin) else {
            return Vec::new();
        };
        keys.window.fence = Some(keys.window.sent);
        keys.window.fences_sent += 1;
        vec![Self::fence(keys)]
    }

    /// Whether every key goes through `-K` right now.
    pub fn is_window_open(&self, origin: TmuxOriginId) -> bool {
        self.origins
            .get(&origin)
            .is_some_and(|keys| keys.window.open)
    }

    /// When the oldest unacknowledged `-K` key was sent.
    pub fn in_flight_since(&self, origin: TmuxOriginId) -> Option<Instant> {
        self.origins.get(&origin)?.window.in_flight_since
    }

    /// Marks `pane_id` as a popup pane (U14), or clears it.
    pub fn set_popup_pane(&mut self, origin: TmuxOriginId, pane_id: Option<PaneId>) {
        if let Some(keys) = self.origins.get_mut(&origin) {
            keys.popup_pane = pane_id;
        }
    }

    /// The escape-hatch chord: keys pressed before `until` stay with Warp.
    pub fn bypass_until(&mut self, until: Instant) {
        self.bypass_until = Some(until);
    }

    /// Commands that remove this client's hooks; send them before detaching.
    pub fn detach_commands(&self, origin: TmuxOriginId) -> Vec<String> {
        let Some(keys) = self.origins.get(&origin) else {
            return Vec::new();
        };
        match (&keys.identity, keys.hooks_installed) {
            (Some(identity), true) => key_tables::remove_hook_commands(&identity.client_name),
            _ => Vec::new(),
        }
    }

    pub fn notice(&self, origin: TmuxOriginId) -> Option<&KeyNotice> {
        self.origins.get(&origin)?.notice.as_ref()
    }

    pub fn dismiss_notice(&mut self, origin: TmuxOriginId) -> bool {
        self.origins
            .get_mut(&origin)
            .is_some_and(|keys| keys.notice.take().is_some())
    }

    pub fn expand_notice(&mut self, origin: TmuxOriginId) -> bool {
        match self
            .origins
            .get_mut(&origin)
            .and_then(|keys| keys.notice.as_mut())
        {
            Some(notice) if !notice.expanded => {
                notice.expanded = true;
                true
            }
            _ => false,
        }
    }

    /// Shows R27's notice when the server is older than tmux 3.4 and this server's notice is not
    /// in `shown`. Returns the memory key to persist.
    pub fn check_old_tmux_notice(
        &mut self,
        origin: TmuxOriginId,
        shown: &[String],
    ) -> Option<String> {
        let keys = self.origins.get_mut(&origin)?;
        if keys.supports_client_keys != Some(false) {
            return None;
        }
        let identity = keys.identity.as_ref()?;
        let memory = format!("old-tmux:{}", identity.server_key());
        if shown.contains(&memory) {
            return None;
        }
        keys.notice = Some(KeyNotice {
            kind: KeyNoticeKind::OldTmux {
                host: identity.host.clone(),
            },
            expanded: false,
        });
        Some(memory)
    }

    /// Shows R19's notice the first time this server's config shadows keys Warp uses
    /// (`warp_uses`), once per server and config hash. Returns the memory key to persist.
    pub fn check_shadow_notice(
        &mut self,
        origin: TmuxOriginId,
        shown: &[String],
        warp_uses: impl Fn(&Keystroke) -> bool,
    ) -> Option<String> {
        let keys = self.origins.get_mut(&origin)?;
        let tables = keys.tables.as_ref()?;
        let identity = keys.identity.as_ref()?;
        let hash = tables.config_hash();
        if keys.shadow_checked == Some(hash) {
            return None;
        }
        keys.shadow_checked = Some(hash);
        let memory = format!("shadowed:{}:{hash:016x}", identity.server_key());
        if shown.contains(&memory) {
            return None;
        }
        let shadowed = tables.shadowed_keys(warp_uses);
        if shadowed.is_empty() {
            return None;
        }
        keys.notice = Some(KeyNotice {
            kind: KeyNoticeKind::Shadowed { keys: shadowed },
            expanded: false,
        });
        Some(memory)
    }

    /// Whether the shadow check for the current config is still to do.
    pub fn needs_shadow_check(&self, origin: TmuxOriginId) -> bool {
        let Some(keys) = self.origins.get(&origin) else {
            return false;
        };
        match (&keys.tables, &keys.identity) {
            (Some(tables), Some(_)) => keys.shadow_checked != Some(tables.config_hash()),
            _ => false,
        }
    }
}

/// Warp actions a tmux pane can never claim (R18, KTD6), plus the escape-hatch chord.
pub fn reserved_key_bindings(escape_hatch: Option<Keystroke>) -> Vec<ReservedKeyBinding> {
    let mut reserved: Vec<ReservedKeyBinding> = [
        "workspace:toggle_command_palette",
        "workspace:quit_warp",
        "workspace:new_window",
        "workspace:show_settings",
        // Paste never goes through tmux's key tables.
        "terminal:paste",
        "terminal:alternate_terminal_paste",
    ]
    .into_iter()
    .map(|name| ReservedKeyBinding::Named(name.to_owned()))
    .collect();
    reserved.extend(
        [
            CustomAction::CommandPalette,
            CustomAction::ShowSettings,
            CustomAction::Paste,
        ]
        .into_iter()
        .map(|action| ReservedKeyBinding::CustomAction(action.into())),
    );
    reserved.extend(escape_hatch.map(ReservedKeyBinding::Keystroke));
    reserved
}

/// The escape-hatch chord from settings, if it parses.
pub fn escape_hatch_keystroke(app: &AppContext) -> Option<Keystroke> {
    let source = TmuxSettings::as_ref(app)
        .escape_hatch_key
        .value()
        .trim()
        .to_owned();
    if source.is_empty() {
        return None;
    }
    Keystroke::parse(&source).ok()
}

/// Notifications from [`TmuxKeyRouter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxKeyRouterEvent {
    /// The session's key notice appeared, changed or went away.
    NoticeChanged { origin: TmuxOriginId },
    /// A `-K` key started or stopped showing the in-flight spinner (R20).
    InFlightChanged { origin: TmuxOriginId },
    /// A fence went unanswered for [`super::native_ui::WATCHDOG_DELAY`]: the last key's
    /// commands keep tmux's command queue busy, e.g. tmux UI opened on Warp's client (U14).
    Stalled {
        origin: TmuxOriginId,
        /// The last `-K` key's bound command.
        command: Option<String>,
    },
}

/// Commands the router sent and waits on, by gateway tag.
const FIRST_TAG: u64 = 1 << 62;

/// Routes keys for every tmux session. See the module docs.
pub struct TmuxKeyRouter {
    core: KeyRouterCore,
    pending: HashMap<(TmuxOriginId, u64), CommandPurpose>,
    next_tag: u64,
    /// Sessions whose tabs show the in-flight spinner.
    spinning: HashSet<TmuxOriginId>,
    /// Sessions with a spinner timer scheduled.
    timers: HashSet<TmuxOriginId>,
    /// Commands other features send before a detach, merged with the router's own (U14).
    extra_detach: HashMap<TmuxOriginId, Vec<String>>,
}

impl Entity for TmuxKeyRouter {
    type Event = TmuxKeyRouterEvent;
}

impl SingletonEntity for TmuxKeyRouter {}

impl TmuxKeyRouter {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        ctx.subscribe_to_model(&TmuxGateways::handle(ctx), |me, _, event, ctx| {
            me.on_gateway_event(event, ctx);
        });
        ctx.subscribe_to_model(&TmuxSettings::handle(ctx), |_, _, _, ctx| {
            let reserved = reserved_key_bindings(escape_hatch_keystroke(ctx));
            ctx.set_reserved_key_bindings(reserved);
        });
        let reserved = reserved_key_bindings(escape_hatch_keystroke(ctx));
        ctx.set_reserved_key_bindings(reserved);
        Self {
            core: KeyRouterCore::default(),
            pending: HashMap::new(),
            next_tag: FIRST_TAG,
            spinning: HashSet::new(),
            timers: HashSet::new(),
            extra_detach: HashMap::new(),
        }
    }

    pub fn core(&self) -> &KeyRouterCore {
        &self.core
    }

    /// Where a keystroke in `pane_id` goes, under the current settings.
    pub fn route(
        &self,
        origin: TmuxOriginId,
        pane_id: PaneId,
        keystroke: &Keystroke,
        app: &AppContext,
    ) -> KeyRoute {
        let settings = TmuxSettings::as_ref(app);
        if !settings.control_mode_enabled() {
            return KeyRoute::Warp;
        }
        self.core.route(
            origin,
            pane_id,
            keystroke,
            settings.bindings_take_precedence_enabled(),
            Instant::now(),
        )
    }

    /// Sends a key through tmux's key tables, in order behind earlier keys and input.
    pub fn send_client_key(
        &mut self,
        origin: TmuxOriginId,
        key: ClientKey,
        ctx: &mut ModelContext<Self>,
    ) {
        self.execute(origin, vec![Outbound::Key(key)], ctx);
    }

    /// U14: marks `pane_id` as the session's focused popup pane (or clears it with `None`).
    /// While it has focus, every key except Warp's reserved actions goes to it as typed input.
    pub fn set_popup_pane(&mut self, origin: TmuxOriginId, pane_id: Option<PaneId>) {
        self.core.set_popup_pane(origin, pane_id);
    }

    /// The command a routed key is bound to (U14).
    pub fn bound_command(&self, origin: TmuxOriginId, key: &ClientKey) -> Option<String> {
        self.core
            .bound_command(origin, key)
            .map(|bound| bound.command.clone())
    }

    /// Warp drew a key's UI natively instead of sending the key (U14).
    pub fn key_intercepted(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let out = self.core.intercepted(origin);
        self.execute(origin, out, ctx);
    }

    /// A pane entered or left a tmux mode (U14).
    pub fn set_pane_mode(&mut self, origin: TmuxOriginId, pane_id: PaneId, in_mode: bool) {
        self.core.set_pane_mode(origin, pane_id, in_mode);
    }

    /// A hidden popup opened or closed on the session's client (U14).
    pub fn set_hidden_popup(&mut self, origin: TmuxOriginId, hidden: bool) {
        self.core.set_hidden_popup(origin, hidden);
    }

    /// Commands another feature needs sent before `origin` detaches, e.g. restoring the
    /// server's PATH (U14).
    pub fn set_extra_detach_commands(
        &mut self,
        origin: TmuxOriginId,
        commands: Vec<String>,
        ctx: &mut ModelContext<Self>,
    ) {
        self.extra_detach.insert(origin, commands);
        self.sync_detach_commands(origin, ctx);
    }

    fn sync_detach_commands(&self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let mut detach_commands = self.core.detach_commands(origin);
        if let Some(extra) = self.extra_detach.get(&origin) {
            detach_commands.extend(extra.iter().cloned());
        }
        TmuxGateways::handle(ctx).update(ctx, |gateways, _| {
            gateways.set_detach_commands(origin, detach_commands);
        });
    }

    /// The escape-hatch chord was pressed: the next key stays with Warp.
    pub fn escape_hatch_pressed(&mut self) {
        self.core.bypass_until(Instant::now() + ESCAPE_HATCH_WINDOW);
    }

    /// Re-reads the session's bindings, e.g. after Warp sent a `source-file` (R17).
    pub fn refresh(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let out = self.core.refresh(origin);
        self.execute(origin, out, ctx);
    }

    pub fn notice(&self, origin: TmuxOriginId) -> Option<&KeyNotice> {
        self.core.notice(origin)
    }

    pub fn notice_text(&self, origin: TmuxOriginId, app: &AppContext) -> Option<String> {
        let escape = escape_hatch_keystroke(app).map(|stroke| stroke.displayed());
        Some(self.core.notice(origin)?.text(escape.as_deref()))
    }

    pub fn dismiss_notice(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        if self.core.dismiss_notice(origin) {
            ctx.emit(TmuxKeyRouterEvent::NoticeChanged { origin });
        }
    }

    pub fn expand_notice(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        if self.core.expand_notice(origin) {
            ctx.emit(TmuxKeyRouterEvent::NoticeChanged { origin });
        }
    }

    /// Whether the session's tabs show the in-flight spinner (R20).
    pub fn is_key_in_flight(&self, origin: TmuxOriginId) -> bool {
        self.spinning.contains(&origin)
    }

    /// Compares the session's bindings with the keys Warp uses (`warp_uses`) and shows R19's
    /// notice once per server and config.
    pub fn check_shadow_notice(
        &mut self,
        origin: TmuxOriginId,
        warp_uses: impl Fn(&Keystroke) -> bool,
        ctx: &mut ModelContext<Self>,
    ) {
        if !self.core.needs_shadow_check(origin)
            || !TmuxSettings::as_ref(ctx).bindings_take_precedence_enabled()
        {
            return;
        }
        let shown = TmuxSettings::as_ref(ctx).key_notices_shown.value().clone();
        if let Some(memory) = self.core.check_shadow_notice(origin, &shown, warp_uses) {
            remember_notice(memory, ctx);
            ctx.emit(TmuxKeyRouterEvent::NoticeChanged { origin });
        }
    }

    fn on_gateway_event(&mut self, event: &TmuxGatewayEvent, ctx: &mut ModelContext<Self>) {
        match event {
            TmuxGatewayEvent::Starting { origin } => self.core.starting(*origin),
            TmuxGatewayEvent::Client { origin, event } => self.on_client_event(*origin, event, ctx),
            TmuxGatewayEvent::CommandCompleted { origin, tag, reply } => {
                if let Some(purpose) = self.pending.remove(&(*origin, *tag)) {
                    let out = self.core.on_reply(*origin, purpose, reply);
                    self.execute(*origin, out, ctx);
                    if purpose == CommandPurpose::Fence && self.core.awaiting_refence(*origin) {
                        let origin = *origin;
                        let _ = ctx.spawn(Timer::after(REFENCE_DELAY), move |me, _, ctx| {
                            let out = me.core.refence(origin);
                            me.execute(origin, out, ctx);
                        });
                    }
                    if purpose == CommandPurpose::Identity {
                        self.check_old_tmux_notice(*origin, ctx);
                    }
                }
            }
            TmuxGatewayEvent::Ended { origin } => self.end(*origin, ctx),
            TmuxGatewayEvent::Refused { .. } | TmuxGatewayEvent::PaneQueueReady { .. } => {}
        }
    }

    fn on_client_event(
        &mut self,
        origin: TmuxOriginId,
        event: &ClientEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let out = match event {
            ClientEvent::KeyConfigLoaded(config) => {
                self.core.key_config_loaded(origin, config);
                Vec::new()
            }
            ClientEvent::PaneModeChanged { pane_id, mode } => {
                self.core.set_pane_mode(origin, *pane_id, mode.is_some());
                Vec::new()
            }
            ClientEvent::Attached { .. } => self.core.attached(origin),
            ClientEvent::Message { text } => self.core.on_message(origin, text),
            ClientEvent::ConfigError { .. } => self.core.refresh(origin),
            ClientEvent::ClientKeyAcknowledged | ClientEvent::ClientKeyDropped { .. } => {
                let out = self.core.key_resolved(origin, Instant::now());
                self.update_spinner(origin, ctx);
                out
            }
            ClientEvent::Exited { .. }
            | ClientEvent::TransportLost
            | ClientEvent::Desync { .. }
            | ClientEvent::AttachFailed { .. } => {
                self.end(origin, ctx);
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.execute(origin, out, ctx);
    }

    fn end(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let had_notice = self.core.notice(origin).is_some();
        self.core.ended(origin);
        self.extra_detach.remove(&origin);
        self.pending.retain(|(o, _), _| *o != origin);
        if self.spinning.remove(&origin) {
            ctx.emit(TmuxKeyRouterEvent::InFlightChanged { origin });
        }
        if had_notice {
            ctx.emit(TmuxKeyRouterEvent::NoticeChanged { origin });
        }
    }

    fn check_old_tmux_notice(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let shown = TmuxSettings::as_ref(ctx).key_notices_shown.value().clone();
        if let Some(memory) = self.core.check_old_tmux_notice(origin, &shown) {
            remember_notice(memory, ctx);
            ctx.emit(TmuxKeyRouterEvent::NoticeChanged { origin });
        }
    }

    /// Sends `out` in order on `origin`'s gateway.
    fn execute(&mut self, origin: TmuxOriginId, out: Vec<Outbound>, ctx: &mut ModelContext<Self>) {
        if out.is_empty() {
            return;
        }
        let Some(sender) = TmuxGateways::as_ref(ctx).request_sender(origin) else {
            return;
        };
        for outbound in out {
            match outbound {
                Outbound::Key(key) => {
                    self.core.note_key(origin, &key);
                    if sender.send(TmuxRequest::ClientKey(key)) {
                        self.core.key_sent(origin, Instant::now());
                        self.schedule_spinner(origin, IN_FLIGHT_INDICATOR_DELAY, ctx);
                    }
                }
                Outbound::Command { line, purpose } => {
                    let tag = self.next_tag;
                    self.next_tag = self.next_tag.wrapping_add(1).max(FIRST_TAG);
                    if sender.send(TmuxRequest::Command { tag, line }) {
                        self.pending.insert((origin, tag), purpose);
                        if purpose == CommandPurpose::Fence {
                            self.watch_fence(origin, ctx);
                        }
                    }
                }
            }
        }
        self.sync_detach_commands(origin, ctx);
    }

    /// Emits [`TmuxKeyRouterEvent::Stalled`] if the fence just sent is still unanswered after
    /// the watchdog delay (U14).
    fn watch_fence(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let Some((sent, _)) = self.core.fence_state(origin) else {
            return;
        };
        let _ = ctx.spawn(
            Timer::after(super::native_ui::WATCHDOG_DELAY),
            move |me, _, ctx| {
                if me.core.fence_state(origin) == Some((sent, true)) {
                    let command = me.core.last_bound(origin).map(str::to_owned);
                    ctx.emit(TmuxKeyRouterEvent::Stalled { origin, command });
                }
            },
        );
    }

    fn schedule_spinner(
        &mut self,
        origin: TmuxOriginId,
        after: Duration,
        ctx: &mut ModelContext<Self>,
    ) {
        if !self.timers.insert(origin) {
            return;
        }
        let _ = ctx.spawn(Timer::after(after), move |me, _, ctx| {
            me.timers.remove(&origin);
            me.update_spinner(origin, ctx);
        });
    }

    fn update_spinner(&mut self, origin: TmuxOriginId, ctx: &mut ModelContext<Self>) {
        let waited = self
            .core
            .in_flight_since(origin)
            .map(|since| since.elapsed());
        let spin = waited.is_some_and(|waited| waited >= IN_FLIGHT_INDICATOR_DELAY);
        if let Some(waited) = waited
            && !spin
        {
            self.schedule_spinner(origin, IN_FLIGHT_INDICATOR_DELAY - waited, ctx);
        }
        let changed = if spin {
            self.spinning.insert(origin)
        } else {
            self.spinning.remove(&origin)
        };
        if changed {
            ctx.emit(TmuxKeyRouterEvent::InFlightChanged { origin });
        }
    }
}

/// Persists that a "once" notice was shown (R29).
fn remember_notice(memory: String, ctx: &mut ModelContext<TmuxKeyRouter>) {
    TmuxSettings::handle(ctx).update(ctx, |settings, ctx| {
        let mut shown = settings.key_notices_shown.value().clone();
        if !shown.contains(&memory) {
            shown.push(memory);
            if let Err(error) = settings.key_notices_shown.set_value(shown, ctx) {
                log::warn!("tmux: could not remember a key notice: {error}");
            }
        }
    });
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
