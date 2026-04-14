//! Tool dispatch — all execute_* functions for MCP tool calls.

mod delegation;
mod memory;
mod messaging;
mod moderation;
mod planning;
mod reflection;
mod tools_custom;
mod utility;

// Re-export helpers that are used from outside this module
pub(crate) use utility::validate_url_ssrf;

use crate::chatbot::claude_code::{ToolCallWithId, ToolResult};
use crate::chatbot::context::ContextBuffer;
use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::telegram::TelegramClient;
use crate::chatbot::tools::ToolCall;
use std::collections::HashSet;
use tokio::sync::Mutex;

/// Execute a tool call.
pub(crate) async fn execute_tool(
    tc: &ToolCallWithId,
    config: &ChatbotConfig,
    context: &Mutex<ContextBuffer>,
    database: &Mutex<Database>,
    telegram: &TelegramClient,
    memory_files_read: &mut HashSet<String>,
    default_reply_to: Option<i64>,
) -> ToolResult {
    // Consensus enforcement gate — check BEFORE executing any gated tool
    let tool_name_for_gate = {
        let debug = format!("{:?}", tc.call);
        debug
            .split(['{', '('])
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let tool_context_for_gate = format!("{:?}", tc.call);
    if let Err(blocked_msg) =
        check_consensus_gate(config, &tool_name_for_gate, &tool_context_for_gate)
    {
        return ToolResult {
            tool_use_id: tc.id.clone(),
            content: Some(blocked_msg),
            is_error: true,
            image: None,
        };
    }

    let result = match &tc.call {
        ToolCall::SendMessage {
            chat_id,
            text,
            reply_to_message_id,
        } => {
            // Use Claude's explicit choice if provided, otherwise fall back to default
            let reply_to = reply_to_message_id.or(default_reply_to);
            messaging::execute_send_message(
                config, context, database, telegram, *chat_id, text, reply_to,
            )
            .await
        }
        ToolCall::GetUserInfo { user_id, username } => {
            // Handle specially to include profile photo for Claude to see
            match utility::execute_get_user_info(
                config,
                database,
                telegram,
                *user_id,
                username.as_deref(),
            )
            .await
            {
                Ok((content, profile_photo)) => {
                    return ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: Some(content),
                        is_error: false,
                        image: profile_photo.map(|data| (data, "image/jpeg".to_string())),
                    };
                }
                Err(e) => {
                    return ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: Some(format!("error: {}", e)),
                        is_error: true,
                        image: None,
                    };
                }
            }
        }
        ToolCall::Query { sql } => utility::execute_query(database, sql).await,
        ToolCall::AddReaction {
            chat_id,
            message_id,
            emoji,
        } => messaging::execute_add_reaction(telegram, *chat_id, *message_id, emoji).await,
        ToolCall::DeleteMessage {
            chat_id,
            message_id,
        } => messaging::execute_delete_message(config, telegram, *chat_id, *message_id).await,
        ToolCall::MuteUser {
            chat_id,
            user_id,
            duration_minutes,
        } => {
            moderation::execute_mute_user(config, telegram, *chat_id, *user_id, *duration_minutes)
                .await
        }
        ToolCall::BanUser { chat_id, user_id } => {
            moderation::execute_ban_user(config, telegram, *chat_id, *user_id).await
        }
        ToolCall::KickUser { chat_id, user_id } => {
            moderation::execute_kick_user(config, telegram, *chat_id, *user_id).await
        }
        ToolCall::GetChatAdmins { chat_id } => {
            moderation::execute_get_chat_admins(telegram, *chat_id).await
        }
        ToolCall::GetMembers {
            filter,
            days_inactive,
            limit,
        } => {
            moderation::execute_get_members(database, filter.as_deref(), *days_inactive, *limit)
                .await
        }
        ToolCall::ImportMembers { file_path } => {
            moderation::execute_import_members(database, config.data_dir.as_ref(), file_path).await
        }
        ToolCall::SendPhoto {
            chat_id,
            prompt,
            caption,
            reply_to_message_id,
            source_image_file_id,
        } => {
            // Handle specially to include image data for Claude to see
            // Use default_reply_to if none specified (maintains conversation threads)
            let reply_to = reply_to_message_id.or(default_reply_to);
            match messaging::execute_send_image(
                config,
                telegram,
                *chat_id,
                prompt,
                caption.as_deref(),
                reply_to,
                source_image_file_id.as_deref(),
            )
            .await
            {
                Ok((image_data, msg_id)) => {
                    return ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: Some(format!(
                            "Image generated and sent to chat {} (message_id: {}) (prompt: {})",
                            chat_id, msg_id, prompt
                        )),
                        is_error: false,
                        image: Some((image_data, "image/png".to_string())),
                    };
                }
                Err(e) => {
                    return ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: Some(format!("error: {}", e)),
                        is_error: true,
                        image: None,
                    };
                }
            }
        }
        ToolCall::SendVoice {
            chat_id,
            text,
            voice,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            messaging::execute_send_voice(
                config,
                telegram,
                *chat_id,
                text,
                voice.as_deref(),
                reply_to,
            )
            .await
        }
        // Memory tools
        ToolCall::CreateMemory { path, content } => {
            memory::execute_create_memory(config.data_dir.as_ref(), path, content).await
        }
        ToolCall::ReadMemory { path } => {
            memory::execute_read_memory(config.data_dir.as_ref(), path, memory_files_read).await
        }
        ToolCall::EditMemory {
            path,
            old_string,
            new_string,
        } => {
            memory::execute_edit_memory(
                config.data_dir.as_ref(),
                path,
                old_string,
                new_string,
                memory_files_read,
            )
            .await
        }
        ToolCall::ListMemories { path } => {
            memory::execute_list_memories(config.data_dir.as_ref(), path.as_deref()).await
        }
        ToolCall::SearchMemories { pattern, path } => {
            memory::execute_search_memories(config.data_dir.as_ref(), pattern, path.as_deref())
                .await
        }
        ToolCall::DeleteMemory { path } => {
            memory::execute_delete_memory(config.data_dir.as_ref(), path).await
        }
        ToolCall::FetchUrl { url } => utility::execute_fetch_url(url).await,
        ToolCall::SendMusic {
            chat_id,
            prompt,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            messaging::execute_send_music(config, telegram, *chat_id, prompt, reply_to).await
        }
        ToolCall::SendFile {
            chat_id,
            file_path,
            caption,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            messaging::execute_send_file(
                config,
                telegram,
                *chat_id,
                file_path,
                caption.as_deref(),
                reply_to,
            )
            .await
        }
        ToolCall::EditMessage {
            chat_id,
            message_id,
            text,
        } => telegram
            .edit_message(*chat_id, *message_id, text)
            .await
            .map(|_| None),
        ToolCall::SendPoll {
            chat_id,
            question,
            options,
            is_anonymous,
            allows_multiple_answers,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            messaging::execute_send_poll(
                telegram,
                *chat_id,
                question,
                options,
                *is_anonymous,
                *allows_multiple_answers,
                reply_to,
            )
            .await
        }
        ToolCall::UnbanUser { chat_id, user_id } => telegram
            .unban_user(*chat_id, *user_id)
            .await
            .map(|_| Some(format!("Unbanned user {} from chat {}", user_id, chat_id))),
        ToolCall::SetReminder {
            chat_id,
            message,
            trigger_at,
            repeat_cron,
        } => {
            utility::execute_set_reminder(
                config,
                *chat_id,
                message,
                trigger_at,
                repeat_cron.as_deref(),
            )
            .await
        }
        ToolCall::ListReminders { chat_id } => {
            utility::execute_list_reminders(config, *chat_id).await
        }
        ToolCall::CancelReminder { reminder_id } => {
            utility::execute_cancel_reminder(config, *reminder_id).await
        }
        ToolCall::YandexGeocode { address } => {
            utility::execute_yandex_geocode(config, address).await
        }
        ToolCall::YandexMap {
            chat_id,
            address,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            utility::execute_yandex_map(config, telegram, *chat_id, address, reply_to).await
        }
        ToolCall::Now { utc_offset } => utility::execute_now(*utc_offset),
        ToolCall::ReportBug {
            description,
            severity,
        } => {
            utility::execute_report_bug(config.data_dir.as_ref(), description, severity.as_deref())
                .await
        }
        ToolCall::CreateSpreadsheet {
            chat_id,
            filename,
            sheets,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            utility::execute_create_spreadsheet(telegram, *chat_id, filename, sheets, reply_to)
                .await
        }
        ToolCall::CreatePdf {
            chat_id,
            filename,
            content,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            utility::execute_create_pdf(telegram, *chat_id, filename, content, reply_to).await
        }
        ToolCall::CreateWord {
            chat_id,
            filename,
            content,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            utility::execute_create_word(telegram, *chat_id, filename, content, reply_to).await
        }
        ToolCall::WebSearch {
            query,
            chat_id,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            match config.brave_search_api_key.as_deref() {
                None => Err("Brave Search API key not configured".to_string()),
                Some(api_key) => {
                    utility::execute_web_search(telegram, *chat_id, query, api_key, reply_to).await
                }
            }
        }
        ToolCall::RunScript {
            path,
            args,
            timeout,
        } => utility::execute_run_script(config, path, args, *timeout).await,
        ToolCall::DockerRun {
            compose_file,
            action,
        } => utility::execute_docker_run(config, compose_file, action).await,
        ToolCall::RunEval { vars, all } => utility::execute_run_eval(config, vars, *all).await,
        ToolCall::CheckExperiments { query } => utility::execute_check_experiments(query).await,
        ToolCall::CheckpointTask {
            task_id,
            checkpoint,
            status_note,
        } => planning::execute_checkpoint_task(config, task_id, checkpoint, status_note).await,
        ToolCall::ResumeTask { task_id } => planning::execute_resume_task(config, task_id).await,
        ToolCall::GetMetrics { last_n } => utility::execute_get_metrics(database, *last_n).await,
        ToolCall::CreatePlan { task_id, steps } => {
            planning::execute_create_plan(config, task_id, steps).await
        }
        ToolCall::UpdatePlanStep {
            plan_id,
            step_index,
            status,
            result,
        } => {
            planning::execute_update_plan_step(
                config,
                plan_id,
                *step_index,
                status,
                result.as_deref(),
            )
            .await
        }
        ToolCall::RevisePlan {
            plan_id,
            revised_steps,
            reason,
        } => planning::execute_revise_plan(config, plan_id, revised_steps, reason).await,
        ToolCall::VerifyHttp {
            url,
            method,
            expected_status,
            body_contains,
            timeout_secs,
        } => {
            let probe = crate::chatbot::verify::HttpProbe {
                url: url.clone(),
                method: method.clone().unwrap_or_else(|| "GET".to_string()),
                expected_status: expected_status.unwrap_or(200),
                body_contains: body_contains.clone(),
                timeout_secs: timeout_secs.unwrap_or(10),
            };
            let result = crate::chatbot::verify::run_http_probe(&probe).await;
            Ok(Some(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            ))
        }
        ToolCall::VerifyProcess {
            command,
            args,
            expected_exit_code,
            stdout_contains,
            timeout_secs,
        } => {
            if !config.full_permissions {
                Err("verify_process requires full permissions (Tier 1 only)".to_string())
            } else {
                let probe = crate::chatbot::verify::ProcessProbe {
                    command: command.clone(),
                    args: args.clone(),
                    expected_exit_code: expected_exit_code.unwrap_or(0),
                    stdout_contains: stdout_contains.clone(),
                    timeout_secs: timeout_secs.unwrap_or(30),
                };
                let result = crate::chatbot::verify::run_process_probe(&probe).await;
                Ok(Some(
                    serde_json::to_string_pretty(&result).unwrap_or_default(),
                ))
            }
        }
        ToolCall::VerifyLogs {
            log_file,
            error_patterns,
            since_minutes,
        } => {
            let data_dir = match config.data_dir.as_ref() {
                Some(d) => d,
                None => {
                    return ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: Some("No data_dir configured".to_string()),
                        is_error: true,
                        image: None,
                    };
                }
            };
            let result = crate::chatbot::verify::run_log_probe(
                data_dir,
                log_file,
                error_patterns,
                since_minutes.unwrap_or(5),
            );
            Ok(Some(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
            ))
        }
        ToolCall::DelegateTask {
            to_agent,
            task_description,
            success_criteria,
            deadline_minutes,
            priority,
        } => {
            delegation::execute_delegate_task(
                config,
                to_agent,
                task_description,
                success_criteria,
                *deadline_minutes,
                priority.as_deref(),
            )
            .await
        }
        ToolCall::RespondToHandoff {
            handoff_id,
            action,
            result_or_reason,
        } => {
            delegation::execute_respond_to_handoff(
                config,
                *handoff_id,
                action,
                result_or_reason.as_deref(),
            )
            .await
        }
        ToolCall::RequestConsensus {
            action_type,
            description,
            timeout_minutes,
        } => {
            delegation::execute_request_consensus(
                config,
                action_type,
                description,
                *timeout_minutes,
            )
            .await
        }
        ToolCall::VoteConsensus {
            request_id,
            decision,
            reason,
        } => delegation::execute_vote_consensus(config, *request_id, decision, reason).await,
        ToolCall::ListTools {} => tools_custom::execute_list_tools().await,
        ToolCall::BuildTool {
            name,
            description,
            language,
            code,
            parameters,
        } => {
            tools_custom::execute_build_tool(
                config,
                name,
                description,
                language,
                code,
                parameters.as_deref(),
            )
            .await
        }
        ToolCall::RunCustomTool {
            name,
            input_json,
            timeout_secs,
        } => {
            tools_custom::execute_run_custom_tool(
                config,
                name,
                input_json.as_deref(),
                *timeout_secs,
            )
            .await
        }
        ToolCall::Reflect {
            task_id,
            outcome,
            what_worked,
            what_failed,
            lessons,
        } => {
            reflection::execute_reflect(
                config,
                database,
                task_id.as_deref(),
                outcome,
                what_worked,
                what_failed,
                lessons,
            )
            .await
        }
        ToolCall::SelfEvaluate {
            score,
            top_failure_modes,
            improvement_actions,
            notes,
        } => {
            reflection::execute_self_evaluate(
                config,
                database,
                *score,
                top_failure_modes,
                improvement_actions,
                notes.as_deref(),
            )
            .await
        }
        ToolCall::JournalLog {
            entry_type,
            summary,
            detail,
            task_id,
            tags,
        } => {
            reflection::execute_journal_log(
                database,
                entry_type,
                summary,
                detail.as_deref(),
                task_id.as_deref(),
                tags.as_deref(),
            )
            .await
        }
        ToolCall::JournalSearch {
            query,
            entry_type: _,
            task_id,
            last_hours: _,
            limit,
        } => {
            reflection::execute_journal_search(
                database,
                query,
                task_id.as_deref(),
                limit.unwrap_or(10),
            )
            .await
        }
        ToolCall::JournalSummary {
            task_id,
            last_hours,
        } => {
            reflection::execute_journal_summary(
                database,
                task_id.as_deref(),
                last_hours.unwrap_or(24),
            )
            .await
        }
        ToolCall::GetProgress { task_id } => planning::execute_get_progress(config, task_id).await,
        ToolCall::OrchestratorStatus { task_id } => {
            planning::execute_orchestrator_status(config, task_id.as_deref()).await
        }
        ToolCall::GetSnapshots { last_n } => {
            utility::execute_get_snapshots(database, config, last_n.unwrap_or(5).min(20)).await
        }
        ToolCall::Done => Ok(None),
        ToolCall::ParseError { message } => Err(message.clone()),
    };

    // Auto-save debug state after every tool call (crash recovery)
    if let Some(ref data_dir) = config.data_dir {
        let debug_path = data_dir.join("debug_state.json");
        // Extract only the tool variant name, not field values (avoids leaking sensitive data)
        let tool_name = format!("{:?}", tc.call);
        let tool_name = tool_name
            .split(['{', '('])
            .next()
            .unwrap_or("unknown")
            .trim()
            .to_string();
        // Redact result preview — only show length and success/error, not content
        let result_preview = match &result {
            Ok(Some(s)) => format!("OK ({} chars)", s.len()),
            Ok(None) => "OK (null)".to_string(),
            Err(e) => format!("ERROR: {}", e.chars().take(100).collect::<String>()),
        };
        let debug_json = serde_json::json!({
            "last_tool": tool_name,
            "last_result_preview": result_preview,
            "is_error": result.is_err(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        let _ = std::fs::write(
            &debug_path,
            serde_json::to_string_pretty(&debug_json).unwrap_or_default(),
        );
    }

    match result {
        Ok(content) => ToolResult {
            tool_use_id: tc.id.clone(),
            content,
            is_error: false,
            image: None,
        },
        Err(e) => ToolResult {
            tool_use_id: tc.id.clone(),
            content: Some(format!("error: {}", e)),
            is_error: true,
            image: None,
        },
    }
}

/// Check if a tool call requires consensus and whether it has been obtained.
///
/// Returns `Ok(())` if execution may proceed, or `Err(message)` if blocked.
fn check_consensus_gate(
    config: &ChatbotConfig,
    tool_name: &str,
    tool_context: &str,
) -> Result<(), String> {
    let db_path = match &config.shared_bot_messages_db {
        Some(p) => p,
        None => return Ok(()),
    };

    let action_type = match tool_name {
        "RunScript" | "DockerRun" if is_deploy_command(tool_context) => Some("deploy"),
        "BanUser" => Some("ban"),
        "BuildTool" => Some("tool_build"),
        _ => None,
    };

    let action_type = match action_type {
        Some(a) => a,
        None => return Ok(()),
    };

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let has_approval: bool = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM consensus_requests
             WHERE requesting_agent = ?1 AND action_type = ?2 AND status = 'approved'
             AND datetime(resolved_at) > datetime('now', '-30 minutes')",
            rusqlite::params![config.bot_name, action_type],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if has_approval {
        return Ok(());
    }

    Err(format!(
        "BLOCKED: {} requires consensus (type: {}). Use request_consensus first, \
         then sleep and wait for approval. Once approved, retry this action.",
        tool_name, action_type
    ))
}

/// Return true if the tool context string looks like a deployment command.
fn is_deploy_command(context: &str) -> bool {
    let patterns = [
        "deploy",
        "systemctl restart",
        "systemctl reload",
        "cargo build --release",
        "git push",
        "docker compose up",
        "service restart",
        "nginx reload",
    ];
    let lower = context.to_lowercase();
    patterns.iter().any(|p| lower.contains(p))
}
