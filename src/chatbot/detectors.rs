//! Phase 1 detectors — background tasks that watch runtime state and
//! emit `BugAlert` rows to the shared alerts DB.
//!
//! This is the "A/S → N" plumbing from the design doc, but the detectors
//! live inside each bot's own process rather than Atlas/Sentinel
//! impersonating Nova's watchdog. Reason: the heartbeat atomic lives
//! in-memory in Nova's process, and the journal lives in Nova's SQLite
//! file. Detectors run alongside the engine, share the atomics/paths,
//! and write to the SHARED alerts file so other bots (and Nova herself
//! later) can read them for triage.
//!
//! # Detectors shipped in this slice
//!
//! 1. **Heartbeat-gap watchdog.** Polls the Claude Code subprocess
//!    heartbeat atomic every 5s; if the gap exceeds `gap_threshold_secs`
//!    (default 30s per design doc line 163), emits
//!    `category=heartbeat.gap severity=high`.
//! 2. **Journal error scanner.** Polls the per-bot journal every 30s
//!    for entries whose `entry_type` ends in `.error` or whose summary
//!    contains "failed"/"panicked". Emits `category=journal.error` with
//!    the entry's summary + detail as evidence.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tracing::{debug, info, warn};

use super::alerts::{AlertsWriter, BugAlert, MaybeEmit, Severity};

/// Default threshold above which a heartbeat gap is surfaced as an alert.
/// Matches the design doc's "heartbeat gap > 30s" failure definition
/// (line 163). Tune with [`spawn_heartbeat_watchdog`]'s `gap_threshold_secs`
/// argument if a particular deployment needs tighter/looser bounds.
pub const DEFAULT_HEARTBEAT_GAP_THRESHOLD_SECS: u64 = 30;

/// Default poll interval for the heartbeat watchdog. 5s gives the
/// watchdog ~6 chances to notice a 30s gap before it doubles, which is
/// plenty without spamming SQLite upserts on the rate-limited path.
pub const DEFAULT_HEARTBEAT_POLL_INTERVAL_SECS: u64 = 5;

/// Default poll interval for the journal scanner. 30s matches the
/// heartbeat gap threshold so the two detectors share roughly the same
/// latency envelope.
pub const DEFAULT_JOURNAL_SCAN_INTERVAL_SECS: u64 = 30;

/// Spawn a background task that watches `heartbeat` (a Unix-ms atomic
/// updated on every Claude stdout line) and emits an alert when the
/// heartbeat age exceeds the threshold.
///
/// The watchdog emits at most one alert per threshold crossing because
/// the alerts table dedups on fingerprint — but every additional
/// crossing still bumps the row's count + last_seen_at, giving triage
/// a sense of "how often this fires" without spamming.
///
/// Returns the `JoinHandle` in case the caller wants to abort on shutdown;
/// current bot processes are long-lived, so it is usually left dangling.
pub fn spawn_heartbeat_watchdog(
    bot_name: String,
    heartbeat: Arc<AtomicU64>,
    alerts_writer: Option<Arc<AlertsWriter>>,
    gap_threshold_secs: u64,
    poll_interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            bot = %bot_name,
            gap_threshold_secs,
            poll_interval_secs,
            "heartbeat watchdog started"
        );
        let mut ticker = tokio::time::interval(Duration::from_secs(poll_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the immediate-tick warm-up so a bot that just spawned and has
        // `heartbeat == 0` doesn't immediately emit a bogus "gap" alert.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let last_ms = heartbeat.load(Ordering::SeqCst);
            if last_ms == 0 {
                // Subprocess hasn't produced its first line yet. Not a gap,
                // it's startup.
                continue;
            }
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if now_ms < last_ms {
                // Clock skew (NTP step backwards) — ignore one tick rather
                // than emit a negative-gap alert.
                continue;
            }
            let gap_ms = now_ms - last_ms;
            let gap_secs = gap_ms / 1000;
            if gap_secs >= gap_threshold_secs {
                // Bucket the gap duration so a one-minute outage and a
                // ten-minute outage don't dedup onto the same row.
                // Fingerprint keys are stable, values are not — so we
                // encode the bucket into the summary string (which IS
                // part of the fingerprint).
                let bucket = bucket_gap_secs(gap_secs);
                let summary = format!(
                    "{}: claude subprocess heartbeat gap ≥{}s",
                    bot_name, bucket
                );
                let alert = BugAlert::new(
                    format!("{}-watchdog", bot_name.to_lowercase()),
                    Severity::High,
                    "heartbeat.gap",
                    summary,
                    json!({
                        "bot": bot_name,
                        "gap_secs": gap_secs,
                        "gap_bucket_secs": bucket,
                        "last_heartbeat_unix_ms": last_ms,
                        "threshold_secs": gap_threshold_secs,
                    }),
                );
                alerts_writer.emit(alert);
                debug!(bot = %bot_name, gap_secs, "heartbeat gap alert emitted");
            }
        }
    })
}

