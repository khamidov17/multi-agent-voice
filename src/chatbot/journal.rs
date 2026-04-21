//! Structured conversation journal — queryable, compressible long-term memory.
//!
//! Records decisions, actions, observations, errors, and milestones.
//! Survives session resets. Searchable via full-text LIKE queries.
//!
//! # Phase 0 entry types
//!
//! The following entry_type constants are used by Phase 0 observability
//! instrumentation. Keep them stable — queries in post-mortem tooling
//! will grep for these exact strings.
//!
//! - [`ENTRY_TOOL_CALL`] — a single MCP tool invocation. One per tool,
//!   emitted at the END of dispatch with success/error.
//! - [`ENTRY_TG_SEND`] — a Telegram `sendMessage` call resolved. Subset
//!   of tool_call for send_message specifically, with HTTP status.
//! - [`ENTRY_GUARDIAN_ALLOW`], [`ENTRY_GUARDIAN_DENY`],
//!   [`ENTRY_GUARDIAN_ERROR`] — outcomes of `protected_write` calls.
//!
//! These events emit through the same `add_entry` path as everything
//! else; they share the shared `Mutex<Connection>` with the engine's
//! own journal writes. A dedicated writer task that decouples these
//! from the engine hot-path is tracked in TODOS.md (HC2 from the
//! autoplan Eng review).

use serde::{Deserialize, Serialize};
use tracing::info;

/// Phase 0 entry_type — one completed MCP tool invocation.
pub const ENTRY_TOOL_CALL: &str = "tool_call";
/// Phase 0 entry_type — one outbound Telegram send resolved (success/error).
pub const ENTRY_TG_SEND: &str = "tg.send";
/// Phase 0 entry_type — guardian accepted a protected_write.
pub const ENTRY_GUARDIAN_ALLOW: &str = "guardian.allow";
/// Phase 0 entry_type — guardian denied a protected_write (protected path or outside allowed root).
pub const ENTRY_GUARDIAN_DENY: &str = "guardian.deny";
/// Phase 0 entry_type — guardian returned a non-denial error (RPC failure, IO failure, etc.).
pub const ENTRY_GUARDIAN_ERROR: &str = "guardian.error";

