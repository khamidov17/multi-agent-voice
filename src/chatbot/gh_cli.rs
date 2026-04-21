//! Phase 3 — thin wrapper around the `gh` CLI for GitHub PR operations.
//!
//! Same philosophy as `git_ops`: shell out to the canonical binary
//! rather than bring in octocrab. `gh` is already on every dev
//! machine that builds trio, its auth is managed by the user (same
//! keychain/token story as git), and the surface we need is small:
//! `gh pr create`, `gh pr view`, and a PATH check.
//!
//! # Auth
//!
//! `gh auth status` determines whether the user is logged in; we
//! don't touch credentials from this process. Errors from `gh pr
//! create` when not-logged-in surface the same way as any other
//! non-zero exit: as a [`GhError::CommandFailed`] whose `stderr`
//! tells the owner exactly what to do.
//!
//! # Input safety
//!
//! Every caller-controlled value (title, body, base, head branch)
//! gets validated or sanitized before being passed to `gh`. Branch
//! names reuse `git_ops::validate_branch_name`. Title is capped at
//! a conservative length and stripped of CR/LF (can't forge body).
//! Body is unbounded but passed via stdin so there's no arg-length
//! explosion risk.

use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;
use tracing::{debug, warn};

use crate::chatbot::git_ops;

#[derive(Debug)]
pub enum GhError {
    CommandFailed {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    SpawnFailed(std::io::Error),
    InvalidArg(String),
    UnexpectedOutput(String),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GhError::CommandFailed { args, status, stderr } => write!(
                f,
                "gh {} failed (status={:?}): {}",
                args.join(" "),
                status,
                stderr.trim()
            ),
            GhError::SpawnFailed(e) => write!(f, "gh spawn failed: {}", e),
            GhError::InvalidArg(s) => write!(f, "gh invalid arg: {}", s),
            GhError::UnexpectedOutput(s) => write!(f, "gh unexpected output: {}", s),
        }
    }
}

impl std::error::Error for GhError {}

pub type Result<T> = std::result::Result<T, GhError>;

/// Is `gh` on PATH? Cheap PATH probe.
pub fn gh_available() -> bool {
    match Command::new("gh").arg("--version").output() {
        Ok(out) => out.status.success(),
        Err(e) => {
            warn!(err = %e, "gh not available on PATH");
            false
        }
    }
}

/// Does the local gh session have an authenticated user? True when
/// `gh auth status` exits 0.
pub fn gh_authed() -> bool {
    match Command::new("gh").args(["auth", "status"]).output() {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// PR title validation. Rules:
/// - non-empty after trim
/// - ≤ 200 chars (GitHub hard-caps at 256 but we stay well under)
/// - no CR/LF (can't forge body or spoof headers)
/// - not starting with `-` (can't be parsed as a flag by any shell
///   tooling that composes with us later)
pub fn validate_title(title: &str) -> Result<()> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(GhError::InvalidArg("pr title empty".to_string()));
    }
    if trimmed.starts_with('-') {
        return Err(GhError::InvalidArg(format!(
            "pr title cannot start with '-': {}",
            trimmed
        )));
    }
    if trimmed.chars().count() > 200 {
        return Err(GhError::InvalidArg("pr title > 200 chars".to_string()));
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(GhError::InvalidArg(
            "pr title contains newline — strip it".to_string(),
        ));
    }
    Ok(())
}

/// Result of a successful `gh pr create`.
#[derive(Debug, Clone)]
pub struct PrCreated {
    /// Full URL returned by gh (e.g. https://github.com/owner/repo/pull/42).
    pub url: String,
    /// The numeric PR number, extracted from the URL.
    pub number: u64,
}

