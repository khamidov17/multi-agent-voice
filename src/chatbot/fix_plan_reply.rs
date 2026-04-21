//! Phase 2 — owner-reply parser for fix-plan approvals.
//!
//! When the owner DMs Nova with `approve #42` or `reject #42 because
//! the risk is too high`, this module:
//! 1. Parses the keyword + plan id + optional reason
//! 2. Transitions the plan via `fix_plans::update_status`
//! 3. Sends a confirmation DM back to the owner
//!
//! Runs in `engine::handle_message` BEFORE the message hits Nova's
//! pending queue. Nova still sees the message (so she can acknowledge
//! in-conversation), but the status transition has already happened
//! deterministically at the harness layer — no LLM turn burned on
//! mechanical keyword parsing.
//!
//! # Accepted syntax
//!
//! ```text
//!   approve #42
//!   approve #42 looks good
//!   reject #42 because the risk is too high
//!   reject 42 - wrong approach
//! ```
//!
//! The `#` is optional. Anything after the id is the optional
//! decision note. `approve`/`reject` are case-insensitive. Any other
//! message is [`OwnerReply::None`] and flows through unchanged.

use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::fix_plans::{self, FixPlanStatus};
use crate::chatbot::telegram::TelegramClient;
use tracing::{info, warn};

/// Result of parsing an owner's DM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerReply {
    None,
    Approve { plan_id: i64, note: Option<String> },
    Reject { plan_id: i64, note: Option<String> },
}

/// Parse an owner DM. Returns `OwnerReply::None` for anything that
/// doesn't start with an approval keyword — regular conversation
/// flows through untouched.
pub fn parse_owner_reply(text: &str) -> OwnerReply {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    let (kind, rest) = if let Some(r) = lower.strip_prefix("approve") {
        ("approve", r)
    } else if let Some(r) = lower.strip_prefix("reject") {
        ("reject", r)
    } else {
        return OwnerReply::None;
    };
    // Slice the remainder from the original (case-preserving) text.
    let rest_original = &trimmed[kind.len()..];
    let after = rest_original.trim_start();
    // Strip optional '#'.
    let after = after.strip_prefix('#').unwrap_or(after).trim_start();
    // Grab the leading integer id.
    let id_end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    if id_end == 0 {
        // Keyword without an id — not a valid reply. Let Nova handle
        // it in conversation instead of confusing the parser.
        let _ = rest;
        return OwnerReply::None;
    }
    let id_str = &after[..id_end];
    let plan_id = match id_str.parse::<i64>() {
        Ok(n) => n,
        Err(_) => return OwnerReply::None,
    };
    // Remainder → note. Strip leading "because", "-", ":", etc.
    let note_raw = after[id_end..].trim();
    let note = strip_note_prefix(note_raw)
        .trim()
        .to_string();
    let note = if note.is_empty() { None } else { Some(note) };
    match kind {
        "approve" => OwnerReply::Approve { plan_id, note },
        "reject" => OwnerReply::Reject { plan_id, note },
        _ => OwnerReply::None,
    }
}

fn strip_note_prefix(s: &str) -> &str {
    let s = s.trim_start();
    for prefix in ["because", "b/c", "-", ":", ","] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest;
        }
    }
    s
}

