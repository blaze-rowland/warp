#![cfg(feature = "local_fs")]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use repo_metadata::repositories::{
    DetectedRepositories, DetectedRepositoriesEvent, RepoDetectionSource,
};
use repo_metadata::watcher::DirectoryWatcher;
use warp_core::features::FeatureFlag;
use warpui::{App, Entity, EntityId, ModelHandle};
use watcher::BulkFilesystemWatcherEvent;

use super::{PaneGroupRepositoryRoots, WorkingDirectoriesEvent};
use crate::code::buffer_location::LocalOrRemotePath;
use crate::code_review::diff_state::DiffMode;
use crate::pane_group::WorkingDirectoriesModel;
use crate::test_util::assert_eventually;

fn local(path: &std::path::Path) -> LocalOrRemotePath {
    LocalOrRemotePath::Local(path.to_path_buf())
}

fn local_str(path: &str) -> LocalOrRemotePath {
    LocalOrRemotePath::Local(PathBuf::from(path))
}

#[test]
fn refresh_working_directories_collapses_subroots_to_nearest_repo_root() {
    App::test((), |mut app| async move {
        let detected_repos_handle = app.add_singleton_model(|_| DetectedRepositories::default());

        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let repo_root = temp_dir.path().join("repo");
        let repo_a = repo_root.join("a");
        let repo_b = repo_root.join("b");
        fs::create_dir_all(&repo_a).expect("create repo/a");
        fs::create_dir_all(&repo_b).expect("create repo/b");

        // Use dunce::canonicalize to match the behavior of warp_util::standardized_path::StandardizedPath and normalize_cwd,
        // which strip the Windows extended-length path prefix (\\?\) for consistent comparison.
        let canonical_repo_root = dunce::canonicalize(&repo_root).expect("canonical repo root");

        // Seed DetectedRepositories so get_root_for_path resolves to this repo.
        detected_repos_handle.update(&mut app, |repos, _ctx| {
            let canonical =
                warp_util::standardized_path::StandardizedPath::from_local_canonicalized(
                    canonical_repo_root.as_path(),
                )
                .expect("canonicalized path");
            repos.insert_test_repo_root(canonical);
        });

        let pane_group_id = EntityId::new();
        let terminal_1 = EntityId::new();
        let terminal_2 = EntityId::new();

        let working_directories_handle = app.add_model(|_| WorkingDirectoriesModel::new());
        let roots: Vec<LocalOrRemotePath> =
            working_directories_handle.update(&mut app, |model, ctx| {
                model.refresh_working_directories_for_pane_group(
                    pane_group_id,
                    vec![
                        (terminal_1, LocalOrRemotePath::Local(repo_a.clone())),
                        (terminal_2, LocalOrRemotePath::Local(repo_b.clone())),
                    ],
                    vec![],
                    Some(terminal_1),
                    ctx,
                );

                model
                    .most_recent_directories_for_pane_group(pane_group_id)
                    .expect("pane group exists")
                    .map(|dir| dir.path)
                    .collect()
            });

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], local(&canonical_repo_root));
    });
}

#[test]
fn refresh_working_directories_preserves_non_repo_paths_and_dedupes() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());

        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let dir_1 = temp_dir.path().join("dir-1");
        let dir_2 = temp_dir.path().join("dir-2");
        fs::create_dir_all(&dir_1).expect("create dir-1");
        fs::create_dir_all(&dir_2).expect("create dir-2");

        // Use dunce::canonicalize to match the behavior of normalize_cwd,
        // which strips the Windows extended-length path prefix (\\?\) for consistent comparison.
        let canonical_1 = dunce::canonicalize(&dir_1).expect("canonical dir-1");
        let canonical_2 = dunce::canonicalize(&dir_2).expect("canonical dir-2");

        let pane_group_id = EntityId::new();
        let terminal_1 = EntityId::new();
        let terminal_2 = EntityId::new();
        let terminal_3 = EntityId::new();

        let working_directories_handle = app.add_model(|_| WorkingDirectoriesModel::new());
        let roots: HashSet<LocalOrRemotePath> =
            working_directories_handle.update(&mut app, |model, ctx| {
                model.refresh_working_directories_for_pane_group(
                    pane_group_id,
                    vec![
                        (terminal_1, LocalOrRemotePath::Local(dir_1.clone())),
                        (terminal_2, LocalOrRemotePath::Local(dir_2.clone())),
                        // Duplicate root should be deduped.
                        (terminal_3, LocalOrRemotePath::Local(dir_1.clone())),
                    ],
                    vec![],
                    Some(terminal_1),
                    ctx,
                );

                model
                    .most_recent_directories_for_pane_group(pane_group_id)
                    .expect("pane group exists")
                    .map(|dir| dir.path)
                    .collect()
            });

        assert_eq!(
            roots,
            HashSet::from_iter([local(&canonical_1), local(&canonical_2)]),
            "should preserve non-repo roots and dedupe exact paths"
        );
    });
}

