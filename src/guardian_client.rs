//! Client-side library for talking to the bootstrap-guardian over UDS.
//!
//! Usage from the harness:
//! ```no_run
//! use std::path::Path;
//! # use claudir::guardian_client::{GuardianClient, WriteResult};
//! let client = GuardianClient::new(
//!     Path::new("/opt/claudir/run/bootstrap-guardian.sock"),
//!     Path::new("/opt/claudir/run/guardian.key"),
//! ).expect("guardian client");
//! let result = client.protected_write(
//!     "/opt/nova/data/memories/note.md",
//!     b"hello, world",
//!     "Nova is writing a memory file",
//! ).expect("write");
//! match result {
//!     WriteResult::Ok { written_bytes: _ } => {}
//!     WriteResult::Denied { reason, alternatives: _ } => eprintln!("denied: {}", reason),
//!     WriteResult::Err { message, .. } => eprintln!("err: {}", message),
//! }
//! ```
//!
//! The MCP tool that Nova calls (`protected_write(path, content, reason)`)
//! wraps this client. When/if Nova's Claude Code tool string eventually
//! drops `Edit, Write`, Nova's only path to writing files in the Claudir
//! tree will be this tool, which goes through the guardian.
//!
//! **Status:** the library is complete. The MCP tool wiring in
//! `tool_dispatch/` is not yet landed — tracked in TODOS.md.
//!
//! ## Security envelope
//!
//! The client signs every request with HMAC-SHA256 over
//! `op || "|" || path || "|" || sha256(bytes) || "|" || nonce_le`. The
//! guardian verifies the HMAC + checks `SO_PEERCRED` against its
//! `allowed_uids`. The signing key is read from a 0400 file that only
//! the harness UID can read. Nova-in-Claude-Code never sees the key.
//!
//! ## Nonce handling
//!
//! Each client instance tracks an in-memory monotonic nonce. On process
//! restart, nonces reset to 0 but the guardian remembers the highest
//! nonce it has seen per UID, so the client will get `ReplayDetected`
//! until it advances past the stored counter. The client bumps past the
//! rejection automatically via timestamp-derived nonces (nanoseconds
//! since epoch), so a fresh harness after a crash will almost always
//! hit a brand-new nonce region.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Typed mirror of the guardian's `ErrCode`. Kept as a separate type (not
/// a re-export) so the harness has no cargo dep on `bootstrap-guardian`.
/// serde rename strings are **pinned** — must match `proto::ErrCode` in the
/// guardian crate exactly. Any drift is caught by the HMAC-fixture tests
/// + the wire-version proto_version field.
///
/// Carries an `Unknown(String)` escape hatch so a newer guardian returning
/// an unseen code still deserializes; the caller can log it rather than
/// crashing deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientErrCode {
    Denied,
    PathTraversal,
    BadHmac,
    ReplayDetected,
    UidMismatch,
    IoError,
    IpcTimeout,
    Malformed,
    Paused,
    OverrideDisabled,
    /// Captures any guardian-side variant we don't recognize, so a newer
    /// guardian returning (say) `rate_limited` still deserializes. Callers
    /// treat this as "try again later, with human review."
    #[serde(other)]
    Unknown,
}

impl ClientErrCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::PathTraversal => "path_traversal",
            Self::BadHmac => "bad_hmac",
            Self::ReplayDetected => "replay_detected",
            Self::UidMismatch => "uid_mismatch",
            Self::IoError => "io_error",
            Self::IpcTimeout => "ipc_timeout",
            Self::Malformed => "malformed",
            Self::Paused => "paused",
            Self::OverrideDisabled => "override_disabled",
            Self::Unknown => "unknown",
        }
    }
}

/// Public result of a `protected_write` call.
///
/// `Err` carries a typed `ClientErrCode` so Nova/the MCP tool can branch
/// cleanly on known categories (Paused vs IoError vs Malformed) rather than
/// string-matching a raw error code. Previously every non-Denied guardian
/// err collapsed into a generic `{code: String, ...}` shape (/review
/// api-contract + maintainability).
#[derive(Debug, Clone)]
pub enum WriteResult {
    Ok {
        written_bytes: u64,
    },
    Denied {
        reason: String,
        alternatives: Vec<String>,
    },
    Err {
        code: ClientErrCode,
        message: String,
        suggested_action: Option<String>,
    },
}

impl WriteResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, WriteResult::Ok { .. })
    }
}

