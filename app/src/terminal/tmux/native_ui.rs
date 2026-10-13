//! tmux interactive UI drawn natively (U14, R26, KTD10).
//!
//! A binding whose command opens tmux UI (`confirm-before`, `command-prompt`, `display-menu`,
//! `display-popup`, `choose-tree`, `display-panes`) would open it on Warp's control client, where
//! nothing draws it: it waits invisibly, stalls the client's command queue and swallows keys.
//! Before a key goes through `send-keys -K`, Warp looks up its bound command in the key mirror
//! ([`classify`]) and, for those commands, draws the UI itself and sends tmux only the resulting
//! command. tmux still does all format expansion (`display-message -p`) and template substitution
//! (a background `command-prompt -b` fed with `send-keys -K`), so Warp never interprets a format
//! or a `%%` template; it only splits the binding's arguments, which `list-keys` prints in tmux's
//! canonical quoting ([`split_commands`]).
//!
//! The surfaces ([`ConfirmState`], [`PromptState`], [`MenuState`], [`PickerState`],
//! [`PanesState`]) are pure keyboard state machines; [`TmuxNativeUi`] runs them against live
//! sessions and the views in `view` draw them.

use std::time::Duration;

use warp_terminal::tmux::{CommandReply, PaneId, quote_argument};
use warpui::keymap::Keystroke;

use super::key_tables::TmuxKey;

/// How long a key's commands may keep tmux's command queue busy before the watchdog cancels
/// whatever UI tmux opened on Warp's client.
pub const WATCHDOG_DELAY: Duration = Duration::from_secs(2);

/// tmux's default `display-time`, used until the session's value is known.
pub const DEFAULT_DISPLAY_TIME: Duration = Duration::from_millis(750);

/// tmux's default `display-panes-time`.
pub const DEFAULT_DISPLAY_PANES_TIME: Duration = Duration::from_millis(1000);

/// Toasts never disappear faster than this, so a short `display-time` stays readable.
pub const MIN_TOAST_TIME: Duration = Duration::from_millis(750);

// Splitting a bound command.

/// One argument of a bound command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The argument's value: quotes removed, escapes applied, a `{ }` block's inner text.
    pub text: String,
    /// The argument exactly as `list-keys` printed it, which tmux parses back to `text`.
    pub raw: String,
    /// The argument was a `{ }` command block.
    pub block: bool,
}

/// Splits a command as `list-keys` prints it into its commands (separated by `;` or `\;`) and
/// each command into arguments. Formats and templates are left alone.
pub fn split_commands(source: &str) -> Vec<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let mut commands = Vec::new();
    let mut current: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == ';' || (c == '\\' && chars.get(i + 1) == Some(&';')) {
            let end = if c == ';' { i + 1 } else { i + 2 };
            if chars.get(end).is_none_or(|next| next.is_whitespace()) {
                if !current.is_empty() {
                    commands.push(std::mem::take(&mut current));
                }
                i = end;
                continue;
            }
        }
        let start = i;
        if c == '{' {
            let end = block_end(&chars, i);
            let inner: String = chars[i + 1..end.saturating_sub(1).max(i + 1)]
                .iter()
                .collect();
            current.push(Token {
                text: inner.trim().to_owned(),
                raw: chars[start..end].iter().collect(),
                block: true,
            });
            i = end;
            continue;
        }
        let mut text = String::new();
        while i < chars.len() && !chars[i].is_whitespace() {
            match chars[i] {
                '\'' => {
                    i += 1;
                    while i < chars.len() && chars[i] != '\'' {
                        text.push(chars[i]);
                        i += 1;
                    }
                    i += 1;
                }
                '"' => {
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' && i + 1 < chars.len() {
                            text.push(unescape(chars[i + 1]));
                            i += 2;
                        } else {
                            text.push(chars[i]);
                            i += 1;
                        }
                    }
                    i += 1;
                }
                '\\' if i + 1 < chars.len() => {
                    text.push(chars[i + 1]);
                    i += 2;
                }
                other => {
                    text.push(other);
                    i += 1;
                }
            }
        }
        current.push(Token {
            text,
            raw: chars[start..i.min(chars.len())].iter().collect(),
            block: false,
        });
    }
    if !current.is_empty() {
        commands.push(current);
    }
    commands
}

fn unescape(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'e' => '\u{1b}',
        other => other,
    }
}

/// The index just past the `}` that closes the block opening at `open`, skipping quoted text.
fn block_end(chars: &[char], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '\\' => i += 1,
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// A command's flags and positional arguments, given which flags take a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    /// Flags without a value, in order.
    pub switches: Vec<char>,
    /// Flags with their value tokens, in order.
    pub values: Vec<(char, Token)>,
    pub positional: Vec<Token>,
}

impl ParsedArgs {
    pub fn has(&self, flag: char) -> bool {
        self.switches.contains(&flag) || self.values.iter().any(|(f, _)| *f == flag)
    }

