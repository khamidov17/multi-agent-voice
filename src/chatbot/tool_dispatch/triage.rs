//! Dispatch for the Phase 1 triage tools: `read_alerts`, `mark_triaged`,
//! `send_triage_report`.
//!
//! These tools are Nova-only by design (A and S detect, N triages). The
//! dispatch enforces `full_permissions=true` at entry; Atlas/Sentinel
//! attempting to call them gets a structured refusal rather than a
//! successful hit on the shared alerts DB.
//!
//! All three calls operate against `data/shared/bug_alerts.db` via an
//! ad-hoc read connection — we don't route reads through the
//! `AlertsWriter` task (it's write-side only). A future Phase 3+ might
//! centralize this; for now the per-call `Connection::open` is cheap
//! (the SQLite file is tiny and WAL-mode).
//!
//! # Output format
//!
//! `read_alerts` returns a JSON array so Nova can structurally reason
//! over it in-turn. `mark_triaged` returns `{"closed": N}`.
//! `send_triage_report` returns `{"sent_message_id": N, "closed": N}`.

use crate::chatbot::alerts::{self, BugAlertRow, Severity};
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::telegram::TelegramClient;
use serde_json::json;
use std::path::PathBuf;

/// Cap on `limit` so a buggy prompt can't ask for 1M rows. Matches the
/// tool description's documented cap.
const READ_ALERTS_MAX_LIMIT: i64 = 200;
const READ_ALERTS_DEFAULT_LIMIT: i64 = 50;

/// Derive the shared alerts DB path from the bot's data_dir. If the
/// `alerts_writer` is wired up, its path matches this — but the writer
/// doesn't expose its path and Nova needs a read connection, so we
/// rederive here.
fn alerts_db_path(config: &ChatbotConfig) -> Result<PathBuf, String> {
    let data_dir = config
        .data_dir
        .as_ref()
        .ok_or_else(|| "triage: data_dir not set in ChatbotConfig".to_string())?;
    Ok(alerts::shared_alerts_db_path(data_dir))
}

/// Return `Err` if the caller isn't Tier 1. Phase 1 triage is Nova-only.
fn require_tier1(config: &ChatbotConfig, tool: &str) -> Result<(), String> {
    if !config.full_permissions {
        return Err(format!(
            "{} is Nova-only (requires full_permissions=true). This bot tier must \
             not triage alerts; Atlas/Sentinel detect, Nova reports.",
            tool
        ));
    }
    Ok(())
}

pub fn execute_read_alerts(
    config: &ChatbotConfig,
    since: Option<&str>,
    category: Option<&str>,
    limit: Option<i64>,
) -> Result<Option<String>, String> {
    require_tier1(config, "read_alerts")?;
    let db_path = alerts_db_path(config)?;
    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("read_alerts: open {}: {}", db_path.display(), e))?;
    // Schema is idempotently ensured by AlertsWriter at spawn, but a
    // Nova-only read against a bot that never spawned AlertsWriter (e.g.
    // new deploy with Phase 1 disabled) would hit "no such table".
    // init_schema is cheap and idempotent.
    alerts::init_schema(&conn).map_err(|e| format!("read_alerts: init_schema: {}", e))?;

    let effective_limit = limit
        .map(|n| n.clamp(1, READ_ALERTS_MAX_LIMIT))
        .unwrap_or(READ_ALERTS_DEFAULT_LIMIT);

    let rows = alerts::query_open_alerts(&conn, since, category, Some(effective_limit))
        .map_err(|e| format!("read_alerts: query: {}", e))?;

    let json_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "fingerprint": r.fingerprint,
                "detected_by": r.detected_by,
                "severity": r.severity.as_str(),
                "category": r.category,
                "summary": r.summary,
                "evidence": r.evidence,
                "first_seen_at": r.first_seen_at,
                "last_seen_at": r.last_seen_at,
                "count": r.count,
            })
        })
        .collect();

    let body = json!({
        "total_open": json_rows.len(),
        "alerts": json_rows,
    });
    Ok(Some(serde_json::to_string(&body).unwrap_or_else(|_| {
        r#"{"error":"serialization"}"#.to_string()
    })))
}

pub fn execute_mark_triaged(
    config: &ChatbotConfig,
    alert_ids: &[i64],
    note: Option<&str>,
) -> Result<Option<String>, String> {
    require_tier1(config, "mark_triaged")?;
    if alert_ids.is_empty() {
        return Err("mark_triaged: alert_ids is empty".to_string());
    }
    let db_path = alerts_db_path(config)?;
    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("mark_triaged: open {}: {}", db_path.display(), e))?;
    alerts::init_schema(&conn).map_err(|e| format!("mark_triaged: init_schema: {}", e))?;
    let closed = alerts::mark_triaged(&conn, alert_ids, note)
        .map_err(|e| format!("mark_triaged: update: {}", e))?;
    Ok(Some(
        json!({ "closed": closed, "requested": alert_ids.len() }).to_string(),
    ))
}

