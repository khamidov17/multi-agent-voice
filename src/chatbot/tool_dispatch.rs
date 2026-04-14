//! Tool dispatch — all execute_* functions for MCP tool calls.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::chatbot::claude_code::{ToolCallWithId, ToolResult};
use crate::chatbot::context::ContextBuffer;
use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::format::strip_html_tags;
use crate::chatbot::gemini::GeminiClient;
use crate::chatbot::message::{ChatMessage, ReplyTo};
use crate::chatbot::telegram::TelegramClient;
use crate::chatbot::tools::ToolCall;
use crate::chatbot::tts::{GeminiTtsClient, TtsClient};
use crate::chatbot::yandex;

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
    let result = match &tc.call {
        ToolCall::SendMessage {
            chat_id,
            text,
            reply_to_message_id,
        } => {
            // Use Claude's explicit choice if provided, otherwise fall back to default
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_send_message(
                config, context, database, telegram, *chat_id, text, reply_to,
            )
            .await
        }
        ToolCall::GetUserInfo { user_id, username } => {
            // Handle specially to include profile photo for Claude to see
            match execute_get_user_info(config, database, telegram, *user_id, username.as_deref())
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
        ToolCall::Query { sql } => execute_query(database, sql).await,
        ToolCall::AddReaction {
            chat_id,
            message_id,
            emoji,
        } => execute_add_reaction(telegram, *chat_id, *message_id, emoji).await,
        ToolCall::DeleteMessage {
            chat_id,
            message_id,
        } => execute_delete_message(config, telegram, *chat_id, *message_id).await,
        ToolCall::MuteUser {
            chat_id,
            user_id,
            duration_minutes,
        } => execute_mute_user(config, telegram, *chat_id, *user_id, *duration_minutes).await,
        ToolCall::BanUser { chat_id, user_id } => {
            execute_ban_user(config, telegram, *chat_id, *user_id).await
        }
        ToolCall::KickUser { chat_id, user_id } => {
            execute_kick_user(config, telegram, *chat_id, *user_id).await
        }
        ToolCall::GetChatAdmins { chat_id } => execute_get_chat_admins(telegram, *chat_id).await,
        ToolCall::GetMembers {
            filter,
            days_inactive,
            limit,
        } => execute_get_members(database, filter.as_deref(), *days_inactive, *limit).await,
        ToolCall::ImportMembers { file_path } => {
            execute_import_members(database, config.data_dir.as_ref(), file_path).await
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
            match execute_send_image(
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
            execute_send_voice(config, telegram, *chat_id, text, voice.as_deref(), reply_to).await
        }
        // Memory tools
        ToolCall::CreateMemory { path, content } => {
            execute_create_memory(config.data_dir.as_ref(), path, content).await
        }
        ToolCall::ReadMemory { path } => {
            execute_read_memory(config.data_dir.as_ref(), path, memory_files_read).await
        }
        ToolCall::EditMemory {
            path,
            old_string,
            new_string,
        } => {
            execute_edit_memory(
                config.data_dir.as_ref(),
                path,
                old_string,
                new_string,
                memory_files_read,
            )
            .await
        }
        ToolCall::ListMemories { path } => {
            execute_list_memories(config.data_dir.as_ref(), path.as_deref()).await
        }
        ToolCall::SearchMemories { pattern, path } => {
            execute_search_memories(config.data_dir.as_ref(), pattern, path.as_deref()).await
        }
        ToolCall::DeleteMemory { path } => {
            execute_delete_memory(config.data_dir.as_ref(), path).await
        }
        ToolCall::FetchUrl { url } => execute_fetch_url(url).await,
        ToolCall::SendMusic {
            chat_id,
            prompt,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_send_music(config, telegram, *chat_id, prompt, reply_to).await
        }
        ToolCall::SendFile {
            chat_id,
            file_path,
            caption,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_send_file(
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
            execute_send_poll(
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
            execute_set_reminder(
                config,
                *chat_id,
                message,
                trigger_at,
                repeat_cron.as_deref(),
            )
            .await
        }
        ToolCall::ListReminders { chat_id } => execute_list_reminders(config, *chat_id).await,
        ToolCall::CancelReminder { reminder_id } => {
            execute_cancel_reminder(config, *reminder_id).await
        }
        ToolCall::YandexGeocode { address } => execute_yandex_geocode(config, address).await,
        ToolCall::YandexMap {
            chat_id,
            address,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_yandex_map(config, telegram, *chat_id, address, reply_to).await
        }
        ToolCall::Now { utc_offset } => execute_now(*utc_offset),
        ToolCall::ReportBug {
            description,
            severity,
        } => execute_report_bug(config.data_dir.as_ref(), description, severity.as_deref()).await,
        ToolCall::CreateSpreadsheet {
            chat_id,
            filename,
            sheets,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_create_spreadsheet(telegram, *chat_id, filename, sheets, reply_to).await
        }
        ToolCall::CreatePdf {
            chat_id,
            filename,
            content,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_create_pdf(telegram, *chat_id, filename, content, reply_to).await
        }
        ToolCall::CreateWord {
            chat_id,
            filename,
            content,
            reply_to_message_id,
        } => {
            let reply_to = reply_to_message_id.or(default_reply_to);
            execute_create_word(telegram, *chat_id, filename, content, reply_to).await
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
                    execute_web_search(telegram, *chat_id, query, api_key, reply_to).await
                }
            }
        }
        ToolCall::RunScript {
            path,
            args,
            timeout,
        } => execute_run_script(config, path, args, *timeout).await,
        ToolCall::DockerRun {
            compose_file,
            action,
        } => execute_docker_run(config, compose_file, action).await,
        ToolCall::RunEval { vars, all } => execute_run_eval(config, vars, *all).await,
        ToolCall::CheckExperiments { query } => execute_check_experiments(query).await,
        ToolCall::CheckpointTask {
            task_id,
            checkpoint,
            status_note,
        } => execute_checkpoint_task(config, task_id, checkpoint, status_note).await,
        ToolCall::ResumeTask { task_id } => execute_resume_task(config, task_id).await,
        ToolCall::Done => Ok(None),
        ToolCall::ParseError { message } => Err(message.clone()),
    };

    // Auto-save debug state after every tool call (crash recovery)
    if let Some(ref data_dir) = config.data_dir {
        let debug_path = data_dir.join("debug_state.json");
        // Extract only the tool variant name, not field values (avoids leaking sensitive data)
        let tool_name = format!("{:?}", tc.call);
        let tool_name = tool_name
            .split(|c: char| c == '{' || c == '(')
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

async fn execute_send_message(
    config: &ChatbotConfig,
    context: &Mutex<ContextBuffer>,
    database: &Mutex<Database>,
    telegram: &TelegramClient,
    chat_id: i64,
    text: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    // STRICT ENFORCEMENT: groups/channels must be in allowed list.
    // Positive chat_ids (DMs) are always allowed.
    if chat_id < 0 && !config.allowed_chat_ids.contains(&chat_id) {
        warn!("🚫 Blocked send_message to unauthorized chat {}", chat_id);
        return Err(format!(
            "Unauthorized chat {}. I am not allowed to send messages to groups or channels that are not in my approved list. Only the owner can authorize me to join new chats.",
            chat_id
        ));
    }

    let preview: String = text.chars().take(50).collect();
    info!("📤 Sending to {}: \"{}\"", chat_id, preview);

    // Validate reply target
    let validated_reply = if let Some(reply_id) = reply_to_message_id {
        let ctx = context.lock().await;
        if let Some(orig) = ctx.get_message(reply_id) {
            if orig.chat_id == chat_id {
                Some(reply_id)
            } else {
                warn!("Reply {} is from different chat, dropping", reply_id);
                None
            }
        } else {
            Some(reply_id) // Not in context, let Telegram decide
        }
    } else {
        None
    };

    let msg_id = telegram
        .send_message(chat_id, text, validated_reply)
        .await?;
    info!("✅ Sent message {} to chat {}", msg_id, chat_id);

    // Build reply info
    let reply_to = if let Some(reply_id) = validated_reply {
        let ctx = context.lock().await;
        ctx.get_message(reply_id).map(|orig| ReplyTo {
            message_id: reply_id,
            username: orig.username.clone(),
            text: orig.text.clone(),
        })
    } else {
        None
    };

    // Store bot's message
    let bot_msg = ChatMessage {
        message_id: msg_id,
        chat_id,
        user_id: config.bot_user_id,
        username: "Atlas".to_string(),
        first_name: None,
        timestamp: chrono::Utc::now().format("%H:%M").to_string(),
        text: text.to_string(),
        reply_to,
        photo_file_id: None,
        image: None,
        voice_transcription: None,
    };

    {
        let mut ctx = context.lock().await;
        ctx.add_message(bot_msg.clone());
    }
    {
        let mut store = database.lock().await;
        store.add_message(bot_msg);
    }

    // Write to the shared bot-message bus so peer bots (Nova, Security) can
    // see this message — Telegram does not deliver bot messages to other bots.
    // Only broadcast group messages; DMs (positive chat_id) are private.
    if chat_id < 0
        && let Some(ref db_path) = config.shared_bot_messages_db
    {
        match crate::chatbot::bot_messages::BotMessageDb::open(db_path) {
            Ok(bus) => {
                if let Err(e) = bus.insert(
                    &config.bot_name,
                    None, // broadcast — all peer bots receive it
                    text,
                    validated_reply,
                    Some(msg_id),
                ) {
                    error!("BotMessageDb insert failed: {e}");
                } else {
                    debug!(
                        "BotMessageDb: published msg_id={} from {}",
                        msg_id, config.bot_name
                    );
                }
            }
            Err(e) => error!("BotMessageDb open failed during send: {e}"),
        }
    }

    Ok(Some(format!("sent (message_id: {})", msg_id)))
}

/// Returns (json_info, optional_profile_photo_bytes)
async fn execute_get_user_info(
    config: &ChatbotConfig,
    database: &Mutex<Database>,
    telegram: &TelegramClient,
    user_id: Option<i64>,
    username: Option<&str>,
) -> Result<(String, Option<Vec<u8>>), String> {
    // Resolve user_id from username if needed
    let resolved_id = if let Some(id) = user_id {
        id
    } else if let Some(name) = username {
        let db = database.lock().await;
        db.find_user_by_username(name)
            .map(|m| m.user_id)
            .ok_or_else(|| format!("User '{}' not found in database", name))?
    } else {
        return Err("get_user_info requires user_id or username".to_string());
    };

    let info = telegram
        .get_chat_member(config.primary_chat_id, resolved_id)
        .await?;

    // Try to get profile photo
    let profile_photo = match telegram.get_profile_photo(resolved_id).await {
        Ok(photo) => photo,
        Err(e) => {
            warn!("Failed to get profile photo: {e}");
            None
        }
    };

    let json_info = serde_json::json!({
        "user_id": info.user_id,
        "username": info.username,
        "first_name": info.first_name,
        "last_name": info.last_name,
        "is_bot": info.is_bot,
        "is_premium": info.is_premium,
        "language_code": info.language_code,
        "status": info.status,
        "custom_title": info.custom_title,
        "has_profile_photo": profile_photo.is_some()
    })
    .to_string();

    Ok((json_info, profile_photo))
}

async fn execute_query(database: &Mutex<Database>, sql: &str) -> Result<Option<String>, String> {
    let store = database.lock().await;
    let preview: String = sql.chars().take(80).collect();
    info!("📚 Executing query: {}", preview);
    let result = store.query(sql)?;
    Ok(Some(result))
}

async fn execute_add_reaction(
    telegram: &TelegramClient,
    chat_id: i64,
    message_id: i64,
    emoji: &str,
) -> Result<Option<String>, String> {
    telegram
        .set_message_reaction(chat_id, message_id, emoji)
        .await?;
    Ok(None) // Action tool
}

/// Execute delete message and notify owner.
async fn execute_delete_message(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    message_id: i64,
) -> Result<Option<String>, String> {
    telegram.delete_message(chat_id, message_id).await?;

    // Notify owner
    if let Some(owner_id) = config.owner_user_id {
        let _ = telegram
            .send_message(
                owner_id,
                &format!("🗑️ Deleted message {} in chat {}", message_id, chat_id),
                None,
            )
            .await;
    }

    Ok(None) // Action tool
}

/// Execute mute user and notify owner.
async fn execute_mute_user(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    user_id: i64,
    duration_minutes: i64,
) -> Result<Option<String>, String> {
    // Clamp duration to 1-1440 minutes
    let duration = duration_minutes.clamp(1, 1440);

    telegram.mute_user(chat_id, user_id, duration).await?;

    // Notify owner
    if let Some(owner_id) = config.owner_user_id {
        let _ = telegram
            .send_message(
                owner_id,
                &format!(
                    "🔇 Muted user {} for {} min in chat {}",
                    user_id, duration, chat_id
                ),
                None,
            )
            .await;
    }

    Ok(None) // Action tool
}

/// Execute ban user and notify owner.
async fn execute_ban_user(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    user_id: i64,
) -> Result<Option<String>, String> {
    telegram.ban_user(chat_id, user_id).await?;

    // Notify owner
    if let Some(owner_id) = config.owner_user_id {
        let _ = telegram
            .send_message(
                owner_id,
                &format!("🚫 Banned user {} from chat {}", user_id, chat_id),
                None,
            )
            .await;
    }

    Ok(None) // Action tool
}

/// Execute kick user (unban immediately so they can rejoin) and notify owner.
async fn execute_kick_user(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    user_id: i64,
) -> Result<Option<String>, String> {
    telegram.kick_user(chat_id, user_id).await?;

    // Notify owner
    if let Some(owner_id) = config.owner_user_id {
        let _ = telegram
            .send_message(
                owner_id,
                &format!("👢 Kicked user {} from chat {}", user_id, chat_id),
                None,
            )
            .await;
    }

    Ok(None) // Action tool
}

/// Get list of chat administrators.
async fn execute_get_chat_admins(
    telegram: &TelegramClient,
    chat_id: i64,
) -> Result<Option<String>, String> {
    let admins = telegram.get_chat_admins(chat_id).await?;
    Ok(Some(admins))
}

/// Get members from database with optional filter.
async fn execute_get_members(
    database: &Mutex<Database>,
    filter: Option<&str>,
    days_inactive: Option<i64>,
    limit: Option<i64>,
) -> Result<Option<String>, String> {
    let db = database.lock().await;
    let limit = limit.unwrap_or(50) as usize;
    let members = db.get_members(filter, days_inactive, limit);

    let result: Vec<serde_json::Value> = members
        .iter()
        .map(|m| {
            serde_json::json!({
                "user_id": m.user_id,
                "username": m.username,
                "first_name": m.first_name,
                "join_date": m.join_date,
                "last_message_date": m.last_message_date,
                "message_count": m.message_count,
                "status": format!("{:?}", m.status).to_lowercase(),
            })
        })
        .collect();

    let total = db.total_members_seen();
    let active = db.member_count();

    Ok(Some(
        serde_json::json!({
            "total_tracked": total,
            "active_members": active,
            "filter": filter.unwrap_or("all"),
            "results": result,
        })
        .to_string(),
    ))
}

/// Import members from a JSON file.
/// Security: Only allows reading files within data_dir to prevent path traversal.
async fn execute_import_members(
    database: &Mutex<Database>,
    data_dir: Option<&PathBuf>,
    file_path: &str,
) -> Result<Option<String>, String> {
    info!("📥 Importing members from: {}", file_path);

    // Security: Validate file path is within data_dir
    let allowed_dir = data_dir.ok_or("No data_dir configured - import disabled")?;

    let requested_path = PathBuf::from(file_path);
    let canonical_path = requested_path
        .canonicalize()
        .map_err(|e| format!("Invalid path: {e}"))?;
    let canonical_dir = allowed_dir
        .canonicalize()
        .map_err(|e| format!("Invalid data_dir: {e}"))?;

    if !canonical_path.starts_with(&canonical_dir) {
        return Err(format!(
            "Security: Path must be within data directory. Got: {}",
            file_path
        ));
    }

    let json = std::fs::read_to_string(&canonical_path)
        .map_err(|e| format!("Failed to read file: {e}"))?;

    let mut db = database.lock().await;
    let count = db.import_members(&json)?;

    Ok(Some(
        serde_json::json!({
            "imported": count,
            "total_members": db.total_members_seen(),
        })
        .to_string(),
    ))
}

async fn execute_send_image(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    prompt: &str,
    caption: Option<&str>,
    reply_to_message_id: Option<i64>,
    source_image_file_id: Option<&str>,
) -> Result<(Vec<u8>, i64), String> {
    let api_key = config
        .gemini_api_key
        .as_ref()
        .ok_or("Gemini API key not configured")?;

    let gemini = GeminiClient::new(api_key.clone());

    let image_data = if let Some(file_id) = source_image_file_id {
        info!("🎨 Editing image (file_id: {}): {}", file_id, prompt);
        let (source_bytes, mime_type) = telegram.download_image(file_id).await?;
        gemini
            .edit_image(prompt, &source_bytes, &mime_type)
            .await?
            .data
    } else {
        info!("🎨 Generating image: {}", prompt);
        gemini.generate_image(prompt).await?.data
    };

    let data_clone = image_data.clone();
    let msg_id = telegram
        .send_image(chat_id, image_data, caption, reply_to_message_id)
        .await?;

    Ok((data_clone, msg_id))
}

async fn execute_send_voice(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    text: &str,
    voice: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    let preview: String = text.chars().take(50).collect();
    info!("🔊 TTS: \"{}\"", preview);

    let voice_data = if let Some(endpoint) = config.tts_endpoint.as_ref() {
        // Use local XTTS endpoint if configured
        let tts = TtsClient::new(endpoint.clone());
        tts.synthesize(text, voice).await?
    } else if let Some(api_key) = config.gemini_api_key.as_ref() {
        // Fall back to Gemini TTS
        let tts = GeminiTtsClient::new(api_key.clone());
        tts.synthesize(text, voice).await?
    } else {
        // Fallback: send as text message when TTS is unavailable
        warn!("TTS not configured — falling back to text message");
        let msg_id = telegram
            .send_message(chat_id, &format!("🔊 {text}"), reply_to_message_id)
            .await
            .map_err(|e| format!("TTS fallback failed: {e}"))?;
        return Ok(Some(format!(
            "Voice unavailable, sent as text (msg_id: {})",
            msg_id
        )));
    };

    let msg_id = match telegram
        .send_voice(chat_id, voice_data, None, reply_to_message_id)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            // Fallback: send as text if voice delivery fails
            warn!("Voice send failed: {e} — falling back to text");
            return telegram
                .send_message(chat_id, &format!("🔊 {text}"), reply_to_message_id)
                .await
                .map(|id| Some(format!("Voice failed, sent as text (msg_id: {})", id)))
                .map_err(|e2| format!("Both voice and text failed: {e2}"));
        }
    };

    Ok(Some(format!(
        "Voice message sent to chat {} (message_id: {})",
        chat_id, msg_id
    )))
}

// === Memory Tool Implementations ===

/// Validate and resolve a memory path. Returns the full path if valid.
pub(crate) fn resolve_memory_path(
    data_dir: Option<&PathBuf>,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let data_dir = data_dir.ok_or("No data_dir configured - memories disabled")?;
    let memories_dir = data_dir.join("memories");

    // Security: reject paths with .. or absolute paths
    if relative_path.contains("..") {
        return Err("Path cannot contain '..'".to_string());
    }
    if relative_path.starts_with('/') || relative_path.starts_with('\\') {
        return Err("Path must be relative".to_string());
    }
    if relative_path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let full_path = memories_dir.join(relative_path);

    // Double-check: canonicalize and verify it's still within memories_dir
    // For non-existent files, canonicalize the parent
    let parent = full_path.parent().ok_or("Invalid path")?;

    // Create memories directory structure if needed
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| format!("Failed to resolve path: {e}"))?;
    let canonical_memories = memories_dir.canonicalize().unwrap_or_else(|_| {
        // memories dir might not exist yet
        std::fs::create_dir_all(&memories_dir).ok();
        memories_dir.canonicalize().unwrap_or(memories_dir.clone())
    });

    if !canonical_parent.starts_with(&canonical_memories) {
        return Err("Path must be within memories directory".to_string());
    }

    Ok(full_path)
}

