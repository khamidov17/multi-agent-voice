//! Context buffer for message lookups and persistence.
//!
//! This stores recent messages for:
//! - Looking up messages by ID (for replies)
//! - Persistence across restarts
//!
//! Note: We no longer use this for building prompts - Claude Code maintains its own history.

use crate::chatbot::message::ChatMessage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

/// Buffer for recent messages.
pub struct ContextBuffer {
    messages: Vec<ChatMessage>,
    index: HashMap<i64, usize>,
}

impl ContextBuffer {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Add a message.
    pub fn add_message(&mut self, msg: ChatMessage) {
        let idx = self.messages.len();
        self.index.insert(msg.message_id, idx);
        self.messages.push(msg);
    }

    /// Edit a message by ID.
    pub fn edit_message(&mut self, message_id: i64, new_text: &str) {
        if let Some(&idx) = self.index.get(&message_id)
            && idx < self.messages.len()
        {
            self.messages[idx].text = new_text.to_string();
        }
    }

    /// Get a message by ID.
    pub fn get_message(&self, message_id: i64) -> Option<&ChatMessage> {
        self.index
            .get(&message_id)
            .and_then(|&idx| self.messages.get(idx))
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (idx, msg) in self.messages.iter().enumerate() {
            self.index.insert(msg.message_id, idx);
        }
    }
}

impl Default for ContextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize, Deserialize)]
struct ContextState {
    messages: Vec<ChatMessage>,
}

impl ContextBuffer {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let state = ContextState {
            messages: self.messages.clone(),
        };

        let json = serde_json::to_string_pretty(&state)
            .map_err(|e| format!("Failed to serialize: {e}"))?;

        std::fs::write(path, json)
            .map_err(|e| format!("Failed to write: {e}"))?;

        info!("💾 Saved context ({} messages)", self.messages.len());
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read: {e}"))?;

        let state: ContextState = serde_json::from_str(&json)
            .map_err(|e| format!("Failed to parse: {e}"))?;

        let mut buffer = Self {
            messages: state.messages,
            index: HashMap::new(),
        };
        buffer.rebuild_index();

        info!("Loaded context from {:?} ({} messages)", path, buffer.messages.len());
        Ok(buffer)
    }

    pub fn load_or_new(path: &Path, _threshold: usize) -> Self {
        if path.exists() {
            match Self::load(path) {
                Ok(buffer) => buffer,
                Err(e) => {
                    warn!("Failed to load context: {e}");
                    Self::new()
                }
            }
        } else {
            info!("No context file, starting fresh");
            Self::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(id: i64, text: &str) -> ChatMessage {
        ChatMessage {
            message_id: id,
            chat_id: -12345,
            user_id: 100,
            username: "test".to_string(),
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
    fn test_add_and_get() {
        let mut ctx = ContextBuffer::new();
        ctx.add_message(make_msg(1, "hello"));

        let msg = ctx.get_message(1).unwrap();
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn test_edit() {
        let mut ctx = ContextBuffer::new();
        ctx.add_message(make_msg(1, "hello"));
        ctx.edit_message(1, "world");

        let msg = ctx.get_message(1).unwrap();
        assert_eq!(msg.text, "world");
    }
}
