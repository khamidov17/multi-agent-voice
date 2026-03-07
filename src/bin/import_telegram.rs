//! Import Telegram Desktop export JSON into messages.json
//!
//! Usage: cargo run --bin import_telegram <export.json> <messages.json>
//!
//! The export.json is from Telegram Desktop: Settings → Advanced → Export Telegram Data
//! Select JSON format and include the chat you want to import.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Telegram export format
#[derive(Deserialize)]
struct TelegramExport {
    name: String,
    #[serde(rename = "type")]
    chat_type: String,
    id: i64,
    messages: Vec<TelegramMessage>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    id: i64,
    #[serde(rename = "type")]
    msg_type: String,
    date: String,
    from: Option<String>,
    from_id: Option<String>,
    #[serde(default)]
    text: TextContent,
    reply_to_message_id: Option<i64>,
}

/// Text can be a string or array of text entities
#[derive(Deserialize, Default)]
#[serde(untagged)]
enum TextContent {
    #[default]
    Empty,
    Simple(String),
    Complex(Vec<TextEntity>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TextEntity {
    Plain(String),
    Formatted { text: String },
}

impl std::fmt::Display for TextContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextContent::Empty => Ok(()),
            TextContent::Simple(s) => write!(f, "{}", s),
            TextContent::Complex(entities) => {
                for e in entities {
                    match e {
                        TextEntity::Plain(s) => write!(f, "{}", s)?,
                        TextEntity::Formatted { text } => write!(f, "{}", text)?,
                    }
                }
                Ok(())
            }
        }
    }
}

/// Our message format
#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    message_id: i64,
    chat_id: i64,
    user_id: i64,
    username: String,
    timestamp: String,
    text: String,
    reply_to: Option<ReplyTo>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ReplyTo {
    message_id: i64,
    username: String,
    text: String,
}

#[derive(Serialize, Deserialize)]
struct MessageStore {
    messages: Vec<ChatMessage>,
}

fn parse_user_id(from_id: &str) -> i64 {
    // Format: "user123456789" or "channel123456789"
    from_id
        .trim_start_matches("user")
        .trim_start_matches("channel")
        .parse()
        .unwrap_or(0)
}

