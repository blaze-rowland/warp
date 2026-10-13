//! Shell integration in tmux panes (U9, R7, R8, R10, KTD7, KTD8).
//!
//! A tmux pane gets Warp's blocks and input editor once its shell runs Warp's integration. Until
//! then, and whenever it runs something else, the pane is a plain terminal: keys go straight to
//! the pane and its output renders in the grid ([`pane_input_mode`]).
//!
//! The integration reaches a pane in one of three ways, and never by typing into a pane a plain
//! tmux client shares:
//!
//! - **Launcher.** Panes Warp creates start `warp-shell.sh`, which Warp installs on the tmux
//!   server's host at each attach ([`LauncherBundle`], [`InstallStep`]). It sources the
//!   marker-only bootstrap from the files beside it, so Warp writes nothing to the pane.
//! - **Typed path.** A pane that appears from elsewhere gets Warp's init-subshell command typed
//!   only when no plain client is attached, its foreground program is a known shell, and it is
//!   not on the alternate screen ([`new_pane_plan`]).
//! - **"Warpify this pane".** Everything else offers R8's action, disabled with a reason when the
//!   pane does not run a shell at a prompt ([`warpify_availability`]).
//!
//! Hooks stay validated (KTD8): a pane's session id is a keyed hash of the server's
//! `@warp-hook-token` and the pane id ([`derive_session_id`]). Warp registers only that id, so a
//! hook printed by a program (`cat` of a log) with any other id is rejected.

use sha2::{Digest, Sha256};
use warp_terminal::tmux::{CommandReply, PaneId};

use crate::terminal::model::session::SessionId;
use crate::terminal::shell::ShellType;

/// The server-wide option holding the token session ids are derived from.
pub const HOOK_TOKEN_OPTION: &str = "@warp-hook-token";
/// The pane option the launcher records its session id in.
pub const SESSION_ID_OPTION: &str = "@warp_session_id";
/// The pane option the launcher records its shell in.
pub const SHELL_OPTION: &str = "@warp_shell";
/// Where the installed launcher lives, under the tmux host's `$HOME`.
pub const INSTALL_ROOT: &str = ".cache/warp/tmux";
/// The launcher's file name in the install directory.
pub const LAUNCHER_FILE: &str = "warp-shell.sh";
/// Base64 characters per upload command. tmux has no hard line limit, but short lines keep each
/// command cheap to parse and well under any shell's argument limit.
pub const UPLOAD_CHUNK_CHARS: usize = 2048;
/// The shell command that prints the SHA-256 of its input as hex, on any host with coreutils,
/// macOS's `shasum` or OpenSSL.
const SHA256_COMMAND: &str = "{ sha256sum 2>/dev/null || shasum -a 256 2>/dev/null || openssl dgst -sha256 -r 2>/dev/null; }";
/// Hex digits of the hash that make the session id: 60 bits, so the launcher's POSIX shell
/// arithmetic (signed 64-bit) stays positive.
const SESSION_ID_HEX_DIGITS: usize = 15;

// Input mode.

/// How a tmux pane takes input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneInputMode {
    /// No Warp input editor: keys go straight to the pane and output renders in the grid, as in
    /// any terminal. Used until the pane's shell is integrated, and for programs that are not
    /// integrated shells (vim, htop, an un-Warpified shell).
    PlainTerminal,
    /// Warp's input editor and blocks.
    Warp,
}

/// How a pane takes input. A tmux pane is a plain terminal until its shell reports it is
/// integrated (Warp's `Bootstrapped` hook, which needs a registered session id). With
/// `integrate_panes` off nothing is registered, so tmux panes stay plain.
pub fn pane_input_mode(is_tmux_pane: bool, shell_integrated: bool) -> PaneInputMode {
    if is_tmux_pane && !shell_integrated {
        PaneInputMode::PlainTerminal
    } else {
        PaneInputMode::Warp
    }
}

// Session ids and the hook token.