/// Best-effort convenience wrapper around [`add_entry`] that never bubbles
/// an error up the caller's hot path — Phase 0 observability must not kill
/// the engine. Failures are logged via `tracing::warn!` and swallowed.
///
/// Use this for observability events that are additive and tolerable to
/// lose on disk pressure / mutex poison. For critical audit writes
/// (billing, security), keep calling `add_entry` and handle the `Result`.
pub fn emit(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    task_id: Option<&str>,
    entry_type: &str,
    summary: &str,
    detail: &str,
    participants: &[String],
    tags: &[String],
) {
    if let Err(e) = add_entry(
        conn,
        task_id,
        entry_type,
        summary,
        detail,
        participants,
        tags,
    ) {
        tracing::warn!(
            entry_type,
            summary,
            err = %e,
            "journal emit failed (non-fatal); observability will miss this event"
        );
    }
}

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
    // UTF-8 safe: iterate chars (not bytes) so a multi-byte codepoint at
    // position 500 doesn't panic. Previously used `&detail[..detail.len().min(500)]`
    // which slices at an arbitrary byte offset — /review performance specialist
    // flagged this as a hot-path panic waiting to happen on Cyrillic/emoji input.
    let detail_capped: String = detail.chars().take(500).collect();
    let conn = conn
        .lock()
        .map_err(|e| anyhow::anyhow!("journal mutex poisoned: {}", e))?;
    conn.execute(
        "INSERT INTO journal (task_id, entry_type, summary, detail, participants, tags)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            task_id,
            entry_type,
            summary,
            &detail_capped,
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

/// Create the journal table schema (used by tests and Database).
pub fn create_journal_table(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS journal (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id     TEXT,
            entry_type  TEXT NOT NULL,
            summary     TEXT NOT NULL,
            detail      TEXT NOT NULL DEFAULT '',
            participants TEXT NOT NULL DEFAULT '[]',
            tags        TEXT NOT NULL DEFAULT '[]',
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> std::sync::Mutex<rusqlite::Connection> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_journal_table(&conn).unwrap();
        std::sync::Mutex::new(conn)
    }

    #[test]
    fn test_add_and_search_entry() {
        let conn = test_conn();
        let id = add_entry(
            &conn,
            Some("task-1"),
            "action",
            "Deployed v2.0",
            "Full deployment of version 2.0 to production",
            &["Nova".to_string()],
            &["deploy".to_string(), "prod".to_string()],
        )
        .unwrap();
        assert!(id > 0);

        let results = search_journal(&conn, "deploy", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].summary, "Deployed v2.0");
        assert_eq!(results[0].task_id, Some("task-1".to_string()));
    }

    #[test]
    fn test_get_journal_for_task() {
        let conn = test_conn();
        add_entry(&conn, Some("t1"), "action", "Step 1", "d1", &[], &[]).unwrap();
        add_entry(&conn, Some("t1"), "action", "Step 2", "d2", &[], &[]).unwrap();
        add_entry(&conn, Some("t2"), "action", "Other", "d3", &[], &[]).unwrap();

        let entries = get_journal_for_task(&conn, "t1");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].summary, "Step 1");
        assert_eq!(entries[1].summary, "Step 2");
    }

    #[test]
    fn test_is_journalable_tool() {
        assert!(is_journalable_tool("send_message"));
        assert!(is_journalable_tool("ban_user"));
        assert!(is_journalable_tool("create_plan"));
        assert!(is_journalable_tool("build_tool"));
        // Non-state-changing tools should not be journaled
        assert!(!is_journalable_tool("query"));
        assert!(!is_journalable_tool("get_metrics"));
        assert!(!is_journalable_tool("read_memory"));
        assert!(!is_journalable_tool("list_tools"));
    }

    #[test]
    fn test_compress_old_entries() {
        let conn = test_conn();
        // Insert an entry with a past date
        {
            let c = conn.lock().unwrap();
            c.execute(
                "INSERT INTO journal (task_id, entry_type, summary, detail, created_at)
                 VALUES ('t1', 'action', 'old summary', 'long old detail text', datetime('now', '-10 days'))",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO journal (task_id, entry_type, summary, detail)
                 VALUES ('t1', 'action', 'new summary', 'new detail text')",
                [],
            )
            .unwrap();
        }

        // Compress entries older than 7 days
        let count = compress_old_entries(&conn, 7).unwrap();
        assert_eq!(count, 1);

        // Verify old entry's detail = summary
        let c = conn.lock().unwrap();
        let (summary, detail): (String, String) = c
            .query_row(
                "SELECT summary, detail FROM journal ORDER BY id ASC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary, "old summary");
        assert_eq!(detail, "old summary"); // compressed
    }

    #[test]
    fn test_format_task_timeline() {
        let entries = vec![
            JournalEntry {
                id: 1,
                task_id: Some("t1".into()),
                entry_type: "action".into(),
                summary: "Did thing".into(),
                detail: String::new(),
                participants: vec![],
                tags: vec![],
                created_at: "2025-01-01 10:00:00".into(),
            },
            JournalEntry {
                id: 2,
                task_id: Some("t1".into()),
                entry_type: "observation".into(),
                summary: "Saw result".into(),
                detail: String::new(),
                participants: vec![],
                tags: vec![],
                created_at: "2025-01-01 10:05:00".into(),
            },
        ];
        let timeline = format_task_timeline(&entries);
        assert!(timeline.contains("action: Did thing"));
        assert!(timeline.contains("observation: Saw result"));
    }

    // ---- Phase 0 emit + truncation tests ----
    //
    // These live in the same test module (rather than a second one) to
    // satisfy clippy::items_after_test_module. `test_conn()` above covers
    // schema setup.

    #[test]
    fn emit_swallows_err_on_missing_schema() {
        // No `journal` table → add_entry fails at INSERT → emit MUST swallow.
        let conn = std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap());
        emit(
            &conn,
            None,
            ENTRY_TOOL_CALL,
            "summary",
            "detail",
            &[],
            &["test".to_string()],
        );
    }

    #[test]
    fn emit_writes_row_on_happy_path() {
        let conn = test_conn();
        emit(
            &conn,
            None,
            ENTRY_GUARDIAN_ALLOW,
            "guardian.allow: wrote 42 bytes",
            "path=/opt/foo reason=test",
            &[],
            &["nova".to_string(), "/opt/foo".to_string()],
        );
        let rows: i64 = conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn add_entry_utf8_safe_truncation_no_panic() {
        let conn = test_conn();
        // 600 emoji chars — would have panicked under the old
        // `&detail[..byte_500]` slicing logic at a mid-codepoint byte.
        let big: String = "🚀".repeat(600);
        let result = add_entry(&conn, None, ENTRY_TOOL_CALL, "summary", &big, &[], &[]);
        assert!(result.is_ok(), "utf-8 truncation must not panic or error");
    }

    #[test]
    fn add_entry_maps_poisoned_mutex_to_err_not_panic() {
        let conn = test_conn();
        // Poison the mutex by panicking inside a lock.
        let _ = std::panic::catch_unwind(|| {
            let _g = conn.lock().unwrap();
            panic!("intentional poison");
        });
        let result = add_entry(&conn, None, ENTRY_TOOL_CALL, "summary", "detail", &[], &[]);
        assert!(result.is_err(), "poisoned mutex must produce Err, not panic");
        // emit also must not panic on a poisoned mutex.
        emit(&conn, None, ENTRY_TOOL_CALL, "s", "d", &[], &[]);
    }
}