    /// The last value given for `flag`.
    pub fn value(&self, flag: char) -> Option<&Token> {
        self.values
            .iter()
            .rev()
            .find(|(f, _)| *f == flag)
            .map(|(_, token)| token)
    }

    /// Every value given for `flag`.
    pub fn all(&self, flag: char) -> impl Iterator<Item = &Token> {
        self.values
            .iter()
            .filter(move |(f, _)| *f == flag)
            .map(|(_, token)| token)
    }
}

/// Parses `args` (a command's arguments after its name) getopt-style. `with_value` lists the
/// flags that take a value.
pub fn parse_args(args: &[Token], with_value: &str) -> ParsedArgs {
    let mut parsed = ParsedArgs::default();
    let mut i = 0;
    while i < args.len() {
        let token = &args[i];
        if token.block || !token.text.starts_with('-') || token.text.len() < 2 {
            break;
        }
        if token.text == "--" {
            i += 1;
            break;
        }
        let flags: Vec<char> = token.text[1..].chars().collect();
        let mut j = 0;
        while j < flags.len() {
            let flag = flags[j];
            if with_value.contains(flag) {
                let attached: String = flags[j + 1..].iter().collect();
                if !attached.is_empty() {
                    parsed.values.push((
                        flag,
                        Token {
                            raw: quote_argument(&attached),
                            text: attached,
                            block: false,
                        },
                    ));
                } else if let Some(value) = args.get(i + 1) {
                    parsed.values.push((flag, value.clone()));
                    i += 1;
                }
                break;
            }
            parsed.switches.push(flag);
            j += 1;
        }
        i += 1;
    }
    parsed.positional = args[i.min(args.len())..].to_vec();
    parsed
}

// Classifying a bound command.

/// `confirm-before`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmSpec {
    /// The `-p` prompt, a format.
    pub prompt: Option<String>,
    /// The command to run on confirmation.
    pub command: String,
    /// `-y`: Enter confirms too.
    pub default_yes: bool,
    /// `-c`: the key that confirms instead of `y`.
    pub confirm_key: Option<String>,
}

/// `command-prompt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSpec {
    /// The `-p` prompts, comma-separated, each a format.
    pub prompts: Option<String>,
    /// The `-I` initial texts, comma-separated, each a format.
    pub inputs: Option<String>,
    /// The template exactly as bound, `None` for tmux's default (run the text as a command).
    pub template_raw: Option<String>,
    /// `-1` or `-k`: the prompt takes one key.
    pub single_key: bool,
    /// `-k`: the key's name is substituted.
    pub key_name: bool,
    /// `-F`: the template is a format.
    pub format_template: bool,
    /// `-N`: numeric input only.
    pub numeric: bool,
}

/// `choose-tree` and friends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChooseSpec {
    /// `-s`: start with sessions collapsed.
    pub sessions_collapsed: bool,
    /// `-w`: start with windows collapsed.
    pub windows_collapsed: bool,
}

/// One `display-menu` entry before expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuSpecItem {
    Separator,
    Item {
        /// The name, a format; empty after expansion hides it, a leading `-` disables it.
        name: String,
        key: String,
        /// The command as bound; a block's inner text.
        command: String,
    },
}

/// `display-menu`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSpec {
    pub title: Option<String>,
    pub items: Vec<MenuSpecItem>,
}

/// `display-popup`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupSpec {
    /// The shell command and its arguments, as bound. Empty runs the default shell.
    pub command_raw: Vec<String>,
    /// `-d`, as bound.
    pub directory_raw: Option<String>,
    /// `-e` values, as bound.
    pub environment_raw: Vec<String>,
    pub title: Option<String>,
}

/// `display-panes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanesSpec {
    /// The template run with the chosen pane, as bound.
    pub template: Option<String>,
}

/// A bound command Warp draws natively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interactive {
    Confirm(ConfirmSpec),
    Prompt(PromptSpec),
    Choose(ChooseSpec),
    Menu(MenuSpec),
    Popup(PopupSpec),
    Panes(PanesSpec),
}

/// What a bound command needs from Warp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Classified {
    /// Nothing interactive: tmux runs it.
    Plain,
    /// Warp draws it.
    Native(Interactive),
    /// It would open tmux UI Warp cannot draw in its place (inside a command sequence, or a
    /// variant Warp does not support). The watchdog cancels it if it blocks. Holds the command's
    /// name.
    Unsupported(String),
}

/// The canonical name of an interactive command, resolving tmux's aliases.
fn interactive_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "confirm-before" | "confirm" => "confirm-before",
        "command-prompt" => "command-prompt",
        "display-menu" | "menu" => "display-menu",
        "display-popup" | "popup" => "display-popup",
        "choose-tree" => "choose-tree",
        "choose-session" => "choose-session",
        "choose-window" => "choose-window",
        "display-panes" | "displayp" => "display-panes",
        _ => return None,
    })
}

