//! Verification pipeline — HTTP probes, process checks, log analysis.
//!
//! Agents use these to verify their own work actually works, not just
//! "tests pass" but actually hitting endpoints, checking outputs, reading logs.

use std::path::Path;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ─── HTTP Probe ──────────────────────────────────────────────────────────

/// Configuration for an HTTP probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpProbe {
    pub url: String,
    pub method: String,
    pub expected_status: u16,
    pub body_contains: Option<String>,
    pub timeout_secs: u64,
}

/// Result of an HTTP probe.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub passed: bool,
    pub actual_status: u16,
    pub body_snippet: String,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Run an HTTP probe with SSRF protection.
pub async fn run_http_probe(probe: &HttpProbe) -> ProbeResult {
    let start = Instant::now();

    // SSRF protection: validate URL before fetching
    if let Err(e) = crate::chatbot::tool_dispatch::validate_url_ssrf(&probe.url).await {
        return ProbeResult {
            passed: false,
            actual_status: 0,
            body_snippet: String::new(),
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some(format!("SSRF blocked: {e}")),
        };
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(probe.timeout_secs.min(30)))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ProbeResult {
                passed: false,
                actual_status: 0,
                body_snippet: String::new(),
                latency_ms: start.elapsed().as_millis() as u64,
                error: Some(format!("Client build error: {e}")),
            };
        }
    };

    let request = match probe.method.to_uppercase().as_str() {
        "POST" => client.post(&probe.url),
        "PUT" => client.put(&probe.url),
        "DELETE" => client.delete(&probe.url),
        "HEAD" => client.head(&probe.url),
        _ => client.get(&probe.url),
    };

    match request.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            let latency = start.elapsed().as_millis() as u64;

            let mut passed = status == probe.expected_status;
            let mut error = None;

            if passed {
                if let Some(ref contains) = probe.body_contains {
                    if !body.contains(contains.as_str()) {
                        passed = false;
                        error = Some(format!(
                            "Body does not contain '{}' (got {} chars)",
                            contains,
                            body.len()
                        ));
                    }
                }
            } else {
                error = Some(format!(
                    "Expected status {}, got {}",
                    probe.expected_status, status
                ));
            }

            info!(
                "[verify] HTTP {} {} → {} ({}ms) {}",
                probe.method,
                probe.url,
                status,
                latency,
                if passed { "PASS" } else { "FAIL" }
            );

            ProbeResult {
                passed,
                actual_status: status,
                body_snippet: snippet,
                latency_ms: latency,
                error,
            }
        }
        Err(e) => ProbeResult {
            passed: false,
            actual_status: 0,
            body_snippet: String::new(),
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some(format!("Request failed: {e}")),
        },
    }
}

// ─── Process Probe ───────────────────────────────────────────────────────

/// Configuration for a process probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessProbe {
    pub command: String,
    pub args: Vec<String>,
    pub expected_exit_code: i32,
    pub stdout_contains: Option<String>,
    pub timeout_secs: u64,
}

/// Result of a process probe.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessResult {
    pub passed: bool,
    pub exit_code: i32,
    pub stdout_snippet: String,
    pub stderr_snippet: String,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// Run a process probe (Tier 1 only — caller must check permissions).
pub async fn run_process_probe(probe: &ProcessProbe) -> ProcessResult {
    let start = Instant::now();
    let timeout = Duration::from_secs(probe.timeout_secs.min(120));

    let result = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(&probe.command)
            .args(&probe.args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await;

    let latency = start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout_snippet: String = stdout.chars().take(500).collect();
            let stderr_snippet: String = stderr.chars().take(500).collect();

            let mut passed = exit_code == probe.expected_exit_code;
            let mut error = None;

            if passed {
                if let Some(ref contains) = probe.stdout_contains {
                    if !stdout.contains(contains.as_str()) {
                        passed = false;
                        error = Some(format!("stdout does not contain '{}'", contains));
                    }
                }
            } else {
                error = Some(format!(
                    "Expected exit code {}, got {}",
                    probe.expected_exit_code, exit_code
                ));
            }

            info!(
                "[verify] Process {} → exit={} ({}ms) {}",
                probe.command,
                exit_code,
                latency,
                if passed { "PASS" } else { "FAIL" }
            );

            ProcessResult {
                passed,
                exit_code,
                stdout_snippet,
                stderr_snippet,
                latency_ms: latency,
                error,
            }
        }
        Ok(Err(e)) => ProcessResult {
            passed: false,
            exit_code: -1,
            stdout_snippet: String::new(),
            stderr_snippet: String::new(),
            latency_ms: latency,
            error: Some(format!("Failed to run: {e}")),
        },
        Err(_) => ProcessResult {
            passed: false,
            exit_code: -1,
            stdout_snippet: String::new(),
            stderr_snippet: String::new(),
            latency_ms: latency,
            error: Some(format!("Timed out after {}s", probe.timeout_secs)),
        },
    }
}

// ─── Log Probe ───────────────────────────────────────────────────────────

/// Result of a log probe.
#[derive(Debug, Clone, Serialize)]
pub struct LogResult {
    pub passed: bool,
    pub matches_found: usize,
    pub matching_lines: Vec<String>,
    pub error: Option<String>,
}

/// Run a log probe — check for error patterns in log files.
/// `log_path` must be relative to `data_dir` (path traversal protection).
pub fn run_log_probe(
    data_dir: &Path,
    log_file: &str,
    error_patterns: &[String],
    since_minutes: u64,
) -> LogResult {
    // Path traversal protection (same as memory tools)
    if log_file.contains("..") || log_file.starts_with('/') {
        return LogResult {
            passed: false,
            matches_found: 0,
            matching_lines: Vec::new(),
            error: Some("Path traversal detected".to_string()),
        };
    }

    let full_path = data_dir.join(log_file);
    if !full_path.exists() {
        return LogResult {
            passed: true, // no log = no errors
            matches_found: 0,
            matching_lines: Vec::new(),
            error: Some(format!("Log file not found: {}", log_file)),
        };
    }

    // Verify canonical path is inside data_dir
    if let (Ok(canonical), Ok(base)) = (full_path.canonicalize(), data_dir.canonicalize()) {
        if !canonical.starts_with(&base) {
            return LogResult {
                passed: false,
                matches_found: 0,
                matching_lines: Vec::new(),
                error: Some("Path escapes data directory".to_string()),
            };
        }
    }

    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(e) => {
            return LogResult {
                passed: false,
                matches_found: 0,
                matching_lines: Vec::new(),
                error: Some(format!("Failed to read log: {e}")),
            };
        }
    };

    // Filter by timestamp (approximate: check last N lines based on since_minutes)
    // For simplicity, just check the last `since_minutes * 10` lines
    let lines: Vec<&str> = content.lines().collect();
    let check_lines = (since_minutes * 10) as usize;
    let recent_lines = if lines.len() > check_lines {
        &lines[lines.len() - check_lines..]
    } else {
        &lines[..]
    };

    // Compile patterns
    let regexes: Vec<Regex> = error_patterns
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    let mut matching_lines = Vec::new();
    for line in recent_lines {
        for re in &regexes {
            if re.is_match(line) {
                matching_lines.push(line.to_string());
                break;
            }
        }
    }

    let passed = matching_lines.is_empty();
    info!(
        "[verify] LogProbe {} → {} matches {}",
        log_file,
        matching_lines.len(),
        if passed { "PASS" } else { "FAIL" }
    );

    LogResult {
        passed,
        matches_found: matching_lines.len(),
        matching_lines: matching_lines.into_iter().take(20).collect(), // cap at 20
        error: None,
    }
}