// Regression test for GH-10598: the code review panel's manually selected
// repository must be remembered per pane group so it survives leaving and
// returning to an Agent session.
#[test]
fn selected_review_repo_is_remembered_per_pane_group() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());

        let pane_group_a = EntityId::new();
        let pane_group_b = EntityId::new();
        let repo_x = PathBuf::from("/repos/x");
        let repo_y = PathBuf::from("/repos/y");
        let repo_p = PathBuf::from("/repos/p");

        let working_directories_handle = app.add_model(|_| WorkingDirectoriesModel::new());

        // Initially nothing is saved for either pane group.
        working_directories_handle.update(&mut app, |model, _ctx| {
            assert!(model.get_selected_review_repo(pane_group_a).is_none());
            assert!(model.get_selected_review_repo(pane_group_b).is_none());
        });

        // User selects repo Y in pane group A.
        working_directories_handle.update(&mut app, |model, _ctx| {
            model.set_selected_review_repo(pane_group_a, local(&repo_y));
        });

        // The selection for A is remembered and is independent from B's.
        working_directories_handle.update(&mut app, |model, _ctx| {
            assert_eq!(
                model.get_selected_review_repo(pane_group_a).cloned(),
                Some(local(&repo_y)),
                "pane group A should remember its manual selection"
            );
            assert!(
                model.get_selected_review_repo(pane_group_b).is_none(),
                "pane group B should be untouched by selections in A"
            );
        });

        // User selects repo P in pane group B; A's selection must not change.
        working_directories_handle.update(&mut app, |model, _ctx| {
            model.set_selected_review_repo(pane_group_b, local(&repo_p));
            assert_eq!(
                model.get_selected_review_repo(pane_group_a).cloned(),
                Some(local(&repo_y)),
                "selecting in B must not clobber A's saved selection"
            );
            assert_eq!(
                model.get_selected_review_repo(pane_group_b).cloned(),
                Some(local(&repo_p)),
            );
        });

        // Updating A's selection overwrites the previous saved value for A.
        working_directories_handle.update(&mut app, |model, _ctx| {
            model.set_selected_review_repo(pane_group_a, local(&repo_x));
            assert_eq!(
                model.get_selected_review_repo(pane_group_a).cloned(),
                Some(local(&repo_x)),
            );
        });
    });
}

// Regression test for GH-10598: closing a tab (i.e. destroying a pane group)
// must clean up the saved code-review-panel selection so it cannot leak into
// or be confused with a future pane group that happens to reuse an EntityId.
#[test]
fn selected_review_repo_is_cleared_when_pane_group_is_removed() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());

        let pane_group_id = EntityId::new();
        let repo = PathBuf::from("/repos/x");

        let working_directories_handle = app.add_model(|_| WorkingDirectoriesModel::new());

        working_directories_handle.update(&mut app, |model, ctx| {
            model.set_selected_review_repo(pane_group_id, local(&repo));
            assert_eq!(
                model.get_selected_review_repo(pane_group_id).cloned(),
                Some(local(&repo)),
            );

            model.remove_pane_group(pane_group_id, ctx);
            assert!(
                model.get_selected_review_repo(pane_group_id).is_none(),
                "removing a pane group must clear its saved review-panel selection"
            );
        });
    });
}

// ── PaneGroupRepositoryRoots unit tests ──────────────────────────

#[test]
fn pane_group_repository_roots_insert_updates_both_maps() {
    let mut roots = PaneGroupRepositoryRoots::default();
    let pane_a = EntityId::new();
    let path = local_str("/repos/x");

    assert!(roots.insert(pane_a, path.clone()));
    // Re-inserting the same (pane_group, path) is a no-op.
    assert!(!roots.insert(pane_a, path.clone()));

    let forward = roots.get(pane_a).expect("pane group registered");
    assert!(forward.contains(&path), "forward map must contain the path");
    assert_eq!(
        roots.path_to_pane_groups.get(&path).cloned(),
        Some(HashSet::from_iter([pane_a])),
        "reverse map must reflect the inserted pane group"
    );
}

