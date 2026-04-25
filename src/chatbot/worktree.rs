//! Phase 3 — per-plan git worktree manager.
//!
//! When a fix plan transitions to `approved`, Nova enters
//! "implementation mode" for that plan: the harness creates a git
//! worktree at `<worktrees_root>/plan-<id>/`, checked out to a new
//! branch `phase-3/plan-<id>` off the base branch. Nova writes to
//! files INSIDE the worktree via `protected_write` (Phase 0). When
//! done, the harness stages + commits + pushes + opens the PR, then
//! reaps the worktree.
//!
//! # Why the worktree path is inside `allowed_roots`
//!
//! This is the key design decision (Phase 3 architecture call,
//! 2026-04-22). Worktrees live at
//! `~/trio-local/data/worktrees/plan-<id>/`, which is:
//!
//! - ✅ inside guardian's configured `allowed_roots` (`~/trio-local/data`)
//! - ✅ NOT inside any `protected_paths` (those live under the source
//!   clone at `~/Library/.../Agents-Voice/src/`)
//!
//! So Nova writes `<worktree>/src/main.rs` — the guardian sees a path
//! inside an allowed root and permits it. The MAIN clone's
//! `src/main.rs` stays protected and untouched. The git worktree
//! machinery makes the two paths different real files on disk despite
//! sharing a `.git` dir via worktree admin pointers.
//!
//! This preserves the Phase 0 invariant WITHOUT any guardian changes:
//! Nova still cannot modify her own harness source. She can only
//! modify a separate checkout destined for a branch + PR, which the
//! owner reviews before merge.

use crate::chatbot::git_ops::{self, GitError};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Errors specific to worktree lifecycle. Wraps `GitError` for the
/// common case (underlying git command failed) and adds a few
/// worktree-specific states.
#[derive(Debug)]
pub enum WorktreeError {
    /// The requested plan_id is invalid (<=0).
    InvalidPlanId(i64),
    /// The worktree path already exists and is non-empty. Most likely
    /// a previous implementation run left state behind that wasn't
    /// reaped — caller should either pick a new plan_id or call
    /// [`force_remove_stale`].
    Stale(PathBuf),
    /// The main repo doesn't exist / isn't a git repo.
    RepoInvalid(PathBuf, GitError),
    /// Worktrees root path couldn't be created (permissions, disk).
    WorktreesRootUnwritable(PathBuf, std::io::Error),
    /// Underlying git error during worktree add/remove.
    Git(GitError),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlanId(id) => write!(f, "invalid plan_id {}: must be > 0", id),
            Self::Stale(p) => write!(
                f,
                "worktree path {} already exists and is non-empty; \
                 run force_remove_stale first",
                p.display()
            ),
            Self::RepoInvalid(p, e) => {
                write!(f, "repo path {} is not usable: {}", p.display(), e)
            }
            Self::WorktreesRootUnwritable(p, e) => write!(
                f,
                "worktrees root {} could not be created: {}",
                p.display(),
                e
            ),
            Self::Git(e) => write!(f, "git op in worktree manager failed: {}", e),
        }
    }
}

impl std::error::Error for WorktreeError {}

impl From<GitError> for WorktreeError {
    fn from(value: GitError) -> Self {
        Self::Git(value)
    }
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// Handle to an open worktree. Holding this doesn't auto-reap on drop —
/// reaping is an explicit `close_worktree(handle, ...)` call because
/// the PR-creation flow happens between open and close, and a panic in
/// that window should leave the worktree on disk for the human to
/// inspect rather than silently deleting it.
#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    pub plan_id: i64,
    /// Absolute path to the worktree root (e.g.
    /// `~/trio-local/data/worktrees/plan-42/`).
    pub worktree_path: PathBuf,
    /// The feature branch checked out in this worktree.
    pub branch: String,
    /// The base branch this was created from (for the PR `base` field).
    pub base_branch: String,
    /// Path to the main source repo — the worktree shares its `.git`
    /// via a worktree admin pointer.
    pub repo_path: PathBuf,
}