/// Apply a parsed reply: transition the plan via `fix_plans`, DM the
/// owner a one-line confirmation. Errors are logged as `warn!` and
/// swallowed — we never bubble up into the message handler because
/// that would block Nova from seeing the original message.
pub async fn apply_owner_reply(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    reply: OwnerReply,
) {
    let (plan_id, new_status, note, kind_label) = match reply {
        OwnerReply::None => return,
        OwnerReply::Approve { plan_id, note } => (plan_id, FixPlanStatus::Approved, note, "approved"),
        OwnerReply::Reject { plan_id, note } => (plan_id, FixPlanStatus::Rejected, note, "rejected"),
    };
    let data_dir = match config.data_dir.as_ref() {
        Some(p) => p,
        None => {
            warn!(plan_id, "fix_plan_reply: data_dir not set — cannot apply owner reply");
            return;
        }
    };
    let db_path = fix_plans::shared_fix_plans_db_path(data_dir);
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(plan_id, err = %e, path = %db_path.display(),
                  "fix_plan_reply: could not open fix_plans db");
            return;
        }
    };
    if let Err(e) = fix_plans::init_schema(&conn) {
        warn!(plan_id, err = %e, "fix_plan_reply: init_schema failed");
        return;
    }
    let decision_note = note.clone().unwrap_or_else(|| "no note".to_string());
    match fix_plans::update_status(&conn, plan_id, new_status, Some(&decision_note)) {
        Ok(row) => {
            info!(
                plan_id = row.id,
                status = row.status.as_str(),
                "fix_plan_reply: owner {} plan #{}", kind_label, plan_id
            );
            let confirmation = format!(
                "✅ plan #{} {} — noted. {}",
                plan_id,
                kind_label,
                match &note {
                    Some(n) => format!("(\"{}\")", n.chars().take(120).collect::<String>()),
                    None => String::new(),
                }
            );
            if let Err(e) = telegram.send_message(chat_id, &confirmation, None).await {
                warn!(err = %e, "fix_plan_reply: confirmation DM failed");
            }
        }
        Err(e) => {
            warn!(plan_id, err = %e, "fix_plan_reply: status transition failed");
            // Give the owner a heads-up so they know the reply didn't land.
            let msg = format!(
                "⚠️ couldn't {} plan #{}: {}",
                kind_label, plan_id, e
            );
            let _ = telegram.send_message(chat_id, &msg, None).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_approve() {
        assert_eq!(
            parse_owner_reply("approve #42"),
            OwnerReply::Approve {
                plan_id: 42,
                note: None
            }
        );
    }

    #[test]
    fn parse_reject_with_because_clause() {
        let r = parse_owner_reply("reject #17 because the risk is too high");
        assert_eq!(
            r,
            OwnerReply::Reject {
                plan_id: 17,
                note: Some("the risk is too high".to_string())
            }
        );
    }

    #[test]
    fn parse_without_hash_sign() {
        assert_eq!(
            parse_owner_reply("approve 42"),
            OwnerReply::Approve {
                plan_id: 42,
                note: None
            }
        );
    }

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(
            parse_owner_reply("APPROVE #5"),
            OwnerReply::Approve {
                plan_id: 5,
                note: None
            }
        );
        assert_eq!(
            parse_owner_reply("Reject #5 - bad idea"),
            OwnerReply::Reject {
                plan_id: 5,
                note: Some("bad idea".to_string())
            }
        );
    }

    #[test]
    fn parse_handles_leading_whitespace() {
        assert_eq!(
            parse_owner_reply("   approve #3  "),
            OwnerReply::Approve {
                plan_id: 3,
                note: None
            }
        );
    }

    #[test]
    fn parse_returns_none_for_non_reply_messages() {
        assert_eq!(parse_owner_reply("hey what's up"), OwnerReply::None);
        assert_eq!(parse_owner_reply(""), OwnerReply::None);
        assert_eq!(parse_owner_reply("check alerts please"), OwnerReply::None);
    }

    #[test]
    fn parse_returns_none_for_keyword_without_id() {
        // Just "approve" / "reject" with no id — ambiguous, let Nova handle.
        assert_eq!(parse_owner_reply("approve"), OwnerReply::None);
        assert_eq!(parse_owner_reply("reject that idea"), OwnerReply::None);
    }

    #[test]
    fn parse_reject_with_colon_note() {
        assert_eq!(
            parse_owner_reply("reject #9: test plan is weak"),
            OwnerReply::Reject {
                plan_id: 9,
                note: Some("test plan is weak".to_string())
            }
        );
    }

    #[test]
    fn parse_approve_with_trailing_note_no_prefix() {
        assert_eq!(
            parse_owner_reply("approve #100 looks good, ship it"),
            OwnerReply::Approve {
                plan_id: 100,
                note: Some("looks good, ship it".to_string())
            }
        );
    }
}
