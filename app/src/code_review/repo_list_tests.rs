use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
        selection_after_change(Some(Path::new("/w/b")), &visible),
        Some(PathBuf::from("/w/b"))
    );
}

#[test]
fn selection_falls_back_to_first_when_saved_missing_or_hidden() {
    let visible = [PathBuf::from("/w/a"), PathBuf::from("/w/b")];
    assert_eq!(
        selection_after_change(Some(Path::new("/w/hidden")), &visible),
        Some(PathBuf::from("/w/a"))
    );
    assert_eq!(
        selection_after_change(None, &visible),
        Some(PathBuf::from("/w/a"))
    );
}

#[test]
fn selection_is_none_when_list_empty() {
    assert_eq!(selection_after_change(Some(Path::new("/w/a")), &[]), None);
}