/// A new random `@warp-hook-token`: 32 hex digits.
pub fn new_hook_token() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

/// The session id the launcher derives for `pane_id` from `token`: the first 60 bits of
/// SHA-256(`token:%N`). The launcher computes the same with `sha256sum`/`shasum`/`openssl`.
pub fn derive_session_id(token: &str, pane_id: PaneId) -> SessionId {
    let digest = Sha256::digest(format!("{token}:{pane_id}").as_bytes());
    let hex = hex::encode(digest);
    let value = u64::from_str_radix(&hex[..SESSION_ID_HEX_DIGITS], 16)
        .expect("SHA-256 hex digits parse as u64");
    SessionId::from(value)
}

/// The session id in a pane's `@warp_session_id`, when it is the one `token` derives for the
/// pane. Anything else (no option, a stale token, a forged value) does not verify.
pub fn verified_session_id(pane: &PaneFacts, token: &str) -> Option<SessionId> {
    let recorded: u64 = pane.session_id_option.as_deref()?.trim().parse().ok()?;
    let expected = derive_session_id(token, pane.pane_id);
    (recorded == expected.as_u64()).then_some(expected)
}

// What Warp asks tmux.

/// `list-panes` / `display-message` format matching [`parse_pane_facts`]. The current command
/// is last because it is the only free-text field.
pub const PANE_FACTS_FORMAT: &str =
    "#{pane_id}\t#{alternate_on}\t#{@warp_session_id}\t#{@warp_shell}\t#{pane_current_command}";

/// What Warp needs to know about a pane to decide how it gets integration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneFacts {
    pub pane_id: PaneId,
    pub alternate_on: bool,
    /// The launcher's `@warp_session_id`, unverified.
    pub session_id_option: Option<String>,
    /// The launcher's `@warp_shell`.
    pub shell_option: Option<String>,
    /// `#{pane_current_command}`.
    pub current_command: Option<String>,
}

/// Every pane of the attached session.
pub fn session_panes_query() -> String {
    format!("list-panes -s -F '{PANE_FACTS_FORMAT}'")
}

/// One pane.
pub fn pane_facts_query(pane_id: PaneId) -> String {
    format!("display-message -p -t {pane_id} '{PANE_FACTS_FORMAT}'")
}

/// Parses a [`PANE_FACTS_FORMAT`] reply. Rows that do not parse are skipped.
pub fn parse_pane_facts(reply: &CommandReply) -> Vec<PaneFacts> {
    if reply.error {
        return Vec::new();
    }
    reply
        .lines
        .iter()
        .filter_map(|line| {
            let line = String::from_utf8_lossy(line);
            let mut fields = line.splitn(5, '\t');
            let pane_id = PaneId::parse(fields.next()?)?;
            let alternate_on = fields.next()? == "1";
            let non_empty = |field: &str| (!field.is_empty()).then(|| field.to_owned());
            let session_id_option = non_empty(fields.next()?);
            let shell_option = non_empty(fields.next()?);
            let current_command = fields.next().and_then(non_empty);
            Some(PaneFacts {
                pane_id,
                alternate_on,
                session_id_option,
                shell_option,
                current_command,
            })
        })
        .collect()
}

/// Lists the clients attached to the server, control clients marked.
pub const LIST_CLIENTS_QUERY: &str = "list-clients -F '#{client_control_mode}\t#{client_name}'";

/// The names of the plain (non-control-mode) clients in a [`LIST_CLIENTS_QUERY`] reply. `None`
/// when the reply is an error, which counts as "a plain client may be attached".
pub fn plain_clients(reply: &CommandReply) -> Option<Vec<String>> {
    if reply.error {
        return None;
    }
    Some(
        reply
            .lines
            .iter()
            .filter_map(|line| {
                let line = String::from_utf8_lossy(line);
                let (control, name) = line.split_once('\t')?;
                (control != "1").then(|| clean(name))
            })
            .collect(),
    )
}

