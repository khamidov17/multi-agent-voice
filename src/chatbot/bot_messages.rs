//! Shared bot-to-bot message bus backed by a SQLite database.
//!
//! Telegram bots cannot see messages from other bots in groups (Telegram API
//! limitation). This module provides a workaround: bots write their outgoing
//! group messages to a shared SQLite file, and each bot polls that file for
//! messages it has not yet processed.
//!
//! # Tables
//!
//! - `bot_messages` — directed/broadcast messages between bots
//! - `heartbeats` — per-bot liveness tracking with iteration count
//!
//! # Message Types
//!
//! Messages carry a `message_type` field for structured routing:
//! - `chat`       — normal conversation message (default for group messages)
//! - `task`       — task assignment from Nova to other bots
//! - `status`     — progress update
//! - `alert`      — critical issue (health, security)
//! - `heartbeat`  — liveness signal (stored in heartbeats table, not messages)
//!
//! `to_bot = NULL` means broadcast to every bot.
//! `read_by` is a comma-separated list of bot names that have read the row.

use rusqlite::{Connection, params};
use std::path::Path;
use tracing::{error, info, warn};

/// Known message types for structured routing.
pub mod message_type {
    pub const CHAT: &str = "chat";
    pub const TASK: &str = "task";
    pub const STATUS: &str = "status";
    pub const ALERT: &str = "alert";
}

/// A single record from the `bot_messages` table.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BotMessage {
    pub id: i64,
    pub from_bot: String,
    pub to_bot: Option<String>,
    pub message: String,
    pub message_type: String,
    pub reply_to_msg_id: Option<i64>,
    /// The Telegram message_id of the sent message, so other bots can quote it.
    pub telegram_msg_id: Option<i64>,
    pub created_at: String,
}

/// Per-bot heartbeat record from the `heartbeats` table.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BotHeartbeat {
    pub bot_name: String,
    pub last_heartbeat: String,
    pub iteration_count: i64,
    pub status: String,
    pub current_task: Option<String>,
}

/// Thin wrapper around a `rusqlite::Connection` to the shared bus database.
pub struct BotMessageDb {
    conn: Connection,
}

impl BotMessageDb {
    /// Open (or create) the shared database at `path`.
    ///
    /// Enables WAL journal mode for concurrent readers/writers and creates the
    /// table if it does not yet exist.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;

        // WAL mode: allows one writer and many concurrent readers without
        // blocking each other — essential for three separate processes.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        // Create tables (without indexes on new columns — those come after migration).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bot_messages (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                from_bot         TEXT    NOT NULL,
                to_bot           TEXT,
                message          TEXT    NOT NULL,
                message_type     TEXT    NOT NULL DEFAULT 'chat',
                reply_to_msg_id  INTEGER,
                telegram_msg_id  INTEGER,
                created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
                read_by          TEXT    NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS heartbeats (
                bot_name         TEXT    PRIMARY KEY,
                last_heartbeat   TEXT    NOT NULL,
                iteration_count  INTEGER NOT NULL DEFAULT 0,
                status           TEXT    NOT NULL DEFAULT 'idle',
                current_task     TEXT
            );

            -- Shared task board: every agent can see and claim tasks
            CREATE TABLE IF NOT EXISTS tasks (
                id               TEXT    PRIMARY KEY,
                title            TEXT    NOT NULL,
                status           TEXT    NOT NULL DEFAULT 'pending',
                assigned_to      TEXT,
                created_by       TEXT    NOT NULL,
                created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
                updated_at       TEXT    NOT NULL DEFAULT (datetime('now')),
                context          TEXT,
                result           TEXT,
                blocked_reason   TEXT,
                depends_on       TEXT
            );

