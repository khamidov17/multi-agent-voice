use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use teloxide::types::{ChatId, UserId};

#[derive(Deserialize)]
struct ConfigFile {
    owner_ids: Vec<u64>,
    telegram_bot_token: String,
    /// Bot display name (e.g. "Atlas", "Nova", "Security").
    #[serde(default)]
    bot_name: Option<String>,
    /// Tier 1 (full permissions) vs Tier 2 (WebSearch only).
    /// When true, Claude Code gets Bash, Edit, Write, Read, WebSearch.
    /// When false (default), Claude Code gets WebSearch only.
    #[serde(default)]
    full_permissions: bool,
    /// Custom tool list override. When set, this is used instead of the
    /// full_permissions boolean. Example: "Bash,Read,WebSearch"
    #[serde(default)]
    tools: Option<String>,
    /// When true, only owner DMs and bot_xona group are accepted.
    /// Other users/groups are silently ignored.
    #[serde(default)]
    owner_dms_only: bool,
    /// Cognitive loop interval in seconds (default: 300 for Tier 1, 600 for Tier 2).
    /// Set to 0 to disable.
    #[serde(default)]
    cognitive_interval: Option<u64>,
    /// Enable/disable cognitive loop (default: true).
    #[serde(default = "default_true_config")]
    cognitive_enabled: bool,
    /// Gemini API key for image generation
    #[serde(default)]
    gemini_api_key: String,
    /// Groq API key for speech-to-text.
    #[serde(default)]
    groq_api_key: String,
    /// OpenAI API key for Whisper STT (preferred over Groq when set).
    #[serde(default)]
    openai_api_key: String,
    #[serde(default)]
    allowed_groups: Vec<i64>,
    #[serde(default)]
    trusted_channels: Vec<i64>,
    #[serde(default)]
    spam_patterns: Vec<String>,
    #[serde(default)]
    safe_patterns: Vec<String>,
    #[serde(default = "default_max_strikes")]
    max_strikes: u8,
    #[serde(default)]
    dry_run: bool,
    log_chat_id: Option<i64>,
    /// Directory for state files (logs, context). Defaults to current directory.
    data_dir: Option<String>,
    /// Path to Whisper model file (.bin) for voice transcription.
    whisper_model_path: Option<String>,
    /// TTS endpoint for Kokoro-FastAPI (e.g., "http://localhost:8880").
    tts_endpoint: Option<String>,
    /// Premium user IDs (unlimited messages, no rate limit).
    #[serde(default)]
    premium_users: Vec<u64>,
    /// Public HTTPS URL for the Gemini Live mini app (e.g. "https://atlas.example.com").
    live_app_url: Option<String>,
    /// Port for the built-in HTTP server serving the live mini app (default 3001).
    #[serde(default = "default_live_app_port")]
    live_app_port: u16,
    /// ngrok auth token — if set, ngrok is started automatically to tunnel live_app_port.
    /// The resulting HTTPS URL is used as live_app_url (overrides the config value).
    ngrok_auth_token: Option<String>,
    /// Additional user IDs allowed to use /live (besides owners).
    #[serde(default)]
    live_allowed_users: Vec<u64>,
    /// Yandex API key for geocoding and static maps.
    #[serde(default)]
    yandex_api_key: String,
    /// Brave Search API key for web search tool.
    #[serde(default)]
    brave_search_api_key: String,
    /// Dashboard username (required to access /dashboard).
    dashboard_username: Option<String>,
    /// Dashboard password (required to access /dashboard).
    dashboard_password: Option<String>,
    /// Enable dual-lane processing (deep work + quick response). Default: true.
    #[serde(default = "default_true_config")]
    dual_lane_enabled: bool,
    /// Deep-lane Claude model to pass to `claude --model`. Values Claude Code
    /// understands: `opus`, `sonnet`, `haiku`, or a full model id. Defaults
    /// to None here (plumbed through to `start_with_guardian`, which falls
    /// back to `opus`). nova.json sets `"model":"opus"`; prior to 2026-04-21
    /// this field was silently dropped because the struct didn't declare it.
    #[serde(default)]
    model: Option<String>,
    /// Model override for the quick response lane (e.g. "claude-haiku-4-5").
    /// When None, the quick lane uses the same model as the deep lane.
    #[serde(default)]
    quick_lane_model: Option<String>,
    /// Daily token budget for cognitive loop (default 500000).
    #[serde(default = "default_cognitive_token_budget")]
    cognitive_token_budget: u64,

