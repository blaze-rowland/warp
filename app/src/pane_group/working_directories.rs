use std::collections::HashMap;
#[cfg(feature = "local_fs")]
use std::collections::{BTreeMap, HashSet};
#[cfg(feature = "local_fs")]
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

#[cfg(feature = "local_fs")]
use indexmap::IndexSet;
#[cfg(feature = "local_fs")]
use notify_debouncer_full::notify::{RecursiveMode, WatchFilter};
#[cfg(feature = "local_fs")]
use remote_server::manager::RemoteServerManager;
#[cfg(feature = "local_fs")]
use repo_metadata::discovery::{RepoDiscovery, discover_repos};
#[cfg(feature = "local_fs")]
use repo_metadata::repositories::DetectedRepositories;
use warp_core::SessionId;
#[cfg(feature = "local_fs")]
use warp_errors::report_error;
#[cfg(feature = "local_fs")]
use warp_util::remote_path::RemotePath;
#[cfg(all(feature = "local_fs", test))]
use warpui::r#async::FutureId;
#[cfg(feature = "local_fs")]
use warpui::r#async::SpawnedFutureHandle;
#[cfg(feature = "local_fs")]
use warpui::{AppContext, SingletonEntity as _};
use warpui::{Entity, EntityId, ModelContext, ModelHandle, ViewHandle};
#[cfg(feature = "local_fs")]
use watcher::{BulkFilesystemWatcher, BulkFilesystemWatcherEvent};

use crate::code::buffer_location::LocalOrRemotePath;
#[cfg(feature = "local_fs")]
use crate::code::file_tree::FileTreeView;
use crate::code_review::code_review_view::CodeReviewView;
use crate::code_review::comments::{
    AttachedReviewComment, PendingImportedReviewComment, ReviewCommentBatch,
};
use crate::code_review::diff_state::{DiffMode, DiffStateModel};
#[cfg(feature = "local_fs")]
use crate::features::FeatureFlag;
use crate::workspace::view::global_search::view::GlobalSearchView;

#[cfg(all(feature = "local_fs", not(test)))]
const DISCOVERY_WATCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);
/// Caps the parent-folder watches held per discovery so a truncated scan cannot register
/// thousands of watches; the shallowest folders are kept.
#[cfg(feature = "local_fs")]
const MAX_DISCOVERY_WATCHES: usize = 256;

/// Type-safe wrapper around the map of `LocalOrRemotePath` → `DiffStateModel`.
///
/// Enforces that local keys are always paired with local-backend models and
/// remote keys with remote-backend models via dedicated insertion methods.
#[cfg(feature = "local_fs")]
#[derive(Default)]
struct DiffStateModelMap {
    models: HashMap<LocalOrRemotePath, ModelHandle<DiffStateModel>>,
}

#[cfg(feature = "local_fs")]
impl DiffStateModelMap {
    fn get(&self, key: &LocalOrRemotePath) -> Option<&ModelHandle<DiffStateModel>> {
        self.models.get(key)
    }

    /// Insert a model that was created from a `LocalOrRemotePath::Local` key.
    fn insert_local(
        &mut self,
        path: PathBuf,
        model: ModelHandle<DiffStateModel>,
        ctx: &AppContext,
    ) {
        debug_assert!(
            matches!(model.as_ref(ctx), DiffStateModel::Local(_)),
            "insert_local called with a remote-backend DiffStateModel",
        );
        self.models.insert(LocalOrRemotePath::Local(path), model);
    }

    /// Insert a model that was created from a `LocalOrRemotePath::Remote` key.
    fn insert_remote(
        &mut self,
        remote_id: RemotePath,
        model: ModelHandle<DiffStateModel>,
        ctx: &AppContext,
    ) {
        debug_assert!(
            matches!(model.as_ref(ctx), DiffStateModel::Remote(_)),
            "insert_remote called with a local-backend DiffStateModel",
        );
        self.models
            .insert(LocalOrRemotePath::Remote(remote_id), model);
    }

    fn remove(&mut self, key: &LocalOrRemotePath) -> Option<ModelHandle<DiffStateModel>> {
        self.models.remove(key)
    }
}

/// Bidirectional map of pane groups to the repository roots they reference.
///
/// Maintains both a forward map (`pane_group_id -> ordered set of repo paths`)
/// and a reverse map (`repo path -> set of pane group ids that reference it`)
/// in lockstep, so callers can answer "is this repo still referenced by any
/// pane group?" in O(1) without scanning every pane group's set.
///
/// All mutations go through methods on this wrapper to guarantee the two
/// maps stay in sync.
#[cfg(feature = "local_fs")]
#[derive(Default)]
struct PaneGroupRepositoryRoots {
    /// Forward: per-pane-group ordered set of repository roots.
    /// IndexSet maintains insertion order so most recently added repos appear later.
    pane_group_to_paths: HashMap<EntityId, IndexSet<LocalOrRemotePath>>,
    /// Reverse: which pane groups reference each repo path.
    /// Maintained in lockstep with `pane_group_to_paths`.
    path_to_pane_groups: HashMap<LocalOrRemotePath, HashSet<EntityId>>,
}

#[cfg(feature = "local_fs")]
impl PaneGroupRepositoryRoots {
    /// Read-only view of a pane group's repository roots, preserving the
    /// existing `HashMap::get(&pane_group_id)` semantics.
    fn get(&self, pane_group_id: EntityId) -> Option<&IndexSet<LocalOrRemotePath>> {
        self.pane_group_to_paths.get(&pane_group_id)
    }

    /// Insert a single repo for a pane group. Returns `true` if it was newly
    /// added to the pane group (matching `IndexSet::insert` semantics).
    ///
    /// Always keeps the reverse map in sync: if the path was newly added to
    /// the pane group, the pane group is added to the path's reverse entry.
    fn insert(&mut self, pane_group_id: EntityId, path: LocalOrRemotePath) -> bool {
        let added = self
            .pane_group_to_paths
            .entry(pane_group_id)
            .or_default()
            .insert(path.clone());

        if added {
            self.path_to_pane_groups
                .entry(path)
                .or_default()
                .insert(pane_group_id);
        }

        added
    }

    /// Set the full list of repository roots for a pane group,
    /// preserving the insertion-order of the previous set.
    ///
    /// Returns the paths that left this pane group's set AND no longer have
    /// any other pane group referencing them — i.e. the paths whose shared
    /// `DiffStateModel` is now safe to drop. Any paths that were already referenced
    /// by other pane groups are kept in their original order.
    fn set_paths(
        &mut self,
        pane_group_id: EntityId,
        new_paths: impl IntoIterator<Item = LocalOrRemotePath>,
    ) -> Vec<LocalOrRemotePath> {
        let new_paths: Vec<LocalOrRemotePath> = new_paths.into_iter().collect();
        let new_set: HashSet<&LocalOrRemotePath> = new_paths.iter().collect();

        // Update the forward map and capture which paths left this pane group
        // (`removed`) and which were newly inserted into it (`newly_added`).
        // Tracking `newly_added` separately lets us skip redundant reverse-map
        // updates for paths the pane group already referenced.
        let (removed, newly_added): (Vec<LocalOrRemotePath>, Vec<LocalOrRemotePath>) = {
            let forward = self.pane_group_to_paths.entry(pane_group_id).or_default();
            let removed: Vec<LocalOrRemotePath> = forward
                .iter()
                .filter(|item| !new_set.contains(*item))
                .cloned()
                .collect();
            forward.retain(|item| new_set.contains(item));
            let mut newly_added: Vec<LocalOrRemotePath> = Vec::new();
            for item in &new_paths {
                if forward.insert(item.clone()) {
                    newly_added.push(item.clone());
                }
            }
            (removed, newly_added)
        };

        // Add pane_group_id only for paths that are newly referenced by this
        // pane group; paths it already referenced are already in the reverse
        // entry by the invariant maintained on every mutation.
        for path in newly_added {
            self.path_to_pane_groups
                .entry(path)
                .or_default()
                .insert(pane_group_id);
        }

        // Drop pane_group_id from the reverse map for paths it no longer
        // references; collect the paths whose reverse entry became empty.
        removed
            .into_iter()
            .filter(|path| self.remove_path(pane_group_id, path))
            .collect()
    }