/// Bucket a gap-in-seconds into stable thresholds. Matches the pattern
/// used by error-rate monitoring dashboards: 30s, 1m, 5m, 30m, 1h+.
/// Anything over an hour gets lumped into "3600+" — past that point the
/// bot is clearly dead and triage doesn't need finer resolution.
fn bucket_gap_secs(gap_secs: u64) -> u64 {
    if gap_secs < 60 {
        30
    } else if gap_secs < 300 {
        60
    } else if gap_secs < 1800 {
        300
    } else if gap_secs < 3600 {
        1800
    } else {
        3600
    }
}

/// Spawn a background task that periodically scans the per-bot journal
/// for recent entries that look like failures, and emits corresponding
/// alerts.
///
/// The scanner watermarks progress using the journal's `id` column so
/// each entry is scanned at most once even across bot restarts (we
/// persist the cursor in-memory for now; the next boot starts from
/// the latest `id` to avoid re-alerting on old rows).
pub fn spawn_journal_scanner(
    bot_name: String,
    database_path: std::path::PathBuf,
    alerts_writer: Option<Arc<AlertsWriter>>,
    poll_interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!(
            bot = %bot_name,
            path = %database_path.display(),
            poll_interval_secs,
            "journal scanner started"
        );
        // Open a dedicated connection for the scanner so we don't
        // contend on the engine's journal mutex. WAL mode on the writer
        // side means concurrent reads are fine.
        let conn = match rusqlite::Connection::open(&database_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    path = %database_path.display(),
                    err = %e,
                    "journal scanner: could not open DB; scanner disabled"
                );
                return;
            }
        };
        // Initial cursor = the current max(id), so we don't re-alert on
        // historical rows from before the process started. First real
        // scan will pick up rows with id > this cursor.
        let mut last_id: i64 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM journal", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0);

        let mut ticker = tokio::time::interval(Duration::from_secs(poll_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let rows_result: rusqlite::Result<Vec<JournalRow>> = (|| {
                let mut stmt = conn.prepare(
                    "SELECT id, entry_type, summary, detail, created_at
                     FROM journal
                     WHERE id > ?1
                     ORDER BY id ASC",
                )?;
                let rows = stmt
                    .query_map([last_id], |row| {
                        Ok(JournalRow {
                            id: row.get(0)?,
                            entry_type: row.get(1)?,
                            summary: row.get(2)?,
                            detail: row.get(3)?,
                            created_at: row.get(4)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })();

            let rows = match rows_result {
                Ok(r) => r,
                Err(e) => {
                    warn!(err = %e, "journal scanner query failed");
                    continue;
                }
            };

            for row in rows {
                last_id = row.id.max(last_id);
                if let Some(alert) = classify_journal_row(&bot_name, &row) {
                    alerts_writer.emit(alert);
                }
            }
        }
    })
}

#[derive(Debug, Clone)]
struct JournalRow {
    id: i64,
    entry_type: String,
    summary: String,
    detail: String,
    #[allow(dead_code)]
    created_at: String,
}

/// Classify a journal row — return Some(alert) if it looks like a
/// failure worth surfacing. Returns None for normal events.
///
/// Signals we treat as alerts:
/// - `entry_type` ends in `.error` (e.g. `guardian.error`, `claude.error`)
/// - summary contains `panicked` or `crashed` (case-insensitive)
///
/// We intentionally do NOT alert on `guardian.deny` — denials are the
/// guardian doing its job, not a bug. Only `guardian.error` (RPC
/// failure, IO failure) counts.
fn classify_journal_row(bot_name: &str, row: &JournalRow) -> Option<BugAlert> {
    let summary_lower = row.summary.to_lowercase();
    let is_error_type = row.entry_type.ends_with(".error");
    let is_panic = summary_lower.contains("panicked") || summary_lower.contains("panic at");
    let is_crash = summary_lower.contains("crashed") || summary_lower.contains("subprocess exited");
    if !is_error_type && !is_panic && !is_crash {
        return None;
    }

    // Severity heuristic. Panics/crashes are critical; typed `.error`
    // events are high by default unless the type itself suggests a
    // lower tier.
    let severity = if is_panic || is_crash {
        Severity::Critical
    } else {
        Severity::High
    };

    // Category carries the journal entry_type verbatim so downstream
    // triage can group by it.
    let category = format!("journal.{}", row.entry_type);

    Some(BugAlert::new(
        format!("{}-journal-scanner", bot_name.to_lowercase()),
        severity,
        category,
        // The summary IS the dedup signal — same failure, same summary,
        // same fingerprint. Truncate to keep the fingerprint input
        // bounded.
        truncate(&row.summary, 160),
        json!({
            "bot": bot_name,
            "journal_id": row.id,
            "entry_type": row.entry_type,
            "summary": row.summary,
            "detail": truncate(&row.detail, 2000),
        }),
    ))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Char-boundary safe truncation.
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_gap_maps_ranges_correctly() {
        assert_eq!(bucket_gap_secs(30), 30);
        assert_eq!(bucket_gap_secs(59), 30);
        assert_eq!(bucket_gap_secs(60), 60);
        assert_eq!(bucket_gap_secs(299), 60);
        assert_eq!(bucket_gap_secs(300), 300);
        assert_eq!(bucket_gap_secs(1799), 300);
        assert_eq!(bucket_gap_secs(1800), 1800);
        assert_eq!(bucket_gap_secs(3599), 1800);
        assert_eq!(bucket_gap_secs(3600), 3600);
        assert_eq!(bucket_gap_secs(86_400), 3600);
    }

    fn row(entry_type: &str, summary: &str) -> JournalRow {
        JournalRow {
            id: 1,
            entry_type: entry_type.to_string(),
            summary: summary.to_string(),
            detail: "some detail".to_string(),
            created_at: "2026-04-21T00:00:00".to_string(),
        }
    }

    #[test]
    fn classify_ignores_benign_events() {
        assert!(classify_journal_row("Nova", &row("tool_call", "send_message ok")).is_none());
        assert!(classify_journal_row("Nova", &row("guardian.allow", "wrote 42 bytes")).is_none());
        assert!(
            classify_journal_row("Nova", &row("guardian.deny", "path blocked"))
                .is_none(),
            "guardian.deny is expected behavior, never an alert"
        );
        assert!(classify_journal_row("Nova", &row("tg.send", "status 200")).is_none());
    }

    #[test]
    fn classify_catches_error_types() {
        let a = classify_journal_row(
            "Nova",
            &row("guardian.error", "RPC timeout talking to guardian"),
        )
        .expect("guardian.error must classify as alert");
        assert_eq!(a.severity, Severity::High);
        assert_eq!(a.category, "journal.guardian.error");
        assert_eq!(a.detected_by, "nova-journal-scanner");
    }

    #[test]
    fn classify_catches_panics_as_critical() {
        let a = classify_journal_row(
            "Nova",
            &row("tool_call", "claude worker panicked at 'unwrap on None'"),
        )
        .expect("panic keyword must classify as alert");
        assert_eq!(a.severity, Severity::Critical);
    }

    #[test]
    fn classify_catches_crashed_as_critical() {
        let a = classify_journal_row(
            "Nova",
            &row("subprocess.exit", "Claude Code subprocess exited unexpectedly"),
        )
        .expect("'subprocess exited' keyword must classify");
        assert_eq!(a.severity, Severity::Critical);
    }

    #[test]
    fn truncate_handles_multibyte_boundaries() {
        // é is 2 bytes. Truncating at byte 5 with a char-boundary in the
        // middle must step back to the previous boundary, not panic.
        let s = "aaaaééé";
        let out = truncate(s, 5);
        assert!(out.ends_with('…'));
        // Must not panic; must produce valid UTF-8.
        let _ = out.chars().count();
    }
}
