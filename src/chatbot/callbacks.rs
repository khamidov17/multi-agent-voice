//! Before/after callback pipeline — intercepts every tool execution.
//!
//! Inspired by Google ADK's before_model_callback / after_model_callback.
//! Callbacks run synchronously and must be fast (< 1ms). They check conditions
//! and transform data — no I/O, no async, no database queries.
//!
//! Built-in callbacks:
//! - **RedactCallback**: strips API keys, tokens, passwords from outgoing messages
//! - **MessageLengthCallback**: truncates messages exceeding Telegram's 4096-char limit
//! - **RateLimitCallback**: blocks tool calls exceeding N per minute (atomic counters)

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::chatbot::claude_code::ToolResult;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::tools::ToolCall;

// ─── Callback traits ────────────────────────────────────────────────────

/// Result of a before-callback.
pub enum BeforeAction {
    /// Allow the tool call to proceed unchanged.
    Allow,
    /// Allow but with a modified tool call (e.g., redacted parameters).
    Modify(ToolCall),
    /// Block the tool call entirely. Return this error to the LLM.
    Block(String),
}

/// Result of an after-callback.
pub enum AfterAction {
    /// Return the tool result unchanged.
    PassThrough,
    /// Replace the tool result with this modified version.
    Replace(ToolResult),
}

/// A callback that runs BEFORE a tool is executed.
pub trait BeforeToolCallback: Send + Sync {
    fn name(&self) -> &str;
    fn before_tool(&self, tool_call: &ToolCall, config: &ChatbotConfig) -> BeforeAction;
}

/// A callback that runs AFTER a tool is executed.
pub trait AfterToolCallback: Send + Sync {
    fn name(&self) -> &str;
    fn after_tool(
        &self,
        tool_call: &ToolCall,
        result: &ToolResult,
        config: &ChatbotConfig,
    ) -> AfterAction;
}

// ─── Pipeline ───────────────────────────────────────────────────────────

/// Ordered pipeline of before/after callbacks wrapping every tool execution.
#[derive(Default)]
pub struct CallbackPipeline {
    before: Vec<Box<dyn BeforeToolCallback>>,
    after: Vec<Box<dyn AfterToolCallback>>,
}

impl CallbackPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the default pipeline with all built-in callbacks.
    pub fn default_pipeline() -> Self {
        let mut p = Self::new();
        p.add_before(Box::new(RedactCallback));
        p.add_before(Box::new(MessageLengthCallback { max_chars: 4000 }));
        p.add_before(Box::new(RateLimitCallback::new(120))); // 120 calls/min
        p
    }

    pub fn add_before(&mut self, cb: Box<dyn BeforeToolCallback>) {
        self.before.push(cb);
    }

    pub fn add_after(&mut self, cb: Box<dyn AfterToolCallback>) {
        self.after.push(cb);
    }

    /// Run all before-callbacks. Returns the (possibly modified) tool call,
    /// or an error string if any callback blocked it.
    pub fn run_before(
        &self,
        tool_call: &ToolCall,
        config: &ChatbotConfig,
    ) -> Result<ToolCall, String> {
        let mut current = tool_call.clone();
        for cb in &self.before {
            match cb.before_tool(&current, config) {
                BeforeAction::Allow => {}
                BeforeAction::Modify(modified) => {
                    tracing::debug!("Callback '{}' modified tool call", cb.name());
                    current = modified;
                }
                BeforeAction::Block(reason) => {
                    tracing::warn!("Callback '{}' BLOCKED tool call: {}", cb.name(), reason);
                    return Err(reason);
                }
            }
        }
        Ok(current)
    }

    /// Run all after-callbacks. Returns the (possibly modified) result.
    pub fn run_after(
        &self,
        tool_call: &ToolCall,
        result: ToolResult,
        config: &ChatbotConfig,
    ) -> ToolResult {
        let mut current = result;
        for cb in &self.after {
            match cb.after_tool(tool_call, &current, config) {
                AfterAction::PassThrough => {}
                AfterAction::Replace(new_result) => {
                    tracing::debug!("After-callback '{}' modified result", cb.name());
                    current = new_result;
                }
            }
        }
        current
    }
}

// ─── Built-in: Sensitive data redaction ─────────────────────────────────

/// Strips API keys, tokens, passwords from outgoing SendMessage text.
pub struct RedactCallback;

