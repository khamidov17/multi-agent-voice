//! Chatbot engine - relays Telegram messages to Claude Code.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::chatbot::claude_code::{ClaudeCode, ToolResult};
use crate::chatbot::context::ContextBuffer;
use crate::chatbot::database::Database;
use crate::chatbot::debounce::Debouncer;
use crate::chatbot::message::ChatMessage;
use crate::chatbot::reminders::ReminderStore;
use crate::chatbot::telegram::TelegramClient;
use crate::chatbot::tools::ToolCall;

use crate::chatbot::format;
use crate::chatbot::prompt_builder;
use crate::chatbot::tool_dispatch;

/// Maximum tool call iterations before forcing exit (Tier 2 chatbots).
const MAX_ITERATIONS: usize = 20;

/// Maximum tool call iterations for Tier 1 bots (full_permissions) that need to implement code.
const MAX_ITERATIONS_FULL: usize = 40;

/// Maximum wall-clock time for Tier 2 chatbots (seconds) — 10 minutes for proactive agents.
const MAX_PROCESSING_SECS: u64 = 600;

/// Maximum wall-clock time for Tier 1 bots implementing code (seconds) — 15 minutes.
const MAX_PROCESSING_SECS_FULL: u64 = 900;

/// Chatbot configuration.
#[derive(Debug, Clone)]
pub struct ChatbotConfig {
    pub primary_chat_id: i64,
    pub bot_user_id: i64,
    pub bot_username: Option<String>,
    pub bot_name: String,
    pub full_permissions: bool,
    pub owner_user_id: Option<i64>,
    pub debounce_ms: u64,
    pub data_dir: Option<PathBuf>,
    pub gemini_api_key: Option<String>,
    pub tts_endpoint: Option<String>,
    pub yandex_api_key: Option<String>,
    pub brave_search_api_key: Option<String>,
    pub reminder_store: Option<ReminderStore>,
    /// Allowed group/channel chat IDs. Negative IDs only. Positive (DMs) are always allowed.
    pub allowed_chat_ids: HashSet<i64>,
    /// Path to the shared bot-to-bot message bus database.
    /// When set, outgoing group messages are written to this DB and peer-bot
    /// messages are injected into the pending queue via a background poller.
    pub shared_bot_messages_db: Option<PathBuf>,
    /// Cognitive loop interval in seconds (0 = disabled).
    pub cognitive_interval_secs: u64,
    /// Whether cognitive loop is enabled.
    pub cognitive_enabled: bool,
}

impl Default for ChatbotConfig {
    fn default() -> Self {
        Self {
            primary_chat_id: 0,
            bot_user_id: 0,
            bot_username: None,
            bot_name: "Atlas".to_string(),
            full_permissions: false,
            owner_user_id: None,
            debounce_ms: 1000,
            data_dir: None,
            gemini_api_key: None,
            tts_endpoint: None,
            yandex_api_key: None,
            brave_search_api_key: None,
            reminder_store: None,
            allowed_chat_ids: HashSet::new(),
            shared_bot_messages_db: None,
            cognitive_interval_secs: 300,
            cognitive_enabled: true,
        }
    }
}

/// The chatbot engine.
pub struct ChatbotEngine {
    config: ChatbotConfig,
    context: Arc<Mutex<ContextBuffer>>,
    database: Arc<Mutex<Database>>,
    telegram: Arc<TelegramClient>,
    claude: Arc<Mutex<ClaudeCode>>,
    debouncer: Option<Arc<Debouncer>>,
    /// New messages pending processing.
    pending: Arc<Mutex<Vec<ChatMessage>>>,
    /// Atomic flag: true while a processing turn is active.
    is_processing: Arc<AtomicBool>,
    /// Direct inject handle — usable without holding the ClaudeCode mutex.
    inject_handle: Arc<std::sync::Mutex<std::sync::mpsc::Sender<String>>>,
    /// Performance metrics collector.
    pub metrics: Arc<crate::chatbot::metrics::MetricsCollector>,
}

