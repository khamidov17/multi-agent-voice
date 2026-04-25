//! Phase 1 end-to-end: AlertsWriter + query + mark_triaged in a
//! realistic multi-bot setup.
//!
//! This test spins up a real `AlertsWriter` pointing at a per-test
//! SQLite file, emits alerts from multiple simulated detection sources
//! (heartbeat watchdog + journal scanner + telegram-error hook),
//! verifies dedup and ordering via [`trio::chatbot::alerts::query_open_alerts`],
//! then exercises the triage close path.
//!
//! It does NOT spawn real Nova / real Claude Code — that would require
//! a Telegram bot token and live API access. The dispatch tools
//! (`read_alerts` / `mark_triaged` / `send_triage_report`) are covered
//! by unit tests inside the dispatch modules; this file covers the
//! shared-DB + writer integration.
//!
//! Closes the "Phase 1 has no main-crate integration harness" gap
//! parallel to `tests/phase0_protected_write.rs`.

use serde_json::json;
use serial_test::serial;
use trio::chatbot::alerts::{self, AlertsWriter, BugAlert, Severity};

#[tokio::test]
#[serial]
async fn writer_dedups_concurrent_detector_bursts() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("bug_alerts.db");
    let writer = AlertsWriter::spawn_with_path(&db).unwrap();

    // Simulate three detectors firing at once during a minute-long outage:
    // - heartbeat watchdog: 20 ticks, all same fingerprint (same bucket)
    // - journal scanner: 5 errors of the same entry_type
    // - telegram-error hook: 3 repeated 429s
    for _ in 0..20 {
        writer.emit(BugAlert::new(
            "nova-watchdog",
            Severity::High,
            "heartbeat.gap",
            "Nova: claude subprocess heartbeat gap ≥60s",
            json!({"bot": "Nova", "gap_bucket_secs": 60}),
        ));
    }
    for i in 0..5 {
        writer.emit(BugAlert::new(
            "nova-journal-scanner",
            Severity::High,
            "journal.guardian.error",
            "RPC timeout talking to guardian",
            json!({"journal_id": i, "entry_type": "guardian.error"}),
        ));
    }
    for _ in 0..3 {
        writer.emit(BugAlert::new(
            "atlas",
            Severity::Medium,
            "telegram.error",
            "sendMessage 429 — rate limited",
            json!({"http_status": 429}),
        ));
    }

    // Let the writer drain — 28 emits through a single mpsc is fast but
    // a slow CI env might take longer than 100ms.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let conn = rusqlite::Connection::open(&db).unwrap();
        let rows = alerts::query_open_alerts(&conn, None, None, None).unwrap();
        if rows.len() == 3 {
            // Verify the dedup counts landed correctly.
            let hb = rows
                .iter()
                .find(|r| r.category == "heartbeat.gap")
                .expect("heartbeat.gap must be present");
            let jn = rows
                .iter()
                .find(|r| r.category == "journal.guardian.error")
                .expect("journal scanner alert must be present");
            let tg = rows
                .iter()
                .find(|r| r.category == "telegram.error")
                .expect("telegram.error alert must be present");
            assert_eq!(hb.count, 20, "20 heartbeat emits must dedup to one row");
            assert_eq!(jn.count, 5, "5 scanner emits must dedup to one row");
            assert_eq!(tg.count, 3, "3 telegram 429s must dedup to one row");
            // Severity ordering in query_open: critical > high > medium > low.
            // No critical here so order is hb (high), jn (high), tg (medium).
            assert_eq!(rows[rows.len() - 1].category, "telegram.error",);
            return;
        }
    }
    panic!("writer did not drain 28 events into 3 deduped rows within 1.5s");
}

#[tokio::test]
#[serial]
async fn triaged_alert_reappears_on_regression() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("bug_alerts.db");
    let writer = AlertsWriter::spawn_with_path(&db).unwrap();

    let mk = || {
        BugAlert::new(
            "sentinel",
            Severity::Critical,
            "subprocess.crash",
            "nova claude_code died unexpectedly",
            json!({"exit_code": 137}),
        )
    };

    // First sighting.
    writer.emit(mk());
    // Drain.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let conn = rusqlite::Connection::open(&db).unwrap();
        if !alerts::query_open_alerts(&conn, None, None, None)
            .unwrap()
            .is_empty()
        {
            break;
        }
    }

    // Nova triages.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let rows = alerts::query_open_alerts(&conn, None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        let id = rows[0].id;
        let closed = alerts::mark_triaged(&conn, &[id], Some("restarted nova")).unwrap();
        assert_eq!(closed, 1);
        assert!(
            alerts::query_open_alerts(&conn, None, None, None)
                .unwrap()
                .is_empty()
        );
    }

    // Bug recurs. Writer upserts — the module invariant is that a
    // retrigger clears triaged_at so the regression re-surfaces.
    writer.emit(mk());
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let conn = rusqlite::Connection::open(&db).unwrap();
        let open = alerts::query_open_alerts(&conn, None, None, None).unwrap();
        if !open.is_empty() {
            assert_eq!(open.len(), 1);
            assert!(
                open[0].triaged_at.is_none(),
                "regression must clear triaged flag"
            );
            assert!(
                open[0].triage_note.is_none(),
                "old triage note must be cleared on regression"
            );
            assert_eq!(open[0].count, 2, "count must bump on retrigger");
            return;
        }
    }
    panic!("regression did not re-surface within 1s");
}

#[tokio::test]
#[serial]
async fn category_filter_and_severity_ordering() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("bug_alerts.db");
    let writer = AlertsWriter::spawn_with_path(&db).unwrap();

    writer.emit(BugAlert::new(
        "atlas",
        Severity::Low,
        "telegram.error",
        "chatty 400",
        json!({}),
    ));
    writer.emit(BugAlert::new(
        "sentinel",
        Severity::Critical,
        "subprocess.crash",
        "nova died",
        json!({}),
    ));
    writer.emit(BugAlert::new(
        "atlas",
        Severity::Medium,
        "telegram.error",
        "dropped poll",
        json!({}),
    ));

    // Drain.
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let conn = rusqlite::Connection::open(&db).unwrap();
        let all = alerts::query_open_alerts(&conn, None, None, None).unwrap();
        if all.len() == 3 {
            // Ordered by severity — critical first.
            assert_eq!(all[0].severity, Severity::Critical);
            // Category filter scopes correctly.
            let tg = alerts::query_open_alerts(&conn, None, Some("telegram.error"), None).unwrap();
            assert_eq!(tg.len(), 2);
            assert!(tg.iter().all(|r| r.category == "telegram.error"));
            return;
        }
    }
    panic!("writer did not drain 3 events in 1.5s");
}
