//! Agent self-evaluation — periodic aggregate performance assessment.
//!
//! Runs during cognitive IMPROVE mode (max once every 6 hours).
//! Gathers metrics, reflections, and task outcomes to generate a self-assessment.

use serde::{Deserialize, Serialize};
use tracing::info;

/// A self-evaluation record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub period_start: String,
    pub period_end: String,
    pub messages_handled: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    pub avg_response_quality: f32,
    pub top_failure_modes: Vec<String>,
    pub improvement_actions: Vec<String>,
    pub score: f32,
}

/// Raw data gathered for evaluation.
pub struct EvalData {
    pub messages_handled: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    pub tool_failure_rate: f32,
    pub recent_reflections_summary: String,
    pub last_score: Option<f32>,
    pub score_trend: Vec<f32>,
    pub period_start: String,
    pub period_end: String,
}

/// Save an evaluation to the bot's database.
pub fn save_evaluation(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    eval: &Evaluation,
) -> anyhow::Result<()> {
    let conn = conn.lock().unwrap();
    conn.execute(
        "INSERT INTO self_evaluations
         (period_start, period_end, messages_handled, tasks_completed, tasks_failed,
          avg_quality, top_failure_modes, improvement_actions, score)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            eval.period_start,
            eval.period_end,
            eval.messages_handled,
            eval.tasks_completed,
            eval.tasks_failed,
            eval.avg_response_quality,
            serde_json::to_string(&eval.top_failure_modes)?,
            serde_json::to_string(&eval.improvement_actions)?,
            eval.score,
        ],
    )?;
    info!("Self-evaluation saved: score={}", eval.score);
    Ok(())
}

/// Get the last evaluation (for trend tracking).
pub fn get_last_evaluation(conn: &std::sync::Mutex<rusqlite::Connection>) -> Option<Evaluation> {
    let conn = conn.lock().ok()?;
    conn.query_row(
        "SELECT period_start, period_end, messages_handled, tasks_completed, tasks_failed,
                avg_quality, top_failure_modes, improvement_actions, score
         FROM self_evaluations ORDER BY id DESC LIMIT 1",
        [],
        |row| {
            let fm_json: String = row.get(6)?;
            let ia_json: String = row.get(7)?;
            Ok(Evaluation {
                period_start: row.get(0)?,
                period_end: row.get(1)?,
                messages_handled: row.get(2)?,
                tasks_completed: row.get(3)?,
                tasks_failed: row.get(4)?,
                avg_response_quality: row.get(5)?,
                top_failure_modes: serde_json::from_str(&fm_json).unwrap_or_default(),
                improvement_actions: serde_json::from_str(&ia_json).unwrap_or_default(),
                score: row.get(8)?,
            })
        },
    )
    .ok()
}

/// Get the last N evaluation scores for trend tracking.
pub fn get_score_trend(conn: &std::sync::Mutex<rusqlite::Connection>, count: usize) -> Vec<f32> {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt =
        match conn.prepare("SELECT score FROM self_evaluations ORDER BY id DESC LIMIT ?1") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let scores: Vec<f32> = stmt
        .query_map(rusqlite::params![count], |row| row.get(0))
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    // Reverse so oldest is first
    scores.into_iter().rev().collect()
}

/// Check if enough time has passed since last evaluation (6 hour minimum).
pub fn should_evaluate(conn: &std::sync::Mutex<rusqlite::Connection>) -> bool {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last_ts: Option<String> = conn
        .query_row(
            "SELECT created_at FROM self_evaluations ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    match last_ts {
        None => true, // never evaluated
        Some(ts) => {
            if let Ok(last) = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S") {
                let now = chrono::Utc::now().naive_utc();
                now.signed_duration_since(last).num_hours() >= 6
            } else {
                true
            }
        }
    }
}

/// Compute raw evaluation data from the database.
pub fn compute_eval_data(conn: &std::sync::Mutex<rusqlite::Connection>) -> EvalData {
    let locked = conn.lock().unwrap();

    // Messages in last 24h
    let messages: u64 = locked
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE timestamp > datetime('now', '-24 hours')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // Metrics totals from snapshots in last 24h
    let (tool_total, tool_failed): (u64, u64) = locked
        .query_row(
            "SELECT COALESCE(SUM(tool_calls_total),0), COALESCE(SUM(tool_calls_failed),0)
             FROM metrics_snapshots WHERE timestamp > datetime('now', '-24 hours')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    let tool_failure_rate = if tool_total > 0 {
        (tool_failed as f32 / tool_total as f32) * 100.0
    } else {
        0.0
    };

    // Reflections summary
    drop(locked);
    let reflections = crate::chatbot::reflection::get_recent_reflections(conn, 5);
    let reflections_summary = crate::chatbot::reflection::format_reflection_summary(&reflections);

    // Score trend
    let trend = get_score_trend(conn, 3);
    let last_score = trend.last().copied();

    let now = chrono::Utc::now();
    let period_start = (now - chrono::Duration::hours(24)).to_rfc3339();
    let period_end = now.to_rfc3339();

    EvalData {
        messages_handled: messages,
        tasks_completed: 0, // would need shared DB access for tasks
        tasks_failed: 0,
        tool_failure_rate,
        recent_reflections_summary: reflections_summary,
        last_score,
        score_trend: trend,
        period_start,
        period_end,
    }
}

/// Format evaluation data into a cognitive IMPROVE prompt.
pub fn format_eval_prompt(data: &EvalData) -> String {
    let trend_str = if data.score_trend.is_empty() {
        "No previous evaluations.".to_string()
    } else {
        let scores: Vec<String> = data
            .score_trend
            .iter()
            .map(|s| format!("{:.1}", s))
            .collect();
        format!("Last scores: {}", scores.join(" → "))
    };

    format!(
        "[COGNITIVE:IMPROVE] Time for self-evaluation. Stats for the last 24 hours:\n\
         Messages handled: {}\n\
         Tool failure rate: {:.1}%\n\
         {}\n\
         Recent reflections:\n{}\n\n\
         Score yourself 1-10 and identify:\n\
         - Your top 3 failure modes\n\
         - 3 concrete improvement actions\n\
         Save using self_evaluate tool. Also update memories/reflections/self_eval.md.",
        data.messages_handled,
        data.tool_failure_rate,
        trend_str,
        if data.recent_reflections_summary.is_empty() {
            "No reflections yet.".to_string()
        } else {
            data.recent_reflections_summary.clone()
        },
    )
}