impl ChatbotEngine {
    /// Create a new chatbot engine.
    pub fn new(config: ChatbotConfig, telegram: Arc<TelegramClient>, claude: ClaudeCode) -> Self {
        let context_path = config.data_dir.as_ref().map(|d| d.join("context.json"));
        let database_path = config.data_dir.as_ref().map(|d| d.join("database.db"));

        // Load context (for message lookups, not for sending to Claude)
        let context = if let Some(ref path) = context_path {
            ContextBuffer::load_or_new(path, 50000)
        } else {
            ContextBuffer::new()
        };

        // Load message store
        let database = if let Some(ref path) = database_path {
            Database::load_or_new(path)
        } else {
            Database::new()
        };

        // Grab the inject handle before wrapping claude in Arc<Mutex>.
        let inject_handle = claude.inject_handle();

        Self {
            config,
            context: Arc::new(Mutex::new(context)),
            database: Arc::new(Mutex::new(database)),
            telegram,
            claude: Arc::new(Mutex::new(claude)),
            debouncer: None,
            pending: Arc::new(Mutex::new(Vec::new())),
            is_processing: Arc::new(AtomicBool::new(false)),
            inject_handle,
            metrics: Arc::new(crate::chatbot::metrics::MetricsCollector::new()),
        }
    }

    /// Run startup health checks: DB integrity + pending DM recovery.
    pub async fn run_startup_checks(&self) {
        let db = self.database.lock().await;
        crate::chatbot::health::run_startup_checks(&db, &self.config.bot_name);
    }

