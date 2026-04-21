//! Claude Code CLI - simple message relay with session persistence.
//!
//! Spawns a persistent Claude Code process and relays messages to it.
//! Claude Code maintains conversation history internally.
//! Uses --resume to continue previous sessions across restarts.
//!
//! SECURITY: Uses `--tools "WebSearch"` to allow only read-only web search.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Security model value Claude CLI uses when the session is corrupted/overflowed.
const SYNTHETIC_MODEL: &str = "<synthetic>";

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::tools::ToolCall;

/// RAII guard for Claude Code child process (claudir architecture).
/// Ensures wait() is always called on the child process, preventing zombies.
#[allow(dead_code)]
struct ChildGuard {
    child: Option<Child>,
    pid: u32,
}

#[allow(dead_code)]
impl ChildGuard {
    fn new(child: Child) -> Self {
        let pid = child.id();
        Self {
            child: Some(child),
            pid,
        }
    }

    fn take(&mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            info!("ChildGuard: killing PID {}", self.pid);
            let _ = child.kill();
            let _ = child.wait(); // Prevent zombie process
        }
    }
}

/// JSON schema for structured output - control actions + tool_calls.
const TOOL_CALLS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": ["stop", "sleep", "heartbeat"],
      "description": "Control action: stop (done processing, MUST include reason), sleep (pause then check for new messages), heartbeat (still working)"
    },
    "reason": {
      "type": "string",
      "description": "Required when action=stop. Explain WHY you are stopping (e.g. 'responded to Atlas task assignment'). Forces you to think about whether stopping is appropriate."
    },
    "sleep_ms": {
      "type": "integer",
      "description": "When action=sleep, how long to pause in milliseconds (max 300000 = 5 min). Use this to wait for a teammate to respond before checking back."
    },
    "tool_calls": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "tool": { "type": "string" },
          "chat_id": { "type": "integer" },
          "text": { "type": "string" },
          "reply_to_message_id": { "type": "integer" },
          "user_id": { "type": "integer" },
          "message_id": { "type": "integer" },
          "emoji": { "type": "string" },
          "last_n": { "type": "integer" },
          "from_date": { "type": "string" },
          "to_date": { "type": "string" },
          "username": { "type": "string" },
          "limit": { "type": "integer" },
          "duration_minutes": { "type": "integer" },
          "days_inactive": { "type": "integer" },
          "filter": { "type": "string" },
          "file_path": { "type": "string" },
          "path": { "type": "string" },
          "content": { "type": "string" },
          "old_string": { "type": "string" },
          "new_string": { "type": "string" },
          "pattern": { "type": "string" },
          "prompt": { "type": "string" },
          "caption": { "type": "string" },
          "description": { "type": "string" },
          "severity": { "type": "string" },
          "url": { "type": "string" },
          "source_image_file_id": { "type": "string" }
        },
        "required": ["tool"]
      }
    }
  },
  "required": ["action"]
}"#;

/// Tool call with ID for tracking.
#[derive(Debug, Clone)]
pub struct ToolCallWithId {
    pub id: String,
    pub call: ToolCall,
}

/// Tool execution result.
#[derive(Debug)]
pub struct ToolResult {
    pub tool_use_id: String,
    /// None = no results to show Claude, Some = results Claude should see
    pub content: Option<String>,
    pub is_error: bool,
    /// Optional image data (bytes, media_type) for Claude to see
    pub image: Option<(Vec<u8>, String)>,
}

/// Response from Claude Code.
#[derive(Debug)]
pub struct Response {
    pub tool_calls: Vec<ToolCallWithId>,
    /// True if context compaction occurred during this response.
    pub compacted: bool,
    /// Control action from structured output: "stop", "sleep", or "heartbeat"
    pub action: String,
    /// Reason for stopping (required by claudir architecture)
    pub reason: Option<String>,
    /// Sleep duration in ms (when action = "sleep")
    pub sleep_ms: Option<u64>,
    /// Dropped text detected in result field — Claude wrote text instead of calling send_message.
    /// The worker injects a correction back to Claude on the next turn.
    pub dropped_text: Option<String>,
}

/// Claude Code client - maintains persistent subprocess.
pub struct ClaudeCode {
    tx: mpsc::Sender<WorkerMessage>,
    rx: mpsc::Receiver<Response>,
    /// Inject channel — send text directly to Claude's stdin mid-turn.
    /// Wrapped in Arc<Mutex<>> so it survives subprocess restarts.
    inject_tx: Arc<Mutex<std::sync::mpsc::Sender<String>>>,
    /// PID of the currently running Claude Code subprocess (Feature 4).
    #[allow(dead_code)]
    pid: Arc<AtomicU32>,
    /// Millisecond timestamp of the last JSON line received from stdout (Feature 4).
    #[allow(dead_code)]
    heartbeat: Arc<AtomicU64>,
    /// Unix-millisecond timestamp until which the worker should sleep due to quota (Feature 5).
    #[allow(dead_code)]
    quota_reset_at: Arc<AtomicU64>,
}

enum WorkerMessage {
    UserMessage(String),
    /// Message with image: (text, image_data, media_type)
    ImageMessage(String, Vec<u8>, String),
    ToolResults(Vec<ToolResult>),
    /// Kill the subprocess and restart with a fresh session.
    Reset,
}

/// Shared atomics passed from ClaudeCode to the worker thread (Features 4 & 5).
struct WorkerAtomics {
    /// PID of the currently running Claude Code subprocess (Feature 4).
    pid: Arc<AtomicU32>,
    /// Millisecond timestamp of the last JSON line received from stdout (Feature 4).
    heartbeat: Arc<AtomicU64>,
    /// Unix-millisecond timestamp until which the worker should sleep due to quota (Feature 5).
    quota_reset_at: Arc<AtomicU64>,
}

/// Static configuration for the worker thread (reduces argument count).
struct WorkerConfig {
    system_prompt: String,
    resume_session: Option<String>,
    session_file: Option<PathBuf>,
    full_permissions: bool,
    tools_override: Option<String>,
    /// Phase 0 shadow-mode flag. When true AND `full_permissions` is true,
    /// Nova's Claude Code spawns with `Read,WebSearch` only (no Bash, Edit,
    /// or Write). The MCP `protected_write` tool becomes the only way to
    /// write files. Bash is deliberately dropped — with Bash available Nova
    /// could read guardian.key off disk (0400 owned by harness UID = Bash
    /// UID) and mint its own HMAC, bypassing the guardian entirely.
    use_protected_write: bool,
}

