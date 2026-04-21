//! UDS server loop — accepts connections, verifies caller UID, checks HMAC,
//! consumes nonce, decides path verdict, writes file, returns response.
//!
//! Runs one blocking task per connection via `tokio::task::spawn_blocking`.
//! We use `std::os::unix::net` (blocking sockets) rather than tokio's
//! `UnixListener` because SO_PEERCRED / `getpeereid` are simpler on raw fds
//! and these requests are low-rate.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Maximum bytes the guardian will read from a single request before giving
/// up with `Malformed`. Defends against a local client that opens a socket
/// and streams unbounded bytes with no newline. 16 MiB is a balance: large
/// enough for any realistic base64-encoded payload (a ~11 MB raw write
/// after base64 overhead), small enough that a malicious stream cannot OOM
/// the guardian before the read timeout fires.
const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum simultaneous connection handlers. Previously the accept loop
/// did `std::thread::spawn` per connection with no cap — a local accept
/// flood could exhaust ulimit -u and crash the guardian (plus any other
/// process sharing the rlimit). /review adversarial flagged this.
///
/// 256 is comfortably above realistic harness concurrency (3 bots × dual-
/// lane × ~4 in-flight writes ≈ 24) and small enough that hitting the cap
/// is a clear signal something is wrong.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// Active connection counter guard. Increments on scope entry, decrements
/// on drop. Used to cap accept-time concurrency without threading a
/// semaphore through the handler.
struct ConnCountGuard {
    counter: Arc<AtomicUsize>,
}

impl ConnCountGuard {
    fn try_acquire(counter: &Arc<AtomicUsize>) -> Option<Self> {
        let prev = counter.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_CONCURRENT_CONNECTIONS {
            counter.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self {
            counter: Arc::clone(counter),
        })
    }
}

impl Drop for ConnCountGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

use crate::audit::{AuditEvent, AuditLog};
use crate::auth::{compute_hmac, constant_time_eq, load_key, peer_uid};
use crate::config::GuardianConfig;
use crate::nonce::NonceStore;
use crate::paths::{PathGuard, Verdict};
use crate::proto::{ErrCode, Op, Req, Resp};

pub struct Guardian {
    pub cfg: GuardianConfig,
    pub key: Vec<u8>,
    pub nonces: NonceStore,
    pub paths: PathGuard,
    pub audit: AuditLog,
}

impl Guardian {
    pub fn new(cfg: GuardianConfig, key: Vec<u8>) -> Result<Self> {
        let nonces = NonceStore::open(&cfg.nonce_db_path())?;
        let paths = PathGuard::new(cfg.allowed_roots.clone(), cfg.protected_paths.clone())?;
        let audit = AuditLog::new(cfg.audit_log_path());
        Ok(Self {
            cfg,
            key,
            nonces,
            paths,
            audit,
        })
    }

    pub fn bind(&self) -> Result<UnixListener> {
        let sock = self.cfg.socket_path();
        // Remove stale socket from previous run.
        // If the file exists and is NOT a socket OR is owned by a different
        // user, we refuse. Previously we trusted any socket inode at the
        // configured path; a local user running as any UID could create a
        // socket there while the guardian is down, and on restart the
        // guardian would `unlink` it blindly. /review security flagged.
        if sock.exists() {
            let meta = std::fs::metadata(&sock).context("stat existing socket")?;
            let file_type = meta.file_type();
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                use std::os::unix::fs::MetadataExt;
                if !file_type.is_socket() {
                    anyhow::bail!(
                        "refusing to bind: {} exists and is not a socket (type: {:?}) — \
                         refusing to remove it. Investigate before starting.",
                        sock.display(),
                        file_type
                    );
                }
                let our_uid = unsafe { libc::geteuid() };
                if meta.uid() != our_uid {
                    anyhow::bail!(
                        "refusing to bind: stale socket at {} is owned by uid {} \
                         but this process is uid {} — a different user placed a \
                         socket at our expected path. Investigate before starting.",
                        sock.display(),
                        meta.uid(),
                        our_uid
                    );
                }
            }
            std::fs::remove_file(&sock).with_context(|| format!("rm stale {}", sock.display()))?;
        }
        let listener =
            UnixListener::bind(&sock).with_context(|| format!("bind UDS {}", sock.display()))?;

