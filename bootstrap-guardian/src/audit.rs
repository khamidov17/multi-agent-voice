//! Audit log for every request the guardian sees — one JSON line per event.
//!
//! Rotated externally (logrotate / launchd / systemd.timer). The guardian
//! only appends. Survives restarts and is the forensic record Nova (and the
//! human owner) can read after an incident.

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Serialize)]
pub struct AuditEvent<'a> {
    pub ts: String,
    pub uid: u32,
    pub op: &'a str,
    pub path: String,
    pub decision: &'a str,
    pub bytes: Option<u64>,
    pub reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<&'a str>,
}

pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn write(&self, event: &AuditEvent<'_>) {
        let _guard = match self.lock.lock() {
            Ok(g) => g,
            Err(p) => {
                // Poisoned is recoverable for append-only logging.
                tracing::warn!("audit log mutex poisoned; continuing");
                p.into_inner()
            }
        };

        let line = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(err = %e, "audit log serialize failed");
                return;
            }
        };

        // Observability must NOT kill the hot path — log the io error, keep going.
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| {
                f.write_all(line.as_bytes())?;
                f.write_all(b"\n")
            });

        if let Err(e) = result {
            tracing::error!(path = %self.path.display(), err = %e, "audit log write failed");
        }
    }

    pub fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_line_per_event() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("audit.jsonl");
        let log = AuditLog::new(path.clone());
        log.write(&AuditEvent {
            ts: "2026-04-21T00:00:00Z".into(),
            uid: 501,
            op: "write",
            path: "/x/y".into(),
            decision: "allow",
            bytes: Some(4),
            reason: Some("test"),
            err: None,
        });
        log.write(&AuditEvent {
            ts: "2026-04-21T00:00:01Z".into(),
            uid: 501,
            op: "write",
            path: "/x/z".into(),
            decision: "denied",
            bytes: None,
            reason: Some("test"),
            err: Some("denied"),
        });
        let got = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"decision\":\"allow\""));
        assert!(lines[1].contains("\"decision\":\"denied\""));
    }
}
