//! Tool dispatch — messaging tools.

use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::chatbot::context::ContextBuffer;
use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::gemini::GeminiClient;
use crate::chatbot::message::{ChatMessage, ReplyTo};
use crate::chatbot::telegram::TelegramClient;
use crate::chatbot::tts::{GeminiTtsClient, TtsClient};

pub(super) async fn execute_send_message(
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

pub(super) async fn execute_add_reaction(
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
pub(super) async fn execute_delete_message(
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

pub(super) async fn execute_send_image(
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

pub(super) async fn execute_send_voice(
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

pub(super) async fn execute_send_poll(
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

pub(super) async fn execute_send_file(
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

pub(super) async fn execute_send_music(
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