        // Lock socket file to 0600 — only guardian-uid can connect.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&sock)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&sock, perms)?;
        }
        Ok(listener)
    }

    pub fn run(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        tracing::info!(
            socket = %self.cfg.socket_path().display(),
            allowed_uids = ?self.cfg.allowed_uids,
            allowed_roots = ?self.cfg.allowed_roots,
            protected = ?self.cfg.protected_paths,
            max_concurrent = MAX_CONCURRENT_CONNECTIONS,
            "guardian listening"
        );

        let conn_counter: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    // Bounded concurrency: refuse the connection if
                    // MAX_CONCURRENT_CONNECTIONS handlers are already
                    // running. Previously unbounded — a local accept flood
                    // could exhaust ulimit -u.
                    let Some(guard) = ConnCountGuard::try_acquire(&conn_counter) else {
                        tracing::warn!(
                            max = MAX_CONCURRENT_CONNECTIONS,
                            "guardian at connection cap — dropping new client"
                        );
                        drop(stream);
                        continue;
                    };
                    let me = Arc::clone(&self);
                    std::thread::spawn(move || {
                        let _g = guard; // released when this thread exits
                        if let Err(e) = me.handle_connection(stream) {
                            tracing::warn!(err = %e, "guardian connection handler failed");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(err = %e, "accept failed; continuing");
                }
            }
        }
        Ok(())
    }

    fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let timeout = Duration::from_secs(self.cfg.request_timeout_secs);
        stream
            .set_read_timeout(Some(timeout))
            .context("set read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("set write timeout")?;

        let uid = match peer_uid(&stream) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(err = %e, "peer UID read failed");
                return Ok(());
            }
        };

        // Size-cap the request to defend against a client that opens a socket
        // and writes an unbounded stream with no newline — /review adversarial
        // flagged this as an OOM-style DoS that outlives the read timeout.
        // 16 MiB is comfortably larger than any realistic base64-encoded write
        // payload we expect (typical source files < 1 MiB, memory blobs < 10 MiB).
        let mut reader = BufReader::new(stream.try_clone()?.take(MAX_REQUEST_BYTES));
        let mut writer = stream;

        let mut line = String::new();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(err = %e, uid, "read_line failed");
                return Ok(());
            }
        };
        if n == 0 {
            return Ok(());
        }
        // If we filled the buffer without seeing a newline, treat as malformed
        // and bail (Take caps at MAX_REQUEST_BYTES so read_line returns early).
        if n >= MAX_REQUEST_BYTES as usize && !line.ends_with('\n') {
            tracing::warn!(uid, n, "request exceeded size cap without newline");
            let resp = Resp::err(
                ErrCode::Malformed,
                format!(
                    "request exceeded {} byte cap without a terminating newline",
                    MAX_REQUEST_BYTES
                ),
            );
            let resp_line = serde_json::to_string(&resp)?;
            writer.write_all(resp_line.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            return Ok(());
        }

        let resp = self.process_line(uid, line.trim_end());
        let resp_line = serde_json::to_string(&resp)?;
        writer.write_all(resp_line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    fn process_line(&self, uid: u32, line: &str) -> Resp {
        // Paused mode — admin break-glass via `guardianctl pause`.
        if self.cfg.pause_flag_path().exists() {
            self.audit.write(&AuditEvent {
                ts: AuditLog::now(),
                uid,
                op: "?",
                path: line.chars().take(200).collect(),
                decision: "paused",
                bytes: None,
                reason: None,
                err: Some("paused"),
            });
            return Resp::err(ErrCode::Paused, "guardian is admin-paused");
        }

        // UID check — before any parse attempt. No need to reveal error shape
        // to unauthorized callers.
        if !self.cfg.allowed_uids.contains(&uid) {
            self.audit.write(&AuditEvent {
                ts: AuditLog::now(),
                uid,
                op: "?",
                path: String::new(),
                decision: "uid_mismatch",
                bytes: None,
                reason: None,
                err: Some("uid_mismatch"),
            });
            return Resp::err(
                ErrCode::UidMismatch,
                format!("uid {} is not registered with this guardian", uid),
            );
        }

        let req: Req = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "?",
                    path: String::new(),
                    decision: "malformed",
                    bytes: None,
                    reason: None,
                    err: Some("malformed"),
                });
                return Resp::err(ErrCode::Malformed, format!("json: {}", e));
            }
        };

        // Wire-version gate: refuse clients NEWER than what we know how to
        // interpret. Older or absent versions are accepted permissively so
        // a guardian upgrade doesn't break outdated harnesses.
        if let Some(client_v) = req.proto_version
            && client_v > crate::proto::PROTO_VERSION
        {
            self.audit.write(&AuditEvent {
                ts: AuditLog::now(),
                uid,
                op: op_tag(req.op),
                path: req.path.clone(),
                decision: "malformed",
                bytes: None,
                reason: req.reason.as_deref(),
                err: Some("proto_version_newer"),
            });
            return Resp::err(
                ErrCode::Malformed,
                format!(
                    "client proto_version={} but guardian supports up to {}",
                    client_v,
                    crate::proto::PROTO_VERSION
                ),
            );
        }

        let bytes = match base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &req.bytes_b64,
        ) {
            Ok(b) => b,
            Err(e) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: op_tag(req.op),
                    path: req.path.clone(),
                    decision: "malformed",
                    bytes: None,
                    reason: req.reason.as_deref(),
                    err: Some("base64"),
                });
                return Resp::err(ErrCode::Malformed, format!("bytes_b64: {}", e));
            }
        };

        // Pick the signing key. `OverrideWrite` is signed with a SEPARATE
        // break-glass key (`override.key`) not known to the harness, so a
        // compromised harness cannot mint an override request. Every other op
        // verifies against the shared guardian key.
        let key_for_verify: std::borrow::Cow<'_, [u8]> = match req.op {
            Op::OverrideWrite => {
                let Some(ref override_path) = self.cfg.override_key_path else {
                    self.audit.write(&AuditEvent {
                        ts: AuditLog::now(),
                        uid,
                        op: op_tag(req.op),
                        path: req.path.clone(),
                        decision: "override_disabled",
                        bytes: Some(bytes.len() as u64),
                        reason: req.reason.as_deref(),
                        err: Some("override_disabled"),
                    });
                    return Resp::err(ErrCode::OverrideDisabled, "override disabled");
                };
                match load_key(override_path) {
                    Ok(k) => std::borrow::Cow::Owned(k),
                    Err(e) => {
                        self.audit.write(&AuditEvent {
                            ts: AuditLog::now(),
                            uid,
                            op: op_tag(req.op),
                            path: req.path.clone(),
                            decision: "override_disabled",
                            bytes: Some(bytes.len() as u64),
                            reason: req.reason.as_deref(),
                            err: Some("override_key_load"),
                        });
                        return Resp::err(
                            ErrCode::OverrideDisabled,
                            format!("override disabled: {}", e),
                        );
                    }
                }
            }
            Op::Write | Op::Ping => std::borrow::Cow::Borrowed(self.key.as_slice()),
        };

        // HMAC check — constant-time, over canonical triple.
        let expected = compute_hmac(&key_for_verify, req.op, &req.path, &bytes, req.nonce);
        if !constant_time_eq(&expected, &req.hmac) {
            self.audit.write(&AuditEvent {
                ts: AuditLog::now(),
                uid,
                op: op_tag(req.op),
                path: req.path.clone(),
                decision: "bad_hmac",
                bytes: Some(bytes.len() as u64),
                reason: req.reason.as_deref(),
                err: Some("bad_hmac"),
            });
            return Resp::err(ErrCode::BadHmac, "HMAC mismatch");
        }

        // Nonce — replay protection. Two-phase pattern:
        //   1. `would_accept` (non-mutating) validates nonce-is-fresh BEFORE
        //      we attempt the op.
        //   2. After the op succeeds, we call `consume` to commit.
        //
        // Previously the nonce was consumed here, before the write. A
        // transient fs error (ENOSPC, EROFS, quota) would burn the nonce,
        // forcing the client to skip a value. /review adversarial flagged
        // this as a correctness hazard. The two-phase pattern makes failed
        // writes idempotent from the nonce's perspective.
        match self.nonces.would_accept(uid, req.nonce) {
            Ok(true) => {}
            Ok(false) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: op_tag(req.op),
                    path: req.path.clone(),
                    decision: "replay",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("replay"),
                });
                return Resp::err(ErrCode::ReplayDetected, "nonce already consumed");
            }
            Err(e) => {
                tracing::error!(err = %e, "nonce store failed");
                return Resp::err(ErrCode::IoError, format!("nonce store: {}", e));
            }
        }

        let resp = match req.op {
            Op::Ping => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "ping",
                    path: req.path.clone(),
                    decision: "allow",
                    bytes: None,
                    reason: req.reason.as_deref(),
                    err: None,
                });
                Resp::ok_pong()
            }
            Op::Write => self.do_write(uid, &req, &bytes),
            Op::OverrideWrite => self.do_override_write(uid, &req, &bytes),
        };

        // Commit the nonce only if the op succeeded. On failure the client
        // can retry the SAME nonce — two-phase pattern, per /review adversarial.
        if resp.ok
            && let Err(e) = self.nonces.consume(uid, req.nonce)
        {
            // Op succeeded on disk but we cannot commit the nonce. Log
            // loudly; next request with the same nonce will hit this
            // would_accept branch as true again, so there's a tiny
            // replay window until we retry the commit. Acceptable given
            // the typical NonceStore failure is transient disk pressure
            // that also blocks future writes.
            tracing::error!(err = %e, uid, nonce = req.nonce, "nonce commit failed after successful op");
        }
        resp
    }

    fn do_write(&self, uid: u32, req: &Req, bytes: &[u8]) -> Resp {
        let target = Path::new(&req.path);
        let (verdict, canonical) = match self.paths.decide(target) {
            Ok(pair) => pair,
            Err(e) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "write",
                    path: req.path.clone(),
                    decision: "traversal",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("traversal"),
                });
                return Resp::err(ErrCode::PathTraversal, e.to_string());
            }
        };

        let alt_roots: Vec<String> = self
            .cfg
            .allowed_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        match verdict {
            Verdict::Allow => {}
            Verdict::DenyProtected => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "write",
                    path: canonical.display().to_string(),
                    decision: "denied_protected",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("denied"),
                });
                return Resp::denied(
                    format!("path {} is in a protected root", canonical.display()),
                    alt_roots,
                );
            }
            Verdict::DenyOutsideAllowed => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "write",
                    path: canonical.display().to_string(),
                    decision: "denied_outside",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("denied"),
                });
                return Resp::denied(
                    format!("path {} is outside every allowed root", canonical.display()),
                    alt_roots,
                );
            }
            Verdict::DenyTraversal => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "write",
                    path: canonical.display().to_string(),
                    decision: "denied_traversal",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("traversal"),
                });
                return Resp::err(
                    ErrCode::PathTraversal,
                    format!("path contains traversal: {}", canonical.display()),
                );
            }
        }

        if let Some(parent) = canonical.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.audit.write(&AuditEvent {
                ts: AuditLog::now(),
                uid,
                op: "write",
                path: canonical.display().to_string(),
                decision: "io_error",
                bytes: Some(bytes.len() as u64),
                reason: req.reason.as_deref(),
                err: Some("mkdir"),
            });
            return Resp::err(
                ErrCode::IoError,
                format!("mkdir {}: {}", parent.display(), e),
            );
        }

        // Write with O_NOFOLLOW to defeat any last-moment symlink swap.
        match write_with_nofollow(&canonical, bytes) {
            Ok(n) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "write",
                    path: canonical.display().to_string(),
                    decision: "allow",
                    bytes: Some(n),
                    reason: req.reason.as_deref(),
                    err: None,
                });
                Resp::ok_written(n)
            }
            Err(e) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "write",
                    path: canonical.display().to_string(),
                    decision: "io_error",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("write"),
                });
                Resp::err(
                    ErrCode::IoError,
                    format!("write {}: {}", canonical.display(), e),
                )
            }
        }
    }

    /// Owner-only break-glass write. Same flow as `do_write` but bypasses
    /// `Verdict::DenyProtected`. Still enforces `DenyOutsideAllowed` and
    /// `DenyTraversal`; still uses `O_NOFOLLOW` so a symlink swap between
    /// canonicalize and open won't let the write escape into an unrelated
    /// file. Audit decision is `override_allow` so the log makes the intent
    /// obvious to a post-mortem reviewer.
    fn do_override_write(&self, uid: u32, req: &Req, bytes: &[u8]) -> Resp {
        let target = Path::new(&req.path);
        let (verdict, canonical) = match self.paths.decide(target) {
            Ok(pair) => pair,
            Err(e) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "override_write",
                    path: req.path.clone(),
                    decision: "traversal",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("traversal"),
                });
                return Resp::err(ErrCode::PathTraversal, e.to_string());
            }
        };

        let alt_roots: Vec<String> = self
            .cfg
            .allowed_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        match verdict {
            // Override is the ONE path that bypasses DenyProtected.
            Verdict::Allow | Verdict::DenyProtected => {}
            Verdict::DenyOutsideAllowed => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "override_write",
                    path: canonical.display().to_string(),
                    decision: "denied_outside",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("denied"),
                });
                return Resp::denied(
                    format!(
                        "path {} is outside every allowed root (override cannot \
                         escape the allowlist)",
                        canonical.display()
                    ),
                    alt_roots,
                );
            }
            Verdict::DenyTraversal => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "override_write",
                    path: canonical.display().to_string(),
                    decision: "denied_traversal",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("traversal"),
                });
                return Resp::err(
                    ErrCode::PathTraversal,
                    format!("path contains traversal: {}", canonical.display()),
                );
            }
        }

        if let Some(parent) = canonical.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.audit.write(&AuditEvent {
                ts: AuditLog::now(),
                uid,
                op: "override_write",
                path: canonical.display().to_string(),
                decision: "io_error",
                bytes: Some(bytes.len() as u64),
                reason: req.reason.as_deref(),
                err: Some("mkdir"),
            });
            return Resp::err(
                ErrCode::IoError,
                format!("mkdir {}: {}", parent.display(), e),
            );
        }

        match write_with_nofollow(&canonical, bytes) {
            Ok(n) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "override_write",
                    path: canonical.display().to_string(),
                    decision: "override_allow",
                    bytes: Some(n),
                    reason: req.reason.as_deref(),
                    err: None,
                });
                Resp::ok_written(n)
            }
            Err(e) => {
                self.audit.write(&AuditEvent {
                    ts: AuditLog::now(),
                    uid,
                    op: "override_write",
                    path: canonical.display().to_string(),
                    decision: "io_error",
                    bytes: Some(bytes.len() as u64),
                    reason: req.reason.as_deref(),
                    err: Some("write"),
                });
                Resp::err(
                    ErrCode::IoError,
                    format!("write {}: {}", canonical.display(), e),
                )
            }
        }
    }
}

