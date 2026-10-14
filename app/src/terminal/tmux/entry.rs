//! Typed `tmux` commands that open natively (U8, R3, R6).
//!
//! A command converts only when, after expanding a leading alias, it is a single simple command
//! whose argv[0] is `tmux` and whose subcommand opens a session: none, `new`/`new-session`
//! without `-d`, or `attach`/`attach-session`/`a`/`at`. The rewrite inserts `-CC` right after
//! `tmux` and leaves the rest of the text, quoting included, as typed. Anything the shell would
//! treat as more than one command (pipes, redirects, chains, subshells, command substitution) is
//! left alone, as is a command that already asks for control mode.

use std::collections::HashSet;

use super::TmuxOriginId;
use crate::terminal::model::session::SessionId;

/// The line a nested origin shows when a typed tmux command stays raw (R6, R29).
pub const NESTED_NOTE: &str =
    "[tmux: Nested tmux is not supported natively; showing tmux's own screen]";

/// The `-L name` or `-S path` a tmux command used, kept as shell text so it can be repeated in
/// later commands on the same host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TmuxSocket {
    /// `L` or `S`.
    flag: char,
    /// The argument as shell text: as typed, or quoted when built from a plain value.
    arg: String,
}

impl TmuxSocket {
    /// A `-L` socket name.
    pub fn name(name: &str) -> Self {
        Self {
            flag: 'L',
            arg: shell_quote(name),
        }
    }

    /// The global flags that select this socket, e.g. `-L work`.
    pub fn global_flags(&self) -> String {
        format!("-{} {}", self.flag, self.arg)
    }
}

/// What to do with a command about to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Run `command` instead, which opens the session natively.
    Convert {
        command: String,
        socket: Option<TmuxSocket>,
    },
    /// The command already asks for control mode; it runs unchanged and converts without asking.
    ControlMode { socket: Option<TmuxSocket> },
    /// The command would convert, but the origin already runs inside tmux: it runs raw, with
    /// [`NESTED_NOTE`].
    Nested,
    /// Not a command that opens a session; it runs unchanged.
    Unchanged,
}

/// Decides what to do with `command`. `alias_value` looks up a shell alias; `nested` is whether
/// the shell that runs it is already inside tmux.
pub fn plan_entry(
    command: &str,
    alias_value: impl Fn(&str) -> Option<String>,
    nested: bool,
) -> Entry {
    plan_entry_with_launcher(command, alias_value, nested, None)
}

/// [`plan_entry`], where a converted command that creates a session without a shell-command of
/// its own starts its first pane through U9's launcher at `launcher` (KTD7).
pub fn plan_entry_with_launcher(
    command: &str,
    alias_value: impl Fn(&str) -> Option<String>,
    nested: bool,
    launcher: Option<&str>,
) -> Entry {
    let Some(words) = lex_simple_command(command) else {
        return Entry::Unchanged;
    };
    let Some(first) = words.first() else {
        return Entry::Unchanged;
    };
    let alias = first
        .is_plain(command)
        .then(|| alias_value(&first.text))
        .flatten();
    let (source, words) = match alias {
        Some(value) => {
            let expanded = format!("{value}{}", &command[first.end..]);
            let Some(words) = lex_simple_command(&expanded) else {
                return Entry::Unchanged;
            };
            (expanded, words)
        }
        None => (command.to_owned(), words),
    };
    let Some(invocation) = parse_invocation(&source, &words) else {
        return Entry::Unchanged;
    };
    match invocation {
        Invocation::ControlMode { socket } => Entry::ControlMode { socket },
        Invocation::OpensSession { .. } if nested => Entry::Nested,
        Invocation::OpensSession {
            argv0_end,
            socket,
            launcher_slot,
        } => {
            let mut command = format!("{} -CC{}", &source[..argv0_end], &source[argv0_end..]);
            if let (Some(slot), Some(launcher)) = (launcher_slot, launcher) {
                command = format!(
                    "{}{}{}",
                    command.trim_end(),
                    slot.prefix(),
                    shell_quote(launcher)
                );
            }
            Entry::Convert { command, socket }
        }
    }
}

impl Entry {
    /// Whether running the command makes Warp the one starting tmux control mode, so the origin's
    /// gateway is armed and the session opens without asking (R3). `converting` is whether a
    /// [`Entry::Convert`] rewrite is applied.
    pub fn arms_gateway(&self, converting: bool) -> bool {
        match self {
            Self::ControlMode { .. } => true,
            Self::Convert { .. } => converting,
            Self::Nested | Self::Unchanged => false,
        }
    }
}

