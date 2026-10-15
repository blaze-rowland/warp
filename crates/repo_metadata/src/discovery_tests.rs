use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn canonical_tempdir() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dunce::canonicalize(dir.path()).unwrap();
    (dir, path)
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
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

fn discover(root: &Path) -> RepoDiscovery {
    discover_repos_with(root, Some(Path::new("/nonexistent-home")), MAX_VISITED_DIRS)
}

fn roots(result: &RepoDiscovery) -> Vec<PathBuf> {
    result.repos.iter().map(|repo| repo.root.clone()).collect()
}

#[test]
fn discovery_finds_main_repos_and_linked_worktree() {
    let (_dir, root) = canonical_tempdir();
    let first = root.join("first");
    let second = root.join("second");
    make_repo(&first);
    make_repo(&second);
    let worktree = root.join(".claude/worktrees/A");
    make_worktree(&first, &worktree, "A");

    let result = discover(&root);

    assert_eq!(
        roots(&result),
        vec![worktree.clone(), first.clone(), second]
    );
    let wt = result.repos.iter().find(|r| r.root == worktree).unwrap();
    assert_eq!(wt.main_repo_name.as_deref(), Some("first"));
    let main = result.repos.iter().find(|r| r.root == first).unwrap();
    assert_eq!(main.main_repo_name, None);
    assert!(result.repos.iter().all(|repo| repo.recency.is_some()));
    assert!(!result.truncated);
}

#[test]
fn discovery_skips_node_modules_repo() {
    let (_dir, root) = canonical_tempdir();
    let outer = root.join("outer");
    make_repo(&outer);
    make_repo(&outer.join("node_modules/pkg"));

    assert_eq!(roots(&discover(&root)), vec![outer]);
}

#[test]
fn discovery_skips_skip_listed_dirs_outside_repos() {
    let (_dir, root) = canonical_tempdir();
    make_repo(&root.join("node_modules/pkg"));
    make_repo(&root.join("target/thing"));

    let result = discover(&root);
    assert!(result.repos.is_empty());
    assert_eq!(result.visited_dirs, vec![root]);
}

#[test]
fn discovery_does_not_return_nested_repo() {
    let (_dir, root) = canonical_tempdir();
    let outer = root.join("outer");
    make_repo(&outer);
    make_repo(&outer.join("sub"));

    assert_eq!(roots(&discover(&root)), vec![outer]);
}

#[test]
fn discovery_ignores_repo_beyond_max_depth() {
    let (_dir, root) = canonical_tempdir();
    make_repo(&root.join("a/b/c/d"));
    let shallow = root.join("a/b/c");
    fs::create_dir_all(&shallow).unwrap();

    let result = discover(&root);
    assert!(result.repos.is_empty());
    assert!(!result.truncated);
    assert!(result.visited_dirs.contains(&shallow));
}

#[test]
fn discovery_returns_root_that_is_itself_a_repo() {
    let (_dir, root) = canonical_tempdir();
    make_repo(&root);

    let result = discover(&root);
    assert_eq!(roots(&result), vec![root]);
    assert!(result.visited_dirs.is_empty());
}

#[cfg(unix)]
#[test]
fn discovery_does_not_follow_symlinks() {
    let (_dir, root) = canonical_tempdir();
    let (_other_dir, other) = canonical_tempdir();
    let elsewhere = other.join("elsewhere");
    make_repo(&elsewhere);
    std::os::unix::fs::symlink(&elsewhere, root.join("link")).unwrap();

    let result = discover(&root);
    assert!(result.repos.is_empty());
    assert_eq!(result.visited_dirs, vec![root]);
}

#[cfg(unix)]
#[test]
fn discovery_skips_unreadable_dir() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, root) = canonical_tempdir();
    let locked = root.join("locked");
    make_repo(&locked.join("inner"));
    let readable = root.join("readable");
    make_repo(&readable);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let result = discover(&root);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(roots(&result), vec![readable]);
}

#[test]
fn discovery_skips_worktree_with_missing_gitdir() {
    let (_dir, root) = canonical_tempdir();
    let stale = root.join("stale");
    write(
        &stale.join(".git"),
        &format!("gitdir: {}\n", root.join("gone/.git/worktrees/x").display()),
    );

    let result = discover(&root);
    assert!(result.repos.is_empty());
    assert!(!result.visited_dirs.contains(&stale));
}

#[test]
fn discovery_skips_worktree_with_unresolved_head() {
    let (_dir, root) = canonical_tempdir();
    let main = root.join("main");
    make_repo(&main);
    let worktree = root.join("half");
    make_worktree(&main, &worktree, "half");
    fs::remove_file(main.join(".git/refs/heads/half")).unwrap();

    assert_eq!(roots(&discover(&root)), vec![main]);
}

#[test]
fn discovery_accepts_worktree_head_in_packed_refs() {
    let (_dir, root) = canonical_tempdir();
    let main = root.join("main");
    make_repo(&main);
    let worktree = root.join("packed");
    make_worktree(&main, &worktree, "packed");
    fs::remove_file(main.join(".git/refs/heads/packed")).unwrap();
    write(
        &main.join(".git/packed-refs"),
        &format!("# pack-refs with: peeled\n{SHA} refs/heads/packed\n"),
    );

    assert_eq!(roots(&discover(&root)), vec![main, worktree]);
}

#[test]
fn discovery_refuses_home_and_ancestors() {
    let (_dir, root) = canonical_tempdir();
    let home = root.join("home");
    make_repo(&home.join("project"));

    for refused in [home.as_path(), root.as_path()] {
        let result = discover_repos_with(refused, Some(&home), MAX_VISITED_DIRS);
        assert!(result.repos.is_empty(), "{refused:?}");
        assert!(result.visited_dirs.is_empty(), "{refused:?}");
    }

    let allowed = discover_repos_with(&home.join("project"), Some(&home), MAX_VISITED_DIRS);
    assert_eq!(allowed.repos.len(), 1);
}

#[test]
fn discovery_refuses_filesystem_and_volume_roots() {
    for refused in ["/", "/Volumes", "/Volumes/External"] {
        let result = discover_repos_with(Path::new(refused), None, MAX_VISITED_DIRS);
        assert!(result.repos.is_empty(), "{refused}");
        assert!(result.visited_dirs.is_empty(), "{refused}");
        assert!(!result.truncated, "{refused}");
    }
}

#[test]
fn discovery_truncates_at_visited_cap() {
    let (_dir, root) = canonical_tempdir();
    make_repo(&root.join("a"));
    for name in ["b", "c", "d", "e"] {
        fs::create_dir_all(root.join(name)).unwrap();
    }

    let result = discover_repos_with(&root, None, 3);
    assert!(result.truncated);
    assert_eq!(roots(&result), vec![root.join("a")]);
}

#[test]
fn discovery_visited_dirs_exclude_repo_contents() {
    let (_dir, root) = canonical_tempdir();
    let first = root.join("first");
    make_repo(&first);
    fs::create_dir_all(first.join("src/deep")).unwrap();
    let worktree = root.join(".claude/worktrees/A");
    make_worktree(&first, &worktree, "A");
    fs::create_dir_all(worktree.join("src")).unwrap();

    let result = discover(&root);
    assert_eq!(
        result.visited_dirs,
        vec![
            root.clone(),
            root.join(".claude"),
            root.join(".claude/worktrees"),
        ]
    );
}
