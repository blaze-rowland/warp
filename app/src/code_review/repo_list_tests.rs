use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use repo_metadata::TargetFile;

use super::*;
use crate::settings::{RepoListPrefs, canonical_repo_list_key};

fn entry(path: &str, secs: Option<u64>) -> RepoListEntry {
    RepoListEntry {
        path: PathBuf::from(path),
        recency: secs.map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s)),
    }
}

fn paths(entries: &[RepoListEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.path.to_str().unwrap()).collect()
}

fn prefs(pinned: &[&str], hidden: &[&str]) -> RepoListPrefs {
    RepoListPrefs {
        pinned: pinned.iter().map(|p| p.to_string()).collect(),
        hidden: hidden.iter().map(|p| p.to_string()).collect(),
    }
}

#[test]
fn pinned_first_in_pin_order_then_newest_first() {
    let entries = [
        entry("/w/old", Some(10)),
        entry("/w/pin_b", Some(5)),
        entry("/w/unknown", None),
        entry("/w/new", Some(30)),
        entry("/w/pin_a", Some(1)),
        entry("/w/mid", Some(20)),
    ];
    let ordered = order_repos(&entries, &prefs(&["/w/pin_a", "/w/pin_b"], &[]));
    assert_eq!(
        paths(&ordered.visible),
        [
            "/w/pin_a",
            "/w/pin_b",
            "/w/new",
            "/w/mid",
            "/w/old",
            "/w/unknown"
        ]
    );
    assert!(ordered.hidden.is_empty());
}

#[test]
fn equal_recency_breaks_ties_by_path() {
    let entries = [entry("/w/b", Some(1)), entry("/w/a", Some(1))];
    let ordered = order_repos(&entries, &RepoListPrefs::default());
    assert_eq!(paths(&ordered.visible), ["/w/a", "/w/b"]);
}

#[test]
fn hidden_repos_excluded_from_visible_and_listed_separately() {
    let entries = [
        entry("/w/Mobile", Some(10)),
        entry("/w/Desktop-MT3-5509", Some(50)),
    ];
    let ordered = order_repos(&entries, &prefs(&[], &["/w/Desktop-MT3-5509"]));
    assert_eq!(paths(&ordered.visible), ["/w/Mobile"]);
    assert_eq!(paths(&ordered.hidden), ["/w/Desktop-MT3-5509"]);
}

#[test]
fn pinned_path_missing_from_entries_is_ignored_but_kept() {
    let entries = [entry("/w/a", Some(1))];
    let stored = prefs(&["/w/gone", "/w/a"], &["/w/also_gone"]);
    let ordered = order_repos(&entries, &stored);
    assert_eq!(paths(&ordered.visible), ["/w/a"]);
    assert!(ordered.hidden.is_empty());
    assert_eq!(stored.pinned, ["/w/gone", "/w/a"]);
}

#[test]
fn pin_and_hide_mutations() {
    let mut p = RepoListPrefs::default();
    p.hide("/w/a");
    assert!(p.is_hidden("/w/a"));

    p.pin("/w/a");
    assert!(p.is_pinned("/w/a"));
    assert!(!p.is_hidden("/w/a"));

    p.pin("/w/b");
    p.pin("/w/a");
    assert_eq!(p.pinned, ["/w/a", "/w/b"]);

    p.hide("/w/a");
    assert!(!p.is_pinned("/w/a"));
    assert_eq!(p.hidden, ["/w/a"]);

    p.unhide("/w/a");
    p.unpin("/w/b");
    assert_eq!(p, RepoListPrefs::default());
}

#[test]
fn repo_labels_for_worktrees_and_main_checkouts() {
    assert_eq!(
        repo_label(Path::new("/w/worktrees/MB-1759"), Some("Mobile")),
        "Mobile · MB-1759"
    );
    assert_eq!(repo_label(Path::new("/w/Mobile"), None), "Mobile");

    let web_api = repo_label(Path::new("/w/web-wt/api"), Some("Web"));
    let infra_api = repo_label(Path::new("/w/infra-wt/api"), Some("Infra"));
    assert_ne!(web_api, infra_api);
}

