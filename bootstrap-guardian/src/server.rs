//! UDS server loop — accepts connections, verifies caller UID, checks HMAC,
//! consumes nonce, decides path verdict, writes file, returns response.
//!
//! Runs one blocking task per connection via `tokio::task::spawn_blocking`.
//! We use `std::os::unix::net` (blocking sockets) rather than tokio's
//! `UnixListener` because SO_PEERCRED / `getpeereid` are simpler on raw fds
//! and these requests are low-rate.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
        // If the file exists and is NOT a socket, we refuse — someone replaced it.
        if sock.exists() {
            let meta = std::fs::metadata(&sock).context("stat existing socket")?;
            let file_type = meta.file_type();
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if !file_type.is_socket() {
                    anyhow::bail!(
                        "refusing to bind: {} exists and is not a socket (type: {:?}) — \
                         refusing to remove it. Investigate before starting.",
                        sock.display(),
                        file_type
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
            "guardian listening"
        );

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let me = Arc::clone(&self);
                    std::thread::spawn(move || {
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

        let mut reader = BufReader::new(stream.try_clone()?);
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

        // Nonce — replay protection.
        match self.nonces.consume(uid, req.nonce) {
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

        match req.op {
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
        }
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

/// Open + write a file with O_NOFOLLOW on the target filename. If the target
/// is a symlink, open fails with ELOOP — symlink swap TOCTOU defeated.
fn write_with_nofollow(path: &Path, bytes: &[u8]) -> std::io::Result<u64> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
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