pub struct GuardianClient {
    socket_path: PathBuf,
    // Key bytes — never Debug-printed (see Debug impl below).
    key: Vec<u8>,
    /// Retained so a supervisor task can periodically re-stat the key file
    /// to catch mode drift (0400 → 0644 after a disk-restore, a misapplied
    /// chmod, etc.). The key bytes themselves are never reloaded from disk
    /// at runtime — rotating the key is a restart-the-harness operation.
    key_path: PathBuf,
    nonce: AtomicU64,
    /// Serializes writes so multiple harness threads take turns talking
    /// to the guardian — one request per connection is cheap and keeps
    /// ordering predictable.
    connect_lock: Mutex<()>,
    /// Timeout for both read and write on the UDS stream.
    timeout: Duration,
}

// Manual Debug so the HMAC key never leaks into logs via `{:?}`.
impl std::fmt::Debug for GuardianClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardianClient")
            .field("socket_path", &self.socket_path)
            .field("key", &"<redacted>")
            .field("nonce", &self.nonce.load(Ordering::Relaxed))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl GuardianClient {
    /// Open a new client. Reads the key immediately (so a missing or
    /// bad-mode key is loud at startup, not at first write).
    ///
    /// Nonce seed combines two sources:
    /// - high 32 bits: nanosecond timestamp (monotonic-ish across reboots)
    /// - low 32 bits: random from `/dev/urandom`
    ///
    /// This makes the seed space 2^64 even if the clock jumps backward
    /// (NTP step, VM snapshot restore), so a fresh harness won't land on a
    /// sequence the guardian has already seen and trigger permanent
    /// `ReplayDetected`. /review security + adversarial flagged plain
    /// `(nanos as u64)` as a lockout risk under clock regressions.
    pub fn new(socket_path: &Path, key_path: &Path) -> Result<Self> {
        let key = load_key(key_path)?;
        let seed = Self::seed_nonce();
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            key,
            key_path: key_path.to_path_buf(),
            nonce: AtomicU64::new(seed),
            connect_lock: Mutex::new(()),
            timeout: Duration::from_secs(5),
        })
    }

    /// Re-stat the key file on disk and return Err if the mode drifted
    /// from the startup value (must still be 0400 or 0600) OR if the file
    /// is missing. Intended for periodic invocation from a supervisor task
    /// so silent perms drift doesn't leave the HMAC key world-readable.
    ///
    /// Does NOT reload the key bytes — rotation is a harness-restart
    /// operation by design. /review security flagged that the client
    /// loads the key once at startup and never re-validates; this closes
    /// the visibility gap without introducing runtime key reload.
    pub fn recheck_key_mode(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&self.key_path).with_context(|| {
            format!(
                "guardian key {} missing or unstatable",
                self.key_path.display()
            )
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o400 && mode != 0o600 {
            anyhow::bail!(
                "guardian key {} mode drifted to 0{:o}; expected 0400 or 0600. \
                 Reset with `chmod 0400 {}`.",
                self.key_path.display(),
                mode,
                self.key_path.display()
            );
        }
        Ok(())
    }

    fn seed_nonce() -> u64 {
        // Time bits: nanos since epoch, high 32 of the 128 — gives us
        // monotonic-ish behavior at second-scale.
        let nanos: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let time_hi: u64 = ((nanos >> 32) & 0xFFFF_FFFF) as u64;

        // Random low bits from /dev/urandom. Best-effort; if it fails we
        // fall back to nanos low bits (still unpredictable to an external
        // attacker because they don't know our boot time to the ns).
        let mut rand_bytes = [0u8; 4];
        let rand_lo: u64 = match std::fs::File::open("/dev/urandom") {
            Ok(mut f) => {
                use std::io::Read;
                if f.read_exact(&mut rand_bytes).is_ok() {
                    u32::from_le_bytes(rand_bytes) as u64
                } else {
                    (nanos as u64) & 0xFFFF_FFFF
                }
            }
            Err(_) => (nanos as u64) & 0xFFFF_FFFF,
        };

        (time_hi << 32) | rand_lo
    }

    /// Write `content` to `path` via the guardian. `reason` is audit-logged.
    pub fn protected_write(&self, path: &str, content: &[u8], reason: &str) -> Result<WriteResult> {
        use base64::Engine;

        // Nonce requires only per-request uniqueness; no happens-before ordering
        // against any other atomic. Relaxed is correct and avoids the ARM full-
        // barrier that SeqCst would impose (/review performance specialist).
        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed);
        let hmac = compute_hmac(&self.key, "write", path, content, nonce);
        let req = Req {
            op: "write".to_string(),
            path: path.to_string(),
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(content),
            nonce,
            hmac,
            reason: Some(reason.to_string()),
            proto_version: CLIENT_PROTO_VERSION,
        };
        let resp = self.rpc(&req)?;
        Ok(map_resp(resp))
    }

    /// Cheap health check. Returns `true` if the guardian answers `ok`.
    pub fn ping(&self) -> Result<bool> {
        // Nonce requires only per-request uniqueness; no happens-before ordering
        // against any other atomic. Relaxed is correct and avoids the ARM full-
        // barrier that SeqCst would impose (/review performance specialist).
        let nonce = self.nonce.fetch_add(1, Ordering::Relaxed);
        let hmac = compute_hmac(&self.key, "ping", "", &[], nonce);
        let req = Req {
            op: "ping".to_string(),
            path: String::new(),
            bytes_b64: String::new(),
            nonce,
            hmac,
            reason: Some("ping".to_string()),
            proto_version: CLIENT_PROTO_VERSION,
        };
        let resp = self.rpc(&req)?;
        Ok(resp.ok)
    }

    fn rpc(&self, req: &Req) -> Result<Resp> {
        let _g = self
            .connect_lock
            .lock()
            .map_err(|e| anyhow::anyhow!("guardian client mutex poisoned: {}", e))?;
        let mut stream = UnixStream::connect(&self.socket_path)
            .with_context(|| format!("connect {}", self.socket_path.display()))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .context("set read timeout")?;
        stream
            .set_write_timeout(Some(self.timeout))
            .context("set write timeout")?;

        let line = serde_json::to_string(req).context("serialize req")? + "\n";
        stream.write_all(line.as_bytes()).context("write req")?;
        stream.flush().context("flush req")?;

        let mut resp_line = String::new();
        BufReader::new(&stream)
            .read_line(&mut resp_line)
            .context("read resp")?;
        let resp: Resp = serde_json::from_str(resp_line.trim_end()).context("parse resp")?;
        Ok(resp)
    }
}