#[test]
fn repo_list_key_matches_across_case_on_case_insensitive_fs() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("Sites").join("x");
    std::fs::create_dir_all(&real).unwrap();
    let lower = dir.path().join("sites").join("x");

    let key = canonical_repo_list_key(&real);
    assert_eq!(key, canonical_repo_list_key(Path::new(&key)));

    let case_insensitive = lower.exists();
    if case_insensitive {
        assert_eq!(canonical_repo_list_key(&lower), key);
    }
}

#[test]
fn repo_list_key_falls_back_for_missing_path() {
    let missing = Path::new("/definitely/not/a/real/repo_list/path");
    assert_eq!(canonical_repo_list_key(missing), missing.to_string_lossy());
}

#[test]
fn selection_keeps_saved_when_listed() {
    let visible = [PathBuf::from("/w/a"), PathBuf::from("/w/b")];
    assert_eq!(
        selection_after_change(Some(&PathBuf::from("/w/b")), &visible),
        Some(PathBuf::from("/w/b"))
    );
}

#[test]
fn selection_falls_back_to_first_when_saved_missing_or_hidden() {
    let visible = [PathBuf::from("/w/a"), PathBuf::from("/w/b")];
    assert_eq!(
        selection_after_change(Some(&PathBuf::from("/w/hidden")), &visible),
        Some(PathBuf::from("/w/a"))
    );
    assert_eq!(
        selection_after_change(None, &visible),
        Some(PathBuf::from("/w/a"))
    );
}

#[test]
fn selection_is_none_when_list_empty() {
    assert_eq!(
        selection_after_change(Some(&PathBuf::from("/w/a")), &[]),
        None
    );
}

#[test]
fn hiding_the_selected_repo_falls_back_to_first_visible() {
    let entries = [entry("/w/a", Some(10)), entry("/w/b", Some(20))];
    let ordered = order_repos(&entries, &prefs(&[], &["/w/b"]));
    let visible: Vec<PathBuf> = ordered.visible.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        selection_after_change(Some(&PathBuf::from("/w/b")), &visible),
        Some(PathBuf::from("/w/a"))
    );
}

#[test]
fn selection_kept_when_still_listed_after_reorder() {
    let entries = [entry("/w/a", Some(10)), entry("/w/b", Some(20))];
    let ordered = order_repos(&entries, &prefs(&["/w/a"], &[]));
    let visible: Vec<PathBuf> = ordered.visible.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        selection_after_change(Some(&PathBuf::from("/w/b")), &visible),
        Some(PathBuf::from("/w/b"))
    );
}

#[test]
fn merge_prefs_keeps_only_discovered_repos_across_parents() {
    let first = prefs(&["/p1/a", "/p1/cwd_repo"], &["/p1/b"]);
    let second = prefs(&["/p2/c"], &["/p2/gone"]);
    let discovered = [Path::new("/p1/a"), Path::new("/p1/b"), Path::new("/p2/c")];
    let merged = merge_prefs([&first, &second], &discovered);
    assert_eq!(merged.pinned, ["/p1/a", "/p2/c"]);
    assert_eq!(merged.hidden, ["/p1/b"]);
}

#[test]
fn frozen_order_keeps_rows_in_place_and_appends_new_ones() {
    let frozen = [
        PathBuf::from("/w/a"),
        PathBuf::from("/w/b"),
        PathBuf::from("/w/c"),
    ];
    // `c` was pinned and `b` hidden while the dropdown was open, and `d` appeared.
    let entries = [
        entry("/w/a", Some(30)),
        entry("/w/b", Some(20)),
        entry("/w/c", Some(10)),
        entry("/w/d", Some(40)),
    ];
    let fresh = order_repos(&entries, &prefs(&["/w/c"], &["/w/b"]));
    assert_eq!(paths(&fresh.visible), ["/w/c", "/w/d", "/w/a"]);

    let frozen_view = keep_frozen_order(fresh, &frozen);
    assert_eq!(paths(&frozen_view.visible), ["/w/a", "/w/c", "/w/d"]);
    assert_eq!(paths(&frozen_view.hidden), ["/w/b"]);
}