    // ---- Phase 0: Bootstrap guardian integration ----
    // When `guardian_enabled` is true and guardian_socket_path + guardian_key_path
    // both resolve to existing files, the harness constructs a `GuardianClient`
    // at startup and wires it into the MCP tool dispatch. If any of those are
    // missing, the harness still runs (guardian is opt-in) but the MCP
    // `protected_write` tool will return an error if Nova attempts to use it.
    #[serde(default)]
    guardian_enabled: bool,
    /// Absolute path to the guardian's UDS socket.
    /// Defaults to `/opt/trio/run/bootstrap-guardian.sock` in prod-like
    /// environments; set explicitly for dev.
    #[serde(default)]
    guardian_socket_path: Option<String>,
    /// Absolute path to the guardian's shared HMAC key (mode 0400, >=32 bytes).
    /// When None, the harness cannot sign requests and `protected_write`
    /// is disabled.
    #[serde(default)]
    guardian_key_path: Option<String>,
    /// When true AND guardian_enabled is true, Nova's Claude Code tool
    /// string drops `Edit` and `Write` in favor of the MCP `protected_write`
    /// tool. When false (default), Nova keeps Edit/Write — this is the
    /// shadow-mode flag for the 48h cutover period.
    ///
    /// Not yet wired to Nova's CC spawn args — tool-string removal is the
    /// next Phase 0 slice.
    #[serde(default)]
    nova_use_protected_write: bool,
    /// Phase 3 — path to the main source-code clone for worktree
    /// creation. Defaults to `std::env::current_dir()` at boot when
    /// unset, which is the usual "run trio from inside the repo"
    /// pattern. Set explicitly when the harness runs from a deploy
    /// location distinct from the source.
    #[serde(default)]
    repo_path: Option<String>,
}

fn default_cognitive_token_budget() -> u64 {
    500_000
}

fn default_max_strikes() -> u8 {
    3
}

fn default_true_config() -> bool {
    true
}

fn default_live_app_port() -> u16 {
    3001
}

pub struct Config {
    pub owner_ids: HashSet<UserId>,
    pub telegram_bot_token: String,
    /// Bot display name (e.g. "Atlas", "Nova", "Security").
    pub bot_name: String,
    /// Absolute path to the directory that contains the config JSON file.
    /// Used by the /kill handler to write the kill marker where the wrapper
    /// can find it without needing to parse JSON.
    pub config_dir: PathBuf,
    /// Tier 1 = full Claude Code permissions; Tier 2 = WebSearch only.
    pub full_permissions: bool,
    /// Custom tool list override (e.g. "Bash,Read,WebSearch" for Sentinel eval).
    pub tools_override: Option<String>,
    /// When true, only owner DMs and allowed groups are accepted.
    pub owner_dms_only: bool,
    pub gemini_api_key: String,
    /// Groq API key for speech-to-text.
    pub groq_api_key: String,
    /// OpenAI API key for Whisper STT (preferred over Groq when set).
    pub openai_api_key: String,
    pub allowed_groups: HashSet<ChatId>,
    pub trusted_channels: HashSet<ChatId>,
    pub spam_patterns: Vec<Regex>,
    pub safe_patterns: Vec<Regex>,
    pub max_strikes: u8,
    pub dry_run: bool,
    pub log_chat_id: Option<ChatId>,
    /// Directory for state files (logs, context).
    pub data_dir: PathBuf,
    /// Path to Whisper model file (.bin) for voice transcription.
    pub whisper_model_path: Option<PathBuf>,
    /// TTS endpoint for Kokoro-FastAPI (e.g., "http://localhost:8880").
    pub tts_endpoint: Option<String>,
    /// Premium user IDs (unlimited DM messages).
    pub premium_users: HashSet<UserId>,
    /// Public HTTPS URL for the Gemini Live mini app.
    pub live_app_url: Option<String>,
    /// Port for the built-in HTTP server (default 3001).
    pub live_app_port: u16,
    /// ngrok auth token for auto-tunneling.
    pub ngrok_auth_token: Option<String>,
    /// User IDs allowed to use /live (owners + this list).
    pub live_allowed_users: HashSet<UserId>,
    /// Yandex API key for geocoding and static maps.
    pub yandex_api_key: String,
    /// Brave Search API key for web search tool.
    pub brave_search_api_key: String,
    /// Dashboard credentials (loaded from config, never hardcoded).
    pub dashboard_username: Option<String>,
    pub dashboard_password: Option<String>,
    /// Cognitive loop interval in seconds. 0 = disabled.
    pub cognitive_interval_secs: u64,
    /// Whether cognitive loop is enabled.
    pub cognitive_enabled: bool,
    /// Enable dual-lane processing (deep work + quick response).
    pub dual_lane_enabled: bool,
    /// Deep-lane Claude model (value for `claude --model`).
    pub model: Option<String>,
    /// Model override for the quick response lane.
    pub quick_lane_model: Option<String>,
    /// Daily token budget for cognitive loop.
    pub cognitive_token_budget: u64,

