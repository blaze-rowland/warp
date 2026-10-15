//! Bounded discovery of git repositories and linked worktrees beneath a non-repo folder.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

const MAX_DEPTH: usize = 3;
pub(crate) const MAX_VISITED_DIRS: usize = 5_000;

const SKIPPED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "vendor",
    "Library",
    ".cache",
    ".Trash",
];

/// A git working tree found by [`discover_repos`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
    /// Canonical path of the working tree.
    pub root: PathBuf,
    /// Directory name of the main repository when this is a linked worktree.
    pub main_repo_name: Option<String>,
    /// Newest mtime among the git index, `logs/HEAD` and the top-level working-tree entries.
    pub recency: Option<SystemTime>,
}

/// Result of a bounded repository scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoDiscovery {
    /// Discovered repositories, sorted by root.
    pub repos: Vec<DiscoveredRepo>,
    /// Walked directories that are neither repositories nor inside one, sorted.
    pub visited_dirs: Vec<PathBuf>,
    /// Whether the scan stopped at the visited-directory cap.
    pub truncated: bool,
}

/// Synchronously scans `root` for git repositories and linked worktrees, within fixed depth and
/// size bounds. Returns an empty result for `$HOME`, its ancestors, filesystem roots and volume
/// roots. Performs blocking I/O, so callers should run it off the main thread.
pub fn discover_repos(root: &Path) -> RepoDiscovery {
    discover_repos_with(root, dirs::home_dir().as_deref(), MAX_VISITED_DIRS)
}

pub(crate) fn discover_repos_with(
    root: &Path,
    home: Option<&Path>,
    max_visited: usize,
) -> RepoDiscovery {
    if is_refused_root(root, home) {
        return RepoDiscovery::default();
    }
    let Ok(root) = dunce::canonicalize(root) else {
        return RepoDiscovery::default();
    };
    let home = home.map(|home| dunce::canonicalize(home).unwrap_or_else(|_| home.to_owned()));
    if is_refused_root(&root, home.as_deref()) {
        return RepoDiscovery::default();
    }

    let mut result = RepoDiscovery::default();
    let mut visited = 0;
    let mut walker = WalkDir::new(&root)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.file_type().is_dir()
                && !SKIPPED_DIRS
                    .iter()
                    .any(|skipped| entry.file_name() == *skipped)
        });

    while let Some(entry) = walker.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::debug!("Repo discovery skipped unreadable directory: {error}");
                continue;
            }
        };
        if visited == max_visited {
            result.truncated = true;
            break;
        }
        visited += 1;

        let dir = entry.path();
        match classify_dir(dir) {
            DirKind::Plain => result.visited_dirs.push(dir.to_owned()),
            DirKind::Repo(repo) => {
                result.repos.push(repo);
                walker.skip_current_dir();
            }
            DirKind::BrokenRepo => walker.skip_current_dir(),
        }
    }

    result.repos.sort_by(|a, b| a.root.cmp(&b.root));
    result.repos.dedup_by(|a, b| a.root == b.root);
    result
}

fn is_refused_root(root: &Path, home: Option<&Path>) -> bool {
    let volumes = Path::new("/Volumes");
    root.parent().is_none_or(|parent| parent == volumes)
        || root == volumes
        || home.is_some_and(|home| home.starts_with(root))
}

enum DirKind {
    Plain,
    Repo(DiscoveredRepo),
    BrokenRepo,
}

fn classify_dir(dir: &Path) -> DirKind {
    let dot_git = dir.join(".git");
    let Ok(metadata) = fs::symlink_metadata(&dot_git) else {
        return DirKind::Plain;
    };
    if metadata.is_dir() {
        if !dot_git.join("HEAD").is_file() {
            return DirKind::Plain;
        }
        return DirKind::Repo(DiscoveredRepo {
            root: dir.to_owned(),
            main_repo_name: None,
            recency: recency(dir, &dot_git),
        });
    }
    if !metadata.is_file() {
        return DirKind::Plain;
    }
    let Some(git_dir) = read_gitdir_file(dir, &dot_git) else {
        return DirKind::Plain;
    };
    if !git_dir.join("HEAD").is_file() {
        return DirKind::BrokenRepo;
    }
    let common_dir = read_common_dir(&git_dir);
    if !head_resolves(&git_dir, common_dir.as_deref().unwrap_or(&git_dir)) {
        return DirKind::BrokenRepo;
    }
    DirKind::Repo(DiscoveredRepo {
        root: dir.to_owned(),
        main_repo_name: common_dir.as_deref().and_then(main_repo_name),
        recency: recency(dir, &git_dir),
    })
}

fn read_gitdir_file(dir: &Path, dot_git: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(dot_git).ok()?;
    let gitdir = PathBuf::from(contents.trim().strip_prefix("gitdir:")?.trim());
    Some(if gitdir.is_absolute() {
        gitdir
    } else {
        dir.join(gitdir)
    })
}

/// The shared git dir of a linked worktree; `None` for other gitfile targets such as submodules.
fn read_common_dir(git_dir: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let common_dir = git_dir.join(contents.trim());
    Some(dunce::canonicalize(&common_dir).unwrap_or(common_dir))
}

fn main_repo_name(common_dir: &Path) -> Option<String> {
    let name = common_dir.file_name()?.to_str()?;
    if name == ".git" {
        return Some(common_dir.parent()?.file_name()?.to_str()?.to_owned());
    }
    Some(name.strip_suffix(".git").unwrap_or(name).to_owned())
}

fn head_resolves(git_dir: &Path, common_dir: &Path) -> bool {
    let Ok(head) = fs::read_to_string(git_dir.join("HEAD")) else {
        return false;
    };
    let head = head.trim();
    let Some(ref_name) = head.strip_prefix("ref:").map(str::trim) else {
        return head.len() >= 40 && head.chars().all(|c| c.is_ascii_hexdigit());
    };
    git_dir.join(ref_name).is_file()
        || common_dir.join(ref_name).is_file()
        || fs::read_to_string(common_dir.join("packed-refs")).is_ok_and(|packed| {
            packed
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(ref_name))
        })
}

fn recency(work_tree: &Path, git_dir: &Path) -> Option<SystemTime> {
    let top_level = fs::read_dir(work_tree)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.metadata().ok()?.modified().ok());
    [git_dir.join("index"), git_dir.join("logs/HEAD")]
        .iter()
        .filter_map(|path| fs::metadata(path).ok()?.modified().ok())
        .chain(top_level)
        .max()
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
