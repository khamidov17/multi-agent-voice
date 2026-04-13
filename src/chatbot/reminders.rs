//! Reminder system: SQLite-backed scheduled messages with a background firing loop.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};
use tracing::{error, info, warn};

use crate::chatbot::telegram::TelegramClient;

/// A single reminder record.
#[derive(Debug, Clone)]
pub struct Reminder {
    pub id: i64,
    pub chat_id: i64,
    pub message: String,
    pub trigger_at: DateTime<Utc>,
    pub repeat_cron: Option<String>,
}

/// SQLite-backed reminder store. Safe to clone (inner Arc+Mutex).
#[derive(Clone)]
pub struct ReminderStore {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for ReminderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReminderStore").finish_non_exhaustive()
    }
}

impl ReminderStore {
    pub fn open(db_path: &Path) -> Result<Self, String> {
        let conn = Connection::open(db_path).map_err(|e| format!("Open reminders DB: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS reminders (
                id              INTEGER PRIMARY KEY,
                chat_id         INTEGER NOT NULL,
                user_id         INTEGER NOT NULL,
                message         TEXT    NOT NULL,
                trigger_at      TEXT    NOT NULL,
                repeat_cron     TEXT,
                created_at      TEXT    NOT NULL,
                last_triggered  TEXT,
                active          INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_reminders_trigger
                ON reminders(trigger_at) WHERE active = 1;",
        )
        .map_err(|e| format!("Create reminders table: {e}"))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Schedule a new reminder. Returns the new reminder ID.
    pub fn set(
        &self,
        chat_id: i64,
        user_id: i64,
        message: &str,
        trigger_at: DateTime<Utc>,
        repeat_cron: Option<&str>,
    ) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO reminders (chat_id, user_id, message, trigger_at, repeat_cron, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                chat_id,
                user_id,
                message,
                trigger_at.to_rfc3339(),
                repeat_cron,
                Utc::now().to_rfc3339(),
            ],
        )
        .map_err(|e| format!("Insert reminder: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    /// List active reminders, optionally filtered by chat_id.
    pub fn list(&self, chat_id: Option<i64>) -> Result<Vec<Reminder>, String> {
        let conn = self.conn.lock().unwrap();
        let sql = if chat_id.is_some() {
            "SELECT id, chat_id, message, trigger_at, repeat_cron
             FROM reminders WHERE active = 1 AND chat_id = ?1 ORDER BY trigger_at"
        } else {
            "SELECT id, chat_id, message, trigger_at, repeat_cron
             FROM reminders WHERE active = 1 ORDER BY trigger_at"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("Prepare list: {e}"))?;
        let dummy = chat_id.unwrap_or(0);
        let rows = stmt
            .query_map(params![dummy], |row| {
                let ts: String = row.get(3)?;
                let trigger_at = DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(Reminder {
                    id: row.get(0)?,
                    chat_id: row.get(1)?,
                    message: row.get(2)?,
                    trigger_at,
                    repeat_cron: row.get(4)?,
                })
            })
            .map_err(|e| format!("Query list: {e}"))?;

        let mut result = Vec::new();
        for r in rows {
            match r {
                Ok(rem) => result.push(rem),
                Err(e) => warn!("Reminder row error: {e}"),
            }
        }
        Ok(result)
    }

    /// Cancel a reminder by ID. Returns true if a row was updated.
    pub fn cancel(&self, id: i64) -> Result<bool, String> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("UPDATE reminders SET active = 0 WHERE id = ?1", params![id])
            .map_err(|e| format!("Cancel reminder: {e}"))?;
        Ok(n > 0)
    }

    /// Fetch all reminders that are due (trigger_at <= now, active = 1).
    fn due(&self) -> Vec<Reminder> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, chat_id, message, trigger_at, repeat_cron
             FROM reminders WHERE active = 1 AND trigger_at <= ?1 ORDER BY trigger_at",
        ) else {
            return vec![];
        };
        let rows = stmt.query_map(params![now], |row| {
            let ts: String = row.get(3)?;
            let trigger_at = DateTime::parse_from_rfc3339(&ts)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(Reminder {
                id: row.get(0)?,
                chat_id: row.get(1)?,
                message: row.get(2)?,
                trigger_at,
                repeat_cron: row.get(4)?,
            })
        });
        match rows {
            Ok(iter) => iter.flatten().collect(),
            Err(e) => {
                warn!("Query due reminders: {e}");
                vec![]
            }
        }
    }

    /// Mark a reminder as fired. For periodic reminders, schedule the next occurrence.
    fn mark_fired(&self, rem: &Reminder) {
        let conn = self.conn.lock().unwrap();
        let now_str = Utc::now().to_rfc3339();

        if let Some(cron_expr) = &rem.repeat_cron {
            // Calculate next trigger_at from cron expression (simple interval parsing)
            if let Some(next) = next_from_cron(cron_expr) {
                if let Err(e) = conn.execute(
                    "UPDATE reminders SET last_triggered = ?1, trigger_at = ?2 WHERE id = ?3",
                    params![now_str, next.to_rfc3339(), rem.id],
                ) {
                    warn!("Update periodic reminder {}: {e}", rem.id);
                }
                return;
            }
        }

        // One-time: deactivate
        if let Err(e) = conn.execute(
            "UPDATE reminders SET active = 0, last_triggered = ?1 WHERE id = ?2",
            params![now_str, rem.id],
        ) {
            warn!("Deactivate reminder {}: {e}", rem.id);
        }
    }
}

/// Parse cron-like repeat expression and return next trigger time.
/// Supports simple intervals: "+30m", "+2h", "+1d", "+1w"
/// and basic daily cron "HH:MM" (fires every day at that time).
fn next_from_cron(expr: &str) -> Option<DateTime<Utc>> {
    let expr = expr.trim();

    // Relative interval: +30m, +2h, +1d, +1w
    if let Some(rest) = expr.strip_prefix('+') {
        let (num_str, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: i64 = num_str.parse().ok()?;
        return Some(match unit {
            "m" => Utc::now() + Duration::minutes(n),
            "h" => Utc::now() + Duration::hours(n),
            "d" => Utc::now() + Duration::days(n),
            "w" => Utc::now() + Duration::weeks(n),
            _ => return None,
        });
    }

    // "HH:MM" daily — fire tomorrow at that time (UTC)
    if expr.len() == 5 && expr.as_bytes()[2] == b':' {
        let h: u32 = expr[..2].parse().ok()?;
        let m: u32 = expr[3..].parse().ok()?;
        let now = Utc::now();
        let today = now.date_naive().and_hms_opt(h, m, 0)?;
        let today_utc: DateTime<Utc> = DateTime::from_naive_utc_and_offset(today, Utc);
        // If that time already passed today, schedule for tomorrow
        let next = if today_utc > now {
            today_utc
        } else {
            today_utc + Duration::days(1)
        };
        return Some(next);
    }

    None
}

/// Parse a trigger_at string from user input into a UTC datetime.
///
/// Accepts:
/// - "+30m", "+2h", "+1d", "+1w" — relative to now
/// - "YYYY-MM-DD HH:MM" or "YYYY-MM-DDTHH:MM:SS" — absolute UTC
pub fn parse_trigger_at(s: &str) -> Result<DateTime<Utc>, String> {
    let s = s.trim();

    // Relative
    if let Some(rest) = s.strip_prefix('+') {
        let (num_str, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: i64 = num_str
            .parse()
            .map_err(|_| format!("Invalid number in '{s}'"))?;
        return Ok(match unit {
            "m" => Utc::now() + Duration::minutes(n),
            "h" => Utc::now() + Duration::hours(n),
            "d" => Utc::now() + Duration::days(n),
            "w" => Utc::now() + Duration::weeks(n),
            _ => return Err(format!("Unknown unit '{unit}'. Use m/h/d/w")),
        });
    }

    // Try RFC3339 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // "YYYY-MM-DD HH:MM"
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    }

    // "YYYY-MM-DD HH:MM:SS"
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    }

    Err(format!(
        "Cannot parse '{s}'. Use '+30m', '+2h', '+1d', or 'YYYY-MM-DD HH:MM'"
    ))
}

/// Spawn the background reminder loop. Checks every 60 seconds and fires due reminders.
pub fn start_reminder_loop(store: ReminderStore, telegram: Arc<TelegramClient>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let due = store.due();
            if due.is_empty() {
                continue;
            }
            info!("⏰ {} reminder(s) due", due.len());
            for rem in &due {
                info!("⏰ Firing reminder {} to chat {}", rem.id, rem.chat_id);
                match telegram.send_message(rem.chat_id, &rem.message, None).await {
                    Ok(_) => store.mark_fired(rem),
                    Err(e) => error!("Failed to send reminder {}: {e}", rem.id),
                }
            }
        }
    });
}
