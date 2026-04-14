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

/// A task on the shared task board.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: String,
    pub assigned_to: Option<String>,
    pub created_by: String,
    pub context: Option<String>,
    pub result: Option<String>,
    pub plan_id: Option<String>,
    pub checkpoint_json: Option<String>,
    pub priority: i32,
    pub error_log: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
}

/// A formal delegation contract between agents.
#[derive(Debug, Clone)]
pub struct Handoff {
    pub id: i64,
    pub from_agent: String,
    pub to_agent: String,
    pub task_id: String,
    pub handoff_type: String,
    pub payload: String,
    pub status: String,
    pub result: Option<String>,
    pub created_at: String,
}

/// A consensus request requiring multi-agent approval.
#[derive(Debug, Clone)]
pub struct ConsensusRequest {
    pub id: i64,
    pub requesting_agent: String,
    pub action_type: String,
    pub description: String,
    pub required_approvers: String, // JSON array
    pub approvals: String,          // JSON array
    pub status: String,
    pub timeout_minutes: i64,
    pub created_at: String,
}

/// A single entry in the progress audit ledger.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub id: i64,
    pub task_id: String,
    pub agent: String,
    pub action: String,
    pub detail: Option<String>,
    pub consensus_id: Option<i64>,
    pub created_at: String,
}

/// Status of a consensus request.
#[derive(Debug, Clone, PartialEq)]
pub enum ConsensusStatus {
    Pending,
    Approved,
    Rejected(String), // reason
    Expired,
}

