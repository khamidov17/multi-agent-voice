/// Protected-path allowlist. A PR touching ANY of these is ineligible for
/// auto-merge regardless of whether the rest of the diff is fmt-equivalent.
///
/// Keep this list in sync with `docs/phase4-setup.md`. When adding a path,
/// also add a unit test below.
///
/// Matching rules:
/// - Exact file paths compare equal (e.g. `rust-toolchain.toml`).
/// - Prefixes ending in `/` match any path under that directory.
/// - Glob `**` at the end of a prefix is treated identically to a trailing `/`.
pub const PROTECTED: &[&str] = &[
    // Phase 0 bootstrap invariant — the whole guardian crate.
    "bootstrap-guardian/",
    // CI is the trust boundary. If Nova could modify workflows, "CI green"
    // becomes attacker-controlled. See Eng-review D1.
    ".github/workflows/",
    // Deploy templates (launchd, systemd).
    "deploy/",
    // Toolchain pin — bumping invalidates the classifier's ground truth.
    // Must be a manual-merge PR with re-validation, never auto-merge.
    "rust-toolchain.toml",
    // The classifier cannot decide its own eligibility. A PR touching
    // tools/classify-pr/ always requires manual review.
    "tools/classify-pr/",
    // Root wrapper script if it exists on any branch.
    "scripts/wrapper.sh",
    // The supervisor binary (separate cargo project, runs Nova).
    "supervisor/",
];

/// Returns the subset of `changed_paths` that touch protected paths.
///
/// Takes `&[String]` rather than `&[&str]` because callers typically own
/// the path list from JSON or stdin.
pub fn find_protected_touches(changed_paths: &[String]) -> Vec<String> {
    let mut hits = Vec::new();
    for path in changed_paths {
        if is_protected(path) {
            hits.push(path.clone());
        }
    }
    hits
}

fn is_protected(path: &str) -> bool {
    // Reject obvious traversal — classifier operates on PR diffs from gh,
    // which shouldn't contain these, but fail safe if it somehow does.
    if path.contains("..") {
        return true;
    }

    for pattern in PROTECTED {
        if let Some(prefix) = pattern.strip_suffix("/**") {
            let normalized = format!("{prefix}/");
            if path.starts_with(&normalized) || path == prefix {
                return true;
            }
        } else if let Some(prefix) = pattern.strip_suffix('/') {
            let normalized = format!("{prefix}/");
            if path.starts_with(&normalized) || path == prefix {
                return true;
            }
        } else if path == *pattern {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_guardian_paths() {
        let hits = find_protected_touches(&v(&["bootstrap-guardian/src/main.rs"]));
        assert_eq!(hits, vec!["bootstrap-guardian/src/main.rs"]);
    }

    #[test]
    fn flags_workflow_paths() {
        let hits = find_protected_touches(&v(&[
            ".github/workflows/automerge-classify.yml",
            ".github/CODEOWNERS",
        ]));
        assert_eq!(hits, vec![".github/workflows/automerge-classify.yml"]);
    }

    #[test]
    fn flags_rust_toolchain_toml_exact() {
        let hits = find_protected_touches(&v(&["rust-toolchain.toml"]));
        assert_eq!(hits, vec!["rust-toolchain.toml"]);
    }

    #[test]
    fn does_not_flag_unrelated_toml() {
        let hits = find_protected_touches(&v(&["Cargo.toml", "src/lib.rs"]));
        assert!(hits.is_empty());
    }

    #[test]
    fn flags_classify_pr_self_modifications() {
        let hits = find_protected_touches(&v(&["tools/classify-pr/src/fmt_equiv.rs"]));
        assert_eq!(hits, vec!["tools/classify-pr/src/fmt_equiv.rs"]);
    }

    #[test]
    fn flags_path_traversal_attempts() {
        let hits = find_protected_touches(&v(&["src/../../etc/passwd"]));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn mixed_diff_returns_only_protected_paths() {
        let hits = find_protected_touches(&v(&[
            "src/chatbot/engine.rs",
            "bootstrap-guardian/src/guardian.rs",
            "README.md",
            ".github/workflows/ci.yml",
        ]));
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|p| p.starts_with("bootstrap-guardian/")));
        assert!(hits.iter().any(|p| p.starts_with(".github/workflows/")));
    }
}