/// Sets `@warp-hook-token` to `token` unless the server already has one. `if-shell` would answer
/// with two reply blocks and break reply correlation, so a shell does the check.
pub fn ensure_token_command(token: &str) -> String {
    run_shell(&format!(
        "tmux show-options -gqv {HOOK_TOKEN_OPTION} | grep -q . \
         || tmux set-option -g {HOOK_TOKEN_OPTION} {token}"
    ))
}

/// Reads `@warp-hook-token`.
pub fn read_token_query() -> String {
    format!("show-options -gqv {HOOK_TOKEN_OPTION}")
}

/// The token in a [`read_token_query`] reply.
pub fn parse_token(reply: &CommandReply) -> Option<String> {
    if reply.error {
        return None;
    }
    let token = String::from_utf8_lossy(reply.lines.first()?)
        .trim()
        .to_owned();
    let valid = !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric());
    valid.then_some(token)
}

// R8's action.

/// Whether "Warpify this pane" can run in a pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarpifyAvailability {
    Available(ShellType),
    /// Shown disabled, with the reason.
    Unavailable(String),
}

/// "Warpify this pane" types Warp's init command, so it needs a known shell at its prompt: the
/// pane's foreground program is bash, zsh or fish and the pane is not on the alternate screen.
pub fn warpify_availability(
    current_command: Option<&str>,
    alternate_on: bool,
) -> WarpifyAvailability {
    let shell = current_command
        .and_then(ShellType::from_name)
        .filter(|shell| *shell != ShellType::PowerShell);
    match shell {
        _ if alternate_on => WarpifyAvailability::Unavailable(format!(
            "{} is running full screen",
            program_name(current_command)
        )),
        Some(shell) => WarpifyAvailability::Available(shell),
        None => WarpifyAvailability::Unavailable(format!(
            "{} is running, not a supported shell (bash, zsh or fish)",
            program_name(current_command)
        )),
    }
}

/// The confirmation shown before "Warpify this pane" types into the pane.
pub fn warpify_confirmation_text(plain_clients: Option<&[String]>) -> String {
    const TYPES: &str = "Warp will type its setup command into this pane.";
    match plain_clients {
        None => format!("{TYPES} Other tmux clients attached to this session may see it."),
        Some([]) => TYPES.to_owned(),
        Some([one]) => format!("{TYPES} The tmux client {one} is also attached and will see it."),
        Some([rest @ .., last]) => format!(
            "{TYPES} The tmux clients {} and {last} are also attached and will see it.",
            rest.join(", ")
        ),
    }
}

fn program_name(current_command: Option<&str>) -> String {
    match current_command.map(clean).filter(|name| !name.is_empty()) {
        Some(name) => name,
        None => "A program".to_owned(),
    }
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

// How each pane gets integration.

/// What Warp does for a pane that existed when it attached (KTD8, R8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExistingPanePlan {
    /// `integrate_panes` is off: a plain terminal, nothing offered.
    Off,
    /// The launcher integrated it while Warp was away: ask it to announce itself again, and
    /// blocks show from its next prompt (F1, F3).
    Reannounce(SessionId),
    /// R8's action.
    Warpify(WarpifyAvailability),
}

/// Plans a pane that existed at attach.
pub fn existing_pane_plan(
    pane: &PaneFacts,
    token: Option<&str>,
    integrate_panes: bool,
) -> ExistingPanePlan {
    if !integrate_panes {
        return ExistingPanePlan::Off;
    }
    if let Some(session_id) = token.and_then(|token| verified_session_id(pane, token)) {
        return ExistingPanePlan::Reannounce(session_id);
    }
    ExistingPanePlan::Warpify(warpify_availability(
        pane.current_command.as_deref(),
        pane.alternate_on,
    ))
}