fn op_tag(op: Op) -> &'static str {
    match op {
        Op::Write => "write",
        Op::Ping => "ping",
        Op::OverrideWrite => "override_write",
    }
}

/// Atomic write with O_NOFOLLOW + temp-file-then-rename.
///
/// Write goes to `<path>.tmp.<pid>.<nanos>` with `O_EXCL|O_NOFOLLOW`, gets
/// `sync_all`'d, then atomically `rename`d over the target. This fixes two
/// issues /review adversarial flagged:
///
/// - **Crash mid-write = corrupt file.** The previous
///   `create+truncate+write` opened the target, zeroed it, then streamed
///   bytes. A power loss or OOM between truncate and the final byte left
///   the target file truncated or partial. Nova-serialized state
///   (memories, session_id) would fail to deserialize on next boot. The
///   temp-then-rename pattern means the target is either the old bytes OR
///   the new bytes — never a mix.
/// - **Concurrent writes interleave.** Two simultaneous `protected_write`
///   calls to the same path with the previous code would race on the same
///   fd and interleave bytes. With this pattern, each call writes to its
///   own uniquely-named temp file, and only the final `rename` is racy —
///   but `rename` is atomic at the directory-entry level on POSIX, so one
///   wins, the other loses (last rename wins), and neither file is
///   interleaved.
///
/// `O_NOFOLLOW` on the temp path defends against an attacker swapping in a
/// symlink at the temp name between our `openat` and the exclusive
/// creation. Note: `O_NOFOLLOW` only checks the final path component; the
/// TOCTOU surface on intermediate directories remains (tracked in
/// TODOS.md as a Linux-only `openat2(RESOLVE_NO_SYMLINKS|RESOLVE_BENEATH)`
/// hardening follow-up).
fn write_with_nofollow(path: &Path, bytes: &[u8]) -> std::io::Result<u64> {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    let file_name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;

    // Per-call unique temp name (pid + nanos), inside the same parent so
    // the final rename is an atomic intra-directory operation.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(".{}.tmp.{}.{}", file_name, std::process::id(), nanos);
    let tmp_path = parent.join(&tmp_name);

    // Create EXCL|NOFOLLOW — fails if an attacker pre-created the name or
    // swapped a symlink in. Mode 0600 so only the guardian UID can read
    // partial state if a crash leaves the temp file behind.
    let mut file = std::fs::OpenOptions::new()
        .read(false)
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    // Atomic rename over the target. Cleanup temp on failure.
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // fsync the parent directory so the rename is durable on crash (ext4
    // default; belt-and-suspenders on other filesystems). Best-effort —
    // opening a directory for fsync isn't portable to every filesystem.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(bytes.len() as u64)
}

#[allow(dead_code)]
fn socket_path_owner_check(sock: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(sock)?;
    let expected = unsafe { libc::geteuid() };
    if meta.uid() != expected {
        anyhow::bail!(
            "socket {} is owned by uid {} but guardian runs as {} — refuse to bind",
            sock.display(),
            meta.uid(),
            expected
        );
    }
    // silence unused PathBuf import path
    let _: PathBuf = sock.to_path_buf();
    Ok(())
}
