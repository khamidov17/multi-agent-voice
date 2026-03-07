//! Claude Code CLI - simple message relay with session persistence.
//!
//! Spawns a persistent Claude Code process and relays messages to it.
//! Claude Code maintains conversation history internally.
//! Uses --resume to continue previous sessions across restarts.
//!
//! SECURITY: Uses `--tools "WebSearch"` to allow only read-only web search.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

/// Sentinel model value Claude CLI uses when the session is corrupted/overflowed.
const SYNTHETIC_MODEL: &str = "<synthetic>";

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::tools::ToolCall;

/// JSON schema for structured output - tool_calls array.
const TOOL_CALLS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
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
  "required": ["tool_calls"]
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
}

/// Claude Code client - maintains persistent subprocess.
pub struct ClaudeCode {
    tx: mpsc::Sender<WorkerMessage>,
    rx: mpsc::Receiver<Response>,
}

enum WorkerMessage {
    UserMessage(String),
    /// Message with image: (text, image_data, media_type)
    ImageMessage(String, Vec<u8>, String),
    ToolResults(Vec<ToolResult>),
    /// Kill the subprocess and restart with a fresh session.
    Reset,
}

impl ClaudeCode {
    /// Start Claude Code, optionally resuming a previous session.
    /// If session_file exists, resume that session. Otherwise start fresh with system_prompt.
    pub fn start(system_prompt: String, session_file: Option<PathBuf>) -> Result<Self, String> {
        let (msg_tx, msg_rx) = mpsc::channel::<WorkerMessage>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<Response>(32);

        // Check for existing session
        let resume_session = session_file.as_ref().and_then(|p| load_session_id(p));

        std::thread::spawn(move || {
            if let Err(e) = worker_loop(system_prompt, resume_session, session_file, msg_rx, resp_tx) {
                error!("Claude Code worker died: {}", e);
            }
        });

        Ok(Self { tx: msg_tx, rx: resp_rx })
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
    pub async fn send_tool_results(&mut self, results: Vec<ToolResult>) -> Result<Response, String> {
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
        self.rx.recv().await.ok_or_else(|| "Response channel closed".to_string())?;
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
    tool_calls: Vec<RawToolCall>,
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
/// Returns (process, stdin, out_rx). Updates `session_id` in place.
fn setup_claude_process(
    system_prompt: &str,
    resume_session: Option<&str>,
    session_file: &Option<PathBuf>,
    session_id: &mut Option<String>,
) -> Result<(Child, ChildStdin, mpsc::Receiver<OutputMessage>), String> {
    let mut process = spawn_process(resume_session)?;
    let mut stdin = process.stdin.take().ok_or("No stdin")?;
    let stdout = process.stdout.take().ok_or("No stdout")?;
    let stderr = process.stderr.take();

    info!("🚀 Claude Code started (PID {})", process.id());

    // Stderr reader thread — logs errors from Claude CLI
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) if !l.is_empty() => error!("Claude CLI stderr: {}", l),
                    Err(e) => {
                        warn!("Stderr read error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    let (out_tx, mut out_rx) = mpsc::channel::<OutputMessage>(100);

    // Stdout reader thread
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

            match serde_json::from_str::<OutputMessage>(&line) {
                Ok(msg) => {
                    if out_tx.blocking_send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!("Parse error: {} (line: {})", e, preview);
                }
            }
        }
    });

    // Send first message to trigger Claude Code output
    let first_message = if resume_session.is_some() {
        "Session resumed. Ready for new messages.".to_string()
    } else {
        system_prompt.to_string()
    };
    send_message(&mut stdin, &first_message)?;

    // Wait for system message (comes first in output)
    loop {
        match out_rx.blocking_recv() {
            Some(OutputMessage::System(sys)) if sys.subtype.is_none() || sys.subtype.as_deref() == Some("init") => {
                let allowed = ["StructuredOutput", "WebSearch"];
                let unexpected: Vec<_> = sys.tools.iter().filter(|t| !allowed.contains(&t.as_str())).collect();
                if !unexpected.is_empty() {
                    error!("SECURITY: Unexpected tools: {:?}", unexpected);
                    return Err("Security violation".to_string());
                }
                if let Some(sid) = sys.session_id {
                    info!("Got session ID: {}", sid);
                    *session_id = Some(sid);
                }
                info!("🤖 Claude Code session ready");
                break;
            }
            Some(_) => continue,
            None => return Err("Output channel closed".to_string()),
        }
    }

    // Wait for result of first message (ignore synthetic flag — if init fails, bigger problem)
    let (_, new_sid, _) = wait_for_result(&mut out_rx)?;
    if let Some(sid) = new_sid {
        *session_id = Some(sid);
    }
    info!("First message processed, ready for chat");

    // Save session ID
    if let (Some(sid), Some(path)) = (session_id.as_ref(), session_file.as_ref()) {
        save_session_id(path, sid);
    }

    Ok((process, stdin, out_rx))
}

fn worker_loop(
    system_prompt: String,
    resume_session: Option<String>,
    session_file: Option<PathBuf>,
    mut msg_rx: mpsc::Receiver<WorkerMessage>,
    resp_tx: mpsc::Sender<Response>,
) -> Result<(), String> {
    let mut session_id: Option<String> = None;
    let (mut process, mut stdin, mut out_rx) =
        setup_claude_process(&system_prompt, resume_session.as_deref(), &session_file, &mut session_id)?;

    // Main loop
    while let Some(msg) = msg_rx.blocking_recv() {
        // Handle Reset before interacting with Claude subprocess
        if matches!(msg, WorkerMessage::Reset) {
            info!("🔄 Resetting Claude session (timeout recovery)");
            drop(stdin);
            let _ = process.kill();
            let _ = process.wait();
            if let Some(ref path) = session_file {
                match std::fs::remove_file(path) {
                    Ok(()) => info!("Deleted session file for reset"),
                    Err(e) => warn!("Could not delete session file: {e}"),
                }
            }
            session_id = None;
            match setup_claude_process(&system_prompt, None, &session_file, &mut session_id) {
                Ok((new_proc, new_stdin, new_out_rx)) => {
                    process = new_proc;
                    stdin = new_stdin;
                    out_rx = new_out_rx;
                    info!("✅ Claude session reset after timeout");
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

        let (response, new_sid, is_synthetic) = wait_for_result(&mut out_rx)?;

        // Update session ID if changed
        if let Some(sid) = new_sid
            && session_id.as_ref() != Some(&sid)
        {
            session_id = Some(sid.clone());
            if let Some(ref path) = session_file {
                save_session_id(path, &sid);
            }
        }

        // Circuit breaker: synthetic error = session overflow → auto-reset
        if is_synthetic {
            warn!("🔄 Synthetic error detected — auto-resetting Claude session");

            // Delete the corrupt session file so we start fresh
            if let Some(ref path) = session_file {
                match std::fs::remove_file(path) {
                    Ok(()) => info!("Deleted corrupt session file"),
                    Err(e) => warn!("Could not delete session file: {}", e),
                }
            }
            session_id = None;

            // Kill old process (drop stdin first to close the pipe cleanly)
            drop(stdin);
            let _ = process.kill();
            let _ = process.wait();
            // out_rx will be reassigned below; the old reader thread exits naturally when its out_tx is dropped

            // Spawn a fresh process with no resume
            match setup_claude_process(&system_prompt, None, &session_file, &mut session_id) {
                Ok((new_proc, new_stdin, new_out_rx)) => {
                    process = new_proc;
                    stdin = new_stdin;
                    out_rx = new_out_rx;
                    info!("✅ Claude session reset successfully");
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
    drop(stdin);
    let _ = process.wait();
    Ok(())
}

fn spawn_process(resume_session: Option<&str>) -> Result<Child, String> {
    let schema: serde_json::Value = serde_json::from_str(TOOL_CALLS_SCHEMA)
        .map_err(|e| format!("Bad schema: {}", e))?;
    let schema_str = serde_json::to_string(&schema)
        .map_err(|e| format!("Failed to serialize schema: {}", e))?;

    let mut cmd = Command::new("claude");
    cmd.args([
        "--print",
        "--input-format", "stream-json",
        "--output-format", "stream-json",
        "--verbose",
        "--model", "sonnet",
        "--tools", "WebSearch",  // SECURITY: only allow read-only web search
        // WebSearch auto-approved via .claude/settings.local.json on the server
        "--json-schema", &schema_str,
    ]);

    // Add --resume if we have a session to resume
    if let Some(session_id) = resume_session {
        info!("Resuming session: {}", session_id);
        cmd.args(["--resume", session_id]);
    }

    // Unset CLAUDECODE to allow spawning inside a supervisor Claude Code session
    cmd.env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Spawn failed: {}", e))
}

fn send_message(stdin: &mut ChildStdin, content: &str) -> Result<(), String> {
    send_content(stdin, MessageContent::Text(content.to_string()))
}

fn send_message_with_image(stdin: &mut ChildStdin, text: &str, image_data: &[u8], media_type: &str) -> Result<(), String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(image_data);

    let content = MessageContent::MultiPart(vec![
        ContentPart::Text { text: text.to_string() },
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
    stdin.write_all(json.as_bytes()).map_err(|e| format!("Write: {}", e))?;
    stdin.write_all(b"\n").map_err(|e| format!("Write newline: {}", e))?;
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
fn wait_for_result(out_rx: &mut mpsc::Receiver<OutputMessage>) -> Result<(Response, Option<String>, bool), String> {
    let mut compacted = false;
    let mut is_synthetic = false;

    loop {
        match out_rx.blocking_recv() {
            Some(OutputMessage::Assistant { message }) => {
                if let Some(msg) = message {
                    // Synthetic model = session overflow signal
                    if msg.model.as_deref() == Some(SYNTHETIC_MODEL) {
                        warn!("Synthetic model detected — session overflowed");
                        is_synthetic = true;
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
            Some(OutputMessage::System(sys)) => {
                // Detect new-style compaction via system subtype:compact_boundary
                if sys.subtype.as_deref() == Some("compact_boundary") {
                    warn!("Context compaction detected (compact_boundary)!");
                    compacted = true;
                }
            }
            Some(OutputMessage::Result { total_cost_usd, is_error, structured_output, session_id }) => {
                if is_error {
                    warn!("Claude returned is_error:true — session error, treating as synthetic");
                    is_synthetic = true;
                }

                info!("🤖 Response (cost: ${:.4}{})", total_cost_usd, if is_synthetic { ", SYNTHETIC" } else { "" });

                let tool_calls = match structured_output {
                    Some(so) => {
                        so.tool_calls
                            .iter()
                            .enumerate()
                            .map(|(i, tc)| ToolCallWithId {
                                id: format!("tool_{}", i),
                                call: tc.to_tool_call(),
                            })
                            .collect()
                    }
                    None => {
                        if !is_synthetic {
                            warn!("No structured output");
                        }
                        Vec::new()
                    }
                };

                info!("Got {} tool call(s){}", tool_calls.len(), if compacted { " (after compaction)" } else { "" });
                return Ok((Response { tool_calls, compacted }, session_id, is_synthetic));
            }
            Some(OutputMessage::Other) => continue,
            None => return Err("Output channel closed".to_string()),
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
