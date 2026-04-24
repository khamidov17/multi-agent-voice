use serde::{Deserialize, Serialize};

/// Wire-stable verdict schema v1. Downstream consumers: classify workflow
/// (label + status check), canary workflow, shadow-mode reconciler, weekly
/// audit digest. Renaming fields breaks all of them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub schema: u32,
    pub eligible: bool,
    pub class: Option<Class>,
    pub reason_code: ReasonCode,
    pub human_message: String,
    pub suggested_fix: String,
    pub alternative_action: Option<AlternativeAction>,
    pub protected_paths_touched: Vec<String>,
    pub toolchain_sha: Option<String>,
    pub clippy_lints_sha: Option<String>,
    pub docs_url: String,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    FmtEquiv,
    DeadCode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReasonCode {
    Ok,
    FmtDrift,
    DeadCodeExtraLines,
    ProtectedPath,
    ToolchainHashMismatch,
    ClippyDrift,
    Paused,
    ClassifierError,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AlternativeAction {
    SplitPr,
    ManualMerge,
    OutOfScope,
}

impl Verdict {
    const DOCS_BASE: &'static str = "docs/phase4-debugging.md";

    pub fn eligible_fmt(head_sha: Option<String>, base_sha: Option<String>) -> Self {
        Self {
            schema: 1,
            eligible: true,
            class: Some(Class::FmtEquiv),
            reason_code: ReasonCode::Ok,
            human_message: "Diff is byte-identical to `cargo fmt` output. Safe to auto-merge."
                .to_string(),
            suggested_fix: String::new(),
            alternative_action: None,
            protected_paths_touched: Vec::new(),
            toolchain_sha: None,
            clippy_lints_sha: None,
            docs_url: Self::DOCS_BASE.to_string(),
            head_sha,
            base_sha,
        }
    }

    pub fn ineligible_fmt_drift(drift_summary: &str) -> Self {
        Self {
            schema: 1,
            eligible: false,
            class: None,
            reason_code: ReasonCode::FmtDrift,
            human_message: format!(
                "PR contains changes that `cargo fmt` would not produce: {drift_summary}"
            ),
            suggested_fix:
                "Run `cargo fmt` locally and re-push, or remove the non-formatting changes."
                    .to_string(),
            alternative_action: Some(AlternativeAction::ManualMerge),
            protected_paths_touched: Vec::new(),
            toolchain_sha: None,
            clippy_lints_sha: None,
            docs_url: format!("{}#fmt_drift", Self::DOCS_BASE),
            head_sha: None,
            base_sha: None,
        }
    }

    pub fn ineligible_protected(touched: Vec<String>) -> Self {
        let paths_summary = touched.join(", ");
        Self {
            schema: 1,
            eligible: false,
            class: None,
            reason_code: ReasonCode::ProtectedPath,
            human_message: format!(
                "PR modifies protected path(s): {paths_summary}. Auto-merge is disabled for these paths for security reasons."
            ),
            suggested_fix:
                "Split this PR — move the protected-path changes into a separate PR that you merge manually."
                    .to_string(),
            alternative_action: Some(AlternativeAction::SplitPr),
            protected_paths_touched: touched,
            toolchain_sha: None,
            clippy_lints_sha: None,
            docs_url: format!("{}#protected_path", Self::DOCS_BASE),
            head_sha: None,
            base_sha: None,
        }
    }

    pub fn paused() -> Self {
        Self {
            schema: 1,
            eligible: false,
            class: None,
            reason_code: ReasonCode::Paused,
            human_message:
                "Auto-merge is paused (AUTOMERGE_ENABLED != 1). The classifier is running in observe-only mode."
                    .to_string(),
            suggested_fix:
                "Merge manually. To re-enable auto-merge after verifying pause was not triggered by a recent regression, set AUTOMERGE_ENABLED=1 and see docs/phase4-runbook.md."
                    .to_string(),
            alternative_action: Some(AlternativeAction::ManualMerge),
            protected_paths_touched: Vec::new(),
            toolchain_sha: None,
            clippy_lints_sha: None,
            docs_url: format!("{}#paused", Self::DOCS_BASE),
            head_sha: None,
            base_sha: None,
        }
    }

    pub fn classifier_error(detail: &str) -> Self {
        Self {
            schema: 1,
            eligible: false,
            class: None,
            reason_code: ReasonCode::ClassifierError,
            human_message: format!("Classifier failed to reach a decision: {detail}"),
            suggested_fix:
                "Check the workflow logs. Classifier errors fail closed — treat this PR as requiring manual review."
                    .to_string(),
            alternative_action: Some(AlternativeAction::ManualMerge),
            protected_paths_touched: Vec::new(),
            toolchain_sha: None,
            clippy_lints_sha: None,
            docs_url: format!("{}#classifier_error", Self::DOCS_BASE),
            head_sha: None,
            base_sha: None,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| {
            r#"{"schema":1,"eligible":false,"reason_code":"CLASSIFIER_ERROR","human_message":"verdict serialization failed"}"#.to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligible_fmt_roundtrips_to_json() {
        let v = Verdict::eligible_fmt(Some("abc123".into()), Some("def456".into()));
        let j = v.to_json();
        let parsed: Verdict = serde_json::from_str(&j).unwrap();
        assert!(parsed.eligible);
        assert_eq!(parsed.schema, 1);
        assert_eq!(parsed.reason_code, ReasonCode::Ok);
    }

    #[test]
    fn protected_path_lists_touched_paths() {
        let touched = vec![
            ".github/workflows/ci.yml".to_string(),
            "bootstrap-guardian/src/main.rs".to_string(),
        ];
        let v = Verdict::ineligible_protected(touched.clone());
        assert!(!v.eligible);
        assert_eq!(v.protected_paths_touched, touched);
        assert_eq!(v.reason_code, ReasonCode::ProtectedPath);
    }

    #[test]
    fn paused_is_not_eligible_and_has_manual_merge_alternative() {
        let v = Verdict::paused();
        assert!(!v.eligible);
        assert_eq!(v.reason_code, ReasonCode::Paused);
        assert_eq!(v.alternative_action, Some(AlternativeAction::ManualMerge));
    }

    #[test]
    fn reason_code_serializes_screaming_snake() {
        let j = serde_json::to_string(&ReasonCode::FmtDrift).unwrap();
        assert_eq!(j, "\"FMT_DRIFT\"");
        let j = serde_json::to_string(&ReasonCode::ToolchainHashMismatch).unwrap();
        assert_eq!(j, "\"TOOLCHAIN_HASH_MISMATCH\"");
    }
}