async fn execute_create_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
    content: &str,
) -> Result<Option<String>, String> {
    let full_path = resolve_memory_path(data_dir, path)?;

    // Fail if file already exists
    if full_path.exists() {
        return Err(format!(
            "File already exists: {}. Use edit_memory to modify.",
            path
        ));
    }

    debug!("📝 Creating memory: {}", path);
    std::fs::write(&full_path, content).map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(None) // Action tool
}

async fn execute_read_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
    files_read: &mut HashSet<String>,
) -> Result<Option<String>, String> {
    let full_path = resolve_memory_path(data_dir, path)?;

    if !full_path.exists() {
        return Err(format!("File not found: {}", path));
    }

    debug!("📖 Reading memory: {}", path);
    let content =
        std::fs::read_to_string(&full_path).map_err(|e| format!("Failed to read file: {e}"))?;

    // Track that this file has been read (for edit validation)
    files_read.insert(path.to_string());

    // Format with line numbers like Claude Code's Read tool
    let numbered: String = content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:>5}→{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Some(numbered)) // Query tool - Claude needs to see the content
}

async fn execute_edit_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
    old_string: &str,
    new_string: &str,
    files_read: &HashSet<String>,
) -> Result<Option<String>, String> {
    // Must have read the file first
    if !files_read.contains(path) {
        return Err(format!("Must read_memory('{}') before editing", path));
    }

    let full_path = resolve_memory_path(data_dir, path)?;

    if !full_path.exists() {
        return Err(format!("File not found: {}", path));
    }

    let content =
        std::fs::read_to_string(&full_path).map_err(|e| format!("Failed to read file: {e}"))?;

    // Find and replace
    let count = content.matches(old_string).count();
    if count == 0 {
        return Err("old_string not found in file. Make sure it matches exactly.".to_string());
    }
    if count > 1 {
        return Err(format!("old_string found {} times. Must be unique.", count));
    }

    debug!("✏️ Editing memory: {}", path);
    let new_content = content.replace(old_string, new_string);
    std::fs::write(&full_path, &new_content).map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(None) // Action tool
}