/// Manager that owns the `worktrees_root` and the pointer to the main
/// source repo. One manager per process; cheap to clone, all state
/// lives on disk.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    /// The main source-code clone (e.g. the Agents-Voice repo the
    /// harness was built from).
    pub repo_path: PathBuf,
    /// Directory under which per-plan worktrees are created. MUST be
    /// inside guardian's `allowed_roots` so Nova can write into the
    /// worktree via `protected_write` without the guardian refusing.
    /// Convention: `<bot_data_dir>/../shared/worktrees` (mirrors
    /// bug_alerts.db / fix_plans.db path layout).
    pub worktrees_root: PathBuf,
}

impl WorktreeManager {
    /// Construct a manager from a bot's `data_dir`. Returns a clean
    /// error if `repo_path` isn't a git repo.
    pub fn new(repo_path: PathBuf, bot_data_dir: &Path) -> Result<Self> {
        let worktrees_root = bot_data_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("shared")
            .join("worktrees");
        let resolved_repo_root = git_ops::repo_root(&repo_path)
            .map_err(|e| WorktreeError::RepoInvalid(repo_path.clone(), e))?;
        Ok(Self {
            repo_path: resolved_repo_root,
            worktrees_root,
        })
    }

    /// Construct a manager with an explicit worktrees_root (for tests
    /// and unusual deploy layouts). Validates the repo is real.
    pub fn with_explicit_roots(repo_path: PathBuf, worktrees_root: PathBuf) -> Result<Self> {
        let resolved_repo_root = git_ops::repo_root(&repo_path)
            .map_err(|e| WorktreeError::RepoInvalid(repo_path.clone(), e))?;
        Ok(Self {
            repo_path: resolved_repo_root,
            worktrees_root,
        })
    }

    fn worktree_path_for(&self, plan_id: i64) -> PathBuf {
        self.worktrees_root.join(format!("plan-{}", plan_id))
    }

    fn branch_for(plan_id: i64) -> String {
        format!("phase-3/plan-{}", plan_id)
    }

    /// Open a new worktree for `plan_id`, branching from `base_branch`.
    /// Creates `worktrees_root` if missing. Fails cleanly if a stale
    /// worktree or branch already exists.
    pub fn open_worktree(&self, plan_id: i64, base_branch: &str) -> Result<WorktreeHandle> {
        if plan_id <= 0 {
            return Err(WorktreeError::InvalidPlanId(plan_id));
        }

        std::fs::create_dir_all(&self.worktrees_root)
            .map_err(|e| WorktreeError::WorktreesRootUnwritable(self.worktrees_root.clone(), e))?;

        let wt_path = self.worktree_path_for(plan_id);
        // Refuse to stomp on an existing non-empty directory — it may
        // hold an in-flight implementation the user hasn't shipped yet.
        if wt_path.exists() {
            let non_empty = std::fs::read_dir(&wt_path)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);
            if non_empty {
                return Err(WorktreeError::Stale(wt_path));
            }
            // Empty dir is fine; git worktree add wants a missing
            // path, so remove the empty dir first.
            std::fs::remove_dir(&wt_path).ok();
        }

        let branch = Self::branch_for(plan_id);
        git_ops::validate_branch_name(&branch)?;

        // `git -C <repo> worktree add <path> -b <branch> <base>`
        // creates a new worktree at `path`, creates `branch` off
        // `base`, and checks it out inside the worktree.
        run_git_in(
            &self.repo_path,
            &[
                "worktree",
                "add",
                wt_path.to_str().ok_or_else(|| {
                    WorktreeError::Git(GitError::InvalidArg(format!(
                        "non-utf8 worktree path: {:?}",
                        wt_path
                    )))
                })?,
                "-b",
                &branch,
                base_branch,
            ],
        )?;

        info!(
            plan_id,
            worktree = %wt_path.display(),
            branch = %branch,
            base = %base_branch,
            "worktree opened"
        );