#[test]
fn pane_group_repository_roots_set_paths_returns_only_truly_orphaned_paths() {
    let mut roots = PaneGroupRepositoryRoots::default();
    let pane_a = EntityId::new();
    let pane_b = EntityId::new();
    let shared = local_str("/repos/shared");
    let only_a = local_str("/repos/only-a");

    // Both pane groups reference `shared`; only A references `only_a`.
    let orphans_a = roots.set_paths(pane_a, vec![shared.clone(), only_a.clone()]);
    assert!(orphans_a.is_empty(), "first insert never produces orphans");
    let orphans_b = roots.set_paths(pane_b, vec![shared.clone()]);
    assert!(orphans_b.is_empty());

    // A drops both of its paths.
    let orphans = roots.set_paths(pane_a, Vec::<LocalOrRemotePath>::new());

    // `shared` is still referenced by B, so it must not be reported as orphaned.
    // `only_a` was only referenced by A, so it must be.
    assert_eq!(
        orphans,
        vec![only_a.clone()],
        "shared paths must not appear in the orphan list"
    );

    // Reverse map: `shared` only references B; `only_a` is gone entirely.
    assert_eq!(
        roots.path_to_pane_groups.get(&shared).cloned(),
        Some(HashSet::from_iter([pane_b])),
    );
    assert!(
        !roots.path_to_pane_groups.contains_key(&only_a),
        "orphaned path must be evicted from the reverse map"
    );

    // Forward map: A is now empty (entry retained), B still owns `shared`.
    let a_forward = roots.get(pane_a).expect("pane group A entry retained");
    assert!(a_forward.is_empty(), "A's forward set should be empty");
    let b_forward = roots.get(pane_b).expect("pane group B entry retained");
    assert!(b_forward.contains(&shared));
}

#[test]
fn pane_group_repository_roots_set_paths_preserves_insertion_order() {
    let mut roots = PaneGroupRepositoryRoots::default();
    let pane = EntityId::new();
    let x = local_str("/repos/x");
    let y = local_str("/repos/y");
    let z = local_str("/repos/z");

    // Initial set in order x, y.
    let _ = roots.set_paths(pane, vec![x.clone(), y.clone()]);

    // Replace with y, z. y should keep its position; z is appended; x is removed.
    let _ = roots.set_paths(pane, vec![y.clone(), z.clone()]);

    let forward: Vec<LocalOrRemotePath> = roots
        .get(pane)
        .expect("pane group present")
        .iter()
        .cloned()
        .collect();
    assert_eq!(
        forward,
        vec![y, z],
        "existing items must keep their order; new items appended after"
    );
}

#[test]
fn pane_group_repository_roots_remove_pane_group_returns_orphans() {
    let mut roots = PaneGroupRepositoryRoots::default();
    let pane_a = EntityId::new();
    let pane_b = EntityId::new();
    let shared = local_str("/repos/shared");
    let only_a = local_str("/repos/only-a");

    let _ = roots.set_paths(pane_a, vec![shared.clone(), only_a.clone()]);
    let _ = roots.set_paths(pane_b, vec![shared.clone()]);

    // Removing A while B still references `shared` only orphans `only_a`.
    let orphans: HashSet<LocalOrRemotePath> = roots
        .remove_pane_group(pane_a)
        .expect("pane group A was present")
        .into_iter()
        .collect();
    assert_eq!(orphans, HashSet::from_iter([only_a.clone()]));
    assert!(roots.get(pane_a).is_none(), "pane group A entry is gone");

    // Removing B now orphans `shared`.
    let orphans = roots
        .remove_pane_group(pane_b)
        .expect("pane group B was present");
    assert_eq!(orphans, vec![shared.clone()]);
    assert!(roots.path_to_pane_groups.is_empty());
    assert!(roots.pane_group_to_paths.is_empty());
}

#[test]
fn pane_group_repository_roots_remove_unknown_pane_group_is_noop() {
    let mut roots = PaneGroupRepositoryRoots::default();
    let missing = EntityId::new();
    assert!(roots.remove_pane_group(missing).is_none());
}

// ── End-to-end cleanup behavior tests ────────────────────────────

/// Helper for end-to-end cleanup tests: registers the singletons required by
/// `DiffStateModel::new_local` (the `DirectoryWatcher`), prepares a temp dir,
/// seeds it as a detected repo root, and returns the canonical repo path along
/// with a fresh `WorkingDirectoriesModel` handle.
fn setup_repo(
    app: &mut warpui::App,
    detected_repos: &warpui::ModelHandle<DetectedRepositories>,
) -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    warpui::ModelHandle<WorkingDirectoriesModel>,
) {
    app.add_singleton_model(DirectoryWatcher::new_for_testing);

    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let repo_path = temp_dir.path().join("repo");
    fs::create_dir_all(&repo_path).expect("create repo dir");
    let canonical_repo = dunce::canonicalize(&repo_path).expect("canonical repo");

    detected_repos.update(app, |repos, _ctx| {
        let canonical = warp_util::standardized_path::StandardizedPath::from_local_canonicalized(
            canonical_repo.as_path(),
        )
        .expect("canonicalized path");
        repos.insert_test_repo_root(canonical);
    });

    let working_directories_handle = app.add_model(|_| WorkingDirectoriesModel::new());
    (
        temp_dir,
        repo_path,
        canonical_repo,
        working_directories_handle,
    )
}

