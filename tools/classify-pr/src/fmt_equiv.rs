use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Outcome of the fmt-equivalence check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FmtCheckResult {
    /// `cargo fmt --check` exits 0 on the PR branch — every Rust file is
    /// already formatted. This means either (a) the PR didn't change any Rust
    /// files, or (b) any Rust changes happen to already satisfy rustfmt. In
    /// both cases, fmt-equivalence is trivially true.
    AlreadyClean,

    /// `cargo fmt --check` reports drift. If the entire drift is producible
    /// by running `cargo fmt` (drift_summary describes what would change),
    /// the PR is NOT fmt-equivalent — the PR author made manual edits that
    /// rustfmt would undo. Rejected.
    DriftDetected { drift_summary: String },
}

/// Runs `cargo fmt --check` in `repo_root` and classifies the result.
///
/// Contract:
/// - Returns `AlreadyClean` when rustfmt passes.
/// - Returns `DriftDetected` when rustfmt reports diffs.
/// - Returns `Err` only on operational failures (cargo missing, repo not
///   accessible, etc.). Those become `CLASSIFIER_ERROR` at the caller.
pub fn check_with_cargo(repo_root: &Path) -> Result<FmtCheckResult> {
    let output = Command::new("cargo")
        .arg("fmt")
        .arg("--check")
        .arg("--all")
        .current_dir(repo_root)
        .output()
        .context("failed to invoke `cargo fmt --check` — is cargo on PATH?")?;

    if output.status.success() {
        return Ok(FmtCheckResult::AlreadyClean);
    }

    // `cargo fmt --check` exits non-zero when drift is detected. Stdout
    // contains a unified diff of the would-be changes. Capture a short
    // summary for the verdict's human_message.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let summary = summarize_drift(&stdout);
    Ok(FmtCheckResult::DriftDetected {
        drift_summary: summary,
    })
}

/// Pulls the first few lines of a rustfmt diff into a short, log-friendly
/// summary. Full diff goes into the workflow logs; the summary lands in the
/// verdict JSON where operators see it.
fn summarize_drift(rustfmt_stdout: &str) -> String {
    let mut files = Vec::new();
    for line in rustfmt_stdout.lines() {
        // rustfmt's check-mode diff uses `Diff in <path>:<line>:` markers.
        if let Some(rest) = line.strip_prefix("Diff in ") {
            let path = rest.trim_end_matches(':').split(':').next().unwrap_or(rest);
            if !files.contains(&path.to_string()) {
                files.push(path.to_string());
            }
        }
    }

    if files.is_empty() {
        return "rustfmt reported drift (see workflow log for diff)".to_string();
    }

    if files.len() <= 3 {
        format!("rustfmt would modify: {}", files.join(", "))
    } else {
        format!(
            "rustfmt would modify {} files including {}",
            files.len(),
            files[..3].join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_drift_empty_stdout() {
        let s = summarize_drift("");
        assert!(s.contains("rustfmt reported drift"));
    }

    #[test]
    fn summarize_drift_single_file() {
        let stdout = "Diff in src/lib.rs:42:\n\
                      -use foo;\n\
                      +use bar;\n";
        let s = summarize_drift(stdout);
        assert!(s.contains("src/lib.rs"));
    }

    #[test]
    fn summarize_drift_multi_file_caps_at_three() {
        let stdout =
            "Diff in a.rs:1:\nDiff in b.rs:2:\nDiff in c.rs:3:\nDiff in d.rs:4:\nDiff in e.rs:5:\n";
        let s = summarize_drift(stdout);
        assert!(s.contains("5 files"));
        assert!(s.contains("a.rs"));
        assert!(s.contains("b.rs"));
        assert!(s.contains("c.rs"));
        assert!(!s.contains("d.rs"));
    }
}