impl ClaudeCode {
    /// Start Claude Code, optionally resuming a previous session.
    /// If session_file exists, resume that session. Otherwise start fresh with system_prompt.
    /// `full_permissions` — Tier 1 gets Bash,Edit,Write,Read,WebSearch; Tier 2 gets WebSearch only.
    /// `tools_override` — if set, overrides the full_permissions-based tool selection.
    /// Start Claude Code, optionally resuming a previous session.
    ///
    /// - `full_permissions` — Tier 1 gets Bash,Edit,Write,Read,WebSearch; Tier 2 gets WebSearch only.
    /// - `tools_override` — if set, overrides the full_permissions-based tool selection.
    /// - `use_protected_write` — Phase 0 shadow-mode flag. When true AND `full_permissions`
    ///   is true, Nova's Claude Code spawns WITHOUT Bash/Edit/Write (only `Read,WebSearch`);
    ///   the AI is expected to use the MCP `protected_write` tool for file writes, which
    ///   routes through the bootstrap guardian. Dropping Bash is deliberate: Bash can
    ///   `cat > /opt/nova/src/main.rs` directly, bypassing the guardian. Without Bash,
    ///   the MCP tool is the only write path, so the guardian's allowlist/blocklist is
    ///   actually enforced.
    pub fn start_with_guardian(
        system_prompt: String,
        session_file: Option<PathBuf>,
        full_permissions: bool,
        tools_override: Option<String>,
        use_protected_write: bool,
    ) -> Result<Self, String> {
        let (msg_tx, msg_rx) = mpsc::channel::<WorkerMessage>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<Response>(32);

        // Inject channel: std::sync::mpsc because the worker is a std thread.
        // The sender is wrapped in Arc<Mutex<>> so the engine can swap it on restart.
        let (inject_tx_raw, inject_rx) = std::sync::mpsc::channel::<String>();
        let inject_tx = Arc::new(Mutex::new(inject_tx_raw));

        // Features 4 & 5: shared atomics for PID, heartbeat, and quota tracking.
        let pid = Arc::new(AtomicU32::new(0));
        let heartbeat = Arc::new(AtomicU64::new(0));
        let quota_reset_at = Arc::new(AtomicU64::new(0));

        let atomics_worker = WorkerAtomics {
            pid: pid.clone(),
            heartbeat: heartbeat.clone(),
            quota_reset_at: quota_reset_at.clone(),
        };

        // Check for existing session
        let resume_session = session_file.as_ref().and_then(|p| load_session_id(p));

        let config = WorkerConfig {
            system_prompt,
            resume_session,
            session_file,
            full_permissions,
            tools_override,
            use_protected_write,
        };

        std::thread::spawn(move || {
            if let Err(e) = worker_loop(config, msg_rx, resp_tx, inject_rx, atomics_worker) {
                error!("Claude Code worker died: {}", e);
            }
        });

        Ok(Self {
            tx: msg_tx,
            rx: resp_rx,
            inject_tx,
            pid,
            heartbeat,
            quota_reset_at,
        })
    }

    /// Return the PID of the currently running Claude Code subprocess (Feature 4).
    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::SeqCst)
    }

    /// Return the Unix-millisecond timestamp of the last heartbeat from Claude (Feature 4).
    #[allow(dead_code)]
    pub fn last_heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::SeqCst)
    }

    /// Return a cloned `Arc` to the PID atomic so callers can observe it without
    /// holding the `ClaudeCode` mutex (used by the health monitor).
    pub fn pid_handle(&self) -> Arc<AtomicU32> {
        self.pid.clone()
    }

    /// Return a cloned `Arc` to the heartbeat atomic so callers can observe it
    /// without holding the `ClaudeCode` mutex (used by the health monitor).
    pub fn heartbeat_handle(&self) -> Arc<AtomicU64> {
        self.heartbeat.clone()
    }

    /// Inject a message directly into Claude's stdin mid-turn.
    /// The message is delivered without waiting for the current turn to finish.
    #[allow(dead_code)]
    pub fn inject_message(&self, text: &str) {
        match self.inject_tx.lock() {
            Ok(tx) => {
                if tx.send(text.to_string()).is_err() {
                    warn!("inject_message: inject channel closed (worker may have died)");
                }
            }
            Err(e) => {
                error!("inject_message: failed to lock inject_tx: {}", e);
            }
        }
    }

    /// Return a clone of the inject sender handle so callers can inject without
    /// holding the ClaudeCode mutex (needed during active processing turns).
    pub fn inject_handle(&self) -> Arc<Mutex<std::sync::mpsc::Sender<String>>> {
        self.inject_tx.clone()
    }

    /// Send a user message and get response.
    pub async fn send_message(&mut self, content: String) -> Result<Response, String> {
        self.tx
            .send(WorkerMessage::UserMessage(content))
            .await
            .map_err(|_| "Worker channel closed")?;

        self.rx
            .recv()
            .await
            .ok_or_else(|| "Response channel closed".to_string())
    }

    /// Send tool results and get next response.
    pub async fn send_tool_results(
        &mut self,
        results: Vec<ToolResult>,
    ) -> Result<Response, String> {
        self.tx
            .send(WorkerMessage::ToolResults(results))
            .await
            .map_err(|_| "Worker channel closed")?;

        self.rx
            .recv()
            .await
            .ok_or_else(|| "Response channel closed".to_string())
    }

    /// Kill the Claude subprocess and restart with a fresh session.
    /// Call this after a timeout to ensure the next request starts cleanly.
    pub async fn reset(&mut self) -> Result<(), String> {
        self.tx
            .send(WorkerMessage::Reset)
            .await
            .map_err(|_| "Worker channel closed")?;
        // Wait for the synthetic Done that confirms the reset completed
        self.rx
            .recv()
            .await
            .ok_or_else(|| "Response channel closed".to_string())?;
        Ok(())
    }

    /// Send a message with an image and get response.
    pub async fn send_image_message(
        &mut self,
        text: String,
        image_data: Vec<u8>,
        media_type: String,
    ) -> Result<Response, String> {
        self.tx
            .send(WorkerMessage::ImageMessage(text, image_data, media_type))
            .await
            .map_err(|_| "Worker channel closed")?;

        self.rx
            .recv()
            .await
            .ok_or_else(|| "Response channel closed".to_string())
    }
}

#[derive(Serialize)]
struct InputMessage {
    #[serde(rename = "type")]
    msg_type: String,
    message: InputContent,
}

#[derive(Serialize)]
struct InputContent {
    role: String,
    content: MessageContent,
}

/// Message content - either plain text or multi-part (text + images).
#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    MultiPart(Vec<ContentPart>),
}