    /// Drop all entries for a pane group (used when a tab is closed or the
    /// pane group becomes empty). Returns `Some(orphans)` when the pane group
    /// had a `repository_roots` entry, where `orphans` are the paths that no
    /// longer have any pane group referencing them. Returns `None` when the
    /// pane group was not tracked, so callers can distinguish "present with
    /// no orphans" from "not present" in a single call.
    fn remove_pane_group(&mut self, pane_group_id: EntityId) -> Option<Vec<LocalOrRemotePath>> {
        let paths = self.pane_group_to_paths.remove(&pane_group_id)?;
        Some(
            paths
                .into_iter()
                .filter(|path| self.remove_path(pane_group_id, path))
                .collect(),
        )
    }

    /// Remove `pane_group_id` from the reverse-map entry for `path`.
    /// Returns `true` if removing this reference left `path` with no pane groups
    /// referencing it — i.e. the path is now globally orphaned.
    ///
    /// This only mutates the reverse map; callers are responsible for
    /// removing `path` from `pane_group_id`'s forward entry before (or
    /// after) calling this.
    fn remove_path(&mut self, pane_group_id: EntityId, path: &LocalOrRemotePath) -> bool {
        let became_empty = self.path_to_pane_groups.get_mut(path).is_some_and(|set| {
            set.remove(&pane_group_id);
            set.is_empty()
        });
        if became_empty {
            self.path_to_pane_groups.remove(path);
        }
        became_empty
    }
}

/// Where a repo found beneath a non-repo working directory came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredRepoInfo {
    /// The canonical working directory the repo was discovered under.
    pub parent: PathBuf,
    /// Directory name of the main repository when the repo is a linked worktree.
    pub main_repo_name: Option<String>,
    /// Per-worktree git dir when it lives outside the working tree, as for a linked worktree.
    pub external_git_dir: Option<PathBuf>,
    /// Last-change time read during the scan.
    pub recency: Option<SystemTime>,
}

/// Progress of repo discovery for a pane group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepoDiscoveryState {
    /// Whether any discovery scan for the pane group has not finished yet.
    pub scanning: bool,
    /// Whether any scan stopped at its size bound, so the list may be incomplete.
    pub truncated: bool,
}

/// Discovery state for one non-repo working directory.
#[cfg(feature = "local_fs")]
struct DiscoveryEntry {
    /// The terminal whose cwd this is; discovered repos map to it.
    terminal_id: EntityId,
    result: Option<RepoDiscovery>,
    scan: Option<SpawnedFutureHandle>,
    rescan_requested: bool,
}

#[cfg(feature = "local_fs")]
impl DiscoveryEntry {
    fn watched_dirs(&self) -> Vec<PathBuf> {
        self.result
            .as_ref()
            .map(discovery_watch_dirs)
            .unwrap_or_default()
    }
}

/// Repo discovery for one pane group, keyed by canonical cwd.
#[cfg(feature = "local_fs")]
#[derive(Default)]
struct PaneGroupDiscovery {
    entries: BTreeMap<PathBuf, DiscoveryEntry>,
    /// Repo roots and their terminals from the latest refresh, before discovered roots join them.
    cwd_repo_roots: Vec<LocalOrRemotePath>,
    cwd_root_to_terminal: HashMap<LocalOrRemotePath, EntityId>,
    /// Roots that only discovery produced in this pane group.
    discovered_roots: HashSet<PathBuf>,
}

#[cfg(feature = "local_fs")]
impl PaneGroupDiscovery {
    fn state(&self) -> RepoDiscoveryState {
        RepoDiscoveryState {
            scanning: self
                .entries
                .values()
                .any(|entry| entry.scan.is_some() || entry.result.is_none()),
            truncated: self
                .entries
                .values()
                .any(|entry| entry.result.as_ref().is_some_and(|result| result.truncated)),
        }
    }