/// Regression: closing pane group A while pane group B still references the
/// same repo must NOT drop the shared `DiffStateModel`. Before the fix,
/// `drop_unused_diff_state_models` removed the cache entry unconditionally for
/// any repo that left A's set, even when B still relied on it.
#[test]
fn shared_diff_state_model_survives_when_other_pane_group_still_references_repo() {
    App::test((), |mut app| async move {
        let detected_repos = app.add_singleton_model(|_| DetectedRepositories::default());

        let pane_group_a = EntityId::new();
        let pane_group_b = EntityId::new();
        let terminal_a = EntityId::new();
        let terminal_b = EntityId::new();

        let (_temp_dir, repo_path, canonical_repo, working_directories_handle) =
            setup_repo(&mut app, &detected_repos);

        // Both pane groups land in the same repo.
        working_directories_handle.update(&mut app, |model, ctx| {
            model.refresh_working_directories_for_pane_group(
                pane_group_a,
                vec![(terminal_a, LocalOrRemotePath::Local(repo_path.clone()))],
                vec![],
                Some(terminal_a),
                ctx,
            );
            model.refresh_working_directories_for_pane_group(
                pane_group_b,
                vec![(terminal_b, LocalOrRemotePath::Local(repo_path.clone()))],
                vec![],
                Some(terminal_b),
                ctx,
            );
        });

        // Open the shared diff state model.
        let initial_id = working_directories_handle.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&canonical_repo), None, ctx)
                .expect("local diff state model must be created")
                .id()
        });

        // Pane group A's terminals go away (close the tab path).
        working_directories_handle.update(&mut app, |model, ctx| {
            model.refresh_working_directories_for_pane_group(
                pane_group_a,
                vec![],
                vec![],
                None,
                ctx,
            );
        });

        // Re-fetching should return the SAME cached model (no re-creation).
        let after_id = working_directories_handle.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&canonical_repo), None, ctx)
                .expect("local diff state model must still be present")
                .id()
        });

        assert_eq!(
            initial_id, after_id,
            "shared DiffStateModel must survive when another pane group still references the repo"
        );
    });
}

/// When the last pane group referencing a repo navigates away, the shared
/// `DiffStateModel` is dropped from the cache, so a subsequent
/// `get_or_create_diff_state_model` creates a fresh model.
#[test]
fn diff_state_model_is_dropped_when_no_pane_group_references_repo() {
    App::test((), |mut app| async move {
        let detected_repos = app.add_singleton_model(|_| DetectedRepositories::default());

        let pane_group = EntityId::new();
        let terminal = EntityId::new();

        let (_temp_dir, repo_path, canonical_repo, working_directories_handle) =
            setup_repo(&mut app, &detected_repos);

        working_directories_handle.update(&mut app, |model, ctx| {
            model.refresh_working_directories_for_pane_group(
                pane_group,
                vec![(terminal, LocalOrRemotePath::Local(repo_path.clone()))],
                vec![],
                Some(terminal),
                ctx,
            );
        });

        let initial_id = working_directories_handle.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&canonical_repo), None, ctx)
                .expect("local diff state model must be created")
                .id()
        });

        // Only pane group leaves the repo → model is orphaned and dropped.
        working_directories_handle.update(&mut app, |model, ctx| {
            model.refresh_working_directories_for_pane_group(pane_group, vec![], vec![], None, ctx);
        });

        let after_id = working_directories_handle.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&canonical_repo), None, ctx)
                .expect("local diff state model must be re-created")
                .id()
        });

        assert_ne!(
            initial_id, after_id,
            "DiffStateModel should be dropped and re-created when no pane group references the repo"
        );
    });
}