/// A part of multi-part content.
#[derive(Serialize)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
}

/// Image source for base64-encoded images.
#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OutputMessage {
    #[serde(rename = "system")]
    System(SystemMessage),
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default)]
        message: Option<AssistantMessage>,
    },
    #[serde(rename = "result")]
    Result {
        #[serde(default)]
        total_cost_usd: f64,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        structured_output: Option<StructuredOutput>,
        #[serde(default)]
        session_id: Option<String>,
        /// Direct text output from Claude (dropped text detection).
        /// If this is non-empty, Claude wrote text instead of calling send_message.
        #[serde(default)]
        result: Option<String>,
    },
    #[serde(other)]
    Other,
}

/// Assistant message with context management info.
#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    context_management: Option<ContextManagement>,
}

/// Context management info from compaction.
#[derive(Debug, Deserialize)]
struct ContextManagement {
    #[serde(default)]
    truncated_content_length: Option<usize>,
}

/// System subtype message (compaction, status, etc.)
#[derive(Debug, Deserialize)]
struct SystemMessage {
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StructuredOutput {
    /// Control action: "stop", "sleep", or "heartbeat"
    #[serde(default = "default_action")]
    action: String,
    /// Required when action=stop: justification for stopping
    #[serde(default)]
    reason: Option<String>,
    /// When action=sleep: how long to pause (ms, max 300000)
    #[serde(default)]
    sleep_ms: Option<u64>,
    /// Tool calls to execute before the action takes effect
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

fn default_action() -> String {
    "stop".to_string()
}

#[derive(Debug, Deserialize)]
struct RawToolCall {
    tool: String,
    #[serde(default)]
    chat_id: Option<i64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    reply_to_message_id: Option<i64>,
    #[serde(default)]
    user_id: Option<i64>,
    #[serde(default)]
    message_id: Option<i64>,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    duration_minutes: Option<i64>,
    #[serde(default)]
    days_inactive: Option<i64>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    // Memory tool fields
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    new_string: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    // send_image fields
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    // report_bug fields
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    // send_voice fields
    #[serde(default)]
    voice: Option<String>,
    // query tool field
    #[serde(default)]
    sql: Option<String>,
    // fetch_url field
    #[serde(default)]
    url: Option<String>,
    // poll fields
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    options: Option<Vec<String>>,
    #[serde(default)]
    is_anonymous: Option<bool>,
    #[serde(default)]
    allows_multiple_answers: Option<bool>,
    // reminder fields
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    trigger_at: Option<String>,
    #[serde(default)]
    repeat_cron: Option<String>,
    #[serde(default)]
    reminder_id: Option<i64>,
    // send_photo optional source image for editing
    #[serde(default)]
    source_image_file_id: Option<String>,
    // address field (yandex)
    #[serde(default)]
    address: Option<String>,
    // now tool
    #[serde(default)]
    utc_offset: Option<i32>,
}

impl RawToolCall {
    /// Convert raw tool call to typed ToolCall.
    /// Returns ParseError variant if tool is unknown or missing required fields.
    fn to_tool_call(&self) -> ToolCall {
        let parse = || -> Result<ToolCall, String> {
            match self.tool.as_str() {
                "send_message" => Ok(ToolCall::SendMessage {
                    chat_id: self.chat_id.ok_or("send_message requires chat_id")?,
                    text: self.text.clone().unwrap_or_default(),
                    reply_to_message_id: self.reply_to_message_id,
                }),
                "get_user_info" => {
                    if self.user_id.is_none() && self.username.is_none() {
                        Err("get_user_info requires user_id or username".to_string())
                    } else {
                        Ok(ToolCall::GetUserInfo {
                            user_id: self.user_id,
                            username: self.username.clone(),
                        })
                    }
                }
                "query" => Ok(ToolCall::Query {
                    sql: self.sql.clone().ok_or("query requires sql")?,
                }),
                "add_reaction" => Ok(ToolCall::AddReaction {
                    chat_id: self.chat_id.ok_or("add_reaction requires chat_id")?,
                    message_id: self.message_id.ok_or("add_reaction requires message_id")?,
                    emoji: self.emoji.clone().unwrap_or_default(),
                }),
                "delete_message" => Ok(ToolCall::DeleteMessage {
                    chat_id: self.chat_id.ok_or("delete_message requires chat_id")?,
                    message_id: self.message_id.ok_or("delete_message requires message_id")?,
                }),
                "mute_user" => Ok(ToolCall::MuteUser {
                    chat_id: self.chat_id.ok_or("mute_user requires chat_id")?,
                    user_id: self.user_id.ok_or("mute_user requires user_id")?,
                    duration_minutes: self.duration_minutes.unwrap_or(5),
                }),
                "ban_user" => Ok(ToolCall::BanUser {
                    chat_id: self.chat_id.ok_or("ban_user requires chat_id")?,
                    user_id: self.user_id.ok_or("ban_user requires user_id")?,
                }),
                "kick_user" => Ok(ToolCall::KickUser {
                    chat_id: self.chat_id.ok_or("kick_user requires chat_id")?,
                    user_id: self.user_id.ok_or("kick_user requires user_id")?,
                }),
                "get_chat_admins" => Ok(ToolCall::GetChatAdmins {
                    chat_id: self.chat_id.ok_or("get_chat_admins requires chat_id")?,
                }),
                "get_members" => Ok(ToolCall::GetMembers {
                    filter: self.filter.clone(),
                    days_inactive: self.days_inactive,
                    limit: self.limit,
                }),
                "import_members" => Ok(ToolCall::ImportMembers {
                    file_path: self.file_path.clone().ok_or("import_members requires file_path")?,
                }),
                "send_photo" => Ok(ToolCall::SendPhoto {
                    chat_id: self.chat_id.ok_or("send_photo requires chat_id")?,
                    prompt: self.prompt.clone().ok_or("send_photo requires prompt")?,
                    caption: self.caption.clone(),
                    reply_to_message_id: self.reply_to_message_id,
                    source_image_file_id: self.source_image_file_id.clone(),
                }),
                "send_voice" => Ok(ToolCall::SendVoice {
                    chat_id: self.chat_id.ok_or("send_voice requires chat_id")?,
                    text: self.text.clone().ok_or("send_voice requires text")?,
                    voice: self.voice.clone(),
                    reply_to_message_id: self.reply_to_message_id,
                }),
                // Memory tools
                "create_memory" => Ok(ToolCall::CreateMemory {
                    path: self.path.clone().ok_or("create_memory requires path")?,
                    content: self.content.clone().ok_or("create_memory requires content")?,
                }),
                "read_memory" => Ok(ToolCall::ReadMemory {
                    path: self.path.clone().ok_or("read_memory requires path")?,
                }),
                "edit_memory" => Ok(ToolCall::EditMemory {
                    path: self.path.clone().ok_or("edit_memory requires path")?,
                    old_string: self.old_string.clone().ok_or("edit_memory requires old_string")?,
                    new_string: self.new_string.clone().unwrap_or_default(),
                }),
                "list_memories" => Ok(ToolCall::ListMemories {
                    path: self.path.clone(),
                }),
                "search_memories" => Ok(ToolCall::SearchMemories {
                    pattern: self.pattern.clone().ok_or("search_memories requires pattern")?,
                    path: self.path.clone(),
                }),
                "delete_memory" => Ok(ToolCall::DeleteMemory {
                    path: self.path.clone().ok_or("delete_memory requires path")?,
                }),
                "fetch_url" => Ok(ToolCall::FetchUrl {
                    url: self.url.clone().ok_or("fetch_url requires url")?,
                }),
                "send_file" => Ok(ToolCall::SendFile {
                    chat_id: self.chat_id.ok_or("send_file requires chat_id")?,
                    file_path: self.file_path.clone().or_else(|| self.path.clone()).ok_or("send_file requires file_path")?,
                    caption: self.caption.clone(),
                    reply_to_message_id: self.reply_to_message_id,
                }),
                "send_music" => Ok(ToolCall::SendMusic {
                    chat_id: self.chat_id.ok_or("send_music requires chat_id")?,
                    prompt: self.prompt.clone().ok_or("send_music requires prompt")?,
                    reply_to_message_id: self.reply_to_message_id,
                }),
                "edit_message" => Ok(ToolCall::EditMessage {
                    chat_id: self.chat_id.ok_or("edit_message requires chat_id")?,
                    message_id: self.message_id.ok_or("edit_message requires message_id")?,
                    text: self.text.clone().ok_or("edit_message requires text")?,
                }),
                "send_poll" => Ok(ToolCall::SendPoll {
                    chat_id: self.chat_id.ok_or("send_poll requires chat_id")?,
                    question: self.question.clone().ok_or("send_poll requires question")?,
                    options: self.options.clone().ok_or("send_poll requires options")?,
                    is_anonymous: self.is_anonymous.unwrap_or(true),
                    allows_multiple_answers: self.allows_multiple_answers.unwrap_or(false),
                    reply_to_message_id: self.reply_to_message_id,
                }),
                "unban_user" => Ok(ToolCall::UnbanUser {
                    chat_id: self.chat_id.ok_or("unban_user requires chat_id")?,
                    user_id: self.user_id.ok_or("unban_user requires user_id")?,
                }),
                "set_reminder" => Ok(ToolCall::SetReminder {
                    chat_id: self.chat_id.ok_or("set_reminder requires chat_id")?,
                    message: self.message.clone().ok_or("set_reminder requires message")?,
                    trigger_at: self.trigger_at.clone().ok_or("set_reminder requires trigger_at")?,
                    repeat_cron: self.repeat_cron.clone(),
                }),
                "list_reminders" => Ok(ToolCall::ListReminders {
                    chat_id: self.chat_id,
                }),
                "cancel_reminder" => Ok(ToolCall::CancelReminder {
                    reminder_id: self.reminder_id.ok_or("cancel_reminder requires reminder_id")?,
                }),
                "yandex_geocode" => Ok(ToolCall::YandexGeocode {
                    address: self.address.clone().ok_or("yandex_geocode requires address")?,
                }),
                "yandex_map" => Ok(ToolCall::YandexMap {
                    chat_id: self.chat_id.ok_or("yandex_map requires chat_id")?,
                    address: self.address.clone().ok_or("yandex_map requires address")?,
                    reply_to_message_id: self.reply_to_message_id,
                }),
                "now" => Ok(ToolCall::Now {
                    utc_offset: self.utc_offset,
                }),
                "report_bug" => Ok(ToolCall::ReportBug {
                    description: self.description.clone().ok_or("report_bug requires description")?,
                    severity: self.severity.clone(),
                }),
                "done" => Ok(ToolCall::Done),
                "WebSearch" => Err("WebSearch is a Claude Code built-in tool. Use it BEFORE outputting tool_calls (it runs automatically when you search). Don't include it in the tool_calls array.".to_string()),
                _ => Err(format!("Unknown tool: '{}'. Available tools: send_message, get_user_info, query, add_reaction, delete_message, mute_user, ban_user, kick_user, get_chat_admins, get_members, import_members, send_photo, send_voice, create_memory, read_memory, edit_memory, list_memories, search_memories, delete_memory, fetch_url, send_music, edit_message, send_poll, unban_user, set_reminder, list_reminders, cancel_reminder, yandex_geocode, yandex_map, now, report_bug, done", self.tool)),
            }
        };

        match parse() {
            Ok(tool_call) => tool_call,
            Err(message) => {
                warn!("Tool parse error for '{}': {}", self.tool, message);
                ToolCall::ParseError { message }
            }
        }
    }
}

fn load_session_id(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_session_id(path: &Path, session_id: &str) {
    if let Err(e) = std::fs::write(path, session_id) {
        warn!("Failed to save session ID: {}", e);
    } else {
        info!("Saved session ID to {:?}", path);
    }
}

/// Set up a fresh Claude Code process, send the init message, and wait until ready.
/// Returns (process, stdin, out_rx, stderr_buf). Updates `session_id` in place.
/// `stderr_buf` is Feature 3: last 10 stderr lines for crash diagnostics.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn setup_claude_process(
    system_prompt: &str,
    resume_session: Option<&str>,
    session_file: &Option<PathBuf>,
    session_id: &mut Option<String>,
    full_permissions: bool,
    tools_override: &Option<String>,
    use_protected_write: bool,
    atomics: &WorkerAtomics,
) -> Result<
    (
        Child,
        ChildStdin,
        std::sync::mpsc::Receiver<OutputMessage>,
        Arc<Mutex<VecDeque<String>>>,
    ),
    String,
> {
    let mut process = spawn_process_with_guardian(
        system_prompt,
        resume_session,
        full_permissions,
        tools_override,
        use_protected_write,
    )?;
    let mut stdin = process.stdin.take().ok_or("No stdin")?;
    let stdout = process.stdout.take().ok_or("No stdout")?;
    let stderr = process.stderr.take();

    // Feature 4: update PID atomic with the new process PID.
    let new_pid = process.id();
    atomics.pid.store(new_pid, Ordering::SeqCst);
    info!("Claude Code started (PID {})", new_pid);

    // Feature 3: circular stderr buffer — last 10 lines shared with worker for crash diagnostics.
    let stderr_buf: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(10)));

