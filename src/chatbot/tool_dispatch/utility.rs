//! Tool dispatch — utility tools.

use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::chatbot::database::Database;
use crate::chatbot::engine::ChatbotConfig;
use crate::chatbot::format::strip_html_tags;
use crate::chatbot::telegram::TelegramClient;
use crate::chatbot::yandex;

/// Returns (json_info, optional_profile_photo_bytes)
pub(super) async fn execute_get_user_info(
    config: &ChatbotConfig,
    database: &Mutex<Database>,
    telegram: &TelegramClient,
    user_id: Option<i64>,
    username: Option<&str>,
) -> Result<(String, Option<Vec<u8>>), String> {
    // Resolve user_id from username if needed
    let resolved_id = if let Some(id) = user_id {
        id
    } else if let Some(name) = username {
        let db = database.lock().await;
        db.find_user_by_username(name)
            .map(|m| m.user_id)
            .ok_or_else(|| format!("User '{}' not found in database", name))?
    } else {
        return Err("get_user_info requires user_id or username".to_string());
    };

    let info = telegram
        .get_chat_member(config.primary_chat_id, resolved_id)
        .await?;

    // Try to get profile photo
    let profile_photo = match telegram.get_profile_photo(resolved_id).await {
        Ok(photo) => photo,
        Err(e) => {
            warn!("Failed to get profile photo: {e}");
            None
        }
    };

    let json_info = serde_json::json!({
        "user_id": info.user_id,
        "username": info.username,
        "first_name": info.first_name,
        "last_name": info.last_name,
        "is_bot": info.is_bot,
        "is_premium": info.is_premium,
        "language_code": info.language_code,
        "status": info.status,
        "custom_title": info.custom_title,
        "has_profile_photo": profile_photo.is_some()
    })
    .to_string();

    Ok((json_info, profile_photo))
}

pub(super) async fn execute_query(
    database: &Mutex<Database>,
    sql: &str,
) -> Result<Option<String>, String> {
    let store = database.lock().await;
    let preview: String = sql.chars().take(80).collect();
    info!("📚 Executing query: {}", preview);
    let result = store.query(sql)?;
    Ok(Some(result))
}

pub(super) fn execute_now(utc_offset: Option<i32>) -> Result<Option<String>, String> {
    let offset_hours = utc_offset.unwrap_or(0).clamp(-12, 14);
    let now = chrono::Utc::now();
    let offset = chrono::Duration::hours(offset_hours as i64);
    let local = now + offset;
    let sign = if offset_hours >= 0 { "+" } else { "" };
    Ok(Some(format!(
        "Current time: {} (UTC{sign}{offset_hours})",
        local.format("%Y-%m-%d %H:%M:%S")
    )))
}

/// Report a bug to the developer feedback file.
pub(super) async fn execute_report_bug(
    data_dir: Option<&PathBuf>,
    description: &str,
    severity: Option<&str>,
) -> Result<Option<String>, String> {
    let data_dir = data_dir.ok_or("No data_dir configured")?;
    let feedback_file = data_dir.join("feedback.log");

    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let severity = severity.unwrap_or("medium");

    let entry = format!(
        "\n---\n[{}] severity={}\n{}\n",
        timestamp, severity, description
    );

    let preview: String = description.chars().take(50).collect();
    info!("🐛 Bug report ({}): {}", severity, preview);

    // Append to feedback file
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&feedback_file)
        .map_err(|e| format!("Failed to open feedback file: {e}"))?;

    file.write_all(entry.as_bytes())
        .map_err(|e| format!("Failed to write feedback: {e}"))?;

    Ok(None) // Action tool - developer will see it via the poller
}

/// Get system performance metrics (GetMetrics tool).
pub(super) async fn execute_get_metrics(
    database: &Mutex<Database>,
    last_n: u64,
) -> Result<Option<String>, String> {
    let db = database.lock().await;
    let conn = db.connection().lock().unwrap();
    let snapshots = crate::chatbot::metrics::get_recent_snapshots(&conn, last_n);
    if snapshots.is_empty() {
        return Ok(Some(
            "No metrics data yet. Metrics are flushed every 5 minutes.".to_string(),
        ));
    }
    Ok(Some(crate::chatbot::metrics::format_metrics_summary(
        &snapshots,
    )))
}

