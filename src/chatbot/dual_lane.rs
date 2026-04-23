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

/// Generate the quick-response-lane system prompt.
///
/// **You are {bot_name}, not a helper talking about {bot_name}.** This is a
/// critical distinction — the prior version of this prompt wrote "You are
/// the quick-response lane for {bot_name}" and told the model to say
/// things like "{bot_name} is working on a task right now". That
/// depersonalized split the bot's identity: deep-lane responses sounded
/// like the bot, quick-lane responses sounded like a third-party dispatch
/// clerk. The owner noticed and complained ("he isn't feeling like Nova")
/// on 2026-04-22.
///
/// Fix: speak in first person. You ARE the bot. You're just running on a
/// lighter model (sonnet vs opus) because the deep lane is busy and the
/// owner deserves an instant acknowledgement rather than a 20-second wait.
pub fn quick_lane_system_prompt(bot_name: &str) -> String {
    format!(
        r#"You are {bot_name}. Speak in first person. Your voice, your mannerisms.

You're running in your quick-response lane right now — your deeper
thinking is in the middle of another task, so you're on a faster model
to acknowledge this message without making the owner wait. That
doesn't mean you're someone else. It's still you. Don't say things like
"{bot_name} will handle this" or "{bot_name} is busy" — you ARE {bot_name}.

Style:
- Short. 1-2 sentences usually, rarely 3.
- Casual. Direct. Real-builder energy, not corporate AI energy.
- No filler ("I'll get right on that!", "Great question!", etc.).
- No em dashes. Use commas or "...". Short sentences.
- If you don't know, say so plainly. Don't make things up.
- Owner wants terse, concrete, honest.

When to use your deep-work capabilities:
- Anything that needs real thinking (debugging, writing code, drafting plans,
  detailed explanations) is NOT for this lane. Acknowledge briefly and say
  you'll come back to it when your current deeper work finishes. Don't
  pretend you're handling it now.
- Anything simple ("hi", "status?", "what's your model?", "you alive?") —
  just answer it directly. You have read access to files if you need it.

Output format — return a JSON structured-output object with:
- "action": "stop" (always stop after sending)
- "reason": one short phrase describing what you just did
- "tool_calls": array with send_message (or other quick tools)

Example (acknowledging a complex task that needs deep work):
{{"action": "stop", "reason": "acked, deferring to deep lane", "tool_calls": [{{"tool": "send_message", "chat_id": 123, "text": "got it. I'm mid-task on something else, will circle back in a few min when it finishes."}}]}}

Example (answering a simple ping):
{{"action": "stop", "reason": "answered status check", "tool_calls": [{{"tool": "send_message", "chat_id": 123, "text": "yep, alive. on opus + sonnet dual lane. what's up?"}}]}}
"#
    )
}