    // Stderr reader thread — logs errors from Claude CLI, fills buffer, detects quota (Feature 5).
    if let Some(stderr) = stderr {
        let stderr_buf_clone = stderr_buf.clone();
        let quota_reset_at_clone = atomics.quota_reset_at.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) if !l.is_empty() => {
                        error!("Claude CLI stderr: {}", l);

                        // Feature 3: maintain circular buffer of last 10 lines.
                        if let Ok(mut buf) = stderr_buf_clone.lock() {
                            if buf.len() == 10 {
                                buf.pop_front();
                            }
                            buf.push_back(l.clone());
                        }

                        // Feature 5: detect API quota / rate-limit messages in stderr.
                        let lower = l.to_lowercase();
                        let is_quota = lower.contains("rate limit")
                            || lower.contains("rate_limit")
                            || lower.contains("quota")
                            || lower.contains("exceeded")
                            || lower.contains("retry after")
                            || lower.contains("reset");
                        if is_quota {
                            warn!("Quota/rate-limit detected in stderr: {}", l);
                            // Default: sleep for 60 seconds.  Try to parse a number of seconds
                            // from the line (e.g. "retry after 30s" or "reset in 45").
                            let sleep_secs: u64 = l
                                .split_whitespace()
                                .filter_map(|w| w.trim_end_matches('s').parse::<u64>().ok())
                                .find(|&n| n > 0 && n < 3600)
                                .unwrap_or(60);
                            let now_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let reset_at = now_ms + sleep_secs * 1000;
                            quota_reset_at_clone.store(reset_at, Ordering::SeqCst);
                            info!(
                                "Quota: worker will pause for {}s (reset_at={})",
                                sleep_secs, reset_at
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Stderr read error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    // Use std::sync::mpsc so wait_for_result can use recv_timeout for inject polling.
    let (out_tx, mut out_rx) = std::sync::mpsc::channel::<OutputMessage>();

    // Stdout reader thread — Feature 4: update heartbeat on every JSON line.
    let heartbeat_clone = atomics.heartbeat.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(l) if !l.is_empty() => l,
                Ok(_) => continue,
                Err(e) => {
                    warn!("Read error: {}", e);
                    break;
                }
            };

            let preview: String = line.chars().take(120).collect();
            info!("Claude stdout: {}", preview);

            // Feature 4: update heartbeat timestamp on every parsed JSON line.
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            heartbeat_clone.store(now_ms, Ordering::SeqCst);

            match serde_json::from_str::<OutputMessage>(&line) {
                Ok(msg) => {
                    if out_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!("Parse error: {} (line: {})", e, preview);
                }
            }
        }
    });

    // Send a short init message to trigger Claude Code's first output.
    //
    // **This is load-bearing.** Do NOT send `system_prompt` as a user message —
    // the system prompt is already passed via `--system-prompt` on the Command
    // line above (see `spawn_process_with_guardian`). Dumping it again as a
    // user message with nothing to respond to triggers an infinite synthetic-
    // response loop under `--json-schema`: Claude emits empty/synthetic, CC
    // rejects it, injects "Stop hook feedback: You MUST call the Structured
    // output tool", Claude emits synthetic again, ~1000 loops/sec.
    //
    // The init prompt asks for a `sleep` structured response, which is the
    // correct "no work to do" action in Nova's control schema. This gives
    // Claude a well-defined thing to emit, the schema validates, the harness
    // moves past startup into its normal message loop.
    let first_message = if resume_session.is_some() {
        "Session resumed. No pending work — call StructuredOutput with action=sleep until a new message arrives.".to_string()
    } else {
        "Startup handshake. No pending work — call StructuredOutput with action=sleep (e.g. 60000 ms) until a new message arrives.".to_string()
    };
    send_message(&mut stdin, &first_message)?;

    // Wait for system message (comes first in output)
    loop {
        match out_rx.recv().ok() {
            Some(OutputMessage::System(sys))
                if sys.subtype.is_none() || sys.subtype.as_deref() == Some("init") =>
            {
                // Build the allowed tools list from the single source of
                // truth — compute_allowed_tools. Previously this logic was
                // duplicated ("MUST mirror ... exactly"), a known bug magnet
                // called out by the /review maintainability specialist.
                let allowed_tools_str =
                    compute_allowed_tools(full_permissions, use_protected_write, tools_override);
                let mut allowed_set: Vec<&str> = vec!["StructuredOutput"];
                for tool in allowed_tools_str.split(',') {
                    let t = tool.trim();
                    if !t.is_empty() && !allowed_set.contains(&t) {
                        allowed_set.push(Box::leak(t.to_string().into_boxed_str()));
                    }
                }
                let unexpected: Vec<_> = sys
                    .tools
                    .iter()
                    .filter(|t| !allowed_set.contains(&t.as_str()))
                    .collect();
                if !unexpected.is_empty() {
                    error!("SECURITY: Unexpected tools: {:?}", unexpected);
                    return Err("Security violation".to_string());
                }
                if let Some(sid) = sys.session_id {
                    info!("Got session ID: {}", sid);
                    *session_id = Some(sid);
                }
                info!("Claude Code session ready");
                break;
            }
            Some(_) => continue,
            None => return Err("Output channel closed".to_string()),
        }
    }

    // Wait for result of first message (no inject needed during init).
    let (_, new_sid, _) = wait_for_result(&mut out_rx, None)?;
    if let Some(sid) = new_sid {
        *session_id = Some(sid);
    }
    info!("First message processed, ready for chat");

    // Save session ID
    if let (Some(sid), Some(path)) = (session_id.as_ref(), session_file.as_ref()) {
        save_session_id(path, sid);
    }

    Ok((process, stdin, out_rx, stderr_buf))
}