/// `remove_pane_group` (explicit tab teardown) must respect the same refcount
/// semantics: pane group B's shared `DiffStateModel` survives when A is closed.
#[test]
fn remove_pane_group_does_not_drop_diff_state_model_shared_with_other_pane_group() {
    App::test((), |mut app| async move {
        let detected_repos = app.add_singleton_model(|_| DetectedRepositories::default());

        let pane_group_a = EntityId::new();
        let pane_group_b = EntityId::new();
        let terminal_a = EntityId::new();
        let terminal_b = EntityId::new();

        let (_temp_dir, repo_path, canonical_repo, working_directories_handle) =
            setup_repo(&mut app, &detected_repos);

        working_directories_handle.update(&mut app, |model, ctx| {
            model.refresh_working_directories_for_pane_group(
                pane_group_a,
                vec![(terminal_a, LocalOrRemotePath::Local(repo_path.clone()))],
                vec![],
                Some(terminal_a),
                ctx,
            );
            model.refresh_working_directories_for_pane_group(
                pane_group_b,
                vec![(terminal_b, LocalOrRemotePath::Local(repo_path.clone()))],
                vec![],
                Some(terminal_b),
                ctx,
            );
        });

        let initial_id = working_directories_handle.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&canonical_repo), None, ctx)
                .expect("local diff state model must be created")
                .id()
        });

        // Tear down pane group A.
        working_directories_handle.update(&mut app, |model, ctx| {
            model.remove_pane_group(pane_group_a, ctx);
        });

        let after_id = working_directories_handle.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&canonical_repo), None, ctx)
                .expect("local diff state model must still be present")
                .id()
        });

        assert_eq!(
            initial_id, after_id,
            "removing pane group A must not drop a model that pane group B still references"
        );
    });
}

#[test]
fn clear_selected_review_repo_removes_only_the_targeted_pane_group_entry() {
    App::test((), |mut app| async move {
        app.add_singleton_model(|_| DetectedRepositories::default());

        let pane_group_a = EntityId::new();
        let pane_group_b = EntityId::new();
        let repo_a = PathBuf::from("/repos/a");
        let repo_b = PathBuf::from("/repos/b");

        let working_directories_handle = app.add_model(|_| WorkingDirectoriesModel::new());

        working_directories_handle.update(&mut app, |model, _ctx| {
            model.set_selected_review_repo(pane_group_a, local(&repo_a));
            model.set_selected_review_repo(pane_group_b, local(&repo_b));

            model.clear_selected_review_repo(pane_group_a);

            assert!(model.get_selected_review_repo(pane_group_a).is_none());
            assert_eq!(
                model.get_selected_review_repo(pane_group_b).cloned(),
                Some(local(&repo_b)),
            );
        });
    });
}

// ── Repo discovery beneath non-repo working directories ──────────

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, contents).expect("write file");
}

fn make_repo(dir: &Path) {
    let git_dir = dir.join(".git");
    write(&git_dir.join("HEAD"), "ref: refs/heads/main\n");
    write(&git_dir.join("refs/heads/main"), &format!("{SHA}\n"));
}

fn make_worktree(main: &Path, worktree: &Path, name: &str) {
    let admin = main.join(".git/worktrees").join(name);
    write(&admin.join("HEAD"), &format!("ref: refs/heads/{name}\n"));
    write(&admin.join("commondir"), "../..\n");
    write(
        &admin.join("gitdir"),
        &format!("{}\n", worktree.join(".git").display()),
    );
    write(
        &main.join(".git/refs/heads").join(name),
        &format!("{SHA}\n"),
    );
    write(
        &worktree.join(".git"),
        &format!("gitdir: {}\n", admin.display()),
    );
}

/// A canonical parent folder holding repos `a` and `b`.
struct ParentFixture {
    _temp_dir: tempfile::TempDir,
    parent: PathBuf,
    repo_a: PathBuf,
    repo_b: PathBuf,
}

fn parent_with_two_repos() -> ParentFixture {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let parent = dunce::canonicalize(temp_dir.path()).expect("canonical parent");
    let repo_a = parent.join("a");
    let repo_b = parent.join("b");
    make_repo(&repo_a);
    make_repo(&repo_b);
    ParentFixture {
        _temp_dir: temp_dir,
        parent,
        repo_a,
        repo_b,
    }
}

fn setup_discovery(
    app: &mut App,
) -> (
    ModelHandle<DetectedRepositories>,
    ModelHandle<WorkingDirectoriesModel>,
) {
    let detected_repos = app.add_singleton_model(|_| DetectedRepositories::default());
    app.add_singleton_model(DirectoryWatcher::new_for_testing);
    let working_directories = app.add_model(|_| WorkingDirectoriesModel::new());
    (detected_repos, working_directories)
}

fn seed_detected_repo(
    app: &mut App,
    detected_repos: &ModelHandle<DetectedRepositories>,
    root: &Path,
) {
    detected_repos.update(app, |repos, _| {
        repos.insert_test_repo_root(
            warp_util::standardized_path::StandardizedPath::from_local_canonicalized(root)
                .expect("canonicalized path"),
        );
    });
}

fn refresh(
    app: &mut App,
    working_directories: &ModelHandle<WorkingDirectoriesModel>,
    pane_group_id: EntityId,
    cwds: &[(EntityId, &Path)],
) {
    let terminal_cwds = cwds
        .iter()
        .map(|(terminal_id, cwd)| (*terminal_id, local(cwd)))
        .collect();
    let focused = cwds.first().map(|(terminal_id, _)| *terminal_id);
    working_directories.update(app, |model, ctx| {
        model.refresh_working_directories_for_pane_group(
            pane_group_id,
            terminal_cwds,
            vec![],
            focused,
            ctx,
        );
    });
}