async fn execute_list_memories(
    data_dir: Option<&PathBuf>,
    subpath: Option<&str>,
) -> Result<Option<String>, String> {
    let data_dir = data_dir.ok_or("No data_dir configured - memories disabled")?;
    let memories_dir = data_dir.join("memories");

    let target_dir = if let Some(sub) = subpath {
        resolve_memory_path(Some(data_dir), sub)?
    } else {
        if !memories_dir.exists() {
            std::fs::create_dir_all(&memories_dir)
                .map_err(|e| format!("Failed to create memories directory: {e}"))?;
        }
        memories_dir
    };

    if !target_dir.is_dir() {
        return Err(format!("Not a directory: {}", subpath.unwrap_or(".")));
    }

    debug!("📂 Listing memories: {}", subpath.unwrap_or("."));
    let mut entries = Vec::new();
    for entry in
        std::fs::read_dir(&target_dir).map_err(|e| format!("Failed to read directory: {e}"))?
    {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{}/", name) } else { name });
    }
    entries.sort();

    Ok(Some(entries.join("\n"))) // Query tool - Claude needs to see the listing
}

async fn execute_search_memories(
    data_dir: Option<&PathBuf>,
    pattern: &str,
    subpath: Option<&str>,
) -> Result<Option<String>, String> {
    let data_dir = data_dir.ok_or("No data_dir configured - memories disabled")?;
    let memories_dir = data_dir.join("memories");

    let search_dir = if let Some(sub) = subpath {
        resolve_memory_path(Some(data_dir), sub)?
    } else {
        if !memories_dir.exists() {
            return Ok(Some("No memories directory yet".to_string()));
        }
        memories_dir.clone()
    };

    debug!("🔍 Searching memories for: {}", pattern);
    let mut results = Vec::new();

    fn search_recursive(
        dir: &PathBuf,
        base: &PathBuf,
        pattern: &str,
        results: &mut Vec<String>,
    ) -> Result<(), String> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir).map_err(|e| format!("Read dir error: {e}"))? {
            let entry = entry.map_err(|e| format!("Entry error: {e}"))?;
            let path = entry.path();
            if path.is_dir() {
                search_recursive(&path, base, pattern, results)?;
            } else if path.is_file()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let rel_path = path.strip_prefix(base).unwrap_or(&path);
                for (line_num, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        results.push(format!("{}:{}:{}", rel_path.display(), line_num + 1, line));
                    }
                }
            }
        }
        Ok(())
    }

    search_recursive(&search_dir, &memories_dir, pattern, &mut results)?;

    if results.is_empty() {
        Ok(Some("No matches found".to_string()))
    } else {
        Ok(Some(results.join("\n")))
    }
}

