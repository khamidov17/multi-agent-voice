mod chatbot;
mod classifier;
mod config;
mod dashboard;
mod live_api;
mod prefilter;
mod telegram_log;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, ChatKind, InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton,
    KeyboardMarkup, ReplyMarkup, WebAppInfo,
};
use tracing::{error, info, warn};
use tracing_subscriber::prelude::*;

use chatbot::document;
use chatbot::{
    ChatMessage, ChatbotConfig, ChatbotEngine, ClaudeCode, GroqTranscriber, OpenAITranscriber,
    ReplyTo, TelegramClient, Whisper, system_prompt,
};
use classifier::{Classification, classify};
use config::Config;
use prefilter::{PrefilterResult, prefilter};

/// Max DM messages per hour for free users.
const FREE_RATE_LIMIT: usize = 50;

/// Onboarding state for DM users.
#[derive(Clone)]
enum OnboardingState {
    AwaitingTos,
}

struct BotState {
    config: Config,
    strikes: Mutex<HashMap<UserId, u8>>,
    chatbot: Option<ChatbotEngine>,
    /// Users who accepted ToS via /start.
    tos_accepted: Mutex<HashSet<UserId>>,
    /// Path to the persisted ToS file.
    tos_file: std::path::PathBuf,
    /// Onboarding state machine per user.
    onboarding: Mutex<HashMap<UserId, OnboardingState>>,
    /// Per-user message timestamps for rate limiting (free tier).
    rate_limits: Mutex<HashMap<UserId, VecDeque<Instant>>>,
    /// OpenAI Whisper STT (preferred).
    openai_transcriber: Option<OpenAITranscriber>,
    /// Groq-based STT (secondary).
    groq_transcriber: Option<GroqTranscriber>,
    /// Local Whisper STT (fallback).
    whisper: Option<Whisper>,
}

