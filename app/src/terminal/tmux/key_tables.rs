//! A mirror of the attached tmux server's key tables (U13, KTD5).
//!
//! Warp never evaluates tmux bindings itself: it only needs to know which keys tmux would act on,
//! so it can hand those to tmux (`send-keys -K`) and leave the rest to Warp. The mirror holds the
//! prefix keys and every binding from `list-keys -F`, keyed by a canonical [`TmuxKey`].
//!
//! This module also builds the command lines the key router sends to keep the mirror fresh: the
//! prefix query, `list-keys`, and the Warp-tagged hooks that tell Warp when bindings change
//! (KTD13). Every builder returns exactly one command line, because the control client expects
//! one reply block per line (`a ; b` on one line would answer twice).

use std::collections::{BTreeSet, HashMap};

use warp_terminal::tmux::{
    CommandReply, KeyBinding, KeyClass, KeyConfig, LIST_KEYS_FORMAT, PaneId,
    parse_key_binding_line, quote_argument,
};
use warpui::keymap::Keystroke;

/// A key in tmux's naming (`C-b`, `M-Up`, `F5`), reduced to a canonical form so names that tmux
/// treats as the same key compare equal.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TmuxKey {
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
    /// The key without modifiers: a single character or a tmux key name such as `Up` or `BSpace`.
    pub base: String,
}

/// Warp key names and the tmux names they map to. tmux has no names past F12.
const NAMED_KEYS: &[(&str, &str)] = &[
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "PPage"),
    ("pagedown", "NPage"),
    ("insert", "IC"),
    ("delete", "DC"),
    ("backspace", "BSpace"),
    ("enter", "Enter"),
    ("numpadenter", "Enter"),
    ("escape", "Escape"),
    ("tab", "Tab"),
    ("f1", "F1"),
    ("f2", "F2"),
    ("f3", "F3"),
    ("f4", "F4"),
    ("f5", "F5"),
    ("f6", "F6"),
    ("f7", "F7"),
    ("f8", "F8"),
    ("f9", "F9"),
    ("f10", "F10"),
    ("f11", "F11"),
    ("f12", "F12"),
];