async fn execute_delete_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
) -> Result<Option<String>, String> {
    let full_path = resolve_memory_path(data_dir, path)?;

    if !full_path.exists() {
        return Err(format!("File not found: {}", path));
    }

    if full_path.is_dir() {
        return Err("Cannot delete directories. Delete files individually.".to_string());
    }

    debug!("🗑️ Deleting memory: {}", path);
    std::fs::remove_file(&full_path).map_err(|e| format!("Failed to delete file: {e}"))?;

    Ok(None) // Action tool
}

/// Report a bug to the developer feedback file.
async fn execute_report_bug(
    data_dir: Option<&PathBuf>,
    description: &str,
    severity: Option<&str>,
) -> Result<Option<String>, String> {
    let data_dir = data_dir.ok_or("No data_dir configured")?;
    let feedback_file = data_dir.join("feedback.log");

    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let severity = severity.unwrap_or("medium");

    let entry = format!(
        "\n---\n[{}] severity={}\n{}\n",
        timestamp, severity, description
    );

    let preview: String = description.chars().take(50).collect();
    info!("🐛 Bug report ({}): {}", severity, preview);

    // Append to feedback file
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&feedback_file)
        .map_err(|e| format!("Failed to open feedback file: {e}"))?;

    file.write_all(entry.as_bytes())
        .map_err(|e| format!("Failed to write feedback: {e}"))?;

    Ok(None) // Action tool - developer will see it via the poller
}

