//! Tool dispatch — delegation tools.

use tracing::info;

use crate::chatbot::engine::ChatbotConfig;

/// Formally delegate work to another agent (DelegateTask tool).
pub(super) async fn execute_delegate_task(
    config: &ChatbotConfig,
    to_agent: &str,
    task_description: &str,
    success_criteria: &str,
    deadline_minutes: Option<u64>,
    priority: Option<&str>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    // Create task in shared board
    let task_id = format!("task-{}", chrono::Utc::now().timestamp_millis());
    let pri = match priority {
        Some("high") => 3,
        Some("medium") => 2,
        _ => 1,
    };
    db.create_task(
        &task_id,
        task_description,
        &config.bot_name,
        Some(task_description),
        pri,
    )
    .map_err(|e| format!("Task creation failed: {e}"))?;

    // Create handoff contract
    let payload = serde_json::json!({
        "task_description": task_description,
        "success_criteria": success_criteria,
        "deadline_minutes": deadline_minutes,
        "priority": priority.unwrap_or("medium"),
    });
    let handoff_id = db
        .create_handoff(
            &config.bot_name,
            to_agent,
            &task_id,
            "delegate",
            &payload.to_string(),
        )
        .map_err(|e| format!("Handoff creation failed: {e}"))?;

    // Send notification via bot_messages
    let notification = format!(
        "[HANDOFF:{}] {} delegates to {}: {}",
        handoff_id, config.bot_name, to_agent, task_description
    );
    let _ = db.insert(&config.bot_name, Some(to_agent), &notification, None, None);

    info!(
        "Delegated to {}: {} (handoff_id={}, task_id={})",
        to_agent, task_description, handoff_id, task_id
    );

    Ok(Some(format!(
        "Delegated to {} (handoff_id={}, task_id={}). They'll receive a [HANDOFF:{}] notification.",
        to_agent, handoff_id, task_id, handoff_id
    )))
}

/// Respond to a handoff: accept, complete, or reject (RespondToHandoff tool).
pub(super) async fn execute_respond_to_handoff(
    config: &ChatbotConfig,
    handoff_id: i64,
    action: &str,
    result_or_reason: Option<&str>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let handoff = db
        .get_handoff(handoff_id)
        .map_err(|e| format!("Query failed: {e}"))?
        .ok_or_else(|| format!("Handoff {} not found", handoff_id))?;

    match action {
        "accept" => {
            let ok = db
                .accept_handoff(handoff_id, &config.bot_name)
                .map_err(|e| format!("Accept failed: {e}"))?;
            if !ok {
                return Err(format!(
                    "Cannot accept handoff {} — it's assigned to {}, not {}",
                    handoff_id, handoff.to_agent, config.bot_name
                ));
            }
            let notif = format!(
                "[HANDOFF_ACCEPTED:{}] {} accepted the delegation",
                handoff_id, config.bot_name
            );
            let _ = db.insert(
                &config.bot_name,
                Some(&handoff.from_agent),
                &notif,
                None,
                None,
            );
            Ok(Some(format!(
                "Accepted handoff {}. Now working on it.",
                handoff_id
            )))
        }
        "complete" => {
            let result_text = result_or_reason.unwrap_or("Completed");
            db.complete_handoff(handoff_id, &config.bot_name, result_text)
                .map_err(|e| format!("Complete failed: {e}"))?;
            let notif = format!(
                "[HANDOFF_COMPLETE:{}] {} completed: {}",
                handoff_id, config.bot_name, result_text
            );
            let _ = db.insert(
                &config.bot_name,
                Some(&handoff.from_agent),
                &notif,
                None,
                None,
            );
            Ok(Some(format!(
                "Handoff {} completed. Notified {}.",
                handoff_id, handoff.from_agent
            )))
        }
        "reject" => {
            let reason = result_or_reason.unwrap_or("No reason given");
            db.reject_handoff(handoff_id, &config.bot_name, reason)
                .map_err(|e| format!("Reject failed: {e}"))?;
            let notif = format!(
                "[HANDOFF_REJECTED:{}] {} rejected: {}",
                handoff_id, config.bot_name, reason
            );
            let _ = db.insert(
                &config.bot_name,
                Some(&handoff.from_agent),
                &notif,
                None,
                None,
            );
            Ok(Some(format!(
                "Handoff {} rejected. Notified {}.",
                handoff_id, handoff.from_agent
            )))
        }
        _ => Err(format!(
            "Invalid action: {}. Use accept/complete/reject.",
            action
        )),
    }
}

