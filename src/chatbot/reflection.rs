//! Automated post-task reflection — extract lessons and update agent behavior.
//!
//! After completing tasks, agents reflect on what worked and what didn't.
//! Lessons are persisted in SQLite and auto-injected into the system prompt.

use serde::{Deserialize, Serialize};
use tracing::info;

/// A post-task reflection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reflection {
    pub task_id: Option<String>,
    pub outcome: String,
    pub what_worked: Vec<String>,
    pub what_failed: Vec<String>,
    pub lessons: Vec<String>,
    pub timestamp: String,
}

/// Save a reflection to the bot's own database.
pub fn save_reflection(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    reflection: &Reflection,
) -> anyhow::Result<()> {
    let conn = conn.lock().unwrap();
    conn.execute(
        "INSERT INTO reflections (task_id, outcome, what_worked, what_failed, lessons)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            reflection.task_id,
            reflection.outcome,
            serde_json::to_string(&reflection.what_worked)?,
            serde_json::to_string(&reflection.what_failed)?,
            serde_json::to_string(&reflection.lessons)?,
        ],
    )?;
    info!(
        "Reflection saved: outcome={}, {} lessons",
        reflection.outcome,
        reflection.lessons.len()
    );
    Ok(())
}

/// Get recent reflections from the database.
pub fn get_recent_reflections(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    limit: usize,
) -> Vec<Reflection> {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT task_id, outcome, what_worked, what_failed, lessons, created_at
         FROM reflections ORDER BY id DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(rusqlite::params![limit], |row| {
        let worked_json: String = row.get(2)?;
        let failed_json: String = row.get(3)?;
        let lessons_json: String = row.get(4)?;
        Ok(Reflection {
            task_id: row.get(0)?,
            outcome: row.get(1)?,
            what_worked: serde_json::from_str(&worked_json).unwrap_or_default(),
            what_failed: serde_json::from_str(&failed_json).unwrap_or_default(),
            lessons: serde_json::from_str(&lessons_json).unwrap_or_default(),
            timestamp: row.get(5)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Format reflections into a condensed summary for system prompt injection.
/// Max 500 chars per reflection to keep prompt lean.
pub fn format_reflection_summary(reflections: &[Reflection]) -> String {
    if reflections.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    for r in reflections {
        let lessons_str = r
            .lessons
            .iter()
            .map(|l| format!("  - {}", l))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = format!(
            "[{}] {} — Lessons:\n{}",
            r.outcome.to_uppercase(),
            r.task_id.as_deref().unwrap_or("general"),
            lessons_str,
        );
        // Cap at 500 chars per reflection
        if entry.len() > 500 {
            lines.push(format!("{}...", &entry[..497]));
        } else {
            lines.push(entry);
        }
    }
    lines.join("\n\n")
}

/// Write reflection to the memories/reflections/ journal file.
pub fn write_reflection_journal(
    data_dir: &std::path::Path,
    reflection: &Reflection,
) -> anyhow::Result<()> {
    let reflections_dir = data_dir.join("memories").join("reflections");
    std::fs::create_dir_all(&reflections_dir)?;

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let journal_path = reflections_dir.join(format!("{date}.md"));

    let entry = format!(
        "\n## {} — {}\n- **Outcome:** {}\n- **What worked:** {}\n- **What failed:** {}\n- **Lessons:** {}\n",
        chrono::Utc::now().format("%H:%M"),
        reflection.task_id.as_deref().unwrap_or("general"),
        reflection.outcome,
        reflection.what_worked.join("; "),
        reflection.what_failed.join("; "),
        reflection.lessons.join("; "),
    );

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&journal_path)?;
    file.write_all(entry.as_bytes())?;

    info!("Reflection journal updated: {}", journal_path.display());
    Ok(())
}
