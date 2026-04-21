//! Dispatch for the Phase 2 fix-plan tools: `draft_fix_plan`,
//! `list_fix_plans`, `update_fix_plan_status`, `send_fix_plan_to_owner`.
//!
//! Nova-only (Tier 1). The dispatch mirrors `triage.rs` — a per-call
//! read/write connection on `data/shared/fix_plans.db`, since the
//! `FixPlansWriter` is write-side only and Nova's tool calls need
//! synchronous results (e.g. the new plan id).

use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::fix_plans::{
    self, FixPlan, FixPlanRow, FixPlanStatus,
};
use crate::chatbot::telegram::TelegramClient;
use serde_json::json;
use std::path::PathBuf;

const LIST_DEFAULT_LIMIT: i64 = 50;
const LIST_MAX_LIMIT: i64 = 500;

fn db_path(config: &ChatbotConfig) -> Result<PathBuf, String> {
    let data_dir = config
        .data_dir
        .as_ref()
        .ok_or_else(|| "fix_plans dispatch: data_dir not set in ChatbotConfig".to_string())?;
    Ok(fix_plans::shared_fix_plans_db_path(data_dir))
}

fn require_tier1(config: &ChatbotConfig, tool: &str) -> Result<(), String> {
    if !config.full_permissions {
        return Err(format!(
            "{} is Nova-only (requires full_permissions=true). Tier-2 bots \
             must not draft fix plans; Nova is the only actor.",
            tool
        ));
    }
    Ok(())
}

fn open(config: &ChatbotConfig, tool: &str) -> Result<rusqlite::Connection, String> {
    let p = db_path(config)?;
    let conn = rusqlite::Connection::open(&p)
        .map_err(|e| format!("{}: open {}: {}", tool, p.display(), e))?;
    fix_plans::init_schema(&conn)
        .map_err(|e| format!("{}: init_schema: {}", tool, e))?;
    Ok(conn)
}

pub fn execute_draft_fix_plan(
    config: &ChatbotConfig,
    alert_id: i64,
    title: &str,
    root_cause: &str,
    steps: &str,
    risk: &str,
    test_plan: &str,
) -> Result<Option<String>, String> {
    require_tier1(config, "draft_fix_plan")?;
    let conn = open(config, "draft_fix_plan")?;
    let plan = FixPlan {
        alert_id,
        title: title.to_string(),
        root_cause: root_cause.to_string(),
        steps: steps.to_string(),
        risk: risk.to_string(),
        test_plan: test_plan.to_string(),
    };
    match fix_plans::draft_plan(&conn, &plan) {
        Ok(row) => Ok(Some(json!({
            "plan_id": row.id,
            "status": row.status.as_str(),
            "alert_id": row.alert_id,
            "created_at": row.created_at,
        }).to_string())),
        // Surface the "non-terminal predecessor" case as a structured
        // response so Nova can reason about it, not as an opaque error.
        Err(fix_plans::DraftError::NonTerminalExists { existing_id, existing_status }) => {
            Ok(Some(json!({
                "error": "non_terminal_plan_exists",
                "existing_id": existing_id,
                "existing_status": existing_status,
                "hint": "Call update_fix_plan_status with new_status='obsolete' \
                        on the existing plan, or send it to the owner first \
                        to move past draft. Then retry this draft."
            }).to_string()))
        }
        Err(e) => Err(format!("draft_fix_plan: {}", e)),
    }
}

pub fn execute_list_fix_plans(
    config: &ChatbotConfig,
    status: Option<&str>,
    limit: Option<i64>,
) -> Result<Option<String>, String> {
    require_tier1(config, "list_fix_plans")?;
    let conn = open(config, "list_fix_plans")?;
    let status_filter = match status {
        Some(s) => Some(FixPlanStatus::parse(s).ok_or_else(|| {
            format!("list_fix_plans: unknown status '{}'", s)
        })?),
        None => None,
    };
    let effective_limit = limit
        .map(|n| n.clamp(1, LIST_MAX_LIMIT))
        .unwrap_or(LIST_DEFAULT_LIMIT);
    let rows = fix_plans::list_plans(&conn, status_filter, Some(effective_limit))
        .map_err(|e| format!("list_fix_plans: {}", e))?;
    Ok(Some(json!({
        "total": rows.len(),
        "plans": rows.iter().map(plan_to_json).collect::<Vec<_>>(),
    }).to_string()))
}

fn plan_to_json(p: &FixPlanRow) -> serde_json::Value {
    json!({
        "id": p.id,
        "alert_id": p.alert_id,
        "title": p.title,
        "root_cause": p.root_cause,
        "steps": p.steps,
        "risk": p.risk,
        "test_plan": p.test_plan,
        "status": p.status.as_str(),
        "created_at": p.created_at,
        "updated_at": p.updated_at,
        "decision_note": p.decision_note,
    })
}