/// Request consensus from other agents (RequestConsensus tool).
pub(super) async fn execute_request_consensus(
    config: &ChatbotConfig,
    action_type: &str,
    description: &str,
    timeout_minutes: Option<u64>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    // Determine required approvers based on action type
    let required: Vec<String> = match action_type {
        "deploy" => vec!["Security".to_string()],
        "ban" => vec!["Nova".to_string()],
        "config_change" => vec!["Nova".to_string(), "Security".to_string()],
        "tool_build" => vec!["Security".to_string()],
        "plan_approve" => vec!["Security".to_string()],
        _ => vec!["Security".to_string()], // default: security review
    };

    let timeout = timeout_minutes.unwrap_or(10);
    let request_id = db
        .request_consensus(
            &config.bot_name,
            action_type,
            description,
            &required,
            timeout,
        )
        .map_err(|e| format!("Consensus request failed: {e}"))?;

    // Notify each required approver
    for approver in &required {
        let notif = format!(
            "[CONSENSUS_REQUEST:{}] {} wants to {}: {}. Approve or reject using vote_consensus.",
            request_id, config.bot_name, action_type, description
        );
        let _ = db.insert(&config.bot_name, Some(approver), &notif, None, None);
    }

    Ok(Some(format!(
        "Consensus request #{} created. Waiting for approval from: {}. Timeout: {}min.",
        request_id,
        required.join(", "),
        timeout,
    )))
}

/// Vote on a consensus request (VoteConsensus tool).
pub(super) async fn execute_vote_consensus(
    config: &ChatbotConfig,
    request_id: i64,
    decision: &str,
    reason: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let status = db
        .vote_on_consensus(request_id, &config.bot_name, decision, reason)
        .map_err(|e| format!("Vote failed: {e}"))?;

    // Get requesting agent to notify them
    let requesting_agent: String = db
        .conn
        .query_row(
            "SELECT requesting_agent FROM consensus_requests WHERE id = ?1",
            rusqlite::params![request_id],
            |r| r.get(0),
        )
        .unwrap_or_default();

    let result_msg = match status {
        crate::chatbot::bot_messages::ConsensusStatus::Approved => {
            let notif = format!(
                "[CONSENSUS_APPROVED:{}] Your action was approved. Proceed.",
                request_id
            );
            let _ = db.insert(
                &config.bot_name,
                Some(&requesting_agent),
                &notif,
                None,
                None,
            );
            format!(
                "Consensus #{} APPROVED. Notified {}.",
                request_id, requesting_agent
            )
        }
        crate::chatbot::bot_messages::ConsensusStatus::Rejected(ref r) => {
            let notif = format!(
                "[CONSENSUS_REJECTED:{}] Action rejected. Reason: {}",
                request_id, r
            );
            let _ = db.insert(
                &config.bot_name,
                Some(&requesting_agent),
                &notif,
                None,
                None,
            );
            format!(
                "Consensus #{} REJECTED. Reason: {}. Notified {}.",
                request_id, r, requesting_agent
            )
        }
        crate::chatbot::bot_messages::ConsensusStatus::Pending => {
            format!(
                "Vote recorded on #{} ({}). Waiting for more votes.",
                request_id, decision
            )
        }
        crate::chatbot::bot_messages::ConsensusStatus::Expired => {
            format!("Consensus #{} has expired.", request_id)
        }
    };

    Ok(Some(result_msg))
}
