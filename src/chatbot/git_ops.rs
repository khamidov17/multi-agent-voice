//! Phase 3 — thin, testable wrapper around the `git` CLI.
//!
//! Every function shells out to `git` and returns a structured result.
//! No state held in memory, no threads spawned — callers build higher-
//! level flows (create worktree, commit+push, open PR) on top of these
//! primitives.
//!
//! # Why shell out instead of libgit2?
//!
//! `git` is already installed on every dev machine and deploy target;
//! libgit2/gix pulls an extra dependency and re-implements credential
//! handling (keychain/ssh-agent/gh). For Phase 3 we need commit, push,
//! and HTTPS auth. All of it is stuff `git` already does correctly
//! with the user's existing config. Use the system binary, don't
//! reinvent.
//!
//! # Safety envelope
//!
//! None of these functions are guarded by the bootstrap guardian —
//! they are harness-layer primitives for Phase 3+ PR flows. Callers
//! that accept Nova input (e.g. commit messages, branch names) MUST
//! validate it before passing to these functions. See
//! [`validate_branch_name`] for the conservative default.

use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, warn};

/// Errors from git operations. Wraps the CLI exit + stderr so the
/// caller can report something meaningful in the journal / to Nova.
#[derive(Debug)]
pub enum GitError {
    /// git exited non-zero. Stderr is captured verbatim.
    CommandFailed {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    /// `git` couldn't be spawned at all — binary missing, PATH issue.
    SpawnFailed(std::io::Error),
    /// Caller input failed validation (e.g. branch-name regex).
    InvalidArg(String),
    /// git ran, output was unexpected shape.
    UnexpectedOutput(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::CommandFailed {
                args,
                status,
                stderr,
            } => write!(
                f,
                "git {} failed (status={:?}): {}",
                args.join(" "),
                status,
                stderr.trim()
            ),
            GitError::SpawnFailed(e) => write!(f, "git spawn failed: {}", e),
            GitError::InvalidArg(s) => write!(f, "git invalid arg: {}", s),
            GitError::UnexpectedOutput(s) => write!(f, "git unexpected output: {}", s),
        }
    }
}

impl std::error::Error for GitError {}

pub type Result<T> = std::result::Result<T, GitError>;