/// Wire types — MUST match bootstrap-guardian/src/proto.rs exactly.
/// They are duplicated (rather than shared via a crate) because the
/// harness is a teloxide/tokio-flavored Rust crate and the guardian is
/// a tiny self-contained binary, and cross-dep-sharing adds more pain
/// than the ~50 lines of duplication saves.
/// Current wire-format version the client advertises. Must match
/// `bootstrap_guardian::proto::PROTO_VERSION`. Bumping requires a
/// coordinated edit of both crates' constants plus the HMAC fixture tests.
const CLIENT_PROTO_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Req {
    op: String,
    path: String,
    bytes_b64: String,
    nonce: u64,
    hmac: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// Wire-format version. Guardian rejects requests NEWER than it
    /// understands with `Malformed`. Always set; older guardians that
    /// don't know the field simply ignore it via `#[serde(default)]`.
    proto_version: u32,
}

#[derive(Debug, Deserialize)]
struct Resp {
    ok: bool,
    #[serde(default)]
    written_bytes: Option<u64>,
    /// Typed err_code. `#[serde(other)]` on `ClientErrCode::Unknown` means a
    /// newer guardian returning an unrecognized variant still deserializes.
    #[serde(default)]
    err_code: Option<ClientErrCode>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    suggested_action: Option<String>,
    #[serde(default)]
    alternative_roots: Option<Vec<String>>,
    #[serde(default)]
    #[allow(dead_code)] // reserved for future version-gated client behavior
    proto_version: Option<u32>,
}

fn load_key(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o400 && mode != 0o600 {
        anyhow::bail!(
            "guardian key at {} has mode 0{:o}; must be 0400 (or 0600). \
             chmod 0400 {}",
            path.display(),
            mode,
            path.display()
        );
    }
    let key = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if key.len() < 32 {
        anyhow::bail!(
            "guardian key at {} is only {} bytes; need at least 32",
            path.display(),
            key.len()
        );
    }
    Ok(key)
}