impl BotState {
    async fn new(config: Config, bot: &Bot) -> Self {
        // Get bot info
        let (bot_user_id, bot_username) = match bot.get_me().await {
            Ok(me) => {
                info!("Bot user ID: {}, username: @{}", me.id, me.username());
                (me.id.0 as i64, Some(me.username().to_string()))
            }
            Err(e) => {
                warn!("Failed to get bot info: {e}");
                (0, None)
            }
        };

        // Create chatbot if enabled
        let chatbot = if !config.allowed_groups.is_empty() {
            let primary_chat_id = config
                .allowed_groups
                .iter()
                .next()
                .map(|id| id.0)
                .unwrap_or(0);
            let owner_user_id = config.owner_ids.iter().next().map(|id| id.0 as i64);

            // Open reminder store
            let reminder_store = {
                let db_path = config.data_dir.join("reminders.db");
                match chatbot::reminders::ReminderStore::open(&db_path) {
                    Ok(store) => {
                        info!("⏰ Reminder store opened at {:?}", db_path);
                        Some(store)
                    }
                    Err(e) => {
                        warn!("Failed to open reminder store: {e}");
                        None
                    }
                }
            };

            let allowed_chat_ids: HashSet<i64> =
                config.allowed_groups.iter().map(|id| id.0).collect();

            // Shared bot-message bus: derive path from the data directory.
            // e.g. data/atlas → data/shared/bot_messages.db
            let shared_bot_messages_db = config
                .data_dir
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("shared")
                .join("bot_messages.db");

            let chatbot_config = ChatbotConfig {
                primary_chat_id,
                bot_user_id,
                bot_username: bot_username.clone(),
                bot_name: config.bot_name.clone(),
                full_permissions: config.full_permissions,
                owner_user_id,
                debounce_ms: 1000,
                data_dir: Some(config.data_dir.clone()),
                gemini_api_key: if config.gemini_api_key.is_empty() {
                    None
                } else {
                    Some(config.gemini_api_key.clone())
                },
                tts_endpoint: config.tts_endpoint.clone(),
                yandex_api_key: if config.yandex_api_key.is_empty() {
                    None
                } else {
                    Some(config.yandex_api_key.clone())
                },
                brave_search_api_key: if config.brave_search_api_key.is_empty() {
                    None
                } else {
                    Some(config.brave_search_api_key.clone())
                },
                reminder_store: reminder_store.clone(),
                allowed_chat_ids,
                shared_bot_messages_db: Some(shared_bot_messages_db),
            };

            // Fetch available TTS voices if endpoint configured
            let available_voices = if let Some(ref endpoint) = config.tts_endpoint {
                use crate::chatbot::tts::TtsClient;
                let tts = TtsClient::new(endpoint.clone());
                let voices = tts.list_voices().await;
                if !voices.is_empty() {
                    info!("TTS voices available: {}", voices.join(", "));
                }
                Some(voices)
            } else {
                None
            };

            // Query last message timestamp for time-gap awareness in system prompt
            let last_interaction = {
                let db_path = config.data_dir.join("database.db");
                if db_path.exists() {
                    rusqlite::Connection::open(&db_path).ok().and_then(|conn| {
                        conn.query_row(
                            "SELECT timestamp FROM messages ORDER BY rowid DESC LIMIT 1",
                            [],
                            |row| row.get::<_, String>(0),
                        )
                        .ok()
                    })
                } else {
                    None
                }
            };

            // Start Claude Code with system prompt and session persistence
            let mut prompt = system_prompt(
                &chatbot_config,
                available_voices.as_deref(),
                last_interaction.as_deref(),
            );
            let session_file = Some(config.data_dir.join("session_id"));

            // If no existing session, inject recent message history from the
            // database so the bot has context even on first start or after a
            // session reset. This is the safety net for when --resume isn't
            // available (new server, session overflow, corruption).
            let has_existing_session = session_file.as_ref().is_some_and(|p| {
                p.exists() && std::fs::read_to_string(p).is_ok_and(|s| !s.trim().is_empty())
            });
            if !has_existing_session {
                let db_path = config.data_dir.join("database.db");
                if db_path.exists() {
                    if let Ok(conn) = rusqlite::Connection::open(&db_path) {
                        let history = build_history_context(&conn, 50);
                        if !history.is_empty() {
                            info!(
                                "📜 No existing session — injecting {} chars of message history",
                                history.len()
                            );
                            prompt.push_str("\n\n");
                            prompt.push_str(&history);
                        }
                    }
                }
            }
            let claude_code = match ClaudeCode::start(
                prompt,
                session_file,
                config.full_permissions,
                config.tools_override.clone(),
            ) {
                Ok(cc) => cc,
                Err(e) => {
                    panic!("Failed to start Claude Code: {}", e);
                }
            };

            let telegram = Arc::new(TelegramClient::new(bot.clone()));

            // Start reminder background loop
            if let Some(ref store) = reminder_store {
                chatbot::reminders::start_reminder_loop(store.clone(), telegram.clone());
                info!("⏰ Reminder loop started");
            }

            let mut engine = ChatbotEngine::new(chatbot_config, telegram, claude_code);
            engine.run_startup_checks().await;
            engine.start_debouncer();
            engine.notify_owner("hey, just restarted").await;

            info!("Chatbot enabled (primary chat: {})", primary_chat_id);
            Some(engine)
        } else {
            info!("Chatbot disabled (no allowed_groups)");
            None
        };

        // Initialize OpenAI Whisper transcriber (preferred)
        let openai_transcriber = if !config.openai_api_key.is_empty() {
            info!("OpenAI Whisper STT enabled (preferred)");
            Some(OpenAITranscriber::new(config.openai_api_key.clone()))
        } else {
            None
        };

        // Initialize Groq transcriber (secondary, used if OpenAI key not set)
        let groq_transcriber = if openai_transcriber.is_none() && !config.groq_api_key.is_empty() {
            info!("Groq STT enabled (secondary transcription path)");
            Some(GroqTranscriber::new(config.groq_api_key.clone()))
        } else {
            if openai_transcriber.is_none() {
                info!("No STT API key configured - falling back to local Whisper if available");
            }
            None
        };

        // Initialize Whisper if model path is configured
        let whisper = if let Some(ref model_path) = config.whisper_model_path {
            match Whisper::new(model_path) {
                Ok(w) => {
                    info!("Whisper loaded from {:?}", model_path);
                    Some(w)
                }
                Err(e) => {
                    warn!("Failed to load Whisper model: {}", e);
                    None
                }
            }
        } else {
            info!("No Whisper model configured - voice transcription disabled");
            None
        };

        // Load persisted ToS acceptances from disk
        let tos_file = config.data_dir.join("tos_accepted.json");
        let mut tos_set: HashSet<UserId> = if tos_file.exists() {
            match std::fs::read_to_string(&tos_file) {
                Ok(s) => serde_json::from_str::<Vec<u64>>(&s)
                    .unwrap_or_default()
                    .into_iter()
                    .map(UserId)
                    .collect(),
                Err(e) => {
                    warn!("Failed to load tos_accepted.json: {e}");
                    HashSet::new()
                }
            }
        } else {
            HashSet::new()
        };
        info!("Loaded {} ToS-accepted users from disk", tos_set.len());

        // Owners and premium users always pre-accepted
        for owner in &config.owner_ids {
            tos_set.insert(*owner);
        }
        for premium in &config.premium_users {
            tos_set.insert(*premium);
        }

        Self {
            config,
            strikes: Mutex::new(HashMap::new()),
            chatbot,
            tos_accepted: Mutex::new(tos_set),
            tos_file,
            onboarding: Mutex::new(HashMap::new()),
            rate_limits: Mutex::new(HashMap::new()),
            openai_transcriber,
            groq_transcriber,
            whisper,
        }
    }

    async fn add_strike(&self, user_id: UserId) -> u8 {
        let mut strikes = self.strikes.lock().await;
        let count = strikes.entry(user_id).or_insert(0);
        *count += 1;
        *count
    }

    /// Check rate limit for a free user. Returns remaining count, or None if blocked.
    async fn check_rate_limit(&self, user_id: UserId) -> Option<usize> {
        // Premium/owner = unlimited
        if self.config.is_premium(user_id) {
            return Some(usize::MAX);
        }

        let mut limits = self.rate_limits.lock().await;
        let timestamps = limits.entry(user_id).or_insert_with(VecDeque::new);

        // Prune entries older than 1 hour
        let one_hour_ago = Instant::now() - std::time::Duration::from_secs(3600);
        while timestamps.front().is_some_and(|t| *t < one_hour_ago) {
            timestamps.pop_front();
        }

        if timestamps.len() >= FREE_RATE_LIMIT {
            None // Blocked
        } else {
            timestamps.push_back(Instant::now());
            Some(FREE_RATE_LIMIT - timestamps.len())
        }
    }
}

