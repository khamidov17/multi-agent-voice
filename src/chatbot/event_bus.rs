//! Event-driven agent coordination — replaces sleep-polling with instant wake-ups.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::Notify;
use tracing::info;

pub struct EventBus {
    waiters: RwLock<HashMap<String, Arc<Notify>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            waiters: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, bot_name: &str) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.waiters
            .write()
            .unwrap()
            .insert(bot_name.to_lowercase(), notify.clone());
        notify
    }

    pub fn wake(&self, bot_name: &str) {
        if let Some(notify) = self.waiters.read().unwrap().get(&bot_name.to_lowercase()) {
            notify.notify_one();
        }
    }

    pub fn wake_all(&self) {
        for notify in self.waiters.read().unwrap().values() {
            notify.notify_one();
        }
    }
}

static EVENT_BUS: OnceLock<EventBus> = OnceLock::new();

pub fn global_event_bus() -> &'static EventBus {
    EVENT_BUS.get_or_init(EventBus::new)
}

/// Touch a wake file for cross-process notification.
pub fn touch_wake_file(bot_name: &str) {
    let wake_path = format!("data/shared/{}.wake", bot_name.to_lowercase());
    let _ = std::fs::create_dir_all("data/shared");
    let _ = std::fs::write(&wake_path, chrono::Utc::now().to_rfc3339());
    info!("Touched wake file for {}", bot_name);
}
