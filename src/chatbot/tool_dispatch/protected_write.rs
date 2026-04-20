//! Dispatch for the MCP `protected_write` tool.
//!
//! Bridges the AI-agent-facing tool call to `guardian_client::GuardianClient`.
//! Returns structured results that Nova (the caller) can reason about —
//! see the DX review in the design doc: "denied" tokens alone are
//! insufficient; the caller needs `human_message + suggested_action +
//! alternative_roots` so it can retry intelligently.

use crate::chatbot::claude_code::ToolResult;
use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::journal;
use crate::guardian_client::WriteResult;
use tokio::sync::Mutex;

/// Execute a `protected_write(path, content, reason)` request.
///
/// Gate conditions:
/// - Bot must be Tier 1 (`full_permissions = true`). Atlas/Sentinel calling
///   this is a red flag — reject and log.
/// - `guardian_client` must be present in `ChatbotConfig`. Absent means
///   either the config did not enable the guardian or the socket/key
///   files were missing at startup.
///
/// Returns a `ToolResult` whose `content` field holds a compact JSON
/// payload Nova can parse to choose a follow-up action.
pub(super) async fn execute_protected_write(
    tool_use_id: &str,
    config: &ChatbotConfig,
    database: &Mutex<Database>,
    path: &str,
    content: &str,
    reason: &str,
) -> ToolResult {
    // Gate 1: Tier-2 bots must not call this.
    if !config.full_permissions {
        tracing::warn!(
            bot = %config.bot_name,
            "protected_write called by non-Tier-1 bot — rejecting"
        );
        return ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: Some(
                r#"{"ok":false,"err_code":"forbidden_tier","message":"protected_write is only available to Tier 1 bots (Nova). This bot does not have full_permissions.","suggested_action":"Do not call this tool again. If you need to write a file, ask Nova to do it via the cross-bot message bus."}"#
                    .to_string(),
            ),
            is_error: true,
            image: None,
        };
    }

    // Gate 2: guardian must be configured.
    let Some(client) = config.guardian_client.as_ref() else {
        tracing::warn!(
            bot = %config.bot_name,
            "protected_write called but guardian_client is None — check guardian_enabled + socket/key paths"
        );
        return ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: Some(
                r#"{"ok":false,"err_code":"guardian_disabled","message":"Bootstrap guardian is not configured or not reachable at startup. The harness must be restarted with guardian_enabled=true and valid guardian_socket_path + guardian_key_path.","suggested_action":"Tell the owner the guardian is down. Do not retry."}"#
                    .to_string(),
            ),
            is_error: true,
            image: None,
        };
    };

    // Gate 3: path sanity — reject obviously-wrong inputs BEFORE touching the guardian
    // so a busy guardian doesn't have to field garbage.
    if path.trim().is_empty() {
        return tool_error(
            tool_use_id,
            "empty_path",
            "path must be a non-empty string",
            "Provide an absolute filesystem path.",
        );
    }
    if !path.starts_with('/') {
        return tool_error(
            tool_use_id,
            "not_absolute",
            "path must be absolute — starts with /",
            "Prepend the absolute root (e.g. /opt/nova/data/) and try again.",
        );
    }
    if reason.trim().is_empty() {
        return tool_error(
            tool_use_id,
            "empty_reason",
            "reason must be a non-empty string",
            "Explain in one sentence why you are writing this file. Logged in the guardian audit trail.",
        );
    }

    // The guardian client's I/O is blocking; wrap in spawn_blocking so
    // we don't stall the tokio executor on a slow guardian.
    let client_arc: std::sync::Arc<crate::guardian_client::GuardianClient> =
        std::sync::Arc::clone(client);
    let path_owned = path.to_string();
    let content_bytes = content.as_bytes().to_vec();
    let reason_owned = reason.to_string();

    let result = tokio::task::spawn_blocking(move || {
        client_arc.protected_write(&path_owned, &content_bytes, &reason_owned)
    })
    .await;

    let write_result = match result {
        Ok(Ok(wr)) => wr,
        Ok(Err(e)) => {
            tracing::warn!(err = %e, path = %path, "guardian RPC error");
            return tool_error(
                tool_use_id,
                "rpc_error",
                &format!("guardian RPC failed: {}", e),
                "Check that the guardian process is running and the socket is accessible.",
            );
        }
        Err(join_err) => {
            tracing::error!(err = %join_err, "guardian spawn_blocking task panicked");
            return tool_error(
                tool_use_id,
                "internal_error",
                "guardian dispatch task panicked",
                "Restart the harness. This is a bug — please capture logs.",
            );
        }
    };

    // Grab the journal connection once so we emit the right event type
    // based on the write outcome.
    let db = database.lock().await;
    let conn = db.connection();
    let bot = config.bot_name.clone();

    match write_result {
        WriteResult::Ok { written_bytes } => {
            journal::emit(
                conn,
                None,
                journal::ENTRY_GUARDIAN_ALLOW,
                &format!("guardian.allow: wrote {} bytes", written_bytes),
                &format!("path={} reason={}", path, reason),
                &[],
                &[bot, path.to_string()],
            );
            let body = serde_json::json!({
                "ok": true,
                "written_bytes": written_bytes,
            });
            ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: Some(body.to_string()),
                is_error: false,
                image: None,
            }
        }
        WriteResult::Denied {
            reason: denial_reason,
            alternatives,
        } => {
            journal::emit(
                conn,
                None,
                journal::ENTRY_GUARDIAN_DENY,
                &format!("guardian.deny: {}", denial_reason),
                &format!("path={} reason={}", path, reason),
                &[],
                &[bot, path.to_string()],
            );
            let body = serde_json::json!({
                "ok": false,
                "err_code": "denied",
                "message": denial_reason,
                "suggested_action": "Pick a path inside one of `alternative_roots`. Protected paths (harness source, configs) are owner-only.",
                "alternative_roots": alternatives,
            });
            ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: Some(body.to_string()),
                is_error: true, // treat denial as a tool error so Nova notices
                image: None,
            }
        }
        WriteResult::Err {
            code,
            message,
            suggested_action,
        } => {
            journal::emit(
                conn,
                None,
                journal::ENTRY_GUARDIAN_ERROR,
                &format!("guardian.error: {} ({})", code, message),
                &format!("path={} reason={}", path, reason),
                &[],
                &[bot, path.to_string()],
            );
            let body = serde_json::json!({
                "ok": false,
                "err_code": code,
                "message": message,
                "suggested_action": suggested_action,
            });
            ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: Some(body.to_string()),
                is_error: true,
                image: None,
            }
        }
    }
}

fn tool_error(
    tool_use_id: &str,
    err_code: &str,
    message: &str,
    suggested_action: &str,
) -> ToolResult {
    let body = serde_json::json!({
        "ok": false,
        "err_code": err_code,
        "message": message,
        "suggested_action": suggested_action,
    });
    ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: Some(body.to_string()),
        is_error: true,
        image: None,
    }
}
