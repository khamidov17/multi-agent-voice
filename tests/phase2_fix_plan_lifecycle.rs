//! Phase 2 end-to-end: full fix-plan lifecycle through the writer +
//! DB, owner-reply parser round-trip, and invalid-transition refusal.
//!
//! Like `phase1_alerts_flow`, this test does NOT spawn real Nova /
//! Claude Code. It exercises the **shared disk + writer + parser**
//! integration that Phase 2 adds. The dispatch tools themselves have
//! unit tests inside `tool_dispatch/fix_plans.rs` and the parser has
//! its own tests in `fix_plan_reply.rs`.

use trio::chatbot::fix_plan_reply::{parse_owner_reply, OwnerReply};
use trio::chatbot::fix_plans::{
    self, DraftError, FixPlan, FixPlanStatus, FixPlansWriter, UpdateStatusError,
};
use serial_test::serial;

fn mkplan(alert_id: i64, title: &str) -> FixPlan {
    FixPlan {
        alert_id,
        title: title.into(),
        root_cause: "watchdog fired 19× in 2h; Nova's sleep=60000 is by design".into(),
        steps: "- raise heartbeat threshold 30s → 90s\n- add gap-distribution metric".into(),
        risk: "low — detector tuning only; no runtime code paths change".into(),
        test_plan: "cargo test chatbot::detectors; live-soak 30min".into(),
    }
}

#[tokio::test]
#[serial]
async fn full_lifecycle_draft_to_sent_to_approved_to_implemented() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("fix_plans.db");
    let writer = FixPlansWriter::spawn_with_path(&db).unwrap();

    // Nova drafts.
    writer.draft(mkplan(17, "tighten heartbeat threshold"));
    let plan_id = wait_for_plan(&db, |rows| rows.len() == 1).await;

    // Nova sends to owner — status transitions draft → sent.
    writer.set_status(plan_id, FixPlanStatus::Sent, Some("dmed owner".into()));
    wait_for_status(&db, plan_id, FixPlanStatus::Sent).await;

    // Owner approves via Telegram DM — parsed by fix_plan_reply, applied
    // via fix_plans::update_status directly (the parser layer does this
    // out-of-band from the writer, synchronously).
    let conn = rusqlite::Connection::open(&db).unwrap();
    let reply = parse_owner_reply("approve #1 looks good");
    assert!(matches!(
        reply,
        OwnerReply::Approve {
            plan_id: 1,
            note: Some(_)
        }
    ));
    let row = fix_plans::update_status(
        &conn,
        plan_id,
        FixPlanStatus::Approved,
        Some("looks good"),
    )
    .expect("sent → approved");
    assert_eq!(row.status, FixPlanStatus::Approved);
    assert_eq!(row.decision_note.as_deref(), Some("looks good"));

    // Phase 3 (future) marks it implemented.
    let row = fix_plans::update_status(&conn, plan_id, FixPlanStatus::Implemented, None)
        .expect("approved → implemented");
    assert_eq!(row.status, FixPlanStatus::Implemented);

    // Listing status=implemented shows it; status=draft does not.
    let impls = fix_plans::list_plans(&conn, Some(FixPlanStatus::Implemented), None).unwrap();
    assert_eq!(impls.len(), 1);
    let drafts = fix_plans::list_plans(&conn, Some(FixPlanStatus::Draft), None).unwrap();
    assert!(drafts.is_empty());
}

#[tokio::test]
#[serial]
async fn reject_transitions_and_new_draft_unblocks() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("fix_plans.db");
    let writer = FixPlansWriter::spawn_with_path(&db).unwrap();

    writer.draft(mkplan(99, "first shot"));
    let first_id = wait_for_plan(&db, |rows| rows.len() == 1).await;
    writer.set_status(first_id, FixPlanStatus::Sent, None);
    wait_for_status(&db, first_id, FixPlanStatus::Sent).await;

    // Trying to draft a second plan while the first is non-terminal must fail.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let err = fix_plans::draft_plan(&conn, &mkplan(99, "second shot")).unwrap_err();
    match err {
        DraftError::NonTerminalExists {
            existing_id,
            existing_status,
        } => {
            assert_eq!(existing_id, first_id);
            assert_eq!(existing_status, "sent");
        }
        other => panic!("expected NonTerminalExists, got {:?}", other),
    }

    // Owner rejects the first plan.
    let reply = parse_owner_reply("reject #1 because the risk is underestimated");
    assert_eq!(
        reply,
        OwnerReply::Reject {
            plan_id: 1,
            note: Some("the risk is underestimated".into())
        }
    );
    fix_plans::update_status(
        &conn,
        first_id,
        FixPlanStatus::Rejected,
        Some("the risk is underestimated"),
    )
    .expect("sent → rejected");

    // Now a fresh draft for alert 99 is allowed.
    let second = fix_plans::draft_plan(&conn, &mkplan(99, "second shot, safer"))
        .expect("terminal rejection unblocks a new draft");
    assert_ne!(second.id, first_id);
    assert_eq!(second.status, FixPlanStatus::Draft);
}

#[tokio::test]
#[serial]
async fn invalid_transitions_are_refused() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("fix_plans.db");
    let _writer = FixPlansWriter::spawn_with_path(&db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    fix_plans::init_schema(&conn).unwrap();

    let r = fix_plans::draft_plan(&conn, &mkplan(1, "x")).unwrap();
    // draft → approved must go through sent first.
    let err = fix_plans::update_status(&conn, r.id, FixPlanStatus::Approved, None)
        .unwrap_err();
    matches!(err, UpdateStatusError::InvalidTransition { .. });
    // draft → implemented would skip human review entirely.
    let err = fix_plans::update_status(&conn, r.id, FixPlanStatus::Implemented, None)
        .unwrap_err();
    matches!(err, UpdateStatusError::InvalidTransition { .. });
    // approved → rejected (walk-back) is forbidden — need to go obsolete.
    fix_plans::update_status(&conn, r.id, FixPlanStatus::Sent, None).unwrap();
    fix_plans::update_status(&conn, r.id, FixPlanStatus::Approved, None).unwrap();
    let err = fix_plans::update_status(&conn, r.id, FixPlanStatus::Rejected, None)
        .unwrap_err();
    matches!(err, UpdateStatusError::InvalidTransition { .. });
}

// -------- helpers --------

async fn wait_for_plan(
    db: &std::path::Path,
    pred: impl Fn(&[fix_plans::FixPlanRow]) -> bool,
) -> i64 {
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let conn = rusqlite::Connection::open(db).unwrap();
        let rows = fix_plans::list_plans(&conn, None, None).unwrap();
        if pred(&rows) {
            return rows[0].id;
        }
    }
    panic!("waiter: plan predicate never satisfied within 1.2s");
}

async fn wait_for_status(db: &std::path::Path, id: i64, target: FixPlanStatus) {
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let conn = rusqlite::Connection::open(db).unwrap();
        if let Some(row) = fix_plans::get_plan(&conn, id)
            && row.status == target
        {
            return;
        }
    }
    panic!("plan #{} never reached status {:?} within 1.2s", id, target);
}
