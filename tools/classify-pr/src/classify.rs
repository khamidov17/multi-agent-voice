use anyhow::{Context, Result};

use crate::fmt_equiv::{self, FmtCheckResult};
use crate::protected_paths;
use crate::verdict::Verdict;

/// Classifier input: what the workflow hands us per PR.
pub struct ClassifyInput {
    pub repo_root: std::path::PathBuf,
    pub changed_paths: Vec<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    /// `AUTOMERGE_ENABLED` repo variable. Fail-safe: ANY value other than
    /// Some("1") means paused. Missing/unreadable = paused (the workflow
    /// passes None when it can't read the variable).
    pub automerge_enabled: Option<String>,
}

/// Returns the verdict for this PR. Never fails for policy reasons —
/// ineligibility is a verdict, not an error. Returns `Err` only for
/// operational failures (cargo missing, repo unreadable). The caller
/// converts operational errors into `CLASSIFIER_ERROR` verdicts that fail
/// closed.
pub fn classify(input: &ClassifyInput) -> Result<Verdict> {
    // Gate 1: paused. Checked FIRST so a paused system never expends work
    // running rustfmt on every PR push. AUTOMERGE_ENABLED=1 (explicit, positive
    // polarity) is the only value that lets classification proceed.
    if input.automerge_enabled.as_deref() != Some("1") {
        return Ok(Verdict::paused());
    }

    // Gate 2: protected paths. Checked BEFORE fmt so we never shell out to
    // rustfmt on PRs we were never going to auto-merge anyway.
    let touched = protected_paths::find_protected_touches(&input.changed_paths);
    if !touched.is_empty() {
        return Ok(Verdict::ineligible_protected(touched));
    }

    // Gate 3: fmt-equivalence. This shells out to `cargo fmt --check`.
    let fmt_result = fmt_equiv::check_with_cargo(&input.repo_root)
        .context("fmt-equivalence check failed to run")?;

    match fmt_result {
        FmtCheckResult::AlreadyClean => Ok(Verdict::eligible_fmt(
            input.head_sha.clone(),
            input.base_sha.clone(),
        )),
        FmtCheckResult::DriftDetected { drift_summary } => {
            Ok(Verdict::ineligible_fmt_drift(&drift_summary))
        }
    }
}

/// Convenience wrapper: classify, then map to an exit code per `exit_codes`.
pub fn classify_and_exit_code(input: &ClassifyInput) -> (Verdict, i32) {
    match classify(input) {
        Ok(v) => {
            let code = exit_code_for(&v);
            (v, code)
        }
        Err(e) => {
            let v = Verdict::classifier_error(&format!("{e:#}"));
            (v, crate::exit_codes::OPERATIONAL_ERROR)
        }
    }
}