fn worker_loop(
    cfg: WorkerConfig,
    mut msg_rx: mpsc::Receiver<WorkerMessage>,
    resp_tx: mpsc::Sender<Response>,
    inject_rx: std::sync::mpsc::Receiver<String>,
    atomics: WorkerAtomics,
) -> Result<(), String> {
    let mut session_id: Option<String> = None;
    let (process, mut stdin, mut out_rx, mut _stderr_buf) = setup_claude_process(
        &cfg.system_prompt,
        cfg.resume_session.as_deref(),
        &cfg.session_file,
        &mut session_id,
        cfg.full_permissions,
        &cfg.tools_override,
        cfg.use_protected_write,
        &atomics,
    )?;

    // Feature 1: wrap the process in a ChildGuard for RAII cleanup.
    let mut guard = ChildGuard::new(process);

    // Main loop
    while let Some(msg) = msg_rx.blocking_recv() {
        // Feature 5: check quota before processing each message.
        {
            let reset_at = atomics.quota_reset_at.load(Ordering::SeqCst);
            if reset_at > 0 {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if now_ms < reset_at {
                    let sleep_ms = reset_at - now_ms;
                    warn!(
                        "Quota active — sleeping {}ms before processing next message",
                        sleep_ms
                    );
                    std::thread::sleep(Duration::from_millis(sleep_ms));
                }
                // Clear the quota timer once we have passed the reset point.
                atomics.quota_reset_at.store(0, Ordering::SeqCst);
            }
        }

        // Handle Reset before interacting with Claude subprocess
        if matches!(msg, WorkerMessage::Reset) {
            info!("Resetting Claude session (timeout recovery)");

            // Feature 1: take child from guard to kill it, then create new guard below.
            drop(stdin);
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }

            if let Some(ref path) = cfg.session_file {
                match std::fs::remove_file(path) {
                    Ok(()) => info!("Deleted session file for reset"),
                    Err(e) => warn!("Could not delete session file: {e}"),
                }
            }
            session_id = None;
            match setup_claude_process(
                &cfg.system_prompt,
                None,
                &cfg.session_file,
                &mut session_id,
                cfg.full_permissions,
                &cfg.tools_override,
                cfg.use_protected_write,
                &atomics,
            ) {
                Ok((new_proc, new_stdin, new_out_rx, new_stderr_buf)) => {
                    // Feature 1: wrap new process in a fresh ChildGuard.
                    guard = ChildGuard::new(new_proc);
                    stdin = new_stdin;
                    out_rx = new_out_rx;
                    _stderr_buf = new_stderr_buf;
                    info!("Claude session reset after timeout");
                }
                Err(e) => {
                    error!("Failed to reset Claude after timeout: {e}");
                    return Err(e);
                }
            }
            let reset_response = Response {
                tool_calls: vec![ToolCallWithId {
                    id: "reset_0".to_string(),
                    call: ToolCall::Done,
                }],
                compacted: false,
                action: "stop".to_string(),
                reason: Some("session reset".to_string()),
                sleep_ms: None,
                dropped_text: None,
            };
            if resp_tx.blocking_send(reset_response).is_err() {
                break;
            }
            continue;
        }

        match msg {
            WorkerMessage::UserMessage(content) => {
                send_message(&mut stdin, &content)?;
            }
            WorkerMessage::ImageMessage(text, image_data, media_type) => {
                send_message_with_image(&mut stdin, &text, &image_data, &media_type)?;
            }
            WorkerMessage::ToolResults(results) => {
                let content = format_tool_results(&results);
                send_message(&mut stdin, &content)?;
            }
            WorkerMessage::Reset => unreachable!("handled above"),
        }

        let (response, new_sid, is_synthetic) =
            wait_for_result(&mut out_rx, Some((&inject_rx, &mut stdin)))?;

        // Update session ID if changed
        if let Some(sid) = new_sid
            && session_id.as_ref() != Some(&sid)
        {
            session_id = Some(sid.clone());
            if let Some(ref path) = cfg.session_file {
                save_session_id(path, &sid);
            }
        }

        // Feature 2: if Claude dropped text instead of calling send_message, inject a correction
        // on the next turn so Claude learns to use the tool instead.
        if let Some(ref dropped) = response.dropped_text {
            let correction = format!(
                "[SYSTEM] You output the following text directly instead of calling send_message:\n\
                \"{}\"\n\
                Always use the send_message tool to communicate. Plain text output is dropped.",
                dropped.chars().take(300).collect::<String>()
            );
            if let Err(e) = send_message(&mut stdin, &correction) {
                warn!("Failed to inject dropped-text correction: {}", e);
            }
        }

        // Circuit breaker: synthetic error = session overflow → auto-reset
        if is_synthetic {
            warn!("Synthetic error detected — auto-resetting Claude session");

            // Delete the corrupt session file so we start fresh
            if let Some(ref path) = cfg.session_file {
                match std::fs::remove_file(path) {
                    Ok(()) => info!("Deleted corrupt session file"),
                    Err(e) => warn!("Could not delete session file: {}", e),
                }
            }
            session_id = None;

            // Feature 1: take from guard, kill, then create a new guard below.
            // Drop stdin first to close the pipe cleanly.
            drop(stdin);
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            // out_rx will be reassigned below; the old reader thread exits naturally when its out_tx is dropped

            // Feature 3: log last stderr lines on crash for diagnostics.
            if let Ok(buf) = _stderr_buf.lock()
                && !buf.is_empty()
            {
                error!("Last {} stderr line(s) before synthetic reset:", buf.len());
                for line in buf.iter() {
                    error!("  stderr: {}", line);
                }
            }

            // Spawn a fresh process with no resume
            match setup_claude_process(
                &cfg.system_prompt,
                None,
                &cfg.session_file,
                &mut session_id,
                cfg.full_permissions,
                &cfg.tools_override,
                cfg.use_protected_write,
                &atomics,
            ) {
                Ok((new_proc, new_stdin, new_out_rx, new_stderr_buf)) => {
                    // Feature 1: wrap new process in ChildGuard.
                    guard = ChildGuard::new(new_proc);
                    stdin = new_stdin;
                    out_rx = new_out_rx;
                    _stderr_buf = new_stderr_buf;
                    info!("Claude session reset successfully");
                }
                Err(e) => {
                    error!("Failed to reset Claude session: {}", e);
                    return Err(e);
                }
            }

            // Return a synthetic Done so the engine exits cleanly for this message.
            // The user's next message will hit the fresh session.
            let reset_response = Response {
                tool_calls: vec![ToolCallWithId {
                    id: "circuit_breaker_0".to_string(),
                    call: ToolCall::Done,
                }],
                compacted: false,
                action: "stop".to_string(),
                reason: Some("circuit breaker reset".to_string()),
                sleep_ms: None,
                dropped_text: None,
            };
            if resp_tx.blocking_send(reset_response).is_err() {
                break;
            }
            continue;
        }

        if resp_tx.blocking_send(response).is_err() {
            break;
        }
    }

    info!("Claude Code worker shutting down");
    // Feature 1: drop stdin first to close the pipe, then let guard Drop kill/wait the process.
    drop(stdin);
    drop(guard);
    Ok(())
}

