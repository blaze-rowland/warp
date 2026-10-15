use std::fs;
use std::path::Path;

use repo_metadata::repositories::{DetectedRepositories, DetectedRepositoriesEvent};
use repo_metadata::watcher::DirectoryWatcher;
use warp_util::standardized_path::StandardizedPath;
use warpui::{App, Entity, ModelHandle, SingletonEntity};

use super::*;
use crate::code::buffer_location::LocalOrRemotePath;

#[derive(Default)]
struct DetectionRecorder {
    detected: usize,
}

impl Entity for DetectionRecorder {
    type Event = ();
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn registered_repository(app: &mut App, root: &Path) -> ModelHandle<Repository> {
    let root = StandardizedPath::from_local_canonicalized(root).unwrap();
    DirectoryWatcher::handle(app).update(app, |watcher, ctx| {
        watcher.add_directory(root, ctx).unwrap()
    })
}

#[test]
fn watching_repos_skips_repo_detection_and_releases_on_unwatch() {
    App::test((), |mut app| async move {
        let detected_repos = app.add_singleton_model(|_| DetectedRepositories::default());
        app.add_singleton_model(DirectoryWatcher::new_for_testing);
        let recorder = app.add_model(|ctx| {
            ctx.subscribe_to_model(
                &detected_repos,
                |me: &mut DetectionRecorder, _, event, _| {
                    let DetectedRepositoriesEvent::DetectedGitRepo { .. } = event;
                    me.detected += 1;
                },
            );
            DetectionRecorder::default()
        });

        let temp_dir = tempfile::TempDir::new().unwrap();
        let parent = dunce::canonicalize(temp_dir.path()).unwrap();
        let main_repo = parent.join("main");
        write(&main_repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        let worktree = parent.join(".claude/worktrees/wt");
        let worktree_git_dir = main_repo.join(".git/worktrees/wt");
        write(&worktree_git_dir.join("HEAD"), "ref: refs/heads/wt\n");
        write(
            &worktree.join(".git"),
            &format!("gitdir: {}\n", worktree_git_dir.display()),
        );

        let watcher = app.add_model(RepoChangeWatcher::new);
        watcher.update(&mut app, |watcher, ctx| {
            watcher.set_watched_repos(
                vec![
                    WatchedRepo {
                        root: main_repo.clone(),
                        external_git_dir: None,
                    },
                    WatchedRepo {
                        root: worktree.clone(),
                        external_git_dir: Some(worktree_git_dir.clone()),
                    },
                ],
                ctx,
            );
        });

        let main_repository = registered_repository(&mut app, &main_repo);
        let worktree_repository = registered_repository(&mut app, &worktree);
        assert_eq!(
            main_repository.read(&app, |repo, _| repo.watcher_count()),
            1
        );
        worktree_repository.read(&app, |repo, _| {
            assert_eq!(repo.watcher_count(), 1);
            assert_eq!(repo.git_dir(), worktree_git_dir);
        });
        recorder.read(&app, |recorder, _| assert_eq!(recorder.detected, 0));
        detected_repos.read(&app, |repos, _| {
            assert_eq!(
                repos.get_root_for_path(&LocalOrRemotePath::Local(main_repo.clone())),
                None
            );
        });

        watcher.update(&mut app, |watcher, ctx| {
            watcher.set_watched_repos(
                vec![WatchedRepo {
                    root: worktree.clone(),
                    external_git_dir: Some(worktree_git_dir.clone()),
                }],
                ctx,
            );
        });
        assert_eq!(
            main_repository.read(&app, |repo, _| repo.watcher_count()),
            0
        );
        assert_eq!(
            worktree_repository.read(&app, |repo, _| repo.watcher_count()),
            1
        );

        watcher.update(&mut app, |watcher, ctx| {
            watcher.set_watched_repos(vec![], ctx)
        });
        assert_eq!(
            worktree_repository.read(&app, |repo, _| repo.watcher_count()),
            0
        );
    });
}

#[test]
fn watching_shares_an_already_registered_repository() {
    App::test((), |mut app| async move {
        app.add_singleton_model(DirectoryWatcher::new_for_testing);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let repo = dunce::canonicalize(temp_dir.path()).unwrap();
        write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        let existing = registered_repository(&mut app, &repo);

        let watcher = app.add_model(RepoChangeWatcher::new);
        watcher.update(&mut app, |watcher, ctx| {
            watcher.set_watched_repos(
                vec![WatchedRepo {
                    root: repo.clone(),
                    external_git_dir: None,
                }],
                ctx,
            );
        });

        assert_eq!(existing.read(&app, |repo, _| repo.watcher_count()), 1);
    });
}