/// Create a PR via `gh pr create --title T --body B --base BASE
/// --head HEAD`. The `body` is passed through stdin via `--body-file -`
/// to avoid arg-length issues with long plan bodies.
pub fn create_pr(
    repo_dir: &Path,
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    draft: bool,
) -> Result<PrCreated> {
    validate_title(title)?;
    git_ops::validate_branch_name(base)
        .map_err(|e| GhError::InvalidArg(format!("bad base branch: {}", e)))?;
    git_ops::validate_branch_name(head)
        .map_err(|e| GhError::InvalidArg(format!("bad head branch: {}", e)))?;

    let mut args: Vec<String> = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        title.trim().to_string(),
        "--body-file".into(),
        "-".into(), // read body from stdin
        "--base".into(),
        base.to_string(),
        "--head".into(),
        head.to_string(),
    ];
    if draft {
        args.push("--draft".into());
    }

    let mut child = Command::new("gh")
        .args(&args)
        .current_dir(repo_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(GhError::SpawnFailed)?;

    // Feed the body on stdin. gh reads until EOF.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes()).map_err(GhError::SpawnFailed)?;
    }
    let output = child.wait_with_output().map_err(GhError::SpawnFailed)?;
    if !output.status.success() {
        return Err(GhError::CommandFailed {
            args: args.clone(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    let url = String::from_utf8_lossy(&output.stdout)
        .lines()
        // gh prints the URL on the last non-empty line.
        .rev()
        .find(|l| l.starts_with("http"))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| {
            GhError::UnexpectedOutput(format!(
                "no PR url in gh output: {}",
                String::from_utf8_lossy(&output.stdout)
            ))
        })?;
    let number = extract_pr_number(&url).ok_or_else(|| {
        GhError::UnexpectedOutput(format!("could not parse PR number from url: {}", url))
    })?;
    debug!(url = %url, number, "gh pr created");
    Ok(PrCreated { url, number })
}

/// Parse `https://github.com/owner/repo/pull/42` → `42`. Accepts
/// trailing slashes and query strings.
pub fn extract_pr_number(url: &str) -> Option<u64> {
    let after = url.rsplit("/pull/").next()?;
    // `after` might be `42`, `42/`, `42?x=y`, `42/files`.
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Quick "is there already an open PR for `head_branch`?" check.
/// Returns the PR URL if one exists, None otherwise. Uses `gh pr list
/// --head <branch> --state open --json url --jq '.[0].url'`.
pub fn existing_pr_for_branch(
    repo_dir: &Path,
    head_branch: &str,
) -> Result<Option<String>> {
    git_ops::validate_branch_name(head_branch)
        .map_err(|e| GhError::InvalidArg(format!("bad head branch: {}", e)))?;
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            head_branch,
            "--state",
            "open",
            "--json",
            "url",
            "--jq",
            ".[0].url // empty",
        ])
        .current_dir(repo_dir)
        .output()
        .map_err(GhError::SpawnFailed)?;
    if !output.status.success() {
        return Err(GhError::CommandFailed {
            args: vec!["pr".into(), "list".into(), "--head".into(), head_branch.into()],
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Ok(None)
    } else {
        Ok(Some(stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_title_accepts_reasonable_titles() {
        assert!(validate_title("fix: tighten heartbeat threshold").is_ok());
        assert!(validate_title("WIP: Phase 3 slice 3 — gh wrapper").is_ok());
    }

    #[test]
    fn validate_title_rejects_garbage() {
        assert!(validate_title("").is_err());
        assert!(validate_title("   ").is_err());
        assert!(validate_title("-injected").is_err());
        assert!(validate_title("has\nnewline").is_err());
        assert!(validate_title("has\rcarriage").is_err());
        let long = "a".repeat(201);
        assert!(validate_title(&long).is_err());
    }

    #[test]
    fn extract_pr_number_parses_standard_url() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42"),
            Some(42)
        );
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42/"),
            Some(42)
        );
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42/files"),
            Some(42)
        );
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42?x=1"),
            Some(42)
        );
    }

    #[test]
    fn extract_pr_number_returns_none_for_non_pr_urls() {
        assert_eq!(extract_pr_number(""), None);
        assert_eq!(extract_pr_number("https://example.com/other/path"), None);
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo"),
            None
        );
    }
}