/// Single source of truth for Claude Code's `--tools` arg AND the runtime
/// `allowed_set` check that rejects unexpected tools. Review flagged the
/// prior duplication ("MUST mirror" comment) as a bug magnet — this fn is
/// the dedupe.
///
/// Tool matrix:
/// - `tools_override`  → whatever the operator configured
/// - Tier 2 (`full_permissions=false`) → `WebSearch` only
/// - Tier 1 + `use_protected_write=true` → `Read, WebSearch`
///   (**Bash + Edit + Write all dropped** so the MCP `protected_write` tool
///   is the only write path. Dropping Bash closes the `cat > /opt/nova/src/...`
///   bypass the /review security specialist flagged — with Bash available,
///   Nova could mint its own HMAC from guardian.key and ignore the
///   guardian entirely. No Bash ⇒ no shell-based bypass.)
/// - Tier 1 (default) → `Bash, Edit, Write, Read, WebSearch`
fn compute_allowed_tools(
    full_permissions: bool,
    use_protected_write: bool,
    tools_override: &Option<String>,
) -> String {
    if let Some(ov) = tools_override {
        return ov.clone();
    }
    if full_permissions && use_protected_write {
        return "Read,WebSearch".to_string();
    }
    if full_permissions {
        return "Bash,Edit,Write,Read,WebSearch".to_string();
    }
    "WebSearch".to_string()
}

