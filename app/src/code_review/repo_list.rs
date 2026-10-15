//! Pure ordering, labeling and selection logic for the code review repo dropdown.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::settings::RepoListPrefs;

/// A repo that can appear in the dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListEntry {
    pub path: PathBuf,
    /// Most recent known change in the repo; `None` when unknown.
    pub recency: Option<SystemTime>,
}

/// Dropdown contents: the main list in display order, plus the collapsed hidden group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderedRepos {
    pub visible: Vec<RepoListEntry>,
    pub hidden: Vec<RepoListEntry>,
}

/// Orders `entries` as pinned (in pin order), then the rest newest-first. Hidden repos
/// go to [`OrderedRepos::hidden`]. Prefs entries with no matching repo are ignored, so a
/// pin survives its worktree being deleted and recreated.
pub fn order_repos(entries: &[RepoListEntry], prefs: &RepoListPrefs) -> OrderedRepos {
    let matches = |stored: &str, entry: &RepoListEntry| Path::new(stored) == entry.path;
    let is_hidden = |entry: &RepoListEntry| prefs.hidden.iter().any(|h| matches(h, entry));

    let mut visible: Vec<RepoListEntry> = prefs
        .pinned
        .iter()
        .filter_map(|pinned| entries.iter().find(|entry| matches(pinned, entry)))
        .filter(|entry| !is_hidden(entry))
        .cloned()
        .collect();

    let mut rest: Vec<RepoListEntry> = entries
        .iter()
        .filter(|entry| !is_hidden(entry) && !visible.contains(entry))
        .cloned()
        .collect();
    sort_by_recency(&mut rest);
    visible.extend(rest);

    let mut hidden: Vec<RepoListEntry> = entries.iter().filter(|e| is_hidden(e)).cloned().collect();
    sort_by_recency(&mut hidden);

    OrderedRepos { visible, hidden }
}

/// Newest first, unknown recency last, path as a tiebreak so the order is stable.
fn sort_by_recency(entries: &mut [RepoListEntry]) {
    entries.sort_by(|a, b| {
        (a.recency.is_none(), Reverse(a.recency), &a.path).cmp(&(
            b.recency.is_none(),
            Reverse(b.recency),
            &b.path,
        ))
    });
}

/// Display label for a repo. `main_repo_name` is the name of the main checkout when
/// `path` is a linked worktree, and `None` for a main checkout.
pub fn repo_label(path: &Path, main_repo_name: Option<&str>) -> String {
    let folder = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    match main_repo_name {
        Some(main) => format!("{main} · {folder}"),
        None => folder,
    }
}

/// Keeps the saved selection while it is still listed, otherwise falls back to the first
/// visible repo.
pub fn selection_after_change(
    saved: Option<&Path>,
    ordered_visible: &[PathBuf],
) -> Option<PathBuf> {
    saved
        .filter(|saved| ordered_visible.iter().any(|path| path == saved))
        .map(Path::to_path_buf)
        .or_else(|| ordered_visible.first().cloned())
}

#[cfg(test)]
#[path = "repo_list_tests.rs"]
mod tests;
