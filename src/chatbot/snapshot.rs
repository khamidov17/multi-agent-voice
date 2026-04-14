//! Automatic turn snapshots — LangGraph-inspired state capture.
//!
//! Every turn boundary is automatically snapshotted by the engine.
//! Captures what triggered the turn, what tools were used, what messages
//! were sent, and how the turn ended. The LLM doesn't need to do anything.
//!
//! Complements manual `checkpoint_task` — snapshots capture the *broader*
//! context while checkpoints capture *task-specific* structured state.

use serde::{Deserialize, Serialize};

/// A snapshot of a single processing turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSnapshot {
    pub snapshot_id: String,
    pub bot_name: String,
    pub turn_number: u64,
    pub timestamp: String,

    /// Messages that triggered this turn.
    pub trigger_messages: Vec<SnapshotMessage>,

    /// Tool names called during this turn.
    pub tool_calls_made: Vec<String>,
    pub tool_call_count: usize,

    /// Messages sent by the bot during this turn: (chat_id, text_preview).
    pub messages_sent: Vec<(i64, String)>,

    /// Active task context (if any).
    pub active_task_id: Option<String>,
    pub active_plan_id: Option<String>,
    pub plan_step_index: Option<usize>,

    /// How the turn ended.
    pub exit_action: String,
    pub exit_reason: Option<String>,
}

/// A message reference inside a snapshot (lightweight, no full content).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMessage {
    pub username: String,
    pub text_preview: String,
    pub chat_id: i64,
}

/// Save a turn snapshot to the bot's local database.
pub fn save_snapshot(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    snapshot: &TurnSnapshot,
) -> anyhow::Result<()> {
    let conn = conn.lock().unwrap();
    let json = serde_json::to_string(snapshot)?;
    conn.execute(
        "INSERT OR REPLACE INTO turn_snapshots (id, bot_name, turn_number, snapshot_json)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            snapshot.snapshot_id,
            snapshot.bot_name,
            snapshot.turn_number,
            json
        ],
    )?;
    Ok(())
}

/// Get the most recent snapshot for a bot.
pub fn get_last_snapshot(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    bot_name: &str,
) -> Option<TurnSnapshot> {
    let conn = conn.lock().ok()?;
    let json: String = conn
        .query_row(
            "SELECT snapshot_json FROM turn_snapshots
             WHERE bot_name = ?1 ORDER BY turn_number DESC LIMIT 1",
            rusqlite::params![bot_name],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str(&json).ok()
}

/// Get the last N snapshots for a bot (newest first).
pub fn get_snapshots_since(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    bot_name: &str,
    count: u64,
) -> Vec<TurnSnapshot> {
    let conn = match conn.lock() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT snapshot_json FROM turn_snapshots
         WHERE bot_name = ?1 ORDER BY turn_number DESC LIMIT ?2",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(rusqlite::params![bot_name, count], |row| {
        row.get::<_, String>(0)
    })
    .ok()
    .map(|rows| {
        rows.filter_map(|r| r.ok())
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    })
    .unwrap_or_default()
}

/// Delete old snapshots, keeping the most recent `keep_count`.
pub fn cleanup_old_snapshots(
    conn: &std::sync::Mutex<rusqlite::Connection>,
    bot_name: &str,
    keep_count: u64,
) -> anyhow::Result<u64> {
    let conn = conn.lock().unwrap();
    let deleted = conn.execute(
        "DELETE FROM turn_snapshots WHERE bot_name = ?1 AND id NOT IN (
             SELECT id FROM turn_snapshots WHERE bot_name = ?1
             ORDER BY turn_number DESC LIMIT ?2
         )",
        rusqlite::params![bot_name, keep_count],
    )?;
    Ok(deleted as u64)
}

/// Format a snapshot for inclusion in a resume message.
pub fn format_snapshot_for_resume(snap: &TurnSnapshot) -> String {
    let trigger_user = snap
        .trigger_messages
        .first()
        .map(|m| m.username.as_str())
        .unwrap_or("unknown");
    let tools = if snap.tool_calls_made.is_empty() {
        "none".to_string()
    } else {
        snap.tool_calls_made.join(", ")
    };
    format!(
        "Last turn snapshot (before restart):\n\
         - Trigger: {} message(s) from {}\n\
         - Tools used: {}\n\
         - Messages sent: {}\n\
         - Ended with: {} ({})\n\
         - Active task: {}",
        snap.trigger_messages.len(),
        trigger_user,
        tools,
        snap.messages_sent.len(),
        snap.exit_action,
        snap.exit_reason.as_deref().unwrap_or("no reason"),
        snap.active_task_id.as_deref().unwrap_or("none"),
    )
}

