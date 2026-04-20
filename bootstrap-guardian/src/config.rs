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
    /// Path to the owner-only override key (`override.key`, 0400, >=32 bytes).
    /// When `None`, `override-once` is disabled and any `OverrideWrite`
    /// request is rejected with `ErrCode::OverrideDisabled`. When set, the
    /// key is loaded on demand (not at boot) so the guardian can start with
    /// an absent override key and still serve normal writes.
    #[serde(default)]
    pub override_key_path: Option<PathBuf>,
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
            override_key_path: None,
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

    /// Default path for the override key when `override_key_path` is unset
    /// but the caller still wants a well-known location (e.g. `guardianctl`
    /// picking the default when not explicitly configured). Guardian itself
    /// only honors override writes when `override_key_path` is Some().
    pub fn default_override_key_path(&self) -> PathBuf {
        self.run_dir.join("override.key")
    }
}
