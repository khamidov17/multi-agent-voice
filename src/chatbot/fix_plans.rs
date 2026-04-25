//! Phase 2 — Nova's fix-plan drafting surface.
//!
//! After Phase 1's detectors fire and Nova triages an alert, Phase 2
//! gives Nova a structured way to **propose a fix** without writing
//! any code yet. She drafts a markdown plan describing:
//! - the root cause she believes she understands
//! - concrete steps she would take
//! - risk / blast radius
//! - how she would verify it worked
//!
//! The plan gets stored in `data/shared/fix_plans.db`, linked to the
//! originating `bug_alert_id`, and can be sent to the owner for
//! approval. Approval in Phase 2 is just a status transition; Phase 3
//! is where Nova actually opens a PR implementing an approved plan.
//!
//! This keeps the design-doc invariant: **no code written by Nova in
//! Phase 0/1/2.** Plans only.
//!
//! # Dedup model
//!
//! Unlike alerts, fix plans are *draft artifacts* — multiple plans for
//! the same alert are legitimate (Nova iterates). So the table does
//! NOT have a unique index on `alert_id`. Instead we enforce: at most
//! **one non-terminal plan per alert_id at a time**. Terminal statuses
//! (`approved`, `rejected`, `obsolete`, `implemented`) don't block new
//! drafts. Non-terminal statuses (`draft`, `sent`) do — the existing
//! plan must move to a terminal state before a new one is accepted.
//! Enforced in [`draft_plan`] rather than via SQL partial unique
//! indexes (SQLite's partial-unique support is fragile across
//! versions).

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Channel capacity. Same rationale as `AlertsWriter::QUEUE_CAP`.
pub const FIX_PLANS_WRITER_QUEUE_CAP: usize = 1024;

/// Lifecycle states a fix plan can be in. Flow:
///
/// ```text
///   draft ──(send_fix_plan_to_owner)──▶ sent
///   sent  ──(owner approves)──▶ approved
///   sent  ──(owner rejects)──▶ rejected
///   draft/sent ──(Nova cancels)──▶ obsolete
///   approved ──(Phase 3 ships PR)──▶ implemented
/// ```
///
/// Terminal states: `approved` (ready for Phase 3), `rejected`,
/// `obsolete`, `implemented`. A non-terminal plan blocks new drafts
/// against the same alert_id.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum FixPlanStatus {
    Draft,
    Sent,
    Approved,
    Rejected,
    Obsolete,
    Implemented,
}

impl FixPlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FixPlanStatus::Draft => "draft",
            FixPlanStatus::Sent => "sent",
            FixPlanStatus::Approved => "approved",
            FixPlanStatus::Rejected => "rejected",
            FixPlanStatus::Obsolete => "obsolete",
            FixPlanStatus::Implemented => "implemented",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "sent" => Some(Self::Sent),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "obsolete" => Some(Self::Obsolete),
            "implemented" => Some(Self::Implemented),
            _ => None,
        }
    }

    /// Plans in a terminal state don't block new drafts for the same
    /// alert.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Approved | Self::Rejected | Self::Obsolete | Self::Implemented
        )
    }
}

/// A draft fix plan, as produced by Nova. All fields are free-form
/// markdown except `alert_id`, which links back to the Phase 1 alert
/// this plan is addressing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixPlan {
    pub alert_id: i64,
    pub title: String,
    /// What Nova believes caused the alert. One paragraph.
    pub root_cause: String,
    /// The concrete change Nova would make. Markdown bullet list.
    pub steps: String,
    /// What could break. Short.
    pub risk: String,
    /// How to verify the fix worked.
    pub test_plan: String,
}

/// A row as stored, including auto-assigned id, status, timestamps,
/// and the owner's optional decision note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixPlanRow {
    pub id: i64,
    pub alert_id: i64,
    pub title: String,
    pub root_cause: String,
    pub steps: String,
    pub risk: String,
    pub test_plan: String,
    pub status: FixPlanStatus,
    pub created_at: String,
    pub updated_at: String,
    pub decision_note: Option<String>,
}

