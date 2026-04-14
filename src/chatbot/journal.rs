//! Structured conversation journal — queryable, compressible long-term memory.
//!
//! Records decisions, actions, observations, errors, and milestones.
//! Survives session resets. Searchable via full-text LIKE queries.

use serde::{Deserialize, Serialize};
use tracing::info;

/// A journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: i64,
    pub task_id: Option<String>,
    pub entry_type: String,
    pub summary: String,
    pub detail: String,
    pub participants: Vec<String>,
    pub tags: Vec<String>,
    pub created_at: String,
}

/// Add an entry to the journal.
pub fn add_entry(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    task_id: Option<&str>,
    entry_type: &str,
    summary: &str,
    detail: &str,
    participants: &[String],
    tags: &[String],
) -> anyhow::Result<i64> {
    let conn = conn.lock().unwrap();
    conn.execute(
        "INSERT INTO journal (task_id, entry_type, summary, detail, participants, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            task_id,
            entry_type,
            summary,
            &detail[..detail.len().min(500)], // cap at 500 chars
            serde_json::to_string(participants)?,
            serde_json::to_string(tags)?,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Search journal entries by keyword (LIKE on summary + detail + tags).
pub fn search_journal(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    query: &str,
    limit: u64,
) -> Vec<JournalEntry> {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let pattern = format!("%{}%", query);
    let mut stmt = match conn.prepare(
        "SELECT id, task_id, entry_type, summary, detail, participants, tags, created_at
         FROM journal
         WHERE summary LIKE ?1 OR detail LIKE ?1 OR tags LIKE ?1
         ORDER BY id DESC LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(rusqlite::params![pattern, limit], |row| {
        let participants_json: String = row.get(5)?;
        let tags_json: String = row.get(6)?;
        Ok(JournalEntry {
            id: row.get(0)?,
            task_id: row.get(1)?,
            entry_type: row.get(2)?,
            summary: row.get(3)?,
            detail: row.get(4)?,
            participants: serde_json::from_str(&participants_json).unwrap_or_default(),
            tags: serde_json::from_str(&tags_json).unwrap_or_default(),
            created_at: row.get(7)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Get journal entries for a specific task.
pub fn get_journal_for_task(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    task_id: &str,
) -> Vec<JournalEntry> {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, task_id, entry_type, summary, detail, participants, tags, created_at
         FROM journal WHERE task_id = ?1 ORDER BY id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(rusqlite::params![task_id], |row| {
        let p: String = row.get(5)?;
        let t: String = row.get(6)?;
        Ok(JournalEntry {
            id: row.get(0)?,
            task_id: row.get(1)?,
            entry_type: row.get(2)?,
            summary: row.get(3)?,
            detail: row.get(4)?,
            participants: serde_json::from_str(&p).unwrap_or_default(),
            tags: serde_json::from_str(&t).unwrap_or_default(),
            created_at: row.get(7)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Get recent entries within last N hours.
pub fn get_recent_entries(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    hours: u64,
    limit: u64,
) -> Vec<JournalEntry> {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let query = format!(
        "SELECT id, task_id, entry_type, summary, detail, participants, tags, created_at
         FROM journal WHERE created_at > datetime('now', '-{} hours')
         ORDER BY id DESC LIMIT ?1",
        hours
    );
    let mut stmt = match conn.prepare(&query) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(rusqlite::params![limit], |row| {
        let p: String = row.get(5)?;
        let t: String = row.get(6)?;
        Ok(JournalEntry {
            id: row.get(0)?,
            task_id: row.get(1)?,
            entry_type: row.get(2)?,
            summary: row.get(3)?,
            detail: row.get(4)?,
            participants: serde_json::from_str(&p).unwrap_or_default(),
            tags: serde_json::from_str(&t).unwrap_or_default(),
            created_at: row.get(7)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Compress old entries — replace detail with summary for entries older than N days.
pub fn compress_old_entries(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    older_than_days: u64,
) -> anyhow::Result<u64> {
    let conn = conn.lock().unwrap();
    let count = conn.execute(
        &format!(
            "UPDATE journal SET detail = summary
             WHERE created_at < datetime('now', '-{} days')
             AND detail != summary",
            older_than_days
        ),
        [],
    )?;
    if count > 0 {
        info!(
            "[journal] Compressed {} entries older than {} days",
            count, older_than_days
        );
    }
    Ok(count as u64)
}

/// Format entries as a compact timeline for task resume.
pub fn format_task_timeline(entries: &[JournalEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let ts = e.created_at.get(..16).unwrap_or(&e.created_at);
            format!("[{}] {}: {}", ts, e.entry_type, e.summary)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Generate an auto-journal summary for a tool call (lightweight, no LLM).
pub fn auto_journal_summary(tool_name: &str, key_params: &str) -> String {
    let params_truncated: String = key_params.chars().take(200).collect();
    format!("{}: {}", tool_name, params_truncated)
}

/// Set of tools that should be auto-journaled (state-changing tools).
pub fn is_journalable_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "send_message"
            | "ban_user"
            | "kick_user"
            | "mute_user"
            | "create_plan"
            | "update_plan_step"
            | "revise_plan"
            | "checkpoint_task"
            | "delegate_task"
            | "respond_to_handoff"
            | "request_consensus"
            | "vote_consensus"
            | "build_tool"
            | "run_script"
            | "docker_run"
            | "create_memory"
            | "edit_memory"
            | "delete_memory"
            | "set_reminder"
    )
}