/// Classifies a bound command (KTD10).
pub fn classify(command: &str) -> Classified {
    let commands = split_commands(command);
    let interactive: Vec<&'static str> = commands
        .iter()
        .filter_map(|tokens| interactive_name(&tokens.first()?.text))
        .collect();
    let Some(first) = interactive.first() else {
        return Classified::Plain;
    };
    if commands.len() != 1 {
        return Classified::Unsupported((*first).to_owned());
    }
    let tokens = &commands[0];
    let args = &tokens[1..];
    if *first == "display-popup" && parse_args(args, "bcdehsStTwxy").has('C') {
        // Closing a popup neither draws nor waits; tmux runs it.
        return Classified::Plain;
    }
    let native = match *first {
        "confirm-before" => confirm_spec(args).map(Interactive::Confirm),
        "command-prompt" => Some(Interactive::Prompt(prompt_spec(args))),
        "display-menu" => menu_spec(args).map(Interactive::Menu),
        "display-popup" => popup_spec(args).map(Interactive::Popup),
        "choose-tree" => {
            let parsed = parse_args(args, "FfKOt");
            Some(Interactive::Choose(ChooseSpec {
                sessions_collapsed: parsed.has('s'),
                windows_collapsed: parsed.has('w'),
            }))
        }
        "choose-session" => Some(Interactive::Choose(ChooseSpec {
            sessions_collapsed: true,
            windows_collapsed: false,
        })),
        "choose-window" => Some(Interactive::Choose(ChooseSpec {
            sessions_collapsed: false,
            windows_collapsed: true,
        })),
        "display-panes" => {
            let parsed = parse_args(args, "dt");
            Some(Interactive::Panes(PanesSpec {
                template: parsed.positional.first().map(|token| token.text.clone()),
            }))
        }
        _ => None,
    };
    match native {
        Some(native) => Classified::Native(native),
        None => Classified::Unsupported((*first).to_owned()),
    }
}

fn confirm_spec(args: &[Token]) -> Option<ConfirmSpec> {
    let parsed = parse_args(args, "cpt");
    let command = parsed.positional.first()?.text.clone();
    Some(ConfirmSpec {
        prompt: parsed.value('p').map(|token| token.text.clone()),
        command,
        default_yes: parsed.has('y'),
        confirm_key: parsed.value('c').map(|token| token.text.clone()),
    })
}

fn prompt_spec(args: &[Token]) -> PromptSpec {
    let parsed = parse_args(args, "IptT");
    PromptSpec {
        prompts: parsed.value('p').map(|token| token.text.clone()),
        inputs: parsed.value('I').map(|token| token.text.clone()),
        template_raw: parsed.positional.first().map(|token| token.raw.clone()),
        single_key: parsed.has('1') || parsed.has('k'),
        key_name: parsed.has('k'),
        format_template: parsed.has('F'),
        numeric: parsed.has('N'),
    }
}

fn menu_spec(args: &[Token]) -> Option<MenuSpec> {
    let parsed = parse_args(args, "bcCHsStTxy");
    let mut items = Vec::new();
    let mut rest = parsed.positional.iter();
    while let Some(name) = rest.next() {
        if name.text.is_empty() {
            items.push(MenuSpecItem::Separator);
            continue;
        }
        let key = rest.next()?;
        let command = rest.next()?;
        items.push(MenuSpecItem::Item {
            name: name.text.clone(),
            key: key.text.clone(),
            command: command.text.clone(),
        });
    }
    Some(MenuSpec {
        title: parsed.value('T').map(|token| token.text.clone()),
        items,
    })
}

fn popup_spec(args: &[Token]) -> Option<PopupSpec> {
    let parsed = parse_args(args, "bcdehsStTwxy");
    Some(PopupSpec {
        command_raw: parsed
            .positional
            .iter()
            .map(|token| token.raw.clone())
            .collect(),
        directory_raw: parsed.value('d').map(|token| token.raw.clone()),
        environment_raw: parsed.all('e').map(|token| token.raw.clone()).collect(),
        title: parsed.value('T').map(|token| token.text.clone()),
    })
}

// Command builders.

/// A command line that prints `format` expanded for `pane_id`'s context.
pub fn expand_query(format: &str, pane_id: PaneId) -> String {
    format!(
        "display-message -p -t {pane_id} {}",
        quote_argument(&strip_control(format))
    )
}

/// The first line of a [`expand_query`] reply, with tmux style markup (`#[...]`) removed.
pub fn parse_expansion(reply: &CommandReply) -> Option<String> {
    if reply.error {
        return None;
    }
    let line = reply
        .lines
        .first()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .unwrap_or_default();
    Some(strip_style(&line))
}

