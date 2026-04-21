//! Phase 1 — cross-bot bug-alert bus.
//!
//! Atlas, Sentinel, and Nova-side watchdogs detect problems (Telegram
//! errors, heartbeat gaps, subprocess crashes, journal-detected failures)
//! and emit `BugAlert` rows into a shared SQLite table at
//! `data/shared/bug_alerts.db`. Nova reads the open alerts through the
//! `read_alerts` / `mark_triaged` MCP tools and produces a triage report
//! to the owner's Telegram chat.
//!
//! The invariant from the design doc is unchanged: **Atlas=ears,
//! Sentinel=eyes, Nova=hands.** A and S only *detect*. Nova is the only
//! actor permitted to reply to the owner or take any action on the
//! alerts. Phase 1 ships the reporting loop; actual fixes come in Phase 3+.
//!
//! # Dedup model
//!
//! Every alert carries a `fingerprint` (SHA-256 over a stable
//! category+summary+evidence-keys triple). The table has a unique index
//! on fingerprint. `INSERT ... ON CONFLICT(fingerprint) DO UPDATE` bumps
//! `count` and `last_seen_at` on repeat detections. This prevents a
//! crashloop from filling the alerts table with 10k duplicate rows, and
//! lets Nova see "this happened 47 times in the last hour" at a glance.
//!
//! # Architecture — mirror of Phase 0's JournalWriter
//!
//! The writer task pattern is identical to `JournalWriter` in [`super::journal`]:
//! mpsc-fed bounded channel, background tokio task holds its own SQLite
//! `Connection`, `try_send` + drop-and-warn on overflow. This keeps the
//! detection hot path decoupled from disk I/O and avoids mutex serialization
//! with the journal hot path (HC2-style contention).

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// How many pending alert emits the in-memory channel will hold before
/// `try_send` starts returning `Full` and the caller drops the event.
/// Sized like `JournalWriter::QUEUE_CAP` for the same reasons — generous
/// enough that a genuine burst (1000 heartbeat alerts in a reset loop)
/// doesn't drop on the floor, small enough that unbounded growth is
/// impossible.
pub const ALERTS_WRITER_QUEUE_CAP: usize = 4096;

/// Severity levels. Ordered critical > high > medium > low for sorting
/// in triage output.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

/// A detected bug / incident / anomaly that the owner should eventually
/// see in a triage report.
///
/// `evidence` is a free-form JSON blob whose shape depends on the
/// `category`. Categories we emit today:
/// - `subprocess.crash` — `{exit_code, bot, last_stderr_lines}`
/// - `heartbeat.gap` — `{bot, gap_secs, last_heartbeat_at}`
/// - `telegram.error` — `{chat_id, http_status, error}`
/// - `guardian.error` — `{path, err_code, reason}` (not denial — that's expected)
/// - `journal.error` — `{entry_type, summary, detail}` (scanned from journal)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugAlert {
    /// SHA-256 hex of `(category + stable_summary + evidence_key_set)`.
    /// Identical fingerprints update the existing row (count+=1,
    /// last_seen_at=now). See [`BugAlert::compute_fingerprint`].
    pub fingerprint: String,
    /// Which bot / watcher emitted this. E.g. `"atlas"`, `"sentinel"`,
    /// `"nova-watchdog"`, `"nova-journal-scanner"`.
    pub detected_by: String,
    pub severity: Severity,
    /// Namespaced machine-readable category. See struct-level doc for the
    /// enumerated set. Keep stable — Nova's triage prompt groups by this.
    pub category: String,
    /// Human one-line description.
    pub summary: String,
    pub evidence: serde_json::Value,
}

impl BugAlert {
    /// Stable fingerprint over `category` + `summary` + sorted evidence
    /// *keys* (not values). Keys-only so two alerts describing the same
    /// problem but with slightly different values (e.g. different error
    /// message bodies) still dedup.
    pub fn compute_fingerprint(
        category: &str,
        summary: &str,
        evidence: &serde_json::Value,
    ) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(category.as_bytes());
        hasher.update(b"\0");
        hasher.update(summary.as_bytes());
        hasher.update(b"\0");
        if let Some(obj) = evidence.as_object() {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            for k in keys {
                hasher.update(k.as_bytes());
                hasher.update(b",");
            }
        }
        hex::encode(hasher.finalize())
    }

    /// Build an alert with an auto-computed fingerprint.
    pub fn new(
        detected_by: impl Into<String>,
        severity: Severity,
        category: impl Into<String>,
        summary: impl Into<String>,
        evidence: serde_json::Value,
    ) -> Self {
        let category = category.into();
        let summary = summary.into();
        let fingerprint = Self::compute_fingerprint(&category, &summary, &evidence);
        Self {
            fingerprint,
            detected_by: detected_by.into(),
            severity,
            category,
            summary,
            evidence,
        }
    }
}