async fn execute_send_poll(
    telegram: &TelegramClient,
    chat_id: i64,
    question: &str,
    options: &[String],
    is_anonymous: bool,
    allows_multiple_answers: bool,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    if options.len() < 2 || options.len() > 10 {
        return Err(format!(
            "send_poll requires 2-10 options, got {}",
            options.len()
        ));
    }
    let msg_id = telegram
        .send_poll(
            chat_id,
            question,
            options,
            is_anonymous,
            allows_multiple_answers,
        )
        .await?;
    // Reply support requires a separate message — Telegram polls can't use reply_parameters directly
    // but we can at minimum forward if it was requested
    let _ = reply_to_message_id; // accepted but not applicable to polls in teloxide
    Ok(Some(format!("Poll sent (message_id: {})", msg_id)))
}

async fn execute_set_reminder(
    config: &ChatbotConfig,
    chat_id: i64,
    message: &str,
    trigger_at_str: &str,
    repeat_cron: Option<&str>,
) -> Result<Option<String>, String> {
    use crate::chatbot::reminders::parse_trigger_at;
    let store = config
        .reminder_store
        .as_ref()
        .ok_or("Reminder store not configured")?;
    let trigger_at = parse_trigger_at(trigger_at_str)?;
    let id = store.set(chat_id, 0, message, trigger_at, repeat_cron)?;
    let human = trigger_at.format("%Y-%m-%d %H:%M UTC").to_string();
    info!("⏰ Reminder {} set for {} at {}", id, chat_id, human);
    Ok(Some(format!("Reminder #{id} set — will fire at {human}")))
}

async fn execute_list_reminders(
    config: &ChatbotConfig,
    chat_id: Option<i64>,
) -> Result<Option<String>, String> {
    let store = config
        .reminder_store
        .as_ref()
        .ok_or("Reminder store not configured")?;
    let reminders = store.list(chat_id)?;
    if reminders.is_empty() {
        return Ok(Some("No active reminders.".to_string()));
    }
    let lines: Vec<String> = reminders
        .iter()
        .map(|r| {
            let repeat = r
                .repeat_cron
                .as_deref()
                .map(|c| format!(" (repeat: {c})"))
                .unwrap_or_default();
            format!(
                "#{}: chat={} at {}{} — {}",
                r.id,
                r.chat_id,
                r.trigger_at.format("%Y-%m-%d %H:%M UTC"),
                repeat,
                r.message
            )
        })
        .collect();
    Ok(Some(lines.join("\n")))
}

async fn execute_cancel_reminder(
    config: &ChatbotConfig,
    reminder_id: i64,
) -> Result<Option<String>, String> {
    let store = config
        .reminder_store
        .as_ref()
        .ok_or("Reminder store not configured")?;
    if store.cancel(reminder_id)? {
        Ok(Some(format!("Reminder #{reminder_id} cancelled.")))
    } else {
        Err(format!(
            "Reminder #{reminder_id} not found or already inactive."
        ))
    }
}

async fn execute_yandex_geocode(
    config: &ChatbotConfig,
    address: &str,
) -> Result<Option<String>, String> {
    let key = config
        .yandex_api_key
        .as_deref()
        .ok_or("Yandex API key not configured")?;
    let (name, lon, lat) = yandex::geocode(address, key).await?;
    Ok(Some(format!(
        "📍 {name}\nCoordinates: {lat:.6}, {lon:.6} (lat, lon)"
    )))
}

async fn execute_yandex_map(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    address: &str,
    reply_to: Option<i64>,
) -> Result<Option<String>, String> {
    let key = config
        .yandex_api_key
        .as_deref()
        .ok_or("Yandex API key not configured")?;
    let (name, lon, lat) = yandex::geocode(address, key).await?;
    let image = yandex::static_map(lon, lat, key, 15).await?;
    telegram
        .send_image(chat_id, image, Some(&name), reply_to)
        .await?;
    Ok(None)
}

fn execute_now(utc_offset: Option<i32>) -> Result<Option<String>, String> {
    let offset_hours = utc_offset.unwrap_or(0).clamp(-12, 14);
    let now = chrono::Utc::now();
    let offset = chrono::Duration::hours(offset_hours as i64);
    let local = now + offset;
    let sign = if offset_hours >= 0 { "+" } else { "" };
    Ok(Some(format!(
        "Current time: {} (UTC{sign}{offset_hours})",
        local.format("%Y-%m-%d %H:%M:%S")
    )))
}