/// Wrapper mode: monitors and restarts the harness process.
///
/// Sliding window crash detection: if the harness crashes more than 10 times
/// within 10 minutes, the wrapper gives up and exits with code 1.
/// Exponential backoff is applied for rapid crashes (< 10 seconds runtime).
fn run_wrapper(config_path: &str) -> ! {
    let mut recent_restarts: Vec<Instant> = Vec::new();
    let window = Duration::from_secs(600); // 10 minutes
    let max_restarts = 10;
    let mut restart_count: u32 = 0;

    // Kill marker lives next to the config file so the wrapper can find it
    // without having to parse the JSON (avoids pulling in serde here).
    let config_dir = Path::new(config_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let kill_marker = config_dir.join("kill_marker");

    loop {
        // Re-resolve binary on every iteration for hot-reload support.
        let binary = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .expect("Failed to resolve binary path");

        // Check kill marker before spawning.
        if kill_marker.exists() {
            eprintln!("[wrapper] Kill marker found, exiting cleanly");
            std::process::exit(0);
        }

        let start = Instant::now();

        eprintln!(
            "[wrapper] Spawning harness: {} {}",
            binary.display(),
            config_path
        );

        let mut child = match std::process::Command::new(&binary).arg(config_path).spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[wrapper] Failed to spawn harness: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let status = match child.wait() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[wrapper] Failed to wait for harness: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let runtime = start.elapsed();
        eprintln!(
            "[wrapper] Harness exited: {:?} (ran for {:.1}s)",
            status,
            runtime.as_secs_f64()
        );

        // Check kill marker after exit — clean shutdown via /kill command.
        if kill_marker.exists() {
            eprintln!("[wrapper] Kill marker found after exit, stopping");
            std::process::exit(0);
        }

        // Sliding window crash detection.
        let now = Instant::now();
        recent_restarts.push(now);
        recent_restarts.retain(|t| now.duration_since(*t) < window);

        if recent_restarts.len() > max_restarts {
            eprintln!(
                "[wrapper] Too many restarts ({} in {:?}), giving up",
                recent_restarts.len(),
                window
            );
            std::process::exit(1);
        }

        // Exponential backoff for rapid crashes.
        if runtime < Duration::from_secs(10) {
            restart_count += 1;
            // 2^restart_count seconds, capped at 64s (2^6).
            let delay = Duration::from_secs(2u64.pow(restart_count.min(6)));
            eprintln!(
                "[wrapper] Rapid crash, backing off {:.0}s",
                delay.as_secs_f64()
            );
            std::thread::sleep(delay);
        } else {
            // Reset backoff counter after a healthy run.
            restart_count = 0;
        }
    }
}

/// Parse command-line arguments.
/// Returns `(wrapper_mode, config_path)`.
fn parse_args() -> (bool, String) {
    let args: Vec<String> = std::env::args().collect();
    let mut config_path = "claudir.json".to_string();
    let mut wrapper_mode = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--wrapper" => {
                wrapper_mode = true;
                i += 1;
                // Next argument (if present) is the config path.
                if i < args.len() && !args[i].starts_with('-') {
                    config_path = args[i].clone();
                    i += 1;
                }
            }
            arg if !arg.starts_with('-') => {
                config_path = arg.to_string();
                i += 1;
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                i += 1;
            }
        }
    }

    (wrapper_mode, config_path)
}

