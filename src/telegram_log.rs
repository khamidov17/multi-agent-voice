use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::ChatId;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Log message with priority.
enum LogMessage {
    /// High priority (WARN/ERROR) - send immediately
    Urgent(String),
    /// Low priority (INFO) - batch and send periodically
    Info(String),
}

pub struct TelegramLogLayer {
    tx: mpsc::UnboundedSender<LogMessage>,
}

impl TelegramLogLayer {
    pub fn new(bot: Bot, chat_id: ChatId) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<LogMessage>();

        tokio::spawn(async move {
            let mut info_buffer: Vec<String> = Vec::new();
            let mut interval = tokio::time::interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(LogMessage::Urgent(text)) => {
                                // Send WARN/ERROR immediately
                                send_log(&bot, chat_id, &text).await;
                            }
                            Some(LogMessage::Info(text)) => {
                                // Buffer INFO logs
                                info_buffer.push(text);
                                // If buffer gets too large, flush early
                                if info_buffer.len() >= 50 {
                                    flush_buffer(&bot, chat_id, &mut info_buffer).await;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = interval.tick() => {
                        // Periodic flush of INFO buffer
                        if !info_buffer.is_empty() {
                            flush_buffer(&bot, chat_id, &mut info_buffer).await;
                        }
                    }
                }
            }
        });

        Self { tx }
    }
}

async fn send_log(bot: &Bot, chat_id: ChatId, text: &str) {
    let text = if text.len() > 4000 {
        let truncated: String = text.chars().take(4000).collect();
        format!("{}...", truncated)
    } else {
        text.to_string()
    };
    if let Err(e) = bot.send_message(chat_id, &text).await {
        eprintln!("Failed to send log to Telegram: {e}");
    }
}

async fn flush_buffer(bot: &Bot, chat_id: ChatId, buffer: &mut Vec<String>) {
    if buffer.is_empty() {
        return;
    }
    // Join all messages with newlines
    let combined = buffer.join("\n");
    buffer.clear();
    send_log(bot, chat_id, &combined).await;
}

struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else if self.message.is_empty() {
            self.message = format!("{} = {:?}", field.name(), value);
        } else {
            self.message
                .push_str(&format!(", {} = {:?}", field.name(), value));
        }
    }
}

impl<S: Subscriber> Layer<S> for TelegramLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();

        // Only send INFO, WARN, ERROR to Telegram
        if level > Level::INFO {
            return;
        }

        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);

        // Add emoji prefix for WARN/ERROR levels
        let msg = match level {
            Level::ERROR => LogMessage::Urgent(format!("❌ {}", visitor.message)),
            Level::WARN => LogMessage::Urgent(format!("⚠️ {}", visitor.message)),
            _ => LogMessage::Info(visitor.message),
        };

        if self.tx.send(msg).is_err() {
            eprintln!("Log channel closed, message dropped");
        }
    }
}