/// Format multiple snapshots into a readable summary (for GetSnapshots tool).
pub fn format_snapshots_summary(snapshots: &[TurnSnapshot]) -> String {
    if snapshots.is_empty() {
        return "No turn snapshots recorded yet.".to_string();
    }
    let mut lines = vec![format!("Last {} turn snapshot(s):", snapshots.len())];
    for snap in snapshots {
        let trigger_user = snap
            .trigger_messages
            .first()
            .map(|m| m.username.as_str())
            .unwrap_or("?");
        let ts = snap.timestamp.get(..19).unwrap_or(&snap.timestamp);
        let tools_summary = if snap.tool_calls_made.len() <= 3 {
            snap.tool_calls_made.join(", ")
        } else {
            format!(
                "{}, ... +{} more",
                snap.tool_calls_made[..3].join(", "),
                snap.tool_calls_made.len() - 3
            )
        };
        lines.push(format!(
            "\n  Turn #{} [{}] — triggered by {} ({} msg(s))\n\
             \x20   Tools: {} ({} total)\n\
             \x20   Sent: {} message(s) | Exit: {} ({})\n\
             \x20   Task: {}",
            snap.turn_number,
            ts,
            trigger_user,
            snap.trigger_messages.len(),
            if tools_summary.is_empty() {
                "none".to_string()
            } else {
                tools_summary
            },
            snap.tool_call_count,
            snap.messages_sent.len(),
            snap.exit_action,
            snap.exit_reason.as_deref().unwrap_or("-"),
            snap.active_task_id.as_deref().unwrap_or("none"),
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> std::sync::Mutex<rusqlite::Connection> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE turn_snapshots (
                id TEXT PRIMARY KEY,
                bot_name TEXT NOT NULL,
                turn_number INTEGER NOT NULL,
                snapshot_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .unwrap();
        std::sync::Mutex::new(conn)
    }

    fn make_snapshot(id: &str, bot: &str, turn: u64) -> TurnSnapshot {
        TurnSnapshot {
            snapshot_id: id.to_string(),
            bot_name: bot.to_string(),
            turn_number: turn,
            timestamp: "2025-01-01T10:00:00Z".to_string(),
            trigger_messages: vec![SnapshotMessage {
                username: "alice".into(),
                text_preview: "hello".into(),
                chat_id: -12345,
            }],
            tool_calls_made: vec!["send_message".into(), "query".into()],
            tool_call_count: 2,
            messages_sent: vec![(-12345, "hi there".into())],
            active_task_id: Some("task-1".into()),
            active_plan_id: None,
            plan_step_index: None,
            exit_action: "stop".into(),
            exit_reason: Some("done".into()),
        }
    }

    #[test]
    fn test_save_and_get_last_snapshot() {
        let conn = test_conn();
        let snap = make_snapshot("s1", "Nova", 1);
        save_snapshot(&conn, &snap).unwrap();

        let last = get_last_snapshot(&conn, "Nova");
        assert!(last.is_some());
        let last = last.unwrap();
        assert_eq!(last.snapshot_id, "s1");
        assert_eq!(last.turn_number, 1);
        assert_eq!(last.tool_calls_made.len(), 2);
    }

    #[test]
    fn test_get_last_returns_newest() {
        let conn = test_conn();
        save_snapshot(&conn, &make_snapshot("s1", "Nova", 1)).unwrap();
        save_snapshot(&conn, &make_snapshot("s2", "Nova", 2)).unwrap();
        save_snapshot(&conn, &make_snapshot("s3", "Nova", 3)).unwrap();

        let last = get_last_snapshot(&conn, "Nova").unwrap();
        assert_eq!(last.snapshot_id, "s3");
        assert_eq!(last.turn_number, 3);
    }

    #[test]
    fn test_get_snapshots_since() {
        let conn = test_conn();
        for i in 0..5 {
            save_snapshot(&conn, &make_snapshot(&format!("s{i}"), "Atlas", i)).unwrap();
        }

        let snaps = get_snapshots_since(&conn, "Atlas", 3);
        assert_eq!(snaps.len(), 3);
        // Should be newest first
        assert_eq!(snaps[0].turn_number, 4);
        assert_eq!(snaps[2].turn_number, 2);
    }

    #[test]
    fn test_cleanup_old_snapshots() {
        let conn = test_conn();
        for i in 0..10 {
            save_snapshot(&conn, &make_snapshot(&format!("s{i}"), "Nova", i)).unwrap();
        }

        let deleted = cleanup_old_snapshots(&conn, "Nova", 3).unwrap();
        assert_eq!(deleted, 7);

        let remaining = get_snapshots_since(&conn, "Nova", 100);
        assert_eq!(remaining.len(), 3);
    }

    #[test]
    fn test_bot_isolation() {
        let conn = test_conn();
        save_snapshot(&conn, &make_snapshot("n1", "Nova", 1)).unwrap();
        save_snapshot(&conn, &make_snapshot("a1", "Atlas", 1)).unwrap();

        assert!(get_last_snapshot(&conn, "Nova").is_some());
        assert!(get_last_snapshot(&conn, "Atlas").is_some());
        assert!(get_last_snapshot(&conn, "Sentinel").is_none());
    }

    #[test]
    fn test_format_snapshot_for_resume() {
        let snap = make_snapshot("s1", "Nova", 5);
        let formatted = format_snapshot_for_resume(&snap);
        assert!(formatted.contains("1 message(s) from alice"));
        assert!(formatted.contains("send_message, query"));
        assert!(formatted.contains("stop (done)"));
        assert!(formatted.contains("task-1"));
    }
}
