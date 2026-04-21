//! Phase 3 end-to-end: the full worktree → commit → push-less flow
//! driven against a temp git repo (no remote, no GitHub).
//!
//! What this covers: worktree manager + git_ops composed together
//! in the exact order Nova's tools invoke them. It does NOT exercise
//! `gh pr create` — that requires authenticated GitHub access and
//! a real test repo. The `gh_cli::validate_title` / PR-number
//! parsing logic has its own unit tests.
//!
//! The canonical scenario: plan #42 is approved, Nova opens a
//! worktree, writes a new file, commits, then reaps. A second test
//! verifies the Phase 0 invariant — writes to the worktree do NOT
//! modify the main clone — under the full compose flow.

use trio::chatbot::git_ops;
use trio::chatbot::worktree::{WorktreeManager};
use serial_test::serial;
use std::fs;

/// Init a throwaway repo with a single commit on `main`.
fn mk_repo() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let p = td.path();
    assert!(std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(p)
        .status()
        .unwrap()
        .success());
    for args in [
        vec!["config", "user.name", "Phase 3 Test"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        assert!(std::process::Command::new("git")
            .args(&args)
            .current_dir(p)
            .status()
            .unwrap()
            .success());
    }
    fs::write(p.join("README.md"), "# test\n").unwrap();
    git_ops::stage_all(p).unwrap();
    git_ops::commit(p, "init").unwrap();
    td
}

#[tokio::test]
#[serial]
async fn worktree_open_write_commit_close_lifecycle() {
    let td = mk_repo();
    let wt_root = td.path().join("worktrees");
    let mgr =
        WorktreeManager::with_explicit_roots(td.path().to_path_buf(), wt_root.clone())
            .unwrap();

    // Nova enters implementation mode for plan 42.
    let handle = mgr.open_worktree(42, "main").unwrap();
    assert!(handle.worktree_path.exists());
    assert_eq!(handle.branch, "phase-3/plan-42");

    // Nova writes three files via protected_write (simulated here as
    // direct fs writes — the guardian allow-check is covered in
    // phase0_protected_write.rs; this test is about the git flow).
    fs::write(handle.worktree_path.join("new1.rs"), "// first file").unwrap();
    fs::write(handle.worktree_path.join("new2.md"), "# second file").unwrap();
    fs::create_dir_all(handle.worktree_path.join("sub")).unwrap();
    fs::write(handle.worktree_path.join("sub").join("new3.txt"), "third").unwrap();

    assert!(!git_ops::is_clean(&handle.worktree_path).unwrap(),
            "worktree must be dirty after writes");

    // commit_and_push dispatch simulation — stage + commit (skip push,
    // no remote configured on this test repo).
    git_ops::stage_all(&handle.worktree_path).unwrap();
    let sha = git_ops::commit(
        &handle.worktree_path,
        "feat(plan-42): add smoke files for Phase 3 lifecycle test",
    )
    .unwrap();
    assert_eq!(sha.len(), 7);
    assert!(git_ops::is_clean(&handle.worktree_path).unwrap(),
            "worktree must be clean after commit");

    // Log line lives on the feature branch.
    let commits = git_ops::recent_commits(&handle.worktree_path, 3).unwrap();
    assert_eq!(commits.len(), 2);
    assert!(commits[0].subject.starts_with("feat(plan-42):"));
    assert_eq!(commits[1].subject, "init");

    // Main clone's branch must still be on `init` only — the feature
    // branch history must not have leaked across.
    let main_commits = git_ops::recent_commits(td.path(), 5).unwrap();
    assert_eq!(
        main_commits.len(),
        1,
        "main clone must have only the init commit: {:?}",
        main_commits
    );

    // Clean up: worktree + branch gone, but history on main is intact.
    mgr.close_worktree(&handle, true).unwrap();
    assert!(!handle.worktree_path.exists());
    let branch_listing = std::process::Command::new("git")
        .args(["branch", "--list", &handle.branch])
        .current_dir(td.path())
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&branch_listing.stdout);
    assert!(
        !listing.contains("phase-3/plan-42"),
        "feature branch must be reaped: {}",
        listing
    );
}