/// Spawn the Claude Code subprocess. Takes the `use_protected_write` flag
/// so Nova's tool string can drop Bash/Edit/Write in favor of the MCP
/// `protected_write` shim — see `compute_allowed_tools` for the matrix.
fn spawn_process_with_guardian(
    system_prompt: &str,
    resume_session: Option<&str>,
    full_permissions: bool,
    tools_override: &Option<String>,
    use_protected_write: bool,
) -> Result<Child, String> {
    let schema: serde_json::Value =
        serde_json::from_str(TOOL_CALLS_SCHEMA).map_err(|e| format!("Bad schema: {}", e))?;
    let schema_str =
        serde_json::to_string(&schema).map_err(|e| format!("Failed to serialize schema: {}", e))?;

    let tools_string = compute_allowed_tools(full_permissions, use_protected_write, tools_override);
    let tools: &str = &tools_string;
    info!(
        "Claude Code tools: {} (full_permissions={}, override={:?})",
        tools, full_permissions, tools_override
    );

    let mut cmd = Command::new("claude");
    cmd.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        "sonnet",
        "--system-prompt",
        system_prompt,
        "--tools",
        tools,
        "--json-schema",
        &schema_str,
    ]);

    // Tier 1: skip permission prompts so Nova can work outside the project directory
    if full_permissions {
        cmd.arg("--dangerously-skip-permissions");
    }

    // Sandbox: resource limits for Claude Code subprocesses.
    // Prevents runaway processes from consuming all system resources.
    //
    // Limits MUST be generous enough that Claude Code can bootstrap cleanly.
    // A too-tight RLIMIT_NOFILE (previously 1024) broke macOS keychain auth
    // on the subprocess — keychain access opens a lot of fds for XPC
    // connections, MCP servers, config files, etc. When that ran out, auth
    // silently fell through to "Not logged in · Please run /login", which
    // Claude Code's schema enforcer then tried to wrap in a StructuredOutput
    // call, spinning a synthetic-response loop at ~1000/sec. Nova was
    // completely bricked on 2026-04-21 by this.
    //
    // Raised to match typical macOS defaults: NOFILE 10240, NPROC 2048.
    // Memory cap stays at 4 GB — runaway claude subprocesses were the
    // original motivation.
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            use libc::{RLIMIT_AS, RLIMIT_NOFILE, RLIMIT_NPROC, rlimit, setrlimit};
            let mem = rlimit {
                rlim_cur: 4_294_967_296,
                rlim_max: 4_294_967_296,
            }; // 4GB
            let files = rlimit {
                rlim_cur: 10_240,
                rlim_max: 10_240,
            };
            let procs = rlimit {
                rlim_cur: 2048,
                rlim_max: 2048,
            };
            let _ = setrlimit(RLIMIT_AS, &mem);
            let _ = setrlimit(RLIMIT_NOFILE, &files);
            let _ = setrlimit(RLIMIT_NPROC, &procs);
            Ok(())
        });
    }

    // Add --resume if we have a session to resume
    if let Some(session_id) = resume_session {
        info!("Resuming session: {}", session_id);
        cmd.args(["--resume", session_id]);
    }

    // Strip every Claude-Code-related env var inherited from the spawning
    // shell. If the harness is running inside another Claude Code session
    // (VS Code extension, nested CLI, or another claudir), these vars point
    // the subprocess at the WRONG claude binary (`CLAUDE_CODE_EXECPATH`),
    // signal nested-session detection (`CLAUDECODE`), and confuse the auth
    // lookup — producing the "Not logged in · Please run /login" synthetic
    // loop that bricked Nova on 2026-04-21.
    //
    // Nova's subprocess should always look up its own keychain login via
    // `/opt/homebrew/bin/claude`, uncontaminated by the parent session.
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_CODE_EXECPATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Spawn failed: {}", e))
}

fn send_message(stdin: &mut ChildStdin, content: &str) -> Result<(), String> {
    send_content(stdin, MessageContent::Text(content.to_string()))
}

fn send_message_with_image(
    stdin: &mut ChildStdin,
    text: &str,
    image_data: &[u8],
    media_type: &str,
) -> Result<(), String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(image_data);

    let content = MessageContent::MultiPart(vec![
        ContentPart::Text {
            text: text.to_string(),
        },
        ContentPart::Image {
            source: ImageSource {
                source_type: "base64".to_string(),
                media_type: media_type.to_string(),
                data: encoded,
            },
        },
    ]);

    send_content(stdin, content)
}

fn send_content(stdin: &mut ChildStdin, content: MessageContent) -> Result<(), String> {
    let msg = InputMessage {
        msg_type: "user".to_string(),
        message: InputContent {
            role: "user".to_string(),
            content,
        },
    };

    let json = serde_json::to_string(&msg).map_err(|e| format!("Serialize: {}", e))?;
    stdin
        .write_all(json.as_bytes())
        .map_err(|e| format!("Write: {}", e))?;
    stdin
        .write_all(b"\n")
        .map_err(|e| format!("Write newline: {}", e))?;
    stdin.flush().map_err(|e| format!("Flush: {}", e))?;

    let len = match &msg.message.content {
        MessageContent::Text(s) => s.len(),
        MessageContent::MultiPart(parts) => parts.len(),
    };
    debug!("Sent message (len={})", len);
    Ok(())
}