/// Idempotent schema init. Safe to call on every boot.
pub fn init_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS fix_plans (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            alert_id        INTEGER NOT NULL,
            title           TEXT NOT NULL,
            root_cause      TEXT NOT NULL,
            steps           TEXT NOT NULL,
            risk            TEXT NOT NULL,
            test_plan       TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'draft',
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            decision_note   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_fix_plans_alert
            ON fix_plans(alert_id);
        CREATE INDEX IF NOT EXISTS idx_fix_plans_status
            ON fix_plans(status);
        "#,
    )?;
    Ok(())
}

/// Attempt to insert a new fix plan. Returns the row id on success.
///
/// Fails with [`DraftError::NonTerminalExists`] if there is already a
/// plan for this `alert_id` in a non-terminal status (`draft`/`sent`):
/// Nova must resolve or mark obsolete the existing plan first. This
/// prevents accidental plan spam.
pub fn draft_plan(conn: &rusqlite::Connection, plan: &FixPlan) -> Result<FixPlanRow, DraftError> {
    // Guard: must not have an open (non-terminal) plan already for this
    // alert_id. Look up via a tiny query to avoid a race between caller
    // and writer.
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, status FROM fix_plans
             WHERE alert_id = ?1
             ORDER BY id DESC LIMIT 1",
            [plan.alert_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    if let Some((existing_id, status_str)) = existing
        && let Some(s) = FixPlanStatus::parse(&status_str)
        && !s.is_terminal()
    {
        return Err(DraftError::NonTerminalExists {
            existing_id,
            existing_status: status_str,
        });
    }

    conn.execute(
        r#"
        INSERT INTO fix_plans
            (alert_id, title, root_cause, steps, risk, test_plan)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6)
        "#,
        rusqlite::params![
            plan.alert_id,
            plan.title,
            plan.root_cause,
            plan.steps,
            plan.risk,
            plan.test_plan,
        ],
    )
    .map_err(DraftError::Sqlite)?;
    let id = conn.last_insert_rowid();
    get_plan(conn, id).ok_or(DraftError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
}

#[derive(Debug)]
pub enum DraftError {
    NonTerminalExists {
        existing_id: i64,
        existing_status: String,
    },
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for DraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DraftError::NonTerminalExists {
                existing_id,
                existing_status,
            } => write!(
                f,
                "cannot draft a new plan: plan #{} is still {}. Mark it \
                 approved/rejected/obsolete first.",
                existing_id, existing_status
            ),
            DraftError::Sqlite(e) => write!(f, "sqlite error: {}", e),
        }
    }
}

impl std::error::Error for DraftError {}

pub fn get_plan(conn: &rusqlite::Connection, id: i64) -> Option<FixPlanRow> {
    conn.query_row(
        r#"SELECT id, alert_id, title, root_cause, steps, risk, test_plan,
                  status, created_at, updated_at, decision_note
           FROM fix_plans WHERE id = ?1"#,
        [id],
        |row| {
            Ok(FixPlanRow {
                id: row.get(0)?,
                alert_id: row.get(1)?,
                title: row.get(2)?,
                root_cause: row.get(3)?,
                steps: row.get(4)?,
                risk: row.get(5)?,
                test_plan: row.get(6)?,
                status: FixPlanStatus::parse(&row.get::<_, String>(7)?)
                    .unwrap_or(FixPlanStatus::Obsolete),
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
                decision_note: row.get(10)?,
            })
        },
    )
    .ok()
}