/// What Warp does for a pane that appeared after it attached, once the pane first goes idle
/// without having integrated (KTD7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewPanePlan {
    /// `integrate_panes` is off.
    Off,
    /// The launcher (Warp's or the plugin's) is setting the pane up; nothing to do.
    Launcher,
    /// Warp is the only client and a shell waits at its prompt: type the init command.
    TypeInit(ShellType),
    /// R8's action: a plain client is attached, or the pane runs something else.
    Warpify(WarpifyAvailability),
}

/// Plans a pane that appeared after attach. `plain_clients` is `None` when the client list could
/// not be read, which counts as shared.
pub fn new_pane_plan(
    pane: &PaneFacts,
    token: Option<&str>,
    plain_clients: Option<&[String]>,
    integrate_panes: bool,
) -> NewPanePlan {
    if !integrate_panes {
        return NewPanePlan::Off;
    }
    if token.is_some_and(|token| verified_session_id(pane, token).is_some()) {
        return NewPanePlan::Launcher;
    }
    let availability = warpify_availability(pane.current_command.as_deref(), pane.alternate_on);
    match (availability, plain_clients) {
        (WarpifyAvailability::Available(shell), Some([])) => NewPanePlan::TypeInit(shell),
        (availability, _) => NewPanePlan::Warpify(availability),
    }
}

// The launcher bundle and its install.

/// One installed file: its path in the install directory and its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleFile {
    pub path: &'static str,
    pub contents: Vec<u8>,
    pub executable: bool,
}

/// The launcher and the files it sources, as installed on a tmux host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherBundle {
    files: Vec<BundleFile>,
}

impl LauncherBundle {
    pub fn new(files: Vec<BundleFile>) -> Self {
        Self { files }
    }

    pub fn files(&self) -> &[BundleFile] {
        &self.files
    }

    /// SHA-256 over every file's contents in order, as `cat` of the files would hash.
    pub fn checksum(&self) -> String {
        let mut hasher = Sha256::new();
        for file in &self.files {
            hasher.update(&file.contents);
        }
        hex::encode(hasher.finalize())
    }

    /// The install directory's name: the bundle's checksum prefix, so each Warp version installs
    /// beside the others and an install is never half-updated in place.
    pub fn version(&self) -> String {
        self.checksum()[..16].to_owned()
    }

    /// The directory under `$HOME` the bundle installs to.
    pub fn install_dir(&self) -> String {
        format!("{INSTALL_ROOT}/{}", self.version())
    }

    /// The option the install step sets to the launcher's absolute path once it verified.
    pub fn launcher_option(&self) -> String {
        format!("@warp-launcher-{}", self.version())
    }
}

/// One step of installing the launcher over the control channel. Each is one tmux command line;
/// they run in order because tmux finishes a `run-shell` before the next command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallStep {
    /// Records the launcher path if the install directory already verifies.
    Check(String),
    /// Reads the recorded launcher path. Empty means not installed.
    ReadPath(String),
}

/// The commands that check for an existing install: verify, then read the result.
pub fn install_check_commands(bundle: &LauncherBundle) -> [String; 2] {
    [
        run_shell(&verify_script(
            bundle,
            &format!("$HOME/{}", bundle.install_dir()),
        )),
        read_launcher_path_query(bundle),
    ]
}

/// The commands that upload and install the bundle, ending with the read of the result. Files
/// go up as base64 chunks into a staging directory, are decoded and verified there, and only a
/// complete, verified directory is moved into place.
pub fn install_upload_commands(bundle: &LauncherBundle, staging_suffix: &str) -> Vec<String> {
    use base64::Engine as _;

    let staging = format!("$HOME/{}.part-{staging_suffix}", bundle.install_dir());
    let mut commands = vec![run_shell(&format!(
        "mkdir -p \"{staging}\" && cd \"{staging}\" && rm -f ./*.b64"
    ))];
    for (index, file) in bundle.files().iter().enumerate() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&file.contents);
        // An empty file still gets its chunk file.
        let chunks: Vec<&str> = if encoded.is_empty() {
            vec![""]
        } else {
            encoded
                .as_bytes()
                .chunks(UPLOAD_CHUNK_CHARS)
                .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ASCII"))
                .collect()
        };
        for chunk in chunks {
            commands.push(run_shell(&format!(
                "printf %s \"{chunk}\" >> \"{staging}/{index}.b64\""
            )));
        }
    }
    commands.push(run_shell(&finish_script(bundle, &staging)));
    commands.push(read_launcher_path_query(bundle));
    commands
}