            -- Typed handoffs between agents (no NLP trigger needed)
            CREATE TABLE IF NOT EXISTS handoffs (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                from_agent       TEXT    NOT NULL,
                to_agent         TEXT    NOT NULL,
                task_id          TEXT    NOT NULL,
                type             TEXT    NOT NULL,
                payload          TEXT    NOT NULL,
                status           TEXT    NOT NULL DEFAULT 'pending',
                created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_bot_messages_to
                ON bot_messages(to_bot);
            CREATE INDEX IF NOT EXISTS idx_bot_messages_created
                ON bot_messages(created_at);
            CREATE INDEX IF NOT EXISTS idx_tasks_status
                ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_handoffs_to
                ON handoffs(to_agent, status);",
        )?;

        // Migrate: add message_type column if missing (existing DBs).
        let has_message_type: bool = conn
            .prepare("SELECT message_type FROM bot_messages LIMIT 0")
            .is_ok();
        if !has_message_type {
            if let Err(e) = conn.execute_batch(
                "ALTER TABLE bot_messages ADD COLUMN message_type TEXT NOT NULL DEFAULT 'chat';",
            ) {
                warn!("Migration: message_type column may already exist: {e}");
            }
        }

        // Now create index on message_type (safe — column exists after migration).
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_bot_messages_type ON bot_messages(message_type);",
        );

        // Migrate: add status + current_task to heartbeats (existing DBs).
        let _ = conn.execute_batch(
            "ALTER TABLE heartbeats ADD COLUMN status TEXT NOT NULL DEFAULT 'idle';",
        );
        let _ = conn.execute_batch(
            "ALTER TABLE heartbeats ADD COLUMN current_task TEXT;",
        );

        info!("BotMessageDb opened at {}", path.display());
        Ok(Self { conn })
    }

    /// Insert a new message into the bus.
    ///
    /// Call this after successfully sending a Telegram message so peer bots
    /// can pick it up via polling.
    pub fn insert(
        &self,
        from_bot: &str,
        to_bot: Option<&str>,
        message: &str,
        reply_to_msg_id: Option<i64>,
        telegram_msg_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        self.insert_typed(
            from_bot,
            to_bot,
            message,
            message_type::CHAT,
            reply_to_msg_id,
            telegram_msg_id,
        )
    }

    /// Insert a message with an explicit message type.
    pub fn insert_typed(
        &self,
        from_bot: &str,
        to_bot: Option<&str>,
        message: &str,
        msg_type: &str,
        reply_to_msg_id: Option<i64>,
        telegram_msg_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        self.conn.execute(
            "INSERT INTO bot_messages
                 (from_bot, to_bot, message, message_type, reply_to_msg_id, telegram_msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                from_bot,
                to_bot,
                message,
                msg_type,
                reply_to_msg_id,
                telegram_msg_id
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Send a task assignment to a specific bot.
    pub fn send_task(
        &self,
        from_bot: &str,
        to_bot: &str,
        task_description: &str,
    ) -> anyhow::Result<i64> {
        self.insert_typed(
            from_bot,
            Some(to_bot),
            task_description,
            message_type::TASK,
            None,
            None,
        )
    }

    /// Broadcast an alert to all bots.
    pub fn send_alert(&self, from_bot: &str, alert_message: &str) -> anyhow::Result<i64> {
        self.insert_typed(
            from_bot,
            None,
            alert_message,
            message_type::ALERT,
            None,
            None,
        )
    }

    /// Update the heartbeat for a bot with status (upsert into heartbeats table).
    /// `status`: idle, working, waiting, blocked
    /// `current_task`: what the bot is currently doing (None to keep existing)
    pub fn heartbeat_with_status(
        &self,
        bot_name: &str,
        status: &str,
        current_task: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO heartbeats (bot_name, last_heartbeat, iteration_count, status, current_task)
             VALUES (?1, datetime('now'), 1, ?2, ?3)
             ON CONFLICT(bot_name) DO UPDATE SET
                 last_heartbeat = datetime('now'),
                 iteration_count = iteration_count + 1,
                 status = ?2,
                 current_task = COALESCE(?3, current_task)",
            params![bot_name, status, current_task],
        )?;
        Ok(())
    }

    /// Simple heartbeat (backward compatible).
    pub fn heartbeat(&self, bot_name: &str) -> anyhow::Result<()> {
        self.heartbeat_with_status(bot_name, "idle", None)
    }

    /// Get all heartbeats (for health monitoring).
    pub fn get_heartbeats(&self) -> anyhow::Result<Vec<BotHeartbeat>> {
        let mut stmt = self.conn.prepare(
            "SELECT bot_name, last_heartbeat, iteration_count, status, current_task FROM heartbeats ORDER BY bot_name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BotHeartbeat {
                bot_name: row.get(0)?,
                last_heartbeat: row.get(1)?,
                iteration_count: row.get(2)?,
                status: row.get::<_, String>(3).unwrap_or_else(|_| "idle".to_string()),
                current_task: row.get(4).ok(),
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Return all messages that:
    /// - were NOT sent by `this_bot`, AND
    /// - are addressed to `this_bot` or are a broadcast (`to_bot IS NULL`), AND
    /// - have NOT yet been read by `this_bot`.
    ///
    /// The caller is responsible for calling [`mark_read`] on each returned id.
    pub fn unread_for(&self, this_bot: &str) -> anyhow::Result<Vec<BotMessage>> {
        // We filter read_by client-side to avoid SQL LIKE edge cases with commas.
        let mut stmt = self.conn.prepare(
            "SELECT id, from_bot, to_bot, message, message_type, reply_to_msg_id,
                    telegram_msg_id, created_at, read_by
             FROM   bot_messages
             WHERE  from_bot != ?1
               AND  (to_bot IS NULL OR to_bot = ?1)
             ORDER  BY id ASC",
        )?;

        let rows = stmt.query_map(params![this_bot], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (
                id,
                from_bot,
                to_bot,
                message,
                msg_type,
                reply_to_msg_id,
                telegram_msg_id,
                created_at,
                read_by,
            ) = row?;

            // Skip if this bot has already read this message.
            let already_read = read_by
                .split(',')
                .map(str::trim)
                .any(|name| name.eq_ignore_ascii_case(this_bot));

            if !already_read {
                messages.push(BotMessage {
                    id,
                    from_bot,
                    to_bot,
                    message,
                    message_type: msg_type,
                    reply_to_msg_id,
                    telegram_msg_id,
                    created_at,
                });
            }
        }

        Ok(messages)
    }

    /// Return unread messages of type `task` addressed to `this_bot`.
    ///
    /// Used by the engine after CC STOP to check whether there are pending
    /// task-type messages that need autonomous continuation.
    pub fn pending_tasks_for(&self, this_bot: &str) -> anyhow::Result<Vec<BotMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_bot, to_bot, message, message_type, reply_to_msg_id,
                    telegram_msg_id, created_at, read_by
             FROM   bot_messages
             WHERE  from_bot != ?1
               AND  (to_bot IS NULL OR to_bot = ?1)
               AND  message_type = 'task'
             ORDER  BY id ASC",
        )?;

        let rows = stmt.query_map(params![this_bot], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (
                id,
                from_bot,
                to_bot,
                message,
                msg_type,
                reply_to_msg_id,
                telegram_msg_id,
                created_at,
                read_by,
            ) = row?;

            let already_read = read_by
                .split(',')
                .map(str::trim)
                .any(|name| name.eq_ignore_ascii_case(this_bot));

            if !already_read {
                messages.push(BotMessage {
                    id,
                    from_bot,
                    to_bot,
                    message,
                    message_type: msg_type,
                    reply_to_msg_id,
                    telegram_msg_id,
                    created_at,
                });
            }
        }

        Ok(messages)
    }

    /// Append `this_bot` to the `read_by` list of message `id`.
    /// Uses BEGIN IMMEDIATE to prevent race conditions when multiple bots
    /// poll and mark_read within the same 500ms window.
    pub fn mark_read(&self, id: i64, this_bot: &str) -> anyhow::Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;

        let current: String = self
            .conn
            .query_row(
                "SELECT read_by FROM bot_messages WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or_default();

        // Check if already marked (idempotent)
        let already = current.split(',').map(str::trim).any(|n| n.eq_ignore_ascii_case(this_bot));
        if already {
            self.conn.execute_batch("COMMIT")?;
            return Ok(());
        }

        let updated = if current.trim().is_empty() {
            this_bot.to_string()
        } else {
            format!("{},{}", current, this_bot)
        };

        self.conn.execute(
            "UPDATE bot_messages SET read_by = ?1 WHERE id = ?2",
            params![updated, id],
        )?;

        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
}

/// Spawn a background task that polls the shared bot-message bus every 500 ms
/// and injects new peer-bot messages into the engine's pending queue.
///
/// # Arguments
///
/// * `db_path`       — path to the shared `bot_messages.db`
/// * `this_bot`      — name of the current bot (e.g. `"Atlas"`)
/// * `group_chat_id` — the primary Telegram group chat id (e.g. `-1003399442526`)
/// * `pending`       — the engine's pending-message queue
/// * `debouncer`     — the engine's `Debouncer`; `.trigger()` is called after injecting messages
pub fn start_polling(
    db_path: std::path::PathBuf,
    this_bot: String,
    group_chat_id: i64,
    pending: std::sync::Arc<tokio::sync::Mutex<Vec<crate::chatbot::message::ChatMessage>>>,
    debouncer: std::sync::Arc<crate::chatbot::debounce::Debouncer>,
) {
    tokio::spawn(async move {
        // Open DB — retry with exponential back-off if the file is not yet
        // available (a peer bot might not have created it yet).
        let db = loop {
            match BotMessageDb::open(&db_path) {
                Ok(db) => break db,
                Err(e) => {
                    error!("BotMessageDb open error, retrying in 2s: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        };

        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;

            let messages = match db.unread_for(&this_bot) {
                Ok(m) => m,
                Err(e) => {
                    error!("BotMessageDb poll error: {e}");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            info!(
                "BotMessageDb: {} new peer message(s) for {}",
                messages.len(),
                this_bot
            );

            // Push ALL messages to pending FIRST, then trigger debouncer ONCE.
            // This ensures they batch into a single processing turn instead of
            // triggering separate turns for each message.
            {
                let mut p = pending.lock().await;
                for bm in &messages {
                    if let Err(e) = db.mark_read(bm.id, &this_bot) {
                        error!("mark_read failed for id {}: {e}", bm.id);
                    }

                    let sender_user_id = bot_name_to_user_id(&bm.from_bot);
                    let telegram_msg_id = bm.telegram_msg_id.unwrap_or(bm.id);

                    p.push(crate::chatbot::message::ChatMessage {
                        message_id: telegram_msg_id,
                        chat_id: group_chat_id,
                        user_id: sender_user_id,
                        username: bm.from_bot.clone(),
                        first_name: Some(bm.from_bot.clone()),
                        timestamp: bm.created_at.clone(),
                        text: bm.message.clone(),
                        reply_to: None,
                        photo_file_id: None,
                        image: None,
                        voice_transcription: None,
                    });
                }
            }

            // Single trigger for the entire batch.
            debouncer.trigger().await;
        }
    });
}

/// Map a bot display name to its known Telegram user_id.
///
/// These IDs are the real bot accounts in the claudir architecture.
/// If an unknown name is supplied (e.g. in tests) we fall back to 0.
pub fn bot_name_to_user_id(name: &str) -> i64 {
    match name {
        "Atlas" => 8_446_778_880,
        "Nova" => 8_338_468_521,
        "Security" => 8_373_868_633,
        _ => 0,
    }
}
