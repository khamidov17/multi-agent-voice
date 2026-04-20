//! Wire protocol for guardian UDS IPC.
//!
//! Newline-framed JSON — one request per line, one response per line.
//! The client (harness MCP `protected_write` shim) sends `Req`, the guardian
//! replies with `Resp`. The `hmac` field authenticates the triple (op, path,
//! sha256(bytes), nonce) using a shared key stored at
//! `<run_dir>/guardian.key` mode 0400.

use serde::{Deserialize, Serialize};

/// One request from client to guardian.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Req {
    pub op: Op,
    /// Absolute path the client wants to write. Guardian canonicalizes before
    /// deciding allow/deny — callers must send the intended final path, not
    /// a pre-canonicalized one.
    pub path: String,
    /// Payload bytes, base64-encoded so JSON stays 7-bit clean.
    pub bytes_b64: String,
    /// Monotonic per-session nonce. Persisted in `nonces.db`. Highest-seen
    /// wins; replay is rejected.
    pub nonce: u64,
    /// `hex(HMAC-SHA256(key, op || path || sha256(bytes) || nonce))`.
    pub hmac: String,
    /// Free-form rationale the AI caller explains — logged for audit.
    /// Optional but strongly encouraged; present in MCP tool signature.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Operations the guardian understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Create or overwrite a file at `path` with `bytes`.
    Write,
    /// Health probe — guardian returns `ok=true` without touching the filesystem.
    Ping,
    /// Owner-only break-glass write that bypasses the protected-path denial.
    /// Signed with a SEPARATE override key (`<run_dir>/override.key`, not
    /// `guardian.key`) so the harness cannot forge one even if compromised.
    /// Still enforces `allowed_uids`, nonce replay, canonicalize + O_NOFOLLOW,
    /// and the allowed-root outside check. Audit-logged with decision
    /// `override_allow`.
    OverrideWrite,
}

/// One response from guardian to client.
///
/// Structured per the DX review: machine-readable `err_code` + human-readable
/// `message` + `suggested_action` so AI callers (Nova) can reason about
/// rejections instead of retrying blind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resp {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err_code: Option<ErrCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
    /// Set on `Denied` — paths the caller may write to instead.
    /// Populated from the allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternative_roots: Option<Vec<String>>,
}

/// Rejection categories. Kept small and stable; add variants, don't reshape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode {
    /// Canonical path lands in a protected root, or is outside every allowed root.
    Denied,
    /// Path cannot be canonicalized — likely symlink loop or a parent that
    /// does not exist and cannot be created within an allowed root.
    PathTraversal,
    /// HMAC check failed. Either wrong key, tampered payload, or replayed nonce.
    BadHmac,
    /// Nonce already consumed.
    ReplayDetected,
    /// Caller's UID (via `SO_PEERCRED`) is not registered with the guardian.
    UidMismatch,
    /// Guardian's fs::write returned an error after auth passed. `message` has the cause.
    IoError,
    /// Socket saturated or timed out.
    IpcTimeout,
    /// Request payload malformed.
    Malformed,
    /// Guardian is in an admin-initiated pause (via `guardianctl pause`).
    Paused,
    /// Override-once was requested but the guardian has no `override_key_path`
    /// configured — the break-glass path is disabled on this deployment.
    OverrideDisabled,
}

impl Resp {
    pub fn ok_written(bytes: u64) -> Self {
        Self {
            ok: true,
            written_bytes: Some(bytes),
            err_code: None,
            message: None,
            suggested_action: None,
            alternative_roots: None,
        }
    }

    pub fn ok_pong() -> Self {
        Self {
            ok: true,
            written_bytes: None,
            err_code: None,
            message: None,
            suggested_action: None,
            alternative_roots: None,
        }
    }

    pub fn denied(message: impl Into<String>, alternatives: Vec<String>) -> Self {
        Self {
            ok: false,
            written_bytes: None,
            err_code: Some(ErrCode::Denied),
            message: Some(message.into()),
            suggested_action: Some(
                "Pick a path inside one of `alternative_roots`. Protected paths \
                 are reserved for human owner. If this is intentional, request a \
                 one-time override via `guardianctl override-once`."
                    .into(),
            ),
            alternative_roots: Some(alternatives),
        }
    }

    pub fn err(code: ErrCode, message: impl Into<String>) -> Self {
        let suggested = match code {
            ErrCode::BadHmac => Some(
                "Client and guardian are out of sync. Restart the harness so it \
                 re-reads guardian.key; verify file mode is 0400 owned by harness UID."
                    .into(),
            ),
            ErrCode::ReplayDetected => Some(
                "Nonce already used. Increment your in-memory nonce counter; \
                 if guardian restarted it may have rolled forward."
                    .into(),
            ),
            ErrCode::UidMismatch => Some(
                "This process (UID/PID) is not registered with the guardian. \
                 Only the harness should call this. If you are the owner, use \
                 `guardianctl override-once` instead."
                    .into(),
            ),
            ErrCode::PathTraversal => Some(
                "Path canonicalization failed. Check for broken symlinks, or \
                 a missing parent directory outside any allowed root."
                    .into(),
            ),
            ErrCode::IoError => Some(
                "Filesystem returned an error after auth. Disk full? Permissions? \
                 See `message` for kernel-level cause."
                    .into(),
            ),
            ErrCode::IpcTimeout => Some(
                "Guardian socket saturated or guardian stalled. Back off and retry \
                 once. If it persists, check guardian process health."
                    .into(),
            ),
            ErrCode::Malformed => Some(
                "Request JSON failed to parse. Check for newline in payload, \
                 wrong field names, or non-base64 in `bytes_b64`."
                    .into(),
            ),
            ErrCode::Paused => Some(
                "Guardian is admin-paused via `guardianctl pause`. Run \
                 `guardianctl resume` to re-enable writes."
                    .into(),
            ),
            ErrCode::OverrideDisabled => Some(
                "This guardian has no `override_key_path` configured, so \
                 `override-once` is disabled. Configure one in guardian.json \
                 and install an `override.key` (0400, >=32 random bytes) \
                 before attempting again."
                    .into(),
            ),
            ErrCode::Denied => None,
        };
        Self {
            ok: false,
            written_bytes: None,
            err_code: Some(code),
            message: Some(message.into()),
            suggested_action: suggested,
            alternative_roots: None,
        }
    }
}
