//! Autonomous cognitive loop — background "thinking time" for agents.
//!
//! Unlike the health monitor (which checks liveness), this loop triggers
//! the AI to actually THINK: analyze, reflect, maintain, and explore.
//!
//! Runs independently of message handling. User messages always have priority —
//! if the engine is busy, the cognitive tick is skipped.
//!
//! Modes rotate: MONITOR → IMPROVE → MAINTAIN → EXPLORE → repeat.
//!
//! MONITOR and IMPROVE modes auto-inject metrics/self-eval data into the prompt
//! so the agent doesn't have to query them manually.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::chatbot::database::Database;
use crate::chatbot::debounce::Debouncer;
use crate::chatbot::message::ChatMessage;
use crate::chatbot::metrics::MetricsCollector;

/// Cognitive modes that rotate each tick.
const MODES: &[(&str, &str)] = &[
    (
        "MONITOR",
        "Phase 1+2 triage + planning pass. \
      STEP 1 — Alerts: call `read_alerts` (no args). For each open alert: \
      - Critical/high + owner would want to know → `send_triage_report` with a concise \
        preamble and `auto_mark_triaged=true`. \
      - Benign (spurious gap, already-handled restart) → `mark_triaged` with a `note`. \
      - Not sure → leave for next tick. \
      STEP 2 — Fix plans: call `list_fix_plans` with status=\"draft\" and status=\"sent\". \
      - For every CRITICAL alert that does NOT have an open plan (draft or sent), draft one via \
        `draft_fix_plan`. Reference the alert's evidence in root_cause. Be concrete about \
        steps — list the actual files/functions, not 'investigate'. Pick low-risk wording if \
        the change really is small (e.g. detector threshold tuning). \
      - Once drafted, `send_fix_plan_to_owner` with a 1-2 sentence preamble explaining WHY \
        you drafted it. Draft→sent transitions automatically on send. \
      - Do NOT draft plans for medium/low alerts unless they're recurring (count > 5). \
      - Do NOT draft a plan if there's already an 'approved' plan for that alert — Phase 3 \
        will ship it; you're done with that alert. \
      STEP 3 — Owner replies: you'll see messages like `approve #42` / `reject #42 because X`. \
      The harness auto-transitions the plan status when these arrive; you don't need to handle \
      them in the cognitive loop. \
      Also: check data/shared/bot_messages.db for unanswered handoffs. \
      If all clear, stop quickly.",
    ),
    (
        "IMPROVE",
        "Reflect on your recent interactions and decisions. Check: has it been 6+ hours since \
      your last self-evaluation? If so, call self_evaluate with your honest score and lessons. \
      If not, write insights to memories/reflections/ with today's date. \
      Check your experiment log: python3 rag/log_experiment.py --view --last 5 \
      Are there patterns in failures? Lessons to document? \
      If nothing notable, stop quickly.",
    ),
    (
        "MAINTAIN",
        "Check for stale work and maintenance tasks. \
      Are there tasks in the shared tasks table with status='active' that haven't been updated recently? \
      Are there pending handoffs that nobody picked up? \
      Are there overdue reminders? \
      Check data/shared/artifacts.json — any in_progress items that seem abandoned? \
      Clean up turn snapshots older than 48 hours (keep the last 50) — \
      use get_snapshots to review, then they auto-expire. \
      Take action on anything stale. If all clean, stop quickly.",
    ),
    (
        "EXPLORE",
        "Look for optimization opportunities. \
      Check data/shared/eval_config.yaml — are the tests still relevant for the current project? \
      Check workspace/tools/registry.yaml — are there tools that could be improved? \
      Query RAG for recent knowledge: cd rag && python3 query.py 'recent improvements' \
      If you find something worth improving, propose it to the group. \
      If nothing stands out, stop quickly.",
    ),
];

/// State file for tracking cognitive loop progress.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CognitiveState {
    last_mode_index: usize,
    last_run_at: String,
    total_ticks: u64,
}