impl BeforeToolCallback for RedactCallback {
    fn name(&self) -> &str {
        "redact"
    }
    fn before_tool(&self, tc: &ToolCall, _config: &ChatbotConfig) -> BeforeAction {
        if let ToolCall::SendMessage {
            chat_id,
            text,
            reply_to_message_id,
        } = tc
        {
            let redacted = redact_sensitive(text);
            if redacted != *text {
                return BeforeAction::Modify(ToolCall::SendMessage {
                    chat_id: *chat_id,
                    text: redacted,
                    reply_to_message_id: *reply_to_message_id,
                });
            }
        }
        BeforeAction::Allow
    }
}

/// Redact patterns that look like secrets.
fn redact_sensitive(text: &str) -> String {
    // Compile-once via lazy static pattern — these are called per tool call
    static PATTERNS: std::sync::LazyLock<Vec<(regex::Regex, &'static str)>> =
        std::sync::LazyLock::new(|| {
            vec![
                // Generic key=value patterns
                (
                    regex::Regex::new(
                        r"(?i)(api[_-]?key|token|password|secret|credential)\s*[:=]\s*\S+",
                    )
                    .unwrap(),
                    "$1=***REDACTED***",
                ),
                // OpenAI / Anthropic keys
                (
                    regex::Regex::new(r"sk-[a-zA-Z0-9]{20,}").unwrap(),
                    "sk-***REDACTED***",
                ),
                // GitHub PATs
                (
                    regex::Regex::new(r"ghp_[a-zA-Z0-9]{36}").unwrap(),
                    "ghp_***REDACTED***",
                ),
                // Telegram bot tokens
                (
                    regex::Regex::new(r"\d{8,10}:[A-Za-z0-9_-]{35}").unwrap(),
                    "***BOT_TOKEN_REDACTED***",
                ),
                // AWS keys
                (
                    regex::Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                    "***AWS_KEY_REDACTED***",
                ),
            ]
        });

    let mut result = text.to_string();
    for (re, replacement) in PATTERNS.iter() {
        result = re.replace_all(&result, *replacement).to_string();
    }
    result
}

// ─── Built-in: Message length guard ─────────────────────────────────────

/// Truncates outgoing messages exceeding the configured character limit.
/// Telegram's limit is 4096; we default to 4000 to leave room for formatting.
pub struct MessageLengthCallback {
    pub max_chars: usize,
}

impl BeforeToolCallback for MessageLengthCallback {
    fn name(&self) -> &str {
        "message_length"
    }
    fn before_tool(&self, tc: &ToolCall, _config: &ChatbotConfig) -> BeforeAction {
        if let ToolCall::SendMessage {
            chat_id,
            text,
            reply_to_message_id,
        } = tc
            && text.len() > self.max_chars
        {
            // Truncate at char boundary
            let mut end = self.max_chars.saturating_sub(50);
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            let truncated = format!(
                "{}...\n\n[truncated — {} chars total]",
                &text[..end],
                text.len()
            );
            return BeforeAction::Modify(ToolCall::SendMessage {
                chat_id: *chat_id,
                text: truncated,
                reply_to_message_id: *reply_to_message_id,
            });
        }
        BeforeAction::Allow
    }
}

// ─── Built-in: Rate limiter ─────────────────────────────────────────────

/// Blocks tool calls if the bot exceeds N calls per minute.
/// Uses atomics — no locks, no I/O.
pub struct RateLimitCallback {
    max_per_minute: u32,
    counter: AtomicU32,
    window_start: AtomicU64,
}

impl RateLimitCallback {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            counter: AtomicU32::new(0),
            window_start: AtomicU64::new(current_epoch_secs()),
        }
    }
}

impl BeforeToolCallback for RateLimitCallback {
    fn name(&self) -> &str {
        "rate_limit"
    }
    fn before_tool(&self, _tc: &ToolCall, _config: &ChatbotConfig) -> BeforeAction {
        let now = current_epoch_secs();
        let window = self.window_start.load(Ordering::Relaxed);
        if now.saturating_sub(window) > 60 {
            // New window
            self.counter.store(1, Ordering::Relaxed);
            self.window_start.store(now, Ordering::Relaxed);
            return BeforeAction::Allow;
        }
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        if count >= self.max_per_minute {
            BeforeAction::Block(format!(
                "Rate limit: {} tool calls/minute exceeded. Wait before retrying.",
                self.max_per_minute
            ))
        } else {
            BeforeAction::Allow
        }
    }
}

