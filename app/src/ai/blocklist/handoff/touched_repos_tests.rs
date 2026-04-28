//! Tests for `touched_repos.rs`.
//!
//! Only covers `find_git_root`, which actually walks the filesystem against a
//! temporary directory layout. The pure helpers (`parse_github_repo`,
//! `pick_handoff_overlap_env`) are exercised end-to-end by the handoff
//! orchestrator and don't get standalone tests — their correctness is enforced
//! by their call sites.

use super::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn find_git_root_walks_up_to_dot_git() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let nested = repo.join("src").join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(repo.join(".git")).unwrap();

    let file_in_repo = nested.join("foo.rs");
    fs::write(&file_in_repo, "").unwrap();

    let root = find_git_root(&file_in_repo).expect("root for file inside repo");
    assert_eq!(root, repo);

    let root_for_dir = find_git_root(&nested).expect("root for directory inside repo");
    assert_eq!(root_for_dir, repo);

    let outside = tmp.path().join("not_a_repo").join("file.txt");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, "").unwrap();
    assert!(find_git_root(&outside).is_none());
}