/// A single row as stored on disk, including the auto-assigned id,
/// timestamps, count, and triage state. Returned by [`query_open_alerts`]
/// and friends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugAlertRow {
    pub id: i64,
    pub fingerprint: String,
    pub detected_by: String,
    pub severity: Severity,
    pub category: String,
    pub summary: String,
    pub evidence: serde_json::Value,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub count: i64,
    pub triaged_at: Option<String>,
    pub triage_note: Option<String>,
}

/// Idempotently create the `bug_alerts` table + indexes on a given
/// connection. Safe to call on boot every time.
pub fn init_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS bug_alerts (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            fingerprint     TEXT NOT NULL UNIQUE,
            detected_by     TEXT NOT NULL,
            severity        TEXT NOT NULL,
            category        TEXT NOT NULL,
            summary         TEXT NOT NULL,
            evidence        TEXT NOT NULL DEFAULT '{}',
            first_seen_at   TEXT NOT NULL DEFAULT (datetime('now')),
            last_seen_at    TEXT NOT NULL DEFAULT (datetime('now')),
            count           INTEGER NOT NULL DEFAULT 1,
            triaged_at      TEXT,
            triage_note     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_alerts_open
            ON bug_alerts(triaged_at)
            WHERE triaged_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_alerts_category
            ON bug_alerts(category);
        CREATE INDEX IF NOT EXISTS idx_alerts_last_seen
            ON bug_alerts(last_seen_at);
        "#,
    )?;
    Ok(())
}