    // ---- Phase 0: Bootstrap guardian integration ----
    /// Whether to try to attach a `GuardianClient` at startup. When true
    /// AND socket/key paths are valid, the client is constructed and
    /// exposed for the MCP `protected_write` tool. When false, the MCP
    /// tool is unregistered and `Nova.tool_list` is unchanged.
    pub guardian_enabled: bool,
    /// Absolute path to the guardian's UDS socket. None → unconfigured.
    pub guardian_socket_path: Option<PathBuf>,
    /// Absolute path to the guardian's HMAC key. None → unconfigured.
    pub guardian_key_path: Option<PathBuf>,
    /// When true, Nova's Claude Code tool string drops Edit/Write in favor
    /// of `protected_write`. Implemented separately from guardian_enabled
    /// so Phase 0 can ship the guardian first with Nova unchanged, then
    /// flip this to true in a follow-up PR. Shadow-mode cutover knob.
    pub nova_use_protected_write: bool,
    /// Phase 3 — path to the main source-code clone. None = use cwd at
    /// boot. Used by the worktree manager for per-plan implementation
    /// mode; no effect on bots without `full_permissions=true`.
    pub repo_path: Option<PathBuf>,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Self {
        let config_dir = path
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| path.as_ref().to_path_buf())
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let content = std::fs::read_to_string(path.as_ref()).expect("Failed to read config file");
        let file: ConfigFile = serde_json::from_str(&content).expect("Failed to parse config file");

        let owner_ids = file.owner_ids.into_iter().map(UserId).collect();
        let allowed_groups = file.allowed_groups.into_iter().map(ChatId).collect();
        let trusted_channels = file.trusted_channels.into_iter().map(ChatId).collect();

        let spam_patterns = if file.spam_patterns.is_empty() {
            default_spam_patterns()
        } else {
            file.spam_patterns
                .into_iter()
                .map(|p| Regex::new(&p).expect("Invalid spam pattern regex"))
                .collect()
        };

        let safe_patterns = if file.safe_patterns.is_empty() {
            default_safe_patterns()
        } else {
            file.safe_patterns
                .into_iter()
                .map(|p| Regex::new(&p).expect("Invalid safe pattern regex"))
                .collect()
        };

        let data_dir = file
            .data_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            owner_ids,
            telegram_bot_token: file.telegram_bot_token,
            bot_name: file.bot_name.unwrap_or_else(|| "Atlas".to_string()),
            config_dir,
            full_permissions: file.full_permissions,
            tools_override: file.tools,
            owner_dms_only: file.owner_dms_only,
            gemini_api_key: file.gemini_api_key,
            groq_api_key: file.groq_api_key,
            openai_api_key: file.openai_api_key,
            allowed_groups,
            trusted_channels,
            spam_patterns,
            safe_patterns,
            max_strikes: file.max_strikes,
            dry_run: file.dry_run,
            log_chat_id: file.log_chat_id.map(ChatId),
            data_dir,
            whisper_model_path: file.whisper_model_path.map(PathBuf::from),
            tts_endpoint: file.tts_endpoint,
            premium_users: file.premium_users.into_iter().map(UserId).collect(),
            live_app_url: file.live_app_url,
            live_app_port: file.live_app_port,
            ngrok_auth_token: file.ngrok_auth_token,
            live_allowed_users: file.live_allowed_users.into_iter().map(UserId).collect(),
            yandex_api_key: file.yandex_api_key,
            brave_search_api_key: file.brave_search_api_key,
            dashboard_username: file.dashboard_username,
            dashboard_password: file.dashboard_password,
            cognitive_interval_secs: file.cognitive_interval.unwrap_or(if file.full_permissions {
                300
            } else {
                600
            }),
            cognitive_enabled: file.cognitive_enabled,
            dual_lane_enabled: file.dual_lane_enabled,
            model: file.model,
            quick_lane_model: file.quick_lane_model,
            cognitive_token_budget: file.cognitive_token_budget,
            guardian_enabled: file.guardian_enabled,
            guardian_socket_path: file.guardian_socket_path.map(PathBuf::from),
            guardian_key_path: file.guardian_key_path.map(PathBuf::from),
            nova_use_protected_write: file.nova_use_protected_write,
            repo_path: file.repo_path.map(PathBuf::from),
        }
    }

    pub fn can_use_live(&self, user_id: UserId) -> bool {
        self.owner_ids.contains(&user_id) || self.live_allowed_users.contains(&user_id)
    }

    pub fn is_owner(&self, user_id: UserId) -> bool {
        self.owner_ids.contains(&user_id)
    }

    pub fn is_premium(&self, user_id: UserId) -> bool {
        self.owner_ids.contains(&user_id) || self.premium_users.contains(&user_id)
    }

    pub fn is_trusted_channel(&self, chat_id: ChatId) -> bool {
        self.trusted_channels.contains(&chat_id)
    }
}

fn default_spam_patterns() -> Vec<Regex> {
    vec![
        r"(?i)crypto.*profit",
        r"(?i)earn.*\$\d+.*day",
        r"(?i)click.*link.*bio",
        r"(?i)dm.*me.*for",
        r"(?i)investment.*opportunity",
        r"(?i)make.*money.*fast",
        r"(?i)forex.*trading",
        r"(?i)t\.me/\S+",
    ]
    .into_iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
}

fn default_safe_patterns() -> Vec<Regex> {
    vec![r"^[^a-zA-Z]*$", r"^\S{1,20}$", r"(?i)^(hi|hello|thanks)"]
        .into_iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
}