#[tokio::test]
#[serial]
async fn phase0_invariant_holds_writes_to_worktree_dont_touch_main_source() {
    // Absolute backbone test. If this ever fails, the Phase 3
    // architecture call is broken and Nova can reach the main clone.
    let td = mk_repo();
    // Worktrees live in a SEPARATE tempdir so `is_clean(main_clone)`
    // doesn't see the worktree directory itself as untracked.
    let wt_td = tempfile::tempdir().unwrap();
    let mgr = WorktreeManager::with_explicit_roots(
        td.path().to_path_buf(),
        wt_td.path().to_path_buf(),
    )
    .unwrap();

    // Create an actual source file we want to "modify" via the worktree.
    let main_source = td.path().join("src.rs");
    fs::write(&main_source, "original\n").unwrap();
    git_ops::stage_all(td.path()).unwrap();
    git_ops::commit(td.path(), "add src.rs").unwrap();

    let h = mgr.open_worktree(100, "main").unwrap();

    // Scribble on the worktree's copy of src.rs.
    let worktree_src = h.worktree_path.join("src.rs");
    assert!(worktree_src.exists(), "worktree should have the source file");
    fs::write(&worktree_src, "MUTATED IN WORKTREE\n").unwrap();

    // Main clone must NOT reflect the change.
    let main_content = fs::read_to_string(&main_source).unwrap();
    assert_eq!(
        main_content.trim(),
        "original",
        "Phase 0 invariant breach: main clone was modified via worktree!"
    );

    // Confirm git sees the dirty worktree state correctly (staged or
    // not, the worktree is not clean).
    assert!(!git_ops::is_clean(&h.worktree_path).unwrap());
    // And main clone is clean.
    assert!(git_ops::is_clean(td.path()).unwrap());

    // Commit in the worktree. Still doesn't touch main.
    git_ops::stage_all(&h.worktree_path).unwrap();
    git_ops::commit(&h.worktree_path, "mutate src").unwrap();
    let main_content_after_commit = fs::read_to_string(&main_source).unwrap();
    assert_eq!(
        main_content_after_commit.trim(),
        "original",
        "committing in the worktree must not retroactively touch main"
    );

    mgr.close_worktree(&h, true).unwrap();
}

#[tokio::test]
#[serial]
async fn multiple_concurrent_worktrees_dont_interfere() {
    let td = mk_repo();
    let mgr = WorktreeManager::with_explicit_roots(
        td.path().to_path_buf(),
        td.path().join("wt"),
    )
    .unwrap();

    let h1 = mgr.open_worktree(1, "main").unwrap();
    let h5 = mgr.open_worktree(5, "main").unwrap();
    let h9 = mgr.open_worktree(9, "main").unwrap();

    // Write a unique marker in each worktree.
    fs::write(h1.worktree_path.join("who.txt"), "i am plan 1").unwrap();
    fs::write(h5.worktree_path.join("who.txt"), "i am plan 5").unwrap();
    fs::write(h9.worktree_path.join("who.txt"), "i am plan 9").unwrap();

    // Commit in each — separate histories.
    for h in [&h1, &h5, &h9] {
        git_ops::stage_all(&h.worktree_path).unwrap();
        git_ops::commit(&h.worktree_path, &format!("plan {} work", h.plan_id)).unwrap();
    }

    // Each worktree has exactly `init` + its own commit = 2 commits.
    for h in [&h1, &h5, &h9] {
        let log = git_ops::recent_commits(&h.worktree_path, 10).unwrap();
        assert_eq!(log.len(), 2, "plan {} history should have 2 commits", h.plan_id);
        let branch = git_ops::current_branch(&h.worktree_path).unwrap();
        assert_eq!(branch.as_deref(), Some(h.branch.as_str()));
    }

    // Main clone history still untouched — 1 commit.
    let main_log = git_ops::recent_commits(td.path(), 10).unwrap();
    assert_eq!(main_log.len(), 1);

    // list_open reports all three.
    let open = mgr.list_open().unwrap();
    let ids: Vec<i64> = open.iter().map(|(i, _)| *i).collect();
    assert_eq!(ids, vec![1, 5, 9]);

    // Close all — cleanup via list_open.
    for (id, _) in open {
        let handle = trio::chatbot::worktree::WorktreeHandle {
            plan_id: id,
            worktree_path: mgr.worktrees_root.join(format!("plan-{}", id)),
            branch: format!("phase-3/plan-{}", id),
            base_branch: "main".into(),
            repo_path: mgr.repo_path.clone(),
        };
        mgr.close_worktree(&handle, true).unwrap();
    }
    assert_eq!(mgr.list_open().unwrap().len(), 0);
}