/// Thin wrapper around a `rusqlite::Connection` to the shared bus database.
pub struct BotMessageDb {
    pub(crate) conn: Connection,
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
                depends_on       TEXT,
                plan_id          TEXT,
                checkpoint_json  TEXT,
                priority         INTEGER NOT NULL DEFAULT 0,
                started_at       TEXT,
                completed_at     TEXT,
                error_log        TEXT
            );

            -- Structured plans for complex tasks
            CREATE TABLE IF NOT EXISTS plans (
                id               TEXT    PRIMARY KEY,
                task_id          TEXT    NOT NULL,
                steps_json       TEXT    NOT NULL,
                current_step     INTEGER NOT NULL DEFAULT 0,
                status           TEXT    NOT NULL DEFAULT 'planning',
                iteration        INTEGER NOT NULL DEFAULT 0,
                max_iterations   INTEGER NOT NULL DEFAULT 3,
                created_at       TEXT    NOT NULL DEFAULT (datetime('now')),
                updated_at       TEXT    NOT NULL DEFAULT (datetime('now'))
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
                result           TEXT,
                created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_bot_messages_to
                ON bot_messages(to_bot);
            CREATE INDEX IF NOT EXISTS idx_bot_messages_created
                ON bot_messages(created_at);
            CREATE INDEX IF NOT EXISTS idx_tasks_status
                ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_handoffs_to
                ON handoffs(to_agent, status);

            -- Multi-agent consensus for risky actions
            CREATE TABLE IF NOT EXISTS consensus_requests (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                requesting_agent   TEXT    NOT NULL,
                action_type        TEXT    NOT NULL,
                description        TEXT    NOT NULL,
                required_approvers TEXT    NOT NULL,
                approvals          TEXT    NOT NULL DEFAULT '[]',
                status             TEXT    NOT NULL DEFAULT 'pending',
                timeout_minutes    INTEGER NOT NULL DEFAULT 10,
                created_at         TEXT    NOT NULL DEFAULT (datetime('now')),
                resolved_at        TEXT
            );

            -- Immutable audit trail: every proposed, approved, executed, verified step
            -- Code-enforced multi-agent workflows (Google ADK patterns)
            CREATE TABLE IF NOT EXISTS workflows (
                id                TEXT PRIMARY KEY,
                name              TEXT NOT NULL,
                steps_json        TEXT NOT NULL,
                current_step      INTEGER NOT NULL DEFAULT 0,
                status            TEXT NOT NULL DEFAULT 'running',
                max_iterations    INTEGER NOT NULL DEFAULT 5,
                current_iteration INTEGER NOT NULL DEFAULT 0,
                state_json        TEXT NOT NULL DEFAULT '{}',
                created_at        TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS progress_ledger (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id       TEXT    NOT NULL,
                agent         TEXT    NOT NULL,
                action        TEXT    NOT NULL,
                detail        TEXT,
                consensus_id  INTEGER,
                created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_ledger_task ON progress_ledger(task_id);",
        )?;

        // Migrate: add message_type column if missing (existing DBs).
        let has_message_type: bool = conn
            .prepare("SELECT message_type FROM bot_messages LIMIT 0")
            .is_ok();
        if !has_message_type
            && let Err(e) = conn.execute_batch(
                "ALTER TABLE bot_messages ADD COLUMN message_type TEXT NOT NULL DEFAULT 'chat';",
            )
        {
            warn!("Migration: message_type column may already exist: {e}");
        }

        // Now create index on message_type (safe — column exists after migration).
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_bot_messages_type ON bot_messages(message_type);",
        );

        // Migrate: add status + current_task to heartbeats (existing DBs).
        if conn
            .prepare("SELECT status FROM heartbeats LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch(
                "ALTER TABLE heartbeats ADD COLUMN status TEXT NOT NULL DEFAULT 'idle';",
            );
        }
        if conn
            .prepare("SELECT current_task FROM heartbeats LIMIT 0")
            .is_err()
        {
            let _ = conn.execute_batch("ALTER TABLE heartbeats ADD COLUMN current_task TEXT;");
        }

        // Migrate: add new task columns for existing DBs
        for col in &[
            "ALTER TABLE tasks ADD COLUMN plan_id TEXT;",
            "ALTER TABLE tasks ADD COLUMN checkpoint_json TEXT;",
            "ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;",
            "ALTER TABLE tasks ADD COLUMN started_at TEXT;",
            "ALTER TABLE tasks ADD COLUMN completed_at TEXT;",
            "ALTER TABLE tasks ADD COLUMN error_log TEXT;",
        ] {
            let _ = conn.execute_batch(col);
        }

        info!("BotMessageDb opened at {}", path.display());
        Ok(Self { conn })
    }

    // -----------------------------------------------------------------------
    // Task state machine
    // -----------------------------------------------------------------------

    /// Create a new task on the shared board.
    pub fn create_task(
        &self,
        id: &str,
        title: &str,
        created_by: &str,
        context: Option<&str>,
        priority: i32,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO tasks (id, title, created_by, context, priority)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, title, created_by, context, priority],
        )?;
        Ok(())
    }

    /// Claim a task. Returns false if already claimed by another bot.
    pub fn claim_task(&self, task_id: &str, bot_name: &str) -> anyhow::Result<bool> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let current_assignee: Option<String> = self
            .conn
            .query_row(
                "SELECT assigned_to FROM tasks WHERE id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .ok();

        if let Some(ref assignee) = current_assignee
            && !assignee.is_empty()
            && assignee != bot_name
        {
            self.conn.execute_batch("COMMIT")?;
            return Ok(false);
        }

        self.conn.execute(
            "UPDATE tasks SET assigned_to = ?1, status = 'in_progress', started_at = datetime('now'), updated_at = datetime('now') WHERE id = ?2",
            params![bot_name, task_id],
        )?;
        self.conn.execute_batch("COMMIT")?;
        Ok(true)
    }

    /// Save checkpoint state for a task (survives restarts).
    pub fn checkpoint_task(&self, task_id: &str, checkpoint_json: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE tasks SET checkpoint_json = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![checkpoint_json, task_id],
        )?;
        Ok(())
    }

    /// Mark a task as completed.
    pub fn complete_task(&self, task_id: &str, result: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE tasks SET status = 'done', result = ?1, completed_at = datetime('now'), updated_at = datetime('now') WHERE id = ?2",
            params![result, task_id],
        )?;
        Ok(())
    }

    /// Mark a task as failed and append to error log.
    pub fn fail_task(&self, task_id: &str, error: &str) -> anyhow::Result<()> {
        let existing_log: String = self
            .conn
            .query_row(
                "SELECT COALESCE(error_log, '') FROM tasks WHERE id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .unwrap_or_default();
        let new_log = if existing_log.is_empty() {
            error.to_string()
        } else {
            format!("{}\n---\n{}", existing_log, error)
        };
        self.conn.execute(
            "UPDATE tasks SET status = 'failed', error_log = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![new_log, task_id],
        )?;
        Ok(())
    }

    /// Get tasks assigned to this bot that are still in progress or blocked.
    pub fn get_incomplete_tasks(&self, bot_name: &str) -> anyhow::Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, status, assigned_to, created_by, context, result,
                    plan_id, checkpoint_json, priority, error_log, created_at, started_at
             FROM tasks
             WHERE assigned_to = ?1 AND status IN ('in_progress', 'blocked')
             ORDER BY priority DESC",
        )?;
        let rows = stmt.query_map(params![bot_name], |row| {
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                assigned_to: row.get(3)?,
                created_by: row.get(4)?,
                context: row.get(5)?,
                result: row.get(6)?,
                plan_id: row.get(7)?,
                checkpoint_json: row.get(8)?,
                priority: row.get(9)?,
                error_log: row.get(10)?,
                created_at: row.get(11)?,
                started_at: row.get(12)?,
            })
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    /// Get unclaimed tasks ordered by priority.
    pub fn get_pending_tasks_board(&self) -> anyhow::Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, status, assigned_to, created_by, context, result,
                    plan_id, checkpoint_json, priority, error_log, created_at, started_at
             FROM tasks
             WHERE status = 'pending' AND (assigned_to IS NULL OR assigned_to = '')
             ORDER BY priority DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                assigned_to: row.get(3)?,
                created_by: row.get(4)?,
                context: row.get(5)?,
                result: row.get(6)?,
                plan_id: row.get(7)?,
                checkpoint_json: row.get(8)?,
                priority: row.get(9)?,
                error_log: row.get(10)?,
                created_at: row.get(11)?,
                started_at: row.get(12)?,
            })
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    /// Get a single task by ID.
    pub fn get_task(&self, task_id: &str) -> anyhow::Result<Option<Task>> {
        let result = self.conn.query_row(
            "SELECT id, title, status, assigned_to, created_by, context, result,
                    plan_id, checkpoint_json, priority, error_log, created_at, started_at
             FROM tasks WHERE id = ?1",
            params![task_id],
            |row| {
                Ok(Task {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    status: row.get(2)?,
                    assigned_to: row.get(3)?,
                    created_by: row.get(4)?,
                    context: row.get(5)?,
                    result: row.get(6)?,
                    plan_id: row.get(7)?,
                    checkpoint_json: row.get(8)?,
                    priority: row.get(9)?,
                    error_log: row.get(10)?,
                    created_at: row.get(11)?,
                    started_at: row.get(12)?,
                })
            },
        );
        match result {
            Ok(task) => Ok(Some(task)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // -----------------------------------------------------------------------
    // Handoff delegation contracts
    // -----------------------------------------------------------------------

    /// Create a formal handoff (delegation contract) to another agent.
    pub fn create_handoff(
        &self,
        from: &str,
        to: &str,
        task_id: &str,
        handoff_type: &str,
        payload: &str,
    ) -> anyhow::Result<i64> {
        self.conn.execute(
            "INSERT INTO handoffs (from_agent, to_agent, task_id, type, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![from, to, task_id, handoff_type, payload],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Accept a handoff (only the to_agent can accept).
    pub fn accept_handoff(&self, id: i64, agent_name: &str) -> anyhow::Result<bool> {
        let to_agent: String = self
            .conn
            .query_row(
                "SELECT to_agent FROM handoffs WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .map_err(|_| anyhow::anyhow!("Handoff {} not found", id))?;

        if !to_agent.eq_ignore_ascii_case(agent_name) {
            return Ok(false);
        }

        self.conn.execute(
            "UPDATE handoffs SET status = 'accepted' WHERE id = ?1",
            params![id],
        )?;
        Ok(true)
    }

    /// Complete a handoff with result.
    pub fn complete_handoff(
        &self,
        id: i64,
        agent_name: &str,
        result_json: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE handoffs SET status = 'completed', result = ?1 WHERE id = ?2 AND to_agent = ?3",
            params![result_json, id, agent_name],
        )?;
        Ok(())
    }

    /// Reject a handoff with reason.
    pub fn reject_handoff(&self, id: i64, agent_name: &str, reason: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE handoffs SET status = 'rejected', result = ?1 WHERE id = ?2 AND to_agent = ?3",
            params![reason, id, agent_name],
        )?;
        Ok(())
    }

    /// Get pending handoffs for an agent.
    pub fn pending_handoffs_for(&self, agent_name: &str) -> anyhow::Result<Vec<Handoff>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_agent, to_agent, task_id, type, payload, status, result, created_at
             FROM handoffs WHERE to_agent = ?1 AND status = 'pending'
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![agent_name], |row| {
            Ok(Handoff {
                id: row.get(0)?,
                from_agent: row.get(1)?,
                to_agent: row.get(2)?,
                task_id: row.get(3)?,
                handoff_type: row.get(4)?,
                payload: row.get(5)?,
                status: row.get(6)?,
                result: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;
        let mut handoffs = Vec::new();
        for row in rows {
            handoffs.push(row?);
        }
        Ok(handoffs)
    }

    /// Get a single handoff by ID.
    pub fn get_handoff(&self, id: i64) -> anyhow::Result<Option<Handoff>> {
        let result = self.conn.query_row(
            "SELECT id, from_agent, to_agent, task_id, type, payload, status, result, created_at
             FROM handoffs WHERE id = ?1",
            params![id],
            |row| {
                Ok(Handoff {
                    id: row.get(0)?,
                    from_agent: row.get(1)?,
                    to_agent: row.get(2)?,
                    task_id: row.get(3)?,
                    handoff_type: row.get(4)?,
                    payload: row.get(5)?,
                    status: row.get(6)?,
                    result: row.get(7)?,
                    created_at: row.get(8)?,
                })
            },
        );
        match result {
            Ok(h) => Ok(Some(h)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // -----------------------------------------------------------------------
    // Consensus protocol
    // -----------------------------------------------------------------------

    /// Request consensus from other agents for a risky action.
    pub fn request_consensus(
        &self,
        agent: &str,
        action_type: &str,
        description: &str,
        required_approvers: &[String],
        timeout_minutes: u64,
    ) -> anyhow::Result<i64> {
        let approvers_json = serde_json::to_string(required_approvers)?;
        self.conn.execute(
            "INSERT INTO consensus_requests (requesting_agent, action_type, description, required_approvers, timeout_minutes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![agent, action_type, description, approvers_json, timeout_minutes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Vote on a consensus request. Returns the new status.
    pub fn vote_on_consensus(
        &self,
        request_id: i64,
        agent: &str,
        decision: &str,
        reason: &str,
    ) -> anyhow::Result<ConsensusStatus> {
        // Load current state
        let (approvals_str, required_str, status): (String, String, String) = self.conn.query_row(
            "SELECT approvals, required_approvers, status FROM consensus_requests WHERE id = ?1",
            params![request_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        if status != "pending" {
            return Ok(match status.as_str() {
                "approved" => ConsensusStatus::Approved,
                "rejected" => ConsensusStatus::Rejected("already rejected".into()),
                _ => ConsensusStatus::Expired,
            });
        }

        // Add this vote
        let mut approvals: Vec<serde_json::Value> =
            serde_json::from_str(&approvals_str).unwrap_or_default();
        approvals.push(serde_json::json!({
            "agent": agent,
            "decision": decision,
            "reason": reason,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }));
        let new_approvals_json = serde_json::to_string(&approvals)?;

        // Check if rejected
        if decision == "reject" {
            self.conn.execute(
                "UPDATE consensus_requests SET approvals = ?1, status = 'rejected', resolved_at = datetime('now') WHERE id = ?2",
                params![new_approvals_json, request_id],
            )?;
            return Ok(ConsensusStatus::Rejected(reason.to_string()));
        }

        // Check if all required approvers have approved
        let required: Vec<String> = serde_json::from_str(&required_str).unwrap_or_default();
        let approved_agents: Vec<String> = approvals
            .iter()
            .filter(|a| a["decision"] == "approve")
            .filter_map(|a| a["agent"].as_str().map(|s| s.to_string()))
            .collect();

        let all_approved = required
            .iter()
            .all(|req| approved_agents.iter().any(|a| a.eq_ignore_ascii_case(req)));

        if all_approved {
            self.conn.execute(
                "UPDATE consensus_requests SET approvals = ?1, status = 'approved', resolved_at = datetime('now') WHERE id = ?2",
                params![new_approvals_json, request_id],
            )?;
            Ok(ConsensusStatus::Approved)
        } else {
            self.conn.execute(
                "UPDATE consensus_requests SET approvals = ?1 WHERE id = ?2",
                params![new_approvals_json, request_id],
            )?;
            Ok(ConsensusStatus::Pending)
        }
    }

    /// Check consensus status.
    pub fn check_consensus(&self, request_id: i64) -> anyhow::Result<ConsensusStatus> {
        let (status, created_at, timeout): (String, String, i64) = self.conn.query_row(
            "SELECT status, created_at, timeout_minutes FROM consensus_requests WHERE id = ?1",
            params![request_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        if status != "pending" {
            return Ok(match status.as_str() {
                "approved" => ConsensusStatus::Approved,
                "rejected" => ConsensusStatus::Rejected("".into()),
                _ => ConsensusStatus::Expired,
            });
        }

        // Check expiry
        if let Ok(created) = chrono::NaiveDateTime::parse_from_str(&created_at, "%Y-%m-%d %H:%M:%S")
        {
            let now = chrono::Utc::now().naive_utc();
            if now.signed_duration_since(created).num_minutes() > timeout {
                self.conn.execute(
                    "UPDATE consensus_requests SET status = 'expired', resolved_at = datetime('now') WHERE id = ?1",
                    params![request_id],
                )?;
                return Ok(ConsensusStatus::Expired);
            }
        }

        Ok(ConsensusStatus::Pending)
    }

    /// Get pending consensus requests for an agent.
    pub fn pending_consensus_for(&self, agent_name: &str) -> anyhow::Result<Vec<ConsensusRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, requesting_agent, action_type, description, required_approvers, approvals, status, timeout_minutes, created_at
             FROM consensus_requests WHERE status = 'pending'
             AND required_approvers LIKE ?1
             ORDER BY id ASC",
        )?;
        let pattern = format!("%{}%", agent_name);
        let rows = stmt.query_map(params![pattern], |row| {
            Ok(ConsensusRequest {
                id: row.get(0)?,
                requesting_agent: row.get(1)?,
                action_type: row.get(2)?,
                description: row.get(3)?,
                required_approvers: row.get(4)?,
                approvals: row.get(5)?,
                status: row.get(6)?,
                timeout_minutes: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;
        let mut requests = Vec::new();
        for row in rows {
            requests.push(row?);
        }
        Ok(requests)
    }

    /// Expire stale consensus requests. Returns count expired.
    pub fn expire_stale_requests(&self) -> anyhow::Result<u64> {
        let count = self.conn.execute(
            "UPDATE consensus_requests SET status = 'expired', resolved_at = datetime('now')
             WHERE status = 'pending'
             AND datetime(created_at, '+' || timeout_minutes || ' minutes') < datetime('now')",
            [],
        )?;
        Ok(count as u64)
    }

    // -----------------------------------------------------------------------
    // Progress ledger
    // ── Health-aware routing ────────────────────────────────────────────

    /// Check if a bot is alive (heartbeat within last `max_stale_secs`).
    pub fn is_bot_alive(&self, bot_name: &str, max_stale_secs: u64) -> bool {
        let result: rusqlite::Result<String> = self.conn.query_row(
            "SELECT last_heartbeat FROM heartbeats WHERE bot_name = ?1",
            params![bot_name],
            |r| r.get(0),
        );
        match result {
            Ok(ts) => crate::chatbot::health::parse_sqlite_datetime_age_secs(&ts)
                .map(|age| age <= max_stale_secs)
                .unwrap_or(false),
            Err(_) => false, // no heartbeat entry = not alive
        }
    }

    /// Get workflows that are paused waiting for a specific agent.
    pub fn get_paused_workflows_for(
        &self,
        agent: &str,
    ) -> anyhow::Result<Vec<crate::chatbot::workflow::Workflow>> {
        // Load all paused workflows, then filter by current step's agent
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM workflows WHERE status IN ('paused', '\"paused\"')")?;
        let ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut result = Vec::new();
        for id in &ids {
            if let Ok(wf) = crate::chatbot::workflow::load_workflow(&self.conn, id)
                && wf.current_step < wf.steps.len()
                && wf.steps[wf.current_step].agent == agent
            {
                result.push(wf);
            }
        }
        Ok(result)
    }

    // -----------------------------------------------------------------------

    /// Append an entry to the immutable progress audit ledger.
    pub fn log_progress(
        &self,
        task_id: &str,
        agent: &str,
        action: &str,
        detail: Option<&str>,
        consensus_id: Option<i64>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO progress_ledger (task_id, agent, action, detail, consensus_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task_id, agent, action, detail, consensus_id],
        )?;
        Ok(())
    }

    /// Retrieve all ledger entries for a task, ordered chronologically.
    pub fn get_task_progress(&self, task_id: &str) -> anyhow::Result<Vec<LedgerEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent, action, detail, consensus_id, created_at \
             FROM progress_ledger WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id], |row| {
            Ok(LedgerEntry {
                id: row.get(0)?,
                task_id: row.get(1)?,
                agent: row.get(2)?,
                action: row.get(3)?,
                detail: row.get(4)?,
                consensus_id: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
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
        // Cross-process notification: touch a wake file so the target bot's
        // polling loop wakes its sleeping CC turn via the event bus.
        if let Some(target) = to_bot {
            crate::chatbot::event_bus::touch_wake_file(target);
        }
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
                status: row
                    .get::<_, String>(3)
                    .unwrap_or_else(|_| "idle".to_string()),
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
        let already = current
            .split(',')
            .map(str::trim)
            .any(|n| n.eq_ignore_ascii_case(this_bot));
        if already {
            self.conn.execute_batch("COMMIT")?;
            return Ok(());
        }

        let updated = if current.trim().is_empty() {
            this_bot.to_string()
        } else {
            format!("{},{}", current, this_bot)
        };

        if let Err(e) = self.conn.execute(
            "UPDATE bot_messages SET read_by = ?1 WHERE id = ?2",
            params![updated, id],
        ) {
            let _ = self.conn.execute_batch("ROLLBACK");
            return Err(e.into());
        }

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
        let mut poll_count: u64 = 0;

        loop {
            interval.tick().await;
            poll_count += 1;

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
            // Wake the event bus so any sleeping CC turn exits immediately.
            crate::chatbot::event_bus::global_event_bus().wake(&this_bot);

            // Every ~30s (60 ticks at 500ms), check for paused workflows that
            // were waiting for this bot to come back online.
            if poll_count.is_multiple_of(60) {
                match db.get_paused_workflows_for(&this_bot) {
                    Ok(paused) => {
                        for wf in &paused {
                            info!(
                                "Auto-resuming workflow {} — {} is back online",
                                wf.id, this_bot
                            );
                            match crate::chatbot::workflow::advance_workflow(
                                &db.conn,
                                &wf.id,
                                "agent_recovered",
                                true,
                                None,
                            ) {
                                Ok(crate::chatbot::workflow::AdvanceResult::NextStep {
                                    ref agent,
                                    ref message,
                                }) => {
                                    let _ = db.insert_typed(
                                        &this_bot,
                                        Some(agent),
                                        message,
                                        message_type::CHAT,
                                        None,
                                        None,
                                    );
                                }
                                Ok(_) => {} // Completed or max iterations
                                Err(e) => {
                                    warn!("Failed to auto-resume workflow {}: {e}", wf.id);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to check paused workflows: {e}");
                    }
                }
            }
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