/// Upsert an alert. Fresh fingerprints INSERT a new row; repeats UPDATE
/// the existing row with `count = count + 1` and `last_seen_at = now()`.
/// Retriggering an already-triaged alert **clears triaged_at** so Nova
/// sees it again — a regression is worth re-surfacing.
pub fn upsert_alert(
    conn: &rusqlite::Connection,
    alert: &BugAlert,
) -> rusqlite::Result<UpsertResult> {
    let evidence_json =
        serde_json::to_string(&alert.evidence).unwrap_or_else(|_| "{}".to_string());
    // ON CONFLICT path bumps count+last_seen and clears triaged state.
    // RETURNING lets us tell the caller whether this was a new alert or
    // a dedup, which feeds the "first occurrence vs repeat" behavior in
    // Nova's triage logic.
    let result: (i64, i64, String) = conn.query_row(
        r#"
        INSERT INTO bug_alerts
            (fingerprint, detected_by, severity, category, summary, evidence)
        VALUES
            (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(fingerprint) DO UPDATE SET
            count = count + 1,
            last_seen_at = datetime('now'),
            triaged_at = NULL,
            triage_note = NULL
        RETURNING id, count, first_seen_at
        "#,
        rusqlite::params![
            alert.fingerprint,
            alert.detected_by,
            alert.severity.as_str(),
            alert.category,
            alert.summary,
            evidence_json,
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(UpsertResult {
        id: result.0,
        count: result.1,
        first_seen_at: result.2,
        is_first_occurrence: result.1 == 1,
    })
}

#[derive(Debug, Clone)]
pub struct UpsertResult {
    pub id: i64,
    pub count: i64,
    pub first_seen_at: String,
    /// True iff this was the first time we saw this fingerprint.
    pub is_first_occurrence: bool,
}

/// Read all open (non-triaged) alerts, optionally filtered. Ordered by
/// severity (critical first) then `last_seen_at` desc. Intended for Nova's
/// `read_alerts` MCP tool.
pub fn query_open_alerts(
    conn: &rusqlite::Connection,
    since: Option<&str>,
    category: Option<&str>,
    limit: Option<i64>,
) -> rusqlite::Result<Vec<BugAlertRow>> {
    // Severity ordering is stable in ORDER BY via a CASE expression —
    // SQLite doesn't have enum ordering. Keeping this in the query so the
    // MCP tool doesn't have to sort twice.
    let mut sql = String::from(
        r#"
        SELECT id, fingerprint, detected_by, severity, category, summary, evidence,
               first_seen_at, last_seen_at, count, triaged_at, triage_note
        FROM bug_alerts
        WHERE triaged_at IS NULL
        "#,
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(since) = since {
        sql.push_str(" AND last_seen_at >= ?");
        params.push(since.to_string().into());
    }
    if let Some(category) = category {
        sql.push_str(" AND category = ?");
        params.push(category.to_string().into());
    }
    sql.push_str(
        r#"
        ORDER BY
            CASE severity
                WHEN 'critical' THEN 0
                WHEN 'high'     THEN 1
                WHEN 'medium'   THEN 2
                WHEN 'low'      THEN 3
                ELSE 4
            END,
            last_seen_at DESC
        "#,
    );
    if let Some(limit) = limit {
        sql.push_str(&format!(" LIMIT {}", limit));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(BugAlertRow {
                id: row.get(0)?,
                fingerprint: row.get(1)?,
                detected_by: row.get(2)?,
                severity: parse_severity(&row.get::<_, String>(3)?),
                category: row.get(4)?,
                summary: row.get(5)?,
                evidence: serde_json::from_str(&row.get::<_, String>(6)?)
                    .unwrap_or(serde_json::Value::Null),
                first_seen_at: row.get(7)?,
                last_seen_at: row.get(8)?,
                count: row.get(9)?,
                triaged_at: row.get(10)?,
                triage_note: row.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn parse_severity(s: &str) -> Severity {
    match s {
        "critical" => Severity::Critical,
        "high" => Severity::High,
        "medium" => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Mark a batch of alerts as triaged. Returns the number of rows updated.
/// `note` is optional free-form text stored on the alert rows so the next
/// post-mortem reader knows why Nova dismissed them.
pub fn mark_triaged(
    conn: &rusqlite::Connection,
    ids: &[i64],
    note: Option<&str>,
) -> rusqlite::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        r#"UPDATE bug_alerts
           SET triaged_at = datetime('now'), triage_note = ?
           WHERE id IN ({}) AND triaged_at IS NULL"#,
        placeholders
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    params.push(note.unwrap_or("").to_string().into());
    for id in ids {
        params.push((*id).into());
    }
    let updated = conn.execute(&sql, rusqlite::params_from_iter(params.iter()))?;
    Ok(updated)
}

// -------------------- async writer task --------------------

/// Background writer that owns its own SQLite connection and drains alerts
/// from an in-memory mpsc channel. Mirrors `JournalWriter` from
/// [`super::journal`]. Use one `AlertsWriter` per process (a shared Arc
/// passed into `ChatbotConfig.alerts_writer`).
#[derive(Clone)]
pub struct AlertsWriter {
    tx: mpsc::Sender<BugAlert>,
}

impl AlertsWriter {
    /// Open the shared alerts DB at the given path, run `init_schema`,
    /// then spawn the drain task. The caller keeps the returned
    /// `AlertsWriter` and hands clones to Atlas, Sentinel, and Nova's
    /// detection code.
    pub fn spawn_with_path(path: &Path) -> anyhow::Result<Self> {
        // Ensure parent dir exists so shared/ works on first boot.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        // WAL mode so Nova's reads for triage don't block A/S's writes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        init_schema(&conn)?;
        info!(path = %path.display(), "alerts writer spawned");

        let (tx, mut rx) = mpsc::channel::<BugAlert>(ALERTS_WRITER_QUEUE_CAP);

        // `conn` gets moved into a dedicated blocking thread via
        // `spawn_blocking` per-message. This is the same pattern used by
        // `JournalWriter` and works on both single- and multi-threaded
        // tokio runtimes (unlike `block_in_place`, which requires
        // `flavor = "multi_thread"`).
        let conn = Arc::new(std::sync::Mutex::new(conn));
        tokio::spawn(async move {
            while let Some(alert) = rx.recv().await {
                let alert_clone = alert.clone();
                let conn_arc = Arc::clone(&conn);
                let conn_result = tokio::task::spawn_blocking(move || {
                    let conn = conn_arc.lock().expect("alerts conn mutex poisoned");
                    upsert_alert(&conn, &alert_clone)
                })
                .await;
                match conn_result {
                    Ok(Ok(UpsertResult {
                        id,
                        count,
                        is_first_occurrence,
                        ..
                    })) => {
                        if is_first_occurrence {
                            info!(
                                alert_id = id,
                                detected_by = %alert.detected_by,
                                severity = alert.severity.as_str(),
                                category = %alert.category,
                                summary = %alert.summary,
                                "🚨 new bug alert"
                            );
                        } else {
                            info!(
                                alert_id = id,
                                count,
                                category = %alert.category,
                                "bug alert repeat (count bumped)"
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(
                            category = %alert.category,
                            err = %e,
                            "alerts writer upsert failed (non-fatal)"
                        );
                    }
                    Err(join_err) => {
                        tracing::error!(
                            err = %join_err,
                            "alerts writer: spawn_blocking panicked"
                        );
                    }
                }
            }
            info!("alerts writer channel closed — writer task exiting");
        });

        Ok(Self { tx })
    }

    /// Best-effort emit. Never blocks the caller. On queue overflow the
    /// event is dropped and a warn! is logged — matching the
    /// JournalWriter contract exactly.
    pub fn emit(&self, alert: BugAlert) {
        match self.tx.try_send(alert) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(dropped)) => {
                warn!(
                    category = %dropped.category,
                    summary = %dropped.summary,
                    cap = ALERTS_WRITER_QUEUE_CAP,
                    "alerts writer queue full — dropping alert"
                );
            }
            Err(mpsc::error::TrySendError::Closed(dropped)) => {
                warn!(
                    category = %dropped.category,
                    "alerts writer channel closed — caller is emitting after shutdown"
                );
            }
        }
    }
}

/// Shared-state path derivation. Given a bot's data_dir (e.g.
/// `/Users/ava/trio-local/data/nova`), returns the shared alerts DB path
/// (e.g. `/Users/ava/trio-local/data/shared/bug_alerts.db`). Matches the
/// bot_messages.db path convention.
pub fn shared_alerts_db_path(bot_data_dir: &Path) -> std::path::PathBuf {
    bot_data_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("shared")
        .join("bug_alerts.db")
}

// Convenience wrapper on `Option<Arc<AlertsWriter>>` so callers with a
// config field can `.emit(...)` without unwrapping.
pub trait MaybeEmit {
    fn emit(&self, alert: BugAlert);
}

impl MaybeEmit for Option<Arc<AlertsWriter>> {
    fn emit(&self, alert: BugAlert) {
        if let Some(w) = self {
            w.emit(alert);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_inmem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn fingerprint_stable_across_evidence_values() {
        // Different *values* but same keys → same fingerprint.
        let a = BugAlert::compute_fingerprint(
            "subprocess.crash",
            "nova crashed",
            &json!({"exit": 137, "bot": "nova"}),
        );
        let b = BugAlert::compute_fingerprint(
            "subprocess.crash",
            "nova crashed",
            &json!({"exit": 1, "bot": "nova"}),
        );
        assert_eq!(a, b, "value drift must not change fingerprint");
    }

    #[test]
    fn fingerprint_differs_for_different_summary() {
        let a = BugAlert::compute_fingerprint("x", "A", &json!({"k": 1}));
        let b = BugAlert::compute_fingerprint("x", "B", &json!({"k": 1}));
        assert_ne!(a, b);
    }

    #[test]
    fn upsert_first_time_returns_count_1() {
        let conn = open_inmem();
        let alert = BugAlert::new(
            "atlas",
            Severity::High,
            "telegram.error",
            "sendMessage 429",
            json!({"chat_id": -12345, "http_status": 429}),
        );
        let r = upsert_alert(&conn, &alert).unwrap();
        assert_eq!(r.count, 1);
        assert!(r.is_first_occurrence);
    }

    #[test]
    fn upsert_dedup_bumps_count_and_keeps_first_seen() {
        let conn = open_inmem();
        let alert = BugAlert::new(
            "atlas",
            Severity::High,
            "telegram.error",
            "sendMessage 429",
            json!({"chat_id": -12345}),
        );
        let r1 = upsert_alert(&conn, &alert).unwrap();
        let r2 = upsert_alert(&conn, &alert).unwrap();
        let r3 = upsert_alert(&conn, &alert).unwrap();
        assert_eq!(r1.count, 1);
        assert_eq!(r2.count, 2);
        assert_eq!(r3.count, 3);
        assert_eq!(r1.id, r2.id, "dedup should not create new row");
        assert_eq!(r2.first_seen_at, r3.first_seen_at, "first_seen must be stable");
        assert!(!r2.is_first_occurrence);
    }

    #[test]
    fn upsert_retriggers_triaged_alert() {
        let conn = open_inmem();
        let alert = BugAlert::new(
            "sentinel",
            Severity::Critical,
            "subprocess.crash",
            "nova claude_code died",
            json!({"exit_code": 137}),
        );
        let r1 = upsert_alert(&conn, &alert).unwrap();
        assert_eq!(mark_triaged(&conn, &[r1.id], Some("investigated")).unwrap(), 1);
        // A retrigger should clear the triaged flag — regressions re-surface.
        let _r2 = upsert_alert(&conn, &alert).unwrap();
        let open = query_open_alerts(&conn, None, None, None).unwrap();
        assert_eq!(open.len(), 1, "retrigger must bring the alert back to open");
        assert!(open[0].triaged_at.is_none());
        assert!(open[0].triage_note.is_none());
    }

    #[test]
    fn query_open_orders_by_severity_then_recency() {
        let conn = open_inmem();
        upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::Low, "x", "low-1", json!({})),
        )
        .unwrap();
        upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::Critical, "y", "crit-1", json!({})),
        )
        .unwrap();
        upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::High, "z", "high-1", json!({})),
        )
        .unwrap();
        let rows = query_open_alerts(&conn, None, None, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].severity, Severity::Critical);
        assert_eq!(rows[1].severity, Severity::High);
        assert_eq!(rows[2].severity, Severity::Low);
    }

    #[test]
    fn query_open_filters_out_triaged() {
        let conn = open_inmem();
        let a1 = upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::Low, "x", "keep-open", json!({})),
        )
        .unwrap();
        let a2 = upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::Low, "y", "will-triage", json!({})),
        )
        .unwrap();
        mark_triaged(&conn, &[a2.id], None).unwrap();
        let rows = query_open_alerts(&conn, None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, a1.id);
    }

    #[test]
    fn query_open_respects_category_filter() {
        let conn = open_inmem();
        upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::High, "telegram.error", "a", json!({})),
        )
        .unwrap();
        upsert_alert(
            &conn,
            &BugAlert::new("sentinel", Severity::High, "subprocess.crash", "b", json!({})),
        )
        .unwrap();
        let rows =
            query_open_alerts(&conn, None, Some("telegram.error"), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].category, "telegram.error");
    }

    #[test]
    fn mark_triaged_is_idempotent_on_already_triaged() {
        let conn = open_inmem();
        let a = upsert_alert(
            &conn,
            &BugAlert::new("atlas", Severity::Low, "x", "s", json!({})),
        )
        .unwrap();
        assert_eq!(mark_triaged(&conn, &[a.id], Some("first")).unwrap(), 1);
        // Second triage attempt must not double-update.
        assert_eq!(mark_triaged(&conn, &[a.id], Some("second")).unwrap(), 0);
    }

    #[test]
    fn shared_path_derivation_matches_bot_messages_convention() {
        let bot_data_dir = Path::new("/foo/trio-local/data/nova");
        let alerts = shared_alerts_db_path(bot_data_dir);
        assert_eq!(
            alerts,
            Path::new("/foo/trio-local/data/shared/bug_alerts.db")
        );
    }

    #[tokio::test]
    async fn writer_drains_events_to_db() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("alerts.db");
        let writer = AlertsWriter::spawn_with_path(&db).unwrap();

        writer.emit(BugAlert::new(
            "atlas",
            Severity::High,
            "telegram.error",
            "429",
            json!({"chat_id": -1}),
        ));
        writer.emit(BugAlert::new(
            "atlas",
            Severity::High,
            "telegram.error",
            "429",
            json!({"chat_id": -1}),
        ));
        writer.emit(BugAlert::new(
            "sentinel",
            Severity::Critical,
            "subprocess.crash",
            "nova died",
            json!({"exit_code": 137}),
        ));

        // Writer runs in a tokio task — give it a few ms.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let conn = rusqlite::Connection::open(&db).unwrap();
        let rows = query_open_alerts(&conn, None, None, None).unwrap();
        assert_eq!(rows.len(), 2, "dedup should collapse the two 429s");
        let tg = rows.iter().find(|r| r.category == "telegram.error").unwrap();
        assert_eq!(tg.count, 2);
        let crash = rows
            .iter()
            .find(|r| r.category == "subprocess.crash")
            .unwrap();
        assert_eq!(crash.count, 1);
        assert_eq!(crash.severity, Severity::Critical);
    }
}