fn exit_code_for(v: &Verdict) -> i32 {
    use crate::exit_codes::*;
    use crate::verdict::ReasonCode::*;
    match v.reason_code {
        Ok => ELIGIBLE,
        FmtDrift | DeadCodeExtraLines | ClippyDrift => INELIGIBLE,
        ProtectedPath => PROTECTED_PATH,
        ToolchainHashMismatch => TOOLCHAIN_DRIFT,
        Paused => PAUSED,
        ClassifierError => OPERATIONAL_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn paused_when_flag_unset() {
        let d = tempdir();
        let input = ClassifyInput {
            repo_root: d.path().to_path_buf(),
            changed_paths: vec!["README.md".into()],
            head_sha: None,
            base_sha: None,
            automerge_enabled: None,
        };
        let (v, code) = classify_and_exit_code(&input);
        assert!(!v.eligible);
        assert_eq!(code, crate::exit_codes::PAUSED);
    }

    #[test]
    fn paused_when_flag_zero() {
        let d = tempdir();
        let input = ClassifyInput {
            repo_root: d.path().to_path_buf(),
            changed_paths: vec!["README.md".into()],
            head_sha: None,
            base_sha: None,
            automerge_enabled: Some("0".into()),
        };
        let (_, code) = classify_and_exit_code(&input);
        assert_eq!(code, crate::exit_codes::PAUSED);
    }

    #[test]
    fn paused_when_flag_is_anything_other_than_one() {
        let d = tempdir();
        for weird in ["true", "yes", "ENABLED", "", " 1", "1 ", "01"] {
            let input = ClassifyInput {
                repo_root: d.path().to_path_buf(),
                changed_paths: vec!["README.md".into()],
                head_sha: None,
                base_sha: None,
                automerge_enabled: Some(weird.into()),
            };
            let (v, code) = classify_and_exit_code(&input);
            assert!(
                !v.eligible,
                "should be paused for AUTOMERGE_ENABLED={weird:?}"
            );
            assert_eq!(code, crate::exit_codes::PAUSED);
        }
    }

    #[test]
    fn protected_path_blocks_before_fmt_check() {
        // Uses a tempdir with no Cargo project. If fmt check ran, it would
        // fail operationally. Protected-path gate should short-circuit.
        let d = tempdir();
        let input = ClassifyInput {
            repo_root: d.path().to_path_buf(),
            changed_paths: vec!["bootstrap-guardian/src/main.rs".into()],
            head_sha: None,
            base_sha: None,
            automerge_enabled: Some("1".into()),
        };
        let (v, code) = classify_and_exit_code(&input);
        assert!(!v.eligible);
        assert_eq!(code, crate::exit_codes::PROTECTED_PATH);
        assert_eq!(v.protected_paths_touched.len(), 1);
    }

    #[test]
    fn protected_path_reports_all_touched() {
        let d = tempdir();
        let input = ClassifyInput {
            repo_root: d.path().to_path_buf(),
            changed_paths: vec![
                "README.md".into(),
                ".github/workflows/automerge-classify.yml".into(),
                "bootstrap-guardian/src/guardian.rs".into(),
                "src/chatbot/engine.rs".into(),
            ],
            head_sha: None,
            base_sha: None,
            automerge_enabled: Some("1".into()),
        };
        let (v, _) = classify_and_exit_code(&input);
        assert_eq!(v.protected_paths_touched.len(), 2);
    }

    fn exit_code_for_test(v: &Verdict) -> i32 {
        exit_code_for(v)
    }

    #[test]
    fn exit_codes_cover_every_reason_code() {
        // Regression guard: if someone adds a ReasonCode variant and forgets
        // to wire it into exit_code_for, this test exhaustively checks that
        // every variant maps to a specific exit code. Relies on the match in
        // exit_code_for being exhaustive — compile-time enforced — but we
        // still sanity-check the mapped values here.
        use crate::exit_codes as ec;
        use crate::verdict::ReasonCode;
        let pairs: &[(ReasonCode, i32)] = &[
            (ReasonCode::Ok, ec::ELIGIBLE),
            (ReasonCode::FmtDrift, ec::INELIGIBLE),
            (ReasonCode::DeadCodeExtraLines, ec::INELIGIBLE),
            (ReasonCode::ClippyDrift, ec::INELIGIBLE),
            (ReasonCode::ProtectedPath, ec::PROTECTED_PATH),
            (ReasonCode::ToolchainHashMismatch, ec::TOOLCHAIN_DRIFT),
            (ReasonCode::Paused, ec::PAUSED),
            (ReasonCode::ClassifierError, ec::OPERATIONAL_ERROR),
        ];
        for (rc, expected) in pairs {
            let v = Verdict {
                schema: 1,
                eligible: false,
                class: None,
                reason_code: *rc,
                human_message: String::new(),
                suggested_fix: String::new(),
                alternative_action: None,
                protected_paths_touched: Vec::new(),
                toolchain_sha: None,
                clippy_lints_sha: None,
                docs_url: String::new(),
                head_sha: None,
                base_sha: None,
            };
            assert_eq!(
                exit_code_for_test(&v),
                *expected,
                "exit code for {rc:?} should be {expected}"
            );
        }
    }
}