/// tmux's alternative spellings, folded onto the names `list-keys` prints.
const TMUX_ALIASES: &[(&str, &str)] = &[
    ("pageup", "PPage"),
    ("pgup", "PPage"),
    ("ppage", "PPage"),
    ("pagedown", "NPage"),
    ("pgdn", "NPage"),
    ("npage", "NPage"),
    ("insert", "IC"),
    ("ic", "IC"),
    ("delete", "DC"),
    ("dc", "DC"),
    ("bspace", "BSpace"),
    ("enter", "Enter"),
    ("escape", "Escape"),
    ("tab", "Tab"),
    ("btab", "BTab"),
    ("space", "Space"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
];

impl TmuxKey {
    /// Parses a tmux key name as `list-keys` or a config file writes it. Returns `None` for names
    /// that cannot come from a keyboard (mouse events, `Any`, user keys).
    pub fn parse(name: &str) -> Option<Self> {
        let mut rest = name;
        let (mut ctrl, mut meta, mut shift, mut caret) = (false, false, false, false);
        loop {
            let bytes = rest.as_bytes();
            if bytes.len() > 2 && bytes[1] == b'-' {
                match bytes[0] {
                    b'C' | b'c' => ctrl = true,
                    b'M' | b'm' => meta = true,
                    b'S' | b's' => shift = true,
                    _ => break,
                }
                rest = &rest[2..];
            } else if bytes.len() == 2 && bytes[0] == b'^' {
                // `^B` is `C-b`, not `C-S-b`.
                ctrl = true;
                caret = true;
                rest = &rest[1..];
            } else {
                break;
            }
        }
        // `list-keys` escapes keys that tmux's parser treats specially (`\;`, `\#`).
        if rest.len() == 2 && rest.starts_with('\\') {
            rest = &rest[1..];
        }
        if rest.is_empty() {
            return None;
        }
        let base = if rest.chars().count() == 1 {
            if caret {
                rest.to_ascii_lowercase()
            } else {
                rest.to_owned()
            }
        } else {
            let lower = rest.to_ascii_lowercase();
            if let Some((_, canonical)) = TMUX_ALIASES.iter().find(|(alias, _)| *alias == lower) {
                (*canonical).to_owned()
            } else if let Some(number) = lower.strip_prefix('f')
                && number.parse::<u8>().is_ok_and(|n| (1..=12).contains(&n))
            {
                format!("F{number}")
            } else {
                return None;
            }
        };
        Some(Self::canonical(ctrl, meta, shift, base))
    }

    /// The tmux key for a Warp keystroke. `None` for keys tmux cannot receive from a terminal:
    /// anything with cmd, and keys tmux has no name for.
    pub fn from_keystroke(keystroke: &Keystroke) -> Option<Self> {
        if keystroke.cmd {
            return None;
        }
        let meta = keystroke.alt || keystroke.meta;
        let key = keystroke.key.as_str();
        let base = if key == " " {
            "Space".to_owned()
        } else if let Some((_, name)) = NAMED_KEYS.iter().find(|(warp, _)| *warp == key) {
            (*name).to_owned()
        } else if key.chars().count() == 1 {
            key.to_owned()
        } else {
            return None;
        };
        Some(Self::canonical(keystroke.ctrl, meta, keystroke.shift, base))
    }

    /// Folds equivalent spellings together: shifted printable characters carry the shift in the
    /// character itself, `S-Tab` is `BTab`, and `C-H` is `C-h`.
    fn canonical(ctrl: bool, meta: bool, mut shift: bool, mut base: String) -> Self {
        let mut chars = base.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_alphabetic() {
                if ctrl {
                    // tmux names ctrl-letters in lower case; an upper-case letter means shift.
                    shift |= c.is_ascii_uppercase();
                    base = c.to_ascii_lowercase().to_string();
                } else if shift {
                    base = c.to_ascii_uppercase().to_string();
                    shift = false;
                } else if c.is_ascii_uppercase() {
                    // Already shifted.
                }
            } else {
                // A printable character already carries the shift (`%`, not `S-5`).
                shift = false;
            }
        }
        if base == "Tab" && shift {
            base = "BTab".to_owned();
            shift = false;
        }
        Self {
            ctrl,
            meta,
            shift,
            base,
        }
    }

    /// The name to send with `send-keys -K`.
    pub fn name(&self) -> String {
        let mut name = String::new();
        if self.ctrl {
            name.push_str("C-");
        }
        if self.meta {
            name.push_str("M-");
        }
        if self.shift {
            name.push_str("S-");
        }
        name.push_str(&self.base);
        name
    }

    /// The Warp keystroke that produces this key, used to compare tmux bindings with Warp's.
    pub fn to_keystroke(&self) -> Option<Keystroke> {
        let (key, shift) = match self.base.as_str() {
            "Space" => (" ".to_owned(), self.shift),
            "BTab" => ("tab".to_owned(), true),
            base => {
                if let Some((warp, _)) = NAMED_KEYS.iter().find(|(_, name)| *name == base) {
                    ((*warp).to_owned(), self.shift)
                } else if base.chars().count() == 1 {
                    let c = base.chars().next()?;
                    if c.is_ascii_alphabetic() && (c.is_ascii_uppercase() || self.shift) {
                        (c.to_ascii_uppercase().to_string(), true)
                    } else {
                        (base.to_owned(), false)
                    }
                } else {
                    return None;
                }
            }
        };
        Some(Keystroke {
            ctrl: self.ctrl,
            alt: self.meta,
            shift,
            cmd: false,
            meta: false,
            key,
        })
    }

    /// A user-facing name such as `ctrl+k`.
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("ctrl".to_owned());
        }
        if self.meta {
            parts.push("alt".to_owned());
        }
        if self.shift {
            parts.push("shift".to_owned());
        }
        let base = match self.base.as_str() {
            "BTab" => "shift+tab".to_owned(),
            "PPage" => "pageup".to_owned(),
            "NPage" => "pagedown".to_owned(),
            "IC" => "insert".to_owned(),
            "DC" => "delete".to_owned(),
            "BSpace" => "backspace".to_owned(),
            base if base.chars().count() == 1 => base.to_owned(),
            base => base.to_ascii_lowercase(),
        };
        parts.push(base);
        parts.join("+")
    }
}

/// One binding in the mirror.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub command: String,
    pub repeat: bool,
}

/// The attached server's prefix keys and key tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyTables {
    prefix: Option<TmuxKey>,
    prefix2: Option<TmuxKey>,
    tables: HashMap<String, HashMap<TmuxKey, Bound>>,
    /// The raw prefix names and binding rows, for the config hash.
    raw: Vec<String>,
}

impl KeyTables {
    /// The mirror from the handshake's key configuration. `None` below tmux 3.4, which cannot
    /// list keys with formats and cannot evaluate keys for a control client.
    pub fn from_config(config: &KeyConfig) -> Option<Self> {
        let bindings = config.bindings.clone()?;
        Some(Self::from_parts(
            config.prefix.as_deref(),
            config.prefix2.as_deref(),
            bindings,
        ))
    }