fn discovery(scanning: bool, truncated: bool) -> RepoDiscoveryState {
    RepoDiscoveryState {
        scanning,
        truncated,
    }
}

#[test]
fn list_state_scanning_only_while_nothing_is_listed() {
    let empty = OrderedRepos::default();
    assert_eq!(
        repo_list_state(&empty, discovery(true, false)),
        RepoListState::Scanning
    );

    let listed = order_repos(&[entry("/w/a", None)], &prefs(&[], &[]));
    assert_eq!(
        repo_list_state(&listed, discovery(true, false)),
        RepoListState::Listed { truncated: false }
    );
}

#[test]
fn list_state_empty_when_walk_found_nothing() {
    assert_eq!(
        repo_list_state(&OrderedRepos::default(), discovery(false, false)),
        RepoListState::Empty
    );
}

#[test]
fn list_state_all_hidden_when_every_repo_is_hidden() {
    let ordered = order_repos(
        &[entry("/w/a", None), entry("/w/b", None)],
        &prefs(&[], &["/w/a", "/w/b"]),
    );
    assert_eq!(
        repo_list_state(&ordered, discovery(true, false)),
        RepoListState::AllHidden
    );
}

#[test]
fn list_state_notes_truncated_walk() {
    let ordered = order_repos(&[entry("/w/a", None)], &prefs(&[], &["/w/b"]));
    assert_eq!(
        repo_list_state(&ordered, discovery(false, true)),
        RepoListState::Listed { truncated: true }
    );
}

#[test]
fn review_cwd_inside_repo_is_accepted() {
    let root = Path::new("/w/eventric/mobile");
    assert!(cwd_can_review_repo(root, root, None));
    assert!(cwd_can_review_repo(
        Path::new("/w/eventric/mobile/src"),
        root,
        None
    ));
}

#[test]
fn review_cwd_equal_to_discovery_parent_is_accepted() {
    assert!(cwd_can_review_repo(
        Path::new("/w/eventric"),
        Path::new("/w/eventric/mobile"),
        Some(Path::new("/w/eventric")),
    ));
}

#[test]
fn review_cwd_in_unrelated_folder_is_rejected() {
    assert!(!cwd_can_review_repo(
        Path::new("/w/other"),
        Path::new("/w/eventric/mobile"),
        Some(Path::new("/w/eventric")),
    ));
    // A subfolder of the discovery parent is not the parent itself.
    assert!(!cwd_can_review_repo(
        Path::new("/w/eventric/web"),
        Path::new("/w/eventric/mobile"),
        Some(Path::new("/w/eventric")),
    ));
}

#[test]
fn review_cwd_parent_folder_is_rejected_for_repo_not_discovered_there() {
    let cwd = Path::new("/w/eventric");
    let root = Path::new("/w/eventric/mobile");
    assert!(!cwd_can_review_repo(cwd, root, None));
    assert!(!cwd_can_review_repo(
        cwd,
        root,
        Some(Path::new("/w/elsewhere"))
    ));
}

fn tracked(path: &str) -> TargetFile {
    TargetFile::new(PathBuf::from(path), false)
}

fn change(chip: &mut ChangesChip, repo: &str, changed: &[&str], scope: &ChipScope) -> bool {
    let files: Vec<TargetFile> = changed.iter().map(|path| tracked(path)).collect();
    chip.on_repo_changed(Path::new(repo), &files, scope)
}