async fn execute_create_spreadsheet(
    telegram: &TelegramClient,
    chat_id: i64,
    filename: &str,
    sheets: &[serde_json::Value],
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    use rust_xlsxwriter::Workbook;

    info!("📊 Creating spreadsheet: {}", filename);

    let mut workbook = Workbook::new();

    for sheet_val in sheets {
        let name = sheet_val
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Sheet");
        let headers = sheet_val
            .get("headers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|h| h.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let rows = sheet_val
            .get("rows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let worksheet = workbook.add_worksheet();
        worksheet
            .set_name(name)
            .map_err(|e| format!("Invalid sheet name: {e}"))?;

        // Write headers in row 0
        for (col, header) in headers.iter().enumerate() {
            worksheet
                .write_string(0, col as u16, header)
                .map_err(|e| format!("Failed to write header: {e}"))?;
        }

        // Write data rows starting at row 1
        for (row_idx, row) in rows.iter().enumerate() {
            if let Some(cells) = row.as_array() {
                for (col, cell) in cells.iter().enumerate() {
                    let row_num = (row_idx + 1) as u32;
                    let col_num = col as u16;
                    match cell {
                        serde_json::Value::Number(n) => {
                            if let Some(f) = n.as_f64() {
                                worksheet
                                    .write_number(row_num, col_num, f)
                                    .map_err(|e| format!("Failed to write number: {e}"))?;
                            }
                        }
                        serde_json::Value::Bool(b) => {
                            worksheet
                                .write_boolean(row_num, col_num, *b)
                                .map_err(|e| format!("Failed to write bool: {e}"))?;
                        }
                        serde_json::Value::Null => {}
                        other => {
                            worksheet
                                .write_string(
                                    row_num,
                                    col_num,
                                    other.to_string().trim_matches('"').to_string(),
                                )
                                .map_err(|e| format!("Failed to write cell: {e}"))?;
                        }
                    }
                }
            }
        }
    }

    let xlsx_bytes = workbook
        .save_to_buffer()
        .map_err(|e| format!("Failed to save workbook: {e}"))?;
    info!("📊 Spreadsheet created: {} bytes", xlsx_bytes.len());

    let caption = format!("📊 {}", filename);
    telegram
        .send_document(
            chat_id,
            xlsx_bytes,
            filename,
            Some(&caption),
            reply_to_message_id,
        )
        .await?;

    Ok(Some(format!(
        "Spreadsheet '{}' sent successfully.",
        filename
    )))
}

async fn execute_create_pdf(
    telegram: &TelegramClient,
    chat_id: i64,
    filename: &str,
    content: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    use std::process::Command;

    info!("📄 Creating PDF: {}", filename);

    let temp_dir = std::env::temp_dir();
    let html_path = temp_dir.join(format!("atlas_pdf_{}.html", std::process::id()));
    let pdf_path = temp_dir.join(format!("atlas_pdf_{}.pdf", std::process::id()));

    std::fs::write(&html_path, content.as_bytes())
        .map_err(|e| format!("Failed to write HTML temp file: {e}"))?;

    let output = Command::new("wkhtmltopdf")
        .args([
            "--quiet",
            html_path.to_str().unwrap(),
            pdf_path.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| format!("wkhtmltopdf not found (install wkhtmltopdf): {e}"))?;

    let _ = std::fs::remove_file(&html_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wkhtmltopdf failed: {}", stderr));
    }

    let pdf_bytes =
        std::fs::read(&pdf_path).map_err(|e| format!("Failed to read PDF output: {e}"))?;
    let _ = std::fs::remove_file(&pdf_path);

    info!("📄 PDF created: {} bytes", pdf_bytes.len());

    let caption = format!("📄 {}", filename);
    telegram
        .send_document(
            chat_id,
            pdf_bytes,
            filename,
            Some(&caption),
            reply_to_message_id,
        )
        .await?;

    Ok(Some(format!("PDF '{}' sent successfully.", filename)))
}

async fn execute_create_word(
    telegram: &TelegramClient,
    chat_id: i64,
    filename: &str,
    content: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    use std::process::Command;

    info!("📝 Creating Word doc: {}", filename);

    let temp_dir = std::env::temp_dir();
    let md_path = temp_dir.join(format!("atlas_word_{}.md", std::process::id()));
    let docx_path = temp_dir.join(format!("atlas_word_{}.docx", std::process::id()));

    std::fs::write(&md_path, content.as_bytes())
        .map_err(|e| format!("Failed to write Markdown temp file: {e}"))?;

    let output = Command::new("pandoc")
        .args([md_path.to_str().unwrap(), "-o", docx_path.to_str().unwrap()])
        .output()
        .map_err(|e| format!("pandoc not found (install pandoc): {e}"))?;

    let _ = std::fs::remove_file(&md_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pandoc failed: {}", stderr));
    }

    let docx_bytes =
        std::fs::read(&docx_path).map_err(|e| format!("Failed to read DOCX output: {e}"))?;
    let _ = std::fs::remove_file(&docx_path);

    info!("📝 DOCX created: {} bytes", docx_bytes.len());

    let caption = format!("📝 {}", filename);
    telegram
        .send_document(
            chat_id,
            docx_bytes,
            filename,
            Some(&caption),
            reply_to_message_id,
        )
        .await?;

    Ok(Some(format!(
        "Word document '{}' sent successfully.",
        filename
    )))
}

async fn execute_web_search(
    telegram: &TelegramClient,
    chat_id: i64,
    query: &str,
    api_key: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    info!("🔍 Web search: {}", query);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", "5")])
        .send()
        .await
        .map_err(|e| format!("Brave Search request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Brave Search API error {status}: {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Brave Search response: {e}"))?;

    let results = data["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    let title = r["title"].as_str().unwrap_or("");
                    let url = r["url"].as_str().unwrap_or("");
                    let desc = r["description"].as_str().unwrap_or("");
                    format!("<b>{}</b>\n{}\n{}", title, url, desc)
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();

    if results.is_empty() {
        return Ok(Some("No results found.".to_string()));
    }

    let text = format!("🔍 <b>{}</b>\n\n{}", query, results);
    info!("🔍 Search results: {} chars", text.len());

    telegram
        .send_message(chat_id, &text, reply_to_message_id)
        .await
        .map_err(|e| format!("Failed to send search results: {e}"))?;

    Ok(Some(format!("Search results for '{}' sent.", query)))
}

/// Check if an IP address is private/internal (SSRF protection layer 9).
pub(crate) fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()                         // 127.0.0.0/8
                || v4.is_private()                   // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || v4.is_link_local()                // 169.254.0.0/16 (AWS metadata etc.)
                || v4.is_broadcast()                 // 255.255.255.255
                || v4.is_unspecified()               // 0.0.0.0
                || v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127 // 100.64.0.0/10 (CGNAT)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()                         // ::1
                || v6.is_unspecified()               // ::
                || {
                    let segs = v6.segments();
                    (segs[0] >> 9) == 0x7e               // fc00::/7 (full ULA range)
                        || (segs[0] & 0xffc0) == 0xfe80  // fe80::/10 (full link-local)
                        || (segs[0] == 0x2001 && segs[1] == 0x0db8)  // 2001:db8::/32 (documentation)
                }
                // IPv4-mapped IPv6 (::ffff:x.x.x.x) — check the inner v4 address
                || v6.to_ipv4_mapped()
                    .map(|v4| is_private_ip(&std::net::IpAddr::V4(v4)))
                    .unwrap_or(false)
        }
    }
}

/// Validate a URL is safe to fetch (no SSRF into internal networks).
pub(crate) async fn validate_url_ssrf(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // Only allow http/https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(format!(
                "Blocked scheme: {scheme} (only http/https allowed)"
            ));
        }
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Resolve DNS and check all IPs
    use tokio::net::lookup_host;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> = lookup_host(&addr)
        .await
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("No DNS records for {host}"));
    }

    for addr in &addrs {
        if is_private_ip(&addr.ip()) {
            warn!("SSRF blocked: {url} resolves to private IP {}", addr.ip());
            return Err(format!(
                "Blocked: URL resolves to private/internal IP ({})",
                addr.ip()
            ));
        }
    }

    Ok(())
}