    pub fn from_parts(
        prefix: Option<&str>,
        prefix2: Option<&str>,
        bindings: Vec<KeyBinding>,
    ) -> Self {
        let mut raw = vec![
            format!("prefix\t{}", prefix.unwrap_or("None")),
            format!("prefix2\t{}", prefix2.unwrap_or("None")),
        ];
        let mut tables: HashMap<String, HashMap<TmuxKey, Bound>> = HashMap::new();
        for binding in bindings {
            raw.push(format!(
                "{}\t{}\t{}\t{}",
                binding.table, binding.repeat, binding.key, binding.command
            ));
            let Some(key) = TmuxKey::parse(&binding.key) else {
                continue;
            };
            tables.entry(binding.table).or_default().insert(
                key,
                Bound {
                    command: binding.command,
                    repeat: binding.repeat,
                },
            );
        }
        raw.sort();
        Self {
            prefix: prefix.and_then(TmuxKey::parse),
            prefix2: prefix2.and_then(TmuxKey::parse),
            tables,
            raw,
        }
    }

    pub fn prefix(&self) -> Option<&TmuxKey> {
        self.prefix.as_ref()
    }

    /// Whether `key` is the mirrored `prefix` or `prefix2`.
    pub fn is_prefix(&self, key: &TmuxKey) -> bool {
        self.prefix.as_ref() == Some(key) || self.prefix2.as_ref() == Some(key)
    }

    pub fn binding(&self, table: &str, key: &TmuxKey) -> Option<&Bound> {
        self.tables.get(table)?.get(key)
    }

    pub fn root_binding(&self, key: &TmuxKey) -> Option<&Bound> {
        self.binding("root", key)
    }

    /// How a root-table key may be treated on a slow channel (KTD13): navigation keys may be
    /// dropped when stale.
    pub fn root_key_class(&self, key: &TmuxKey) -> KeyClass {
        match self.root_binding(key) {
            Some(bound) if is_navigation_command(&bound.command) => KeyClass::Navigation,
            _ => KeyClass::Other,
        }
    }

    /// A stable hash of the prefixes and every binding, for "once per config" notices (R19).
    pub fn config_hash(&self) -> u64 {
        fnv1a(self.raw.join("\n").as_bytes())
    }

    /// Keys that tmux acts on in the root table, including the prefixes, and that Warp also
    /// uses (`warp_uses`). These are the keys a user loses to tmux (R19). Sorted for display.
    pub fn shadowed_keys(&self, warp_uses: impl Fn(&Keystroke) -> bool) -> Vec<TmuxKey> {
        let mut keys: BTreeSet<&TmuxKey> = BTreeSet::new();
        keys.extend(self.prefix.iter());
        keys.extend(self.prefix2.iter());
        if let Some(root) = self.tables.get("root") {
            keys.extend(root.keys());
        }
        keys.into_iter()
            .filter(|key| key.to_keystroke().is_some_and(|stroke| warp_uses(&stroke)))
            .cloned()
            .collect()
    }
}

/// Whether a bound command moves between panes or windows (KTD13), including an `if-shell`
/// such as vim-tmux-navigator's that wraps a `select-pane`.
pub fn is_navigation_command(command: &str) -> bool {
    const NAVIGATION: &[&str] = &[
        "select-pane",
        "selectp",
        "select-window",
        "selectw",
        "next-window",
        "next",
        "previous-window",
        "prev",
        "last-window",
        "last",
        "last-pane",
        "lastp",
    ];
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if NAVIGATION.contains(&first) {
        return true;
    }
    if first == "if-shell" || first == "if" {
        return command
            .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '{' | '}' | ';'))
            .any(|word| NAVIGATION[..4].contains(&word));
    }
    false
}

/// 64-bit FNV-1a. Stable across runs and Rust versions, unlike `DefaultHasher`, because the hash
/// is persisted.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn reply_lines(reply: &CommandReply) -> Vec<String> {
    reply
        .lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect()
}

fn reply_text(reply: &CommandReply) -> String {
    reply_lines(reply).join("\n").trim().to_owned()
}

// Mirror refresh.

/// Reads `prefix` and `prefix2` in one reply.
pub const PREFIX_QUERY: &str = "display-message -p '#{prefix}\t#{prefix2}'";

pub fn list_keys_command() -> String {
    format!("list-keys -F '{LIST_KEYS_FORMAT}'")
}

/// The prefixes from a [`PREFIX_QUERY`] reply. tmux prints `None` for an unset key.
pub fn parse_prefix_reply(reply: &CommandReply) -> (Option<String>, Option<String>) {
    if reply.error {
        return (None, None);
    }
    let text = reply_text(reply);
    let mut fields = text.split('\t').map(|field| {
        let field = field.trim();
        (!field.is_empty() && field != "None").then(|| field.to_owned())
    });
    (fields.next().flatten(), fields.next().flatten())
}

pub fn parse_list_keys_reply(reply: &CommandReply) -> Vec<KeyBinding> {
    if reply.error {
        return Vec::new();
    }
    reply
        .lines
        .iter()
        .filter_map(|line| parse_key_binding_line(line))
        .collect()
}

