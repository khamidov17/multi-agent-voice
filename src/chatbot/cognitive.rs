//! Autonomous cognitive loop — background "thinking time" for agents.
//!
//! Unlike the health monitor (which checks liveness), this loop triggers
//! the AI to actually THINK: analyze, reflect, maintain, and explore.
//!
//! Runs independently of message handling. User messages always have priority —
//! if the engine is busy, the cognitive tick is skipped.
//!
//! Modes rotate: MONITOR → IMPROVE → MAINTAIN → EXPLORE → repeat.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::info;

use crate::chatbot::debounce::Debouncer;
use crate::chatbot::message::ChatMessage;

/// Cognitive modes that rotate each tick.
const MODES: &[(&str, &str)] = &[
    (
        "MONITOR",
        "Review recent activity. Check data/{bot}/logs/claudir.log for errors or warnings in the last hour. \
      Check data/shared/bot_messages.db for any unanswered messages or failed handoffs. \
      Check heartbeats table — are all peer bots alive and responsive? \
      If you find anomalies, report them to the group. If all clear, stop quickly.",
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
pub fn start_cognitive_loop(
    bot_name: String,
    interval_secs: u64,
    primary_chat_id: i64,
    pending: Arc<Mutex<Vec<ChatMessage>>>,
    debouncer: Arc<Debouncer>,
    is_processing: Arc<AtomicBool>,
    data_dir: Option<PathBuf>,
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

            // Rotate through modes
            let mode_index = state.last_mode_index % MODES.len();
            let (mode_name, mode_prompt) = MODES[mode_index];

            info!(
                "[cognitive] {} — tick #{} mode={}",
                bot_name,
                state.total_ticks + 1,
                mode_name
            );

            // Build the cognitive message
            let now = chrono::Utc::now().format("%H:%M").to_string();
            let full_prompt = format!(
                "[COGNITIVE:{}] {}\n\nBot: {}. Be concise — if nothing needs attention, stop quickly.",
                mode_name, mode_prompt, bot_name
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
