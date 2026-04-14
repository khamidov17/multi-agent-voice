//! Metrics collection and threshold-alerting system.
//!
//! Tracks in-memory counters (atomics), flushes to SQLite periodically,
//! and checks thresholds to generate alerts for the cognitive loop.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[allow(unused_imports)]
use tracing::{info, warn};

/// Alert generated when a metric crosses a threshold.
#[derive(Debug, Clone)]
pub struct Alert {
    pub metric: String,
    pub message: String,
    pub severity: String, // "warning" or "critical"
}

/// Per-tool call statistics.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToolStats {
    pub calls: u64,
    pub failures: u64,
    pub total_ms: u64,
}

/// A snapshot of metrics saved to the database.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub timestamp: String,
    pub messages_processed: u64,
    pub tool_calls_total: u64,
    pub tool_calls_failed: u64,
    pub avg_tool_latency_ms: u64,
    pub cc_turns_total: u64,
    pub cc_turns_timed_out: u64,
    pub spam_detected: u64,
    pub errors_total: u64,
    pub tool_stats_json: String,
}

/// In-memory metrics collector. All counters are atomic for lock-free multi-task access.
pub struct MetricsCollector {
    pub messages_processed: AtomicU64,
    pub tool_calls_total: AtomicU64,
    pub tool_calls_failed: AtomicU64,
    pub tool_call_duration_ms: AtomicU64,
    pub cc_turns_total: AtomicU64,
    pub cc_turns_timed_out: AtomicU64,
    pub spam_detected: AtomicU64,
    pub errors_total: AtomicU64,
    /// Per-tool failure tracking (needs mutex since HashMap isn't atomic).
    pub tool_stats: Mutex<HashMap<String, ToolStats>>,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self {
            messages_processed: AtomicU64::new(0),
            tool_calls_total: AtomicU64::new(0),
            tool_calls_failed: AtomicU64::new(0),
            tool_call_duration_ms: AtomicU64::new(0),
            cc_turns_total: AtomicU64::new(0),
            cc_turns_timed_out: AtomicU64::new(0),
            spam_detected: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            tool_stats: Mutex::new(HashMap::new()),
        }
    }
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a tool call (success or failure).
    pub fn record_tool_call(&self, tool_name: &str, duration_ms: u64, failed: bool) {
        self.tool_calls_total.fetch_add(1, Ordering::Relaxed);
        self.tool_call_duration_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
        if failed {
            self.tool_calls_failed.fetch_add(1, Ordering::Relaxed);
        }

        if let Ok(mut stats) = self.tool_stats.lock() {
            let entry = stats.entry(tool_name.to_string()).or_default();
            entry.calls += 1;
            entry.total_ms += duration_ms;
            if failed {
                entry.failures += 1;
            }
        }
    }

    /// Flush counters to database and reset. Uses swap to avoid losing counts.
    pub fn flush_to_db(&self, conn: &rusqlite::Connection) -> anyhow::Result<()> {
        // Atomically swap counters to 0 and read the old value
        let messages = self.messages_processed.swap(0, Ordering::Relaxed);
        let tool_total = self.tool_calls_total.swap(0, Ordering::Relaxed);
        let tool_failed = self.tool_calls_failed.swap(0, Ordering::Relaxed);
        let tool_duration = self.tool_call_duration_ms.swap(0, Ordering::Relaxed);
        let cc_turns = self.cc_turns_total.swap(0, Ordering::Relaxed);
        let cc_timed_out = self.cc_turns_timed_out.swap(0, Ordering::Relaxed);
        let spam = self.spam_detected.swap(0, Ordering::Relaxed);
        let errors = self.errors_total.swap(0, Ordering::Relaxed);

        let avg_latency = if tool_total > 0 {
            tool_duration / tool_total
        } else {
            0
        };

        // Serialize and reset per-tool stats
        let tool_stats_json = {
            let mut stats = self.tool_stats.lock().unwrap();
            let json = serde_json::to_string(&*stats).unwrap_or_default();
            stats.clear();
            json
        };

        conn.execute(
            "INSERT INTO metrics_snapshots
             (messages_processed, tool_calls_total, tool_calls_failed, avg_tool_latency_ms,
              cc_turns_total, cc_turns_timed_out, spam_detected, errors_total, tool_stats_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                messages,
                tool_total,
                tool_failed,
                avg_latency,
                cc_turns,
                cc_timed_out,
                spam,
                errors,
                tool_stats_json,
            ],
        )?;

        info!(
            "[metrics] Flushed: msgs={} tools={}/{} failed avg_lat={}ms turns={}/{} timed_out spam={} errors={}",
            messages, tool_total, tool_failed, avg_latency, cc_turns, cc_timed_out, spam, errors
        );

        Ok(())
    }

    /// Check thresholds and return alerts.
    pub fn check_thresholds(&self) -> Vec<Alert> {
        let mut alerts = Vec::new();
        let total = self.tool_calls_total.load(Ordering::Relaxed);
        let failed = self.tool_calls_failed.load(Ordering::Relaxed);
        let cc_turns = self.cc_turns_total.load(Ordering::Relaxed);
        let cc_timed_out = self.cc_turns_timed_out.load(Ordering::Relaxed);
        let duration = self.tool_call_duration_ms.load(Ordering::Relaxed);
        let errors = self.errors_total.load(Ordering::Relaxed);

        // Tool failure rate > 20%
        if total > 5 && failed * 100 / total > 20 {
            alerts.push(Alert {
                metric: "tool_failure_rate".into(),
                message: format!(
                    "Tool failure rate: {}% ({}/{} calls)",
                    failed * 100 / total,
                    failed,
                    total
                ),
                severity: "warning".into(),
            });
        }

        // CC timeout rate > 10%
        if cc_turns > 3 && cc_timed_out * 100 / cc_turns > 10 {
            alerts.push(Alert {
                metric: "cc_timeout_rate".into(),
                message: format!(
                    "CC timeout rate: {}% ({}/{} turns)",
                    cc_timed_out * 100 / cc_turns,
                    cc_timed_out,
                    cc_turns
                ),
                severity: "critical".into(),
            });
        }

        // Average tool latency > 5000ms
        if total > 0 && duration / total > 5000 {
            alerts.push(Alert {
                metric: "tool_latency".into(),
                message: format!(
                    "Avg tool latency: {}ms (threshold: 5000ms)",
                    duration / total
                ),
                severity: "warning".into(),
            });
        }

        // Error rate > 5 per interval
        if errors > 5 {
            alerts.push(Alert {
                metric: "error_rate".into(),
                message: format!("{} errors in current interval (threshold: 5)", errors),
                severity: "warning".into(),
            });
        }

        // Check per-tool failure rates
        if let Ok(stats) = self.tool_stats.lock() {
            for (tool, ts) in stats.iter() {
                if ts.calls > 3 && ts.failures * 100 / ts.calls > 40 {
                    alerts.push(Alert {
                        metric: format!("tool_{}_failure", tool),
                        message: format!(
                            "Tool '{}' failing {}% ({}/{} calls)",
                            tool,
                            ts.failures * 100 / ts.calls,
                            ts.failures,
                            ts.calls
                        ),
                        severity: "critical".into(),
                    });
                }
            }
        }

        alerts
    }
}

