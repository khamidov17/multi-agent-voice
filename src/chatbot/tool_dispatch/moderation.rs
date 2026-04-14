//! Tool dispatch — moderation tools.

use std::path::PathBuf;
use tokio::sync::Mutex;
use tracing::info;

use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::telegram::TelegramClient;

/// Execute mute user and notify owner.
pub(super) async fn execute_mute_user(
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
pub(super) async fn execute_ban_user(
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
pub(super) async fn execute_kick_user(
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
pub(super) async fn execute_get_chat_admins(
    telegram: &TelegramClient,
    chat_id: i64,
) -> Result<Option<String>, String> {
    let admins = telegram.get_chat_admins(chat_id).await?;
    Ok(Some(admins))
}

/// Get members from database with optional filter.
pub(super) async fn execute_get_members(
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
pub(super) async fn execute_import_members(
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