#[test]
fn change_in_another_repo_raises_chip() {
    let mut chip = ChangesChip::default();
    let scope = ChipScope {
        viewed: Some(Path::new("/w/Backend_Legacy-MON-1091")),
        hidden: &[],
    };

    assert!(change(
        &mut chip,
        "/w/Desktop",
        &["/w/Desktop/src/main.rs"],
        &scope
    ));

    assert_eq!(
        chip.pending(),
        Some(&PendingChip {
            repo: PathBuf::from("/w/Desktop"),
        })
    );
    assert_eq!(changes_chip_text("Desktop"), "Changes in Desktop");
}

#[test]
fn change_only_under_git_dir_raises_no_chip() {
    let mut chip = ChangesChip::default();
    let scope = ChipScope {
        viewed: Some(Path::new("/w/A")),
        hidden: &[],
    };

    assert!(!change(
        &mut chip,
        "/w/B",
        &["/w/B/.git/index", "/w/B/.git/refs/heads/main"],
        &scope
    ));
    assert_eq!(chip.pending(), None);
}

#[test]
fn gitignored_change_raises_no_chip() {
    let mut chip = ChangesChip::default();
    let scope = ChipScope {
        viewed: Some(Path::new("/w/A")),
        hidden: &[],
    };

    let ignored = TargetFile::new(PathBuf::from("/w/B/target/debug/app"), true);
    let raised = chip.on_repo_changed(Path::new("/w/B"), [&ignored], &scope);

    assert!(!raised);
    assert_eq!(chip.pending(), None);
}

#[test]
fn newer_change_replaces_chip() {
    let mut chip = ChangesChip::default();
    let scope = ChipScope {
        viewed: Some(Path::new("/w/A")),
        hidden: &[],
    };

    change(&mut chip, "/w/B", &["/w/B/a.rs"], &scope);
    change(&mut chip, "/w/C", &["/w/C/b.rs"], &scope);

    assert_eq!(
        chip.pending().map(|c| c.repo.as_path()),
        Some(Path::new("/w/C"))
    );
}

#[test]
fn hidden_repo_change_raises_no_chip() {
    let mut chip = ChangesChip::default();
    let hidden = [PathBuf::from("/w/Desktop-MT3-5509")];
    let scope = ChipScope {
        viewed: Some(Path::new("/w/A")),
        hidden: &hidden,
    };

    assert!(!change(
        &mut chip,
        "/w/Desktop-MT3-5509",
        &["/w/Desktop-MT3-5509/src/lib.rs"],
        &scope
    ));
    assert_eq!(chip.pending(), None);
}

#[test]
fn viewed_repo_change_raises_no_chip_and_keeps_pending() {
    let mut chip = ChangesChip::default();
    let scope = ChipScope {
        viewed: Some(Path::new("/w/A")),
        hidden: &[],
    };
    change(&mut chip, "/w/B", &["/w/B/a.rs"], &scope);

    assert!(!change(&mut chip, "/w/A", &["/w/A/a.rs"], &scope));
    assert_eq!(
        chip.pending().map(|c| c.repo.as_path()),
        Some(Path::new("/w/B"))
    );
}

#[test]
fn viewing_repo_clears_its_chip_only() {
    let mut chip = ChangesChip::default();
    let scope = ChipScope {
        viewed: Some(Path::new("/w/A")),
        hidden: &[],
    };
    change(&mut chip, "/w/B", &["/w/B/a.rs"], &scope);

    chip.clear_for(Path::new("/w/C"));
    assert!(chip.pending().is_some());

    chip.clear_for(Path::new("/w/B"));
    assert_eq!(chip.pending(), None);
}

#[test]
fn truncate_middle_keeps_both_ends() {
    assert_eq!(truncate_middle("Desktop", 10), "Desktop");
    assert_eq!(
        truncate_middle("Backend_Legacy · MON-1091", 13),
        "Backen…N-1091"
    );
    assert_eq!(
        truncate_middle("Backend_Legacy · MON-1091", 13)
            .chars()
            .count(),
        13
    );
}