/// Removes tmux's `#[...]` style markup.
pub fn strip_style(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("#[") {
        out.push_str(&rest[..start]);
        match rest[start..].find(']') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn strip_control(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// tmux's prompt when `confirm-before` has no `-p`.
pub fn default_confirm_prompt(command: &str) -> String {
    let name = command.split_whitespace().next().unwrap_or(command);
    format!("Confirm '{name}'? (y/n)")
}

/// The prompt labels of a `command-prompt`: its `-p` entries, or tmux's default.
pub fn prompt_labels(spec: &PromptSpec) -> Vec<String> {
    match &spec.prompts {
        Some(prompts) => prompts.split(',').map(str::to_owned).collect(),
        None => {
            let name = spec
                .template_raw
                .as_deref()
                .and_then(|raw| split_commands(raw).into_iter().next())
                .and_then(|tokens| {
                    let first = tokens.into_iter().next()?;
                    if first.block {
                        split_commands(&first.text)
                            .into_iter()
                            .next()
                            .and_then(|tokens| tokens.into_iter().next())
                            .map(|token| token.text)
                    } else {
                        first.text.split_whitespace().next().map(str::to_owned)
                    }
                });
            vec![match name {
                Some(name) => format!("({name})"),
                None => ":".to_owned(),
            }]
        }
    }
}

/// The initial texts of a `command-prompt`, one per label.
pub fn prompt_inputs(spec: &PromptSpec, count: usize) -> Vec<String> {
    let mut inputs: Vec<String> = spec
        .inputs
        .as_deref()
        .map(|inputs| inputs.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    inputs.resize(count, String::new());
    inputs
}

/// ` -c <client>`, or nothing to let tmux use the client sending the command: Warp's own.
fn client_flag(client: Option<&str>) -> String {
    match client.filter(|client| !client.is_empty()) {
        Some(client) => format!(" -c {}", quote_argument(client)),
        None => String::new(),
    }
}

/// The background prompt tmux substitutes the template in, and the keys that fill it with
/// `values` (KTD10: Warp never substitutes a template itself). Entered on `client`, Warp's own
/// control client. For a single-key prompt `values` holds the key's tmux name.
pub fn prompt_submit_commands(
    spec: &PromptSpec,
    values: &[String],
    client: Option<&str>,
) -> Vec<String> {
    let mut flags = String::from("-b");
    if spec.format_template {
        flags.push_str(" -F");
    }
    if spec.single_key {
        flags.push_str(if spec.key_name { " -k" } else { " -1" });
    }
    let labels: Vec<String> = values.iter().map(|_| "warp".to_owned()).collect();
    let mut prompt = format!(
        "command-prompt {flags} -p {}",
        quote_argument(&labels.join(","))
    );
    if let Some(template) = &spec.template_raw {
        prompt.push(' ');
        prompt.push_str(template);
    }
    let client = client_flag(client);
    let mut commands = vec![prompt];
    for value in values {
        if spec.single_key {
            commands.push(format!("send-keys -K{client} {}", quote_argument(value)));
            continue;
        }
        let value = strip_control(value);
        if !value.is_empty() {
            commands.push(format!(
                "send-keys -K{client} -l {}",
                quote_argument(&value)
            ));
        }
        commands.push(format!("send-keys -K{client} Enter"));
    }
    commands
}

/// Puts `client` back in the root key table after Warp took the key that followed the prefix.
pub fn reset_key_table_command(client: Option<&str>) -> String {
    format!("switch-client{} -T root", client_flag(client))
}

/// `display-time` of the attached session.
pub const DISPLAY_TIME_QUERY: &str = "show-options -v display-time";

/// How long a toast for a tmux message stays, from a [`DISPLAY_TIME_QUERY`] reply. tmux's 0
/// ("until a key is pressed") and anything shorter than [`MIN_TOAST_TIME`] use that minimum.
pub fn parse_display_time(reply: &CommandReply) -> Duration {
    if reply.error {
        return DEFAULT_DISPLAY_TIME;
    }
    let millis = reply
        .lines
        .first()
        .and_then(|line| String::from_utf8_lossy(line).trim().parse::<u64>().ok());
    match millis {
        Some(millis) => Duration::from_millis(millis).max(MIN_TOAST_TIME),
        None => DEFAULT_DISPLAY_TIME,
    }
}

/// The non-modal strip shown while a pane is in a tmux mode.
pub fn mode_strip_text(mode: &str) -> String {
    let exit = match mode {
        "copy-mode" | "view-mode" | "tree-mode" | "buffer-mode" | "client-mode"
        | "options-mode" => "press q to exit",
        _ => "press q or Escape to exit",
    };
    format!("tmux {mode}: {exit}")
}

/// The toast when the watchdog cancelled UI tmux opened on Warp's client.
pub fn watchdog_toast_text(command: Option<&str>) -> String {
    match command {
        Some(command) => format!(
            "tmux opened {command}, which Warp can't show in this binding, so Warp cancelled it."
        ),
        None => "tmux stopped answering while handling a key, so Warp cancelled the prompt tmux \
                 opened."
            .to_owned(),
    }
}

/// The shell script that closes whatever tmux UI is open on `client` from a second client: the
/// control client's own command queue is stalled behind it.
pub fn watchdog_cancel_script(socket_path: &str, client: &str) -> String {
    let tmux = format!("tmux -S {}", super::entry::shell_quote(socket_path));
    let client = super::entry::shell_quote(client);
    format!(
        "{tmux} send-keys -K -c {client} Escape 2>/dev/null; \
         {tmux} display-popup -C -c {client} 2>/dev/null; true"
    )
}

// choose-tree.

/// One pane of the server for the picker.
pub const CHOOSE_TREE_QUERY: &str = "list-panes -a -F '#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_id}\t#{pane_index}\t#{pane_current_command}\t#{pane_active}\t#{window_active}'";

/// A [`CHOOSE_TREE_QUERY`] row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_id: String,
    pub pane_index: u32,
    pub command: String,
    pub pane_active: bool,
    pub window_active: bool,
}

pub fn parse_tree_rows(reply: &CommandReply) -> Vec<TreeRow> {
    if reply.error {
        return Vec::new();
    }
    reply
        .lines
        .iter()
        .filter_map(|line| {
            let line = String::from_utf8_lossy(line);
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 10 {
                return None;
            }
            Some(TreeRow {
                session_id: fields[0].to_owned(),
                session_name: strip_control(fields[1]),
                window_id: fields[2].to_owned(),
                window_index: fields[3].parse().ok()?,
                window_name: strip_control(fields[4]),
                pane_id: fields[5].to_owned(),
                pane_index: fields[6].parse().ok()?,
                command: strip_control(fields[7]),
                pane_active: fields[8] == "1",
                window_active: fields[9] == "1",
            })
        })
        .filter(|row| !super::popup::is_popup_window_name(&row.window_name))
        .collect()
}

/// What a picker row selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeTarget {
    Session {
        session_id: String,
    },
    Window {
        session_id: String,
        window_id: String,
    },
    Pane {
        session_id: String,
        window_id: String,
        pane_id: String,
    },
}

/// One visible picker row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerRow {
    pub depth: usize,
    pub label: String,
    pub target: TreeTarget,
    /// Whether the row has children and they are shown.
    pub expandable: bool,
    pub expanded: bool,
}

/// The commands that make `client` show `target`. `current_session` is the session the client
/// shows now.
pub fn select_target_commands(
    target: &TreeTarget,
    current_session: Option<&str>,
    client: Option<&str>,
) -> Vec<String> {
    let client = client_flag(client);
    let (session_id, window_id, pane_id) = match target {
        TreeTarget::Session { session_id } => (session_id, None, None),
        TreeTarget::Window {
            session_id,
            window_id,
        } => (session_id, Some(window_id), None),
        TreeTarget::Pane {
            session_id,
            window_id,
            pane_id,
        } => (session_id, Some(window_id), Some(pane_id)),
    };
    let mut commands = Vec::new();
    if let Some(window_id) = window_id {
        commands.push(format!("select-window -t {}", quote_argument(window_id)));
    }
    if let Some(pane_id) = pane_id {
        commands.push(format!("select-pane -t {}", quote_argument(pane_id)));
    }
    if current_session != Some(session_id.as_str()) {
        commands.push(format!(
            "switch-client{client} -t {}",
            quote_argument(session_id)
        ));
    }
    commands
}

// Native surfaces. Each takes keys as tmux key names and returns what to do.

/// What a key did to a surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceOutcome {
    /// The surface stays open (and may have changed).
    Continue,
    /// The surface closes without running anything.
    Cancel,
    /// The surface closes and Warp runs these bound commands (each may itself be interactive).
    Run(Vec<String>),
    /// The prompt closes with these values for tmux to substitute.
    Submit(Vec<String>),
    /// The picker closes on this target.
    Select(TreeTarget),
}

