//! Filesystem-only watchers on discovered repos the review panel is not showing, so the panel can
//! offer a "Changes in <repo>" chip without keeping a live diff model per repo.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use async_channel::Sender;
use repo_metadata::repository::{RepositorySubscriber, SubscriberId};
use repo_metadata::{
    DirectoryWatcher, Repository, RepositoryUpdate, RepositoryWatchMode, TargetFile,
};
use warp_util::standardized_path::StandardizedPath;
use warpui::{Entity, ModelContext, ModelHandle, SingletonEntity};

use crate::code_review::repo_list::is_reviewable_change;

/// A repo to watch for chip-worthy changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedRepo {
    pub root: PathBuf,
    /// Per-worktree git dir outside the working tree, as for a linked worktree.
    pub external_git_dir: Option<PathBuf>,
}

pub enum RepoChangeWatcherEvent {
    FilesChanged {
        repo: PathBuf,
        files: Vec<TargetFile>,
    },
}

struct ActiveWatch {
    repository: ModelHandle<Repository>,
    subscriber_id: SubscriberId,
}

/// Registers repos straight with the [`DirectoryWatcher`] rather than through repo detection,
/// which would emit `DetectedGitRepo` and pull every discovered repo into codebase indexing.
/// A repo already registered by its diff model shares that `Repository` handle.
pub struct RepoChangeWatcher {
    watches: HashMap<PathBuf, ActiveWatch>,
    changes_tx: Sender<(PathBuf, Vec<TargetFile>)>,
}

impl RepoChangeWatcher {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let (changes_tx, changes_rx) = async_channel::unbounded();
        ctx.spawn_stream_local(
            changes_rx,
            |me, (repo, files), ctx| {
                // Updates already queued for a repo that was just unwatched are stale.
                if me.watches.contains_key(&repo) {
                    ctx.emit(RepoChangeWatcherEvent::FilesChanged { repo, files });
                }
            },
            |_, _| {},
        );
        Self {
            watches: HashMap::new(),
            changes_tx,
        }
    }

    /// Watches exactly `repos`, starting and stopping watchers as needed.
    pub fn set_watched_repos(&mut self, repos: Vec<WatchedRepo>, ctx: &mut ModelContext<Self>) {
        let stale: Vec<PathBuf> = self
            .watches
            .keys()
            .filter(|root| !repos.iter().any(|repo| &repo.root == *root))
            .cloned()
            .collect();
        for root in stale {
            if let Some(watch) = self.watches.remove(&root) {
                watch.repository.update(ctx, |repository, ctx| {
                    repository.stop_watching(watch.subscriber_id, ctx);
                });
            }
        }

        for repo in repos {
            if self.watches.contains_key(&repo.root) {
                continue;
            }
            if let Some(watch) = self.start_watch(&repo, ctx) {
                self.watches.insert(repo.root, watch);
            }
        }
    }

    fn start_watch(&self, repo: &WatchedRepo, ctx: &mut ModelContext<Self>) -> Option<ActiveWatch> {
        let root = StandardizedPath::from_local_canonicalized(&repo.root).ok()?;
        let external_git_dir = repo
            .external_git_dir
            .as_deref()
            .and_then(|dir| StandardizedPath::from_local_canonicalized(dir).ok());
        let repository = DirectoryWatcher::handle(ctx)
            .update(ctx, |watcher, ctx| {
                watcher.add_directory_with_git_dir(root, external_git_dir, ctx)
            })
            .inspect_err(|err| {
                log::debug!("Not watching {} for changes: {err}", repo.root.display());
            })
            .ok()?;
        let subscriber = ChangeSubscriber {
            repo: repo.root.clone(),
            changes_tx: self.changes_tx.clone(),
        };
        let start = repository.update(ctx, |repository, ctx| {
            repository.start_watching(
                RepositoryWatchMode::FilesystemOnly,
                Box::new(subscriber),
                ctx,
            )
        });
        let root = repo.root.clone();
        ctx.spawn(start.registration_future, move |_, result, _| {
            if let Err(err) = result {
                log::debug!("Failed to watch {} for changes: {err}", root.display());
            }
        });
        Some(ActiveWatch {
            repository,
            subscriber_id: start.subscriber_id,
        })
    }
}

impl Entity for RepoChangeWatcher {
    type Event = RepoChangeWatcherEvent;
}

struct ChangeSubscriber {
    repo: PathBuf,
    changes_tx: Sender<(PathBuf, Vec<TargetFile>)>,
}

impl RepositorySubscriber for ChangeSubscriber {
    fn on_scan(
        &mut self,
        _repository: &Repository,
        _ctx: &mut ModelContext<Repository>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        // Only changes made while the panel is open raise a chip.
        Box::pin(async {})
    }

    fn on_files_updated(
        &mut self,
        _repository: &Repository,
        update: &RepositoryUpdate,
        _ctx: &mut ModelContext<Repository>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let files: Vec<TargetFile> = update
            .added_or_modified()
            .chain(&update.deleted)
            .chain(update.moved.iter().flat_map(|(to, from)| [to, from]))
            .filter(|file| is_reviewable_change(&self.repo, file))
            .cloned()
            .collect();
        if files.is_empty() {
            return Box::pin(async {});
        }
        let message = (self.repo.clone(), files);
        let changes_tx = self.changes_tx.clone();
        Box::pin(async move {
            let _ = changes_tx.send(message).await;
        })
    }
}

#[cfg(test)]
#[path = "repo_change_watcher_tests.rs"]
mod tests;
