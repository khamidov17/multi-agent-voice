//! Guardian runtime configuration.
//!
//! Two environments — dev (local macOS/iCloud) and prod (Linux server at
//! /opt/nova/). A single JSON file with per-env blocks; guardian picks the
//! right one from `CLAUDIR_ENV` env var (default: `prod`).

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianConfig {
    /// Directory that holds the socket, key, and nonce db.
    pub run_dir: PathBuf,
    /// Paths the guardian will refuse to write. Canonical absolute paths.
    pub protected_paths: Vec<PathBuf>,
    /// Paths the guardian WILL write, any of which a client's request must land inside.
    /// Allowlist wins over blocklist only if a protected path is not a subpath of any
    /// allowed root — guardian enforces "deny by default."
    pub allowed_roots: Vec<PathBuf>,
    /// UIDs registered to speak to the guardian. Typically: [harness_uid].
    pub allowed_uids: Vec<u32>,
    /// Seconds a single request may block before guardian returns IpcTimeout.
    pub request_timeout_secs: u64,
}

impl GuardianConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let envs: std::collections::HashMap<String, GuardianConfig> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;

        let env = std::env::var("CLAUDIR_ENV").unwrap_or_else(|_| "prod".to_string());
        envs.get(&env)
            .cloned()
            .ok_or_else(|| anyhow!("config has no block for CLAUDIR_ENV=\"{}\"", env))
    }

    /// Minimal config for the integration-test harness.
    #[cfg(test)]
    pub fn for_test(run_dir: PathBuf, allowed_root: PathBuf, protected: PathBuf) -> Self {
        Self {
            run_dir,
            protected_paths: vec![protected],
            allowed_roots: vec![allowed_root],
            allowed_uids: vec![unsafe { libc::geteuid() }],
            request_timeout_secs: 5,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.run_dir.join("bootstrap-guardian.sock")
    }

    pub fn key_path(&self) -> PathBuf {
        self.run_dir.join("guardian.key")
    }

    pub fn nonce_db_path(&self) -> PathBuf {
        self.run_dir.join("nonces.db")
    }

    pub fn pause_flag_path(&self) -> PathBuf {
        self.run_dir.join("paused")
    }

    pub fn audit_log_path(&self) -> PathBuf {
        self.run_dir.join("guardian.audit.jsonl")
    }
}