fn is_named(key: &TmuxKey, name: &str) -> bool {
    !key.ctrl && !key.meta && key.base == name
}

/// The character a key types, if it is plain text.
fn typed_char(key: &TmuxKey) -> Option<char> {
    if key.ctrl || key.meta {
        return None;
    }
    if key.base == "Space" {
        return Some(' ');
    }
    let mut chars = key.base.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// A key from Warp's keystroke.
pub fn key_of(keystroke: &Keystroke) -> Option<TmuxKey> {
    TmuxKey::from_keystroke(keystroke)
}

/// `confirm-before`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmState {
    /// The expanded prompt.
    pub prompt: String,
    pub command: String,
    pub default_yes: bool,
    pub confirm_key: Option<TmuxKey>,
}

impl ConfirmState {
    pub fn new(spec: &ConfirmSpec, prompt: String) -> Self {
        Self {
            prompt,
            command: spec.command.clone(),
            default_yes: spec.default_yes,
            confirm_key: spec.confirm_key.as_deref().and_then(TmuxKey::parse),
        }
    }

    /// As in tmux: the confirm key (`y`, or `-c`'s), or Enter with `-y`, runs the command; any
    /// other key cancels.
    pub fn key(&self, key: &TmuxKey) -> SurfaceOutcome {
        let confirm = match &self.confirm_key {
            Some(confirm) => key == confirm,
            None => is_named(key, "y") || is_named(key, "Y"),
        };
        if confirm || (self.default_yes && is_named(key, "Enter")) {
            SurfaceOutcome::Run(vec![self.command.clone()])
        } else {
            SurfaceOutcome::Cancel
        }
    }
}