#[tokio::main]
async fn main() {
    let (wrapper_mode, config_path) = parse_args();

    if wrapper_mode {
        run_wrapper(&config_path);
    }
    let config = Config::load(&config_path);

    let bot = Bot::new(&config.telegram_bot_token);

    // Setup logging
    let log_dir = config.data_dir.join("logs");
    std::fs::create_dir_all(&log_dir).ok();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("claudir.log"))
        .expect("Failed to open log file");
    let (non_blocking, _guard) = tracing_appender::non_blocking(log_file);

    let registry = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive(tracing::Level::INFO.into()),
                ),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive(tracing::Level::INFO.into()),
                ),
        );

    if let Some(log_chat_id) = config.log_chat_id {
        let tg_layer = telegram_log::TelegramLogLayer::new(bot.clone(), log_chat_id);
        registry.with(tg_layer).init();
    } else {
        registry.init();
    }

    info!("🚀 Starting claudir...");
    info!("Loaded config from {config_path}");
    info!("Owner IDs: {:?}", config.owner_ids);
    if config.dry_run {
        info!("DRY RUN mode enabled");
    }

    // Start ngrok tunnel if auth token is configured
    let config = if let Some(ref token) = config.ngrok_auth_token.clone() {
        let port = config.live_app_port;
        match start_ngrok(token, port).await {
            Some(url) => {
                info!("🌐 ngrok tunnel: {}", url);
                Config {
                    live_app_url: Some(url),
                    ..config
                }
            }
            None => {
                warn!("ngrok failed to start — live_app_url remains unchanged");
                config
            }
        }
    } else {
        config
    };

    let state = Arc::new(BotState::new(config, &bot).await);

    // Start HTTP server for Gemini Live mini app
    {
        let live_allowed: std::collections::HashSet<i64> = state
            .config
            .owner_ids
            .iter()
            .map(|u| u.0 as i64)
            .chain(state.config.live_allowed_users.iter().map(|u| u.0 as i64))
            .collect();
        let api_state = live_api::ApiState {
            bot_token: state.config.telegram_bot_token.clone(),
            live_allowed_ids: Arc::new(live_allowed),
            gemini_api_key: state.config.gemini_api_key.clone(),
        };

        // Dashboard state: shared DB is at data_dir/../shared/bot_messages.db
        let shared_db_path = state
            .config
            .data_dir
            .parent()
            .unwrap_or(&state.config.data_dir)
            .join("shared")
            .join("bot_messages.db");
        let dash_state = Arc::new(dashboard::DashboardState {
            shared_db_path,
            bot_token: state.config.telegram_bot_token.clone(),
            telegram_bot_token: state.config.telegram_bot_token.clone(),
            group_chat_id: state
                .config
                .allowed_groups
                .iter()
                .next()
                .map(|c| c.0)
                .unwrap_or(-1003399442526),
            auth_username: state.config.dashboard_username.clone().unwrap_or_default(),
            auth_password: state.config.dashboard_password.clone().unwrap_or_default(),
        });

        let port = state.config.live_app_port;
        let app = live_api::router(api_state).merge(dashboard::router(dash_state));
        let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
        info!("🌐 Live mini app + Dashboard HTTP server on port {}", port);
        tokio::spawn(async move {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .expect("Failed to bind live app port");
            axum::serve(listener, app)
                .await
                .expect("Live app server error");
        });
    }

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_new_message))
        .branch(Update::filter_edited_message().endpoint(handle_edited_message))
        .branch(Update::filter_chat_member().endpoint(handle_chat_member));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

/// Start ngrok to tunnel the given port. Returns the public HTTPS URL, or None on failure.
async fn start_ngrok(auth_token: &str, port: u16) -> Option<String> {
    use tokio::process::Command;

    // Authenticate ngrok (idempotent)
    let auth = Command::new("ngrok")
        .args(["config", "add-authtoken", auth_token])
        .output()
        .await;
    if let Err(e) = auth {
        warn!("ngrok auth failed (is ngrok installed?): {e}");
        return None;
    }

    // Start ngrok tunnel in background (detached from our process group)
    match Command::new("ngrok")
        .args(["http", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_child) => {} // intentionally not awaiting — runs in background
        Err(e) => {
            warn!("Failed to spawn ngrok: {e}");
            return None;
        }
    }

    // Wait for ngrok API to become ready
    let client = reqwest::Client::new();
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok(resp) = client.get("http://localhost:4040/api/tunnels").send().await
            && let Ok(json) = resp.json::<serde_json::Value>().await
            && let Some(url) = json["tunnels"]
                .as_array()
                .and_then(|t| t.iter().find(|t| t["proto"].as_str() == Some("https")))
                .and_then(|t| t["public_url"].as_str())
        {
            return Some(url.to_string());
        }
    }

    warn!("ngrok tunnel did not become ready in time");
    None
}

