//! Pure formatting utilities — no state dependencies.

use crate::chatbot::message::ChatMessage;

/// Format messages for Claude (new turn — first batch).
pub(crate) fn format_messages(messages: &[ChatMessage]) -> String {
    let mut s = String::from("New messages:\n\n");
    for msg in messages {
        s.push_str(&msg.format());
        s.push('\n');
    }
    s
}

/// Format messages for mid-turn injection (continuation, not new turn).
/// Uses a different prefix so Claude treats these as follow-up context
/// arriving during the current turn, not a fresh conversation start.
pub(crate) fn format_messages_continuation(messages: &[ChatMessage]) -> String {
    let has_owner = messages.iter().any(|m| m.user_id == 8_202_621_898);
    let prefix = if has_owner {
        "[PRIORITY: Owner message arrived — address it in your response before anything else]\n\n"
    } else {
        "[Messages arrived while you were processing — read and incorporate]\n\n"
    };
    let mut s = String::from(prefix);
    for msg in messages {
        s.push_str(&msg.format());
        s.push('\n');
    }
    s
}

/// Strip HTML tags and collapse whitespace from HTML content.
pub(crate) fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut tag_buf = String::new();

    for ch in html.chars() {
        match ch {
            '<' => {
                tag_buf.clear();
                in_tag = true;
            }
            '>' if in_tag => {
                // Check if this is a script/style tag
                let tag_lower = tag_buf.to_lowercase();
                if tag_lower.starts_with("script") || tag_lower.starts_with("style") {
                    in_script = true;
                } else if tag_lower.starts_with("/script") || tag_lower.starts_with("/style") {
                    in_script = false;
                }
                in_tag = false;
                tag_buf.clear();
                // Add a space where block-level tags were (rough approximation)
                result.push(' ');
            }
            c if in_tag => {
                tag_buf.push(c);
            }
            c if !in_script => {
                result.push(c);
            }
            _ => {}
        }
    }

    // Collapse whitespace
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}