pub async fn execute_send_triage_report(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    preamble: &str,
    auto_mark_triaged: bool,
) -> Result<Option<String>, String> {
    require_tier1(config, "send_triage_report")?;
    // Refuse if target chat isn't the owner — triage reports are
    // confidential. Nova's prompt shouldn't do this by accident, but the
    // dispatch gate prevents accidental leakage to a group.
    if let Some(owner_id) = config.owner_user_id
        && chat_id != owner_id
    {
        return Err(format!(
            "send_triage_report refused: chat_id={} is not the owner ({}). \
             Triage reports go to the owner's DM only.",
            chat_id, owner_id
        ));
    }

    let db_path = alerts_db_path(config)?;
    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("send_triage_report: open {}: {}", db_path.display(), e))?;
    alerts::init_schema(&conn).map_err(|e| format!("send_triage_report: init_schema: {}", e))?;

    let rows = alerts::query_open_alerts(&conn, None, None, Some(READ_ALERTS_MAX_LIMIT))
        .map_err(|e| format!("send_triage_report: query: {}", e))?;

    let text = format_triage_markdown(preamble, &rows);
    let sent = telegram
        .send_message(chat_id, &text, None)
        .await
        .map_err(|e| format!("send_triage_report: telegram send: {}", e))?;

    let closed = if auto_mark_triaged && !rows.is_empty() {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        alerts::mark_triaged(
            &conn,
            &ids,
            Some("closed by send_triage_report auto_mark_triaged=true"),
        )
        .map_err(|e| format!("send_triage_report: auto-triage failed: {}", e))?
    } else {
        0
    };

    Ok(Some(
        json!({
            "sent_message_id": sent,
            "reported_alerts": rows.len(),
            "closed": closed,
        })
        .to_string(),
    ))
}

/// Format a triage report as Telegram markdown. Kept harness-side so
/// Nova doesn't need to remember our output style and can't drift it
/// accidentally.
fn format_triage_markdown(preamble: &str, rows: &[BugAlertRow]) -> String {
    let mut out = String::new();
    let preamble_trimmed = preamble.trim();
    if !preamble_trimmed.is_empty() {
        out.push_str(preamble_trimmed);
        out.push_str("\n\n");
    }

    if rows.is_empty() {
        out.push_str("✅ *No open alerts.* All clear.");
        return out;
    }

    out.push_str(&format!("🚨 *Open alerts: {}*\n", rows.len()));

    // Group by severity so the owner can skip low-priority at a glance.
    let mut by_sev: std::collections::BTreeMap<u8, Vec<&BugAlertRow>> =
        std::collections::BTreeMap::new();
    for r in rows {
        let rank = match r.severity {
            Severity::Critical => 0,
            Severity::High => 1,
            Severity::Medium => 2,
            Severity::Low => 3,
        };
        by_sev.entry(rank).or_default().push(r);
    }
    for (rank, group) in by_sev {
        let label = match rank {
            0 => "🔴 critical",
            1 => "🟠 high",
            2 => "🟡 medium",
            _ => "⚪ low",
        };
        out.push_str(&format!("\n*{}* ({})\n", label, group.len()));
        for r in group {
            let count_suffix = if r.count > 1 {
                format!(" ×{}", r.count)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "• `#{}` {} — {} ({}{})\n",
                r.id,
                r.category,
                truncate_line(&r.summary, 120),
                r.last_seen_at,
                count_suffix,
            ));
        }
    }

    out.push_str("\n_Reply to close specific alerts or call mark_triaged with the IDs above._");
    out
}

fn truncate_line(s: &str, max_chars: usize) -> String {
    // Single-line (no newlines) and bounded.
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json as json_macro;

    fn row(id: i64, severity: Severity, category: &str, summary: &str, count: i64) -> BugAlertRow {
        BugAlertRow {
            id,
            fingerprint: "fp".into(),
            detected_by: "atlas".into(),
            severity,
            category: category.into(),
            summary: summary.into(),
            evidence: json_macro!({}),
            first_seen_at: "2026-04-21T14:00:00".into(),
            last_seen_at: "2026-04-21T14:05:00".into(),
            count,
            triaged_at: None,
            triage_note: None,
        }
    }

    #[test]
    fn format_empty_shows_all_clear() {
        let out = format_triage_markdown("", &[]);
        assert!(out.contains("No open alerts"));
    }

    #[test]
    fn format_includes_preamble_and_sorted_groups() {
        let rows = vec![
            row(1, Severity::Low, "x", "low stuff", 1),
            row(2, Severity::Critical, "subprocess.crash", "nova died", 3),
            row(3, Severity::High, "heartbeat.gap", "gap ≥60s", 1),
        ];
        let out = format_triage_markdown("owner, here is the status:", &rows);
        assert!(out.starts_with("owner, here is the status:"));
        // Critical section appears before low section.
        let crit_idx = out.find("critical").expect("critical label");
        let low_idx = out.find("low").expect("low label");
        assert!(crit_idx < low_idx);
        // Count suffix appears on the deduped alert.
        assert!(out.contains("×3"));
    }

    #[test]
    fn truncate_line_bounded_and_no_newlines() {
        let s = "a b c\n d e\r f";
        let out = truncate_line(s, 100);
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
    }
}