async fn handle_new_message(bot: Bot, msg: Message, state: Arc<BotState>) -> ResponseResult<()> {
    let is_group = matches!(msg.chat.kind, ChatKind::Public(_));
    let is_private = matches!(msg.chat.kind, ChatKind::Private(_));

    let user = match msg.from {
        Some(ref u) => u,
        None => return Ok(()),
    };

    let username = user.username.as_deref().unwrap_or(&user.first_name);

    // Handle DMs — when owner_dms_only, silently ignore non-owner DMs
    if is_private {
        if state.config.owner_dms_only && !state.config.is_owner(user.id) {
            return Ok(());
        }
        return handle_dm(&bot, &msg, user, username, &state).await;
    }

    if !is_group {
        return Ok(());
    }

    // Check allowed group
    if !state.config.allowed_groups.is_empty()
        && !state.config.allowed_groups.contains(&msg.chat.id)
    {
        return Ok(());
    }

    // Get text (or caption for images/voice)
    let text = msg.text().or_else(|| msg.caption());
    let has_image = msg.photo().is_some();
    let has_voice = msg.voice().is_some();
    let has_document = msg.document().is_some();

    // Skip if no text, image, voice, or document
    if text.is_none() && !has_image && !has_voice && !has_document {
        return Ok(());
    }

    // SPAM FILTER FIRST - spam messages must NEVER reach the chatbot
    let is_spam = if let Some(text) = text {
        // Owners and trusted channels bypass spam filter
        let bypass_filter = state.config.is_owner(user.id)
            || msg
                .sender_chat
                .as_ref()
                .is_some_and(|c| state.config.is_trusted_channel(c.id));

        if bypass_filter {
            info!("Bypass spam filter for {username} ({})", user.id);
            false
        } else {
            let prefilter_result = prefilter(text, &state.config);
            let text_preview: String = text.chars().take(100).collect();
            info!(
                "Message from {username} ({}): \"{text_preview}\" → {:?}",
                user.id, prefilter_result
            );

            match prefilter_result {
                PrefilterResult::ObviousSpam => true,
                PrefilterResult::ObviousSafe => false,
                PrefilterResult::Ambiguous => match classify(text).await {
                    Ok(Classification::Spam) => {
                        info!("Haiku: spam");
                        true
                    }
                    Ok(Classification::NotSpam) => {
                        info!("Haiku: not spam");
                        false
                    }
                    Err(e) => {
                        warn!("Classification error: {e}");
                        false
                    }
                },
            }
        }
    } else {
        false // No text = not spam (image/voice only)
    };

    // Handle spam: delete, strike, ban - and DO NOT pass to chatbot
    if is_spam {
        let dry = state.config.dry_run;

        if dry {
            info!("[DRY RUN] Would delete message {}", msg.id);
        } else if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
            warn!("Failed to delete: {e}");
        }

        let strikes = state.add_strike(user.id).await;
        info!("{username} has {strikes} strike(s)");

        if strikes >= state.config.max_strikes {
            if dry {
                info!("[DRY RUN] Would ban {username}");
            } else {
                info!("Banning {username}");
                if let Err(e) = bot.ban_chat_member(msg.chat.id, user.id).await {
                    warn!("Failed to ban: {e}");
                }
            }
        }

        // CRITICAL: Do not pass spam to chatbot
        return Ok(());
    }

    // Only non-spam messages reach the chatbot
    if let Some(ref chatbot) = state.chatbot {
        // Download image if present
        let (image, photo_file_id) = if has_image {
            if let Some(photos) = msg.photo() {
                if let Some(largest) = photos.iter().max_by_key(|p| p.width * p.height) {
                    let fid = largest.file.id.0.clone();
                    match chatbot.download_image(&fid).await {
                        Ok(img) => (Some(img), Some(fid)),
                        Err(e) => {
                            warn!("Failed to download image: {}", e);
                            (None, Some(fid))
                        }
                    }
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        // Transcribe voice if present
        let voice_transcription = transcribe_voice(&bot, &state, &msg).await;

        // Extract document text if present
        let document_text = extract_document(&bot, &msg).await;

        let chat_msg = telegram_to_chat_message_with_media(
            &msg,
            image,
            photo_file_id,
            voice_transcription,
            document_text,
        );
        chatbot.handle_message(chat_msg).await;
    }

    Ok(())
}

/// Handle a private DM message.
async fn handle_dm(
    bot: &Bot,
    msg: &Message,
    user: &teloxide::types::User,
    username: &str,
    state: &BotState,
) -> ResponseResult<()> {
    let text = msg.text().unwrap_or("");

    // /kill command — owner-only graceful shutdown with kill marker.
    // Writes a marker file so the wrapper process does not restart the harness.
    if text == "/kill" {
        if !state.config.is_owner(user.id) {
            bot.send_message(msg.chat.id, "Access denied.").await.ok();
            return Ok(());
        }

        info!(
            "/kill from owner {} ({}), writing kill marker and shutting down",
            username, user.id
        );

        // Write kill marker next to the config file so the wrapper can find it
        // without parsing JSON (matches the wrapper's kill_marker path).
        let kill_marker = state.config.config_dir.join("kill_marker");

        if let Err(e) = std::fs::write(&kill_marker, b"") {
            warn!("Failed to write kill marker at {:?}: {e}", kill_marker);
        }

        bot.send_message(msg.chat.id, "Shutting down. Wrapper will not restart.")
            .await
            .ok();

        // Give Telegram a moment to deliver the reply before we exit.
        tokio::time::sleep(Duration::from_millis(500)).await;

        std::process::exit(0);
    }

    // /live command — open Gemini Live mini app (owners + live_allowed_users)
    if text == "/live" {
        if !state.config.can_use_live(user.id) {
            bot.send_message(msg.chat.id, "🚫 Access denied.")
                .await
                .ok();
            return Ok(());
        }

        match &state.config.live_app_url {
            Some(url) => {
                let web_app_url = format!("{}/live", url.trim_end_matches('/'));
                let keyboard =
                    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::web_app(
                        "🎤 Open Atlas Live",
                        WebAppInfo {
                            url: reqwest::Url::parse(&web_app_url).unwrap(),
                        },
                    )]]);
                bot.send_message(msg.chat.id, "Open Atlas Live voice assistant:")
                    .reply_markup(ReplyMarkup::InlineKeyboard(keyboard))
                    .await
                    .ok();
            }
            None => {
                bot.send_message(
                    msg.chat.id,
                    "⚠️ live_app_url not configured in claudir.json",
                )
                .await
                .ok();
            }
        }
        return Ok(());
    }

    // /start command — send bilingual welcome + ToS immediately
    if text == "/start" {
        info!("/start from {} ({})", username, user.id);

        let welcome = "\
<b>Atlas</b> — Your Telegram Assistant / Telegramdagi yordamchingiz

By using this bot, you agree to the terms of service:
Botdan foydalanish orqali siz foydalanish shartlariga rozilik bildirasiz:

• Illegal or harmful content is not allowed / Noqonuniy yoki zararli kontentga ruxsat berilmaydi
• Conversations may be processed to improve quality / Suhbatlar sifatni yaxshilash uchun qayta ishlanishi mumkin";

        let keyboard = KeyboardMarkup::new(vec![vec![
            KeyboardButton::new("Agree / Roziman ✅"),
            KeyboardButton::new("Decline / Rad etaman ❌"),
        ]])
        .resize_keyboard()
        .one_time_keyboard();

        bot.send_message(msg.chat.id, welcome)
            .parse_mode(teloxide::types::ParseMode::Html)
            .reply_markup(ReplyMarkup::Keyboard(keyboard))
            .await
            .ok();

        let mut onboarding = state.onboarding.lock().await;
        onboarding.insert(user.id, OnboardingState::AwaitingTos);
        return Ok(());
    }

    // Handle onboarding flow
    {
        let onboarding_state = {
            let onboarding = state.onboarding.lock().await;
            onboarding.get(&user.id).cloned()
        };

        if let Some(OnboardingState::AwaitingTos) = onboarding_state {
            let is_agree = text.contains('✅')
                || text.to_lowercase().contains("agree")
                || text.to_lowercase().contains("roziman");

            let is_decline = text.contains('❌')
                || text.to_lowercase().contains("decline")
                || text.to_lowercase().contains("rad etaman");

            if is_agree {
                info!("{} ({}) accepted ToS", username, user.id);

                {
                    let mut tos = state.tos_accepted.lock().await;
                    tos.insert(user.id);
                    let ids: Vec<u64> = tos.iter().map(|u| u.0).collect();
                    if let Ok(s) = serde_json::to_string(&ids)
                        && let Err(e) = std::fs::write(&state.tos_file, s)
                    {
                        warn!("Failed to persist tos_accepted: {e}");
                    }
                }
                {
                    let mut onboarding = state.onboarding.lock().await;
                    onboarding.remove(&user.id);
                }

                bot.send_message(msg.chat.id, "Great! You can write me anything / Ajoyib! Menga istalgan narsani yozishingiz mumkin 💬")
                    .reply_markup(ReplyMarkup::kb_remove())
                    .await
                    .ok();
                return Ok(());
            } else if is_decline {
                info!("{} ({}) declined ToS", username, user.id);

                let mut onboarding = state.onboarding.lock().await;
                onboarding.remove(&user.id);

                bot.send_message(msg.chat.id, "You declined the terms. Send /start to try again.\nSiz shartlarni rad etdingiz. Qayta urinish uchun /start yuboring.")
                    .reply_markup(ReplyMarkup::kb_remove())
                    .await
                    .ok();
                return Ok(());
            } else {
                // Unrecognized — re-show buttons
                let keyboard = KeyboardMarkup::new(vec![vec![
                    KeyboardButton::new("Agree / Roziman ✅"),
                    KeyboardButton::new("Decline / Rad etaman ❌"),
                ]])
                .resize_keyboard()
                .one_time_keyboard();

                bot.send_message(
                    msg.chat.id,
                    "Please tap a button / Iltimos, tugmani bosing:",
                )
                .reply_markup(ReplyMarkup::Keyboard(keyboard))
                .await
                .ok();
                return Ok(());
            }
        }
    }

    // Check ToS acceptance
    {
        let tos = state.tos_accepted.lock().await;
        if !tos.contains(&user.id) {
            bot.send_message(
                msg.chat.id,
                "Send /start to begin. / Boshlash uchun /start yuboring.",
            )
            .await
            .ok();
            return Ok(());
        }
    }

    // Check rate limit
    match state.check_rate_limit(user.id).await {
        None => {
            info!("Rate limit hit for {} ({})", username, user.id);

            let limit_msg = "Your free limit is reached (50 messages/hour).\nBepul limitingiz tugadi (soatiga 50 xabar).\n\nFor unlimited access contact @hamidov_avazbek.\nCheksiz foydalanish uchun @hamidov_avazbek ga yozing.\n\nUnlimited messages for just <b>19,000 so'm</b>/month!\nCheksiz xabarlar atigi <b>19 000 so'm</b>/oy!";

            bot.send_message(msg.chat.id, limit_msg)
                .parse_mode(teloxide::types::ParseMode::Html)
                .await
                .ok();
            return Ok(());
        }
        Some(remaining) => {
            if remaining <= 10 && remaining > 0 && !state.config.is_premium(user.id) {
                info!(
                    "DM from {} ({}) — {} messages remaining",
                    username, user.id, remaining
                );
            }
        }
    }

    // Chatbot must exist
    let chatbot = match state.chatbot.as_ref() {
        Some(c) => c,
        None => {
            error!("DM from {} but chatbot not initialized", username);
            bot.send_message(
                msg.chat.id,
                "Bot hozircha ishlamayapti. Keyinroq urinib ko'ring.",
            )
            .await
            .ok();
            return Ok(());
        }
    };

    info!("📨 DM from {} ({})", username, user.id);

    // Send typing indicator immediately
    if let Err(e) = bot.send_chat_action(msg.chat.id, ChatAction::Typing).await {
        warn!("Failed to send typing indicator: {}", e);
    }

    // Download image if present
    let (image, photo_file_id) = if let Some(photos) = msg.photo() {
        if let Some(largest) = photos.iter().max_by_key(|p| p.width * p.height) {
            let fid = largest.file.id.0.clone();
            match chatbot.download_image(&fid).await {
                Ok(img) => (Some(img), Some(fid)),
                Err(e) => {
                    warn!("Failed to download image: {}", e);
                    (None, Some(fid))
                }
            }
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Transcribe voice if present
    let voice_transcription = transcribe_voice(bot, state, msg).await;

    // Extract document text if present
    let document_text = extract_document(bot, msg).await;

    // Skip if no content at all
    if text.is_empty()
        && image.is_none()
        && voice_transcription.is_none()
        && document_text.is_none()
    {
        return Ok(());
    }

    let chat_msg = telegram_to_chat_message_with_media(
        msg,
        image,
        photo_file_id,
        voice_transcription,
        document_text,
    );
    chatbot.handle_message(chat_msg).await;

    Ok(())
}

fn telegram_to_chat_message_with_media(
    msg: &Message,
    image: Option<(Vec<u8>, String)>,
    photo_file_id: Option<String>,
    voice_transcription: Option<String>,
    document_text: Option<String>,
) -> ChatMessage {
    let user = msg.from.as_ref();
    let user_id = user.map(|u| u.id.0 as i64).unwrap_or(0);
    let first_name = user.map(|u| {
        let mut name = u.first_name.clone();
        if let Some(ref ln) = u.last_name {
            name.push(' ');
            name.push_str(ln);
        }
        name
    });
    let username = user
        .and_then(|u| u.username.as_deref())
        .unwrap_or_else(|| user.map(|u| u.first_name.as_str()).unwrap_or("unknown"))
        .to_string();

    let timestamp = msg.date.format("%Y-%m-%d %H:%M").to_string();
    // Use text, or caption (for images/voice), or document text, or empty
    let base_text = msg
        .text()
        .or_else(|| msg.caption())
        .unwrap_or("")
        .to_string();

    // Prepend document content to the message text so Claude sees it
    let text = if let Some(ref doc) = document_text {
        let filename = msg
            .document()
            .and_then(|d| d.file_name.as_deref())
            .unwrap_or("document");
        if base_text.is_empty() {
            format!("[📎 {}]\n\n{}", filename, doc)
        } else {
            format!("{}\n\n[📎 {}]\n\n{}", base_text, filename, doc)
        }
    } else {
        base_text
    };

    let reply_to = msg.reply_to_message().map(|reply| {
        let reply_user = reply.from.as_ref();
        let reply_username = reply_user
            .and_then(|u| u.username.as_deref())
            .unwrap_or_else(|| {
                reply_user
                    .map(|u| u.first_name.as_str())
                    .unwrap_or("unknown")
            })
            .to_string();

        ReplyTo {
            message_id: reply.id.0 as i64,
            username: reply_username,
            text: reply.text().unwrap_or("").to_string(),
        }
    });

    ChatMessage {
        message_id: msg.id.0 as i64,
        chat_id: msg.chat.id.0,
        user_id,
        username,
        first_name,
        timestamp,
        text,
        reply_to,
        photo_file_id,
        image,
        voice_transcription,
    }
}

/// Download and transcribe a voice message if present.
/// Priority: OpenAI Whisper → Groq → local Whisper.
async fn transcribe_voice(bot: &Bot, state: &BotState, msg: &Message) -> Option<String> {
    use teloxide::net::Download;

    let voice = msg.voice()?;

    info!(
        "🎤 Voice message from user {} ({} seconds)",
        msg.from.as_ref().map(|u| u.id.0).unwrap_or(0),
        voice.duration
    );

    // Download voice file
    let file = match bot.get_file(voice.file.id.clone()).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to get voice file info: {}", e);
            return Some(format!("[Voice message - download failed: {}]", e));
        }
    };

    let mut data = Vec::new();
    if let Err(e) = bot.download_file(&file.path, &mut data).await {
        warn!("Failed to download voice file: {}", e);
        return Some(format!("[Voice message - download failed: {}]", e));
    }

    info!("📥 Downloaded voice ({} bytes)", data.len());

    // Primary: OpenAI Whisper API
    if let Some(ref openai) = state.openai_transcriber {
        return match openai.transcribe(&data, voice.duration.seconds()).await {
            Ok(text) => Some(text),
            Err(e) => {
                warn!("OpenAI Whisper transcription failed: {}", e);
                Some(format!("[Voice message - transcription failed: {}]", e))
            }
        };
    }

    // Secondary: Groq API
    if let Some(ref groq) = state.groq_transcriber {
        return match groq.transcribe(&data, voice.duration.seconds()).await {
            Ok(text) => Some(text),
            Err(e) => {
                warn!("Groq transcription failed: {}", e);
                Some(format!("[Voice message - transcription failed: {}]", e))
            }
        };
    }

    // Fallback: local Whisper model
    if let Some(whisper) = state.whisper.as_ref() {
        let whisper = whisper.clone();
        return match tokio::task::spawn_blocking(move || whisper.transcribe(&data)).await {
            Ok(Ok(text)) => {
                let preview: String = text.chars().take(100).collect();
                info!("📝 Whisper transcribed: \"{}\"", preview);
                Some(text)
            }
            Ok(Err(e)) => {
                warn!("Whisper transcription failed: {}", e);
                Some(format!("[Voice message - transcription failed: {}]", e))
            }
            Err(e) => {
                warn!("Whisper transcription task panicked: {}", e);
                Some("[Voice message - transcription failed]".to_string())
            }
        };
    }

    warn!("Voice message received but no transcription configured");
    Some("[Voice message - transcription not available]".to_string())
}