/// Waits for every discovery scan started so far, including rescans they queue.
async fn await_discovery_scans(
    app: &mut App,
    working_directories: &ModelHandle<WorkingDirectoriesModel>,
) {
    loop {
        let futures = working_directories.update(app, |model, _| {
            std::mem::take(&mut model.discovery_scan_futures)
        });
        if futures.is_empty() {
            return;
        }
        for future_id in futures {
            working_directories
                .update(app, |_, ctx| ctx.await_spawned_future(future_id))
                .await;
        }
    }
}

fn repos(
    app: &App,
    working_directories: &ModelHandle<WorkingDirectoriesModel>,
    pane_group_id: EntityId,
) -> HashSet<LocalOrRemotePath> {
    working_directories.read(app, |model, _| {
        model
            .most_recent_repositories_for_pane_group(pane_group_id)
            .map(|repos| repos.collect())
            .unwrap_or_default()
    })
}

fn scans_started(app: &App, working_directories: &ModelHandle<WorkingDirectoriesModel>) -> usize {
    working_directories.read(app, |model, _| model.discovery_scan_futures.len())
}

#[derive(Default)]
struct EventRecorder {
    repositories: Vec<Vec<LocalOrRemotePath>>,
    directories: Vec<Vec<LocalOrRemotePath>>,
    detection_sources: Vec<RepoDetectionSource>,
}

impl Entity for EventRecorder {
    type Event = ();
}

fn record_events(
    app: &mut App,
    working_directories: &ModelHandle<WorkingDirectoriesModel>,
    detected_repos: &ModelHandle<DetectedRepositories>,
) -> ModelHandle<EventRecorder> {
    app.add_model(|ctx| {
        ctx.subscribe_to_model(
            working_directories,
            |me: &mut EventRecorder, _, event, _| match event {
                WorkingDirectoriesEvent::RepositoriesChanged { repositories, .. } => {
                    me.repositories.push(repositories.clone());
                }
                WorkingDirectoriesEvent::DirectoriesChanged { directories, .. } => {
                    me.directories
                        .push(directories.iter().map(|dir| dir.path.clone()).collect());
                }
                WorkingDirectoriesEvent::FocusedRepoChanged { .. } => {}
            },
        );
        ctx.subscribe_to_model(detected_repos, |me: &mut EventRecorder, _, event, _| {
            let DetectedRepositoriesEvent::DetectedGitRepo { source, .. } = event;
            me.detection_sources.push(*source);
        });
        EventRecorder::default()
    })
}

fn diff_mode(
    app: &mut App,
    working_directories: &ModelHandle<WorkingDirectoriesModel>,
    root: &Path,
) -> DiffMode {
    let model = working_directories.update(app, |model, ctx| {
        model
            .get_or_create_diff_state_model(local(root), None, ctx)
            .expect("local diff state model")
    });
    model.read(app, |model, ctx| model.diff_mode(ctx))
}

#[test]
fn discovery_lists_repos_beneath_parent_without_changing_directories() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (detected_repos, working_directories) = setup_discovery(&mut app);
        let recorder = record_events(&mut app, &working_directories, &detected_repos);
        let fixture = parent_with_two_repos();
        let pane_group_id = EntityId::new();
        let terminal = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(terminal, &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        let expected = HashSet::from_iter([local(&fixture.repo_a), local(&fixture.repo_b)]);
        assert_eq!(repos(&app, &working_directories, pane_group_id), expected);
        recorder.read(&app, |recorder, _| {
            let last = recorder
                .repositories
                .last()
                .expect("RepositoriesChanged emitted");
            assert_eq!(last.iter().cloned().collect::<HashSet<_>>(), expected);
            assert_eq!(recorder.directories, vec![vec![local(&fixture.parent)]]);
        });
        working_directories.read(&app, |model, _| {
            for root in [&fixture.repo_a, &fixture.repo_b] {
                assert_eq!(
                    model.get_terminal_id_for_root_path(pane_group_id, &local(root)),
                    Some(terminal)
                );
                let info = model
                    .discovered_repo_info(pane_group_id, root)
                    .expect("discovered repo info");
                assert_eq!(info.parent, fixture.parent);
            }
            assert!(!model.discovery_state(pane_group_id).scanning);
            assert!(model.discovery_watch_counts.contains_key(&fixture.parent));
        });
    });
}