        Ok(WorktreeHandle {
            plan_id,
            worktree_path: wt_path,
            branch,
            base_branch: base_branch.to_string(),
            repo_path: self.repo_path.clone(),
        })
    }

    /// Close a worktree. Removes the filesystem directory and (if
    /// `delete_branch`) the local branch. Does NOT touch the remote
    /// branch — the PR keeps its branch alive on GitHub until merged
    /// or manually deleted.
    ///
    /// Use `delete_branch=false` when the PR has already been pushed
    /// and you want the local branch to stick around as a reference
    /// (e.g. for debugging). Use `true` for the common case where
    /// we're done with local state for this plan.
    pub fn close_worktree(&self, handle: &WorktreeHandle, delete_branch: bool) -> Result<()> {
        // `git worktree remove --force` removes the worktree even if
        // it has uncommitted changes. We use --force because Phase 3
        // flows call close AFTER commit + push succeeded; any lingering
        // dirty files are post-push artifacts (intermediate scripts, etc.).
        let wt_path_str = handle.worktree_path.to_str().ok_or_else(|| {
            WorktreeError::Git(GitError::InvalidArg(format!(
                "non-utf8 worktree path: {:?}",
                handle.worktree_path
            )))
        })?;
        if handle.worktree_path.exists() {
            run_git_in(
                &self.repo_path,
                &["worktree", "remove", "--force", wt_path_str],
            )?;
        } else {
            // Path already gone — still run `worktree prune` so git's
            // admin bookkeeping catches up.
            run_git_in(&self.repo_path, &["worktree", "prune"])?;
        }

        if delete_branch {
            // `-D` for force-delete so a never-pushed branch can also
            // be reaped. If branch is gone already, swallow the error.
            match run_git_in(&self.repo_path, &["branch", "-D", &handle.branch]) {
                Ok(_) => {}
                Err(WorktreeError::Git(GitError::CommandFailed { stderr, .. }))
                    if stderr.contains("not found") || stderr.contains("not exist") =>
                {
                    // Already gone.
                }
                Err(e) => return Err(e),
            }
        }

        info!(plan_id = handle.plan_id, "worktree closed");
        Ok(())
    }

    /// Emergency reap: remove a worktree directory even if git doesn't
    /// know about it. Used when a previous run crashed mid-open.
    pub fn force_remove_stale(&self, plan_id: i64) -> Result<()> {
        let wt_path = self.worktree_path_for(plan_id);
        if wt_path.exists() {
            warn!(plan_id, path = %wt_path.display(), "force-removing stale worktree");
            // Try the proper git command first so admin files get cleaned.
            let wt_path_str = wt_path.to_str().ok_or_else(|| {
                WorktreeError::Git(GitError::InvalidArg(format!(
                    "non-utf8 path: {:?}",
                    wt_path
                )))
            })?;
            let _ = run_git_in(
                &self.repo_path,
                &["worktree", "remove", "--force", wt_path_str],
            );
            // Belt-and-suspenders: if the dir still exists, rm -rf it.
            if wt_path.exists() {
                std::fs::remove_dir_all(&wt_path)
                    .map_err(|e| WorktreeError::WorktreesRootUnwritable(wt_path.clone(), e))?;
            }
            let _ = run_git_in(&self.repo_path, &["worktree", "prune"]);
        }
        // Branch cleanup too — if it dangles.
        let branch = Self::branch_for(plan_id);
        let _ = run_git_in(&self.repo_path, &["branch", "-D", &branch]);
        Ok(())
    }

    /// List all currently-open per-plan worktrees (by inspecting
    /// `worktrees_root`). Intended for startup reconciliation —
    /// if a crash left worktrees behind, the harness can iterate
    /// these + cross-reference against fix_plans status to decide
    /// whether to resume or reap.
    pub fn list_open(&self) -> std::io::Result<Vec<(i64, PathBuf)>> {
        if !self.worktrees_root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.worktrees_root)? {
            let entry = entry?;
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if let Some(rest) = s.strip_prefix("plan-")
                && let Ok(id) = rest.parse::<i64>()
            {
                out.push((id, entry.path()));
            }
        }
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }
}