/// List plans, optionally filtered by status. Returns newest first.
pub fn list_plans(
    conn: &rusqlite::Connection,
    status: Option<FixPlanStatus>,
    limit: Option<i64>,
) -> rusqlite::Result<Vec<FixPlanRow>> {
    let mut sql = String::from(
        r#"SELECT id, alert_id, title, root_cause, steps, risk, test_plan,
                  status, created_at, updated_at, decision_note
           FROM fix_plans"#,
    );
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(s) = status {
        sql.push_str(" WHERE status = ?");
        args.push(s.as_str().to_string().into());
    }
    sql.push_str(" ORDER BY id DESC");
    if let Some(n) = limit {
        sql.push_str(&format!(" LIMIT {}", n.clamp(1, 500)));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), |row| {
            Ok(FixPlanRow {
                id: row.get(0)?,
                alert_id: row.get(1)?,
                title: row.get(2)?,
                root_cause: row.get(3)?,
                steps: row.get(4)?,
                risk: row.get(5)?,
                test_plan: row.get(6)?,
                status: FixPlanStatus::parse(&row.get::<_, String>(7)?)
                    .unwrap_or(FixPlanStatus::Obsolete),
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
                decision_note: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Atomically move a plan to a new status, writing an optional decision
/// note. Returns the updated row. Fails if the plan doesn't exist OR
/// if the requested transition is invalid for the current status.
pub fn update_status(
    conn: &rusqlite::Connection,
    id: i64,
    new_status: FixPlanStatus,
    note: Option<&str>,
) -> Result<FixPlanRow, UpdateStatusError> {
    let current = get_plan(conn, id).ok_or(UpdateStatusError::NotFound(id))?;
    if !is_valid_transition(current.status, new_status) {
        return Err(UpdateStatusError::InvalidTransition {
            from: current.status.as_str().to_string(),
            to: new_status.as_str().to_string(),
        });
    }
    conn.execute(
        r#"UPDATE fix_plans
           SET status = ?1, decision_note = ?2, updated_at = datetime('now')
           WHERE id = ?3"#,
        rusqlite::params![new_status.as_str(), note, id],
    )
    .map_err(UpdateStatusError::Sqlite)?;
    get_plan(conn, id).ok_or(UpdateStatusError::NotFound(id))
}

/// Return true if `from -> to` is a permitted state transition.
/// Terminal states are sinks (no transitions out), except `approved →
/// implemented` so Phase 3's PR shipper can close the loop.
pub fn is_valid_transition(from: FixPlanStatus, to: FixPlanStatus) -> bool {
    use FixPlanStatus::*;
    matches!(
        (from, to),
        (Draft, Sent)
            | (Draft, Obsolete)
            | (Sent, Approved)
            | (Sent, Rejected)
            | (Sent, Obsolete)
            | (Approved, Implemented)
            | (Approved, Obsolete)
    )
}

#[derive(Debug)]
pub enum UpdateStatusError {
    NotFound(i64),
    InvalidTransition { from: String, to: String },
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for UpdateStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "fix plan #{} not found", id),
            Self::InvalidTransition { from, to } => write!(
                f,
                "invalid fix-plan transition: {} → {}. See \
                 `is_valid_transition` for the allowed edges.",
                from, to
            ),
            Self::Sqlite(e) => write!(f, "sqlite error: {}", e),
        }
    }
}

impl std::error::Error for UpdateStatusError {}

/// Shared-state path derivation. Given a bot's data_dir, returns the
/// shared fix-plans DB path.
pub fn shared_fix_plans_db_path(bot_data_dir: &Path) -> std::path::PathBuf {
    bot_data_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("shared")
        .join("fix_plans.db")
}

// -------------------- async writer task --------------------

/// Mpsc-fed writer task that owns its own SQLite connection. Mirrors
/// `AlertsWriter`. Intended for the `draft_fix_plan` hot path where a
/// Nova cognitive-loop turn drafts a plan without blocking on disk.
#[derive(Clone)]
pub struct FixPlansWriter {
    tx: mpsc::Sender<WriterMsg>,
}

impl std::fmt::Debug for FixPlansWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixPlansWriter")
            .field("capacity_remaining", &self.tx.capacity())
            .finish()
    }
}

enum WriterMsg {
    Draft(FixPlan),
    SetStatus {
        id: i64,
        status: FixPlanStatus,
        note: Option<String>,
    },
}

