//! bootstrap-guardian binary — starts the UDS server from a config path.

use anyhow::{Context, Result};
use bootstrap_guardian::auth::load_key;
use bootstrap_guardian::{Guardian, GuardianConfig};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

/// Write-guarding process that prevents Nova from modifying its own harness
/// or wrapper files at 3am. Runs as a sibling to the Trio harness,
/// accepts UDS requests, authenticates via HMAC, and enforces a path
/// allowlist/blocklist.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Path to guardian.json. Expected shape: {"dev": {...}, "prod": {...}}.
    /// Guardian picks the block matching `TRIO_ENV` (default `prod`).
    #[arg(short, long, default_value = "guardian.json")]
    config: PathBuf,

    /// Override the env selector. If unset, reads `TRIO_ENV`.
    #[arg(long)]
    env: Option<String>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,bootstrap_guardian=info")
            }),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    if let Some(e) = args.env {
        // SAFETY: single-threaded at program start; setting env is fine here
        // before any other thread is spawned.
        unsafe {
            std::env::set_var("TRIO_ENV", e);
        }
    }

    let cfg = GuardianConfig::load(&args.config)
        .with_context(|| format!("loading guardian config {}", args.config.display()))?;

    tracing::info!(
        run_dir = %cfg.run_dir.display(),
        socket = %cfg.socket_path().display(),
        "bootstrap-guardian starting"
    );

    // Ensure run_dir exists and is 0700.
    std::fs::create_dir_all(&cfg.run_dir)
        .with_context(|| format!("mkdir {}", cfg.run_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&cfg.run_dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&cfg.run_dir, perms)?;
    }

    let key = load_key(&cfg.key_path())
        .with_context(|| format!("loading key {}", cfg.key_path().display()))?;

    let guardian = Arc::new(Guardian::new(cfg, key)?);
    let listener = guardian.bind()?;
    guardian.run(listener)?;
    Ok(())
}
