//! Context buffer for message lookups and persistence.
//!
//! This stores recent messages for:
//! - Looking up messages by ID (for replies)
//! - Persistence across restarts
//!
//! Bounded to MAX_MESSAGES entries. Oldest messages are evicted when the limit
//! is reached, preventing unbounded memory growth on long-running bots.
//!
//! Note: We no longer use this for building prompts - Claude Code maintains its own history.

use crate::chatbot::message::ChatMessage;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use tracing::{info, warn};

/// Maximum messages to keep in the context buffer.
/// 500 messages ~= 250 KB — plenty for reply lookups without unbounded growth.
const MAX_MESSAGES: usize = 500;

/// Buffer for recent messages.
pub struct ContextBuffer {
    messages: VecDeque<ChatMessage>,
    index: HashMap<i64, usize>,
}

impl ContextBuffer {
    pub fn new() -> Self {
        Self {
            messages: VecDeque::with_capacity(MAX_MESSAGES),
            index: HashMap::new(),
        }
    }

    /// Add a message. Evicts the oldest if at capacity.
    pub fn add_message(&mut self, msg: ChatMessage) {
        // Evict oldest messages until under limit
        while self.messages.len() >= MAX_MESSAGES {
            if let Some(old) = self.messages.pop_front() {
                self.index.remove(&old.message_id);
            }
            // Rebuild index since VecDeque indices shifted
            self.rebuild_index();
        }
        let idx = self.messages.len();
        self.index.insert(msg.message_id, idx);
        self.messages.push_back(msg);
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
    /// Save to disk using atomic write (temp + rename) to prevent corruption.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let state = ContextState {
            messages: self.messages.iter().cloned().collect(),
        };

        let json = serde_json::to_string_pretty(&state)
            .map_err(|e| format!("Failed to serialize: {e}"))?;

        // Atomic write: write to .tmp, then rename — prevents half-written files on crash.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).map_err(|e| format!("Failed to write tmp: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("Failed to rename: {e}"))?;

        info!("Saved context ({} messages)", self.messages.len());
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let json = std::fs::read_to_string(path).map_err(|e| format!("Failed to read: {e}"))?;

        let state: ContextState =
            serde_json::from_str(&json).map_err(|e| format!("Failed to parse: {e}"))?;

        // Only keep the last MAX_MESSAGES when loading (truncate old history).
        let messages: VecDeque<ChatMessage> = if state.messages.len() > MAX_MESSAGES {
            state.messages[state.messages.len() - MAX_MESSAGES..]
                .iter()
                .cloned()
                .collect()
        } else {
            state.messages.into_iter().collect()
        };

        let mut buffer = Self {
            messages,
            index: HashMap::new(),
        };
        buffer.rebuild_index();

        info!(
            "Loaded context from {:?} ({} messages)",
            path,
            buffer.messages.len()
        );
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

    #[test]
    fn test_eviction_at_capacity() {
        let mut ctx = ContextBuffer::new();
        // Fill to MAX_MESSAGES + 10
        for i in 0..=(MAX_MESSAGES as i64 + 10) {
            ctx.add_message(make_msg(i, &format!("msg{i}")));
        }
        // Buffer should not exceed MAX_MESSAGES
        assert!(ctx.messages.len() <= MAX_MESSAGES);
        // Oldest messages should be evicted
        assert!(ctx.get_message(0).is_none());
        // Recent messages should still be there
        assert!(ctx.get_message(MAX_MESSAGES as i64 + 10).is_some());
    }
}
