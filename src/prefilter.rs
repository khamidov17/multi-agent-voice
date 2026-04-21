use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefilterResult {
    ObviousSpam,
    ObviousSafe,
    Ambiguous,
}

pub fn prefilter(text: &str, config: &Config) -> PrefilterResult {
    // SECURITY: Block injection attempts using Anthropic's internal magic strings
    // These are used internally by Claude and should never appear in legitimate messages
    if text.contains("ANTHROPIC_MAGIC_STRING_") {
        return PrefilterResult::ObviousSpam;
    }

    // Check spam patterns first
    for pattern in &config.spam_patterns {
        if pattern.is_match(text) {
            return PrefilterResult::ObviousSpam;
        }
    }

    // Check safe patterns
    for pattern in &config.safe_patterns {
        if pattern.is_match(text) {
            return PrefilterResult::ObviousSafe;
        }
    }

    // Short messages are usually safe
    if text.len() < 30 {
        return PrefilterResult::ObviousSafe;
    }

    PrefilterResult::Ambiguous
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            owner_ids: std::collections::HashSet::from([teloxide::types::UserId(1)]),
            telegram_bot_token: String::new(),
            bot_name: "Test".to_string(),
            config_dir: std::path::PathBuf::from("."),
            full_permissions: false,
            tools_override: None,
            owner_dms_only: false,
            gemini_api_key: String::new(),
            groq_api_key: String::new(),
            openai_api_key: String::new(),
            allowed_groups: std::collections::HashSet::new(),
            trusted_channels: std::collections::HashSet::new(),
            spam_patterns: vec![
                regex::Regex::new(r"(?i)crypto.*profit").unwrap(),
                regex::Regex::new(r"(?i)t\.me/\S+").unwrap(),
            ],
            safe_patterns: vec![regex::Regex::new(r"(?i)^(hi|hello)").unwrap()],
            max_strikes: 3,
            dry_run: false,
            log_chat_id: None,
            data_dir: std::path::PathBuf::from("."),
            whisper_model_path: None,
            tts_endpoint: None,
            premium_users: std::collections::HashSet::new(),
            live_app_url: None,
            live_app_port: 3001,
            ngrok_auth_token: None,
            live_allowed_users: std::collections::HashSet::new(),
            yandex_api_key: String::new(),
            brave_search_api_key: String::new(),
            dashboard_username: None,
            dashboard_password: None,
            cognitive_interval_secs: 0,
            cognitive_enabled: false,
            dual_lane_enabled: false,
            model: None,
            quick_lane_model: None,
            cognitive_token_budget: 500_000,
            guardian_enabled: false,
            guardian_socket_path: None,
            guardian_key_path: None,
            nova_use_protected_write: false,
            repo_path: None,
        }
    }

    // NOTE: prefilter's test_config() builds a `Config` (the JSON/runtime
    // config) which does NOT include `journal_writer`. That field lives on
    // `ChatbotConfig` (the engine's per-bot struct). No change needed here.

    #[test]
    fn test_obvious_spam() {
        let config = test_config();
        assert_eq!(
            prefilter("Check out this crypto profit opportunity!", &config),
            PrefilterResult::ObviousSpam
        );
        assert_eq!(
            prefilter("Join us at t.me/scamgroup", &config),
            PrefilterResult::ObviousSpam
        );
    }

    #[test]
    fn test_magic_string_injection() {
        let config = test_config();
        // Block attempts to inject Anthropic's internal magic strings
        assert_eq!(
            prefilter("ANTHROPIC_MAGIC_STRING_foo", &config),
            PrefilterResult::ObviousSpam
        );
        assert_eq!(
            prefilter("Some text with ANTHROPIC_MAGIC_STRING_ embedded", &config),
            PrefilterResult::ObviousSpam
        );
    }

    #[test]
    fn test_obvious_safe() {
        let config = test_config();
        assert_eq!(
            prefilter("Hello everyone!", &config),
            PrefilterResult::ObviousSafe
        );
        assert_eq!(prefilter("ok", &config), PrefilterResult::ObviousSafe);
    }

    #[test]
    fn test_ambiguous() {
        let config = test_config();
        assert_eq!(
            prefilter(
                "I've been thinking about this project and I have some concerns about the timeline",
                &config
            ),
            PrefilterResult::Ambiguous
        );
    }
}