pub fn execute_update_fix_plan_status(
    config: &ChatbotConfig,
    plan_id: i64,
    new_status: &str,
    note: Option<&str>,
) -> Result<Option<String>, String> {
    require_tier1(config, "update_fix_plan_status")?;
    let new_status_parsed = FixPlanStatus::parse(new_status).ok_or_else(|| {
        format!(
            "update_fix_plan_status: unknown status '{}'. Valid: draft, sent, \
             approved, rejected, obsolete, implemented.",
            new_status
        )
    })?;
    let conn = open(config, "update_fix_plan_status")?;
    match fix_plans::update_status(&conn, plan_id, new_status_parsed, note) {
        Ok(row) => Ok(Some(json!({
            "plan_id": row.id,
            "status": row.status.as_str(),
            "updated_at": row.updated_at,
        }).to_string())),
        Err(e) => Err(format!("update_fix_plan_status: {}", e)),
    }
}

pub async fn execute_send_fix_plan_to_owner(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    plan_id: i64,
    chat_id: i64,
    preamble: &str,
) -> Result<Option<String>, String> {
    require_tier1(config, "send_fix_plan_to_owner")?;
    if let Some(owner_id) = config.owner_user_id
        && chat_id != owner_id
    {
        return Err(format!(
            "send_fix_plan_to_owner refused: chat_id={} is not the owner ({}). \
             Fix plans go to the owner's DM only — they contain proposed code \
             changes that the owner must approve.",
            chat_id, owner_id
        ));
    }
    let conn = open(config, "send_fix_plan_to_owner")?;
    let plan = fix_plans::get_plan(&conn, plan_id)
        .ok_or_else(|| format!("send_fix_plan_to_owner: plan #{} not found", plan_id))?;
    // Must be in a state that's legal to send from. draft is the normal
    // case; re-send from 'sent' is refused (owner already has it).
    if plan.status != FixPlanStatus::Draft {
        return Err(format!(
            "send_fix_plan_to_owner refused: plan #{} is {}, can only send from \
             'draft'. If you need to resend, draft a new plan.",
            plan_id,
            plan.status.as_str()
        ));
    }

    let body = format_plan_markdown(preamble, &plan);
    let sent = telegram
        .send_message(chat_id, &body, None)
        .await
        .map_err(|e| format!("send_fix_plan_to_owner: telegram send: {}", e))?;

    // Transition draft → sent atomically after the Telegram write
    // succeeded. If this update fails, the plan is still sent but the
    // status lags; a follow-up `update_fix_plan_status` call fixes it.
    fix_plans::update_status(
        &conn,
        plan_id,
        FixPlanStatus::Sent,
        Some(&format!("sent to owner (telegram msg {})", sent)),
    )
    .map_err(|e| format!("send_fix_plan_to_owner: status transition: {}", e))?;

    Ok(Some(json!({
        "sent_message_id": sent,
        "plan_id": plan_id,
        "new_status": "sent",
    }).to_string()))
}

/// Format a plan as Telegram markdown. Harness-side so Nova can't drift
/// the output format accidentally.
fn format_plan_markdown(preamble: &str, p: &FixPlanRow) -> String {
    let mut out = String::new();
    let pre = preamble.trim();
    if !pre.is_empty() {
        out.push_str(pre);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "📋 *Fix plan #{}* (alert #{})\n",
        p.id, p.alert_id
    ));
    out.push_str(&format!("*{}*\n\n", p.title));
    out.push_str("*Root cause*\n");
    out.push_str(p.root_cause.trim());
    out.push_str("\n\n");
    out.push_str("*Steps*\n");
    out.push_str(p.steps.trim());
    out.push_str("\n\n");
    out.push_str("*Risk*\n");
    out.push_str(p.risk.trim());
    out.push_str("\n\n");
    out.push_str("*Test plan*\n");
    out.push_str(p.test_plan.trim());
    out.push_str(
        "\n\n_Reply with `approve #",
    );
    out.push_str(&p.id.to_string());
    out.push_str(
        "` or `reject #",
    );
    out.push_str(&p.id.to_string());
    out.push_str("` + reason._");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatbot::fix_plans::FixPlanStatus;

    fn row() -> FixPlanRow {
        FixPlanRow {
            id: 3,
            alert_id: 17,
            title: "tighten the heartbeat threshold".into(),
            root_cause: "Nova sleeps for 60s; watchdog fires at 30s. False alarms.".into(),
            steps: "- raise gap threshold to 90s\n- add metric for gap distribution".into(),
            risk: "low — detector tuning only".into(),
            test_plan: "cargo test chatbot::detectors; live-soak 30 min".into(),
            status: FixPlanStatus::Draft,
            created_at: "2026-04-21T14:00:00".into(),
            updated_at: "2026-04-21T14:00:00".into(),
            decision_note: None,
        }
    }

    #[test]
    fn format_plan_markdown_includes_all_sections_and_approval_hint() {
        let out = format_plan_markdown("FYI:", &row());
        for expected in [
            "FYI:",
            "Fix plan #3",
            "tighten the heartbeat threshold",
            "Root cause",
            "Steps",
            "Risk",
            "Test plan",
            "approve #3",
            "reject #3",
        ] {
            assert!(
                out.contains(expected),
                "formatted plan missing `{}`:\n{}",
                expected,
                out
            );
        }
    }

    #[test]
    fn format_plan_markdown_with_empty_preamble_omits_leading_blank() {
        let out = format_plan_markdown("", &row());
        assert!(out.starts_with("📋 *Fix plan #3*"));
    }
}
