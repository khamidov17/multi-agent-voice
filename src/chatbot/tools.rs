//! Tool definitions for Claude to interact with the group.

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_script_timeout() -> u64 {
    60
}

/// Tool definition for Claude.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tool calls that Claude can make.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolCall {
    /// Send a message to a chat.
    SendMessage {
        /// Target chat ID (required - use the chat_id from the message you're responding to)
        chat_id: i64,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Get info about a user by ID or username.
    GetUserInfo {
        #[serde(skip_serializing_if = "Option::is_none")]
        user_id: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
    },

    /// Execute a SQL SELECT query on the database.
    Query {
        /// SQL SELECT query. Must start with SELECT. Max 100 rows returned.
        sql: String,
    },

    /// Add a reaction emoji to a message.
    AddReaction {
        /// Target chat ID (use the chat_id from the message you're reacting to)
        chat_id: i64,
        /// Message ID to react to
        message_id: i64,
        /// Emoji to react with (e.g. "👍", "❤", "🔥", "😂")
        emoji: String,
    },

    /// Delete a message (admin action - use for spam/abuse).
    DeleteMessage { chat_id: i64, message_id: i64 },

    /// Mute a user temporarily (admin action).
    MuteUser {
        chat_id: i64,
        user_id: i64,
        /// Duration in minutes (1-1440, i.e. up to 24 hours)
        duration_minutes: i64,
    },

    /// Ban a user permanently (admin action - use for severe abuse).
    BanUser { chat_id: i64, user_id: i64 },

    /// Kick a user from the group (softer than ban - they can rejoin).
    KickUser { chat_id: i64, user_id: i64 },

    /// Get list of chat administrators.
    GetChatAdmins { chat_id: i64 },

    /// Get list of known members from the database.
    GetMembers {
        /// Filter: "all", "active", "inactive", "never_posted", "left", "banned" (default "all")
        #[serde(default)]
        filter: Option<String>,
        /// For "inactive" filter: minimum days since last message (default 30)
        #[serde(default)]
        days_inactive: Option<i64>,
        /// Maximum users to return (default 50)
        #[serde(default)]
        limit: Option<i64>,
    },

    /// Import members from a JSON file (backfill from browser extension export).
    ImportMembers {
        /// Path to JSON file containing member array
        file_path: String,
    },

    /// Send an image to a chat.
    SendPhoto {
        /// Target chat ID
        chat_id: i64,
        /// Text prompt to generate or edit an AI image (uses Gemini/Nano Banana)
        prompt: String,
        /// Optional caption for the image
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
        /// Optional Telegram file_id of a source image to edit (enables image editing mode)
        #[serde(skip_serializing_if = "Option::is_none")]
        source_image_file_id: Option<String>,
    },

    /// Send a voice message (TTS).
    SendVoice {
        /// Target chat ID
        chat_id: i64,
        /// Text to convert to speech
        text: String,
        /// Optional voice name (default: "af_heart" - American English female)
        #[serde(skip_serializing_if = "Option::is_none")]
        voice: Option<String>,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    // === Memory Tools ===
    /// Create a new memory file. Fails if file already exists.
    CreateMemory {
        /// Relative path within memories directory (e.g. "users/nodir.md")
        path: String,
        /// Content to write
        content: String,
    },

    /// Read a memory file with line numbers.
    ReadMemory {
        /// Relative path within memories directory
        path: String,
    },

    /// Edit a memory file. Requires the file to have been read first.
    EditMemory {
        /// Relative path within memories directory
        path: String,
        /// Exact string to find and replace
        old_string: String,
        /// Replacement string
        new_string: String,
    },

    /// List files in the memories directory.
    ListMemories {
        /// Optional subdirectory path (default: root of memories)
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },

    /// Search for a pattern across memory files (like grep).
    SearchMemories {
        /// Search pattern (substring match)
        pattern: String,
        /// Optional subdirectory to search in
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },

    /// Delete a memory file.
    DeleteMemory {
        /// Relative path within memories directory
        path: String,
    },

    /// Fetch the content of a URL and return its text.
    FetchUrl {
        /// Full URL to fetch (https://...)
        url: String,
    },

    /// Generate music from a text prompt and send it to a chat.
    SendMusic {
        /// Target chat ID
        chat_id: i64,
        /// Text prompt describing the music to generate
        prompt: String,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Send an existing audio/document file from the filesystem to a chat.
    SendFile {
        chat_id: i64,
        /// Absolute path to the file on disk (e.g. "/Users/ava/Desktop/TestProject/output.wav")
        file_path: String,
        /// Optional caption
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Edit a previously sent message.
    EditMessage {
        chat_id: i64,
        message_id: i64,
        text: String,
    },

    /// Send a poll to a chat.
    SendPoll {
        chat_id: i64,
        question: String,
        options: Vec<String>,
        #[serde(default = "default_true")]
        is_anonymous: bool,
        #[serde(default)]
        allows_multiple_answers: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Unban a user from the group.
    UnbanUser { chat_id: i64, user_id: i64 },

    /// Schedule a reminder message.
    SetReminder {
        chat_id: i64,
        /// Message text to send when the reminder fires.
        message: String,
        /// When to fire: "+30m", "+2h", "+1d", "+1w", or "YYYY-MM-DD HH:MM" (UTC)
        trigger_at: String,
        /// Optional repeat expression: "+1d", "+1w", or "HH:MM" (daily at that time UTC)
        #[serde(skip_serializing_if = "Option::is_none")]
        repeat_cron: Option<String>,
    },

    /// List active reminders.
    ListReminders {
        /// Filter by chat_id, or None for all.
        #[serde(skip_serializing_if = "Option::is_none")]
        chat_id: Option<i64>,
    },

    /// Cancel a reminder by ID.
    CancelReminder { reminder_id: i64 },

    /// Geocode an address using Yandex (returns coordinates + display name).
    YandexGeocode { address: String },

    /// Send a static map image for an address using Yandex Maps.
    YandexMap {
        chat_id: i64,
        address: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Get the current server time, optionally in a UTC offset.
    Now {
        /// UTC offset in hours, e.g. 5 for UTC+5, -8 for UTC-8 (default 0 = UTC)
        #[serde(skip_serializing_if = "Option::is_none")]
        utc_offset: Option<i32>,
    },

    /// Report a bug or issue to the developer (Claude Code).
    ReportBug {
        /// Description of the bug or issue
        description: String,
        /// Severity: "low", "medium", "high", "critical"
        #[serde(default)]
        severity: Option<String>,
    },

    /// Create an Excel spreadsheet and send it to a chat.
    CreateSpreadsheet {
        /// Target chat ID
        chat_id: i64,
        /// Filename for the .xlsx file (e.g. "report.xlsx")
        filename: String,
        /// Array of sheet objects: [{name, headers, rows}]
        sheets: Vec<serde_json::Value>,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Create a PDF document and send it to a chat.
    CreatePdf {
        /// Target chat ID
        chat_id: i64,
        /// Filename for the .pdf file (e.g. "report.pdf")
        filename: String,
        /// HTML content to render as PDF
        content: String,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Create a Word document and send it to a chat.
    CreateWord {
        /// Target chat ID
        chat_id: i64,
        /// Filename for the .docx file (e.g. "report.docx")
        filename: String,
        /// Markdown content to convert to DOCX
        content: String,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Search the web using Brave Search API.
    WebSearch {
        /// The search query
        query: String,
        /// Target chat ID to send results to
        chat_id: i64,
        /// Optional message ID to reply to
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to_message_id: Option<i64>,
    },

    /// Run a script from workspace. Nova creates scripts, then executes them.
    /// Allows agents to build new capabilities at runtime.
    RunScript {
        /// Path to script (relative to project root, must be inside workspace/ or scripts/)
        path: String,
        /// Optional arguments
        #[serde(default)]
        args: Vec<String>,
        /// Timeout in seconds (default 60, max 300)
        #[serde(default = "default_script_timeout")]
        timeout: u64,
    },

    /// Run a Docker container for isolated execution.
    DockerRun {
        /// Path to docker-compose.yml or Dockerfile (relative to project root)
        compose_file: String,
        /// Action: "up", "down", "logs", "ps"
        action: String,
    },

    /// Run the generic evaluation suite (reads eval_config.yaml).
    RunEval {
        /// JSON variables to pass (e.g. {"anon_dir": "/path/to/output"})
        #[serde(default)]
        vars: String,
        /// Run all tests including optional (default: required only)
        #[serde(default)]
        all: bool,
    },

    /// Signal that processing is complete.
    Done,

    /// Parse error - tool call couldn't be parsed. Error message will be sent back to model.
    #[serde(skip)]
    ParseError { message: String },
}

/// Get the tool definitions for Claude.
pub fn get_tool_definitions() -> Vec<Tool> {
    vec![
        Tool {
            name: "send_message".to_string(),
            description: "Send a message to a chat. Use the chat_id from the message you're responding to.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": {
                        "type": "integer",
                        "description": "Target chat ID (use the chat_id from the incoming message)"
                    },
                    "text": {
                        "type": "string",
                        "description": "The message text to send"
                    },
                    "reply_to_message_id": {
                        "type": "integer",
                        "description": "Optional message ID to reply to"
                    }
                },
                "required": ["chat_id", "text"]
            }),
        },
        Tool {
            name: "get_user_info".to_string(),
            description: "Get detailed information about a user including their profile photo. Returns: user_id, username, first_name, last_name, is_bot, is_premium, language_code, status (owner/administrator/member/restricted/banned), custom_title, and profile_photo_base64. Username lookup only works for users seen in the group.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "integer",
                        "description": "The user ID to look up"
                    },
                    "username": {
                        "type": "string",
                        "description": "Username to look up (case-insensitive partial match)"
                    }
                }
            }),
        },
        Tool {
            name: "query".to_string(),
            description: "Execute a SQL SELECT query on the database. Tables: 'messages' (message_id, chat_id, user_id, username, timestamp, text, reply_to_id, reply_to_username, reply_to_text) and 'users' (user_id, username, first_name, join_date, last_message_date, message_count, status). Indexes exist on timestamp, user_id, username. Max 100 rows returned, text truncated to 100 chars.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "SQL SELECT query. Only SELECT is allowed. Examples: 'SELECT * FROM messages ORDER BY timestamp DESC LIMIT 10', 'SELECT username, message_count FROM users WHERE status = \"member\" ORDER BY message_count DESC LIMIT 20'"
                    }
                },
                "required": ["sql"]
            }),
        },
        Tool {
            name: "add_reaction".to_string(),
            description: "Add an emoji reaction to a message. Use sparingly - only when a reaction is more appropriate than a reply.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": {
                        "type": "integer",
                        "description": "Target chat ID (use the chat_id from the message)"
                    },
                    "message_id": {
                        "type": "integer",
                        "description": "Message ID to react to"
                    },
                    "emoji": {
                        "type": "string",
                        "description": "Emoji to react with (e.g. 👍, ❤, 🔥, 😂, 🎉, 👀, 🤔)"
                    }
                },
                "required": ["chat_id", "message_id", "emoji"]
            }),
        },
        Tool {
            name: "delete_message".to_string(),
            description: "Delete a message. Use for spam, abuse, or rule violations. Owner will be notified.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" },
                    "message_id": { "type": "integer", "description": "Message ID to delete" }
                },
                "required": ["chat_id", "message_id"]
            }),
        },
        Tool {
            name: "mute_user".to_string(),
            description: "Temporarily mute a user (prevent them from posting). Use for minor violations. Duration 1-1440 minutes. Owner will be notified.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" },
                    "user_id": { "type": "integer", "description": "User ID to mute" },
                    "duration_minutes": { "type": "integer", "description": "Duration in minutes (1-1440)" }
                },
                "required": ["chat_id", "user_id", "duration_minutes"]
            }),
        },
        Tool {
            name: "ban_user".to_string(),
            description: "Permanently ban a user. Use only for severe abuse (spam bots, repeated violations). Owner will be notified.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" },
                    "user_id": { "type": "integer", "description": "User ID to ban" }
                },
                "required": ["chat_id", "user_id"]
            }),
        },
        Tool {
            name: "kick_user".to_string(),
            description: "Kick a user from the group. Softer than ban - they can rejoin via invite link. Use for inactive members or minor issues. Owner will be notified.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" },
                    "user_id": { "type": "integer", "description": "User ID to kick" }
                },
                "required": ["chat_id", "user_id"]
            }),
        },
        Tool {
            name: "get_chat_admins".to_string(),
            description: "Get list of chat administrators.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" }
                },
                "required": ["chat_id"]
            }),
        },
        Tool {
            name: "get_members".to_string(),
            description: "Get list of known members from the database. Only includes members tracked since this feature was enabled.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "filter": {
                        "type": "string",
                        "description": "Filter: 'all', 'active', 'inactive', 'never_posted', 'left', 'banned' (default 'all')",
                        "enum": ["all", "active", "inactive", "never_posted", "left", "banned"]
                    },
                    "days_inactive": { "type": "integer", "description": "For 'inactive' filter: min days since last post (default 30)" },
                    "limit": { "type": "integer", "description": "Max users to return (default 50)" }
                }
            }),
        },
        Tool {
            name: "import_members".to_string(),
            description: "Import members from a JSON file (for backfilling from browser extension export). Only Nodir can use this.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Path to JSON file with member array" }
                },
                "required": ["file_path"]
            }),
        },
        Tool {
            name: "send_photo".to_string(),
            description: "Generate or edit an AI image and send it to a chat. Uses Gemini for image generation/editing. When source_image_file_id is provided, edits that image according to the prompt instead of generating from scratch.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "prompt": { "type": "string", "description": "Text prompt describing the image to generate, or editing instruction if source_image_file_id is provided" },
                    "caption": { "type": "string", "description": "Optional caption for the image" },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" },
                    "source_image_file_id": { "type": "string", "description": "Optional Telegram file_id of a source image to edit (from a photo in the chat). When provided, edits the source image using the prompt." }
                },
                "required": ["chat_id", "prompt"]
            }),
        },
        Tool {
            name: "send_voice".to_string(),
            description: "Send a voice message using text-to-speech. Use this to speak back when a user sends a voice message (match their medium), or for greetings, announcements, and personal moments. Powered by Gemini TTS — sounds natural and warm. Default voice is 'Kore' (warm female). Other options: 'Puck' (energetic male), 'Charon' (deep male), 'Fenrir' (expressive male), 'Aoede' (bright female), 'Leda' (soft female), 'Orus' (neutral).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "text": { "type": "string", "description": "Text to convert to speech. Keep concise for voice — 1-3 sentences is ideal." },
                    "voice": { "type": "string", "description": "Gemini voice name. Default: 'Kore' (warm female). Options: Kore, Puck, Charon, Fenrir, Aoede, Leda, Orus." },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "text"]
            }),
        },
        // === Memory Tools ===
        Tool {
            name: "create_memory".to_string(),
            description: "Create a new memory file. Fails if file already exists - use edit_memory to modify existing files.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within memories directory (e.g. 'users/nodir.md')" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"]
            }),
        },
        Tool {
            name: "read_memory".to_string(),
            description: "Read a memory file. Returns content with line numbers. Must read before editing.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within memories directory" }
                },
                "required": ["path"]
            }),
        },
        Tool {
            name: "edit_memory".to_string(),
            description: "Edit a memory file by replacing a string. File must have been read first in this session.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within memories directory" },
                    "old_string": { "type": "string", "description": "Exact string to find and replace" },
                    "new_string": { "type": "string", "description": "Replacement string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        Tool {
            name: "list_memories".to_string(),
            description: "List files in the memories directory.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional subdirectory path (default: root)" }
                }
            }),
        },
        Tool {
            name: "search_memories".to_string(),
            description: "Search for a pattern across memory files (like grep).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (substring match)" },
                    "path": { "type": "string", "description": "Optional subdirectory to search in" }
                },
                "required": ["pattern"]
            }),
        },
        Tool {
            name: "delete_memory".to_string(),
            description: "Delete a memory file.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within memories directory" }
                },
                "required": ["path"]
            }),
        },
        Tool {
            name: "fetch_url".to_string(),
            description: "Fetch text content from a URL. Use this when a user shares a link and wants you to read or summarize its content. Returns stripped text from the page (max ~8000 chars).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Full URL to fetch (must start with http:// or https://)"
                    }
                },
                "required": ["url"]
            }),
        },
        Tool {
            name: "send_file".to_string(),
            description: "Send an existing file from the filesystem to a Telegram chat. Use this to send audio files (WAV, OGG, MP3), documents, or any file from disk. The file must already exist on the server.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "file_path": { "type": "string", "description": "Absolute path to the file on disk (e.g. /Users/ava/Desktop/TestProject/output.wav)" },
                    "caption": { "type": "string", "description": "Optional caption for the file" },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "file_path"]
            }),
        },
        Tool {
            name: "send_music".to_string(),
            description: "Generate music from a text prompt using Gemini Lyria and send it to a chat. Use when a user asks for music or a song.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "prompt": { "type": "string", "description": "Text prompt describing the music (e.g. 'upbeat electronic dance music', 'calm acoustic guitar')" },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "prompt"]
            }),
        },
        Tool {
            name: "edit_message".to_string(),
            description: "Edit the text of a message Atlas previously sent.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" },
                    "message_id": { "type": "integer", "description": "ID of the message to edit" },
                    "text": { "type": "string", "description": "New text content (HTML formatting allowed)" }
                },
                "required": ["chat_id", "message_id", "text"]
            }),
        },
        Tool {
            name: "send_poll".to_string(),
            description: "Send a poll to a chat.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "question": { "type": "string", "description": "Poll question (max 300 chars)" },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "List of answer options (2-10 items)" },
                    "is_anonymous": { "type": "boolean", "description": "True = anonymous poll (default true)" },
                    "allows_multiple_answers": { "type": "boolean", "description": "Allow multiple selections (default false)" },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "question", "options"]
            }),
        },
        Tool {
            name: "unban_user".to_string(),
            description: "Unban a previously banned user from the group. Only affects users who are currently banned.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Chat ID" },
                    "user_id": { "type": "integer", "description": "User ID to unban" }
                },
                "required": ["chat_id", "user_id"]
            }),
        },
        Tool {
            name: "set_reminder".to_string(),
            description: "Schedule a message to be sent to a chat at a later time. Can be one-time or repeating.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "message": { "type": "string", "description": "Message text to send when reminder fires" },
                    "trigger_at": { "type": "string", "description": "When to fire: '+30m', '+2h', '+1d', '+1w' (relative), or 'YYYY-MM-DD HH:MM' (UTC absolute)" },
                    "repeat_cron": { "type": "string", "description": "Optional repeat: '+1d', '+1w', or 'HH:MM' for daily at that UTC time. Omit for one-time." }
                },
                "required": ["chat_id", "message", "trigger_at"]
            }),
        },
        Tool {
            name: "list_reminders".to_string(),
            description: "List all active reminders, optionally filtered by chat.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Filter by chat_id (optional, omit for all chats)" }
                }
            }),
        },
        Tool {
            name: "cancel_reminder".to_string(),
            description: "Cancel an active reminder by its ID.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "reminder_id": { "type": "integer", "description": "Reminder ID (from list_reminders or set_reminder response)" }
                },
                "required": ["reminder_id"]
            }),
        },
        Tool {
            name: "yandex_geocode".to_string(),
            description: "Geocode an address using Yandex Maps — returns coordinates and the official display name.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "Address or place name to geocode" }
                },
                "required": ["address"]
            }),
        },
        Tool {
            name: "yandex_map".to_string(),
            description: "Send a static map image for a given address using Yandex Static Maps.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "address": { "type": "string", "description": "Address or place to show on the map" },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "address"]
            }),
        },
        Tool {
            name: "now".to_string(),
            description: "Get the current server time. Useful for calculating reminder times or telling users the time.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "utc_offset": { "type": "integer", "description": "UTC offset in hours (e.g. 5 for UTC+5, -8 for UTC-8). Default 0 = UTC." }
                }
            }),
        },
        Tool {
            name: "create_spreadsheet".to_string(),
            description: "Create an Excel (.xlsx) spreadsheet and send it to a chat. Use when a user asks for a spreadsheet, table, or data in Excel format.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "filename": { "type": "string", "description": "Filename (e.g. 'report.xlsx'). Must end in .xlsx" },
                    "sheets": {
                        "type": "array",
                        "description": "Array of sheet objects",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Sheet tab name" },
                                "headers": { "type": "array", "items": { "type": "string" }, "description": "Column headers" },
                                "rows": { "type": "array", "items": { "type": "array" }, "description": "Data rows (each row is an array of values)" }
                            },
                            "required": ["name", "headers", "rows"]
                        }
                    },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "filename", "sheets"]
            }),
        },
        Tool {
            name: "create_pdf".to_string(),
            description: "Create a PDF document from HTML content and send it to a chat. Use when a user asks for a PDF report, document, or formatted output.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "filename": { "type": "string", "description": "Filename (e.g. 'report.pdf'). Must end in .pdf" },
                    "content": { "type": "string", "description": "HTML content to render as PDF. Use standard HTML with inline CSS for formatting." },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "filename", "content"]
            }),
        },
        Tool {
            name: "create_word".to_string(),
            description: "Create a Word (.docx) document from Markdown content and send it to a chat. Use when a user asks for a Word document or DOCX file.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": { "type": "integer", "description": "Target chat ID" },
                    "filename": { "type": "string", "description": "Filename (e.g. 'report.docx'). Must end in .docx" },
                    "content": { "type": "string", "description": "Markdown content to convert to DOCX. Supports headings, bold, italic, lists, tables." },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["chat_id", "filename", "content"]
            }),
        },
        Tool {
            name: "web_search".to_string(),
            description: "Search the web using Brave Search and send the results to a chat. Use when a user asks for current information, news, prices, or anything that requires up-to-date data.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query" },
                    "chat_id": { "type": "integer", "description": "Target chat ID to send results to" },
                    "reply_to_message_id": { "type": "integer", "description": "Optional message ID to reply to" }
                },
                "required": ["query", "chat_id"]
            }),
        },
        Tool {
            name: "report_bug".to_string(),
            description: "Report a bug or issue to the developer (Claude Code). Use this when you encounter unexpected behavior, errors, or problems you can't resolve. The developer monitors these reports and will fix issues.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "Detailed description of the bug or issue" },
                    "severity": { "type": "string", "description": "Severity level: low, medium, high, or critical" }
                },
                "required": ["description"]
            }),
        },
        Tool {
            name: "run_script".to_string(),
            description: "Execute a script file. Use this to run custom scripts you've created. Scripts must be inside workspace/ or scripts/ directory. Returns stdout/stderr and exit code. Timeout default 60s, max 300s.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to script (relative to project root)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Arguments to pass" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (default 60, max 300)" }
                },
                "required": ["path"]
            }),
        },
        Tool {
            name: "docker_run".to_string(),
            description: "Manage Docker containers for isolated execution. Actions: 'up' (start), 'down' (stop), 'logs' (view logs), 'ps' (list containers).".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "compose_file": { "type": "string", "description": "Path to docker-compose.yml" },
                    "action": { "type": "string", "enum": ["up", "down", "logs", "ps"], "description": "Docker action" }
                },
                "required": ["compose_file", "action"]
            }),
        },
        Tool {
            name: "run_eval".to_string(),
            description: "Run the evaluation suite defined in eval_config.yaml. Returns PASS/FAIL for each test. Sentinel uses this for all project types.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "vars": { "type": "string", "description": "JSON variables: {\"anon_dir\": \"/path\"}" },
                    "all": { "type": "boolean", "description": "Include optional tests (default: required only)" }
                }
            }),
        },
        Tool {
            name: "done".to_string(),
            description: "Legacy stop signal. PREFER using action='stop' with a reason field in the structured output instead. If you use this tool, it acts as action='stop'. In DMs, always send a message first. In groups, you MUST respond to teammate messages before stopping.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_call_serialize() {
        let call = ToolCall::SendMessage {
            chat_id: -12345,
            text: "hello".to_string(),
            reply_to_message_id: Some(123),
        };

        let json = serde_json::to_string(&call).unwrap();
        assert!(json.contains("send_message"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn test_tool_call_deserialize() {
        let json = r#"{"tool": "send_message", "chat_id": -12345, "text": "hello", "reply_to_message_id": 123}"#;
        let call: ToolCall = serde_json::from_str(json).unwrap();

        match call {
            ToolCall::SendMessage {
                chat_id,
                text,
                reply_to_message_id,
            } => {
                assert_eq!(chat_id, -12345);
                assert_eq!(text, "hello");
                assert_eq!(reply_to_message_id, Some(123));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_get_tool_definitions() {
        let tools = get_tool_definitions();
        assert_eq!(tools[0].name, "send_message");
        assert_eq!(tools[1].name, "get_user_info");
        assert_eq!(tools[2].name, "query");
        assert_eq!(tools[3].name, "add_reaction");
        assert_eq!(tools[4].name, "delete_message");
        assert_eq!(tools[5].name, "mute_user");
        assert_eq!(tools[6].name, "ban_user");
        assert_eq!(tools[7].name, "kick_user");
        assert_eq!(tools[8].name, "get_chat_admins");
        assert_eq!(tools[9].name, "get_members");
        assert_eq!(tools[10].name, "import_members");
        assert_eq!(tools[11].name, "send_photo");
        assert_eq!(tools[12].name, "send_voice");
        assert_eq!(tools[13].name, "create_memory");
        assert_eq!(tools[14].name, "read_memory");
        assert_eq!(tools[15].name, "edit_memory");
        assert_eq!(tools[16].name, "list_memories");
        assert_eq!(tools[17].name, "search_memories");
        assert_eq!(tools[18].name, "delete_memory");
        assert_eq!(tools[19].name, "fetch_url");
        assert_eq!(tools[20].name, "send_music");
        assert_eq!(tools[21].name, "edit_message");
        assert_eq!(tools[22].name, "send_poll");
        assert_eq!(tools[23].name, "unban_user");
        assert_eq!(tools[24].name, "set_reminder");
        assert_eq!(tools[25].name, "list_reminders");
        assert_eq!(tools[26].name, "cancel_reminder");
        assert_eq!(tools[27].name, "yandex_geocode");
        assert_eq!(tools[28].name, "yandex_map");
        assert_eq!(tools[29].name, "now");
        assert_eq!(tools[30].name, "create_spreadsheet");
        assert_eq!(tools[31].name, "create_pdf");
        assert_eq!(tools[32].name, "create_word");
        assert_eq!(tools[33].name, "web_search");
        assert_eq!(tools[34].name, "report_bug");
        assert_eq!(tools[35].name, "done");
        assert_eq!(tools.len(), 36);
    }
}