/// Download a document and extract its text (PDF/DOCX/XLSX).
/// Returns None for unsupported types or if download fails.
async fn extract_document(bot: &Bot, msg: &Message) -> Option<String> {
    use teloxide::net::Download;

    let doc = msg.document()?;
    let filename = doc.file_name.as_deref().unwrap_or("unknown");
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();

    if !matches!(ext.as_str(), "pdf" | "docx" | "xlsx" | "xls") {
        return None;
    }

    info!(
        "📎 Document from user {}: {} ({} bytes)",
        msg.from.as_ref().map(|u| u.id.0).unwrap_or(0),
        filename,
        doc.file.size
    );

    let file = match bot.get_file(doc.file.id.clone()).await {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to get document file info: {}", e);
            return Some(format!(
                "[Document '{}' - download failed: {}]",
                filename, e
            ));
        }
    };

    let mut data = Vec::new();
    if let Err(e) = bot.download_file(&file.path, &mut data).await {
        warn!("Failed to download document: {}", e);
        return Some(format!(
            "[Document '{}' - download failed: {}]",
            filename, e
        ));
    }

    info!("📥 Downloaded document ({} bytes)", data.len());

    // Extract text on blocking thread (file I/O + subprocess)
    let filename_owned = filename.to_string();
    match tokio::task::spawn_blocking(move || document::extract_text(&filename_owned, &data)).await
    {
        Ok(Some(text)) => {
            let preview: String = text.chars().take(80).collect();
            info!("📄 Extracted document text: \"{}\"...", preview);
            Some(text)
        }
        Ok(None) => {
            warn!("Unsupported document type: {}", filename);
            None
        }
        Err(e) => {
            warn!("Document extraction task panicked: {}", e);
            Some(format!("[Document '{}' - extraction failed]", filename))
        }
    }
}