/// The command to send to the shell for `command`: the converted command when automatic
/// conversion is on and applies, otherwise `command` itself.
pub fn command_to_run(
    command: &str,
    alias_value: impl Fn(&str) -> Option<String>,
    nested: bool,
    auto_convert: bool,
) -> String {
    if !auto_convert {
        return command.to_owned();
    }
    match plan_entry(command, alias_value, nested) {
        Entry::Convert { command, .. } => command,
        _ => command.to_owned(),
    }
}

/// What a terminal pane knows about tmux entry.
#[derive(Debug, Default)]
pub struct PaneTmuxState {
    /// The gateway origin of the pane's PTY, once linked.
    pub origin: Option<TmuxOriginId>,
    /// The pane shows a tmux pane, so tmux typed here is nested.
    pub is_tmux_pane: bool,
    /// The tmux pane this pane shows.
    pub tmux_pane: Option<(TmuxOriginId, warp_terminal::tmux::PaneId)>,
    /// The columns and rows tmux's layout gives the tmux pane. The grid always has this size,
    /// whatever the size of the view.
    pub grid_size: Option<(usize, usize)>,
    /// R8's "Warpify this pane", once the pane is known to need it: offered, or disabled with
    /// the reason.
    pub warpify: Option<super::integration::WarpifyAvailability>,
    /// The socket the pane last opened tmux with; the session menu reuses it.
    pub socket: Option<TmuxSocket>,
    /// A note to write when the next command's block starts.
    pub pending_note: Option<&'static str>,
    /// The command as typed behind the conversion that is running, to rerun raw if the server
    /// turns out to be too old (R6).
    pub converted_from: Option<(SessionId, String)>,
    /// Sessions whose tmux is too old for control mode; their commands are not converted.
    pub unsupported_sessions: HashSet<SessionId>,
    /// The pane listens for its origin's gateway refusals.
    pub follows_gateways: bool,
    /// Control-mode bytes Warp did not start wait for the user's answer in this pane (R3).
    pub confirm_pending: bool,
}

/// Single-quotes `value` for a POSIX shell (and fish) unless it is made only of safe characters.
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | '/' | '@' | '%' | '+' | '=' | ':' | ',')
        });
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

enum Invocation {
    OpensSession {
        /// Byte offset just past argv[0], where `-CC` goes.
        argv0_end: usize,
        socket: Option<TmuxSocket>,
        /// Where a launcher can go: the command creates a session with no shell-command.
        launcher_slot: Option<LauncherSlot>,
    },
    ControlMode {
        socket: Option<TmuxSocket>,
    },
}

/// How a launcher is added to a command that creates a session.
#[derive(Clone, Copy)]
enum LauncherSlot {
    /// `tmux` alone: add `new-session` and the launcher.
    NewSession,
    /// `tmux new` without a shell-command: add the launcher.
    ShellCommand,
}

impl LauncherSlot {
    fn prefix(self) -> &'static str {
        match self {
            Self::NewSession => " new-session ",
            Self::ShellCommand => " ",
        }
    }
}

/// tmux's global flags that take no argument and leave the client attachable.
const GLOBAL_SWITCHES: &str = "2lNuv";
/// tmux's global flags that take an argument.
const GLOBAL_OPTIONS: &str = "fLST";
/// `new-session` flags that take an argument; `-d` among the others keeps it detached.
const NEW_SESSION_OPTIONS: &str = "ceFfnstxy";

fn parse_invocation(source: &str, words: &[Word]) -> Option<Invocation> {
    let argv0 = words.first()?;
    if argv0.text.rsplit('/').next() != Some("tmux") {
        return None;
    }

    let mut control_mode = false;
    let mut socket = None;
    let mut index = 1;
    while let Some(word) = words.get(index) {
        let text = word.text.as_str();
        if text == "--" {
            index += 1;
            break;
        }
        let Some(flags) = text.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        for (offset, flag) in flags.char_indices() {
            if flag == 'C' {
                control_mode = true;
            } else if GLOBAL_OPTIONS.contains(flag) {
                let attached_len = 1 + offset + flag.len_utf8();
                let arg = if attached_len < text.len() {
                    attached_argument(source, word, attached_len)
                } else {
                    index += 1;
                    let value = words.get(index)?;
                    source[value.start..value.end].to_owned()
                };
                if matches!(flag, 'L' | 'S') {
                    socket = Some(TmuxSocket { flag, arg });
                }
                break;
            } else if !GLOBAL_SWITCHES.contains(flag) {
                // `-V`, `-c`, `-D` and unknown flags do not open a session.
                return None;
            }
        }
        index += 1;
    }
    if control_mode {
        return Some(Invocation::ControlMode { socket });
    }

    let (opens_session, launcher_slot) = match words.get(index).map(|word| word.text.as_str()) {
        None => (true, Some(LauncherSlot::NewSession)),
        Some("attach" | "attach-session" | "a" | "at") => (true, None),
        Some("new" | "new-session") => {
            let args = &words[index + 1..];
            let slot = (!new_session_has_command(args)).then_some(LauncherSlot::ShellCommand);
            (!new_session_is_detached(args), slot)
        }
        Some(_) => (false, None),
    };
    opens_session.then_some(Invocation::OpensSession {
        argv0_end: argv0.end,
        socket,
        launcher_slot,
    })
}