/// The launcher path in a [`InstallStep::ReadPath`] reply.
pub fn parse_launcher_path(reply: &CommandReply) -> Option<String> {
    if reply.error {
        return None;
    }
    let path = String::from_utf8_lossy(reply.lines.first()?)
        .trim()
        .to_owned();
    let usable = path.starts_with('/')
        && path.ends_with(LAUNCHER_FILE)
        && !path.chars().any(|c| c.is_control() || c == '\'');
    usable.then_some(path)
}

fn read_launcher_path_query(bundle: &LauncherBundle) -> String {
    format!("show-options -gqv {}", bundle.launcher_option())
}

/// A shell script that, if `dir` holds exactly the bundle, records the launcher's absolute path
/// in the bundle's option.
fn verify_script(bundle: &LauncherBundle, dir: &str) -> String {
    let files = bundle
        .files()
        .iter()
        .map(|file| format!("\"{}\"", file.path))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "cd \"{dir}\" 2>/dev/null && [ \"$(cat {files} 2>/dev/null | {SHA256_COMMAND} | cut -c1-64)\" = {sum} ] \
         && tmux set-option -g {option} \"$(pwd -P)/{LAUNCHER_FILE}\"",
        sum = bundle.checksum(),
        option = bundle.launcher_option(),
    )
}

/// Decodes the staged chunks into the bundle's files, verifies them, and moves the directory
/// into place. A directory another Warp installed meanwhile is kept.
fn finish_script(bundle: &LauncherBundle, staging: &str) -> String {
    let mut script = format!("cd \"{staging}\" && mkdir -p zsh");
    for (index, file) in bundle.files().iter().enumerate() {
        let path = file.path;
        script.push_str(&format!(
            " && {{ base64 -d < {index}.b64 2>/dev/null || base64 -D < {index}.b64 2>/dev/null \
             || openssl base64 -d -A < {index}.b64; }} > \"{path}\""
        ));
        if file.executable {
            script.push_str(&format!(" && chmod 755 \"{path}\""));
        }
    }
    let target = format!("$HOME/{}", bundle.install_dir());
    script.push_str(&format!(
        " && rm -f ./*.b64 && cd .. \
         && {{ [ -e \"{target}\" ] || mv \"{staging}\" \"{target}\"; }}; rm -rf \"{staging}\"; {}",
        verify_script(bundle, &target)
    ));
    script
}

/// A `run-shell` of `script` with all output dropped: in control mode tmux writes a
/// `run-shell`'s output outside any reply block. It always succeeds, so a failed step is read
/// from the option it did not set rather than from an error reply. tmux expands formats in the
/// command, so the script must not contain `#`, and it is single-quoted, so it must not contain
/// `'`.
fn run_shell(script: &str) -> String {
    debug_assert!(!script.contains('#') && !script.contains('\''));
    format!("run-shell '{{ {script}; }} >/dev/null 2>&1 || true'")
}

/// Where launchers look for a request to announce themselves again, under the host's `$HOME`.
/// It is beside the versioned install directories, so a pane started by an older Warp's
/// launcher still finds it.
pub const ANNOUNCE_DIR: &str = ".cache/warp/tmux/announce";

