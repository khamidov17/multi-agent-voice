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

/// Cap on content size accepted from the model. 10 MiB is already generous;
/// beyond that we refuse rather than let Nova allocate unbounded memory
/// through this tool. /review adversarial flagged the unbounded copy path.
const MAX_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// Truncation cap for path/reason strings we echo into the journal. These
/// become audit-log rows in SQLite; without a cap a 10 MB `reason` balloons
/// the DB. `path` gets 4 KiB (filesystems allow longer but 99.9% of paths
/// fit), `reason` gets 2 KiB (plenty for free-form rationale).
const MAX_PATH_CHARS: usize = 4096;
const MAX_REASON_CHARS: usize = 2048;

/// UTF-8 safe character truncation with a trailing ellipsis when clipped.
fn truncate_for_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Strip newlines and ASCII control characters that could be used to forge
/// fake log lines. /review security flagged journal log-injection.
fn sanitize_for_log(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

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

    // Size cap: refuse oversized payloads before the base64 copy (which
    // doubles memory). 10 MiB is already generous — Phase 0 writes are
    // source files + memory blobs, not binaries. /review adversarial
    // flagged an adversarial 10MB reason / 10MB content pair as an OOM path.
    if content.len() > MAX_CONTENT_BYTES {
        return tool_error(
            tool_use_id,
            "content_too_large",
            &format!(
                "content is {} bytes; cap is {} ({} MiB)",
                content.len(),
                MAX_CONTENT_BYTES,
                MAX_CONTENT_BYTES / (1024 * 1024)
            ),
            "Split the write into smaller files, or compress the payload before sending.",
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
    // based on the write outcome. Strip control chars + cap length on
    // model-sourced strings so they can't forge fake log lines or bloat
    // the journal DB (/review security + adversarial).
    let db = database.lock().await;
    let conn = db.connection();
    let bot = config.bot_name.clone();
    let safe_path = sanitize_for_log(&truncate_for_log(path, MAX_PATH_CHARS));
    let safe_reason = sanitize_for_log(&truncate_for_log(reason, MAX_REASON_CHARS));

    match write_result {
        WriteResult::Ok { written_bytes } => {
            journal::emit(
                conn,
                None,
                journal::ENTRY_GUARDIAN_ALLOW,
                &format!("guardian.allow: wrote {} bytes", written_bytes),
                &format!("path={} reason={}", safe_path, safe_reason),
                &[],
                &[bot, safe_path.clone()],
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
                &format!("path={} reason={}", safe_path, safe_reason),
                &[],
                &[bot, safe_path.clone()],
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
                &format!("path={} reason={}", safe_path, safe_reason),
                &[],
                &[bot, safe_path.clone()],
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