/// Whether `new-session`'s arguments end with a shell-command (or another tmux command), so
/// nothing may be appended.
fn new_session_has_command(args: &[Word]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let text = arg.text.as_str();
        if text == "--" {
            return args.next().is_some();
        }
        let Some(flags) = text.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            return true;
        };
        for (offset, flag) in flags.char_indices() {
            if NEW_SESSION_OPTIONS.contains(flag) {
                if offset + flag.len_utf8() == flags.len() {
                    args.next();
                }
                break;
            }
        }
    }
    false
}

fn new_session_is_detached(args: &[Word]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let text = arg.text.as_str();
        if text == "--" || text == ";" {
            return false;
        }
        let Some(flags) = text.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            return false;
        };
        for (offset, flag) in flags.char_indices() {
            if flag == 'd' {
                return true;
            }
            if NEW_SESSION_OPTIONS.contains(flag) {
                if offset + flag.len_utf8() == flags.len() {
                    args.next();
                }
                break;
            }
        }
    }
    false
}

/// The argument of a flag written in the same word (`-Lwork`), as shell text.
fn attached_argument(source: &str, word: &Word, decoded_prefix_len: usize) -> String {
    let raw = &source[word.start..word.end];
    let prefix = &word.text[..decoded_prefix_len];
    match raw.strip_prefix(prefix) {
        Some(rest) => rest.to_owned(),
        None => shell_quote(&word.text[decoded_prefix_len..]),
    }
}

/// One shell word: its unquoted text and where it sits in the source.
struct Word {
    text: String,
    start: usize,
    end: usize,
}

impl Word {
    /// Whether the word was written without quotes or escapes, so the shell could treat it as an
    /// alias.
    fn is_plain(&self, source: &str) -> bool {
        source[self.start..self.end] == self.text
    }
}

/// Splits `source` into words if it is one simple command. Returns `None` for anything the shell
/// would run as more than one command or whose words depend on running another command.
fn lex_simple_command(source: &str) -> Option<Vec<Word>> {
    let mut words = Vec::new();
    let mut chars = source.char_indices().peekable();
    let mut current: Option<Word> = None;

    while let Some((index, c)) = chars.next() {
        match c {
            ' ' | '\t' => {
                if let Some(word) = current.take() {
                    words.push(word);
                }
                continue;
            }
            '\n' | '\r' => {
                // A trailing newline ends the command; anything after it is another command.
                if source[index..].trim().is_empty() {
                    break;
                }
                return None;
            }
            '#' if current.is_none() => break,
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' => return None,
            _ => {}
        }

        let word = current.get_or_insert_with(|| Word {
            text: String::new(),
            start: index,
            end: index,
        });
        match c {
            '\\' => match chars.next() {
                Some((_, '\n')) => {}
                Some((_, escaped)) => word.text.push(escaped),
                None => return None,
            },
            '\'' => loop {
                match chars.next() {
                    Some((_, '\'')) => break,
                    Some((_, quoted)) => word.text.push(quoted),
                    None => return None,
                }
            },
            '"' => loop {
                match chars.next() {
                    Some((_, '"')) => break,
                    Some((_, '`')) => return None,
                    Some((_, '$')) if matches!(chars.peek(), Some((_, '('))) => return None,
                    Some((_, '\\')) => match chars.next() {
                        Some((_, escaped @ ('"' | '\\' | '$' | '`'))) => word.text.push(escaped),
                        Some((_, '\n')) => {}
                        Some((_, other)) => {
                            word.text.push('\\');
                            word.text.push(other);
                        }
                        None => return None,
                    },
                    Some((_, quoted)) => word.text.push(quoted),
                    None => return None,
                }
            },
            '$' if matches!(chars.peek(), Some((_, '('))) => return None,
            other => word.text.push(other),
        }
        word.end = chars.peek().map_or(source.len(), |(next, _)| *next);
    }
    if let Some(word) = current.take() {
        words.push(word);
    }
    Some(words)
}

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