    /// Start the debounce timer and (optionally) the shared bot-message poller.
    pub fn start_debouncer(&mut self) {
        let context = self.context.clone();
        let database = self.database.clone();
        let telegram = self.telegram.clone();
        let claude = self.claude.clone();
        let config = self.config.clone();
        let pending = self.pending.clone();
        let is_processing = self.is_processing.clone();
        let inject_handle = self.inject_handle.clone();
        let metrics = self.metrics.clone();

        // Notify used by the debouncer callback to request a re-trigger
        // after CC STOP when pending tasks remain. A watcher task (spawned
        // below) listens on this and calls debouncer.trigger().
        let retrigger = Arc::new(tokio::sync::Notify::new());
        let retrigger_inner = retrigger.clone();

        let debouncer = Arc::new(Debouncer::new(
            Duration::from_millis(self.config.debounce_ms),
            move || {
                let context = context.clone();
                let database = database.clone();
                let telegram = telegram.clone();
                let claude = claude.clone();
                let config = config.clone();
                let pending = pending.clone();
                let is_processing = is_processing.clone();
                let inject_handle = inject_handle.clone();
                let metrics = metrics.clone();
                let retrigger = retrigger_inner.clone();

                info!("Debouncer fired");

                // Check if a turn is already running.
                if is_processing
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    // Not processing — start a new turn.
                    tokio::spawn(async move {
                        // Take pending messages
                        let messages = {
                            let mut p = pending.lock().await;
                            std::mem::take(&mut *p)
                        };

                        if messages.is_empty() {
                            info!("No pending messages");
                            is_processing.store(false, Ordering::SeqCst);
                            return;
                        }

                        info!("Processing {} message(s)", messages.len());

                        let timeout_secs = if config.full_permissions {
                            MAX_PROCESSING_SECS_FULL
                        } else {
                            MAX_PROCESSING_SECS
                        };
                        let result = tokio::time::timeout(
                            tokio::time::Duration::from_secs(timeout_secs),
                            process_messages(
                                &config, &context, &database, &telegram, &claude, &pending,
                                &messages, &metrics,
                            ),
                        )
                        .await;

                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => error!("Process error: {}", e),
                            Err(_) => {
                                metrics
                                    .cc_turns_timed_out
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                error!("Processing timed out after {}s — aborting", timeout_secs);
                                // Reset Claude to a clean state so the next message starts fresh.
                                {
                                    let mut cc = claude.lock().await;
                                    if let Err(e) = cc.reset().await {
                                        error!("Failed to reset Claude after timeout: {e}");
                                    }
                                }
                                // Notify the last sender that something went wrong
                                if let Some(msg) = messages.last() {
                                    let _ = telegram
                                        .send_message(
                                            msg.chat_id,
                                            "Xatolik yuz berdi, qayta urinib ko'ring.",
                                            Some(msg.message_id),
                                        )
                                        .await;
                                }
                            }
                        }

                        // Save state
                        if let Some(ref data_dir) = config.data_dir {
                            let ctx = context.lock().await;
                            if let Err(e) = ctx.save(&data_dir.join("context.json")) {
                                error!("Failed to save context: {}", e);
                            }
                            let store = database.lock().await;
                            if let Err(e) = store.save() {
                                error!("Failed to save messages: {}", e);
                            }
                        }

                        is_processing.store(false, Ordering::SeqCst);

                        // Autonomous continuation: after CC STOP, check if there
                        // are pending bot-to-bot task messages that need a new turn.
                        // Without this, multi-step tasks stall because nothing
                        // triggers the next processing turn after CC stops.
                        let mut needs_retrigger = false;

                        if let Some(ref db_path) = config.shared_bot_messages_db {
                            if let Ok(db) =
                                crate::chatbot::bot_messages::BotMessageDb::open(db_path)
                            {
                                if let Ok(tasks) = db.pending_tasks_for(&config.bot_name) {
                                    if !tasks.is_empty() {
                                        info!(
                                            "Autonomous continuation: {} pending task(s) for {}",
                                            tasks.len(),
                                            config.bot_name
                                        );
                                        // Push a synthetic TASK_CONTINUE into pending
                                        // so the next debouncer turn picks it up.
                                        let task = &tasks[0];
                                        let continue_text = format!(
                                            "[SYSTEM] TASK_CONTINUE: you have {} pending task(s). \
                                             Next task from {}: {}",
                                            tasks.len(),
                                            task.from_bot,
                                            task.message,
                                        );
                                        pending.lock().await.push(
                                            crate::chatbot::message::ChatMessage {
                                                message_id: 0,
                                                chat_id: config.primary_chat_id,
                                                user_id: 0,
                                                username: "system".to_string(),
                                                first_name: Some("System".to_string()),
                                                timestamp: chrono::Utc::now()
                                                    .format("%Y-%m-%d %H:%M:%S")
                                                    .to_string(),
                                                text: continue_text,
                                                reply_to: None,
                                                photo_file_id: None,
                                                image: None,
                                                voice_transcription: None,
                                            },
                                        );
                                        needs_retrigger = true;
                                    }
                                }
                            }
                        }

                        // Also re-trigger if new messages arrived while saving state.
                        if !needs_retrigger {
                            let p = pending.lock().await;
                            if !p.is_empty() {
                                needs_retrigger = true;
                            }
                        }

                        if needs_retrigger {
                            // Signal the watcher task to call debouncer.trigger().
                            // Small delay lets is_processing=false settle.
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            retrigger.notify_one();
                        }
                    });
                } else {
                    // Already processing — inject pending messages mid-turn.
                    tokio::spawn(async move {
                        let messages = {
                            let mut p = pending.lock().await;
                            std::mem::take(&mut *p)
                        };

                        if messages.is_empty() {
                            return;
                        }

                        info!(
                            "Mid-turn inject: {} message(s) while processing",
                            messages.len()
                        );
                        // Use a continuation prefix so Claude treats this as part of
                        // the ongoing conversation, NOT a brand-new turn.
                        let formatted = format::format_messages_continuation(&messages);
                        match inject_handle.lock() {
                            Ok(tx) => {
                                if tx.send(formatted).is_err() {
                                    warn!("Mid-turn inject channel closed");
                                }
                            }
                            Err(e) => {
                                error!("Failed to lock inject handle: {}", e);
                            }
                        }
                    });
                }
            },
        ));

        // Watcher task: when the debouncer callback signals `retrigger`,
        // call debouncer.trigger() to start a new processing turn.
        {
            let debouncer_ref = debouncer.clone();
            tokio::spawn(async move {
                loop {
                    retrigger.notified().await;
                    info!("Retrigger: starting new debouncer cycle for pending tasks");
                    debouncer_ref.trigger().await;
                }
            });
        }

        // Start the shared bot-message poller if a DB path is configured.
        if let Some(ref db_path) = self.config.shared_bot_messages_db {
            crate::chatbot::bot_messages::start_polling(
                db_path.clone(),
                self.config.bot_name.clone(),
                self.config.primary_chat_id,
                self.pending.clone(),
                debouncer.clone(),
            );
            info!(
                "BotMessageDb polling started (bot={}, db={})",
                self.config.bot_name,
                db_path.display()
            );
        }

        // Start health monitor — grab the CC atomic handles while we have
        // exclusive access (no other task can hold the mutex at this point).
        {
            let (cc_pid, cc_heartbeat) = match self.claude.try_lock() {
                Ok(cc) => (cc.pid_handle(), cc.heartbeat_handle()),
                Err(_) => {
                    warn!("Health monitor: could not lock ClaudeCode at startup — skipping");
                    self.debouncer = Some(debouncer);
                    return;
                }
            };

            crate::chatbot::health::start_health_monitor(
                self.config.bot_name.clone(),
                self.telegram.bot_handle(),
                cc_pid,
                cc_heartbeat,
                self.config.owner_user_id,
                self.config.shared_bot_messages_db.clone(),
            );
            info!("Health monitor started for {}", self.config.bot_name);
        }

        // Start metrics flush task (every 5 minutes)
        {
            let metrics_ref = self.metrics.clone();
            let db_ref = self.database.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(300));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await; // skip first
                loop {
                    interval.tick().await;
                    let db = db_ref.lock().await;
                    let conn = db.connection().lock().unwrap();
                    if let Err(e) = metrics_ref.flush_to_db(&conn) {
                        error!("Metrics flush failed: {e}");
                    }
                }
            });
            info!("Metrics flush task started (5min interval)");
        }

        // Start cognitive loop — autonomous background thinking
        if self.config.cognitive_enabled && self.config.cognitive_interval_secs > 0 {
            crate::chatbot::cognitive::start_cognitive_loop(
                self.config.bot_name.clone(),
                self.config.cognitive_interval_secs,
                self.config.primary_chat_id,
                self.pending.clone(),
                debouncer.clone(),
                self.is_processing.clone(),
                self.config.data_dir.clone(),
            );
            info!(
                "Cognitive loop started for {} (interval={}s)",
                self.config.bot_name, self.config.cognitive_interval_secs
            );
        }

        self.debouncer = Some(debouncer);

        // Resume incomplete tasks from shared DB (spawn as async task)
        let resume_config = self.config.clone();
        let resume_pending = self.pending.clone();
        let resume_debouncer_opt = self.debouncer.clone();
        tokio::spawn(async move {
            // Small delay to let debouncer initialize
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Self::resume_incomplete_tasks_static(
                &resume_config,
                &resume_pending,
                resume_debouncer_opt.as_ref(),
            )
            .await;
        });
    }

    /// Check for incomplete tasks assigned to this bot and inject resume messages.
    async fn resume_incomplete_tasks_static(
        config: &ChatbotConfig,
        pending: &Arc<Mutex<Vec<ChatMessage>>>,
        debouncer: Option<&Arc<Debouncer>>,
    ) {
        let db_path = match &config.shared_bot_messages_db {
            Some(p) => p.clone(),
            None => return,
        };

        let db = match crate::chatbot::bot_messages::BotMessageDb::open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                warn!("Could not open shared DB for task resume: {e}");
                return;
            }
        };

        let tasks = match db.get_incomplete_tasks(&config.bot_name) {
            Ok(t) => t,
            Err(e) => {
                warn!("Could not query incomplete tasks: {e}");
                return;
            }
        };

        if tasks.is_empty() {
            return;
        }

        info!(
            "Found {} incomplete task(s) to resume for {}",
            tasks.len(),
            config.bot_name
        );

        let mut p = pending.lock().await;
        for task in &tasks {
            let checkpoint_info = task
                .checkpoint_json
                .as_deref()
                .unwrap_or("No checkpoint saved");

            let resume_text = format!(
                "[SYSTEM] TASK_RESUME: You were working on task \"{}\" (id: {}) before restart.\n\
                 Your last checkpoint: {}\n\
                 Context: {}\n\
                 Resume from where you left off. Use resume_task tool to load full context if needed.",
                task.title,
                task.id,
                checkpoint_info,
                task.context.as_deref().unwrap_or("none"),
            );

            p.push(ChatMessage {
                message_id: 0,
                chat_id: config.primary_chat_id,
                user_id: 0,
                username: "task_resume".to_string(),
                first_name: Some("System".to_string()),
                timestamp: chrono::Utc::now().format("%H:%M").to_string(),
                text: resume_text,
                reply_to: None,
                photo_file_id: None,
                image: None,
                voice_transcription: None,
            });

            info!(
                "Injected resume message for task: {} ({})",
                task.title, task.id
            );
        }

        // Trigger debouncer to process the resume messages
        drop(p); // release lock before triggering
        if let Some(d) = debouncer {
            d.trigger().await;
        }
    }

    /// Handle an incoming message.
    pub async fn handle_message(&self, msg: ChatMessage) {
        info!(
            "📨 {} ({}): \"{}\"",
            msg.username,
            msg.user_id,
            msg.text.chars().take(50).collect::<String>()
        );

        // Store in context and message store
        {
            let mut ctx = self.context.lock().await;
            ctx.add_message(msg.clone());
        }
        {
            let mut store = self.database.lock().await;
            store.add_message(msg.clone());
        }

        // Add to pending
        {
            let mut p = self.pending.lock().await;
            p.push(msg);
        }

        if let Some(ref debouncer) = self.debouncer {
            debouncer.trigger().await;
        }
    }

    /// Handle a message edit.
    pub async fn handle_edit(&self, message_id: i64, new_text: &str) {
        let mut ctx = self.context.lock().await;
        ctx.edit_message(message_id, new_text);
        // Note: edits don't trigger Claude, just update context
    }

    /// Handle a member joining.
    pub async fn handle_member_joined(
        &self,
        user_id: i64,
        username: Option<String>,
        first_name: String,
    ) {
        let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M").to_string();
        let mut db = self.database.lock().await;
        db.member_joined(user_id, username, first_name, timestamp);
    }

    /// Handle a member leaving.
    pub async fn handle_member_left(&self, user_id: i64) {
        let mut db = self.database.lock().await;
        db.member_left(user_id);
    }

    /// Handle a member being banned.
    pub async fn handle_member_banned(&self, user_id: i64) {
        let mut db = self.database.lock().await;
        db.member_banned(user_id);
    }

    /// Send a startup notification to the owner only.
    pub async fn notify_owner(&self, message: &str) {
        let owner_id = match self.config.owner_user_id {
            Some(id) => id,
            None => return,
        };

        info!("Notifying owner ({})", owner_id);
        match self.telegram.send_message(owner_id, message, None).await {
            Ok(msg_id) => {
                info!("Sent notification (msg_id: {})", msg_id);
                let bot_msg = ChatMessage {
                    message_id: msg_id,
                    chat_id: owner_id,
                    user_id: self.config.bot_user_id,
                    username: "Atlas".to_string(),
                    first_name: None,
                    timestamp: chrono::Utc::now().format("%H:%M").to_string(),
                    text: message.to_string(),
                    reply_to: None,
                    photo_file_id: None,
                    image: None,
                    voice_transcription: None,
                };
                {
                    let mut ctx = self.context.lock().await;
                    ctx.add_message(bot_msg.clone());
                }
                {
                    let mut store = self.database.lock().await;
                    store.add_message(bot_msg);
                }
            }
            Err(e) => error!("Failed to notify owner: {}", e),
        }
    }

    /// Download an image from Telegram.
    pub async fn download_image(&self, file_id: &str) -> Result<(Vec<u8>, String), String> {
        self.telegram.download_image(file_id).await
    }
}