/// Whether Warp is sending a `source-file`, after which the mirror refreshes (R17).
pub fn is_source_file_command(line: &str) -> bool {
    matches!(
        line.split_whitespace().next(),
        Some("source-file" | "source")
    )
}

// The server and client the mirror belongs to.

/// Reads the control client's name and the server's identity in one reply.
pub const IDENTITY_QUERY: &str = "display-message -p '#{client_name}\t#{host}\t#{socket_path}'";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerIdentity {
    pub client_name: String,
    pub host: String,
    pub socket_path: String,
}

impl ServerIdentity {
    /// Identifies the server across restarts of Warp, for "once per server" notices (R29).
    pub fn server_key(&self) -> String {
        format!("{}:{}", self.host, self.socket_path)
    }
}

pub fn parse_identity_reply(reply: &CommandReply) -> Option<ServerIdentity> {
    if reply.error {
        return None;
    }
    let text = reply_text(reply);
    let mut fields = text.split('\t');
    let client_name = fields.next()?.trim().to_owned();
    if client_name.is_empty() {
        return None;
    }
    Some(ServerIdentity {
        client_name,
        host: fields.next().unwrap_or_default().trim().to_owned(),
        socket_path: fields.next().unwrap_or_default().trim().to_owned(),
    })
}

// The KTD5 fence.

/// Asks which key table the client is in, and its active pane. Sent after a `-K` key is
/// acknowledged, it runs after that key's bound commands.
pub fn fence_command(client_name: &str) -> String {
    format!(
        "display-message -p -c {} '#{{client_key_table}} #{{pane_id}}'",
        quote_argument(client_name)
    )
}

/// The key table and active pane from a [`fence_command`] reply.
pub fn parse_fence_reply(reply: &CommandReply) -> Option<(String, Option<PaneId>)> {
    if reply.error {
        return None;
    }
    let text = reply_text(reply);
    let mut fields = text.split_whitespace();
    let table = fields.next()?.to_owned();
    let pane = fields
        .next()
        .and_then(|pane| pane.strip_prefix('%'))
        .and_then(|id| id.parse().ok())
        .map(PaneId);
    Some((table, pane))
}

// Warp-tagged hooks (KTD13).

/// The message Warp's hooks show to their control client when bindings change. It arrives as
/// `%message` and marks the hook as Warp's.
pub const HOOK_MARKER: &str = "__warp_keys_changed__";

/// Hooks that run after a binding or the prefix may have changed.
pub const HOOK_NAMES: &[&str] = &["after-bind-key", "after-unbind-key", "after-set-option"];

/// The hook array index a client uses. Fixed per client name, so installing is idempotent and
/// detaching can remove the hook without asking tmux where it is.
pub fn hook_slot(client_name: &str) -> u64 {
    4096 + fnv1a(client_name.as_bytes()) % 60_000
}

pub fn install_hook_commands(client_name: &str) -> Vec<String> {
    let slot = hook_slot(client_name);
    let client = quote_argument(client_name);
    HOOK_NAMES
        .iter()
        .map(|hook| {
            format!("set-hook -g {hook}[{slot}] 'display-message -c {client} {HOOK_MARKER}'")
        })
        .collect()
}

pub fn remove_hook_commands(client_name: &str) -> Vec<String> {
    let slot = hook_slot(client_name);
    HOOK_NAMES
        .iter()
        .map(|hook| format!("set-hook -gu {hook}[{slot}]"))
        .collect()
}

pub fn show_hooks_commands() -> Vec<String> {
    HOOK_NAMES
        .iter()
        .map(|hook| format!("show-hooks -g {hook}"))
        .collect()
}

pub const LIST_CLIENTS_COMMAND: &str = "list-clients -F '#{client_name}'";

pub fn parse_lines(reply: &CommandReply) -> Vec<String> {
    if reply.error {
        return Vec::new();
    }
    reply_lines(reply)
        .into_iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

/// Commands that remove Warp-tagged hooks left by a control client that is no longer attached,
/// such as a crashed Warp, from `show-hooks -g` lines (`after-bind-key[4100] display-message -c
/// client-1 __warp_keys_changed__`).
pub fn stale_hook_removals(hook_lines: &[String], live_clients: &[String]) -> Vec<String> {
    hook_lines
        .iter()
        .filter(|line| line.contains(HOOK_MARKER))
        .filter_map(|line| {
            let (slot, command) = line.split_once(' ')?;
            let mut words = command.split_whitespace();
            let client = words
                .by_ref()
                .skip_while(|word| *word != "-c")
                .nth(1)
                .map(|client| client.trim_matches(|c| c == '"' || c == '\''));
            let is_live = client.is_some_and(|client| live_clients.iter().any(|c| c == client));
            (!is_live).then(|| format!("set-hook -gu {slot}"))
        })
        .collect()
}

#[cfg(test)]
#[path = "key_tables_tests.rs"]
mod tests;