/// Asks the launcher in each of `session_ids`' panes to announce itself again at its next prompt
/// (F1, F3): it sends `InitShell` and `Bootstrapped` again, now that Warp is there to read them.
pub fn reannounce_command(session_ids: &[SessionId]) -> Option<String> {
    if session_ids.is_empty() {
        return None;
    }
    let files = session_ids
        .iter()
        .map(|id| format!("\"$HOME/{ANNOUNCE_DIR}/{}\"", id.as_u64()))
        .collect::<Vec<_>>()
        .join(" ");
    Some(run_shell(&format!(
        "mkdir -p \"$HOME/{ANNOUNCE_DIR}\" && chmod 700 \"$HOME/{ANNOUNCE_DIR}\" && touch {files}"
    )))
}

/// The tmux shell-command that starts a pane through the launcher.
pub fn launcher_shell_command(launcher_path: &str) -> String {
    format!("'{launcher_path}'")
}

/// The launcher bundle built from Warp's assets: the launcher, its per-shell entry files, and
/// Warp's full bootstrap for each shell (in marker-only mode at run time).
pub fn launcher_bundle(assets: &dyn warpui::AssetProvider) -> LauncherBundle {
    let asset = |path: &str| -> Vec<u8> {
        assets
            .get(&format!("bundled/tmux/{path}"))
            .unwrap_or_else(|_| panic!("missing tmux launcher asset {path}"))
            .into_owned()
    };
    let bootstrap =
        |shell| crate::terminal::bootstrap::script_for_shell(shell, assets).into_owned();
    LauncherBundle::new(vec![
        BundleFile {
            path: LAUNCHER_FILE,
            contents: asset("warp-shell.sh"),
            executable: true,
        },
        BundleFile {
            path: "zsh/.zshenv",
            contents: asset("zshenv"),
            executable: false,
        },
        BundleFile {
            path: "init.zsh",
            contents: asset("init.zsh"),
            executable: false,
        },
        BundleFile {
            path: "init.bash",
            contents: asset("init.bash"),
            executable: false,
        },
        BundleFile {
            path: "init.fish",
            contents: asset("init.fish"),
            executable: false,
        },
        BundleFile {
            path: "bootstrap.zsh",
            contents: bootstrap(ShellType::Zsh),
            executable: false,
        },
        BundleFile {
            path: "bootstrap.bash",
            contents: bootstrap(ShellType::Bash),
            executable: false,
        },
        BundleFile {
            path: "bootstrap.fish",
            contents: bootstrap(ShellType::Fish),
            executable: false,
        },
    ])
}

/// Installs `bundle` under `home` on this machine, for sessions Warp starts locally before it is
/// attached (`new-session`). Returns the launcher's path once the directory verifies.
#[cfg(not(target_family = "wasm"))]
pub fn install_locally(
    bundle: &LauncherBundle,
    home: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    use std::fs;
    use std::io::{Error, ErrorKind};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    let dir = home.join(bundle.install_dir());
    if local_install_verifies(bundle, &dir) {
        return Ok(dir.join(LAUNCHER_FILE));
    }
    let staging = home.join(format!(
        "{}.part-{}",
        bundle.install_dir(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&staging);
    for file in bundle.files() {
        let path = staging.join(file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &file.contents)?;
        #[cfg(unix)]
        if file.executable {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    if !dir.exists() {
        fs::rename(&staging, &dir)?;
    }
    let _ = fs::remove_dir_all(&staging);
    if local_install_verifies(bundle, &dir) {
        Ok(dir.join(LAUNCHER_FILE))
    } else {
        Err(Error::new(
            ErrorKind::InvalidData,
            "the tmux launcher install does not match this Warp's bundle",
        ))
    }
}

#[cfg(not(target_family = "wasm"))]
fn local_install_verifies(bundle: &LauncherBundle, dir: &std::path::Path) -> bool {
    let mut hasher = Sha256::new();
    for file in bundle.files() {
        match std::fs::read(dir.join(file.path)) {
            Ok(contents) => hasher.update(&contents),
            Err(_) => return false,
        }
    }
    hex::encode(hasher.finalize()) == bundle.checksum()
}

#[cfg(feature = "local_tty")]
mod app_model;
#[cfg(feature = "local_tty")]
pub use app_model::*;

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;
