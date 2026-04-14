//! Tool dispatch — reflection tools.

use tokio::sync::Mutex;

use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;

/// Log a post-task reflection (Reflect tool).
pub(super) async fn execute_reflect(
    config: &ChatbotConfig,
    database: &Mutex<Database>,
    task_id: Option<&str>,
    outcome: &str,
    what_worked_json: &str,
    what_failed_json: &str,
    lessons_json: &str,
) -> Result<Option<String>, String> {
    let what_worked: Vec<String> = serde_json::from_str(what_worked_json).unwrap_or_default();
    let what_failed: Vec<String> = serde_json::from_str(what_failed_json).unwrap_or_default();
    let lessons: Vec<String> = serde_json::from_str(lessons_json).unwrap_or_default();

    if lessons.is_empty() {
        return Err("Reflection must include at least one lesson. Be specific.".to_string());
    }

    let reflection = crate::chatbot::reflection::Reflection {
        task_id: task_id.map(|s| s.to_string()),
        outcome: outcome.to_string(),
        what_worked,
        what_failed,
        lessons: lessons.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    // Save to database
    let db = database.lock().await;
    crate::chatbot::reflection::save_reflection(db.connection(), &reflection)
        .map_err(|e| format!("Failed to save reflection: {e}"))?;
    drop(db);

    // Write to journal file
    if let Some(ref data_dir) = config.data_dir {
        let _ = crate::chatbot::reflection::write_reflection_journal(data_dir, &reflection);
    }

    Ok(Some(format!(
        "Reflection logged: {} — {} lesson(s). These will be auto-injected into your prompt.",
        outcome,
        lessons.len(),
    )))
}

/// Record periodic self-evaluation (SelfEvaluate tool).
pub(super) async fn execute_self_evaluate(
    config: &ChatbotConfig,
    database: &Mutex<Database>,
    score: f32,
    top_failure_modes_json: &str,
    improvement_actions_json: &str,
    notes: Option<&str>,
) -> Result<Option<String>, String> {
    let score = score.clamp(1.0, 10.0);
    let failure_modes: Vec<String> =
        serde_json::from_str(top_failure_modes_json).unwrap_or_default();
    let actions: Vec<String> = serde_json::from_str(improvement_actions_json).unwrap_or_default();

    let now = chrono::Utc::now();
    let eval = crate::chatbot::self_eval::Evaluation {
        period_start: (now - chrono::Duration::hours(24)).to_rfc3339(),
        period_end: now.to_rfc3339(),
        messages_handled: 0, // filled by compute_eval_data, but we save what the bot reports
        tasks_completed: 0,
        tasks_failed: 0,
        avg_response_quality: score,
        top_failure_modes: failure_modes.clone(),
        improvement_actions: actions.clone(),
        score,
    };

    let db = database.lock().await;
    crate::chatbot::self_eval::save_evaluation(db.connection(), &eval)
        .map_err(|e| format!("Failed to save evaluation: {e}"))?;
    drop(db);

    // Write to journal
    if let Some(ref data_dir) = config.data_dir {
        let eval_dir = data_dir.join("memories").join("reflections");
        let _ = std::fs::create_dir_all(&eval_dir);
        let eval_path = eval_dir.join("self_eval.md");
        let entry = format!(
            "\n## Self-Eval {}\nScore: {:.1}/10\nFailure modes: {}\nActions: {}\n{}\n",
            now.format("%Y-%m-%d %H:%M"),
            score,
            failure_modes.join(", "),
            actions.join(", "),
            notes.unwrap_or(""),
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&eval_path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(entry.as_bytes())
            });
    }

    // Get trend for response
    let db = database.lock().await;
    let trend = crate::chatbot::self_eval::get_score_trend(db.connection(), 5);
    drop(db);

    let trend_str: Vec<String> = trend.iter().map(|s| format!("{:.1}", s)).collect();

    Ok(Some(format!(
        "Self-evaluation recorded: {:.1}/10. Trend: {}. {} failure modes, {} improvement actions.",
        score,
        trend_str.join(" → "),
        failure_modes.len(),
        actions.len(),
    )))
}

/// Log a journal entry (JournalLog tool).
pub(super) async fn execute_journal_log(
    database: &Mutex<Database>,
    entry_type: &str,
    summary: &str,
    detail: Option<&str>,
    task_id: Option<&str>,
    tags_json: Option<&str>,
) -> Result<Option<String>, String> {
    let tags: Vec<String> = tags_json
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let db = database.lock().await;
    let id = crate::chatbot::journal::add_entry(
        db.connection(),
        task_id,
        entry_type,
        summary,
        detail.unwrap_or(summary),
        &[],
        &tags,
    )
    .map_err(|e| format!("Journal log failed: {e}"))?;

    Ok(Some(format!(
        "Journal entry #{} logged: [{}] {}",
        id, entry_type, summary
    )))
}

/// Search journal entries (JournalSearch tool).
pub(super) async fn execute_journal_search(
    database: &Mutex<Database>,
    query: &str,
    task_id: Option<&str>,
    limit: u64,
) -> Result<Option<String>, String> {
    let db = database.lock().await;
    let entries = if let Some(tid) = task_id {
        crate::chatbot::journal::get_journal_for_task(db.connection(), tid)
    } else {
        crate::chatbot::journal::search_journal(db.connection(), query, limit)
    };

    if entries.is_empty() {
        return Ok(Some(format!("No journal entries matching '{}'.", query)));
    }

    let mut lines = vec![format!("Found {} entries:", entries.len())];
    for e in &entries {
        let ts = e.created_at.get(..16).unwrap_or(&e.created_at);
        lines.push(format!("[{}] {}: {}", ts, e.entry_type, e.summary));
    }
    Ok(Some(lines.join("\n")))
}

/// Get journal summary timeline (JournalSummary tool).
pub(super) async fn execute_journal_summary(
    database: &Mutex<Database>,
    task_id: Option<&str>,
    last_hours: u64,
) -> Result<Option<String>, String> {
    let db = database.lock().await;
    let entries = if let Some(tid) = task_id {
        crate::chatbot::journal::get_journal_for_task(db.connection(), tid)
    } else {
        crate::chatbot::journal::get_recent_entries(db.connection(), last_hours, 50)
    };

    if entries.is_empty() {
        return Ok(Some("No journal entries in this period.".to_string()));
    }

    Ok(Some(crate::chatbot::journal::format_task_timeline(
        &entries,
    )))
}