async fn execute_fetch_url(url: &str) -> Result<Option<String>, String> {
    info!("🌐 Fetching URL: {}", url);

    // SSRF protection: validate URL before fetching
    validate_url_ssrf(url).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; Atlas/1.0)")
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch URL: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} for {url}"));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Handle PDF: detect by Content-Type or URL extension
    if content_type.contains("pdf") || url.to_lowercase().ends_with(".pdf") {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read PDF bytes: {e}"))?;
        let text = crate::chatbot::document::extract_pdf(&bytes)
            .map_err(|e| format!("PDF text extraction failed: {e}"))?;
        let preview: String = text.chars().take(80).collect();
        info!(
            "🌐 Fetched PDF from {}: {} chars, preview: \"{}\"...",
            url,
            text.len(),
            preview
        );
        return Ok(Some(text));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let text = if content_type.contains("html") || body.trim_start().starts_with('<') {
        strip_html_tags(&body)
    } else {
        body
    };

    // Truncate to ~8000 chars (UTF-8 safe — never split mid-character)
    let result = if text.chars().count() > 8000 {
        let truncated: String = text.chars().take(8000).collect();
        format!("{truncated}...[truncated at 8000 chars]")
    } else {
        text
    };

    let preview: String = result.chars().take(80).collect();
    info!(
        "🌐 Fetched {} bytes from {}: \"{}\"...",
        result.len(),
        url,
        preview
    );

    Ok(Some(result))
}

