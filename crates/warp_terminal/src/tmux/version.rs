//! Parse tmux version strings (`tmux -V` output or `#{version}`) into feature gates.
//!
//! Warp supports native tmux sessions from 3.2 (control-mode flow control: `pause-after`,
//! `%pause`/`%continue`, `%extended-output`). One feature is gated above that floor:
//! `send-keys -K` and `list-keys -F`, both added in 3.4.

/// A tmux version as tmux reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxVersion {
    /// A release such as `3.4` or `3.7c`. Release candidates (`3.4-rc`) parse as the release.
    Release {
        major: u32,
        minor: u32,
        letter: Option<char>,
    },
    /// A development build heading for the named release, e.g. `next-3.8`.
    Next { major: u32, minor: u32 },
    /// A build from tmux's `master` branch.
    Master,
    /// A tmux bundled with an operating system and versioned with it, e.g. `openbsd-7.5`.
    /// These track tmux's current code, so they are treated as current.
    Platform { name: String, version: String },
}

/// The minimum release for native sessions.
const NATIVE_FLOOR: (u32, u32) = (3, 2);
/// The first release with `send-keys -K` and `list-keys -F`.
const CLIENT_KEY_COMMANDS: (u32, u32) = (3, 4);

impl TmuxVersion {
    /// Parses `tmux 3.7c`, `3.7c`, `next-3.8`, `master` or `openbsd-7.5`.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        let input = input.strip_prefix("tmux ").unwrap_or(input).trim();
        if input == "master" {
            return Some(Self::Master);
        }
        if let Some(rest) = input.strip_prefix("next-") {
            let (major, minor, _) = parse_release(rest)?;
            return Some(Self::Next { major, minor });
        }
        if let Some((major, minor, letter)) = parse_release(input) {
            return Some(Self::Release {
                major,
                minor,
                letter,
            });
        }
        let (name, version) = input.split_once('-')?;
        let is_word = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic());
        let is_version =
            |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.');
        (is_word(name) && is_version(version)).then(|| Self::Platform {
            name: name.to_owned(),
            version: version.to_owned(),
        })
    }

    /// True when the server is new enough for native sessions (3.2+).
    pub fn meets_native_floor(&self) -> bool {
        self.at_least(NATIVE_FLOOR)
    }

    /// True when the server has `send-keys -K` and `list-keys -F` (3.4+).
    pub fn supports_client_key_commands(&self) -> bool {
        self.at_least(CLIENT_KEY_COMMANDS)
    }

    fn at_least(&self, (major, minor): (u32, u32)) -> bool {
        match self {
            Self::Release {
                major: m, minor: n, ..
            }
            | Self::Next { major: m, minor: n } => (*m, *n) >= (major, minor),
            Self::Master | Self::Platform { .. } => true,
        }
    }
}

/// Parses `MAJOR.MINOR[letter][-suffix]`.
fn parse_release(input: &str) -> Option<(u32, u32, Option<char>)> {
    let input = input.split_once('-').map_or(input, |(release, _)| release);
    let (major, rest) = input.split_once('.')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (minor, suffix) = rest.split_at(digits);
    let parse = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let letter = match suffix.as_bytes() {
        [] => None,
        [letter] if letter.is_ascii_lowercase() => Some(char::from(*letter)),
        _ => return None,
    };
    Some((parse(major)?, parse(minor)?, letter))
}

#[cfg(test)]
#[path = "version_tests.rs"]
mod tests;
