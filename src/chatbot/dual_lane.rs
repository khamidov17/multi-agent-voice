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

/// Generate the minimal system prompt for the quick response lane.
pub fn quick_lane_system_prompt(bot_name: &str) -> String {
    format!(
        r#"You are the quick-response lane for {bot_name}.

The deep-work lane is currently handling a complex task. Your job:
- Acknowledge messages so people know the bot is alive
- Answer simple questions briefly
- If someone asks something complex, say: "{bot_name} is working on a task right now. I'll handle this when the current work is done."

Keep responses under 2 sentences. Use send_message to respond, then stop.
Be friendly but brief. You have limited tools — you can only read and respond, not modify anything.

Output format: Return a JSON object with:
- "action": "stop" (always stop after responding)
- "tool_calls": array with send_message calls

Example: {{"action": "stop", "reason": "acknowledged message", "tool_calls": [{{"tool": "send_message", "chat_id": -1003399442526, "text": "Got it, I'm working on something right now. Will respond properly soon!"}}]}}
"#
    )
}