/// Wait for result. Returns (Response, Option<session_id>, is_synthetic).
/// `is_synthetic` is true when the session overflowed and the response is invalid.
/// When `inject` is provided, checks it every second for mid-turn messages and
/// writes them directly to Claude's stdin.
fn wait_for_result(
    out_rx: &mut std::sync::mpsc::Receiver<OutputMessage>,
    mut inject: Option<(&std::sync::mpsc::Receiver<String>, &mut ChildStdin)>,
) -> Result<(Response, Option<String>, bool), String> {
    // Guard against Claude Code's internal synthetic-response loop. Under
    // certain `--json-schema` enforcement failures, Claude Code keeps emitting
    // `{"model":"<synthetic>",...}` assistant messages + internal
    // "Stop hook feedback: You MUST call the ..." user messages back-to-back
    // WITHOUT ever emitting a {"type":"result"} — so this function would spin
    // forever, spewing ~1000 log lines/sec and burning CPU. Nova's startup
    // hit this consistently on 2026-04-21.
    //
    // Bail after N consecutive synthetic responses and return a fake Result
    // so the caller's existing `is_synthetic` circuit breaker fires and
    // resets the Claude session. Real responses reset the counter.
    const SYNTHETIC_LOOP_LIMIT: usize = 5;
    let mut synthetic_count: usize = 0;

    let mut compacted = false;
    let mut is_synthetic = false;
    // Feature 2: capture dropped text for the caller to act on.
    let mut dropped_text: Option<String> = None;

    loop {
        // When an inject channel is available, poll with a 1-second timeout so we can
        // forward mid-turn messages from the engine to Claude's stdin without blocking.
        let msg_opt = if inject.is_some() {
            // Try to receive with a short timeout to keep the loop responsive.
            // We re-borrow inject inside the match below to avoid borrow conflicts.
            match out_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(msg) => Some(msg),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Output channel closed".to_string());
                }
            }
        } else {
            match out_rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return Err("Output channel closed".to_string()),
            }
        };

        // On timeout: check inject channel and continue waiting.
        let msg = match msg_opt {
            None => {
                if let Some((inject_rx, stdin)) = inject.as_mut() {
                    while let Ok(text) = inject_rx.try_recv() {
                        info!("Mid-turn inject: {} chars", text.len());
                        if let Err(e) = send_message(stdin, &text) {
                            warn!("Failed to write inject to Claude stdin: {}", e);
                        }
                    }
                }
                continue;
            }
            Some(m) => m,
        };

        match msg {
            OutputMessage::Assistant { message } => {
                if let Some(msg) = message {
                    // Synthetic model = session overflow signal
                    if msg.model.as_deref() == Some(SYNTHETIC_MODEL) {
                        synthetic_count += 1;
                        is_synthetic = true;
                        warn!(
                            "Synthetic model detected — session overflowed ({}/{})",
                            synthetic_count, SYNTHETIC_LOOP_LIMIT
                        );
                        if synthetic_count >= SYNTHETIC_LOOP_LIMIT {
                            warn!(
                                "Claude Code stuck in synthetic loop — aborting turn, \
                                 caller will reset the session"
                            );
                            return Ok((
                                Response {
                                    tool_calls: Vec::new(),
                                    compacted: false,
                                    action: "stop".to_string(),
                                    reason: Some(
                                        "synthetic-response loop detected".to_string(),
                                    ),
                                    sleep_ms: None,
                                    dropped_text: None,
                                },
                                None,
                                true,
                            ));
                        }
                    } else {
                        // Real response — reset the counter.
                        synthetic_count = 0;
                    }
                    // Legacy compaction detection via context_management field
                    if let Some(ctx) = msg.context_management
                        && ctx.truncated_content_length.is_some()
                    {
                        warn!("Context compaction detected (legacy)!");
                        compacted = true;
                    }
                }
            }
            OutputMessage::System(sys) => {
                // Detect new-style compaction via system subtype:compact_boundary
                if sys.subtype.as_deref() == Some("compact_boundary") {
                    warn!("Context compaction detected (compact_boundary)!");
                    compacted = true;
                }
            }
            OutputMessage::Result {
                total_cost_usd,
                is_error,
                structured_output,
                session_id,
                result: result_text,
            } => {
                // DROPPED TEXT DETECTION (claudir architecture):
                // If Claude output text directly instead of calling send_message,
                // log it and store it so the worker can inject a correction next turn.
                if let Some(ref text) = result_text {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && trimmed.len() > 5 {
                        warn!(
                            "DROPPED TEXT detected ({} chars): \"{}\"",
                            trimmed.len(),
                            trimmed.chars().take(100).collect::<String>()
                        );
                        // Feature 2: store for re-injection as a correction message.
                        dropped_text = Some(trimmed.to_string());
                    }
                }
                if is_error {
                    warn!("Claude returned is_error:true — session error, treating as synthetic");
                    is_synthetic = true;
                }

                info!(
                    "🤖 Response (cost: ${:.4}{})",
                    total_cost_usd,
                    if is_synthetic { ", SYNTHETIC" } else { "" }
                );

                let (tool_calls, action, reason, sleep_ms) = match structured_output {
                    Some(so) => {
                        let calls = so
                            .tool_calls
                            .iter()
                            .enumerate()
                            .map(|(i, tc)| ToolCallWithId {
                                id: format!("tool_{}", i),
                                call: tc.to_tool_call(),
                            })
                            .collect();
                        (calls, so.action, so.reason, so.sleep_ms)
                    }
                    None => {
                        if !is_synthetic {
                            warn!("No structured output");
                        }
                        (Vec::new(), "stop".to_string(), None, None)
                    }
                };

                info!(
                    "Got {} tool call(s), action={}{}",
                    tool_calls.len(),
                    action,
                    if compacted { " (compacted)" } else { "" }
                );
                return Ok((
                    Response {
                        tool_calls,
                        compacted,
                        action,
                        reason,
                        sleep_ms,
                        dropped_text,
                    },
                    session_id,
                    is_synthetic,
                ));
            }
            OutputMessage::Other => continue,
        }
    }
}

fn format_tool_results(results: &[ToolResult]) -> String {
    let mut s = String::from("Tool results:\n");
    for r in results {
        let content = r.content.as_deref().unwrap_or("ok");
        s.push_str(&format!(
            "- {}: {}{}\n",
            r.tool_use_id,
            content,
            if r.is_error { " (ERROR)" } else { "" }
        ));
    }
    s
}
