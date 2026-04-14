//! Tool dispatch — custom tools (list, build, run).

use std::time::Duration;
use tracing::info;

use crate::chatbot::engine::ChatbotConfig;

/// List all registered custom tools (ListTools).
pub(super) async fn execute_list_tools() -> Result<Option<String>, String> {
    let workspace = std::path::Path::new("workspace");
    let tools = crate::chatbot::tool_registry::load_registry(workspace);
    if tools.is_empty() {
        return Ok(Some(
            "No custom tools registered yet. Use build_tool to create one.".to_string(),
        ));
    }
    let mut lines = vec![format!("{} registered tools:", tools.len())];
    for t in &tools {
        lines.push(format!("  - {} ({}) — {}", t.name, t.path, t.description));
    }
    Ok(Some(lines.join("\n")))
}

/// Build and register a new custom tool (BuildTool). Tier 1 only.
pub(super) async fn execute_build_tool(
    config: &ChatbotConfig,
    name: &str,
    description: &str,
    language: &str,
    code: &str,
    parameters: Option<&str>,
) -> Result<Option<String>, String> {
    if !config.full_permissions {
        return Err("build_tool requires full permissions (Tier 1 only)".to_string());
    }

    if !crate::chatbot::tool_registry::validate_tool_name(name) {
        return Err(
            "Invalid tool name. Use only alphanumeric + underscore, max 64 chars.".to_string(),
        );
    }

    let ext = match language {
        "python" => "py",
        "bash" => "sh",
        _ => {
            return Err(format!(
                "Unsupported language: {}. Use 'python' or 'bash'.",
                language
            ));
        }
    };

    let workspace = std::path::Path::new("workspace");
    let tools_dir = workspace.join("tools");
    std::fs::create_dir_all(&tools_dir).map_err(|e| format!("Failed to create tools dir: {e}"))?;

    let script_path = tools_dir.join(format!("{name}.{ext}"));
    std::fs::write(&script_path, code).map_err(|e| format!("Failed to write script: {e}"))?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
    }

    let rel_path = format!("workspace/tools/{name}.{ext}");
    let entry = crate::chatbot::tool_registry::ToolRegistryEntry {
        name: name.to_string(),
        path: rel_path.clone(),
        description: description.to_string(),
        created_by: config.bot_name.clone(),
        parameters_json: parameters.map(|s| s.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    crate::chatbot::tool_registry::register_tool(workspace, entry)
        .map_err(|e| format!("Registration failed: {e}"))?;

    // Broadcast to all agents
    if let Some(ref db_path) = config.shared_bot_messages_db
        && let Ok(db) = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
    {
        let _ = db.insert_typed(
            &config.bot_name,
            None,
            &format!(
                "NEW_TOOL: I built '{}' — {}. Use run_custom_tool to try it.",
                name, description
            ),
            crate::chatbot::bot_messages::message_type::STATUS,
            None,
            None,
        );
    }

    info!("Built tool: {} at {}", name, rel_path);
    Ok(Some(format!(
        "Tool '{}' built and registered at {}. All agents notified.",
        name, rel_path
    )))
}

/// Run a registered custom tool (RunCustomTool). Tier 1 only.
pub(super) async fn execute_run_custom_tool(
    config: &ChatbotConfig,
    name: &str,
    input_json: Option<&str>,
    timeout_secs: Option<u64>,
) -> Result<Option<String>, String> {
    if !config.full_permissions {
        return Err("run_custom_tool requires full permissions (Tier 1 only)".to_string());
    }

    let workspace = std::path::Path::new("workspace");
    let tool = crate::chatbot::tool_registry::find_tool(workspace, name).ok_or_else(|| {
        format!(
            "Tool '{}' not found in registry. Use list_tools to see available.",
            name
        )
    })?;

    let path = std::path::Path::new(&tool.path);
    if !path.exists() {
        return Err(format!("Tool script not found at: {}", tool.path));
    }

    let timeout = timeout_secs.unwrap_or(60).min(300);
    let interpreter = if tool.path.ends_with(".py") {
        "python3"
    } else {
        "bash"
    };

    let mut cmd = tokio::process::Command::new(interpreter);
    cmd.arg(&tool.path);
    if let Some(input) = input_json {
        cmd.arg(input);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(Duration::from_secs(timeout), cmd.output())
        .await
        .map_err(|_| format!("Tool '{}' timed out after {}s", name, timeout))?
        .map_err(|e| format!("Failed to run tool: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(Some(format!(
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
    )))
}