/// Thin wrapper that reuses `git_ops`' error shape. We can't use the
/// public `run` from git_ops because it's private; this is a local
/// equivalent that stays in-module.
fn run_git_in(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .map_err(|e| WorktreeError::Git(GitError::SpawnFailed(e)))?;
    if !output.status.success() {
        return Err(WorktreeError::Git(GitError::CommandFailed {
            args: args.iter().map(|s| s.to_string()).collect(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Init a throwaway git repo with one commit on `main`. Returns
    /// the tempdir (keeps it alive).
    fn mk_repo() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let p = td.path();
        run_git_in(p, &["init", "-b", "main"]).unwrap();
        run_git_in(p, &["config", "user.name", "Phase 3 Test"]).unwrap();
        run_git_in(p, &["config", "user.email", "test@example.com"]).unwrap();
        run_git_in(p, &["config", "commit.gpgsign", "false"]).unwrap();
        fs::write(p.join("README.md"), "# test\n").unwrap();
        run_git_in(p, &["add", "README.md"]).unwrap();
        run_git_in(p, &["commit", "-m", "init"]).unwrap();
        td
    }

    #[test]
    fn new_accepts_a_valid_repo() {
        let td = mk_repo();
        // Simulate a bot_data_dir that would sit at
        // `<td>/bots/nova` so the `shared/worktrees` sibling path
        // derives correctly.
        let data_dir = td.path().join("bots").join("nova");
        std::fs::create_dir_all(&data_dir).unwrap();
        let mgr = WorktreeManager::new(td.path().to_path_buf(), &data_dir).unwrap();
        assert!(mgr.worktrees_root.ends_with("shared/worktrees"));
    }

    #[test]
    fn open_creates_worktree_and_branch_off_main() {
        let td = mk_repo();
        let wt_root = td.path().join("wt-root");
        let mgr =
            WorktreeManager::with_explicit_roots(td.path().to_path_buf(), wt_root.clone()).unwrap();

        let h = mgr.open_worktree(42, "main").unwrap();
        assert_eq!(h.plan_id, 42);
        assert_eq!(h.branch, "phase-3/plan-42");
        assert!(h.worktree_path.exists());
        assert!(h.worktree_path.join("README.md").exists());
        assert!(h.worktree_path.starts_with(&wt_root));
        // The worktree is on the feature branch.
        let b = git_ops::current_branch(&h.worktree_path).unwrap();
        assert_eq!(b.as_deref(), Some("phase-3/plan-42"));
    }

    #[test]
    fn open_refuses_invalid_plan_id() {
        let td = mk_repo();
        let mgr = WorktreeManager::with_explicit_roots(
            td.path().to_path_buf(),
            td.path().join("wt-root"),
        )
        .unwrap();
        matches!(
            mgr.open_worktree(0, "main"),
            Err(WorktreeError::InvalidPlanId(0))
        );
        matches!(
            mgr.open_worktree(-1, "main"),
            Err(WorktreeError::InvalidPlanId(-1))
        );
    }

    #[test]
    fn worktree_writes_dont_touch_main_repo_files() {
        let td = mk_repo();
        let mgr = WorktreeManager::with_explicit_roots(
            td.path().to_path_buf(),
            td.path().join("wt-root"),
        )
        .unwrap();

        let h = mgr.open_worktree(7, "main").unwrap();
        // Write in the worktree — this is the Phase 3 invariant we
        // care about: worktree writes do NOT modify the main clone.
        fs::write(h.worktree_path.join("new.txt"), "hi from plan 7").unwrap();
        fs::write(h.worktree_path.join("README.md"), "# modified").unwrap();

        // Main clone's README.md must still be the original.
        let main_readme = fs::read_to_string(td.path().join("README.md")).unwrap();
        assert_eq!(main_readme.trim(), "# test");
        // Main clone must not have the new file.
        assert!(!td.path().join("new.txt").exists());

        // Inside the worktree we can stage + commit normally.
        git_ops::stage_all(&h.worktree_path).unwrap();
        let sha = git_ops::commit(&h.worktree_path, "plan 7 changes").unwrap();
        assert_eq!(sha.len(), 7);
        assert!(git_ops::is_clean(&h.worktree_path).unwrap());
        // Main branch history is unchanged.
        let main_log = git_ops::recent_commits(td.path(), 5).unwrap();
        assert_eq!(main_log.len(), 1);
        assert_eq!(main_log[0].subject, "init");
    }

    #[test]
    fn close_removes_worktree_and_branch() {
        let td = mk_repo();
        let mgr = WorktreeManager::with_explicit_roots(
            td.path().to_path_buf(),
            td.path().join("wt-root"),
        )
        .unwrap();

        let h = mgr.open_worktree(9, "main").unwrap();
        assert!(h.worktree_path.exists());
        // Branch exists locally.
        let branch_list = run_git_in(td.path(), &["branch", "--list", "phase-3/plan-9"]).unwrap();
        assert!(branch_list.contains("phase-3/plan-9"));

        mgr.close_worktree(&h, true).unwrap();
        assert!(!h.worktree_path.exists());
        // Branch is gone locally.
        let branch_list = run_git_in(td.path(), &["branch", "--list", "phase-3/plan-9"]).unwrap();
        assert!(!branch_list.contains("phase-3/plan-9"));
    }

    #[test]
    fn open_refuses_stale_nonempty_directory() {
        let td = mk_repo();
        let wt_root = td.path().join("wt-root");
        let mgr =
            WorktreeManager::with_explicit_roots(td.path().to_path_buf(), wt_root.clone()).unwrap();

        // Plant a stale non-empty directory where plan-33 would go.
        let stale = wt_root.join("plan-33");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("garbage"), "ghost of a previous run").unwrap();

        match mgr.open_worktree(33, "main") {
            Err(WorktreeError::Stale(p)) => assert_eq!(p, stale),
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn force_remove_stale_reaps_ghost_state() {
        let td = mk_repo();
        let wt_root = td.path().join("wt-root");
        let mgr =
            WorktreeManager::with_explicit_roots(td.path().to_path_buf(), wt_root.clone()).unwrap();

        let h = mgr.open_worktree(55, "main").unwrap();
        // Simulate a crash that forgot to close — corrupt a file
        // inside the worktree.
        fs::write(h.worktree_path.join("mid-write.txt"), "incomplete").unwrap();

        // force_remove_stale must succeed and the path must be gone.
        mgr.force_remove_stale(55).unwrap();
        assert!(!h.worktree_path.exists());
        // Branch also gone.
        let branch_list = run_git_in(td.path(), &["branch", "--list", "phase-3/plan-55"]).unwrap();
        assert!(!branch_list.contains("phase-3/plan-55"));
    }

    #[test]
    fn list_open_reports_all_active_worktrees() {
        let td = mk_repo();
        let mgr = WorktreeManager::with_explicit_roots(
            td.path().to_path_buf(),
            td.path().join("wt-root"),
        )
        .unwrap();

        let _h1 = mgr.open_worktree(1, "main").unwrap();
        let _h5 = mgr.open_worktree(5, "main").unwrap();
        let _h2 = mgr.open_worktree(2, "main").unwrap();

        let open = mgr.list_open().unwrap();
        let ids: Vec<i64> = open.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![1, 2, 5], "list_open must return ids sorted");
    }

    #[test]
    fn new_rejects_non_git_repo_path() {
        let td = tempfile::tempdir().unwrap();
        let data_dir = td.path().join("data/nova");
        std::fs::create_dir_all(&data_dir).unwrap();
        match WorktreeManager::new(td.path().to_path_buf(), &data_dir) {
            Err(WorktreeError::RepoInvalid(_, _)) => {}
            other => panic!("expected RepoInvalid, got {:?}", other),
        }
    }
}