/// Get recent turn snapshots (GetSnapshots tool).
pub(super) async fn execute_get_snapshots(
    database: &Mutex<Database>,
    config: &ChatbotConfig,
    count: u64,
) -> Result<Option<String>, String> {
    let db = database.lock().await;
    let snapshots =
        crate::chatbot::snapshot::get_snapshots_since(db.connection(), &config.bot_name, count);
    Ok(Some(crate::chatbot::snapshot::format_snapshots_summary(
        &snapshots,
    )))
}

pub(super) async fn execute_web_search(
    telegram: &TelegramClient,
    chat_id: i64,
    query: &str,
    api_key: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    info!("🔍 Web search: {}", query);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", api_key)
        .query(&[("q", query), ("count", "5")])
        .send()
        .await
        .map_err(|e| format!("Brave Search request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Brave Search API error {status}: {body}"));
    }

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Brave Search response: {e}"))?;

    let results = data["web"]["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    let title = r["title"].as_str().unwrap_or("");
                    let url = r["url"].as_str().unwrap_or("");
                    let desc = r["description"].as_str().unwrap_or("");
                    format!("<b>{}</b>\n{}\n{}", title, url, desc)
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();

    if results.is_empty() {
        return Ok(Some("No results found.".to_string()));
    }

    let text = format!("🔍 <b>{}</b>\n\n{}", query, results);
    info!("🔍 Search results: {} chars", text.len());

    telegram
        .send_message(chat_id, &text, reply_to_message_id)
        .await
        .map_err(|e| format!("Failed to send search results: {e}"))?;

    Ok(Some(format!("Search results for '{}' sent.", query)))
}

/// Check if an IP address is private/internal (SSRF protection layer 9).
pub(crate) fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()                         // 127.0.0.0/8
                || v4.is_private()                   // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || v4.is_link_local()                // 169.254.0.0/16 (AWS metadata etc.)
                || v4.is_broadcast()                 // 255.255.255.255
                || v4.is_unspecified()               // 0.0.0.0
                || v4.octets()[0] == 100 && v4.octets()[1] >= 64 && v4.octets()[1] <= 127 // 100.64.0.0/10 (CGNAT)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()                         // ::1
                || v6.is_unspecified()               // ::
                || {
                    let segs = v6.segments();
                    (segs[0] >> 9) == 0x7e               // fc00::/7 (full ULA range)
                        || (segs[0] & 0xffc0) == 0xfe80  // fe80::/10 (full link-local)
                        || (segs[0] == 0x2001 && segs[1] == 0x0db8)  // 2001:db8::/32 (documentation)
                }
                // IPv4-mapped IPv6 (::ffff:x.x.x.x) — check the inner v4 address
                || v6.to_ipv4_mapped()
                    .map(|v4| is_private_ip(&std::net::IpAddr::V4(v4)))
                    .unwrap_or(false)
        }
    }
}

/// Validate a URL is safe to fetch (no SSRF into internal networks).
pub(crate) async fn validate_url_ssrf(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Invalid URL: {e}"))?;

    // Only allow http/https schemes
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(format!(
                "Blocked scheme: {scheme} (only http/https allowed)"
            ));
        }
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Resolve DNS and check all IPs
    use tokio::net::lookup_host;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> = lookup_host(&addr)
        .await
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("No DNS records for {host}"));
    }

    for addr in &addrs {
        if is_private_ip(&addr.ip()) {
            warn!("SSRF blocked: {url} resolves to private IP {}", addr.ip());
            return Err(format!(
                "Blocked: URL resolves to private/internal IP ({})",
                addr.ip()
            ));
        }
    }

    Ok(())
}