fn parse_timestamp(date: &str) -> String {
    // Format: "2024-01-15T10:30:00" -> "10:30"
    if let Some(time_part) = date.split('T').nth(1)
        && let Some(hm) = time_part.get(0..5)
    {
        return hm.to_string();
    }
    date.to_string()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("Usage: {} <telegram_export.json> <messages.json> [chat_id]", args[0]);
        eprintln!();
        eprintln!("Import Telegram Desktop export into the message store.");
        eprintln!("Only imports group messages (not DMs).");
        eprintln!();
        eprintln!("Arguments:");
        eprintln!("  telegram_export.json  Path to Telegram export (result.json)");
        eprintln!("  messages.json         Path to output message store");
        eprintln!("  chat_id               Optional: specific chat_id to use (e.g., -1001234567890)");
        eprintln!();
        eprintln!("To export from Telegram Desktop:");
        eprintln!("  1. Open Settings → Advanced → Export Telegram Data");
        eprintln!("  2. Select ONLY the group chat you want to import");
        eprintln!("  3. Choose JSON format");
        eprintln!("  4. Export and find result.json in the export folder");
        std::process::exit(1);
    }

    let export_path = Path::new(&args[1]);
    let store_path = Path::new(&args[2]);
    let override_chat_id: Option<i64> = args.get(3).map(|s| s.parse().expect("Invalid chat_id"));

    // Read Telegram export
    println!("Reading Telegram export from {:?}...", export_path);
    let export_json = std::fs::read_to_string(export_path)
        .expect("Failed to read export file");
    let export: TelegramExport = serde_json::from_str(&export_json)
        .expect("Failed to parse Telegram export JSON");

    println!("Chat: {} (id: {})", export.name, export.id);
    println!("Chat type: {}", export.chat_type);
    println!("Total messages in export: {}", export.messages.len());

    // Check if this is a DM (personal chat) - skip those
    if export.chat_type == "personal_chat" {
        eprintln!("ERROR: This looks like a DM/personal chat, not a group.");
        eprintln!("Please export only group chats.");
        std::process::exit(1);
    }

    // Use override chat_id if provided, otherwise calculate from export
    let chat_id = if let Some(id) = override_chat_id {
        println!("Using provided chat_id: {}", id);
        id
    } else {
        // Telegram uses positive IDs, but for supergroups we need to convert
        // Supergroups have IDs like 1234567890 but chat_id is -1001234567890
        let id = if export.id > 0 {
            // Check if it looks like a supergroup (large ID)
            if export.id > 1_000_000_000 {
                -1000000000000 - export.id
            } else {
                -export.id // Regular group
            }
        } else {
            export.id
        };
        println!("Calculated chat_id: {}", id);
        id
    };

    // Build a map for reply lookups
    let msg_map: HashMap<i64, &TelegramMessage> = export.messages
        .iter()
        .filter(|m| m.msg_type == "message")
        .map(|m| (m.id, m))
        .collect();

    // Convert messages
    let mut imported: Vec<ChatMessage> = Vec::new();
    let mut skipped = 0;

    for msg in &export.messages {
        // Only import regular messages
        if msg.msg_type != "message" {
            skipped += 1;
            continue;
        }

        let text = msg.text.to_string();
        if text.is_empty() {
            skipped += 1;
            continue; // Skip empty messages (media, etc.)
        }

        let user_id = msg.from_id
            .as_ref()
            .map(|id| parse_user_id(id))
            .unwrap_or(0);

        let username = msg.from.clone().unwrap_or_else(|| "unknown".to_string());
        let timestamp = parse_timestamp(&msg.date);

        // Build reply_to if present
        let reply_to = msg.reply_to_message_id.and_then(|reply_id| {
            msg_map.get(&reply_id).map(|reply_msg| {
                ReplyTo {
                    message_id: reply_id,
                    username: reply_msg.from.clone().unwrap_or_else(|| "unknown".to_string()),
                    text: reply_msg.text.to_string().chars().take(100).collect(),
                }
            })
        });

        imported.push(ChatMessage {
            message_id: msg.id,
            chat_id,
            user_id,
            username,
            timestamp,
            text,
            reply_to,
        });
    }

    println!("Converted {} messages ({} skipped)", imported.len(), skipped);

    // Load existing store or create new
    let mut existing: HashMap<i64, ChatMessage> = if store_path.exists() {
        println!("Loading existing message store from {:?}...", store_path);
        let json = std::fs::read_to_string(store_path)
            .expect("Failed to read existing store");
        let store: MessageStore = serde_json::from_str(&json)
            .expect("Failed to parse existing store");
        store.messages.into_iter().map(|m| (m.message_id, m)).collect()
    } else {
        HashMap::new()
    };

    let existing_count = existing.len();
    println!("Existing messages: {}", existing_count);

    // Merge (imported messages take precedence for same ID)
    let mut new_count = 0;
    for msg in imported {
        if !existing.contains_key(&msg.message_id) {
            new_count += 1;
        }
        existing.insert(msg.message_id, msg);
    }

    // Sort by message_id for consistent output
    let mut all_messages: Vec<ChatMessage> = existing.into_values().collect();
    all_messages.sort_by_key(|m| m.message_id);

    println!("New messages added: {}", new_count);
    println!("Total messages after merge: {}", all_messages.len());

    // Save
    let store = MessageStore { messages: all_messages };
    let json = serde_json::to_string_pretty(&store)
        .expect("Failed to serialize");
    std::fs::write(store_path, json)
        .expect("Failed to write store");

    println!("Saved to {:?}", store_path);
}