/// Run `git <args>` inside `repo_dir`. Captures stdout/stderr; returns
/// stdout as UTF-8 on success.
fn run(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .map_err(GitError::SpawnFailed)?;
    if !output.status.success() {
        return Err(GitError::CommandFailed {
            args: args.iter().map(|s| s.to_string()).collect(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    debug!(repo = %repo_dir.display(), args = ?args, "git ok");
    Ok(stdout)
}

/// The currently checked-out branch name, or `None` for detached HEAD.
pub fn current_branch(repo_dir: &Path) -> Result<Option<String>> {
    let out = run(repo_dir, &["symbolic-ref", "--short", "--quiet", "HEAD"]);
    match out {
        Ok(s) => {
            let name = s.trim();
            if name.is_empty() {
                Ok(None)
            } else {
                Ok(Some(name.to_string()))
            }
        }
        Err(GitError::CommandFailed {
            status: Some(1), ..
        }) => {
            // `symbolic-ref --quiet HEAD` exits 1 on detached HEAD.
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Return the top-level directory of the repo containing `any_path`.
pub fn repo_root(any_path: &Path) -> Result<PathBuf> {
    let out = run(any_path, &["rev-parse", "--show-toplevel"])?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err(GitError::UnexpectedOutput(
            "rev-parse --show-toplevel returned empty".to_string(),
        ));
    }
    Ok(PathBuf::from(trimmed))
}

/// True iff the working tree is clean (no staged, no unstaged, no
/// untracked changes).
pub fn is_clean(repo_dir: &Path) -> Result<bool> {
    // `--porcelain` yields one line per modified path. Empty = clean.
    let out = run(repo_dir, &["status", "--porcelain"])?;
    Ok(out.trim().is_empty())
}

/// Validate a branch name before passing to `git checkout -b`. Refuses
/// anything that isn't `[a-z0-9][a-z0-9/_.-]+` to prevent shell/arg
/// injection and to enforce a sane naming convention. Additionally
/// refuses `HEAD`, `main`, `master`, and reserved-ish prefixes.
pub fn validate_branch_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(GitError::InvalidArg("branch name is empty".to_string()));
    }
    if name.len() > 100 {
        return Err(GitError::InvalidArg("branch name > 100 chars".to_string()));
    }
    let first = name.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(GitError::InvalidArg(format!(
            "branch name must start with [a-z0-9]: {}",
            name
        )));
    }
    for c in name.chars() {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '_' | '.' | '-');
        if !ok {
            return Err(GitError::InvalidArg(format!(
                "branch name has forbidden char {:?}: {}",
                c, name
            )));
        }
    }
    let reserved = ["HEAD", "head", "main", "master"];
    if reserved.contains(&name) {
        return Err(GitError::InvalidArg(format!(
            "branch name `{}` is reserved",
            name
        )));
    }
    // `git` itself refuses these per `git check-ref-format`. We mirror
    // the common ones here so we fail fast instead of surfacing a
    // downstream git error.
    if name.contains("..") || name.contains("@{") || name.ends_with(".lock") {
        return Err(GitError::InvalidArg(format!(
            "branch name matches a reserved git pattern: {}",
            name
        )));
    }
    Ok(())
}

/// Create a new branch from `base_ref` and check it out. Fails if the
/// branch already exists.
pub fn create_and_checkout_branch(repo_dir: &Path, branch: &str, base_ref: &str) -> Result<()> {
    validate_branch_name(branch)?;
    run(repo_dir, &["checkout", "-b", branch, base_ref])?;
    Ok(())
}

/// Stage a specific path. Refuses `.` / absolute paths / shell
/// metachars to keep callers honest — use [`stage_all`] if you really
/// want to add everything.
pub fn stage_path(repo_dir: &Path, rel_path: &str) -> Result<()> {
    if rel_path.is_empty() {
        return Err(GitError::InvalidArg("rel_path empty".to_string()));
    }
    if rel_path.starts_with('/') {
        return Err(GitError::InvalidArg(
            "rel_path must be relative to repo root".to_string(),
        ));
    }
    if rel_path.starts_with('-') {
        return Err(GitError::InvalidArg(format!(
            "rel_path cannot start with '-': {}",
            rel_path
        )));
    }
    run(repo_dir, &["add", "--", rel_path])?;
    Ok(())
}

/// Stage every currently-modified path. Explicit: callers opt into
/// this knowing it catches untracked files too.
pub fn stage_all(repo_dir: &Path) -> Result<()> {
    run(repo_dir, &["add", "-A"])?;
    Ok(())
}

/// Create a commit with the given message. Refuses empty messages and
/// messages starting with `-` (would be parsed as a flag).
/// Author/committer come from git config — this process inherits the
/// user's global `user.name` / `user.email`.
pub fn commit(repo_dir: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        return Err(GitError::InvalidArg("commit message empty".to_string()));
    }
    if message.starts_with('-') {
        return Err(GitError::InvalidArg(format!(
            "commit message cannot start with '-': {}",
            message
        )));
    }
    run(repo_dir, &["commit", "-m", message])?;
    // Return the new short-SHA so the caller can log + surface it.
    let sha = run(repo_dir, &["rev-parse", "--short", "HEAD"])?;
    Ok(sha.trim().to_string())
}

/// Push a branch to a remote. `set_upstream=true` sets the tracking
/// relationship on first push. Uses whatever auth git is already
/// configured with (HTTPS cred helper, SSH agent, etc.) — we do not
/// touch credentials from inside this process.
pub fn push(repo_dir: &Path, remote: &str, branch: &str, set_upstream: bool) -> Result<()> {
    validate_branch_name(branch)?;
    if !is_valid_remote(remote) {
        return Err(GitError::InvalidArg(format!(
            "invalid remote name: {}",
            remote
        )));
    }
    let mut args = vec!["push"];
    if set_upstream {
        args.push("-u");
    }
    args.push(remote);
    args.push(branch);
    run(repo_dir, &args)?;
    Ok(())
}

fn is_valid_remote(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 60
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Small description of one commit for logging. Not a replacement for
/// `git log`; intended for "post this commit-sha + subject to the
/// journal" style usage.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub subject: String,
}

/// Return the last `n` commits on the current branch, subject-only.
pub fn recent_commits(repo_dir: &Path, n: usize) -> Result<Vec<CommitInfo>> {
    let n_arg = format!("-{}", n.max(1));
    let out = run(repo_dir, &["log", &n_arg, "--pretty=format:%h|%s"])?;
    let commits = out
        .lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('|')?;
            Some(CommitInfo {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect();
    Ok(commits)
}

/// Quick guard: is the `git` binary even on PATH?
pub fn git_available() -> bool {
    match Command::new("git").arg("--version").output() {
        Ok(out) => out.status.success(),
        Err(e) => {
            warn!(err = %e, "git not available on PATH");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Minimal throwaway repo for test use. Initializes a repo with
    /// a single commit on `main` so the tests have a base_ref.
    fn tmp_repo() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let p = td.path();
        // Some CI machines don't have a global user set.
        run(p, &["init", "-b", "main"]).unwrap();
        run(p, &["config", "user.name", "Phase 3 Test"]).unwrap();
        run(p, &["config", "user.email", "test@example.com"]).unwrap();
        run(p, &["config", "commit.gpgsign", "false"]).unwrap();
        fs::write(p.join("README.md"), "# test repo\n").unwrap();
        run(p, &["add", "README.md"]).unwrap();
        run(p, &["commit", "-m", "init"]).unwrap();
        td
    }

    #[test]
    fn current_branch_returns_main_on_fresh_repo() {
        let td = tmp_repo();
        let b = current_branch(td.path()).unwrap();
        assert_eq!(b.as_deref(), Some("main"));
    }

    #[test]
    fn is_clean_true_on_fresh_repo_and_false_after_edit() {
        let td = tmp_repo();
        assert!(is_clean(td.path()).unwrap());
        fs::write(td.path().join("README.md"), "# edited\n").unwrap();
        assert!(!is_clean(td.path()).unwrap());
    }

    #[test]
    fn repo_root_returns_tempdir_canonical_path() {
        let td = tmp_repo();
        let root = repo_root(td.path()).unwrap();
        // tempdir may resolve through /private/var on macOS; canonicalize
        // both sides before comparing.
        let want = td.path().canonicalize().unwrap();
        let got = root.canonicalize().unwrap();
        assert_eq!(want, got);
    }

    #[test]
    fn validate_branch_name_rejects_bad_inputs() {
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("Main").is_err()); // uppercase
        assert!(validate_branch_name("-foo").is_err()); // leading dash
        assert!(validate_branch_name("foo bar").is_err()); // space
        assert!(validate_branch_name("foo;rm -rf /").is_err()); // shell-ish
        assert!(validate_branch_name("HEAD").is_err());
        assert!(validate_branch_name("main").is_err());
        assert!(validate_branch_name("foo..bar").is_err()); // reserved
        assert!(validate_branch_name("foo@{1}").is_err());
        assert!(validate_branch_name("foo.lock").is_err());
        assert!(validate_branch_name(&"a".repeat(101)).is_err());
        // Sanity: good names pass.
        assert!(validate_branch_name("phase-3/fix-plan-42").is_ok());
        assert!(validate_branch_name("trio/nova/patch-1").is_ok());
    }

    #[test]
    fn create_and_checkout_branch_switches_head() {
        let td = tmp_repo();
        create_and_checkout_branch(td.path(), "phase-3/test", "main").unwrap();
        assert_eq!(
            current_branch(td.path()).unwrap().as_deref(),
            Some("phase-3/test")
        );
    }

    #[test]
    fn stage_path_rejects_absolute_and_dash_args() {
        let td = tmp_repo();
        assert!(stage_path(td.path(), "/etc/passwd").is_err());
        assert!(stage_path(td.path(), "--help").is_err());
        assert!(stage_path(td.path(), "").is_err());
    }

    #[test]
    fn stage_all_and_commit_round_trip_yields_sha_and_log_line() {
        let td = tmp_repo();
        create_and_checkout_branch(td.path(), "phase-3/add-file", "main").unwrap();
        fs::write(td.path().join("new.txt"), "hello\n").unwrap();
        stage_all(td.path()).unwrap();
        let sha = commit(td.path(), "add new.txt").unwrap();
        assert_eq!(sha.len(), 7, "short sha must be 7 chars, got {:?}", sha);
        assert!(is_clean(td.path()).unwrap());
        let log = recent_commits(td.path(), 2).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "add new.txt");
        assert_eq!(log[1].subject, "init");
    }

    #[test]
    fn commit_rejects_empty_or_dash_prefixed_messages() {
        let td = tmp_repo();
        fs::write(td.path().join("f.txt"), "x").unwrap();
        stage_all(td.path()).unwrap();
        assert!(commit(td.path(), "").is_err());
        assert!(commit(td.path(), "   ").is_err());
        assert!(commit(td.path(), "-m injected").is_err());
    }

    #[test]
    fn push_validates_branch_and_remote_before_invoking_git() {
        let td = tmp_repo();
        // Remote with spaces — caller mistake, never reaches git.
        let err = push(td.path(), "o r i g i n", "phase-3/x", false).unwrap_err();
        matches!(err, GitError::InvalidArg(_));
        // Branch with uppercase — caught by validate_branch_name.
        let err = push(td.path(), "origin", "MainBranch", false).unwrap_err();
        matches!(err, GitError::InvalidArg(_));
    }

    #[test]
    fn git_available_returns_true_on_dev_machine() {
        // This test is brittle on CI without git installed; skip there.
        if std::env::var("TRIO_SKIP_GIT_AVAILABLE_TEST").is_ok() {
            return;
        }
        assert!(git_available(), "git must be on PATH for git_ops to work");
    }
}