pub(super) async fn execute_fetch_url(url: &str) -> Result<Option<String>, String> {
    info!("🌐 Fetching URL: {}", url);

    // SSRF protection: validate URL before fetching
    validate_url_ssrf(url).await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; Atlas/1.0)")
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch URL: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} for {url}"));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Handle PDF: detect by Content-Type or URL extension
    if content_type.contains("pdf") || url.to_lowercase().ends_with(".pdf") {
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read PDF bytes: {e}"))?;
        let text = crate::chatbot::document::extract_pdf(&bytes)
            .map_err(|e| format!("PDF text extraction failed: {e}"))?;
        let preview: String = text.chars().take(80).collect();
        info!(
            "🌐 Fetched PDF from {}: {} chars, preview: \"{}\"...",
            url,
            text.len(),
            preview
        );
        return Ok(Some(text));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let text = if content_type.contains("html") || body.trim_start().starts_with('<') {
        strip_html_tags(&body)
    } else {
        body
    };

    // Truncate to ~8000 chars (UTF-8 safe — never split mid-character)
    let result = if text.chars().count() > 8000 {
        let truncated: String = text.chars().take(8000).collect();
        format!("{truncated}...[truncated at 8000 chars]")
    } else {
        text
    };

    let preview: String = result.chars().take(80).collect();
    info!(
        "🌐 Fetched {} bytes from {}: \"{}\"...",
        result.len(),
        url,
        preview
    );

    Ok(Some(result))
}

pub(super) async fn execute_yandex_geocode(
    config: &ChatbotConfig,
    address: &str,
) -> Result<Option<String>, String> {
    let key = config
        .yandex_api_key
        .as_deref()
        .ok_or("Yandex API key not configured")?;
    let (name, lon, lat) = yandex::geocode(address, key).await?;
    Ok(Some(format!(
        "📍 {name}\nCoordinates: {lat:.6}, {lon:.6} (lat, lon)"
    )))
}

pub(super) async fn execute_yandex_map(
    config: &ChatbotConfig,
    telegram: &TelegramClient,
    chat_id: i64,
    address: &str,
    reply_to: Option<i64>,
) -> Result<Option<String>, String> {
    let key = config
        .yandex_api_key
        .as_deref()
        .ok_or("Yandex API key not configured")?;
    let (name, lon, lat) = yandex::geocode(address, key).await?;
    let image = yandex::static_map(lon, lat, key, 15).await?;
    telegram
        .send_image(chat_id, image, Some(&name), reply_to)
        .await?;
    Ok(None)
}

pub(super) async fn execute_set_reminder(
    config: &ChatbotConfig,
    chat_id: i64,
    message: &str,
    trigger_at_str: &str,
    repeat_cron: Option<&str>,
) -> Result<Option<String>, String> {
    use crate::chatbot::reminders::parse_trigger_at;
    let store = config
        .reminder_store
        .as_ref()
        .ok_or("Reminder store not configured")?;
    let trigger_at = parse_trigger_at(trigger_at_str)?;
    let id = store.set(chat_id, 0, message, trigger_at, repeat_cron)?;
    let human = trigger_at.format("%Y-%m-%d %H:%M UTC").to_string();
    info!("⏰ Reminder {} set for {} at {}", id, chat_id, human);
    Ok(Some(format!("Reminder #{id} set — will fire at {human}")))
}

pub(super) async fn execute_list_reminders(
    config: &ChatbotConfig,
    chat_id: Option<i64>,
) -> Result<Option<String>, String> {
    let store = config
        .reminder_store
        .as_ref()
        .ok_or("Reminder store not configured")?;
    let reminders = store.list(chat_id)?;
    if reminders.is_empty() {
        return Ok(Some("No active reminders.".to_string()));
    }
    let lines: Vec<String> = reminders
        .iter()
        .map(|r| {
            let repeat = r
                .repeat_cron
                .as_deref()
                .map(|c| format!(" (repeat: {c})"))
                .unwrap_or_default();
            format!(
                "#{}: chat={} at {}{} — {}",
                r.id,
                r.chat_id,
                r.trigger_at.format("%Y-%m-%d %H:%M UTC"),
                repeat,
                r.message
            )
        })
        .collect();
    Ok(Some(lines.join("\n")))
}

pub(super) async fn execute_cancel_reminder(
    config: &ChatbotConfig,
    reminder_id: i64,
) -> Result<Option<String>, String> {
    let store = config
        .reminder_store
        .as_ref()
        .ok_or("Reminder store not configured")?;
    if store.cancel(reminder_id)? {
        Ok(Some(format!("Reminder #{reminder_id} cancelled.")))
    } else {
        Err(format!(
            "Reminder #{reminder_id} not found or already inactive."
        ))
    }
}