/// Execute a script file (run_script tool).
/// Scripts must be inside workspace/ or scripts/ directory for security.
async fn execute_run_script(
    config: &ChatbotConfig,
    path: &str,
    args: &[String],
    timeout: u64,
) -> Result<Option<String>, String> {
    // Security: only full_permissions bots can run scripts
    if !config.full_permissions {
        return Err("run_script requires full permissions (Tier 1 only)".to_string());
    }

    // Security: canonicalize path and verify it's inside workspace/ or scripts/
    // Prevents ../traversal and symlink escapes.
    let script_path = std::path::Path::new(path);
    if !script_path.exists() {
        return Err(format!("Script not found: {path}"));
    }

    let canonical = script_path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve script path: {e}"))?;
    let cwd = std::env::current_dir().map_err(|e| format!("Cannot get cwd: {e}"))?;
    let workspace_dir = cwd
        .join("workspace")
        .canonicalize()
        .unwrap_or_else(|_| cwd.join("workspace"));
    let scripts_dir = cwd
        .join("scripts")
        .canonicalize()
        .unwrap_or_else(|_| cwd.join("scripts"));

    if !canonical.starts_with(&workspace_dir) && !canonical.starts_with(&scripts_dir) {
        return Err(format!(
            "Security: script {} resolves to {} which is outside workspace/ and scripts/",
            path,
            canonical.display()
        ));
    }

    let timeout_secs = timeout.min(300); // cap at 5 min
    info!(
        "Running script: {} {:?} (timeout={}s)",
        path, args, timeout_secs
    );

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg(path);
    for arg in args {
        cmd.arg(arg);
    }
    // Confine script execution to workspace directory
    cmd.current_dir(&workspace_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| format!("Script timed out after {timeout_secs}s"))?
        .map_err(|e| format!("Failed to run script: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let result = format!(
        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
        exit_code,
        if stdout.len() > 4000 {
            &stdout[..4000]
        } else {
            &stdout
        },
        if stderr.len() > 2000 {
            &stderr[..2000]
        } else {
            &stderr
        },
    );

    Ok(Some(result))
}

/// Execute Docker compose commands (docker_run tool).
async fn execute_docker_run(
    config: &ChatbotConfig,
    compose_file: &str,
    action: &str,
) -> Result<Option<String>, String> {
    if !config.full_permissions {
        return Err("docker_run requires full permissions (Tier 1 only)".to_string());
    }

    // Security: canonicalize and verify compose file is inside workspace/
    let compose_path = std::path::Path::new(compose_file);
    if !compose_path.exists() {
        return Err(format!("Compose file not found: {compose_file}"));
    }
    let canonical = compose_path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve compose path: {e}"))?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let workspace_dir = cwd
        .join("workspace")
        .canonicalize()
        .unwrap_or_else(|_| cwd.join("workspace"));
    if !canonical.starts_with(&workspace_dir) {
        return Err(format!(
            "Security: compose file must be inside workspace/. {} resolves to {}",
            compose_file,
            canonical.display()
        ));
    }

    let args = match action {
        "up" => vec!["-f", compose_file, "up", "-d"],
        "down" => vec!["-f", compose_file, "down"],
        "logs" => vec!["-f", compose_file, "logs", "--tail", "50"],
        "ps" => vec!["-f", compose_file, "ps"],
        _ => return Err(format!("Unknown docker action: {action}")),
    };

    info!("Docker: {} {}", action, compose_file);

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("docker")
            .arg("compose")
            .args(&args)
            .output(),
    )
    .await
    .map_err(|_| "Docker command timed out")?
    .map_err(|e| format!("Docker failed: {e}"))?;

    let result = format!(
        "exit_code: {}\n{}{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    Ok(Some(result))
}

/// Check experiment history (check_experiments tool).
/// All agents can use this — reads experiments.jsonl directly, no Bash needed.
async fn execute_check_experiments(query: &str) -> Result<Option<String>, String> {
    let log_path = std::path::Path::new("data/shared/experiments.jsonl");
    if !log_path.exists() {
        return Ok(Some("No experiments logged yet.".to_string()));
    }

    let content = std::fs::read_to_string(log_path)
        .map_err(|e| format!("Failed to read experiments: {e}"))?;

    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    if entries.is_empty() {
        return Ok(Some("No experiments logged yet.".to_string()));
    }

    match query {
        "summary" => {
            let total = entries.len();
            let passed = entries.iter().filter(|e| e["verdict"] == "PASS").count();
            let mut methods: std::collections::HashMap<String, (usize, usize)> =
                std::collections::HashMap::new();
            for e in &entries {
                let method = e["method"].as_str().unwrap_or("unknown").to_string();
                let entry = methods.entry(method).or_insert((0, 0));
                if e["verdict"] == "PASS" {
                    entry.0 += 1;
                } else {
                    entry.1 += 1;
                }
            }
            let mut result = format!(
                "EXPERIMENT SUMMARY\nTotal: {} ({} PASS, {} FAIL)\n\nMethods:\n",
                total,
                passed,
                total - passed
            );
            for (method, (p, f)) in &methods {
                let status = if *p > 0 { "worked" } else { "NEVER passed" };
                result.push_str(&format!("  [{}P/{}F] {} — {}\n", p, f, method, status));
            }
            result.push_str("\nCheck before planning: don't repeat methods that NEVER passed.");
            Ok(Some(result))
        }
        "view" => {
            let recent: Vec<_> = entries.iter().rev().take(10).collect();
            let mut result = format!("Last {} experiments:\n\n", recent.len());
            for e in recent {
                result.push_str(&format!(
                    "[{}] {} — {}\n  Metrics: {}\n\n",
                    e["verdict"].as_str().unwrap_or("?"),
                    e["task"].as_str().unwrap_or("?"),
                    e["method"].as_str().unwrap_or("?"),
                    e["metrics"],
                ));
            }
            Ok(Some(result))
        }
        keyword => {
            let matches: Vec<_> = entries
                .iter()
                .filter(|e| {
                    let s = serde_json::to_string(e).unwrap_or_default().to_lowercase();
                    s.contains(&keyword.to_lowercase())
                })
                .collect();
            if matches.is_empty() {
                Ok(Some(format!("No experiments matching '{keyword}'.")))
            } else {
                let mut result = format!(
                    "Found {} experiments matching '{keyword}':\n\n",
                    matches.len()
                );
                for e in &matches {
                    result.push_str(&format!(
                        "[{}] {} — {}\n",
                        e["verdict"].as_str().unwrap_or("?"),
                        e["task"].as_str().unwrap_or("?"),
                        e["method"].as_str().unwrap_or("?")
                    ));
                }
                Ok(Some(result))
            }
        }
    }
}

/// Execute the generic evaluation suite (run_eval tool).
async fn execute_run_eval(
    config: &ChatbotConfig,
    vars: &str,
    all: bool,
) -> Result<Option<String>, String> {
    // Security: only full_permissions or Sentinel (tools_override with Bash) can run eval
    // Atlas (WebSearch only) must not be able to trigger shell commands
    if !config.full_permissions && config.bot_name != "Security" {
        return Err("run_eval requires Bash access (Nova or Sentinel only)".to_string());
    }
    let mut cmd_args = vec!["rag/eval_runner.py".to_string()];
    if !vars.is_empty() {
        cmd_args.push("--vars".to_string());
        cmd_args.push(vars.to_string());
    }
    if all {
        cmd_args.push("--all".to_string());
    }
    cmd_args.push("--json".to_string());

    info!("Running eval: python3 {}", cmd_args.join(" "));

    let output = tokio::time::timeout(
        Duration::from_secs(600),
        tokio::process::Command::new("python3")
            .args(&cmd_args)
            .output(),
    )
    .await
    .map_err(|_| "Evaluation timed out (>600s)")?
    .map_err(|e| format!("Eval failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stderr.is_empty() && output.status.code() != Some(0) {
        return Err(format!("Eval error: {}", &stderr[..stderr.len().min(1000)]));
    }

    Ok(Some(stdout.to_string()))
}

async fn execute_send_file(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    file_path: &str,
    caption: Option<&str>,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    // Security: only allow full_permissions bots to send files
    if !config.full_permissions {
        return Err("send_file requires full_permissions (Tier 1 bot only)".to_string());
    }

    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path));
    }

    let data = std::fs::read(path).map_err(|e| format!("Failed to read file: {e}"))?;
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

    info!("📎 Sending file: {} ({} bytes)", filename, data.len());

    let cap = caption.unwrap_or(filename);
    let msg_id = telegram
        .send_document(chat_id, data, filename, Some(cap), reply_to_message_id)
        .await?;

    Ok(Some(format!(
        "File sent: {} (message_id: {})",
        filename, msg_id
    )))
}

async fn execute_send_music(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    prompt: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    let api_key = config
        .gemini_api_key
        .as_deref()
        .ok_or("Gemini API key not configured (required for music generation)")?;

    info!("🎵 send_music: generating \"{}\"", prompt);

    let gemini = crate::chatbot::gemini::GeminiClient::new(api_key.to_string());
    let audio_data = gemini.generate_music(prompt).await?;

    let msg_id = telegram
        .send_audio(chat_id, audio_data, Some(prompt), reply_to_message_id)
        .await?;

    Ok(Some(format!(
        "Music generated and sent to chat {} (message_id: {}) (prompt: {})",
        chat_id, msg_id, prompt
    )))
}

/// Save task checkpoint (CheckpointTask tool).
async fn execute_checkpoint_task(
    config: &ChatbotConfig,
    task_id: &str,
    checkpoint: &str,
    status_note: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    // Save checkpoint
    db.checkpoint_task(task_id, checkpoint)
        .map_err(|e| format!("Checkpoint failed: {e}"))?;

    info!("Checkpoint saved for task {}: {}", task_id, status_note);
    Ok(Some(format!(
        "Checkpoint saved for task {}. Status: {}",
        task_id, status_note
    )))
}

/// Load task state for resumption (ResumeTask tool).
async fn execute_resume_task(
    config: &ChatbotConfig,
    task_id: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let task = db
        .get_task(task_id)
        .map_err(|e| format!("Query failed: {e}"))?
        .ok_or_else(|| format!("Task {} not found", task_id))?;

    let result = serde_json::json!({
        "id": task.id,
        "title": task.title,
        "status": task.status,
        "assigned_to": task.assigned_to,
        "context": task.context,
        "checkpoint": task.checkpoint_json,
        "error_log": task.error_log,
        "created_at": task.created_at,
        "started_at": task.started_at,
    });

    Ok(Some(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    ))
}
