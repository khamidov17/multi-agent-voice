//! Persistent SQLite database for messages and members.

use crate::chatbot::message::ChatMessage;
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, info, warn};

/// Member status in the group.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberStatus {
    Member,
    Left,
    Banned,
}

impl MemberStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "left" => MemberStatus::Left,
            "banned" => MemberStatus::Banned,
            _ => MemberStatus::Member,
        }
    }
}

/// A group member.
#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: i64,
    pub username: Option<String>,
    pub first_name: String,
    pub join_date: String,
    pub last_message_date: Option<String>,
    pub message_count: u32,
    pub status: MemberStatus,
}

/// Persistent SQLite database for the chatbot.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Create a new in-memory database.
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().expect("Failed to create in-memory database");
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_schema();
        db
    }

    /// Load from file if it exists, otherwise create new.
    pub fn load_or_new(path: &Path) -> Self {
        // Check if we need to migrate from JSON
        let json_path = path.with_extension("json");
        let db_exists = path.exists();

        let conn = Connection::open(path).expect("Failed to open database");
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_schema();

        // Migrate from JSON if database is new and JSON exists
        if !db_exists && json_path.exists() {
            info!("Migrating from JSON database: {:?}", json_path);
            if let Err(e) = db.migrate_from_json(&json_path) {
                warn!("Migration failed: {e}");
            }
        }

        let (msg_count, member_count) = db.get_counts();
        info!(
            "Loaded database from {:?} ({} messages, {} members)",
            path, msg_count, member_count
        );

        db
    }

    fn init_schema(&self) {
        let conn = self.conn.lock().unwrap();

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                message_id INTEGER PRIMARY KEY,
                chat_id INTEGER NOT NULL,
                user_id INTEGER NOT NULL,
                username TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                text TEXT NOT NULL,
                reply_to_id INTEGER,
                reply_to_username TEXT,
                reply_to_text TEXT
            );

            CREATE TABLE IF NOT EXISTS users (
                user_id INTEGER PRIMARY KEY,
                username TEXT,
                first_name TEXT NOT NULL,
                join_date TEXT NOT NULL,
                last_message_date TEXT,
                message_count INTEGER DEFAULT 0,
                status TEXT DEFAULT 'member'
            );

            CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp);
            CREATE INDEX IF NOT EXISTS idx_messages_user_id ON messages(user_id);
            CREATE INDEX IF NOT EXISTS idx_messages_username ON messages(username);
            CREATE INDEX IF NOT EXISTS idx_messages_chat_id ON messages(chat_id);
            CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
            CREATE INDEX IF NOT EXISTS idx_users_status ON users(status);

            -- Spam enforcement
            CREATE TABLE IF NOT EXISTS strikes (
                user_id         INTEGER PRIMARY KEY,
                count           INTEGER NOT NULL DEFAULT 0,
                last_strike_at  TEXT
            );

            -- Focus Mode: singleton holding the currently focused chat
            CREATE TABLE IF NOT EXISTS focus_state (
                id              INTEGER PRIMARY KEY CHECK (id = 1),
                focused_chat_id INTEGER,
                focused_at      TEXT
            );

            -- Focus Mode: per-chat cursor tracking
            CREATE TABLE IF NOT EXISTS focus_chats (
                chat_id             INTEGER PRIMARY KEY,
                chat_title          TEXT,
                cursor_message_id   INTEGER DEFAULT 0,
                last_injected_at    TEXT,
                last_pending_count  INTEGER,
                created_at          TEXT NOT NULL
            );

            -- Focus Mode: human-friendly names for chat IDs
            CREATE TABLE IF NOT EXISTS chat_aliases (
                alias       TEXT PRIMARY KEY,
                chat_id     INTEGER NOT NULL,
                created_at  TEXT NOT NULL
            );

            -- Muted group chats
            CREATE TABLE IF NOT EXISTS muted_chats (
                chat_id             INTEGER PRIMARY KEY,
                muted_at            TEXT NOT NULL,
                muted_until         TEXT,
                reason              TEXT,
                messages_received   INTEGER DEFAULT 0,
                unique_users        TEXT DEFAULT '[]'
            );

            -- Muted DMs
            CREATE TABLE IF NOT EXISTS muted_dms (
                id                  INTEGER PRIMARY KEY CHECK (id = 1),
                muted_at            TEXT NOT NULL,
                muted_until         TEXT,
                reason              TEXT,
                messages_received   INTEGER DEFAULT 0,
                unique_users        TEXT DEFAULT '[]'
            );

            -- Billing: star balances
            CREATE TABLE IF NOT EXISTS user_balances (
                user_id         INTEGER PRIMARY KEY,
                balance         INTEGER DEFAULT 0,
                total_deposited INTEGER DEFAULT 0,
                total_spent     INTEGER DEFAULT 0,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            -- Billing: transaction audit log
            CREATE TABLE IF NOT EXISTS transactions (
                id                  INTEGER PRIMARY KEY,
                user_id             INTEGER NOT NULL,
                amount              INTEGER NOT NULL,
                balance_after       INTEGER NOT NULL,
                transaction_type    TEXT NOT NULL,
                description         TEXT,
                created_at          TEXT NOT NULL
            );

            -- Billing: DM rate limiting
            CREATE TABLE IF NOT EXISTS dm_rate_limits (
                user_id         INTEGER NOT NULL,
                hour            TEXT NOT NULL,
                message_count   INTEGER DEFAULT 0,
                PRIMARY KEY (user_id, hour)
            );

            -- Billing: free trial tracking
            CREATE TABLE IF NOT EXISTS dm_free_trial (
                user_id         INTEGER PRIMARY KEY,
                messages_used   INTEGER DEFAULT 0,
                created_at      TEXT NOT NULL
            );

            -- Billing: write-ahead log for crash-safe billing
            -- Records intent BEFORE deducting stars; incomplete entries
            -- auto-refunded on startup.
            CREATE TABLE IF NOT EXISTS pending_dms (
                id              INTEGER PRIMARY KEY,
                user_id         INTEGER NOT NULL,
                chat_id         INTEGER NOT NULL,
                message_text    TEXT,
                cost            INTEGER NOT NULL,
                created_at      TEXT NOT NULL,
                completed_at    TEXT
            );

            -- Billing: privacy consent tracking
            CREATE TABLE IF NOT EXISTS dm_privacy_consent (
                user_id         INTEGER PRIMARY KEY,
                consent_version TEXT NOT NULL,
                consented_at    TEXT NOT NULL,
                declined_at     TEXT
            );

            -- Search: message embeddings for semantic auto-recall
            CREATE TABLE IF NOT EXISTS embeddings (
                chat_id     INTEGER NOT NULL,
                message_id  INTEGER NOT NULL,
                embedding   BLOB NOT NULL,
                text_preview TEXT NOT NULL,
                username    TEXT NOT NULL,
                timestamp   TEXT NOT NULL,
                PRIMARY KEY (chat_id, message_id)
            );

            -- Search: memory file embeddings for RAG
            CREATE TABLE IF NOT EXISTS memory_embeddings (
                id          INTEGER PRIMARY KEY,
                file_path   TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                chunk_text  TEXT NOT NULL,
                embedding   BLOB NOT NULL,
                updated_at  TEXT NOT NULL,
                UNIQUE(file_path, chunk_index)
            );

            -- Search: LLM-generated message chunk summaries (PageIndex)
            CREATE TABLE IF NOT EXISTS page_index (
                id                  INTEGER PRIMARY KEY,
                chat_id             INTEGER NOT NULL,
                start_message_id    INTEGER NOT NULL,
                end_message_id      INTEGER NOT NULL,
                start_timestamp     TEXT NOT NULL,
                end_timestamp       TEXT NOT NULL,
                message_count       INTEGER NOT NULL,
                summary             TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                UNIQUE(chat_id, start_message_id)
            );

            -- Operations: cross-bot heartbeat monitoring
            CREATE TABLE IF NOT EXISTS heartbeats (
                bot_name        TEXT PRIMARY KEY,
                last_heartbeat  TEXT NOT NULL,
                iteration_count INTEGER DEFAULT 0
            );

            -- Operations: channel post rate limiting
            CREATE TABLE IF NOT EXISTS channel_posts (
                chat_id     INTEGER NOT NULL,
                date        TEXT NOT NULL,
                post_count  INTEGER DEFAULT 0,
                PRIMARY KEY (chat_id, date)
            );
        "#,
        )
        .expect("Failed to initialize database schema");
    }

    fn get_counts(&self) -> (usize, usize) {
        let conn = self.conn.lock().unwrap();
        let msg_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap_or(0);
        let member_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .unwrap_or(0);
        (msg_count as usize, member_count as usize)
    }

    /// Migrate data from a JSON database file.
    fn migrate_from_json(&self, json_path: &Path) -> Result<(), String> {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        struct JsonMember {
            user_id: i64,
            username: Option<String>,
            first_name: String,
            join_date: String,
            last_message_date: Option<String>,
            message_count: u32,
            #[serde(default = "default_status")]
            status: String,
        }

        fn default_status() -> String {
            "member".to_string()
        }

        #[derive(Serialize, Deserialize)]
        struct JsonReplyTo {
            message_id: i64,
            username: String,
            text: String,
        }

        #[derive(Serialize, Deserialize)]
        struct JsonMessage {
            message_id: i64,
            chat_id: i64,
            user_id: i64,
            username: String,
            timestamp: String,
            text: String,
            reply_to: Option<JsonReplyTo>,
        }

        #[derive(Serialize, Deserialize)]
        struct JsonDatabase {
            messages: Vec<JsonMessage>,
            #[serde(default)]
            members: Vec<JsonMember>,
        }

        let json =
            std::fs::read_to_string(json_path).map_err(|e| format!("Failed to read JSON: {e}"))?;

        let data: JsonDatabase =
            serde_json::from_str(&json).map_err(|e| format!("Failed to parse JSON: {e}"))?;

        let conn = self.conn.lock().unwrap();

        // Import members
        for m in &data.members {
            conn.execute(
                "INSERT OR REPLACE INTO users (user_id, username, first_name, join_date, last_message_date, message_count, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![m.user_id, m.username, m.first_name, m.join_date, m.last_message_date, m.message_count, m.status]
            ).map_err(|e| format!("Failed to insert member: {e}"))?;
        }

        // Import messages
        for msg in &data.messages {
            let (reply_id, reply_user, reply_text) = match &msg.reply_to {
                Some(r) => (
                    Some(r.message_id),
                    Some(r.username.clone()),
                    Some(r.text.clone()),
                ),
                None => (None, None, None),
            };

            conn.execute(
                "INSERT OR REPLACE INTO messages (message_id, chat_id, user_id, username, timestamp, text, reply_to_id, reply_to_username, reply_to_text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![msg.message_id, msg.chat_id, msg.user_id, msg.username, msg.timestamp, msg.text, reply_id, reply_user, reply_text]
            ).map_err(|e| format!("Failed to insert message: {e}"))?;
        }

        info!(
            "Migrated {} messages and {} members from JSON",
            data.messages.len(),
            data.members.len()
        );
        Ok(())
    }

    /// Save is a no-op for SQLite (auto-committed).
    pub fn save(&self) -> Result<(), String> {
        Ok(())
    }

    // ==================== MESSAGE METHODS ====================

    /// Add a message to the database.
    pub fn add_message(&mut self, msg: ChatMessage) {
        let conn = self.conn.lock().unwrap();

        // Insert or update user — use real first_name if available, else fall back to username
        let first_name = msg.first_name.as_deref().unwrap_or(&msg.username);
        conn.execute(
            "INSERT INTO users (user_id, username, first_name, join_date, last_message_date, message_count, status)
             VALUES (?1, ?2, ?3, ?4, ?4, 1, 'member')
             ON CONFLICT(user_id) DO UPDATE SET
                username = COALESCE(?2, username),
                first_name = ?3,
                last_message_date = ?4,
                message_count = message_count + 1",
            params![msg.user_id, msg.username, first_name, msg.timestamp]
        ).unwrap_or_else(|e| {
            warn!("Failed to update user: {e}");
            0
        });

        // Insert message
        let (reply_id, reply_user, reply_text) = match &msg.reply_to {
            Some(r) => (
                Some(r.message_id),
                Some(r.username.clone()),
                Some(r.text.clone()),
            ),
            None => (None, None, None),
        };

        conn.execute(
            "INSERT OR REPLACE INTO messages (message_id, chat_id, user_id, username, timestamp, text, reply_to_id, reply_to_username, reply_to_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![msg.message_id, msg.chat_id, msg.user_id, msg.username, msg.timestamp, msg.text, reply_id, reply_user, reply_text]
        ).unwrap_or_else(|e| {
            warn!("Failed to insert message: {e}");
            0
        });
    }

    /// Get the timestamp of the most recent message across all chats.
    /// Returns `None` if the database is empty.
    pub fn last_message_timestamp(&self) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT timestamp FROM messages ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok()
    }

    /// Get recent message history for context restoration after a session reset.
    /// Returns messages formatted as XML (same format Claude normally sees).
    /// `limit` controls how many messages to retrieve (most recent first, returned chronological).
    pub fn get_recent_history(&self, limit: usize) -> Vec<ChatMessage> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT message_id, chat_id, user_id, username, timestamp, text,
                    reply_to_id, reply_to_username, reply_to_text
             FROM messages
             ORDER BY rowid DESC
             LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut messages: Vec<ChatMessage> = stmt
            .query_map(params![limit as i64], |row| {
                let reply_to_id: Option<i64> = row.get(6)?;
                let reply_to = reply_to_id.map(|id| {
                    let username: String = row.get::<_, String>(7).unwrap_or_default();
                    let text: String = row.get::<_, String>(8).unwrap_or_default();
                    super::message::ReplyTo {
                        message_id: id,
                        username,
                        text,
                    }
                });

                Ok(ChatMessage {
                    message_id: row.get(0)?,
                    chat_id: row.get(1)?,
                    user_id: row.get(2)?,
                    username: row.get(3)?,
                    first_name: None,
                    timestamp: row.get(4)?,
                    text: row.get(5)?,
                    reply_to,
                    photo_file_id: None,
                    image: None,
                    voice_transcription: None,
                })
            })
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();

        // Reverse so messages are in chronological order
        messages.reverse();
        messages
    }

    /// Total message count.
    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM messages", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    }

    /// Execute a raw SELECT query and return results as formatted strings.
    /// SECURITY: Only SELECT queries on whitelisted tables are allowed.
    pub fn query(&self, sql: &str) -> Result<String, String> {
        let sql_trimmed = sql.trim();

        // Validate it's a SELECT query
        if !sql_trimmed.to_uppercase().starts_with("SELECT") {
            return Err("Only SELECT queries are allowed".to_string());
        }

        // Block dangerous patterns
        let sql_upper = sql_trimmed.to_uppercase();
        for pattern in [
            "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "ATTACH", "DETACH",
        ] {
            if sql_upper.contains(pattern) {
                return Err(format!("Query contains forbidden keyword: {pattern}"));
            }
        }

        // SECURITY: Table whitelist — only allow queries on safe tables.
        // Billing, PII, and internal tables are blocked to prevent data exposure.
        const ALLOWED_TABLES: &[&str] = &["messages", "users", "strikes"];
        const BLOCKED_TABLES: &[&str] = &[
            "USER_BALANCES",
            "TRANSACTIONS",
            "PENDING_DMS",
            "DM_RATE_LIMITS",
            "DM_FREE_TRIAL",
            "DM_PRIVACY_CONSENT",
            "FOCUS_STATE",
            "FOCUS_CHATS",
            "MUTED_CHATS",
            "MUTED_DMS",
            "EMBEDDINGS",
            "MEMORY_EMBEDDINGS",
            "PAGE_INDEX",
            "HEARTBEATS",
            "CHANNEL_POSTS",
            "CHAT_ALIASES",
            "SQLITE_MASTER",
            "SQLITE_SCHEMA",
            "REMINDERS",
        ];
        for blocked in BLOCKED_TABLES {
            if sql_upper.contains(blocked) {
                return Err(format!(
                    "Access denied: restricted table. Only these tables are queryable: {}",
                    ALLOWED_TABLES.join(", ")
                ));
            }
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(sql_trimmed)
            .map_err(|e| format!("Query error: {e}"))?;

        let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let mut rows = stmt
            .query([])
            .map_err(|e| format!("Query execution error: {e}"))?;

        let mut results: Vec<String> = Vec::new();
        let mut row_count = 0;
        const MAX_ROWS: usize = 100;

        while let Some(row) = rows.next().map_err(|e| format!("Row fetch error: {e}"))? {
            if row_count >= MAX_ROWS {
                results.push(format!("... (truncated, showing first {} rows)", MAX_ROWS));
                break;
            }

            let mut values: Vec<String> = Vec::new();
            for (i, col_name) in column_names.iter().enumerate() {
                let value: String = row
                    .get::<_, rusqlite::types::Value>(i)
                    .map(|v| match v {
                        rusqlite::types::Value::Null => "NULL".to_string(),
                        rusqlite::types::Value::Integer(n) => n.to_string(),
                        rusqlite::types::Value::Real(f) => f.to_string(),
                        rusqlite::types::Value::Text(s) => {
                            // Use chars() to respect UTF-8 character boundaries
                            if s.chars().count() > 100 {
                                format!("{}...", s.chars().take(100).collect::<String>())
                            } else {
                                s
                            }
                        }
                        rusqlite::types::Value::Blob(b) => format!("<blob {} bytes>", b.len()),
                    })
                    .unwrap_or_else(|_| "?".to_string());
                values.push(format!("{col_name}: {value}"));
            }
            results.push(values.join(" | "));
            row_count += 1;
        }

        if results.is_empty() {
            Ok("No results".to_string())
        } else {
            Ok(format!("{} row(s):\n{}", row_count, results.join("\n")))
        }
    }

    // ==================== MEMBER METHODS ====================

    /// Import members from a JSON array.
    pub fn import_members(&mut self, members_json: &str) -> Result<usize, String> {
        #[derive(serde::Deserialize)]
        struct ImportMember {
            #[serde(alias = "id")]
            user_id: i64,
            #[serde(default)]
            username: Option<String>,
            #[serde(default, alias = "name")]
            first_name: Option<String>,
        }

        let imported: Vec<ImportMember> = serde_json::from_str(members_json)
            .map_err(|e| format!("Failed to parse members JSON: {e}"))?;

        let conn = self.conn.lock().unwrap();
        let timestamp = "imported";
        let mut count = 0;

        for m in imported {
            let first_name = m
                .first_name
                .or_else(|| m.username.clone())
                .unwrap_or_else(|| format!("User{}", m.user_id));

            let result = conn.execute(
                "INSERT OR IGNORE INTO users (user_id, username, first_name, join_date, status) VALUES (?1, ?2, ?3, ?4, 'member')",
                params![m.user_id, m.username, first_name, timestamp]
            );

            if let Ok(n) = result {
                count += n;
            }
        }

        if count > 0 {
            info!("📥 Imported {} new members", count);
        }

        Ok(count)
    }

    /// Record a member joining.
    pub fn member_joined(
        &mut self,
        user_id: i64,
        username: Option<String>,
        first_name: String,
        timestamp: String,
    ) {
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO users (user_id, username, first_name, join_date, status)
             VALUES (?1, ?2, ?3, ?4, 'member')
             ON CONFLICT(user_id) DO UPDATE SET
                username = ?2,
                first_name = ?3,
                status = 'member'",
            params![user_id, username, first_name, timestamp],
        )
        .unwrap_or_else(|e| {
            warn!("Failed to record member join: {e}");
            0
        });

        info!("👋 Member joined: {} ({})", first_name, user_id);
    }

    /// Record a member leaving.
    pub fn member_left(&mut self, user_id: i64) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET status = 'left' WHERE user_id = ?1",
            params![user_id],
        )
        .unwrap_or_else(|e| {
            warn!("Failed to record member left: {e}");
            0
        });
        debug!("👋 Member left: {}", user_id);
    }

    /// Record a member being banned.
    pub fn member_banned(&mut self, user_id: i64) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET status = 'banned' WHERE user_id = ?1",
            params![user_id],
        )
        .unwrap_or_else(|e| {
            warn!("Failed to record member banned: {e}");
            0
        });
        info!("🚫 Member banned: {}", user_id);
    }

    /// Find a user by username (case-insensitive partial match).
    pub fn find_user_by_username(&self, username: &str) -> Option<Member> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", username.to_lowercase());

        conn.query_row(
            "SELECT user_id, username, first_name, join_date, last_message_date, message_count, status
             FROM users WHERE LOWER(username) LIKE ?1 LIMIT 1",
            params![pattern],
            |row| Ok(Member {
                user_id: row.get(0)?,
                username: row.get(1)?,
                first_name: row.get(2)?,
                join_date: row.get(3)?,
                last_message_date: row.get(4)?,
                message_count: row.get::<_, i64>(5)? as u32,
                status: MemberStatus::from_str(&row.get::<_, String>(6)?),
            })
        ).ok()
    }

    /// Get members with optional filter.
    pub fn get_members(
        &self,
        filter: Option<&str>,
        days_inactive: Option<i64>,
        limit: usize,
    ) -> Vec<Member> {
        let conn = self.conn.lock().unwrap();
        let days = days_inactive.unwrap_or(30);
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let cutoff_str = cutoff.format("%Y-%m-%d %H:%M").to_string();

        let filter_str = filter.unwrap_or("all");
        let sql = match filter_str {
            "active" => {
                "SELECT * FROM users WHERE status = 'member' AND last_message_date IS NOT NULL ORDER BY last_message_date ASC LIMIT ?1"
            }
            "inactive" => {
                "SELECT * FROM users WHERE status = 'member' AND (last_message_date IS NULL OR last_message_date < ?2) ORDER BY COALESCE(last_message_date, join_date) ASC LIMIT ?1"
            }
            "never_posted" => {
                "SELECT * FROM users WHERE status = 'member' AND last_message_date IS NULL ORDER BY join_date ASC LIMIT ?1"
            }
            "left" => "SELECT * FROM users WHERE status = 'left' ORDER BY join_date ASC LIMIT ?1",
            "banned" => {
                "SELECT * FROM users WHERE status = 'banned' ORDER BY join_date ASC LIMIT ?1"
            }
            _ => "SELECT * FROM users ORDER BY COALESCE(last_message_date, join_date) ASC LIMIT ?1",
        };

        let mut stmt = conn.prepare(sql).unwrap();
        let limit_i64 = limit as i64;

        let mut results = Vec::new();
        let mut rows = if filter_str == "inactive" {
            stmt.query(params![limit_i64, cutoff_str]).unwrap()
        } else {
            stmt.query(params![limit_i64]).unwrap()
        };

        while let Ok(Some(row)) = rows.next() {
            if let Ok(member) = (|| -> rusqlite::Result<Member> {
                Ok(Member {
                    user_id: row.get(0)?,
                    username: row.get(1)?,
                    first_name: row.get(2)?,
                    join_date: row.get(3)?,
                    last_message_date: row.get(4)?,
                    message_count: row.get::<_, i64>(5)? as u32,
                    status: MemberStatus::from_str(&row.get::<_, String>(6)?),
                })
            })() {
                results.push(member);
            }
        }

        results
    }

    /// Get member count (active members only).
    pub fn member_count(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM users WHERE status = 'member'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    }

    /// Get total members ever seen.
    pub fn total_members_seen(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    // -----------------------------------------------------------------------
    // Billing: crash-safe WAL for DM payments
    // -----------------------------------------------------------------------

    /// Record intent to charge before processing a DM.
    /// Returns the pending_dms row id.
    pub fn begin_dm_charge(
        &self,
        user_id: i64,
        chat_id: i64,
        message_text: &str,
        cost: i64,
    ) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        match conn.execute(
            "INSERT INTO pending_dms (user_id, chat_id, message_text, cost, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user_id, chat_id, message_text, cost, now],
        ) {
            Ok(_) => Some(conn.last_insert_rowid()),
            Err(e) => {
                warn!("Failed to record pending DM charge: {e}");
                None
            }
        }
    }

    /// Mark a pending DM charge as completed (message delivered).
    pub fn complete_dm_charge(&self, pending_id: i64) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        if let Err(e) = conn.execute(
            "UPDATE pending_dms SET completed_at = ?1 WHERE id = ?2",
            params![now, pending_id],
        ) {
            warn!("Failed to complete pending DM charge {pending_id}: {e}");
        }
    }

    /// Recover incomplete DM charges on startup — auto-refund undelivered messages.
    /// Returns the number of refunded entries.
    pub fn recover_pending_dms(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT id, user_id, cost FROM pending_dms WHERE completed_at IS NULL")
        {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to query pending DMs for recovery: {e}");
                return 0;
            }
        };

        let rows: Vec<(i64, i64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .ok()
            .map(|iter| iter.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();

        let mut refunded = 0;
        for (id, user_id, cost) in &rows {
            // Refund: add cost back to balance
            if let Err(e) = conn.execute(
                "UPDATE user_balances SET balance = balance + ?1, total_spent = total_spent - ?1, updated_at = datetime('now') WHERE user_id = ?2",
                params![cost, user_id],
            ) {
                warn!("Failed to refund user {user_id} for pending DM {id}: {e}");
                continue;
            }
            // Mark as completed (refunded)
            let _ = conn.execute(
                "UPDATE pending_dms SET completed_at = 'refunded' WHERE id = ?1",
                params![id],
            );
            info!("Refunded {cost} stars to user {user_id} for undelivered DM (pending_id={id})");
            refunded += 1;
        }

        if refunded > 0 {
            info!("Recovered {refunded} incomplete DM charge(s) on startup");
        }
        refunded
    }

    // -----------------------------------------------------------------------
    // Focus Mode
    // -----------------------------------------------------------------------

    /// Get the currently focused chat ID (None = process all).
    pub fn get_focus(&self) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT focused_chat_id FROM focus_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .ok()
    }

    /// Set the focused chat. Pass None to clear focus (process all chats).
    pub fn set_focus(&self, chat_id: Option<i64>) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let _ = conn.execute(
            "INSERT INTO focus_state (id, focused_chat_id, focused_at)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET focused_chat_id = ?1, focused_at = ?2",
            params![chat_id, now],
        );
    }

    /// Update the cursor for a chat (last processed message_id).
    pub fn update_focus_cursor(&self, chat_id: i64, message_id: i64) {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let _ = conn.execute(
            "INSERT INTO focus_chats (chat_id, cursor_message_id, created_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET cursor_message_id = ?2",
            params![chat_id, message_id, now],
        );
    }

    /// Get pending messages for a chat (messages after the cursor).
    pub fn get_pending_messages(&self, chat_id: i64) -> Vec<ChatMessage> {
        let conn = self.conn.lock().unwrap();
        let cursor: i64 = conn
            .query_row(
                "SELECT cursor_message_id FROM focus_chats WHERE chat_id = ?1",
                params![chat_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let mut stmt = match conn.prepare(
            "SELECT message_id, chat_id, user_id, username, timestamp, text,
                    reply_to_id, reply_to_username, reply_to_text
             FROM messages
             WHERE chat_id = ?1 AND message_id > ?2
             ORDER BY message_id ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        stmt.query_map(params![chat_id, cursor], |row| {
            let reply_to_id: Option<i64> = row.get(6)?;
            let reply_to = reply_to_id.map(|id| {
                let username: String = row.get::<_, String>(7).unwrap_or_default();
                let text: String = row.get::<_, String>(8).unwrap_or_default();
                super::message::ReplyTo {
                    message_id: id,
                    username,
                    text,
                }
            });

            Ok(ChatMessage {
                message_id: row.get(0)?,
                chat_id: row.get(1)?,
                user_id: row.get(2)?,
                username: row.get(3)?,
                first_name: None,
                timestamp: row.get(4)?,
                text: row.get(5)?,
                reply_to,
                photo_file_id: None,
                image: None,
                voice_transcription: None,
            })
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Disaster recovery
    // -----------------------------------------------------------------------

    /// Run SQLite integrity check. Returns Ok(()) if clean, Err with details otherwise.
    pub fn integrity_check(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let result: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap_or_else(|e| format!("integrity_check failed: {e}"));

        if result == "ok" { Ok(()) } else { Err(result) }
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(id: i64, user_id: i64, username: &str, timestamp: &str, text: &str) -> ChatMessage {
        ChatMessage {
            message_id: id,
            chat_id: -12345,
            user_id,
            username: username.to_string(),
            first_name: None,
            timestamp: timestamp.to_string(),
            text: text.to_string(),
            reply_to: None,
            photo_file_id: None,
            image: None,
            voice_transcription: None,
        }
    }

    #[test]
    fn test_add_message_creates_member() {
        let mut db = Database::new();
        db.add_message(make_msg(1, 100, "alice", "2024-01-15 10:00", "hello"));

        assert_eq!(db.message_count(), 1);
        assert!(db.find_user_by_username("alice").is_some());
    }

    #[test]
    fn test_query_basic() {
        let mut db = Database::new();
        db.add_message(make_msg(1, 100, "alice", "2024-01-15 10:00", "hello"));
        db.add_message(make_msg(2, 101, "bob", "2024-01-15 10:01", "world"));

        let result = db.query("SELECT COUNT(*) as count FROM messages").unwrap();
        assert!(result.contains("2"));
    }

    #[test]
    fn test_query_rejects_insert() {
        let db = Database::new();
        let result = db.query("INSERT INTO messages VALUES (1,2,3,'a','b','c',NULL,NULL,NULL)");
        assert!(result.is_err());
    }

    #[test]
    fn test_query_rejects_drop() {
        let db = Database::new();
        let result = db.query("SELECT * FROM messages; DROP TABLE messages");
        assert!(result.is_err());
    }

    #[test]
    fn test_member_status_changes() {
        let mut db = Database::new();
        db.member_joined(
            100,
            Some("testuser".to_string()),
            "Test".to_string(),
            "2024-01-15 10:00".to_string(),
        );

        let member = db.find_user_by_username("testuser").unwrap();
        assert_eq!(member.status, MemberStatus::Member);

        db.member_left(100);
        let member = db.find_user_by_username("testuser").unwrap();
        assert_eq!(member.status, MemberStatus::Left);

        db.member_joined(
            100,
            Some("testuser".to_string()),
            "Test".to_string(),
            "2024-01-16 10:00".to_string(),
        );
        let member = db.find_user_by_username("testuser").unwrap();
        assert_eq!(member.status, MemberStatus::Member);

        db.member_banned(100);
        let member = db.find_user_by_username("testuser").unwrap();
        assert_eq!(member.status, MemberStatus::Banned);
    }
}
