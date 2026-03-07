use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use teloxide::types::{ChatId, UserId};

#[derive(Deserialize)]
struct ConfigFile {
    owner_ids: Vec<u64>,
    telegram_bot_token: String,
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
    /// Public HTTPS URL for the Gemini Live mini app (e.g. "https://nemo.example.com").
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
}

fn default_max_strikes() -> u8 {
    3
}

fn default_live_app_port() -> u16 {
    3001
}

pub struct Config {
    pub owner_ids: HashSet<UserId>,
    pub telegram_bot_token: String,
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
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Self {
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