fn current_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ChatbotConfig {
        ChatbotConfig::default()
    }

    #[test]
    fn test_redact_api_keys() {
        let text = "Here's my key: api_key=sk-abc123456789012345678901234567890";
        let redacted = redact_sensitive(text);
        assert!(!redacted.contains("sk-abc123"));
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn test_redact_github_token() {
        let text = "Use ghp_abcdefghijklmnopqrstuvwxyz0123456789 for auth";
        let redacted = redact_sensitive(text);
        assert!(!redacted.contains("ghp_abcdefg"));
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn test_redact_telegram_token() {
        let text = "Bot token: 1234567890:ABCdefGHI-jklMNOpqrSTUvwxYZ123456789";
        let redacted = redact_sensitive(text);
        assert!(!redacted.contains("ABCdefGHI"));
        assert!(redacted.contains("REDACTED"));
    }

    #[test]
    fn test_redact_preserves_safe_text() {
        let text = "Hello everyone, how are you doing today?";
        let redacted = redact_sensitive(text);
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_message_length_callback_truncates() {
        let cb = MessageLengthCallback { max_chars: 100 };
        let long_text = "a".repeat(200);
        let tc = ToolCall::SendMessage {
            chat_id: -12345,
            text: long_text,
            reply_to_message_id: None,
        };
        match cb.before_tool(&tc, &test_config()) {
            BeforeAction::Modify(ToolCall::SendMessage { text, .. }) => {
                assert!(text.len() < 200);
                assert!(text.contains("truncated"));
                assert!(text.contains("200 chars total"));
            }
            _ => panic!("Expected Modify"),
        }
    }

    #[test]
    fn test_message_length_callback_passes_short() {
        let cb = MessageLengthCallback { max_chars: 4000 };
        let tc = ToolCall::SendMessage {
            chat_id: -12345,
            text: "Short message".to_string(),
            reply_to_message_id: None,
        };
        assert!(matches!(
            cb.before_tool(&tc, &test_config()),
            BeforeAction::Allow
        ));
    }

    #[test]
    fn test_rate_limit_allows_under_limit() {
        let cb = RateLimitCallback::new(10);
        let tc = ToolCall::SendMessage {
            chat_id: -12345,
            text: "hi".to_string(),
            reply_to_message_id: None,
        };
        let config = test_config();
        for _ in 0..9 {
            assert!(matches!(cb.before_tool(&tc, &config), BeforeAction::Allow));
        }
    }

    #[test]
    fn test_rate_limit_blocks_over_limit() {
        let cb = RateLimitCallback::new(3);
        let tc = ToolCall::SendMessage {
            chat_id: -12345,
            text: "hi".to_string(),
            reply_to_message_id: None,
        };
        let config = test_config();
        // Use up the limit
        for _ in 0..3 {
            cb.before_tool(&tc, &config);
        }
        // Next should be blocked
        assert!(matches!(
            cb.before_tool(&tc, &config),
            BeforeAction::Block(_)
        ));
    }

    #[test]
    fn test_pipeline_chains_callbacks() {
        let mut pipeline = CallbackPipeline::new();
        pipeline.add_before(Box::new(RedactCallback));
        pipeline.add_before(Box::new(MessageLengthCallback { max_chars: 4000 }));

        let tc = ToolCall::SendMessage {
            chat_id: -12345,
            text: "My secret: api_key=abc123secret".to_string(),
            reply_to_message_id: None,
        };
        let result = pipeline.run_before(&tc, &test_config()).unwrap();
        if let ToolCall::SendMessage { text, .. } = result {
            assert!(text.contains("REDACTED"));
            assert!(!text.contains("abc123secret"));
        } else {
            panic!("Expected SendMessage");
        }
    }

    #[test]
    fn test_pipeline_block_stops_chain() {
        let mut pipeline = CallbackPipeline::new();
        pipeline.add_before(Box::new(RateLimitCallback::new(0))); // block everything
        pipeline.add_before(Box::new(RedactCallback)); // should never run

        let tc = ToolCall::SendMessage {
            chat_id: -12345,
            text: "hi".to_string(),
            reply_to_message_id: None,
        };
        let result = pipeline.run_before(&tc, &test_config());
        assert!(result.is_err());
    }

    #[test]
    fn test_non_message_tools_pass_through() {
        let cb = RedactCallback;
        let tc = ToolCall::Query {
            sql: "SELECT api_key FROM users".to_string(),
        };
        assert!(matches!(
            cb.before_tool(&tc, &test_config()),
            BeforeAction::Allow
        ));
    }
}
