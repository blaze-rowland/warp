//! Pure ordering, labeling and selection logic for the code review repo dropdown.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::pane_group::RepoDiscoveryState;
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
pub fn selection_after_change<T: PartialEq + Clone>(
    saved: Option<&T>,
    ordered_visible: &[T],
) -> Option<T> {
    saved
        .filter(|saved| ordered_visible.contains(saved))
        .cloned()
        .or_else(|| ordered_visible.first().cloned())
}

/// Combines the pin/hide prefs of every discovery parent into one, keeping only entries for
/// `discovered` repos so a repo a pane sits inside is never pinned or hidden.
pub fn merge_prefs<'a>(
    per_parent: impl IntoIterator<Item = &'a RepoListPrefs>,
    discovered: &[&Path],
) -> RepoListPrefs {
    let is_discovered = |stored: &String| discovered.iter().any(|path| Path::new(stored) == *path);
    let mut merged = RepoListPrefs::default();
    for prefs in per_parent {
        merged
            .pinned
            .extend(prefs.pinned.iter().filter(|p| is_discovered(p)).cloned());
        merged
            .hidden
            .extend(prefs.hidden.iter().filter(|p| is_discovered(p)).cloned());
    }
    merged
}

/// Keeps still-visible repos in the `frozen` order captured when the dropdown opened, so rows
/// don't move under the pointer. Repos that became visible since go after them, in `ordered`'s
/// order.
pub fn keep_frozen_order(ordered: OrderedRepos, frozen: &[PathBuf]) -> OrderedRepos {
    let (mut kept, added): (Vec<RepoListEntry>, Vec<RepoListEntry>) = ordered
        .visible
        .into_iter()
        .partition(|entry| frozen.contains(&entry.path));
    kept.sort_by_key(|entry| frozen.iter().position(|path| *path == entry.path));
    kept.extend(added);
    OrderedRepos {
        visible: kept,
        hidden: ordered.hidden,
    }
}

/// What the code review panel shows for a tab's repo list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoListState {
    /// The first discovery walk is still running and nothing is listed yet.
    Scanning,
    /// No repos were found.
    Empty,
    /// Every repo is hidden.
    AllHidden,
    Listed {
        /// A walk stopped at its size bound, so the list may be incomplete.
        truncated: bool,
    },
}

pub fn repo_list_state(ordered: &OrderedRepos, discovery: RepoDiscoveryState) -> RepoListState {
    if !ordered.visible.is_empty() {
        RepoListState::Listed {
            truncated: discovery.truncated,
        }
    } else if !ordered.hidden.is_empty() {
        RepoListState::AllHidden
    } else if discovery.scanning {
        RepoListState::Scanning
    } else {
        RepoListState::Empty
    }
}

#[cfg(test)]
#[path = "repo_list_tests.rs"]
mod tests;