#[test]
fn discovery_does_not_scan_cwd_inside_a_repo() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        seed_detected_repo(&mut app, &detected_repos, &fixture.repo_a);
        let pane_group_id = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(EntityId::new(), &fixture.repo_a)],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        assert_eq!(
            repos(&app, &working_directories, pane_group_id),
            HashSet::from_iter([local(&fixture.repo_a)])
        );
        working_directories.read(&app, |model, _| {
            assert!(model.repo_discovery[&pane_group_id].entries.is_empty());
            assert!(model.discovery_watch_counts.is_empty());
        });
    });
}

#[test]
fn discovery_unions_parent_and_repo_panes_without_duplicates() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        seed_detected_repo(&mut app, &detected_repos, &fixture.repo_a);
        let pane_group_id = EntityId::new();
        let parent_terminal = EntityId::new();
        let repo_terminal = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[
                (parent_terminal, &fixture.parent),
                (repo_terminal, &fixture.repo_a),
            ],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        let repo_list: Vec<LocalOrRemotePath> = working_directories.read(&app, |model, _| {
            model
                .most_recent_repositories_for_pane_group(pane_group_id)
                .expect("pane group repos")
                .collect()
        });
        assert_eq!(repo_list.len(), 2);
        assert_eq!(
            repo_list.into_iter().collect::<HashSet<_>>(),
            HashSet::from_iter([local(&fixture.repo_a), local(&fixture.repo_b)])
        );
        working_directories.read(&app, |model, _| {
            assert_eq!(
                model.get_terminal_id_for_root_path(pane_group_id, &local(&fixture.repo_a)),
                Some(repo_terminal)
            );
            assert!(
                model
                    .discovered_repo_info(pane_group_id, &fixture.repo_a)
                    .is_none()
            );
            assert!(
                model
                    .discovered_repo_info(pane_group_id, &fixture.repo_b)
                    .is_some()
            );
        });
    });
}

#[test]
fn discovery_is_cleared_when_cwd_moves_to_home() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (_detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let home = dirs::home_dir().expect("home dir");
        let pane_group_id = EntityId::new();
        let terminal = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(terminal, &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;
        assert_eq!(repos(&app, &working_directories, pane_group_id).len(), 2);

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(terminal, &home)],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        assert!(repos(&app, &working_directories, pane_group_id).is_empty());
        working_directories.read(&app, |model, _| {
            assert!(model.discovery_watch_counts.is_empty());
            assert!(
                model
                    .discovered_repo_info(pane_group_id, &fixture.repo_a)
                    .is_none()
            );
        });
    });
}

#[test]
fn discovery_rescans_when_worktree_is_created_or_removed() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (_detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let worktrees = fixture.parent.join(".claude/worktrees");
        fs::create_dir_all(&worktrees).expect("create worktrees dir");
        let pane_group_id = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(EntityId::new(), &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;
        working_directories.read(&app, |model, _| {
            assert!(model.discovery_watch_counts.contains_key(&worktrees));
        });

        let worktree = worktrees.join("a-feature");
        make_worktree(&fixture.repo_a, &worktree, "a-feature");
        working_directories.update(&mut app, |model, ctx| {
            model.handle_discovery_watch_event(
                &BulkFilesystemWatcherEvent {
                    added: HashSet::from_iter([worktree.clone()]),
                    ..Default::default()
                },
                ctx,
            );
        });
        await_discovery_scans(&mut app, &working_directories).await;

        assert!(repos(&app, &working_directories, pane_group_id).contains(&local(&worktree)));
        working_directories.read(&app, |model, _| {
            let info = model
                .discovered_repo_info(pane_group_id, &worktree)
                .expect("worktree info");
            assert_eq!(info.main_repo_name.as_deref(), Some("a"));
        });

        fs::remove_dir_all(&worktree).expect("remove worktree");
        working_directories.update(&mut app, |model, ctx| {
            model.handle_discovery_watch_event(
                &BulkFilesystemWatcherEvent {
                    deleted: HashSet::from_iter([worktree.clone()]),
                    ..Default::default()
                },
                ctx,
            );
        });
        await_discovery_scans(&mut app, &working_directories).await;

        assert!(!repos(&app, &working_directories, pane_group_id).contains(&local(&worktree)));
    });
}

#[test]
fn discovery_ignores_file_changes_in_parent() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (_detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let pane_group_id = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(EntityId::new(), &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        let notes = fixture.parent.join("notes.md");
        write(&notes, "notes");
        working_directories.update(&mut app, |model, ctx| {
            model.handle_discovery_watch_event(
                &BulkFilesystemWatcherEvent {
                    added: HashSet::from_iter([notes.clone()]),
                    modified: HashSet::from_iter([notes.clone()]),
                    ..Default::default()
                },
                ctx,
            );
        });

        assert_eq!(scans_started(&app, &working_directories), 0);
    });
}