async fn handle_edited_message(msg: Message, state: Arc<BotState>) -> ResponseResult<()> {
    let is_group = matches!(msg.chat.kind, ChatKind::Public(_));
    if !is_group {
        return Ok(());
    }

    if !state.config.allowed_groups.is_empty()
        && !state.config.allowed_groups.contains(&msg.chat.id)
    {
        return Ok(());
    }

    let text = match msg.text() {
        Some(t) => t,
        None => return Ok(()),
    };

    if let Some(ref chatbot) = state.chatbot {
        chatbot.handle_edit(msg.id.0 as i64, text).await;
    }

    Ok(())
}

async fn handle_chat_member(
    update: teloxide::types::ChatMemberUpdated,
    state: Arc<BotState>,
) -> ResponseResult<()> {
    // Only track for allowed groups
    if !state.config.allowed_groups.is_empty()
        && !state.config.allowed_groups.contains(&update.chat.id)
    {
        return Ok(());
    }

    let Some(ref chatbot) = state.chatbot else {
        return Ok(());
    };

    let user = &update.new_chat_member.user;
    let user_id = user.id.0 as i64;
    let username = user.username.clone();
    let first_name = user.first_name.clone();

    use teloxide::types::ChatMemberStatus;
    match update.new_chat_member.status() {
        ChatMemberStatus::Member | ChatMemberStatus::Administrator | ChatMemberStatus::Owner => {
            // User joined or was added
            if matches!(
                update.old_chat_member.status(),
                ChatMemberStatus::Left | ChatMemberStatus::Banned
            ) {
                info!("👋 Member joined: {} ({})", first_name, user_id);
                chatbot
                    .handle_member_joined(user_id, username, first_name)
                    .await;
            }
        }
        ChatMemberStatus::Left => {
            info!("👋 Member left: {} ({})", first_name, user_id);
            chatbot.handle_member_left(user_id).await;
        }
        ChatMemberStatus::Banned => {
            info!("🚫 Member banned: {} ({})", first_name, user_id);
            chatbot.handle_member_banned(user_id).await;
        }
        _ => {}
    }

    Ok(())
}

