//! Path canonicalization + whitelist enforcement.
//!
//! Defeats two footguns:
//! 1. Symlink escape — an attacker places a symlink inside an allowed root
//!    pointing into a protected path, then asks to write "through" it.
//! 2. `..` traversal — `<allowed>/../../src/main.rs` looks allowed in the
//!    request but isn't after canonicalization.
//!
//! Strategy: always operate on the canonical form of `path`, and compare via
//! `starts_with` against canonical forms of allowed roots and protected paths.
//! `fs::canonicalize` resolves symlinks and `..`. If the target file doesn't
//! exist yet (write-to-new-file case), canonicalize its parent instead and
//! reconstruct.

use anyhow::{Context, Result, anyhow};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    DenyProtected,
    DenyOutsideAllowed,
    DenyTraversal,
}

pub struct PathGuard {
    pub allowed_roots: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
}

impl PathGuard {
    /// Build, canonicalizing every root so later comparisons are cheap.
    /// Silently drops roots that do not exist on disk — operator error,
    /// guardian logs but keeps running (fail-closed on later writes anyway).
    pub fn new(allowed_roots: Vec<PathBuf>, protected: Vec<PathBuf>) -> Result<Self> {
        let canon = |items: Vec<PathBuf>, label: &str| -> Vec<PathBuf> {
            items
                .into_iter()
                .filter_map(|p| match std::fs::canonicalize(&p) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!(
                            path = %p.display(),
                            err = %e,
                            "guardian: {} path does not canonicalize — dropping from config",
                            label
                        );
                        None
                    }
                })
                .collect()
        };
        Ok(Self {
            allowed_roots: canon(allowed_roots, "allowed_root"),
            protected: canon(protected, "protected"),
        })
    }

    pub fn decide(&self, requested: &Path) -> Result<(Verdict, PathBuf)> {
        let canonical = resolve_even_if_missing(requested)
            .with_context(|| format!("canonicalizing {}", requested.display()))?;

        // Rule 1: if it lands inside any protected path, deny.
        for p in &self.protected {
            if canonical.starts_with(p) {
                return Ok((Verdict::DenyProtected, canonical));
            }
        }

        // Rule 2: it must be inside an allowed root.
        let within = self.allowed_roots.iter().any(|r| canonical.starts_with(r));
        if !within {
            return Ok((Verdict::DenyOutsideAllowed, canonical));
        }

        // Rule 3 (belt + suspenders): no `..` components in the post-canonical path.
        // `canonicalize` should strip these but in the missing-file branch we
        // rebuild the path ourselves, so defend against it.
        if canonical.components().any(|c| c == Component::ParentDir) {
            return Ok((Verdict::DenyTraversal, canonical));
        }

        Ok((Verdict::Allow, canonical))
    }
}

/// Canonicalize a path that may not exist yet. If the full path doesn't exist,
/// canonicalize the nearest existing ancestor and join the remainder back on.
/// Rejects any input that tries to traverse out of its canonical ancestor.
fn resolve_even_if_missing(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("path must be absolute: {}", path.display());
    }

    match std::fs::canonicalize(path) {
        Ok(p) => return Ok(p),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).context(format!("canonicalize {}", path.display()));
        }
        Err(_) => {}
    }

    // Walk up to the nearest existing ancestor.
    let mut ancestor = path.to_path_buf();
    let mut tail: Vec<PathBuf> = Vec::new();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(canon) => {
                let mut resolved = canon;
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let file_name = ancestor
                    .file_name()
                    .ok_or_else(|| anyhow!("path has no existing ancestor: {}", path.display()))?
                    .to_os_string();
                tail.push(PathBuf::from(file_name));
                if !ancestor.pop() {
                    anyhow::bail!("path has no existing ancestor: {}", path.display());
                }
            }
            Err(e) => return Err(e).context("canonicalize ancestor"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn allow_inside_allowed_root() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let guard = PathGuard::new(vec![root.clone()], vec![]).unwrap();
        let target = root.join("sub/new_file.txt");
        let (v, canon) = guard.decide(&target).unwrap();
        assert_eq!(v, Verdict::Allow);
        assert!(canon.starts_with(&root));
    }

    #[test]
    fn deny_outside_allowed_root() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let guard = PathGuard::new(vec![root.clone()], vec![]).unwrap();
        // /tmp/something-else is real but outside the allowlist
        let other_td = tempfile::tempdir().unwrap();
        let outside = other_td.path().canonicalize().unwrap().join("x.txt");
        let (v, _) = guard.decide(&outside).unwrap();
        assert_eq!(v, Verdict::DenyOutsideAllowed);
    }

    #[test]
    fn deny_protected_inside_allowed() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let protected = root.join("src");
        fs::create_dir_all(&protected).unwrap();
        let guard = PathGuard::new(vec![root.clone()], vec![protected.clone()]).unwrap();
        let target = protected.join("main.rs");
        let (v, _) = guard.decide(&target).unwrap();
        assert_eq!(v, Verdict::DenyProtected);
    }

    #[test]
    fn canonicalize_defeats_dot_dot() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let protected = root.join("src");
        let allowed = root.join("data");
        fs::create_dir_all(&protected).unwrap();
        fs::create_dir_all(&allowed).unwrap();
        fs::write(protected.join("main.rs"), b"old").unwrap();
        let guard = PathGuard::new(vec![allowed.clone()], vec![protected.clone()]).unwrap();

        // Attacker tries to escape from data/ back into src/main.rs
        let sneaky = allowed.join("../src/main.rs");
        let (v, _) = guard.decide(&sneaky).unwrap();
        assert_eq!(v, Verdict::DenyProtected);
    }

    #[test]
    fn symlink_into_protected_is_denied() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let protected = root.join("src");
        let allowed = root.join("data");
        fs::create_dir_all(&protected).unwrap();
        fs::create_dir_all(&allowed).unwrap();
        fs::write(protected.join("main.rs"), b"old").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(protected.join("main.rs"), allowed.join("decoy")).unwrap();

        let guard = PathGuard::new(vec![allowed.clone()], vec![protected.clone()]).unwrap();
        let (v, canonical) = guard.decide(&allowed.join("decoy")).unwrap();
        // The symlink resolves into the protected path, so Deny.
        assert_eq!(
            v,
            Verdict::DenyProtected,
            "canonical resolved to {}",
            canonical.display()
        );
    }

    #[test]
    fn missing_target_ok_if_parent_allowed() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let guard = PathGuard::new(vec![root.clone()], vec![]).unwrap();
        // deep/new/file.txt does not exist yet — guardian must still resolve
        let target = root.join("deep/new/file.txt");
        let (v, canon) = guard.decide(&target).unwrap();
        assert_eq!(v, Verdict::Allow);
        assert!(canon.starts_with(&root));
    }
}