/// Process pending messages by sending to Claude Code.
async fn process_messages(
    config: &ChatbotConfig,
    context: &Mutex<ContextBuffer>,
    database: &Mutex<Database>,
    telegram: &TelegramClient,
    claude: &Mutex<ClaudeCode>,
    pending: &tokio::sync::Mutex<Vec<ChatMessage>>,
    messages: &[ChatMessage],
    metrics: &Arc<crate::chatbot::metrics::MetricsCollector>,
) -> Result<(), String> {
    // Increment messages processed
    metrics
        .messages_processed
        .fetch_add(messages.len() as u64, std::sync::atomic::Ordering::Relaxed);
    metrics
        .cc_turns_total
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Collect images from messages
    let images: Vec<_> = messages
        .iter()
        .filter_map(|m| {
            m.image.as_ref().map(|(data, mime)| {
                let label = format!("Image from {} (msg {}):", m.username, m.message_id);
                (label, data.clone(), mime.clone())
            })
        })
        .collect();

    // Auto-inject memory for DM users (positive chat_id = private chat)
    // This ensures Atlas always has the user's context without an explicit read_memory call.
    // Only loads the file for the specific user(s) in this batch — no wasted tokens.
    let user_memory_prefix = if let Some(ref data_dir) = config.data_dir {
        let mut injected = String::new();
        let mut seen = std::collections::HashSet::new();
        for msg in messages {
            if msg.chat_id > 0
                && msg.user_id > 0
                && seen.insert(msg.user_id)
                && let Some(mem) =
                    prompt_builder::load_user_memory(data_dir, msg.user_id, &msg.username)
            {
                info!(
                    "💾 Auto-injecting memory for user {} ({})",
                    msg.username, msg.user_id
                );
                injected.push_str(&format!(
                    "[Auto-loaded memory for {} (user_id={})]\n{}\n\n",
                    msg.username, msg.user_id, mem
                ));
            }
        }
        injected
    } else {
        String::new()
    };

    // Format the new messages (text only)
    let raw_messages = format::format_messages(messages);
    let content = if user_memory_prefix.is_empty() {
        raw_messages
    } else {
        format!("{user_memory_prefix}{raw_messages}")
    };
    info!(
        "🤖 Sending to Claude: {} chars, {} image(s)",
        content.len(),
        images.len()
    );

    // Send typing indicator to all chats that have pending messages
    for msg in messages {
        telegram.send_typing(msg.chat_id).await;
    }

    let mut claude = claude.lock().await;

    // Send images first (if any)
    let mut response = if !images.is_empty() {
        // Send first image with the text content
        let (label, data, mime) = images.into_iter().next().unwrap();
        let combined = format!("{}\n\n{}", content, label);
        claude.send_image_message(combined, data, mime).await?
    } else {
        claude.send_message(content).await?
    };

    // Handle compaction — stop processing cleanly to avoid runaway API loops.
    // Sending a big context restore message causes Claude to try to continue processing,
    // which can trigger further compactions and $10+ runaway incidents.
    if response.compacted {
        warn!("🔄 Compaction detected on initial send — stopping cleanly");
        return Ok(());
    }

    // Track which memory files have been read (for edit validation)
    let mut memory_files_read: HashSet<String> = HashSet::new();

    // Reply-to: use the last message in the batch. Claude can override via reply_to_message_id.
    let default_reply_to = messages.last().map(|m| m.message_id);

    // Track whether we've already sent a response this round (for stop rejection).
    let mut _has_sent_response = false;

    // Counter for stop rejections — reset each processing turn.
    let mut stop_rejections: u32 = 0;

    // Total tool calls across all iterations — used for post-task reflection trigger.
    let mut total_tool_call_count: usize = 0;

    // Tool call loop — Tier 1 bots get more iterations for code implementation
    let max_iters = if config.full_permissions {
        MAX_ITERATIONS_FULL
    } else {
        MAX_ITERATIONS
    };
    for iteration in 0..max_iters {
        let action = response.action.as_str();
        info!(
            "🔧 Iteration {}: action={}, {} tool call(s)",
            iteration + 1,
            action,
            response.tool_calls.len()
        );

        // Execute tool calls (if any — tool_calls is optional with the new schema)
        let mut results = Vec::new();
        total_tool_call_count += response.tool_calls.len();
        for tc in &response.tool_calls {
            if matches!(tc.call, ToolCall::Done) {
                // Legacy `done` tool — treat as stop action
                results.push(ToolResult {
                    tool_use_id: tc.id.clone(),
                    content: None,
                    is_error: false,
                    image: None,
                });
                continue;
            }

            // Track send_message calls for stop rejection logic
            if matches!(
                tc.call,
                ToolCall::SendMessage { .. } | ToolCall::SendVoice { .. }
            ) {
                _has_sent_response = true;
            }
            info!("🔧 Executing: {:?}", tc.call);
            let tool_start = std::time::Instant::now();
            let result = tool_dispatch::execute_tool(
                tc,
                config,
                context,
                database,
                telegram,
                &mut memory_files_read,
                default_reply_to,
            )
            .await;
            let tool_duration_ms = tool_start.elapsed().as_millis() as u64;
            let tool_name = {
                let raw = format!("{:?}", tc.call);
                raw.split(|c: char| c == '{' || c == '(')
                    .next()
                    .unwrap_or("unknown")
                    .trim()
                    .to_string()
            };
            metrics.record_tool_call(&tool_name, tool_duration_ms, result.is_error);
            if crate::chatbot::journal::is_journalable_tool(&tool_name) {
                let summary = crate::chatbot::journal::auto_journal_summary(
                    &tool_name,
                    &format!("{:?}", tc.call),
                );
                let db = database.lock().await;
                let _ = crate::chatbot::journal::add_entry(
                    db.connection(),
                    None,
                    "action",
                    &summary,
                    &summary,
                    &[],
                    &[],
                );
            }
            if let Some(ref content) = result.content {
                let truncated: String = content.chars().take(100).collect();
                info!("Result: {}", truncated);
            }
            results.push(result);
        }

        let has_error = results.iter().any(|r| r.is_error);
        let has_results = results.iter().any(|r| r.content.is_some());
        let has_images = results.iter().any(|r| r.image.is_some());
        let has_done = response
            .tool_calls
            .iter()
            .any(|tc| matches!(tc.call, ToolCall::Done));

        // ── Control action handling (claudir architecture) ──────────────

        match action {
            // HEARTBEAT: Claude is still working, continue the loop
            "heartbeat" => {
                info!("💓 Heartbeat — still working");
                if !results.is_empty() {
                    response = claude.send_tool_results(results).await?;
                } else {
                    response = claude
                        .send_tool_results(vec![ToolResult {
                            tool_use_id: "heartbeat".to_string(),
                            content: Some("heartbeat acknowledged, continue".to_string()),
                            is_error: false,
                            image: None,
                        }])
                        .await?;
                }
                continue;
            }

            // SLEEP: Pause, then check for new messages before continuing
            "sleep" => {
                let sleep_ms = response.sleep_ms.unwrap_or(5000).min(300_000); // cap 5 min
                info!("Sleeping for {}ms", sleep_ms);
                // Send any pending tool results first (result is ignored — we send a fresh prompt after waking)
                if !results.is_empty() && (has_results || has_error) {
                    let _ = claude.send_tool_results(results).await?;
                }
                // Sleep
                tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
                // After waking, check for pending messages and inject them
                let wake_msg = {
                    let p = pending.lock().await;
                    if p.is_empty() {
                        "You just woke up from sleep. No new messages arrived yet. \
                         If you were waiting for a teammate (Nova/Sentinel), sleep again \
                         to keep checking. Only stop if there's truly nothing left to do."
                            .to_string()
                    } else {
                        let count = p.len();
                        format!(
                            "You just woke up from sleep. {} new message(s) arrived! \
                             They will be delivered to you next. Process them.",
                            count
                        )
                    }
                };
                response = claude
                    .send_message(wake_msg)
                    .await
                    .map_err(|e| format!("Claude error after sleep: {e}"))?;
                continue;
            }

            // STOP: Done processing — with stop rejection
            _ => {
                // STOP: exit if no errors/results to show AND (done tool called OR action=stop)
                if (has_done || action == "stop") && !has_error && !has_results && !has_images {
                    // Stop rejection: if new messages arrived during processing, reject the
                    // stop up to 3 times so Claude handles them before exiting.
                    let has_pending = {
                        let p = pending.lock().await;
                        !p.is_empty()
                    };

                    if has_pending && stop_rejections < 3 {
                        stop_rejections += 1;
                        warn!(
                            "Stop rejected ({}/3) — new messages arrived during processing",
                            stop_rejections
                        );
                        response = claude
                            .send_tool_results(vec![ToolResult {
                                tool_use_id: String::new(),
                                content: Some(format!(
                                    "New messages arrived while you were processing (rejection {}/3). \
                                     Check and respond to them before stopping.",
                                    stop_rejections
                                )),
                                is_error: true,
                                image: None,
                            }])
                            .await?;
                        continue;
                    }

                    if let Some(ref reason) = response.reason {
                        info!("Stopped: {} (iteration {})", reason, iteration + 1);
                    } else {
                        info!("Stopped after {} iteration(s)", iteration + 1);
                    }

                    // Save conversation summary to memory files (survives session
                    // resets, compaction, and server migration).
                    prompt_builder::save_conversation_summary(config, database);

                    // Post-task reflection: if this turn had 3+ tool calls and was NOT
                    // triggered by the cognitive loop, inject a reflect prompt so the bot
                    // learns from what it just did.
                    let is_cognitive = messages.iter().any(|m| m.username == "cognitive_loop");
                    if total_tool_call_count >= 3 && !is_cognitive {
                        info!(
                            "Turn had {} tool calls — injecting reflection prompt",
                            total_tool_call_count
                        );
                        let reflect_msg = ChatMessage {
                            message_id: 0,
                            chat_id: messages.first().map(|m| m.chat_id).unwrap_or(0),
                            user_id: 0,
                            username: "cognitive_loop".to_string(),
                            first_name: Some("System".to_string()),
                            timestamp: chrono::Utc::now().format("%H:%M").to_string(),
                            text: format!(
                                "[REFLECT] You just completed a turn with {} tool calls. \
                                 Call `reflect` to log what worked and what didn't. \
                                 Be specific — your reflections improve future turns.",
                                total_tool_call_count
                            ),
                            reply_to: None,
                            photo_file_id: None,
                            image: None,
                            voice_transcription: None,
                        };
                        let mut p = pending.lock().await;
                        p.push(reflect_msg);
                    }

                    return Ok(());
                }
            }
        }

        // If we reach here, we have results/errors/images to send back to Claude
        if results.is_empty() && !has_done {
            // No tools called and no stop/sleep/heartbeat handled above
            response = claude
                .send_tool_results(vec![ToolResult {
                    tool_use_id: "error".to_string(),
                    content: Some(
                        "No tool calls provided. Use action='stop' with a reason when done, \
                         or call tools like send_message."
                            .to_string(),
                    ),
                    is_error: true,
                    image: None,
                }])
                .await
                .map_err(|e| format!("Claude error: {e}"))?;
            continue;
        }

        // Extract any images before sending results
        let images: Vec<_> = results
            .iter()
            .filter_map(|r| {
                r.image
                    .as_ref()
                    .map(|(data, mime)| (data.clone(), mime.clone()))
            })
            .collect();

        // Send results back to Claude (query tools returned data it needs to see)
        response = claude.send_tool_results(results).await?;

        // Send any generated images for Claude to see
        for (image_data, media_type) in images {
            info!(
                "📷 Sending generated image to Claude ({} bytes)",
                image_data.len()
            );
            response = claude
                .send_image_message(
                    "Here's the image I just generated and sent:".to_string(),
                    image_data,
                    media_type,
                )
                .await?;
        }

        // Handle compaction after tool results — stop cleanly to avoid runaway loops.
        if response.compacted {
            warn!("Compaction detected after tool results — stopping cleanly");
            return Ok(());
        }
    }

    warn!("Max iterations reached");
    Ok(())
}