#[test]
fn discovery_ignores_file_deletions_in_parent() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (_detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let notes = fixture.parent.join("notes.md");
        write(&notes, "notes");
        let pane_group_id = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(EntityId::new(), &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        fs::remove_file(&notes).expect("remove notes");
        working_directories.update(&mut app, |model, ctx| {
            model.handle_discovery_watch_event(
                &BulkFilesystemWatcherEvent {
                    deleted: HashSet::from_iter([notes.clone()]),
                    moved: HashMap::from_iter([(
                        fixture.parent.join("renamed.md"),
                        fixture.parent.join("old.md"),
                    )]),
                    ..Default::default()
                },
                ctx,
            );
        });

        assert_eq!(scans_started(&app, &working_directories), 0);
    });
}

#[test]
fn discovered_diff_model_defaults_to_main_branch_and_skips_indexing_source() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (detected_repos, working_directories) = setup_discovery(&mut app);
        let recorder = record_events(&mut app, &working_directories, &detected_repos);
        let fixture = parent_with_two_repos();
        let cwd_repo_dir = tempfile::TempDir::new().expect("temp dir");
        let cwd_repo = dunce::canonicalize(cwd_repo_dir.path()).expect("canonical repo");
        seed_detected_repo(&mut app, &detected_repos, &cwd_repo);
        let pane_group_id = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[
                (EntityId::new(), &fixture.parent),
                (EntityId::new(), &cwd_repo),
            ],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        assert_eq!(
            diff_mode(&mut app, &working_directories, &fixture.repo_a),
            DiffMode::MainBranch
        );
        assert_eq!(
            diff_mode(&mut app, &working_directories, &cwd_repo),
            DiffMode::Head
        );

        assert_eventually!(
            recorder.read(&app, |recorder, _| !recorder.detection_sources.is_empty()),
            "discovered repo detection never completed"
        );
        recorder.read(&app, |recorder, _| {
            assert_eq!(
                recorder.detection_sources,
                vec![RepoDetectionSource::DiscoveredRepoReview]
            );
        });
    });
}

#[test]
fn discovery_is_off_without_feature_flag() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(false);
    App::test((), |mut app| async move {
        let (_detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let pane_group_id = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(EntityId::new(), &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;

        assert!(repos(&app, &working_directories, pane_group_id).is_empty());
        assert_eq!(scans_started(&app, &working_directories), 0);
    });
}

#[test]
fn unchanged_parent_cwd_scans_once() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (_detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let pane_group_id = EntityId::new();
        let terminal = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(terminal, &fixture.parent)],
        );
        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(terminal, &fixture.parent)],
        );
        assert_eq!(scans_started(&app, &working_directories), 1);

        await_discovery_scans(&mut app, &working_directories).await;
        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(terminal, &fixture.parent)],
        );
        assert_eq!(scans_started(&app, &working_directories), 0);
    });
}

#[test]
fn discovered_default_resets_to_head_when_a_pane_enters_the_repo() {
    let _flag = FeatureFlag::MultiRepoCodeReview.override_enabled(true);
    App::test((), |mut app| async move {
        let (detected_repos, working_directories) = setup_discovery(&mut app);
        let fixture = parent_with_two_repos();
        let pane_group_id = EntityId::new();
        let parent_terminal = EntityId::new();
        let other_terminal = EntityId::new();

        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[(parent_terminal, &fixture.parent)],
        );
        await_discovery_scans(&mut app, &working_directories).await;
        assert_eq!(
            diff_mode(&mut app, &working_directories, &fixture.repo_a),
            DiffMode::MainBranch
        );
        assert_eq!(
            diff_mode(&mut app, &working_directories, &fixture.repo_b),
            DiffMode::MainBranch
        );

        let user_mode = DiffMode::OtherBranch("dev".to_owned());
        let model_b = working_directories.update(&mut app, |model, ctx| {
            model
                .get_or_create_diff_state_model(local(&fixture.repo_b), None, ctx)
                .expect("model b")
        });
        model_b.update(&mut app, |model, ctx| {
            model.set_diff_mode(user_mode.clone(), false, false, None, ctx);
        });

        seed_detected_repo(&mut app, &detected_repos, &fixture.repo_a);
        seed_detected_repo(&mut app, &detected_repos, &fixture.repo_b);
        refresh(
            &mut app,
            &working_directories,
            pane_group_id,
            &[
                (parent_terminal, &fixture.parent),
                (other_terminal, &fixture.repo_a),
                (EntityId::new(), &fixture.repo_b),
            ],
        );

        assert_eq!(
            diff_mode(&mut app, &working_directories, &fixture.repo_a),
            DiffMode::Head
        );
        assert_eq!(
            diff_mode(&mut app, &working_directories, &fixture.repo_b),
            user_mode
        );
    });
}