/// Build a recent message history context string from the database.
/// Used when starting a fresh session (no --resume) to give the bot
/// awareness of what happened before the session reset.
fn build_history_context(conn: &rusqlite::Connection, limit: usize) -> String {
    let mut stmt = match conn.prepare(
        "SELECT message_id, chat_id, user_id, username, timestamp, text
         FROM messages
         ORDER BY rowid DESC
         LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };

    let rows: Vec<(i64, i64, i64, String, String, String)> = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .ok()
        .map(|iter| iter.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    if rows.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "# Recent Message History (restored from database — your session was reset)\n\n\
         These are the most recent messages from before your session reset. \
         Use them to maintain conversational continuity.\n\n",
    );

    // Rows are in reverse order (newest first), reverse for chronological
    for (msg_id, chat_id, user_id, username, timestamp, text) in rows.into_iter().rev() {
        // Escape XML content like the normal message formatter does
        let escaped_text = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let time_short = if timestamp.len() >= 16 {
            &timestamp[11..16] // HH:MM
        } else {
            &timestamp
        };
        out.push_str(&format!(
            "<msg id=\"{}\" chat=\"{}\" user=\"{}\" name=\"{}\" time=\"{}\">{}</msg>\n",
            msg_id, chat_id, user_id, username, time_short, escaped_text
        ));
    }

    out
}