fn compute_hmac(key: &[u8], op: &str, path: &str, bytes: &[u8], nonce: u64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(op.as_bytes());
    mac.update(b"|");
    mac.update(path.as_bytes());
    mac.update(b"|");
    let mut h = Sha256::new();
    h.update(bytes);
    mac.update(&h.finalize());
    mac.update(b"|");
    mac.update(&nonce.to_le_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn map_resp(resp: Resp) -> WriteResult {
    if resp.ok {
        return WriteResult::Ok {
            written_bytes: resp.written_bytes.unwrap_or(0),
        };
    }
    let code = resp.err_code.unwrap_or(ClientErrCode::Unknown);
    match code {
        ClientErrCode::Denied => WriteResult::Denied {
            reason: resp.message.unwrap_or_else(|| "denied".into()),
            alternatives: resp.alternative_roots.unwrap_or_default(),
        },
        other => WriteResult::Err {
            code: other,
            message: resp.message.unwrap_or_else(|| "<no message>".into()),
            suggested_action: resp.suggested_action,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_stable_across_calls() {
        let key = b"k".repeat(32);
        let a = compute_hmac(&key, "write", "/x/y", b"hello", 42);
        let b = compute_hmac(&key, "write", "/x/y", b"hello", 42);
        assert_eq!(a, b);
    }

    /// **Cross-crate wire-compat pin.** Paired with
    /// `bootstrap-guardian/src/auth.rs::tests::hmac_wire_fixture_write_op`.
    /// Both tests assert the same hex over the same fixture inputs. If the
    /// two implementations drift (tag string, separator, hash order, nonce
    /// endianness), at least one side's test fails loudly at CI time — long
    /// before any runtime BadHmac would surface.
    ///
    /// The prior version of this test only checked `got.len() == 64`, which
    /// would have let a whole class of protocol drift through (/review
    /// testing + maintainability + api-contract specialists all flagged it).
    #[test]
    fn hmac_wire_fixture_write_op() {
        let key: Vec<u8> = (0..64).collect();
        let got = compute_hmac(&key, "write", "/a", b"x", 1);
        assert_eq!(
            got, "c28f43f14294ab137e3be1662eb17ad95057fc90af682ef6df2fdbf880613892",
            "HMAC wire format drift! This value MUST match \
             bootstrap-guardian/src/auth.rs::hmac_wire_fixture_write_op. \
             If the protocol changed deliberately, update BOTH tests."
        );
    }

    #[test]
    fn hmac_wire_fixture_ping_op() {
        let key: Vec<u8> = (0..64).collect();
        let got = compute_hmac(&key, "ping", "", &[], 1);
        assert_eq!(
            got, "4f4a2d97f99a96ddeffd284dea1e6a5136cf09b328636fdeab76c5965d2d1615",
            "HMAC wire format for Ping drifted. Update both tests."
        );
    }

    #[test]
    fn map_resp_ok() {
        let r = Resp {
            ok: true,
            written_bytes: Some(5),
            err_code: None,
            message: None,
            suggested_action: None,
            alternative_roots: None,
            proto_version: Some(1),
        };
        assert!(matches!(map_resp(r), WriteResult::Ok { written_bytes: 5 }));
    }

    #[test]
    fn map_resp_denied_has_alternatives() {
        let r = Resp {
            ok: false,
            written_bytes: None,
            err_code: Some(ClientErrCode::Denied),
            message: Some("protected".to_string()),
            suggested_action: None,
            alternative_roots: Some(vec!["/opt/nova/data".to_string()]),
            proto_version: Some(1),
        };
        match map_resp(r) {
            WriteResult::Denied { alternatives, .. } => {
                assert_eq!(alternatives, vec!["/opt/nova/data".to_string()])
            }
            other => panic!("expected Denied, got {:?}", other),
        }
    }

    /// Guardian returned a typed err we recognize → promoted to WriteResult::Err
    /// with the correct ClientErrCode.
    #[test]
    fn map_resp_typed_err_paused() {
        let r = Resp {
            ok: false,
            written_bytes: None,
            err_code: Some(ClientErrCode::Paused),
            message: Some("guardian admin-paused".to_string()),
            suggested_action: Some("run guardianctl resume".to_string()),
            alternative_roots: None,
            proto_version: Some(1),
        };
        match map_resp(r) {
            WriteResult::Err { code, .. } => assert_eq!(code, ClientErrCode::Paused),
            other => panic!("expected Err(Paused), got {:?}", other),
        }
    }

    /// A newer guardian returns an err_code this client doesn't know about.
    /// serde's `#[serde(other)]` routes it to ClientErrCode::Unknown so
    /// deserialization doesn't fail; map_resp still surfaces a useful shape.
    #[test]
    fn unknown_err_code_deserializes_to_unknown_not_panic() {
        let json = r#"{"ok":false,"err_code":"rate_limited","message":"too fast"}"#;
        let resp: Resp = serde_json::from_str(json).unwrap();
        match map_resp(resp) {
            WriteResult::Err { code, .. } => assert_eq!(code, ClientErrCode::Unknown),
            other => panic!("expected Err(Unknown), got {:?}", other),
        }
    }
}
