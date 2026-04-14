//! Dynamic tool registry — discover, build, register, and use custom tools at runtime.
//!
//! Backed by workspace/tools/registry.yaml. Agents can list available tools,
//! Nova can build new ones, and all agents get notified via bot_messages bus.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// A registered custom tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRegistryEntry {
    pub name: String,
    pub path: String,
    pub description: String,
    pub created_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters_json: Option<String>,
    #[serde(default = "default_created_at")]
    pub created_at: String,
}

fn default_created_at() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The registry YAML structure.
#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    tools: Vec<ToolRegistryEntry>,
}

fn registry_path(workspace_dir: &Path) -> std::path::PathBuf {
    workspace_dir.join("tools").join("registry.yaml")
}

/// Load all registered tools from registry.yaml.
pub fn load_registry(workspace_dir: &Path) -> Vec<ToolRegistryEntry> {
    let path = registry_path(workspace_dir);
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_yaml::from_str::<RegistryFile>(&content) {
            Ok(reg) => reg.tools,
            Err(e) => {
                warn!("Failed to parse registry.yaml: {e}");
                Vec::new()
            }
        },
        Err(e) => {
            warn!("Failed to read registry.yaml: {e}");
            Vec::new()
        }
    }
}

/// Save the full registry to registry.yaml.
pub fn save_registry(workspace_dir: &Path, entries: &[ToolRegistryEntry]) -> anyhow::Result<()> {
    let path = registry_path(workspace_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let reg = RegistryFile {
        tools: entries.to_vec(),
    };
    let yaml = serde_yaml::to_string(&reg)?;
    std::fs::write(&path, yaml)?;
    Ok(())
}

/// Register a new tool (append to registry).
pub fn register_tool(workspace_dir: &Path, entry: ToolRegistryEntry) -> anyhow::Result<()> {
    let mut entries = load_registry(workspace_dir);

    // Replace if name already exists
    entries.retain(|e| e.name != entry.name);
    entries.push(entry.clone());

    save_registry(workspace_dir, &entries)?;
    info!("Registered tool: {} at {}", entry.name, entry.path);
    Ok(())
}

/// Find a tool by name.
pub fn find_tool(workspace_dir: &Path, name: &str) -> Option<ToolRegistryEntry> {
    load_registry(workspace_dir)
        .into_iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
}

/// Validate a tool name (alphanumeric + underscore only, prevents path traversal).
pub fn validate_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
