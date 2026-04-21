//! Dispatch for the Phase 3 implementation tools:
//! `start_implementation`, `commit_and_push`, `open_pr`.
//!
//! These compose three modules into the end-to-end flow that turns
//! an `approved` fix plan into a real PR on GitHub:
//!
//! - [`worktree::WorktreeManager`] — creates + reaps the per-plan git
//!   worktree under `<shared>/worktrees/plan-<id>/`.
//! - [`git_ops`] — staging, committing, pushing inside the worktree.
//! - [`gh_cli`] — `gh pr create` with the plan's markdown as body.
//!
//! Plan state transitions happen at the right boundaries:
//! `start_implementation` does NOT transition (plan stays `approved`
//! while Nova writes files — a crash mid-implementation leaves the
//! plan approved so a rerun can pick it up). `open_pr` transitions
//! `approved → implemented` atomically after the PR URL is in hand.
//! The local worktree gets reaped on successful open_pr; failures
//! leave the worktree on disk so the human can inspect.
//!
//! # Tier-gating
//!
//! All three tools are Nova-only (`full_permissions=true`). Atlas or
//! Sentinel calling them gets a structured refusal — they have no
//! business shipping PRs.

use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::fix_plans::{self, FixPlanStatus};
use crate::chatbot::gh_cli;
use crate::chatbot::git_ops;
use crate::chatbot::worktree::WorktreeHandle;
use serde_json::json;
use std::path::PathBuf;
use tracing::info;

/// Default base branch when the caller doesn't specify one. Could be
/// promoted to a config field later if trio ever runs against repos
/// whose default branch isn't `main`.
const DEFAULT_BASE_BRANCH: &str = "main";

fn require_tier1(config: &ChatbotConfig, tool: &str) -> Result<(), String> {
    if !config.full_permissions {
        return Err(format!(
            "{} is Nova-only (requires full_permissions=true). Atlas/Sentinel \
             must not ship PRs — Nova is the only actor with repo-write authority.",
            tool
        ));
    }
    Ok(())
}

fn worktree_manager<'a>(
    config: &'a ChatbotConfig,
    tool: &str,
) -> Result<&'a std::sync::Arc<crate::chatbot::worktree::WorktreeManager>, String> {
    config
        .worktree_manager
        .as_ref()
        .ok_or_else(|| format!(
            "{} requires Phase 3 worktree_manager to be configured. Set \
             `repo_path` in nova.json so the harness can create worktrees \
             inside guardian's allowed_roots.",
            tool
        ))
}

fn fix_plans_db_path(config: &ChatbotConfig) -> Result<PathBuf, String> {
    let data_dir = config
        .data_dir
        .as_ref()
        .ok_or_else(|| "implementation dispatch: data_dir not set".to_string())?;
    Ok(fix_plans::shared_fix_plans_db_path(data_dir))
}

pub fn execute_start_implementation(
    config: &ChatbotConfig,
    plan_id: i64,
    base_branch: Option<&str>,
) -> Result<Option<String>, String> {
    require_tier1(config, "start_implementation")?;
    let mgr = worktree_manager(config, "start_implementation")?;

    // Plan must be status=approved. Refuse if Nova tries to implement
    // drafts or already-implemented plans — either is a workflow bug.
    let plans_db = fix_plans_db_path(config)?;
    let conn = rusqlite::Connection::open(&plans_db)
        .map_err(|e| format!("start_implementation: open {}: {}", plans_db.display(), e))?;
    fix_plans::init_schema(&conn)
        .map_err(|e| format!("start_implementation: init_schema: {}", e))?;
    let plan = fix_plans::get_plan(&conn, plan_id)
        .ok_or_else(|| format!("start_implementation: plan #{} not found", plan_id))?;
    if plan.status != FixPlanStatus::Approved {
        return Err(format!(
            "start_implementation refused: plan #{} is status={}, must be approved. \
             Get the owner to approve via send_fix_plan_to_owner first.",
            plan_id,
            plan.status.as_str()
        ));
    }

    let base = base_branch.unwrap_or(DEFAULT_BASE_BRANCH);
    let handle = mgr
        .open_worktree(plan_id, base)
        .map_err(|e| format!("start_implementation: open_worktree: {}", e))?;

    info!(
        plan_id,
        worktree = %handle.worktree_path.display(),
        branch = %handle.branch,
        "implementation started"
    );

    Ok(Some(
        json!({
            "plan_id": plan_id,
            "worktree_path": handle.worktree_path,
            "branch": handle.branch,
            "base_branch": handle.base_branch,
            "hint": "Call protected_write with paths rooted at worktree_path \
                     (e.g. <worktree_path>/src/main.rs). When done writing \
                     files, call commit_and_push, then open_pr."
        })
        .to_string(),
    ))
}

