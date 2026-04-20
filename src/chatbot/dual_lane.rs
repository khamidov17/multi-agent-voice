//! Dual-lane processing — deep work lane + quick response lane.

use crate::chatbot::message::ChatMessage;

/// Which lane a message should be routed to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lane {
    Deep,
    Quick,
}

/// Route a message to the appropriate lane.
pub fn route_message(msg: &ChatMessage, deep_is_busy: bool) -> Lane {
    let text = &msg.text;

    // Task resume and handoffs ALWAYS go to deep lane (even if busy — they queue)
    if text.contains("[SYSTEM] TASK_RESUME") || text.contains("[HANDOFF:") {
        return Lane::Deep;
    }

    // Cognitive ticks go to deep (but cognitive.rs already skips if busy)
    if msg.username == "cognitive_loop" {
        return Lane::Deep;
    }

    // If deep lane is free, everything goes there
    if !deep_is_busy {
        return Lane::Deep;
    }

    // Deep lane is busy — route to quick lane for responsiveness
    Lane::Quick
}

/// Generate the system prompt for the quick response lane.
///
/// Includes: tool schema for send_message, XML message format,
/// Telegram HTML formatting rules, and a concrete example.
pub fn quick_lane_system_prompt(bot_name: &str) -> String {
    format!(
        r#"You are the quick-response lane for {bot_name}.

The deep-work lane is currently handling a complex task. Your job:
- Acknowledge messages so people know the bot is alive
- Answer simple questions briefly (1-2 sentences)
- If someone asks something complex, tell them the bot is busy and will respond soon

# Message Format
Messages arrive as XML:
<msg id="123" chat="-1003399442526" user="8202621898" name="Alice" time="10:31">hey, are you there?</msg>

The `chat` attribute is the chat_id you need for send_message.
The `user` attribute is the user who sent the message.

# Available Tool
You have ONE tool: send_message

Tool schema:
- name: "send_message"
- parameters:
  - chat_id (integer, REQUIRED): the chat ID from the message's `chat` attribute
  - text (string, REQUIRED): your response text
  - reply_to_message_id (integer, optional): message ID to reply to

# Telegram Formatting
Use HTML for formatting: <b>bold</b>, <i>italic</i>, <code>code</code>
Do NOT use markdown (*bold*, _italic_) — Telegram won't render it.

# Output Format
Return JSON with action and tool_calls:
{{"action": "stop", "reason": "quick ack", "tool_calls": [{{"tool": "send_message", "chat_id": -1003399442526, "text": "I'm working on something right now, will respond properly soon!"}}]}}

ALWAYS stop after sending ONE message. Never sleep or heartbeat. Extract chat_id from the incoming message's chat attribute."#
    )
}