/// One text field of a native prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptField {
    pub label: String,
    pub value: String,
}

/// `command-prompt`: one field per `-p` entry, filled in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptState {
    pub fields: Vec<PromptField>,
    /// The field taking typed text.
    pub active: usize,
    pub single_key: bool,
    pub numeric: bool,
}

impl PromptState {
    pub fn new(spec: &PromptSpec, labels: Vec<String>, inputs: Vec<String>) -> Self {
        let fields = labels
            .into_iter()
            .zip(inputs.into_iter().chain(std::iter::repeat(String::new())))
            .map(|(label, value)| PromptField { label, value })
            .collect();
        Self {
            fields,
            active: 0,
            single_key: spec.single_key,
            numeric: spec.numeric,
        }
    }

    /// Enter moves to the next field and submits on the last; Escape cancels; Tab and BTab move
    /// between fields; Backspace and C-u edit.
    pub fn key(&mut self, key: &TmuxKey) -> SurfaceOutcome {
        if self.single_key {
            if is_named(key, "Escape") {
                return SurfaceOutcome::Cancel;
            }
            return SurfaceOutcome::Submit(vec![key.name()]);
        }
        if is_named(key, "Escape") || (key.ctrl && key.base == "c") {
            return SurfaceOutcome::Cancel;
        }
        if is_named(key, "Enter") {
            if self.active + 1 < self.fields.len() {
                self.active += 1;
                return SurfaceOutcome::Continue;
            }
            return SurfaceOutcome::Submit(
                self.fields
                    .iter()
                    .map(|field| field.value.clone())
                    .collect(),
            );
        }
        if is_named(key, "Tab") {
            self.active = (self.active + 1).min(self.fields.len().saturating_sub(1));
            return SurfaceOutcome::Continue;
        }
        if is_named(key, "BTab") {
            self.active = self.active.saturating_sub(1);
            return SurfaceOutcome::Continue;
        }
        let Some(field) = self.fields.get_mut(self.active) else {
            return SurfaceOutcome::Cancel;
        };
        if is_named(key, "BSpace") {
            field.value.pop();
        } else if key.ctrl && !key.meta && key.base == "u" {
            field.value.clear();
        } else if let Some(c) = typed_char(key)
            && (!self.numeric || c.is_ascii_digit())
        {
            field.value.push(c);
        }
        SurfaceOutcome::Continue
    }
}

/// One expanded `display-menu` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuRow {
    Separator,
    Item {
        label: String,
        key: Option<TmuxKey>,
        command: String,
        enabled: bool,
    },
}

/// `display-menu` with names expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuState {
    pub title: String,
    pub rows: Vec<MenuRow>,
    /// The highlighted row, always an enabled item when there is one.
    pub selected: Option<usize>,
}

impl MenuState {
    /// Builds the menu from `spec` and each item's expanded name (in item order, separators
    /// excluded). Items whose name expands empty are hidden, as in tmux; a leading `-` disables
    /// an item.
    pub fn new(spec: &MenuSpec, title: String, names: &[String]) -> Self {
        let mut names = names.iter();
        let mut rows: Vec<MenuRow> = Vec::new();
        for item in &spec.items {
            match item {
                MenuSpecItem::Separator => {
                    if rows.last().is_some_and(|row| *row != MenuRow::Separator) {
                        rows.push(MenuRow::Separator);
                    }
                }
                MenuSpecItem::Item { key, command, .. } => {
                    let name = names.next().cloned().unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let (label, enabled) = match name.strip_prefix('-') {
                        Some(label) => (label.to_owned(), false),
                        None => (name, true),
                    };
                    rows.push(MenuRow::Item {
                        label,
                        key: TmuxKey::parse(key),
                        command: command.clone(),
                        enabled,
                    });
                }
            }
        }
        while rows.last() == Some(&MenuRow::Separator) {
            rows.pop();
        }
        let selected = rows.iter().position(Self::is_enabled);
        Self {
            title,
            rows,
            selected,
        }
    }

    fn is_enabled(row: &MenuRow) -> bool {
        matches!(row, MenuRow::Item { enabled: true, .. })
    }