/// Reconstruct a `WorktreeHandle` for the given plan_id deterministically.
/// Used by commit_and_push and open_pr without needing in-memory state.
fn derive_handle(
    mgr: &crate::chatbot::worktree::WorktreeManager,
    plan_id: i64,
    base_branch: &str,
) -> WorktreeHandle {
    let worktree_path = mgr
        .worktrees_root
        .join(format!("plan-{}", plan_id));
    WorktreeHandle {
        plan_id,
        worktree_path,
        branch: format!("phase-3/plan-{}", plan_id),
        base_branch: base_branch.to_string(),
        repo_path: mgr.repo_path.clone(),
    }
}

pub fn execute_commit_and_push(
    config: &ChatbotConfig,
    plan_id: i64,
    message: &str,
) -> Result<Option<String>, String> {
    require_tier1(config, "commit_and_push")?;
    let mgr = worktree_manager(config, "commit_and_push")?;

    // Look up the plan to get the base branch (for handle reconstruction).
    let plans_db = fix_plans_db_path(config)?;
    let conn = rusqlite::Connection::open(&plans_db)
        .map_err(|e| format!("commit_and_push: open {}: {}", plans_db.display(), e))?;
    fix_plans::init_schema(&conn)
        .map_err(|e| format!("commit_and_push: init_schema: {}", e))?;
    let plan = fix_plans::get_plan(&conn, plan_id)
        .ok_or_else(|| format!("commit_and_push: plan #{} not found", plan_id))?;
    if plan.status != FixPlanStatus::Approved {
        return Err(format!(
            "commit_and_push refused: plan #{} is status={}. Must be approved \
             with an active worktree — did you call start_implementation?",
            plan_id,
            plan.status.as_str()
        ));
    }

    let handle = derive_handle(mgr, plan_id, DEFAULT_BASE_BRANCH);
    if !handle.worktree_path.exists() {
        return Err(format!(
            "commit_and_push refused: worktree for plan #{} doesn't exist at {}. \
             Call start_implementation first.",
            plan_id,
            handle.worktree_path.display()
        ));
    }

    // Guard: fail fast if there's nothing to commit. Nova forgetting to
    // write files is a common failure mode — surface it as a clear
    // error not a silent no-op.
    let clean = git_ops::is_clean(&handle.worktree_path)
        .map_err(|e| format!("commit_and_push: is_clean: {}", e))?;
    if clean {
        return Err(format!(
            "commit_and_push refused: worktree is clean for plan #{}. Did you \
             forget to call protected_write with paths inside {}?",
            plan_id,
            handle.worktree_path.display()
        ));
    }

    git_ops::stage_all(&handle.worktree_path)
        .map_err(|e| format!("commit_and_push: stage_all: {}", e))?;
    let sha = git_ops::commit(&handle.worktree_path, message)
        .map_err(|e| format!("commit_and_push: commit: {}", e))?;
    git_ops::push(&handle.worktree_path, "origin", &handle.branch, true)
        .map_err(|e| format!("commit_and_push: push: {}", e))?;

    info!(plan_id, sha = %sha, branch = %handle.branch, "commit_and_push ok");
    Ok(Some(
        json!({
            "plan_id": plan_id,
            "sha": sha,
            "branch": handle.branch,
        })
        .to_string(),
    ))
}

pub fn execute_open_pr(
    config: &ChatbotConfig,
    plan_id: i64,
    title_override: Option<&str>,
    draft: bool,
) -> Result<Option<String>, String> {
    require_tier1(config, "open_pr")?;
    let mgr = worktree_manager(config, "open_pr")?;

    let plans_db = fix_plans_db_path(config)?;
    let conn = rusqlite::Connection::open(&plans_db)
        .map_err(|e| format!("open_pr: open {}: {}", plans_db.display(), e))?;
    fix_plans::init_schema(&conn)
        .map_err(|e| format!("open_pr: init_schema: {}", e))?;
    let plan = fix_plans::get_plan(&conn, plan_id)
        .ok_or_else(|| format!("open_pr: plan #{} not found", plan_id))?;
    if plan.status != FixPlanStatus::Approved {
        return Err(format!(
            "open_pr refused: plan #{} is status={}, must be approved.",
            plan_id,
            plan.status.as_str()
        ));
    }

    let handle = derive_handle(mgr, plan_id, DEFAULT_BASE_BRANCH);
    if !handle.worktree_path.exists() {
        return Err(format!(
            "open_pr refused: worktree for plan #{} doesn't exist. \
             Run start_implementation + commit_and_push first.",
            plan_id
        ));
    }

    // Idempotency: if a PR is already open for this head branch, just
    // return its URL instead of erroring — Nova may have been killed
    // after gh pr create but before the status transition.
    if let Some(existing) = gh_cli::existing_pr_for_branch(&handle.repo_path, &handle.branch)
        .map_err(|e| format!("open_pr: existing_pr_for_branch: {}", e))?
    {
        info!(plan_id, url = %existing, "open_pr idempotent — PR already exists");
        // Still transition the plan if it hasn't already moved.
        let _ = fix_plans::update_status(
            &conn,
            plan_id,
            FixPlanStatus::Implemented,
            Some(&format!("pr already exists: {}", existing)),
        );
        return Ok(Some(
            json!({
                "plan_id": plan_id,
                "pr_url": existing,
                "note": "PR already existed; status transitioned (if not already)."
            })
            .to_string(),
        ));
    }

    let title = title_override.unwrap_or(&plan.title);
    let body = format_pr_body(&plan);
    let pr = gh_cli::create_pr(
        &handle.repo_path,
        title,
        &body,
        &handle.base_branch,
        &handle.branch,
        draft,
    )
    .map_err(|e| format!("open_pr: create_pr: {}", e))?;

    // Transition plan → implemented. If this fails the PR still
    // exists; the error surfaces to Nova and the owner can run
    // update_fix_plan_status manually.
    fix_plans::update_status(
        &conn,
        plan_id,
        FixPlanStatus::Implemented,
        Some(&format!("pr opened: {}", pr.url)),
    )
    .map_err(|e| format!("open_pr: fix-plan transition: {}", e))?;

    // Reap the local worktree (keeps remote branch for PR lifetime).
    if let Err(e) = mgr.close_worktree(&handle, true) {
        tracing::warn!(
            plan_id,
            err = %e,
            "open_pr: worktree close failed (non-fatal; PR is live)"
        );
    }

    info!(plan_id, url = %pr.url, number = pr.number, "open_pr success");
    Ok(Some(
        json!({
            "plan_id": plan_id,
            "pr_url": pr.url,
            "pr_number": pr.number,
            "new_plan_status": "implemented",
        })
        .to_string(),
    ))
}