/// Get recent metric snapshots from the database.
pub fn get_recent_snapshots(conn: &rusqlite::Connection, count: u64) -> Vec<MetricsSnapshot> {
    let mut stmt = match conn.prepare(
        "SELECT timestamp, messages_processed, tool_calls_total, tool_calls_failed,
                avg_tool_latency_ms, cc_turns_total, cc_turns_timed_out, spam_detected,
                errors_total, COALESCE(tool_stats_json, '')
         FROM metrics_snapshots ORDER BY id DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map(rusqlite::params![count], |row| {
        Ok(MetricsSnapshot {
            timestamp: row.get(0)?,
            messages_processed: row.get(1)?,
            tool_calls_total: row.get(2)?,
            tool_calls_failed: row.get(3)?,
            avg_tool_latency_ms: row.get(4)?,
            cc_turns_total: row.get(5)?,
            cc_turns_timed_out: row.get(6)?,
            spam_detected: row.get(7)?,
            errors_total: row.get(8)?,
            tool_stats_json: row.get(9)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// Format snapshots into a human-readable summary for the cognitive loop.
pub fn format_metrics_summary(snapshots: &[MetricsSnapshot]) -> String {
    if snapshots.is_empty() {
        return "No metrics data yet.".to_string();
    }

    let mut lines = Vec::new();
    for s in snapshots {
        let fail_rate = if s.tool_calls_total > 0 {
            format!("{}%", s.tool_calls_failed * 100 / s.tool_calls_total)
        } else {
            "N/A".into()
        };
        lines.push(format!(
            "[{}] msgs={} tools={} (fail={}) latency={}ms turns={} (timeout={}) spam={} errors={}",
            s.timestamp,
            s.messages_processed,
            s.tool_calls_total,
            fail_rate,
            s.avg_tool_latency_ms,
            s.cc_turns_total,
            s.cc_turns_timed_out,
            s.spam_detected,
            s.errors_total,
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_tool_call_updates_stats() {
        let mc = MetricsCollector::new();
        mc.record_tool_call("send_message", 150, false);
        mc.record_tool_call("send_message", 200, false);
        mc.record_tool_call("send_message", 100, true);

        assert_eq!(mc.tool_calls_total.load(Ordering::Relaxed), 3);
        assert_eq!(mc.tool_calls_failed.load(Ordering::Relaxed), 1);
        assert_eq!(mc.tool_call_duration_ms.load(Ordering::Relaxed), 450);

        let stats = mc.tool_stats.lock().unwrap();
        let sm = stats.get("send_message").unwrap();
        assert_eq!(sm.calls, 3);
        assert_eq!(sm.failures, 1);
        assert_eq!(sm.total_ms, 450);
    }

    #[test]
    fn test_flush_resets_counters() {
        let mc = MetricsCollector::new();
        mc.record_tool_call("query", 50, false);
        mc.record_tool_call("query", 30, false);
        mc.messages_processed.store(10, Ordering::Relaxed);

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE metrics_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                messages_processed INTEGER, tool_calls_total INTEGER,
                tool_calls_failed INTEGER, avg_tool_latency_ms INTEGER,
                cc_turns_total INTEGER, cc_turns_timed_out INTEGER,
                spam_detected INTEGER, errors_total INTEGER,
                tool_stats_json TEXT
            );",
        )
        .unwrap();

        mc.flush_to_db(&conn).unwrap();

        // Counters should be reset to 0
        assert_eq!(mc.tool_calls_total.load(Ordering::Relaxed), 0);
        assert_eq!(mc.messages_processed.load(Ordering::Relaxed), 0);

        // DB should have one snapshot
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics_snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_check_thresholds_fires_on_high_failure_rate() {
        let mc = MetricsCollector::new();
        // 10 calls, 5 failed = 50% failure rate (threshold is 20%)
        for _ in 0..5 {
            mc.record_tool_call("test", 100, false);
        }
        for _ in 0..5 {
            mc.record_tool_call("test", 100, true);
        }

        let alerts = mc.check_thresholds();
        assert!(!alerts.is_empty(), "Expected alerts for 50% failure rate");
        assert!(alerts.iter().any(|a| a.metric == "tool_failure_rate"));
    }

    #[test]
    fn test_check_thresholds_no_alerts_when_healthy() {
        let mc = MetricsCollector::new();
        for _ in 0..10 {
            mc.record_tool_call("test", 100, false);
        }
        let alerts = mc.check_thresholds();
        assert!(alerts.is_empty(), "Expected no alerts for healthy metrics");
    }

    #[test]
    fn test_format_metrics_summary_empty() {
        let s = format_metrics_summary(&[]);
        assert_eq!(s, "No metrics data yet.");
    }
}
