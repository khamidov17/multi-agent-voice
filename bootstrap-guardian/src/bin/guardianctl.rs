//! guardianctl — owner-only break-glass CLI for the bootstrap-guardian.
//!
//! Subcommands:
//!   pause <duration>     — admin pause (creates <run_dir>/paused flag + schedules resume)
//!   resume               — remove pause flag
//!   status               — report whether the guardian socket is reachable
//!   override-once        — one-shot write that bypasses protected-path denial;
//!                          requires a separate `override.key` not known to the
//!                          harness. Audit-logged with `reason` mandatory.
//!
//! The override path is intentionally not wired through the harness: it is
//! owner-only, and owner-only means `ava` running `guardianctl` on the box
//! with access to `~/.config/claudir/override.key`.

use anyhow::{Context, Result, anyhow};
use bootstrap_guardian::GuardianConfig;
use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(version, about = "Owner break-glass CLI for the bootstrap-guardian")]
struct Cli {
    /// Path to guardian.json.
    #[arg(short, long, default_value = "/opt/claudir/guardian.json")]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Admin-pause the guardian: denies all writes until `resume` or the
    /// pause duration elapses.
    Pause {
        /// Duration like `30m`, `2h`, `1d`.
        #[arg(default_value = "30m")]
        duration: String,
        /// Why are you pausing? Recorded in the pause flag.
        #[arg(short, long, default_value = "unspecified")]
        reason: String,
    },
    /// Remove the pause flag immediately.
    Resume,
    /// Report whether the guardian is reachable and what it thinks.
    Status,
    /// Not implemented in this slice — planned for Phase 0 follow-up PR.
    OverrideOnce {
        #[arg(long)]
        path: String,
        #[arg(long)]
        reason: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = GuardianConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;

    match cli.cmd {
        Cmd::Pause { duration, reason } => pause(&cfg, &duration, &reason),
        Cmd::Resume => resume(&cfg),
        Cmd::Status => status(&cfg),
        Cmd::OverrideOnce { path: _, reason: _ } => {
            anyhow::bail!(
                "override-once is intentionally unimplemented in this slice. \
                 Tracked in TODOS.md. For an emergency bypass right now, \
                 invoke the write manually as the owner UID (outside the guardian)."
            )
        }
    }
}

fn pause(cfg: &GuardianConfig, duration_str: &str, reason: &str) -> Result<()> {
    let dur = parse_duration(duration_str)?;
    let expires_at = Instant::now() + dur; // informational only
    let payload = format!(
        "pause\npaused_at={}\nexpires_after_secs={}\nreason={}\n",
        chrono::Utc::now().to_rfc3339(),
        dur.as_secs(),
        reason
    );
    std::fs::write(cfg.pause_flag_path(), payload)
        .with_context(|| format!("writing {}", cfg.pause_flag_path().display()))?;
    println!(
        "guardian paused; writes will be denied until you run `guardianctl resume` \
         or remove {}",
        cfg.pause_flag_path().display()
    );
    // Informational: we do NOT auto-resume in this slice. A background timer
    // would need a supervisor; keep it manual so there is no surprise re-enable.
    let _ = expires_at;
    Ok(())
}

fn resume(cfg: &GuardianConfig) -> Result<()> {
    let flag = cfg.pause_flag_path();
    if !flag.exists() {
        println!(
            "guardian was not paused (flag {} does not exist)",
            flag.display()
        );
        return Ok(());
    }
    std::fs::remove_file(&flag).with_context(|| format!("removing {}", flag.display()))?;
    println!("guardian resumed");
    Ok(())
}

fn status(cfg: &GuardianConfig) -> Result<()> {
    let sock = cfg.socket_path();
    print!("socket: {} ... ", sock.display());
    std::io::stdout().flush().ok();

    let mut stream =
        UnixStream::connect(&sock).with_context(|| format!("connect {}", sock.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    // Send an explicit `ping` request with a real HMAC so the guardian will
    // actually process it. This requires reading the key — owner reads 0400,
    // so this works when run by owner UID.
    let key = bootstrap_guardian::auth::load_key(&cfg.key_path())
        .with_context(|| "status ping requires reading guardian.key")?;
    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let hmac =
        bootstrap_guardian::auth::compute_hmac(&key, bootstrap_guardian::Op::Ping, "", &[], nonce);
    let req = bootstrap_guardian::Req {
        op: bootstrap_guardian::Op::Ping,
        path: String::new(),
        bytes_b64: String::new(),
        nonce,
        hmac,
        reason: Some("guardianctl status".into()),
    };
    let line = serde_json::to_string(&req)? + "\n";
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut resp_line = String::new();
    BufReader::new(&stream)
        .read_line(&mut resp_line)
        .context("read status response")?;
    let resp: bootstrap_guardian::Resp =
        serde_json::from_str(resp_line.trim_end()).context("parse status response")?;

    if resp.ok {
        println!("OK");
    } else {
        println!(
            "FAIL (code: {:?}, message: {:?})",
            resp.err_code, resp.message
        );
    }

    if cfg.pause_flag_path().exists() {
        println!("pause flag: PRESENT ({})", cfg.pause_flag_path().display());
        println!(
            "{}",
            std::fs::read_to_string(cfg.pause_flag_path())
                .unwrap_or_else(|_| "<unreadable>".into())
                .trim()
        );
    } else {
        println!("pause flag: absent");
    }
    Ok(())
}

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty duration"));
    }
    let (num_str, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    if num_str.is_empty() {
        return Err(anyhow!("duration needs a number: {}", s));
    }
    let n: u64 = num_str.parse().context("parse number in duration")?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        other => return Err(anyhow!("unknown duration unit {:?}", other)),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("30x").is_err());
    }
}