/// Format a fix plan as the body of a GitHub PR. Harness-side so we
/// get stable output and Nova can't drift the format.
fn format_pr_body(plan: &fix_plans::FixPlanRow) -> String {
    format!(
        "## Fix plan #{} (alert #{})\n\
         \n\
         **Title:** {}\n\
         \n\
         ### Root cause\n{}\n\
         \n\
         ### Steps\n{}\n\
         \n\
         ### Risk\n{}\n\
         \n\
         ### Test plan\n{}\n\
         \n\
         ---\n\
         _Opened by trio Phase 3. Plan drafted {} · approved {}._\n\
         _Transitioning plan status to `implemented` on this PR's creation._",
        plan.id,
        plan.alert_id,
        plan.title,
        plan.root_cause.trim(),
        plan.steps.trim(),
        plan.risk.trim(),
        plan.test_plan.trim(),
        plan.created_at,
        plan.updated_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatbot::fix_plans::{FixPlanRow, FixPlanStatus};
    use serde_json::json as json_macro;

    fn row() -> FixPlanRow {
        FixPlanRow {
            id: 7,
            alert_id: 3,
            title: "fix: tighten heartbeat threshold".into(),
            root_cause: "Watchdog fires at 30s; Nova's normal sleep=60000 is misclassified.".into(),
            steps: "- raise gap threshold 30s → 90s\n- bucket gaps in the audit log".into(),
            risk: "low — detector tuning only, no runtime paths change".into(),
            test_plan: "cargo test chatbot::detectors; observe live soak 30min".into(),
            status: FixPlanStatus::Approved,
            created_at: "2026-04-21T14:00:00".into(),
            updated_at: "2026-04-21T14:05:00".into(),
            decision_note: Some("looks good".into()),
        }
    }

    #[test]
    fn format_pr_body_contains_every_plan_section() {
        let body = format_pr_body(&row());
        for needle in [
            "Fix plan #7",
            "alert #3",
            "fix: tighten heartbeat threshold",
            "Root cause",
            "Watchdog fires at 30s",
            "Steps",
            "Risk",
            "Test plan",
            "implemented",
        ] {
            assert!(
                body.contains(needle),
                "PR body missing `{}`:\n{}",
                needle,
                body
            );
        }
    }

    #[test]
    fn format_pr_body_does_not_panic_on_extreme_content() {
        // Pretend a plan has really short fields after trim.
        let mut r = row();
        r.root_cause = " ".into();
        r.steps = "".into();
        r.risk = "\n\n\n".into();
        r.test_plan = "x".into();
        let _ = format_pr_body(&r);
        // Long content too.
        r.root_cause = "a".repeat(10_000);
        let body = format_pr_body(&r);
        assert!(body.len() > 10_000);
    }

    #[test]
    fn require_tier1_rejects_non_tier1() {
        let cfg = ChatbotConfig::default(); // full_permissions=false
        assert!(require_tier1(&cfg, "start_implementation").is_err());
        let tier1 = ChatbotConfig {
            full_permissions: true,
            ..ChatbotConfig::default()
        };
        assert!(require_tier1(&tier1, "start_implementation").is_ok());
    }

    #[test]
    fn worktree_manager_missing_is_a_structured_error() {
        let cfg = ChatbotConfig {
            full_permissions: true,
            ..ChatbotConfig::default()
        };
        let err = worktree_manager(&cfg, "open_pr").unwrap_err();
        assert!(err.contains("worktree_manager"));
        assert!(err.contains("repo_path"), "error should hint at config");
        let _ = json_macro!({ "err": err }); // ensure it's stringifiable
    }
}