pub(super) async fn execute_create_spreadsheet(
    telegram: &TelegramClient,
    chat_id: i64,
    filename: &str,
    sheets: &[serde_json::Value],
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    use rust_xlsxwriter::Workbook;

    info!("📊 Creating spreadsheet: {}", filename);

    let mut workbook = Workbook::new();

    for sheet_val in sheets {
        let name = sheet_val
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Sheet");
        let headers = sheet_val
            .get("headers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|h| h.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let rows = sheet_val
            .get("rows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let worksheet = workbook.add_worksheet();
        worksheet
            .set_name(name)
            .map_err(|e| format!("Invalid sheet name: {e}"))?;

        // Write headers in row 0
        for (col, header) in headers.iter().enumerate() {
            worksheet
                .write_string(0, col as u16, header)
                .map_err(|e| format!("Failed to write header: {e}"))?;
        }

        // Write data rows starting at row 1
        for (row_idx, row) in rows.iter().enumerate() {
            if let Some(cells) = row.as_array() {
                for (col, cell) in cells.iter().enumerate() {
                    let row_num = (row_idx + 1) as u32;
                    let col_num = col as u16;
                    match cell {
                        serde_json::Value::Number(n) => {
                            if let Some(f) = n.as_f64() {
                                worksheet
                                    .write_number(row_num, col_num, f)
                                    .map_err(|e| format!("Failed to write number: {e}"))?;
                            }
                        }
                        serde_json::Value::Bool(b) => {
                            worksheet
                                .write_boolean(row_num, col_num, *b)
                                .map_err(|e| format!("Failed to write bool: {e}"))?;
                        }
                        serde_json::Value::Null => {}
                        other => {
                            worksheet
                                .write_string(
                                    row_num,
                                    col_num,
                                    other.to_string().trim_matches('"').to_string(),
                                )
                                .map_err(|e| format!("Failed to write cell: {e}"))?;
                        }
                    }
                }
            }
        }
    }

    let xlsx_bytes = workbook
        .save_to_buffer()
        .map_err(|e| format!("Failed to save workbook: {e}"))?;
    info!("📊 Spreadsheet created: {} bytes", xlsx_bytes.len());

    let caption = format!("📊 {}", filename);
    telegram
        .send_document(
            chat_id,
            xlsx_bytes,
            filename,
            Some(&caption),
            reply_to_message_id,
        )
        .await?;

    Ok(Some(format!(
        "Spreadsheet '{}' sent successfully.",
        filename
    )))
}

pub(super) async fn execute_create_pdf(
    telegram: &TelegramClient,
    chat_id: i64,
    filename: &str,
    content: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    use std::process::Command;

    info!("📄 Creating PDF: {}", filename);

    let temp_dir = std::env::temp_dir();
    let html_path = temp_dir.join(format!("atlas_pdf_{}.html", std::process::id()));
    let pdf_path = temp_dir.join(format!("atlas_pdf_{}.pdf", std::process::id()));

    std::fs::write(&html_path, content.as_bytes())
        .map_err(|e| format!("Failed to write HTML temp file: {e}"))?;

    let output = Command::new("wkhtmltopdf")
        .args([
            "--quiet",
            html_path.to_str().unwrap(),
            pdf_path.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| format!("wkhtmltopdf not found (install wkhtmltopdf): {e}"))?;

    let _ = std::fs::remove_file(&html_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("wkhtmltopdf failed: {}", stderr));
    }

    let pdf_bytes =
        std::fs::read(&pdf_path).map_err(|e| format!("Failed to read PDF output: {e}"))?;
    let _ = std::fs::remove_file(&pdf_path);

    info!("📄 PDF created: {} bytes", pdf_bytes.len());

    let caption = format!("📄 {}", filename);
    telegram
        .send_document(
            chat_id,
            pdf_bytes,
            filename,
            Some(&caption),
            reply_to_message_id,
        )
        .await?;

    Ok(Some(format!("PDF '{}' sent successfully.", filename)))
}

pub(super) async fn execute_create_word(
    telegram: &TelegramClient,
    chat_id: i64,
    filename: &str,
    content: &str,
    reply_to_message_id: Option<i64>,
) -> Result<Option<String>, String> {
    use std::process::Command;

    info!("📝 Creating Word doc: {}", filename);

    let temp_dir = std::env::temp_dir();
    let md_path = temp_dir.join(format!("atlas_word_{}.md", std::process::id()));
    let docx_path = temp_dir.join(format!("atlas_word_{}.docx", std::process::id()));

    std::fs::write(&md_path, content.as_bytes())
        .map_err(|e| format!("Failed to write Markdown temp file: {e}"))?;

    let output = Command::new("pandoc")
        .args([md_path.to_str().unwrap(), "-o", docx_path.to_str().unwrap()])
        .output()
        .map_err(|e| format!("pandoc not found (install pandoc): {e}"))?;

    let _ = std::fs::remove_file(&md_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pandoc failed: {}", stderr));
    }

    let docx_bytes =
        std::fs::read(&docx_path).map_err(|e| format!("Failed to read DOCX output: {e}"))?;
    let _ = std::fs::remove_file(&docx_path);

    info!("📝 DOCX created: {} bytes", docx_bytes.len());

    let caption = format!("📝 {}", filename);
    telegram
        .send_document(
            chat_id,
            docx_bytes,
            filename,
            Some(&caption),
            reply_to_message_id,
        )
        .await?;

    Ok(Some(format!(
        "Word document '{}' sent successfully.",
        filename
    )))
}

/// Execute a script file (run_script tool).
/// Scripts must be inside workspace/ or scripts/ directory for security.
pub(super) async fn execute_run_script(
    config: &ChatbotConfig,
    path: &str,
    args: &[String],
    timeout: u64,
) -> Result<Option<String>, String> {
    // Security: only full_permissions bots can run scripts
    if !config.full_permissions {
        return Err("run_script requires full permissions (Tier 1 only)".to_string());
    }

    // Security: canonicalize path and verify it's inside workspace/ or scripts/
    // Prevents ../traversal and symlink escapes.
    let script_path = std::path::Path::new(path);
    if !script_path.exists() {
        return Err(format!("Script not found: {path}"));
    }

    let canonical = script_path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve script path: {e}"))?;
    let cwd = std::env::current_dir().map_err(|e| format!("Cannot get cwd: {e}"))?;
    let workspace_dir = cwd
        .join("workspace")
        .canonicalize()
        .unwrap_or_else(|_| cwd.join("workspace"));
    let scripts_dir = cwd
        .join("scripts")
        .canonicalize()
        .unwrap_or_else(|_| cwd.join("scripts"));

    if !canonical.starts_with(&workspace_dir) && !canonical.starts_with(&scripts_dir) {
        return Err(format!(
            "Security: script {} resolves to {} which is outside workspace/ and scripts/",
            path,
            canonical.display()
        ));
    }

    let timeout_secs = timeout.min(300); // cap at 5 min
    info!(
        "Running script: {} {:?} (timeout={}s)",
        path, args, timeout_secs
    );

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg(path);
    for arg in args {
        cmd.arg(arg);
    }
    // Confine script execution to workspace directory
    cmd.current_dir(&workspace_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| format!("Script timed out after {timeout_secs}s"))?
        .map_err(|e| format!("Failed to run script: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    let result = format!(
        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
        exit_code,
        if stdout.len() > 4000 {
            &stdout[..4000]
        } else {
            &stdout
        },
        if stderr.len() > 2000 {
            &stderr[..2000]
        } else {
            &stderr
        },
    );

    Ok(Some(result))
}

/// Execute Docker compose commands (docker_run tool).
pub(super) async fn execute_docker_run(
    config: &ChatbotConfig,
    compose_file: &str,
    action: &str,
) -> Result<Option<String>, String> {
    if !config.full_permissions {
        return Err("docker_run requires full permissions (Tier 1 only)".to_string());
    }

    // Security: canonicalize and verify compose file is inside workspace/
    let compose_path = std::path::Path::new(compose_file);
    if !compose_path.exists() {
        return Err(format!("Compose file not found: {compose_file}"));
    }
    let canonical = compose_path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve compose path: {e}"))?;
    let cwd = std::env::current_dir().unwrap_or_default();
    let workspace_dir = cwd
        .join("workspace")
        .canonicalize()
        .unwrap_or_else(|_| cwd.join("workspace"));
    if !canonical.starts_with(&workspace_dir) {
        return Err(format!(
            "Security: compose file must be inside workspace/. {} resolves to {}",
            compose_file,
            canonical.display()
        ));
    }

    let args = match action {
        "up" => vec!["-f", compose_file, "up", "-d"],
        "down" => vec!["-f", compose_file, "down"],
        "logs" => vec!["-f", compose_file, "logs", "--tail", "50"],
        "ps" => vec!["-f", compose_file, "ps"],
        _ => return Err(format!("Unknown docker action: {action}")),
    };

    info!("Docker: {} {}", action, compose_file);

    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("docker")
            .arg("compose")
            .args(&args)
            .output(),
    )
    .await
    .map_err(|_| "Docker command timed out")?
    .map_err(|e| format!("Docker failed: {e}"))?;

    let result = format!(
        "exit_code: {}\n{}{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    Ok(Some(result))
}

/// Execute the generic evaluation suite (run_eval tool).
pub(super) async fn execute_run_eval(
    config: &ChatbotConfig,
    vars: &str,
    all: bool,
) -> Result<Option<String>, String> {
    // Security: only full_permissions or Sentinel (tools_override with Bash) can run eval
    // Atlas (WebSearch only) must not be able to trigger shell commands
    if !config.full_permissions && config.bot_name != "Security" {
        return Err("run_eval requires Bash access (Nova or Sentinel only)".to_string());
    }
    let mut cmd_args = vec!["rag/eval_runner.py".to_string()];
    if !vars.is_empty() {
        cmd_args.push("--vars".to_string());
        cmd_args.push(vars.to_string());
    }
    if all {
        cmd_args.push("--all".to_string());
    }
    cmd_args.push("--json".to_string());

    info!("Running eval: python3 {}", cmd_args.join(" "));

    let output = tokio::time::timeout(
        Duration::from_secs(600),
        tokio::process::Command::new("python3")
            .args(&cmd_args)
            .output(),
    )
    .await
    .map_err(|_| "Evaluation timed out (>600s)")?
    .map_err(|e| format!("Eval failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stderr.is_empty() && output.status.code() != Some(0) {
        return Err(format!("Eval error: {}", &stderr[..stderr.len().min(1000)]));
    }

    Ok(Some(stdout.to_string()))
}

/// Check experiment history (check_experiments tool).
/// All agents can use this — reads experiments.jsonl directly, no Bash needed.
pub(super) async fn execute_check_experiments(query: &str) -> Result<Option<String>, String> {
    let log_path = std::path::Path::new("data/shared/experiments.jsonl");
    if !log_path.exists() {
        return Ok(Some("No experiments logged yet.".to_string()));
    }

    let content = std::fs::read_to_string(log_path)
        .map_err(|e| format!("Failed to read experiments: {e}"))?;

    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    if entries.is_empty() {
        return Ok(Some("No experiments logged yet.".to_string()));
    }

    match query {
        "summary" => {
            let total = entries.len();
            let passed = entries.iter().filter(|e| e["verdict"] == "PASS").count();
            let mut methods: std::collections::HashMap<String, (usize, usize)> =
                std::collections::HashMap::new();
            for e in &entries {
                let method = e["method"].as_str().unwrap_or("unknown").to_string();
                let entry = methods.entry(method).or_insert((0, 0));
                if e["verdict"] == "PASS" {
                    entry.0 += 1;
                } else {
                    entry.1 += 1;
                }
            }
            let mut result = format!(
                "EXPERIMENT SUMMARY\nTotal: {} ({} PASS, {} FAIL)\n\nMethods:\n",
                total,
                passed,
                total - passed
            );
            for (method, (p, f)) in &methods {
                let status = if *p > 0 { "worked" } else { "NEVER passed" };
                result.push_str(&format!("  [{}P/{}F] {} — {}\n", p, f, method, status));
            }
            result.push_str("\nCheck before planning: don't repeat methods that NEVER passed.");
            Ok(Some(result))
        }
        "view" => {
            let recent: Vec<_> = entries.iter().rev().take(10).collect();
            let mut result = format!("Last {} experiments:\n\n", recent.len());
            for e in recent {
                result.push_str(&format!(
                    "[{}] {} — {}\n  Metrics: {}\n\n",
                    e["verdict"].as_str().unwrap_or("?"),
                    e["task"].as_str().unwrap_or("?"),
                    e["method"].as_str().unwrap_or("?"),
                    e["metrics"],
                ));
            }
            Ok(Some(result))
        }
        keyword => {
            let matches: Vec<_> = entries
                .iter()
                .filter(|e| {
                    let s = serde_json::to_string(e).unwrap_or_default().to_lowercase();
                    s.contains(&keyword.to_lowercase())
                })
                .collect();
            if matches.is_empty() {
                Ok(Some(format!("No experiments matching '{keyword}'.")))
            } else {
                let mut result = format!(
                    "Found {} experiments matching '{keyword}':\n\n",
                    matches.len()
                );
                for e in &matches {
                    result.push_str(&format!(
                        "[{}] {} — {}\n",
                        e["verdict"].as_str().unwrap_or("?"),
                        e["task"].as_str().unwrap_or("?"),
                        e["method"].as_str().unwrap_or("?")
                    ));
                }
                Ok(Some(result))
            }
        }
    }
}

// ─── Shared state tools ─────────────────────────────────────────────────

/// Set a shared state value (SetState tool).
pub(super) async fn execute_set_state(
    config: &ChatbotConfig,
    key: &str,
    value_json: &str,
    workflow_id: Option<&str>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    // Validate JSON
    let value: serde_json::Value =
        serde_json::from_str(value_json).map_err(|e| format!("Invalid JSON value: {e}"))?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    db.set_state(key, &value, &config.bot_name, workflow_id)
        .map_err(|e| format!("Failed to set state: {e}"))?;

    Ok(Some(format!("State '{}' set successfully.", key)))
}

/// Get a shared state value (GetState tool).
pub(super) async fn execute_get_state(
    config: &ChatbotConfig,
    key: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    match db.get_state(key).map_err(|e| format!("DB error: {e}"))? {
        Some(value) => Ok(Some(
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        )),
        None => Ok(Some(format!("State key '{}' not found.", key))),
    }
}

// ─── Token budget tool ──────────────────────────────────────────────────

/// Get token budget status (GetTokenBudget tool).
pub(super) async fn execute_get_token_budget(
    database: &Mutex<Database>,
    config: &ChatbotConfig,
) -> Result<Option<String>, String> {
    let db = database.lock().await;
    let conn = db.connection();
    let conn = conn.lock().unwrap();

    // Check if table exists
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='token_budget'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !table_exists {
        return Ok(Some(
            "Token budget tracking not yet initialized (no data).".to_string(),
        ));
    }

    // Query spending by source in last 24h
    let mut stmt = conn
        .prepare(
            "SELECT source, SUM(estimated_tokens) as total
             FROM token_budget WHERE timestamp > datetime('now', '-24 hours')
             GROUP BY source ORDER BY total DESC",
        )
        .map_err(|e| format!("Query error: {e}"))?;

    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| format!("Query error: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    let total: i64 = rows.iter().map(|(_, t)| t).sum();
    let budget = config.cognitive_daily_token_budget;
    let remaining = budget.saturating_sub(total as u64);

    let mut lines = vec![format!(
        "Token budget (last 24h): {total} / {budget} ({remaining} remaining)"
    )];
    for (source, tokens) in &rows {
        let pct = if total > 0 {
            (*tokens as f64 / total as f64 * 100.0) as u64
        } else {
            0
        };
        lines.push(format!("  {}: {} tokens ({}%)", source, tokens, pct));
    }

    Ok(Some(lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_is_private_ip_v4() {
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_private_ip(&IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        // Public
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn test_is_private_ip_v6() {
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
        // Public
        assert!(!is_private_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2607, 0xf8b0, 0x4004, 0x800, 0, 0, 0, 0x200e
        ))));
    }

    #[test]
    fn test_is_private_ipv4_mapped_v6() {
        let mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001);
        assert!(is_private_ip(&IpAddr::V6(mapped)));
        let mapped_pub = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808);
        assert!(!is_private_ip(&IpAddr::V6(mapped_pub)));
    }
}