    /// Up/Down move over enabled items, Enter runs the highlighted one, Escape or q cancels, and
    /// an item's own key runs it.
    pub fn key(&mut self, key: &TmuxKey) -> SurfaceOutcome {
        if is_named(key, "Escape") || (key.ctrl && key.base == "c") {
            return SurfaceOutcome::Cancel;
        }
        if let Some(command) = self.rows.iter().find_map(|row| match row {
            MenuRow::Item {
                key: Some(item_key),
                command,
                enabled: true,
                ..
            } if item_key == key => Some(command.clone()),
            _ => None,
        }) {
            return SurfaceOutcome::Run(vec![command]);
        }
        if is_named(key, "q") {
            return SurfaceOutcome::Cancel;
        }
        if is_named(key, "Up") || (key.ctrl && key.base == "p") || is_named(key, "k") {
            self.step(false);
        } else if is_named(key, "Down") || (key.ctrl && key.base == "n") || is_named(key, "j") {
            self.step(true);
        } else if is_named(key, "Enter") {
            return match self.selected.and_then(|index| self.rows.get(index)) {
                Some(MenuRow::Item {
                    command,
                    enabled: true,
                    ..
                }) => SurfaceOutcome::Run(vec![command.clone()]),
                _ => SurfaceOutcome::Cancel,
            };
        }
        SurfaceOutcome::Continue
    }

    /// Runs the item at `index` (a click).
    pub fn activate(&self, index: usize) -> SurfaceOutcome {
        match self.rows.get(index) {
            Some(MenuRow::Item {
                command,
                enabled: true,
                ..
            }) => SurfaceOutcome::Run(vec![command.clone()]),
            _ => SurfaceOutcome::Continue,
        }
    }

    fn step(&mut self, forward: bool) {
        let enabled: Vec<usize> = (0..self.rows.len())
            .filter(|index| Self::is_enabled(&self.rows[*index]))
            .collect();
        if enabled.is_empty() {
            return;
        }
        let position = self
            .selected
            .and_then(|selected| enabled.iter().position(|index| *index == selected));
        let next = match (position, forward) {
            (None, _) => 0,
            (Some(position), true) => (position + 1) % enabled.len(),
            (Some(position), false) => (position + enabled.len() - 1) % enabled.len(),
        };
        self.selected = Some(enabled[next]);
    }
}

/// `choose-tree`: sessions, their windows and panes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerState {
    pub title: String,
    tree: Vec<TreeRow>,
    collapsed_sessions: Vec<String>,
    collapsed_windows: Vec<String>,
    /// The highlighted row among [`Self::rows`].
    pub selected: usize,
}

impl PickerState {
    /// The picker over `tree`, with the current session's active pane highlighted.
    pub fn new(spec: ChooseSpec, tree: Vec<TreeRow>, current_session: Option<&str>) -> Self {
        let mut sessions: Vec<String> = Vec::new();
        let mut windows: Vec<String> = Vec::new();
        for row in &tree {
            if !sessions.contains(&row.session_id) {
                sessions.push(row.session_id.clone());
            }
            if !windows.contains(&row.window_id) {
                windows.push(row.window_id.clone());
            }
        }
        let current = current_session.unwrap_or_default().to_owned();
        let collapsed_sessions = if spec.sessions_collapsed {
            sessions
        } else {
            Vec::new()
        };
        let collapsed_windows = if spec.windows_collapsed {
            windows
        } else {
            Vec::new()
        };
        let mut picker = Self {
            title: "Choose a session, window or pane".to_owned(),
            tree,
            collapsed_sessions,
            collapsed_windows,
            selected: 0,
        };
        let rows = picker.rows();
        let current_pane = picker
            .tree
            .iter()
            .find(|row| row.session_id == current && row.window_active && row.pane_active);
        picker.selected = rows
            .iter()
            .position(|row| match (&row.target, current_pane) {
                (TreeTarget::Pane { pane_id, .. }, Some(pane)) => *pane_id == pane.pane_id,
                _ => false,
            })
            .or_else(|| {
                rows.iter().position(|row| match (&row.target, current_pane) {
                    (TreeTarget::Window { window_id, .. }, Some(pane)) => {
                        *window_id == pane.window_id
                    }
                    _ => false,
                })
            })
            .or_else(|| {
                rows.iter().position(
                    |row| matches!(&row.target, TreeTarget::Session { session_id } if *session_id == current),
                )
            })
            .unwrap_or(0);
        picker
    }