/// Start the autonomous cognitive loop as a background task.
///
/// # Arguments
/// * `bot_name` — name of this bot (for logging and prompt)
/// * `interval_secs` — how often to think (default 300 for Tier 1, 600 for Tier 2)
/// * `primary_chat_id` — the main chat ID for message injection
/// * `pending` — the engine's pending message queue
/// * `debouncer` — triggers processing after injecting cognitive message
/// * `is_processing` — atomic flag; if true, skip this tick (user messages have priority)
/// * `data_dir` — path to bot's data directory (for state file)
/// * `database` — bot's local database (for metrics/self-eval queries)
/// * `metrics` — in-memory metrics collector (for threshold checks)
#[allow(clippy::too_many_arguments)]
pub fn start_cognitive_loop(
    bot_name: String,
    interval_secs: u64,
    primary_chat_id: i64,
    pending: Arc<Mutex<Vec<ChatMessage>>>,
    debouncer: Arc<Debouncer>,
    is_processing: Arc<AtomicBool>,
    data_dir: Option<PathBuf>,
    database: Arc<Mutex<Database>>,
    metrics: Arc<MetricsCollector>,
    cognitive_daily_token_budget: u64,
) {
    tokio::spawn(async move {
        // Wait 60 seconds after startup before first cognitive tick
        // (let the bot stabilize, load context, handle initial messages)
        info!(
            "[cognitive] {} — waiting 60s before first tick (interval={}s)",
            bot_name, interval_secs
        );
        tokio::time::sleep(Duration::from_secs(60)).await;

        let state_path = data_dir
            .as_ref()
            .map(|d| d.join("cognitive_state.json"))
            .unwrap_or_else(|| PathBuf::from("cognitive_state.json"));

        // Load previous state
        let mut state: CognitiveState = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the first immediate tick (we already waited 60s)
        interval.tick().await;

        loop {
            interval.tick().await;

            // Respect is_processing — user messages always have priority
            if is_processing.load(Ordering::SeqCst) {
                info!(
                    "[cognitive] {} — skipping tick (engine busy processing)",
                    bot_name
                );
                continue;
            }

            // Token budget check: skip tick if cognitive spending exceeds daily limit
            if cognitive_daily_token_budget > 0
                && let Ok(db) = database.try_lock()
                && let Ok(conn) = db.connection().try_lock()
            {
                let spent: i64 = conn
                    .query_row(
                        "SELECT COALESCE(SUM(estimated_tokens), 0) FROM token_budget \
                         WHERE source = 'cognitive' AND timestamp > datetime('now', '-24 hours')",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if spent as u64 > cognitive_daily_token_budget {
                    info!(
                        "[cognitive] {} — budget exceeded ({} tokens today, limit {}). Skipping tick.",
                        bot_name, spent, cognitive_daily_token_budget
                    );
                    state.last_mode_index = (state.last_mode_index % MODES.len()) + 1;
                    continue;
                }
            }

            // Rotate through modes
            let mode_index = state.last_mode_index % MODES.len();
            let (mode_name, mode_prompt) = MODES[mode_index];

            info!(
                "[cognitive] {} — tick #{} mode={}",
                bot_name,
                state.total_ticks + 1,
                mode_name
            );

            // Build mode-specific extra context by querying metrics/self-eval.
            // Use try_lock() — if the DB is contended, skip injection for this tick.
            let extra_context =
                build_extra_context(mode_name, &bot_name, &database, &metrics).await;

            // Build the cognitive message
            let now = chrono::Utc::now().format("%H:%M").to_string();
            let full_prompt = format!(
                "[COGNITIVE:{}] {}\n\nBot: {}. Be concise — if nothing needs attention, stop quickly.{}",
                mode_name, mode_prompt, bot_name, extra_context
            );

            let cognitive_msg = ChatMessage {
                message_id: 0,
                chat_id: primary_chat_id,
                user_id: 0,
                username: "cognitive_loop".to_string(),
                first_name: Some("System".to_string()),
                timestamp: now,
                text: full_prompt,
                reply_to: None,
                photo_file_id: None,
                image: None,
                voice_transcription: None,
            };

            // Inject into pending queue
            {
                let mut p = pending.lock().await;
                p.push(cognitive_msg);
            }
            debouncer.trigger().await;

            // Update state
            state.last_mode_index = mode_index + 1;
            state.last_run_at = chrono::Utc::now().to_rfc3339();
            state.total_ticks += 1;

            // Save state
            if let Ok(json) = serde_json::to_string_pretty(&state) {
                let _ = std::fs::write(&state_path, json);
            }
        }
    });
}

/// Build mode-specific extra context to inject into the cognitive prompt.
///
/// Uses try_lock() on the database — if contended, returns empty string
/// rather than blocking the cognitive tick.
async fn build_extra_context(
    mode_name: &str,
    bot_name: &str,
    database: &Arc<Mutex<Database>>,
    metrics: &Arc<MetricsCollector>,
) -> String {
    match mode_name {
        "MONITOR" => {
            // Inject recent metrics snapshot + alerts + orchestrator hint for Nova
            let metrics_ctx = build_monitor_context(database, metrics).await;
            let nova_hint = if bot_name == "Nova" {
                "\n\nRun orchestrator_status to see the full picture of what all agents are working on."
            } else {
                ""
            };
            if metrics_ctx.is_empty() && nova_hint.is_empty() {
                String::new()
            } else {
                format!("{}{}", metrics_ctx, nova_hint)
            }
        }
        "IMPROVE" => {
            // Check if self-evaluation is due and inject eval data
            build_improve_context(database).await
        }
        _ => String::new(),
    }
}

/// Build MONITOR context: recent metrics snapshots + threshold alerts.
async fn build_monitor_context(
    database: &Arc<Mutex<Database>>,
    metrics: &Arc<MetricsCollector>,
) -> String {
    // Try to get metrics snapshots from DB — don't block if contended
    let db_guard = match database.try_lock() {
        Ok(g) => g,
        Err(_) => {
            warn!("[cognitive] Skipping metrics injection (DB lock contended)");
            return String::new();
        }
    };

    let conn = db_guard.connection();
    let conn_guard = match conn.try_lock() {
        Ok(g) => g,
        Err(_) => {
            warn!("[cognitive] Skipping metrics injection (conn lock contended)");
            return String::new();
        }
    };

    let snapshots = crate::chatbot::metrics::get_recent_snapshots(&conn_guard, 3);
    // Drop locks before doing string work
    drop(conn_guard);
    drop(db_guard);

    // Check in-memory threshold alerts (no DB lock needed — atomics)
    let alerts = metrics.check_thresholds();

    if snapshots.is_empty() && alerts.is_empty() {
        return String::new();
    }

    let mut ctx = String::from("\n\n--- METRICS DATA (auto-injected) ---\n");
    ctx.push_str(&crate::chatbot::metrics::format_metrics_summary(&snapshots));

    if !alerts.is_empty() {
        ctx.push_str("\n\nALERTS:\n");
        for alert in &alerts {
            ctx.push_str(&format!("- [{}] {}\n", alert.severity, alert.message));
        }
    }

    ctx
}

/// Build IMPROVE context: self-evaluation data if 6+ hours since last eval.
async fn build_improve_context(database: &Arc<Mutex<Database>>) -> String {
    // Try to get DB access — don't block if contended
    let db_guard = match database.try_lock() {
        Ok(g) => g,
        Err(_) => {
            warn!("[cognitive] Skipping self-eval injection (DB lock contended)");
            return String::new();
        }
    };

    let conn = db_guard.connection();

    if crate::chatbot::self_eval::should_evaluate(conn) {
        let data = crate::chatbot::self_eval::compute_eval_data(conn);
        // Drop lock before string formatting
        drop(db_guard);

        let eval_prompt = crate::chatbot::self_eval::format_eval_prompt(&data);
        format!(
            "\n\n--- SELF-EVALUATION DATA (auto-injected) ---\n{}",
            eval_prompt
        )
    } else {
        String::new()
    }
}