impl FixPlansWriter {
    pub fn spawn_with_path(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        init_schema(&conn)?;
        info!(path = %path.display(), "fix_plans writer spawned");

        let (tx, mut rx) = mpsc::channel::<WriterMsg>(FIX_PLANS_WRITER_QUEUE_CAP);
        let conn = Arc::new(std::sync::Mutex::new(conn));
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let conn_arc = Arc::clone(&conn);
                let handled = tokio::task::spawn_blocking(move || {
                    let conn = conn_arc.lock().expect("fix_plans conn poisoned");
                    match msg {
                        WriterMsg::Draft(p) => {
                            let alert_id = p.alert_id;
                            let r = draft_plan(&conn, &p);
                            (
                                Op::Draft { alert_id },
                                r.map(|row| row.id).map_err(|e| e.to_string()),
                            )
                        }
                        WriterMsg::SetStatus { id, status, note } => {
                            let r = update_status(&conn, id, status, note.as_deref());
                            (
                                Op::SetStatus { id, status },
                                r.map(|row| row.id).map_err(|e| e.to_string()),
                            )
                        }
                    }
                })
                .await;
                match handled {
                    Ok((op, Ok(id))) => {
                        info!(?op, plan_id = id, "fix_plans writer committed");
                    }
                    Ok((op, Err(e))) => {
                        warn!(?op, err = %e, "fix_plans writer op failed");
                    }
                    Err(join_err) => {
                        tracing::error!(err = %join_err, "fix_plans writer spawn_blocking panicked");
                    }
                }
            }
            info!("fix_plans writer channel closed — task exiting");
        });

        Ok(Self { tx })
    }

    /// Queue a new plan. Best-effort, non-blocking.
    pub fn draft(&self, plan: FixPlan) {
        if let Err(e) = self.tx.try_send(WriterMsg::Draft(plan)) {
            warn!(err = %e, "fix_plans writer queue full/closed — dropping draft");
        }
    }

    pub fn set_status(&self, id: i64, status: FixPlanStatus, note: Option<String>) {
        if let Err(e) = self.tx.try_send(WriterMsg::SetStatus { id, status, note }) {
            warn!(err = %e, "fix_plans writer queue full/closed — dropping status update");
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum Op {
    Draft { alert_id: i64 },
    SetStatus { id: i64, status: FixPlanStatus },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_inmem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn mkplan(alert_id: i64, title: &str) -> FixPlan {
        FixPlan {
            alert_id,
            title: title.to_string(),
            root_cause: "guess".to_string(),
            steps: "- do a\n- do b".to_string(),
            risk: "low".to_string(),
            test_plan: "cargo test".to_string(),
        }
    }

    #[test]
    fn draft_first_plan_succeeds() {
        let conn = open_inmem();
        let r = draft_plan(&conn, &mkplan(42, "fix the thing")).unwrap();
        assert_eq!(r.alert_id, 42);
        assert_eq!(r.status, FixPlanStatus::Draft);
        assert!(r.decision_note.is_none());
    }

    #[test]
    fn draft_blocked_while_previous_nonterminal() {
        let conn = open_inmem();
        let r1 = draft_plan(&conn, &mkplan(42, "v1")).unwrap();
        let err = draft_plan(&conn, &mkplan(42, "v2")).unwrap_err();
        match err {
            DraftError::NonTerminalExists {
                existing_id,
                existing_status,
            } => {
                assert_eq!(existing_id, r1.id);
                assert_eq!(existing_status, "draft");
            }
            other => panic!("expected NonTerminalExists, got {:?}", other),
        }
    }

    #[test]
    fn draft_unblocked_after_terminal_status() {
        let conn = open_inmem();
        let r1 = draft_plan(&conn, &mkplan(42, "v1")).unwrap();
        update_status(&conn, r1.id, FixPlanStatus::Obsolete, Some("scope cut")).unwrap();
        let r2 = draft_plan(&conn, &mkplan(42, "v2"))
            .expect("terminal predecessor unblocks a new draft");
        assert_ne!(r2.id, r1.id);
    }

    #[test]
    fn transition_draft_to_sent_then_approved() {
        let conn = open_inmem();
        let r = draft_plan(&conn, &mkplan(1, "x")).unwrap();
        let r2 = update_status(&conn, r.id, FixPlanStatus::Sent, None).unwrap();
        assert_eq!(r2.status, FixPlanStatus::Sent);
        let r3 = update_status(&conn, r.id, FixPlanStatus::Approved, Some("lgtm")).unwrap();
        assert_eq!(r3.status, FixPlanStatus::Approved);
        assert_eq!(r3.decision_note.as_deref(), Some("lgtm"));
    }

    #[test]
    fn invalid_transitions_rejected() {
        let conn = open_inmem();
        let r = draft_plan(&conn, &mkplan(1, "x")).unwrap();
        // draft → approved (skipping sent) is not a valid edge; owners
        // approve only AFTER Nova sends.
        let err = update_status(&conn, r.id, FixPlanStatus::Approved, None).unwrap_err();
        matches!(err, UpdateStatusError::InvalidTransition { .. });
        // draft → implemented would bypass human review entirely. No.
        let err2 = update_status(&conn, r.id, FixPlanStatus::Implemented, None).unwrap_err();
        matches!(err2, UpdateStatusError::InvalidTransition { .. });
    }

    #[test]
    fn approved_to_implemented_is_the_only_exit_edge() {
        let conn = open_inmem();
        let r = draft_plan(&conn, &mkplan(1, "x")).unwrap();
        update_status(&conn, r.id, FixPlanStatus::Sent, None).unwrap();
        update_status(&conn, r.id, FixPlanStatus::Approved, None).unwrap();
        // approved → rejected would be a confusing walk-back; not allowed.
        assert!(update_status(&conn, r.id, FixPlanStatus::Rejected, None).is_err());
        // approved → implemented is the only forward move.
        let imp = update_status(&conn, r.id, FixPlanStatus::Implemented, None).unwrap();
        assert_eq!(imp.status, FixPlanStatus::Implemented);
    }

    #[test]
    fn list_orders_newest_first_and_filters_by_status() {
        let conn = open_inmem();
        let r1 = draft_plan(&conn, &mkplan(10, "a")).unwrap();
        update_status(&conn, r1.id, FixPlanStatus::Obsolete, None).unwrap();
        let r2 = draft_plan(&conn, &mkplan(10, "b")).unwrap();
        let r3 = draft_plan(&conn, &mkplan(11, "c")).unwrap();

        let all = list_plans(&conn, None, None).unwrap();
        assert_eq!(all.len(), 3);
        // Newest first: r3 > r2 > r1 by id.
        assert_eq!(all[0].id, r3.id);
        assert_eq!(all[1].id, r2.id);

        let drafts = list_plans(&conn, Some(FixPlanStatus::Draft), None).unwrap();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().all(|p| p.status == FixPlanStatus::Draft));

        let obs = list_plans(&conn, Some(FixPlanStatus::Obsolete), None).unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].id, r1.id);
    }

    #[test]
    fn shared_path_derivation_matches_convention() {
        let p = shared_fix_plans_db_path(Path::new("/foo/trio-local/data/nova"));
        assert_eq!(p, Path::new("/foo/trio-local/data/shared/fix_plans.db"));
    }

    #[tokio::test]
    async fn writer_drafts_and_transitions_through_tokio_task() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("fix_plans.db");
        let writer = FixPlansWriter::spawn_with_path(&db).unwrap();

        writer.draft(mkplan(5, "first"));
        // Let the writer drain.
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let conn = rusqlite::Connection::open(&db).unwrap();
            let rows = list_plans(&conn, None, None).unwrap();
            if rows.len() == 1 {
                let id = rows[0].id;
                writer.set_status(id, FixPlanStatus::Sent, Some("dm'd owner".into()));
                // Let it drain.
                for _ in 0..30 {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    let conn = rusqlite::Connection::open(&db).unwrap();
                    let r = get_plan(&conn, id).unwrap();
                    if r.status == FixPlanStatus::Sent {
                        assert_eq!(r.decision_note.as_deref(), Some("dm'd owner"));
                        return;
                    }
                }
                panic!("set_status did not land within 1.2s");
            }
        }
        panic!("draft did not land within 1.2s");
    }
}
