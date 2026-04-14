//! Tests for the chatbot module.
//!
//! Note: The original integration tests required MockTelegramApi, MockClaudeApi,
//! and TestBot infrastructure that was never implemented. Those have been removed.
//! Current tests focus on unit-testable components.

// Message formatting tests use the actual ChatMessage struct
#[cfg(test)]
mod message_formatting {
    use crate::chatbot::message::{ChatMessage, ReplyTo};

    fn test_msg(text: &str) -> ChatMessage {
        ChatMessage {
            message_id: 1,
            chat_id: -12345,
            user_id: 100,
            username: "Test".to_string(),
            first_name: None,
            timestamp: "10:00".to_string(),
            text: text.to_string(),
            reply_to: None,
            photo_file_id: None,
            image: None,
            voice_transcription: None,
        }
    }

    #[test]
    fn test_basic_format() {
        let msg = test_msg("hello");
        let formatted = msg.format();
        assert!(formatted.contains("user:100"));
        assert!(formatted.contains("Test"));
        assert!(formatted.contains("hello"));
    }

    #[test]
    fn test_escapes_newlines() {
        let msg = test_msg("line1\nline2");
        let formatted = msg.format();
        assert!(!formatted.contains('\n') || formatted.matches('\n').count() == 0
            || formatted.contains("\\n"));
    }

    #[test]
    fn test_reply_format() {
        let mut msg = test_msg("I agree");
        msg.reply_to = Some(ReplyTo {
            message_id: 99,
            username: "Alice".to_string(),
            text: "what about rust?".to_string(),
        });
        let formatted = msg.format();
        assert!(formatted.contains("Alice"));
        assert!(formatted.contains("what about rust?"));
    }
}