    /// The visible rows, sessions then their windows then their panes.
    pub fn rows(&self) -> Vec<PickerRow> {
        let mut rows = Vec::new();
        let mut seen_sessions: Vec<&str> = Vec::new();
        for row in &self.tree {
            if seen_sessions.contains(&row.session_id.as_str()) {
                continue;
            }
            seen_sessions.push(&row.session_id);
            let session_rows: Vec<&TreeRow> = self
                .tree
                .iter()
                .filter(|other| other.session_id == row.session_id)
                .collect();
            let mut window_ids: Vec<&str> = Vec::new();
            for pane in &session_rows {
                if !window_ids.contains(&pane.window_id.as_str()) {
                    window_ids.push(&pane.window_id);
                }
            }
            let session_expanded = !self.collapsed_sessions.contains(&row.session_id);
            let noun = if window_ids.len() == 1 {
                "window"
            } else {
                "windows"
            };
            rows.push(PickerRow {
                depth: 0,
                label: format!("{} ({} {noun})", row.session_name, window_ids.len()),
                target: TreeTarget::Session {
                    session_id: row.session_id.clone(),
                },
                expandable: true,
                expanded: session_expanded,
            });
            if !session_expanded {
                continue;
            }
            for window_id in window_ids {
                let panes: Vec<&&TreeRow> = session_rows
                    .iter()
                    .filter(|pane| pane.window_id == window_id)
                    .collect();
                let Some(first) = panes.first() else {
                    continue;
                };
                let window_expanded = !self.collapsed_windows.contains(&first.window_id);
                rows.push(PickerRow {
                    depth: 1,
                    label: format!("{}: {}", first.window_index, first.window_name),
                    target: TreeTarget::Window {
                        session_id: first.session_id.clone(),
                        window_id: first.window_id.clone(),
                    },
                    expandable: panes.len() > 1,
                    expanded: window_expanded && panes.len() > 1,
                });
                if !window_expanded || panes.len() < 2 {
                    continue;
                }
                for pane in panes {
                    rows.push(PickerRow {
                        depth: 2,
                        label: format!("{}: {}", pane.pane_index, pane.command),
                        target: TreeTarget::Pane {
                            session_id: pane.session_id.clone(),
                            window_id: pane.window_id.clone(),
                            pane_id: pane.pane_id.clone(),
                        },
                        expandable: false,
                        expanded: false,
                    });
                }
            }
        }
        rows
    }

    /// Up/Down move, Right/Left expand and collapse, Enter chooses, Escape or q cancels.
    pub fn key(&mut self, key: &TmuxKey) -> SurfaceOutcome {
        let rows = self.rows();
        if rows.is_empty() || is_named(key, "Escape") || is_named(key, "q") {
            return SurfaceOutcome::Cancel;
        }
        self.selected = self.selected.min(rows.len() - 1);
        if is_named(key, "Up") || is_named(key, "k") {
            self.selected = self.selected.checked_sub(1).unwrap_or(rows.len() - 1);
        } else if is_named(key, "Down") || is_named(key, "j") {
            self.selected = (self.selected + 1) % rows.len();
        } else if is_named(key, "Right") || is_named(key, "l") || is_named(key, "+") {
            self.set_expanded(&rows[self.selected].target, true);
        } else if is_named(key, "Left") || is_named(key, "h") || is_named(key, "-") {
            self.set_expanded(&rows[self.selected].target, false);
        } else if is_named(key, "Enter") {
            return SurfaceOutcome::Select(rows[self.selected].target.clone());
        }
        SurfaceOutcome::Continue
    }

    /// Chooses the row at `index` (a click).
    pub fn activate(&self, index: usize) -> SurfaceOutcome {
        match self.rows().get(index) {
            Some(row) => SurfaceOutcome::Select(row.target.clone()),
            None => SurfaceOutcome::Continue,
        }
    }

    fn set_expanded(&mut self, target: &TreeTarget, expanded: bool) {
        let (list, id) = match target {
            TreeTarget::Session { session_id } => (&mut self.collapsed_sessions, session_id),
            TreeTarget::Window { window_id, .. } => (&mut self.collapsed_windows, window_id),
            TreeTarget::Pane { .. } => return,
        };
        list.retain(|other| other != id);
        if !expanded {
            list.push(id.clone());
        }
    }
}

/// `display-panes`: each pane of the current window with its number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanesState {
    /// Panes in tmux's index order.
    pub panes: Vec<PaneId>,
    pub template: Option<String>,
}

impl PanesState {
    /// A digit chooses that pane, as in tmux; any other key closes the overlay.
    pub fn key(&self, key: &TmuxKey) -> SurfaceOutcome {
        let pane = typed_char(key)
            .and_then(|c| c.to_digit(10))
            .and_then(|digit| self.panes.get(digit as usize));
        match pane {
            Some(pane) => SurfaceOutcome::Run(vec![self.command_for(*pane)]),
            None => SurfaceOutcome::Cancel,
        }
    }

    /// tmux replaces `%%` in display-panes' template with the pane id; this mirrors that one
    /// substitution, which is all display-panes does.
    pub fn command_for(&self, pane: PaneId) -> String {
        match &self.template {
            Some(template) => template.replace("%%", &pane.to_string()),
            None => format!("select-pane -t {pane}"),
        }
    }
}

#[cfg(feature = "local_tty")]
mod model;
#[cfg(feature = "local_tty")]
pub mod view;
#[cfg(feature = "local_tty")]
pub use model::*;

#[cfg(test)]
#[path = "native_ui_tests.rs"]
mod tests;