    /// Appends discovered roots that no cwd produced, mapping each to the terminal whose cwd
    /// discovered it.
    fn chain_discovered_roots(
        &mut self,
        repo_roots: &mut Vec<LocalOrRemotePath>,
        root_to_terminal: &mut HashMap<LocalOrRemotePath, EntityId>,
    ) {
        self.discovered_roots.clear();
        let cwd_roots: HashSet<LocalOrRemotePath> = repo_roots.iter().cloned().collect();
        for entry in self.entries.values() {
            for repo in entry.result.iter().flat_map(|result| &result.repos) {
                let key = LocalOrRemotePath::Local(repo.root.clone());
                if cwd_roots.contains(&key) {
                    continue;
                }
                if self.discovered_roots.insert(repo.root.clone()) {
                    repo_roots.push(key.clone());
                }
                root_to_terminal.entry(key).or_insert(entry.terminal_id);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDirectory {
    pub path: LocalOrRemotePath,
    pub terminal_id: Option<EntityId>,
}

/// Events emitted when the set of working directories changes
#[derive(Clone, Debug)]
pub enum WorkingDirectoriesEvent {
    /// The set of working directories has changed for a specific pane group.
    DirectoriesChanged {
        /// The PaneGroup whose directories changed
        pane_group_id: EntityId,
        /// All active directories (deduplicated) in most to least recently added order.
        directories: Vec<WorkingDirectory>,
    },
    /// The set of repositories has changed for a specific pane group.
    RepositoriesChanged {
        /// The PaneGroup whose repositories changed
        pane_group_id: EntityId,
        /// All active repository roots (deduplicated) in most to least recently added order.
        repositories: Vec<LocalOrRemotePath>,
    },
    /// The focused repository changed for a specific pane group.
    /// This fires when the user focuses a different pane or CDs within the focused pane.
    FocusedRepoChanged {
        /// The PaneGroup whose focused repo changed
        pane_group_id: EntityId,
        /// All active repository-terminal ID pairs (deduplicated)
        repository_terminal_map: HashMap<LocalOrRemotePath, EntityId>,
        /// The repository path of the focused terminal, if any
        focused_repo: Option<LocalOrRemotePath>,
    },
}

#[derive(Default)]
#[cfg(feature = "local_fs")]
/// Workspace model that tracks working directories across all pane groups.
/// Emits events when the set of directories changes for any pane group.
pub struct WorkingDirectoriesModel {
    /// Per-pane-group tracking of active directories (both local and remote) as a
    /// deduplicated, ordered set.
    ///
    /// This stores the *display roots* for the left panel (file tree / global search),
    /// not the raw working directories reported by each pane.
    ///
    /// Concretely, for each pane group's active paths we store:
    /// - the detected repository root when the path belongs to a repo
    /// - otherwise, the normalized path itself (local) or the remote CWD/editor path
    ///
    /// IndexSet maintains insertion order - most recently added directories appear later.
    pane_groups: HashMap<EntityId, IndexSet<LocalOrRemotePath>>,
    /// Per-pane-group tracking of active repository roots as a deduplicated, ordered set.
    /// Covers both local and remote repositories in a single map.
    repository_roots: PaneGroupRepositoryRoots,
    /// Per-pane-group mapping from root paths to a matching terminal view ID.
    /// This allows looking up which terminal is associated with each root path.
    /// Note, a single root path can be associated with multiple terminals.
    /// we're just storing an arbitrary terminal ID for each root path.
    directory_to_terminal: HashMap<EntityId, HashMap<LocalOrRemotePath, EntityId>>,
    /// Global mapping from repository keys to their DiffStateModel.
    /// Since git state is inherently tied to a repository (not a pane group),
    /// this is stored globally and shared across all pane groups viewing the same repo.
    diff_state_models: DiffStateModelMap,
    /// Global mapping from repository locations to their CommentBatch.
    /// Like the DiffStateModel mapping, comments are inherently tied to git diffs
    /// and are shared across all pane groups viewing the same repo.
    comment_models: HashMap<LocalOrRemotePath, ModelHandle<ReviewCommentBatch>>,
    /// Per-pane-group mapping from repository root locations to their CodeReviewView.
    /// This allows reusing code review views across multiple requests for the same repo.
    code_review_views: HashMap<EntityId, HashMap<LocalOrRemotePath, ViewHandle<CodeReviewView>>>,
    /// Per-pane-group tracking of the focused repository root path.
    focused_repo: HashMap<EntityId, Option<LocalOrRemotePath>>,
    /// Per-pane-group tracking of the repository the user has manually selected for the
    /// code review (right) panel. This is the repo that should be restored when the user
    /// leaves the pane group's session and returns to it later, even if the auto-selection
    /// logic would otherwise pick a different default.
    selected_review_repo: HashMap<EntityId, LocalOrRemotePath>,
    global_search_views: HashMap<EntityId, ViewHandle<GlobalSearchView>>,
    file_tree_views: HashMap<EntityId, ViewHandle<FileTreeView>>,
    /// Per-pane-group discovery of repos beneath non-repo working directories.
    repo_discovery: HashMap<EntityId, PaneGroupDiscovery>,
    /// Non-recursive watches on the folders discovery walked, created on first use.
    discovery_watcher: Option<ModelHandle<BulkFilesystemWatcher>>,
    /// Number of discovery entries holding a watch on each folder.
    discovery_watch_counts: HashMap<PathBuf, usize>,
    #[cfg(test)]
    discovery_scan_futures: Vec<FutureId>,
}

#[derive(Default)]
#[cfg(not(feature = "local_fs"))]
/// Does nothing without a local file system
pub struct WorkingDirectoriesModel {}

/// Index Sets are ordered by insertion order. This function updates an index set to match a new set of items.
#[cfg(feature = "local_fs")]
pub fn update_index_set(
    index_set: &mut IndexSet<LocalOrRemotePath>,
    new_items: impl IntoIterator<Item = LocalOrRemotePath>,
) {
    let new_items: Vec<LocalOrRemotePath> = new_items.into_iter().collect();
    index_set.retain(|item| new_items.iter().any(|new_item| new_item == item));
    for item in new_items {
        index_set.insert(item);
    }
}

#[cfg(feature = "local_fs")]
impl WorkingDirectoriesModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the unique directories for a specific pane group in insertion order (oldest first).
    fn least_recent_directories_for_pane_group(
        &self,
        pane_group_id: EntityId,
    ) -> Option<&IndexSet<LocalOrRemotePath>> {
        self.pane_groups.get(&pane_group_id)
    }

    /// Get the unique directories for a specific pane group in most to least recently added order.
    pub fn most_recent_directories_for_pane_group(
        &self,
        pane_group_id: EntityId,
    ) -> Option<impl Iterator<Item = WorkingDirectory> + '_> {
        self.least_recent_directories_for_pane_group(pane_group_id)
            .map(move |dirs| {
                dirs.iter().rev().map(move |lor| WorkingDirectory {
                    path: lor.clone(),
                    terminal_id: self.get_terminal_id_for_root_path(pane_group_id, lor),
                })
            })
    }

    /// Get the unique repository roots for a specific pane group in insertion order (oldest first).
    fn least_recent_repositories_for_pane_group(
        &self,
        pane_group_id: EntityId,
    ) -> Option<&IndexSet<LocalOrRemotePath>> {
        self.repository_roots.get(pane_group_id)
    }

    /// Get the unique repository roots for a specific pane group in most to least recently added order.
    pub fn most_recent_repositories_for_pane_group(
        &self,
        pane_group_id: EntityId,
    ) -> Option<impl Iterator<Item = LocalOrRemotePath> + '_> {
        self.least_recent_repositories_for_pane_group(pane_group_id)
            .map(|repos| repos.iter().rev().cloned())
    }

    /// Get the terminal view ID associated with a specific root path in a pane group.
    pub fn get_terminal_id_for_root_path(
        &self,
        pane_group_id: EntityId,
        root_path: &LocalOrRemotePath,
    ) -> Option<EntityId> {
        self.directory_to_terminal
            .get(&pane_group_id)
            .and_then(|roots| roots.get(root_path).copied())
    }

    /// Get or create a DiffStateModel for a specific repository.
    ///
    /// If the model doesn't exist, it will be created. For remote
    /// repositories we require a connected session for the host; returns
    /// `None` when none exists so callers treat the panel as unavailable
    /// for that repo rather than producing a model that cannot subscribe.
    pub fn get_or_create_diff_state_model(
        &mut self,
        key: LocalOrRemotePath,
        preferred_session: Option<SessionId>,
        ctx: &mut ModelContext<Self>,
    ) -> Option<ModelHandle<DiffStateModel>> {
        if let Some(model) = self.diff_state_models.get(&key) {
            return Some(model.clone());
        }

        let diff_state_model = match &key {
            LocalOrRemotePath::Local(path) => {
                let path = path.clone();
                if self.is_discovered_only_repo(&path) {
                    ctx.add_model(|ctx| DiffStateModel::new_local_for_discovered_repo(path, ctx))
                } else {
                    ctx.add_model(|ctx| DiffStateModel::new_local(path, ctx))
                }
            }
            LocalOrRemotePath::Remote(remote_path) => {
                let mgr_handle = RemoteServerManager::handle(ctx);
                mgr_handle
                    .as_ref(ctx)
                    .client_for_host(&remote_path.host_id)?;
                let remote_path = remote_path.clone();
                ctx.add_model(|ctx| DiffStateModel::new_remote(remote_path, preferred_session, ctx))
            }
        };

        match key {
            LocalOrRemotePath::Local(path) => {
                self.diff_state_models
                    .insert_local(path, diff_state_model.clone(), ctx);
            }
            LocalOrRemotePath::Remote(remote_id) => {
                self.diff_state_models
                    .insert_remote(remote_id, diff_state_model.clone(), ctx);
            }
        }

        Some(diff_state_model)
    }

    /// Drops diff state models for repos that are no longer referenced by any
    /// pane group. The input must already be pre-filtered to orphans, so this
    /// method stops the watcher and removes stale model and view cache entries.
    fn drop_unused_diff_state_models(
        &mut self,
        orphaned_repos: impl IntoIterator<Item = LocalOrRemotePath>,
        ctx: &mut ModelContext<Self>,
    ) {
        for repo_key in orphaned_repos {
            if let Some(model) = self.diff_state_models.remove(&repo_key) {
                model.update(ctx, |model, ctx| {
                    model.stop_active_watcher(ctx);
                });
            }
            for views in self.code_review_views.values_mut() {
                views.remove(&repo_key);
            }
        }
    }

    /// Get or create a ReviewCommentBatch for a specific repository.
    /// If the model doesn't exist, it will be created.
    pub fn get_or_create_code_review_comments(
        &mut self,
        repo_path: &LocalOrRemotePath,
        ctx: &mut ModelContext<Self>,
    ) -> Option<ModelHandle<ReviewCommentBatch>> {
        if let Some(existing) = self.comment_models.get(repo_path) {
            return Some(existing.clone());
        }
        let model = ctx.add_model(|_ctx| ReviewCommentBatch::default());
        self.comment_models.insert(repo_path.clone(), model.clone());
        Some(model)
    }

    /// Store a CodeReviewView for a specific repository in a pane group.
    pub fn store_code_review_view(
        &mut self,
        pane_group_id: EntityId,
        repo_path: LocalOrRemotePath,
        view: ViewHandle<CodeReviewView>,
    ) {
        let pane_group_views = self.code_review_views.entry(pane_group_id).or_default();
        pane_group_views.insert(repo_path, view);

        // Remove any inactive code reviews here. This allows these to be garbage collected.
        self.remove_inactive_code_reviews(pane_group_id);
    }

    /// Remove any code review view state that is not active in any of the terminal views that belong to this pane group.
    fn remove_inactive_code_reviews(&mut self, pane_group_id: EntityId) {
        let Some(code_review_views) = self.code_review_views.get_mut(&pane_group_id) else {
            return;
        };

        let Some(terminal_mapping) = self.directory_to_terminal.get(&pane_group_id) else {
            return;
        };

        code_review_views.retain(|path, _| terminal_mapping.contains_key(path));
    }

    /// Get an existing CodeReviewView for a specific repository in a pane group.
    /// Returns None if no view exists for this combination.
    pub fn get_code_review_view(
        &self,
        pane_group_id: EntityId,
        repo_path: &LocalOrRemotePath,
    ) -> Option<ViewHandle<CodeReviewView>> {
        self.code_review_views
            .get(&pane_group_id)
            .and_then(|pane_group_views| pane_group_views.get(repo_path))
            .cloned()
    }

    /// Get the repository path the user has manually selected for the code review
    /// panel in a given pane group, if any. Used to restore the selection when the
    /// user navigates back to the pane group's session.
    pub fn get_selected_review_repo(&self, pane_group_id: EntityId) -> Option<&LocalOrRemotePath> {
        self.selected_review_repo.get(&pane_group_id)
    }

    /// Persist the repository the user manually selected for the code review panel
    /// in a given pane group. This is only called for explicit user-driven
    /// selections (e.g. via the dropdown), not for auto-selected defaults.
    pub fn set_selected_review_repo(
        &mut self,
        pane_group_id: EntityId,
        repo_path: LocalOrRemotePath,
    ) {
        self.selected_review_repo.insert(pane_group_id, repo_path);
    }

    /// Clear the saved code review panel selection for a pane group.
    pub fn clear_selected_review_repo(&mut self, pane_group_id: EntityId) {
        self.selected_review_repo.remove(&pane_group_id);
    }

    pub fn store_global_search_view(
        &mut self,
        pane_group_id: EntityId,
        view: ViewHandle<GlobalSearchView>,
    ) {
        self.global_search_views.insert(pane_group_id, view);
    }

    pub fn get_global_search_view(
        &self,
        pane_group_id: EntityId,
    ) -> Option<ViewHandle<GlobalSearchView>> {
        self.global_search_views.get(&pane_group_id).cloned()
    }

    pub fn store_file_tree_view(
        &mut self,
        pane_group_id: EntityId,
        view: ViewHandle<FileTreeView>,
    ) {
        self.file_tree_views.insert(pane_group_id, view);
    }

    pub fn get_file_tree_view(&self, pane_group_id: EntityId) -> Option<ViewHandle<FileTreeView>> {
        self.file_tree_views.get(&pane_group_id).cloned()
    }

    /// Permanently removes all state associated with a pane group.
    /// This should be called when a tab is closed (pane group is destroyed),
    /// as opposed to handle_empty_pane_group which is called when working directories
    /// become empty but the pane group still exists (e.g., settings page).
    pub fn remove_pane_group(&mut self, pane_group_id: EntityId, ctx: &mut ModelContext<Self>) {
        // Clean up directories, terminals, and repos (emits events for subscribers)
        self.handle_empty_pane_group(pane_group_id, ctx);

        // Clean up views that should persist in handle_empty_pane_group e.g. there's only a settings pane in the pane group
        // but need to be removed when the pane group is destroyed
        self.global_search_views.remove(&pane_group_id);
        self.file_tree_views.remove(&pane_group_id);
        self.code_review_views.remove(&pane_group_id);
        self.focused_repo.remove(&pane_group_id);
        self.selected_review_repo.remove(&pane_group_id);
    }

    fn handle_empty_pane_group(&mut self, pane_group_id: EntityId, ctx: &mut ModelContext<Self>) {
        if let Some(discovery) = self.repo_discovery.remove(&pane_group_id) {
            for entry in discovery.entries.into_values() {
                self.release_discovery_entry(entry, ctx);
            }
        }
        let did_remove_dirs = self.pane_groups.remove(&pane_group_id).is_some();
        let did_remove_terminals = self.directory_to_terminal.remove(&pane_group_id).is_some();
        let orphaned_repos = self.repository_roots.remove_pane_group(pane_group_id);
        let did_remove_repos = orphaned_repos.is_some();

        if let Some(orphaned_repos) = orphaned_repos {
            self.drop_unused_diff_state_models(orphaned_repos, ctx);
        }

        if did_remove_dirs {
            ctx.emit(WorkingDirectoriesEvent::DirectoriesChanged {
                pane_group_id,
                directories: vec![],
            });
        }
        if did_remove_repos {
            ctx.emit(WorkingDirectoriesEvent::RepositoriesChanged {
                pane_group_id,
                repositories: vec![],
            });
        }
        if did_remove_terminals {
            ctx.emit(WorkingDirectoriesEvent::FocusedRepoChanged {
                pane_group_id,
                repository_terminal_map: HashMap::new(),
                focused_repo: None,
            });
        }
    }

    /// Refreshes the working directories for a pane group from terminal CWDs
    /// (both local and remote) and code editor paths.
    ///
    /// If `focused_terminal_id` is provided, the repo_to_terminal map will prioritize
    pub fn refresh_working_directories_for_pane_group(
        &mut self,
        pane_group_id: EntityId,
        terminal_cwds: Vec<(EntityId, LocalOrRemotePath)>,
        editor_paths: Vec<(EntityId, LocalOrRemotePath)>,
        focused_terminal_id: Option<EntityId>,
        ctx: &mut ModelContext<Self>,
    ) {
        if terminal_cwds.is_empty() && editor_paths.is_empty() {
            self.handle_empty_pane_group(pane_group_id, ctx);
            return;
        }

        let old_directories: Vec<WorkingDirectory> = self
            .least_recent_directories_for_pane_group(pane_group_id)
            .map(|dirs| {
                dirs.iter()
                    .map(|lor| WorkingDirectory {
                        path: lor.clone(),
                        terminal_id: self.get_terminal_id_for_root_path(pane_group_id, lor),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let old_repos: Vec<LocalOrRemotePath> = self
            .least_recent_repositories_for_pane_group(pane_group_id)
            .map(|repos| repos.iter().cloned().collect())
            .unwrap_or_default();
        let old_focused_repo: Option<LocalOrRemotePath> =
            self.focused_repo.get(&pane_group_id).cloned().flatten();

        // Resolve a local path to its detected repository root, or keep the path as-is if no repo is found.
        let root_for_path = |path: PathBuf| {
            DetectedRepositories::as_ref(ctx)
                .get_root_for_path(&LocalOrRemotePath::Local(path.clone()))
                .and_then(|r| PathBuf::try_from(r).ok())
                .unwrap_or(path)
        };

        let root_for_raw_path = |raw_path: &str| normalize_cwd(raw_path).map(root_for_path);

        // Split terminal CWDs into local and remote buckets.
        let mut local_terminal_cwds: Vec<(EntityId, String)> = Vec::new();
        let mut remote_terminal_cwds: Vec<(EntityId, RemotePath)> = Vec::new();
        for (terminal_id, cwd) in &terminal_cwds {
            match cwd {
                LocalOrRemotePath::Local(path) => {
                    local_terminal_cwds.push((*terminal_id, path.to_string_lossy().into_owned()));
                }
                LocalOrRemotePath::Remote(remote_path) => {
                    remote_terminal_cwds.push((*terminal_id, remote_path.clone()));
                }
            }
        }

        // Collapse working directories to their nearest repository root (when detected).
        let mut file_path_ancestors: HashSet<PathBuf> = local_terminal_cwds
            .iter()
            .filter_map(|(_, cwd)| root_for_raw_path(cwd))
            .collect();

        // Split editor paths into local and remote buckets.
        let mut local_editor_paths: Vec<(EntityId, String)> = Vec::new();
        let mut remote_editor_paths: Vec<(EntityId, RemotePath)> = Vec::new();
        for (view_id, path) in &editor_paths {
            match path {
                LocalOrRemotePath::Local(p) => {
                    local_editor_paths.push((*view_id, p.to_string_lossy().into_owned()));
                }
                LocalOrRemotePath::Remote(remote_path) => {
                    remote_editor_paths.push((*view_id, remote_path.clone()));
                }
            }
        }

        let local_cwds: Vec<(EntityId, String)> = local_editor_paths
            .into_iter()
            .filter_map(|(view_id, path)| {
                let path_buf = PathBuf::from(&path);
                let resolved_path = self
                    .get_repo_root_for_path(&path_buf, ctx)
                    .or_else(|| path_buf.parent().map(|p| p.to_path_buf()))?;

                if file_path_ancestors.insert(resolved_path.clone()) {
                    Some((view_id, resolved_path.display().to_string()))
                } else {
                    None
                }
            })
            .collect();

        // Build the local root paths for pane_groups.
        let new_local_root_paths: Vec<PathBuf> = local_terminal_cwds
            .iter()
            .chain(local_cwds.iter())
            .filter_map(|(_, cwd)| root_for_raw_path(cwd))
            .collect();

        // Build remote root paths for pane_groups from remote terminal CWDs
        // and remote editor paths (resolved to repo root when possible).
        let mut new_remote_display_roots: Vec<LocalOrRemotePath> = Vec::new();
        for (_terminal_id, remote_path) in &remote_terminal_cwds {
            let remote_key = LocalOrRemotePath::Remote(remote_path.clone());
            let root = DetectedRepositories::as_ref(ctx)
                .get_root_for_path(&remote_key)
                .unwrap_or(remote_key);
            new_remote_display_roots.push(root);
        }
        for (_view_id, remote_path) in &remote_editor_paths {
            let remote_key = LocalOrRemotePath::Remote(remote_path.clone());
            if let Some(repo_root) =
                DetectedRepositories::as_ref(ctx).get_root_for_path(&remote_key)
            {
                new_remote_display_roots.push(repo_root);
            } else if let Some(parent) = remote_path.path.parent() {
                // Fall back to the parent directory, matching the local editor path behavior.
                new_remote_display_roots.push(LocalOrRemotePath::Remote(RemotePath::new(
                    remote_path.host_id.clone(),
                    parent,
                )));
            }
        }

        // Combine local + remote into the unified display roots set.
        let new_display_roots: Vec<LocalOrRemotePath> = new_local_root_paths
            .iter()
            .cloned()
            .map(LocalOrRemotePath::Local)
            .chain(new_remote_display_roots.iter().cloned())
            .collect();

        // Get or create the IndexSet for this pane group
        // (IndexSet maintains insertion order and auto-deduplicates)
        let pane_group_roots = self.pane_groups.entry(pane_group_id).or_default();
        update_index_set(pane_group_roots, new_display_roots);

        // Build repo roots and their terminal associations
        // First pass: collect all local repo roots and build initial mapping
        let new_local_repo_roots: Vec<PathBuf> = self
            .pane_groups
            .get(&pane_group_id)
            .into_iter()
            .flat_map(|dirs| dirs.iter())
            .filter_map(|lor| lor.to_local_path())
            .filter_map(|dir| self.get_repo_root_for_path(dir, ctx))
            .collect();
        let mut new_roots: HashSet<PathBuf> =
            HashSet::from_iter(new_local_repo_roots.iter().cloned());
        new_roots.extend(new_local_root_paths.iter().cloned());

        // Build mapping from directories to their terminal IDs (keyed by LocalOrRemotePath).
        // Local paths come from `root_for_raw_path` → `normalize_cwd`.
        let mut new_root_to_terminal: HashMap<LocalOrRemotePath, EntityId> = local_terminal_cwds
            .iter()
            .filter_map(|(terminal_id, cwd)| {
                root_for_raw_path(cwd).map(|p| (LocalOrRemotePath::Local(p), *terminal_id))
            })
            .collect();
        new_root_to_terminal
            .retain(|cwd, _terminal_id| cwd.to_local_path().is_some_and(|p| new_roots.contains(p)));

        // Resolve remote terminal CWDs to their repo roots and add to mappings.
        let mut new_remote_repo_roots: Vec<LocalOrRemotePath> = Vec::new();
        for (terminal_id, remote_path) in &remote_terminal_cwds {
            let remote_key = LocalOrRemotePath::Remote(remote_path.clone());
            if let Some(repo_root) =
                DetectedRepositories::as_ref(ctx).get_root_for_path(&remote_key)
            {
                new_root_to_terminal.insert(repo_root.clone(), *terminal_id);
                new_remote_repo_roots.push(repo_root);
            } else {
                // No repo detected — still track the CWD → terminal mapping
                // so `find_review_terminal` can resolve it.
                new_root_to_terminal.insert(remote_key, *terminal_id);
            }
        }

        // Resolve remote editor paths to their repo roots.
        for (_view_id, remote_path) in &remote_editor_paths {
            let remote_key = LocalOrRemotePath::Remote(remote_path.clone());
            if let Some(repo_root) =
                DetectedRepositories::as_ref(ctx).get_root_for_path(&remote_key)
            {
                new_remote_repo_roots.push(repo_root);
            }
        }

        // Second pass: if we have a focused terminal, ensure its repo maps to it
        // This ensures the dropdown selects the correct repo when a pane is focused or CD'd
        let mut focused_repo: Option<LocalOrRemotePath> = None;
        if let Some(focused_id) = focused_terminal_id {
            let mut repos_to_insert = Vec::new();
            for (dir, terminal_id) in &new_root_to_terminal {
                if *terminal_id == focused_id
                    && let Some(repo_root) =
                        DetectedRepositories::as_ref(ctx).get_root_for_path(dir)
                {
                    repos_to_insert.push((repo_root.clone(), focused_id));
                    focused_repo = Some(repo_root);
                }
            }
            for (repo_key, focused_id) in repos_to_insert {
                new_root_to_terminal.insert(repo_key, focused_id);
            }
        }

        // Build the unified set of repo roots (local + remote).
        let mut new_repo_roots_wrapped: Vec<LocalOrRemotePath> = new_local_repo_roots
            .into_iter()
            .map(LocalOrRemotePath::Local)
            .chain(new_remote_repo_roots)
            .chain(new_remote_display_roots)
            .collect();
        // Deduplicate (IndexSet handles this, but avoid duplicates in the input).
        let seen: HashSet<_> = new_repo_roots_wrapped.iter().cloned().collect();
        new_repo_roots_wrapped.retain({
            let mut first_seen = HashSet::new();
            move |item| first_seen.insert(item.clone())
        });
        let _ = seen; // consumed by retain closure above

        let discovery_enabled = FeatureFlag::MultiRepoCodeReview.is_enabled();
        let old_discovery_state = self.discovery_state(pane_group_id);
        if discovery_enabled {
            let mut discovery_cwds: HashMap<PathBuf, EntityId> = HashMap::new();
            for (terminal_id, cwd) in &local_terminal_cwds {
                let Some(cwd) = normalize_cwd(cwd) else {
                    continue;
                };
                if DetectedRepositories::as_ref(ctx)
                    .get_root_for_canonical_path(&LocalOrRemotePath::Local(cwd.clone()))
                    .is_some()
                {
                    continue;
                }
                let mapped = discovery_cwds.entry(cwd).or_insert(*terminal_id);
                if focused_terminal_id == Some(*terminal_id) {
                    *mapped = *terminal_id;
                }
            }
            self.update_discovery_entries(pane_group_id, discovery_cwds, ctx);

            let discovery = self.repo_discovery.entry(pane_group_id).or_default();
            discovery.cwd_repo_roots = new_repo_roots_wrapped.clone();
            discovery.cwd_root_to_terminal = new_root_to_terminal.clone();
            discovery
                .chain_discovered_roots(&mut new_repo_roots_wrapped, &mut new_root_to_terminal);
        }

        let orphaned_repos = self
            .repository_roots
            .set_paths(pane_group_id, new_repo_roots_wrapped);

        self.directory_to_terminal
            .insert(pane_group_id, new_root_to_terminal);

        if discovery_enabled {
            self.reset_discovery_defaults_for_cwd_repos(pane_group_id, ctx);
        }

        let new_directories: Vec<WorkingDirectory> = self
            .pane_groups
            .get(&pane_group_id)
            .map(|dirs| {
                dirs.iter()
                    .map(|lor| WorkingDirectory {
                        path: lor.clone(),
                        terminal_id: self.get_terminal_id_for_root_path(pane_group_id, lor),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let new_deduplicated_repos: Vec<LocalOrRemotePath> = self
            .repository_roots
            .get(pane_group_id)
            .map(|repos| repos.iter().cloned().collect())
            .unwrap_or_default();
        if old_directories != new_directories {
            self.emit_directories_changed(pane_group_id, ctx);
        }

        if old_repos != new_deduplicated_repos {
            self.drop_unused_diff_state_models(orphaned_repos, ctx);
            self.emit_repositories_changed(pane_group_id, ctx);
        } else if old_discovery_state != self.discovery_state(pane_group_id) {
            self.emit_repositories_changed(pane_group_id, ctx);
        }

        if old_focused_repo != focused_repo {
            self.focused_repo
                .insert(pane_group_id, focused_repo.clone());
            self.emit_focused_repo_changed(pane_group_id, focused_repo, ctx);
        }
    }

    /// Maps a repository to a specific terminal view ID so that
    /// `get_terminal_id_for_root_path` can resolve the preferred terminal
    /// for that repo (used by `find_review_terminal`).
    pub fn register_terminal_for_repo(
        &mut self,
        pane_group_id: EntityId,
        repo_key: LocalOrRemotePath,
        terminal_id: EntityId,
    ) {
        self.directory_to_terminal
            .entry(pane_group_id)
            .or_default()
            .insert(repo_key, terminal_id);
    }

    /// Registers a remote repository root for a pane group. Inserts it into
    /// the unified `repository_roots` map and emits `RepositoriesChanged` if
    /// the repo was newly added.
    pub fn register_remote_repo(
        &mut self,
        pane_group_id: EntityId,
        repo_key: LocalOrRemotePath,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.repository_roots.insert(pane_group_id, repo_key) {
            self.emit_repositories_changed(pane_group_id, ctx);
        }
    }

    /// Metadata for `root` when it was discovered beneath one of the pane group's non-repo
    /// working directories and no working directory sits inside it.
    pub fn discovered_repo_info(
        &self,
        pane_group_id: EntityId,
        root: &Path,
    ) -> Option<DiscoveredRepoInfo> {
        let discovery = self.repo_discovery.get(&pane_group_id)?;
        if !discovery.discovered_roots.contains(root) {
            return None;
        }
        discovery.entries.iter().find_map(|(parent, entry)| {
            let repo = entry
                .result
                .as_ref()?
                .repos
                .iter()
                .find(|repo| repo.root == root)?;
            Some(DiscoveredRepoInfo {
                parent: parent.clone(),
                main_repo_name: repo.main_repo_name.clone(),
                external_git_dir: repo.external_git_dir.clone(),
                recency: repo.recency,
            })
        })
    }

    pub fn discovery_state(&self, pane_group_id: EntityId) -> RepoDiscoveryState {
        self.repo_discovery
            .get(&pane_group_id)
            .map(PaneGroupDiscovery::state)
            .unwrap_or_default()
    }

    /// Rescans every non-repo working directory in the pane group, e.g. when the code review
    /// panel opens.
    pub fn rescan_discovered_repos(
        &mut self,
        pane_group_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        let cwds: Vec<PathBuf> = self
            .repo_discovery
            .get(&pane_group_id)
            .map(|discovery| discovery.entries.keys().cloned().collect())
            .unwrap_or_default();
        for cwd in cwds {
            self.request_discovery_scan(pane_group_id, cwd, ctx);
        }
    }

    fn is_discovered_only_repo(&self, root: &Path) -> bool {
        let key = LocalOrRemotePath::Local(root.to_path_buf());
        self.repo_discovery
            .values()
            .any(|discovery| discovery.discovered_roots.contains(root))
            && !self
                .repo_discovery
                .values()
                .any(|discovery| discovery.cwd_repo_roots.contains(&key))
    }

    /// Drops discovery for working directories that left the pane group and starts a scan for
    /// each new one.
    fn update_discovery_entries(
        &mut self,
        pane_group_id: EntityId,
        cwds: HashMap<PathBuf, EntityId>,
        ctx: &mut ModelContext<Self>,
    ) {
        let discovery = self.repo_discovery.entry(pane_group_id).or_default();
        let stale: Vec<PathBuf> = discovery
            .entries
            .keys()
            .filter(|cwd| !cwds.contains_key(*cwd))
            .cloned()
            .collect();
        let stale_entries: Vec<DiscoveryEntry> = stale
            .iter()
            .filter_map(|cwd| discovery.entries.remove(cwd))
            .collect();
        let mut new_cwds = Vec::new();
        for (cwd, terminal_id) in cwds {
            match discovery.entries.get_mut(&cwd) {
                Some(entry) => entry.terminal_id = terminal_id,
                None => {
                    discovery.entries.insert(
                        cwd.clone(),
                        DiscoveryEntry {
                            terminal_id,
                            result: None,
                            scan: None,
                            rescan_requested: false,
                        },
                    );
                    new_cwds.push(cwd);
                }
            }
        }

        for entry in stale_entries {
            self.release_discovery_entry(entry, ctx);
        }
        for cwd in new_cwds {
            self.request_discovery_scan(pane_group_id, cwd, ctx);
        }
    }

    /// Starts a scan of `cwd`, or queues one behind a scan already in flight.
    fn request_discovery_scan(
        &mut self,
        pane_group_id: EntityId,
        cwd: PathBuf,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(entry) = self
            .repo_discovery
            .get_mut(&pane_group_id)
            .and_then(|discovery| discovery.entries.get_mut(&cwd))
        else {
            return;
        };
        if entry.scan.is_some() {
            entry.rescan_requested = true;
            return;
        }

        let scan_cwd = cwd.clone();
        let handle = ctx.spawn(
            async move { discover_repos_outside_repo(&scan_cwd) },
            move |me, result, ctx| me.on_discovery_scan_finished(pane_group_id, cwd, result, ctx),
        );
        #[cfg(test)]
        self.discovery_scan_futures.push(handle.future_id());
        entry.scan = Some(handle);
    }

    fn on_discovery_scan_finished(
        &mut self,
        pane_group_id: EntityId,
        cwd: PathBuf,
        result: RepoDiscovery,
        ctx: &mut ModelContext<Self>,
    ) {
        let old_state = self.discovery_state(pane_group_id);
        let Some(entry) = self
            .repo_discovery
            .get_mut(&pane_group_id)
            .and_then(|discovery| discovery.entries.get_mut(&cwd))
        else {
            return;
        };
        entry.scan = None;
        let old_watches = entry.watched_dirs();
        entry.result = Some(result);
        let new_watches = entry.watched_dirs();
        let rescan = std::mem::take(&mut entry.rescan_requested);

        self.watch_discovery_dirs(new_watches, ctx);
        self.unwatch_discovery_dirs(old_watches, ctx);
        if rescan {
            self.request_discovery_scan(pane_group_id, cwd, ctx);
        }
        self.apply_discovered_roots(pane_group_id, old_state, ctx);
    }

    /// Rebuilds the pane group's repo roots from its latest cwd-derived roots plus the current
    /// discovery results.
    fn apply_discovered_roots(
        &mut self,
        pane_group_id: EntityId,
        old_state: RepoDiscoveryState,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(discovery) = self.repo_discovery.get_mut(&pane_group_id) else {
            return;
        };
        let mut repo_roots = discovery.cwd_repo_roots.clone();
        let mut root_to_terminal = discovery.cwd_root_to_terminal.clone();
        discovery.chain_discovered_roots(&mut repo_roots, &mut root_to_terminal);

        let old_repos: Vec<LocalOrRemotePath> = self
            .least_recent_repositories_for_pane_group(pane_group_id)
            .map(|repos| repos.iter().cloned().collect())
            .unwrap_or_default();
        let orphaned_repos = self.repository_roots.set_paths(pane_group_id, repo_roots);
        self.directory_to_terminal
            .insert(pane_group_id, root_to_terminal);
        let repos_changed = self
            .least_recent_repositories_for_pane_group(pane_group_id)
            .is_none_or(|repos| !repos.iter().eq(old_repos.iter()));

        if repos_changed {
            self.drop_unused_diff_state_models(orphaned_repos, ctx);
        }
        if repos_changed || old_state != self.discovery_state(pane_group_id) {
            self.emit_repositories_changed(pane_group_id, ctx);
        }
    }

    /// A discovered repo that a working directory now sits in gets today's `Head` default back,
    /// unless the user already picked a mode.
    fn reset_discovery_defaults_for_cwd_repos(
        &mut self,
        pane_group_id: EntityId,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(discovery) = self.repo_discovery.get(&pane_group_id) else {
            return;
        };
        let models: Vec<ModelHandle<DiffStateModel>> = discovery
            .cwd_repo_roots
            .iter()
            .filter_map(|root| self.diff_state_models.get(root).cloned())
            .collect();
        for model in models {
            if model.as_ref(ctx).has_discovery_default_mode(ctx) {
                model.update(ctx, |model, ctx| model.reset_discovery_default_mode(ctx));
            }
        }
    }

    fn release_discovery_entry(&mut self, entry: DiscoveryEntry, ctx: &mut ModelContext<Self>) {
        if let Some(scan) = &entry.scan {
            scan.abort();
        }
        self.unwatch_discovery_dirs(entry.watched_dirs(), ctx);
    }

    fn watch_discovery_dirs(&mut self, dirs: Vec<PathBuf>, ctx: &mut ModelContext<Self>) {
        let watcher = self.discovery_watcher(ctx);
        for dir in dirs {
            let count = self.discovery_watch_counts.entry(dir.clone()).or_default();
            *count += 1;
            if *count == 1 {
                watcher.update(ctx, |watcher, _| {
                    std::mem::drop(watcher.register_path(
                        &dir,
                        WatchFilter::accept_all(),
                        RecursiveMode::NonRecursive,
                    ));
                });
            }
        }
    }

    fn unwatch_discovery_dirs(&mut self, dirs: Vec<PathBuf>, ctx: &mut ModelContext<Self>) {
        for dir in dirs {
            let Some(count) = self.discovery_watch_counts.get_mut(&dir) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.discovery_watch_counts.remove(&dir);
                if let Some(watcher) = &self.discovery_watcher {
                    watcher.update(ctx, |watcher, _| {
                        std::mem::drop(watcher.unregister_path(&dir));
                    });
                }
            }
        }
    }

    fn discovery_watcher(
        &mut self,
        ctx: &mut ModelContext<Self>,
    ) -> ModelHandle<BulkFilesystemWatcher> {
        if let Some(watcher) = &self.discovery_watcher {
            return watcher.clone();
        }
        #[cfg(not(test))]
        let watcher =
            ctx.add_model(|ctx| BulkFilesystemWatcher::new(DISCOVERY_WATCH_DEBOUNCE, ctx));
        #[cfg(test)]
        let watcher = ctx.add_model(|_| BulkFilesystemWatcher::new_for_test());
        ctx.subscribe_to_model(&watcher, |me, _, event, ctx| {
            me.handle_discovery_watch_event(event, ctx);
        });
        self.discovery_watcher = Some(watcher.clone());
        watcher
    }

    /// Rescans the discoveries that walked through a folder where a directory or `.git` entry
    /// appeared or disappeared.
    fn handle_discovery_watch_event(
        &mut self,
        event: &BulkFilesystemWatcherEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let appeared = event
            .added
            .iter()
            .chain(event.moved.keys())
            .filter(|path| path.is_dir() || is_dot_git(path));
        let disappeared = event.deleted.iter().chain(event.moved.values());
        let changed: Vec<&PathBuf> = appeared.chain(disappeared).collect();
        if changed.is_empty() {
            return;
        }

        let mut to_rescan = Vec::new();
        for (pane_group_id, discovery) in &self.repo_discovery {
            for (cwd, entry) in &discovery.entries {
                let Some(result) = &entry.result else {
                    continue;
                };
                let affected = changed.iter().any(|path| {
                    path.parent()
                        .is_some_and(|parent| result.visited_dirs.iter().any(|dir| dir == parent))
                });
                if affected {
                    to_rescan.push((*pane_group_id, cwd.clone()));
                }
            }
        }
        for (pane_group_id, cwd) in to_rescan {
            self.request_discovery_scan(pane_group_id, cwd, ctx);
        }
    }

    /// Get the repository root for a given path.
    fn get_repo_root_for_path(&self, path: &Path, ctx: &AppContext) -> Option<PathBuf> {
        DetectedRepositories::as_ref(ctx)
            .get_root_for_path(&LocalOrRemotePath::Local(path.to_path_buf()))
            .and_then(|r| PathBuf::try_from(r).ok())
    }

    /// Emit a DirectoriesChanged event with the current state for a specific pane group.
    /// Directories are returned in most recent first order for use in the UI.
    fn emit_directories_changed(&mut self, pane_group_id: EntityId, ctx: &mut ModelContext<Self>) {
        ctx.emit(WorkingDirectoriesEvent::DirectoriesChanged {
            pane_group_id,
            directories: self
                .most_recent_directories_for_pane_group(pane_group_id)
                .map(|iter| iter.collect())
                .unwrap_or_default(),
        });
    }

    /// Emit a RepositoriesChanged event with the current state for a specific pane group.
    /// Repositories are returned in most recent first order for use in the UI.
    fn emit_repositories_changed(&mut self, pane_group_id: EntityId, ctx: &mut ModelContext<Self>) {
        ctx.emit(WorkingDirectoriesEvent::RepositoriesChanged {
            pane_group_id,
            repositories: self
                .most_recent_repositories_for_pane_group(pane_group_id)
                .map(|iter| iter.collect())
                .unwrap_or_default(),
        });
    }

    fn emit_focused_repo_changed(
        &mut self,
        pane_group_id: EntityId,
        focused_repo: Option<LocalOrRemotePath>,
        ctx: &mut ModelContext<Self>,
    ) {
        ctx.emit(WorkingDirectoriesEvent::FocusedRepoChanged {
            pane_group_id,
            repository_terminal_map: self
                .directory_to_terminal
                .get(&pane_group_id)
                .cloned()
                .unwrap_or_default(),
            focused_repo,
        });
    }

    pub(crate) fn insert_code_review_comments(
        &mut self,
        pane_group_id: EntityId,
        repo_path: &LocalOrRemotePath,
        comments: &Vec<PendingImportedReviewComment>,
        diff_mode: &DiffMode,
        ctx: &mut ModelContext<Self>,
    ) {
        match self.get_code_review_view(pane_group_id, repo_path) {
            Some(code_review_view) => code_review_view.update(ctx, |code_review_view, ctx| {
                code_review_view.set_diff_base(diff_mode.to_owned(), ctx);
                code_review_view.expand_comment_list(ctx);
            }),
            _ => {
                report_error!(
                    "WorkingDirectoriesModel did not find CodeReviewView for repo path",
                    extra: { "repo_path" => ?repo_path }
                );
            }
        }

        if let Some(comment_batch) = self.get_or_create_code_review_comments(repo_path, ctx) {
            let comments = comments.to_owned();
            comment_batch.update(ctx, |comment_batch, ctx| {
                comment_batch.add_pending_imported_comments(comments, diff_mode.to_owned(), ctx);
            })
        }
    }

    /// Inserts pre-flattened (already attached) review comments into the comment batch for the
    /// given repository, creating the batch if needed. Unlike `insert_code_review_comments`, these
    /// comments have already been thread-flattened and converted to `AttachedReviewComment`, so
    /// they are ready to be repositioned onto diff editors immediately.
    pub(crate) fn upsert_flattened_code_review_comments(
        &mut self,
        repo_path: &LocalOrRemotePath,
        comments: Vec<AttachedReviewComment>,
        ctx: &mut ModelContext<Self>,
    ) {
        if let Some(comment_batch) = self.get_or_create_code_review_comments(repo_path, ctx) {
            comment_batch.update(ctx, |comment_batch, ctx| {
                comment_batch.upsert_imported_comments(comments, ctx);
            });
        }
    }
}

#[cfg(not(feature = "local_fs"))]
impl WorkingDirectoriesModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the unique directories for a specific pane group in most to least recently added order.
    pub fn most_recent_directories_for_pane_group(
        &self,
        _pane_group_id: EntityId,
    ) -> Option<impl Iterator<Item = WorkingDirectory> + '_> {
        Option::<std::iter::Empty<WorkingDirectory>>::None
    }

    /// Get the unique repository roots for a specific pane group in most to least recently added order.
    pub fn most_recent_repositories_for_pane_group(
        &self,
        _pane_group_id: EntityId,
    ) -> Option<impl Iterator<Item = LocalOrRemotePath> + '_> {
        Option::<std::iter::Empty<LocalOrRemotePath>>::None
    }

    /// Get the terminal view ID associated with a specific repository in a pane group.
    pub fn get_terminal_id_for_root_path(
        &self,
        _pane_group_id: EntityId,
        _root_path: &LocalOrRemotePath,
    ) -> Option<EntityId> {
        None
    }

    pub fn refresh_working_directories_for_pane_group(
        &mut self,
        _pane_group_id: EntityId,
        _terminal_cwds: Vec<(EntityId, LocalOrRemotePath)>,
        _editor_paths: Vec<(EntityId, LocalOrRemotePath)>,
        _focused_terminal_id: Option<EntityId>,
        _ctx: &mut ModelContext<Self>,
    ) {
    }

    pub fn get_or_create_diff_state_model(
        &mut self,
        _key: LocalOrRemotePath,
        _preferred_session: Option<SessionId>,
        _ctx: &mut ModelContext<Self>,
    ) -> Option<ModelHandle<DiffStateModel>> {
        None
    }

    pub fn get_or_create_code_review_comments(
        &mut self,
        _repo_path: &LocalOrRemotePath,
        _ctx: &mut ModelContext<Self>,
    ) -> Option<ModelHandle<ReviewCommentBatch>> {
        None
    }

    pub fn store_code_review_view(
        &mut self,
        _pane_group_id: EntityId,
        _repo_path: LocalOrRemotePath,
        _view: ViewHandle<CodeReviewView>,
    ) {
    }

    pub fn get_code_review_view(
        &self,
        _pane_group_id: EntityId,
        _repo_path: &LocalOrRemotePath,
    ) -> Option<ViewHandle<CodeReviewView>> {
        None
    }

    pub fn get_selected_review_repo(&self, _pane_group_id: EntityId) -> Option<&LocalOrRemotePath> {
        None
    }

    pub fn set_selected_review_repo(
        &mut self,
        _pane_group_id: EntityId,
        _repo_path: LocalOrRemotePath,
    ) {
    }

    pub fn clear_selected_review_repo(&mut self, _pane_group_id: EntityId) {}

    pub fn store_global_search_view(
        &mut self,
        _pane_group_id: EntityId,
        _view: ViewHandle<GlobalSearchView>,
    ) {
    }

    pub fn get_global_search_view(
        &self,
        _pane_group_id: EntityId,
    ) -> Option<ViewHandle<GlobalSearchView>> {
        None
    }

    pub fn store_file_tree_view(
        &mut self,
        _pane_group_id: EntityId,
        _view: ViewHandle<crate::code::file_tree::FileTreeView>,
    ) {
    }

    pub fn get_file_tree_view(
        &self,
        _pane_group_id: EntityId,
    ) -> Option<ViewHandle<crate::code::file_tree::FileTreeView>> {
        None
    }

    pub fn remove_pane_group(&mut self, _pane_group_id: EntityId, _ctx: &mut ModelContext<Self>) {}

    pub fn discovered_repo_info(
        &self,
        _pane_group_id: EntityId,
        _root: &std::path::Path,
    ) -> Option<DiscoveredRepoInfo> {
        None
    }

    pub fn discovery_state(&self, _pane_group_id: EntityId) -> RepoDiscoveryState {
        RepoDiscoveryState::default()
    }

    pub fn rescan_discovered_repos(
        &mut self,
        _pane_group_id: EntityId,
        _ctx: &mut ModelContext<Self>,
    ) {
    }

    pub(crate) fn insert_code_review_comments(
        &mut self,
        _pane_group_id: EntityId,
        _repo_path: &LocalOrRemotePath,
        _comments: &Vec<PendingImportedReviewComment>,
        _diff_mode: &DiffMode,
        _ctx: &mut ModelContext<Self>,
    ) {
    }

    pub(crate) fn upsert_flattened_code_review_comments(
        &mut self,
        _repo_path: &LocalOrRemotePath,
        _comments: Vec<AttachedReviewComment>,
        _ctx: &mut ModelContext<Self>,
    ) {
    }
}

impl Entity for WorkingDirectoriesModel {
    type Event = WorkingDirectoriesEvent;
}

/// Runs [`discover_repos`] unless `cwd` is inside a git repo that repo detection has not reported
/// yet. Like repo detection, a repo at `$HOME` or above does not count.
#[cfg(feature = "local_fs")]
fn discover_repos_outside_repo(cwd: &Path) -> RepoDiscovery {
    let home = dirs::home_dir();
    for dir in cwd.ancestors() {
        if home.as_deref() == Some(dir) {
            break;
        }
        if dir.join(".git").exists() {
            return RepoDiscovery::default();
        }
    }
    discover_repos(cwd)
}

/// The folders a discovery watches: the walked non-repo folders, shallowest first, capped.
#[cfg(feature = "local_fs")]
fn discovery_watch_dirs(result: &RepoDiscovery) -> Vec<PathBuf> {
    let mut dirs = result.visited_dirs.clone();
    dirs.sort_by_key(|dir| dir.components().count());
    dirs.truncate(MAX_DISCOVERY_WATCHES);
    dirs
}

#[cfg(feature = "local_fs")]
fn is_dot_git(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".git")
}

/// Normalize a CWD path string to a canonical PathBuf
///
/// This function attempts to canonicalize (resolve symlinks, make absolute)
///
/// Returns None if the path is empty, invalid, or cannot be canonicalized.
/// Canonicalization failure may indicate remote paths or non-existent directories,
/// which could be supported in the future.
#[cfg(feature = "local_fs")]
fn normalize_cwd(raw_cwd: &str) -> Option<PathBuf> {
    if raw_cwd.is_empty() {
        return None;
    }

    let path = PathBuf::from(raw_cwd.to_string());
    // Use dunce::canonicalize to avoid Windows extended-length path prefix (\\?\)
    // which would cause path comparison mismatches with CanonicalizedPath.
    dunce::canonicalize(&path).ok()
}

#[cfg(test)]
#[path = "working_directories_tests.rs"]
mod tests;
