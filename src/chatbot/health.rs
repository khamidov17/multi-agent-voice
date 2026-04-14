//! Health monitor background task.
//!
//! Runs every 60 seconds and checks:
//! 1. Telegram API connectivity (getMe ping via the Bot handle)
//! 2. Memory usage of this process (warn if RSS >80% of system RAM)
//! 3. Claude Code subprocess health (PID alive, heartbeat age)
//! 4. Cross-bot heartbeat (shared bot_messages.db has recent entries from peers)

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use teloxide::prelude::*;
use tracing::{error, info, warn};

/// Maximum acceptable age for a CC heartbeat before we warn (seconds).
const HEARTBEAT_STALE_SECS: u64 = 120;

/// Maximum acceptable age for a peer-bot DB entry before we warn (seconds).
const CROSSBOT_STALE_SECS: u64 = 300;

/// Warn when process RSS exceeds this fraction of system RAM.
const MEMORY_WARN_FRACTION: f64 = 0.80;

/// Spawn the health monitor background task.
///
/// Parameters:
/// - `bot_name`      – Name of this bot (used in log messages).
/// - `bot`           – Telegram `Bot` handle used to call `getMe`.
/// - `cc_pid`        – Atomic holding the Claude Code subprocess PID.
/// - `cc_heartbeat`  – Atomic holding the last CC heartbeat timestamp (Unix ms).
/// - `owner_chat_id` – If set, critical alerts are sent as DMs to the owner.
/// - `shared_db`     – Path to the shared bot_messages.db (cross-bot check).
pub fn start_health_monitor(
    bot_name: String,
    bot: Bot,
    cc_pid: Arc<AtomicU32>,
    cc_heartbeat: Arc<AtomicU64>,
    owner_chat_id: Option<i64>,
    shared_db: Option<PathBuf>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        // First tick fires immediately; skip it so we do not alarm on startup.
        interval.tick().await;

        loop {
            interval.tick().await;

            info!("[health] Running periodic health check for {bot_name}");

            // ----------------------------------------------------------------
            // 1. Telegram API connectivity
            // ----------------------------------------------------------------
            check_telegram(&bot, &bot_name).await;

            // ----------------------------------------------------------------
            // 2. Memory usage
            // ----------------------------------------------------------------
            check_memory(&bot_name);

            // ----------------------------------------------------------------
            // 3. Claude Code subprocess health
            // ----------------------------------------------------------------
            check_cc_subprocess(&bot_name, &cc_pid, &cc_heartbeat, owner_chat_id, &bot).await;

            // ----------------------------------------------------------------
            // 4. Cross-bot heartbeat
            // ----------------------------------------------------------------
            if let Some(ref db_path) = shared_db {
                check_crossbot_heartbeat(&bot_name, db_path);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Check 1 — Telegram API
// ---------------------------------------------------------------------------

async fn check_telegram(bot: &Bot, bot_name: &str) {
    match bot.get_me().await {
        Ok(me) => {
            info!(
                "[health] Telegram OK — connected as @{} ({})",
                me.username(),
                bot_name
            );
        }
        Err(e) => {
            error!("[health] Telegram connectivity FAILED for {bot_name}: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Check 2 — Memory usage
// ---------------------------------------------------------------------------

fn check_memory(bot_name: &str) {
    let my_pid = std::process::id();

    // Read our own RSS from `ps`. On macOS and Linux `ps -o rss= -p PID`
    // prints kilobytes of resident set size.
    let rss_kb = read_process_rss_kb(my_pid);

    // Total physical RAM via `sysctl` (macOS) or /proc/meminfo (Linux).
    // We try both; whichever succeeds first wins.
    let total_kb = read_total_ram_kb();

    match (rss_kb, total_kb) {
        (Some(rss), Some(total)) if total > 0 => {
            let fraction = rss as f64 / total as f64;
            let rss_mb = rss / 1024;
            let total_mb = total / 1024;
            if fraction >= MEMORY_WARN_FRACTION {
                warn!(
                    "[health] HIGH MEMORY — {bot_name} RSS {rss_mb} MB / {total_mb} MB ({:.0}%)",
                    fraction * 100.0
                );
            } else {
                info!(
                    "[health] Memory OK — {bot_name} RSS {rss_mb} MB / {total_mb} MB ({:.0}%)",
                    fraction * 100.0
                );
            }
        }
        (Some(rss), _) => {
            let rss_mb = rss / 1024;
            info!("[health] Memory — {bot_name} RSS {rss_mb} MB (total RAM unknown)");
        }
        _ => {
            warn!("[health] Could not read memory stats for {bot_name}");
        }
    }
}

/// Read the RSS (in kB) for the given PID using `ps`.
fn read_process_rss_kb(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u64>().ok()
}

/// Read total physical RAM in kB.
/// Tries `sysctl hw.memsize` (macOS), then falls back to `/proc/meminfo` (Linux).
fn read_total_ram_kb() -> Option<u64> {
    // macOS: `sysctl -n hw.memsize` returns bytes.
    if let Ok(output) = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
    {
        let s = String::from_utf8_lossy(&output.stdout);
        if let Ok(bytes) = s.trim().parse::<u64>() {
            return Some(bytes / 1024); // convert to kB
        }
    }

    // Linux: read /proc/meminfo.
    if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if line.starts_with("MemTotal:") {
                let kb: u64 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if kb > 0 {
                    return Some(kb);
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Check 3 — Claude Code subprocess
// ---------------------------------------------------------------------------

async fn check_cc_subprocess(
    bot_name: &str,
    cc_pid: &Arc<AtomicU32>,
    cc_heartbeat: &Arc<AtomicU64>,
    owner_chat_id: Option<i64>,
    bot: &Bot,
) {
    let pid = cc_pid.load(Ordering::SeqCst);
    let heartbeat_ms = cc_heartbeat.load(Ordering::SeqCst);

    // PID == 0 means the subprocess has not yet started or has been reset.
    if pid == 0 {
        info!("[health] CC subprocess: PID not yet set for {bot_name}");
        return;
    }

    // Check whether the process is alive using `kill -0`.
    let pid_alive = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !pid_alive {
        let msg = format!("[CRITICAL] {bot_name} CC subprocess DEAD — PID {pid} not found");
        error!("{msg}");
        // Alert owner for DEAD processes — this is critical, not just stale
        alert_owner(bot, owner_chat_id, &msg).await;
        return;
    }

    // Check heartbeat age.
    let now_ms = current_unix_ms();
    if heartbeat_ms == 0 {
        info!("[health] CC subprocess PID {pid} alive, no heartbeat yet ({bot_name})");
        return;
    }

    let age_secs = now_ms.saturating_sub(heartbeat_ms) / 1000;
    if age_secs > 300 {
        // >5 min stale = CRITICAL — alert owner directly
        let msg = format!("[CRITICAL] {bot_name} unresponsive for {age_secs}s (PID {pid})");
        error!("{msg}");
        alert_owner(bot, owner_chat_id, &msg).await;
    } else if age_secs > HEARTBEAT_STALE_SECS {
        warn!(
            "[health] CC subprocess STALE — last heartbeat {age_secs}s ago \
             (PID {pid}, bot {bot_name})"
        );
    } else {
        info!("[health] CC subprocess OK — PID {pid}, heartbeat {age_secs}s ago ({bot_name})");
    }
}

// ---------------------------------------------------------------------------
// Check 4 — Cross-bot heartbeat via shared DB
// ---------------------------------------------------------------------------

fn check_crossbot_heartbeat(bot_name: &str, db_path: &PathBuf) {
    if !db_path.exists() {
        info!(
            "[health] Cross-bot DB not yet created at {}",
            db_path.display()
        );
        return;
    }

    match rusqlite::Connection::open(db_path) {
        Err(e) => {
            warn!(
                "[health] Could not open cross-bot DB at {}: {e}",
                db_path.display()
            );
        }
        Ok(conn) => {
            // Find the most recent message that was NOT sent by this bot.
            let result: rusqlite::Result<String> = conn.query_row(
                "SELECT MAX(created_at) FROM bot_messages WHERE from_bot != ?1",
                rusqlite::params![bot_name],
                |row| row.get(0),
            );

            match result {
                Err(e) => {
                    warn!("[health] Cross-bot heartbeat query failed: {e}");
                }
                Ok(ts) => {
                    // Parse the ISO-8601 datetime stored by SQLite
                    // ("YYYY-MM-DD HH:MM:SS") and compute age.
                    let age_secs = parse_sqlite_datetime_age_secs(&ts);
                    match age_secs {
                        None => {
                            info!("[health] Cross-bot: no peer messages found for {bot_name}");
                        }
                        Some(age) if age > CROSSBOT_STALE_SECS => {
                            warn!(
                                "[health] Cross-bot STALE — last peer message {age}s ago \
                                 (bot {bot_name})"
                            );
                        }
                        Some(age) => {
                            info!(
                                "[health] Cross-bot OK — last peer message {age}s ago \
                                 ({bot_name})"
                            );
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the current Unix time in milliseconds.
fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Parse a SQLite `datetime('now')` string ("YYYY-MM-DD HH:MM:SS") and return
/// how many seconds ago it was (UTC). Returns `None` if the string is empty or
/// cannot be parsed.
fn parse_sqlite_datetime_age_secs(ts: &str) -> Option<u64> {
    if ts.is_empty() {
        return None;
    }

    // chrono can parse "YYYY-MM-DD HH:MM:SS" with NaiveDateTime.
    let dt = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S").ok()?;
    let then = dt.and_utc().timestamp() as u64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Some(now.saturating_sub(then))
}

/// Send an alert DM to the owner. Silently swallows errors (best-effort).
async fn alert_owner(bot: &Bot, owner_chat_id: Option<i64>, message: &str) {
    let Some(chat_id) = owner_chat_id else { return };
    if let Err(e) = bot
        .send_message(teloxide::types::ChatId(chat_id), message)
        .await
    {
        error!("[health] Failed to send owner alert: {e}");
    }
}

// ---------------------------------------------------------------------------
// /status command — comprehensive health report
// ---------------------------------------------------------------------------

/// Build a human-readable status report for all systems.
///
/// Call this from the /status command handler.
pub fn build_status_report(
    bot_name: &str,
    cc_pid: &Arc<AtomicU32>,
    cc_heartbeat: &Arc<AtomicU64>,
    shared_db: Option<&std::path::Path>,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("=== {} Status ===", bot_name));

    // Process info
    let my_pid = std::process::id();
    lines.push(format!("Harness PID: {}", my_pid));

    // CC subprocess
    let pid = cc_pid.load(Ordering::SeqCst);
    let hb_ms = cc_heartbeat.load(Ordering::SeqCst);
    if pid == 0 {
        lines.push("CC: not started".to_string());
    } else {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let hb_age = if hb_ms > 0 {
            let age = current_unix_ms().saturating_sub(hb_ms) / 1000;
            format!("{}s ago", age)
        } else {
            "none".to_string()
        };

        let status = if alive { "alive" } else { "DEAD" };
        lines.push(format!(
            "CC: PID {} ({}) heartbeat: {}",
            pid, status, hb_age
        ));
    }

    // Memory
    let rss_kb = read_process_rss_kb(my_pid);
    let total_kb = read_total_ram_kb();
    match (rss_kb, total_kb) {
        (Some(rss), Some(total)) if total > 0 => {
            let pct = (rss as f64 / total as f64) * 100.0;
            lines.push(format!(
                "Memory: {} MB / {} MB ({:.0}%)",
                rss / 1024,
                total / 1024,
                pct
            ));
        }
        (Some(rss), _) => {
            lines.push(format!("Memory: {} MB", rss / 1024));
        }
        _ => lines.push("Memory: unknown".to_string()),
    }

    // Cross-bot heartbeats
    if let Some(db_path) = shared_db
        && db_path.exists()
        && let Ok(conn) = rusqlite::Connection::open(db_path)
        && let Ok(mut stmt) = conn.prepare(
            "SELECT bot_name, last_heartbeat, iteration_count FROM heartbeats ORDER BY bot_name",
        )
        && let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
    {
        lines.push("--- Peer Heartbeats ---".to_string());
        for row in rows.flatten() {
            let (name, ts, iters) = row;
            let age = parse_sqlite_datetime_age_secs(&ts)
                .map(|a| format!("{}s ago", a))
                .unwrap_or_else(|| "unknown".to_string());
            let status = parse_sqlite_datetime_age_secs(&ts)
                .map(|a| {
                    if a > CROSSBOT_STALE_SECS {
                        "STALE"
                    } else {
                        "OK"
                    }
                })
                .unwrap_or("?");
            lines.push(format!(
                "  {} [{}]: {} (iters: {})",
                name, status, age, iters
            ));
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Startup recovery
// ---------------------------------------------------------------------------

/// Run startup health checks: integrity check + pending DM recovery.
///
/// Call this once during bot initialization.
pub fn run_startup_checks(db: &crate::chatbot::database::Database, bot_name: &str) {
    info!("[startup] Running health checks for {bot_name}...");

    // 1. SQLite integrity check
    match db.integrity_check() {
        Ok(()) => info!("[startup] Database integrity: OK"),
        Err(details) => {
            error!("[startup] DATABASE INTEGRITY FAILURE: {details}");
            // Don't panic — let the bot start, but log loudly
        }
    }

    // 2. Recover incomplete DM charges
    let refunded = db.recover_pending_dms();
    if refunded > 0 {
        warn!("[startup] Refunded {refunded} incomplete DM charge(s)");
    }

    info!("[startup] Health checks complete for {bot_name}");
}
