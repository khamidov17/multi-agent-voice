//! Tool dispatch — memory tools.

use std::collections::HashSet;
use std::path::PathBuf;
use tracing::debug;

/// Validate and resolve a memory path. Returns the full path if valid.
pub(crate) fn resolve_memory_path(
    data_dir: Option<&PathBuf>,
    relative_path: &str,
) -> Result<PathBuf, String> {
    let data_dir = data_dir.ok_or("No data_dir configured - memories disabled")?;
    let memories_dir = data_dir.join("memories");

    // Security: reject paths with .. or absolute paths
    if relative_path.contains("..") {
        return Err("Path cannot contain '..'".to_string());
    }
    if relative_path.starts_with('/') || relative_path.starts_with('\\') {
        return Err("Path must be relative".to_string());
    }
    if relative_path.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let full_path = memories_dir.join(relative_path);

    // Double-check: canonicalize and verify it's still within memories_dir
    // For non-existent files, canonicalize the parent
    let parent = full_path.parent().ok_or("Invalid path")?;

    // Create memories directory structure if needed
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| format!("Failed to resolve path: {e}"))?;
    let canonical_memories = memories_dir.canonicalize().unwrap_or_else(|_| {
        // memories dir might not exist yet
        std::fs::create_dir_all(&memories_dir).ok();
        memories_dir.canonicalize().unwrap_or(memories_dir.clone())
    });

    if !canonical_parent.starts_with(&canonical_memories) {
        return Err("Path must be within memories directory".to_string());
    }

    Ok(full_path)
}

pub(super) async fn execute_create_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
    content: &str,
) -> Result<Option<String>, String> {
    let full_path = resolve_memory_path(data_dir, path)?;

    // Fail if file already exists
    if full_path.exists() {
        return Err(format!(
            "File already exists: {}. Use edit_memory to modify.",
            path
        ));
    }

    debug!("📝 Creating memory: {}", path);
    std::fs::write(&full_path, content).map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(None) // Action tool
}

pub(super) async fn execute_read_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
    files_read: &mut HashSet<String>,
) -> Result<Option<String>, String> {
    let full_path = resolve_memory_path(data_dir, path)?;

    if !full_path.exists() {
        return Err(format!("File not found: {}", path));
    }

    debug!("📖 Reading memory: {}", path);
    let content =
        std::fs::read_to_string(&full_path).map_err(|e| format!("Failed to read file: {e}"))?;

    // Track that this file has been read (for edit validation)
    files_read.insert(path.to_string());

    // Format with line numbers like Claude Code's Read tool
    let numbered: String = content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:>5}→{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Some(numbered)) // Query tool - Claude needs to see the content
}

pub(super) async fn execute_edit_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
    old_string: &str,
    new_string: &str,
    files_read: &HashSet<String>,
) -> Result<Option<String>, String> {
    // Must have read the file first
    if !files_read.contains(path) {
        return Err(format!("Must read_memory('{}') before editing", path));
    }

    let full_path = resolve_memory_path(data_dir, path)?;

    if !full_path.exists() {
        return Err(format!("File not found: {}", path));
    }

    let content =
        std::fs::read_to_string(&full_path).map_err(|e| format!("Failed to read file: {e}"))?;

    // Find and replace
    let count = content.matches(old_string).count();
    if count == 0 {
        return Err("old_string not found in file. Make sure it matches exactly.".to_string());
    }
    if count > 1 {
        return Err(format!("old_string found {} times. Must be unique.", count));
    }

    debug!("✏️ Editing memory: {}", path);
    let new_content = content.replace(old_string, new_string);
    std::fs::write(&full_path, &new_content).map_err(|e| format!("Failed to write file: {e}"))?;

    Ok(None) // Action tool
}

pub(super) async fn execute_list_memories(
    data_dir: Option<&PathBuf>,
    subpath: Option<&str>,
) -> Result<Option<String>, String> {
    let data_dir = data_dir.ok_or("No data_dir configured - memories disabled")?;
    let memories_dir = data_dir.join("memories");

    let target_dir = if let Some(sub) = subpath {
        resolve_memory_path(Some(data_dir), sub)?
    } else {
        if !memories_dir.exists() {
            std::fs::create_dir_all(&memories_dir)
                .map_err(|e| format!("Failed to create memories directory: {e}"))?;
        }
        memories_dir
    };

    if !target_dir.is_dir() {
        return Err(format!("Not a directory: {}", subpath.unwrap_or(".")));
    }

    debug!("📂 Listing memories: {}", subpath.unwrap_or("."));
    let mut entries = Vec::new();
    for entry in
        std::fs::read_dir(&target_dir).map_err(|e| format!("Failed to read directory: {e}"))?
    {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{}/", name) } else { name });
    }
    entries.sort();

    Ok(Some(entries.join("\n"))) // Query tool - Claude needs to see the listing
}

pub(super) async fn execute_search_memories(
    data_dir: Option<&PathBuf>,
    pattern: &str,
    subpath: Option<&str>,
) -> Result<Option<String>, String> {
    let data_dir = data_dir.ok_or("No data_dir configured - memories disabled")?;
    let memories_dir = data_dir.join("memories");

    let search_dir = if let Some(sub) = subpath {
        resolve_memory_path(Some(data_dir), sub)?
    } else {
        if !memories_dir.exists() {
            return Ok(Some("No memories directory yet".to_string()));
        }
        memories_dir.clone()
    };

    debug!("🔍 Searching memories for: {}", pattern);
    let mut results = Vec::new();

    fn search_recursive(
        dir: &PathBuf,
        base: &PathBuf,
        pattern: &str,
        results: &mut Vec<String>,
    ) -> Result<(), String> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir).map_err(|e| format!("Read dir error: {e}"))? {
            let entry = entry.map_err(|e| format!("Entry error: {e}"))?;
            let path = entry.path();
            if path.is_dir() {
                search_recursive(&path, base, pattern, results)?;
            } else if path.is_file()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let rel_path = path.strip_prefix(base).unwrap_or(&path);
                for (line_num, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        results.push(format!("{}:{}:{}", rel_path.display(), line_num + 1, line));
                    }
                }
            }
        }
        Ok(())
    }

    search_recursive(&search_dir, &memories_dir, pattern, &mut results)?;

    if results.is_empty() {
        Ok(Some("No matches found".to_string()))
    } else {
        Ok(Some(results.join("\n")))
    }
}

pub(super) async fn execute_delete_memory(
    data_dir: Option<&PathBuf>,
    path: &str,
) -> Result<Option<String>, String> {
    let full_path = resolve_memory_path(data_dir, path)?;

    if !full_path.exists() {
        return Err(format!("File not found: {}", path));
    }

    if full_path.is_dir() {
        return Err("Cannot delete directories. Delete files individually.".to_string());
    }

    debug!("🗑️ Deleting memory: {}", path);
    std::fs::remove_file(&full_path).map_err(|e| format!("Failed to delete file: {e}"))?;

    Ok(None) // Action tool
}
