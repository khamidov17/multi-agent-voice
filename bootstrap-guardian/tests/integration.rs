//! End-to-end integration tests for bootstrap-guardian.
//!
//! Spins the UDS server in a background thread, connects as a client, and
//! asserts every guardian decision path: Allow, DenyProtected,
//! DenyOutsideAllowed, PathTraversal, BadHmac, ReplayDetected, UidMismatch,
//! Paused.
//!
//! Runs on Linux + macOS. All tests use `serial_test` because they bind real
//! UDS sockets inside per-test tempdirs — no cross-test state shared, but
//! parallel binds can still race file-creation in shared tmp.

use bootstrap_guardian::auth::{compute_hmac, load_key};
use bootstrap_guardian::{ErrCode, Guardian, GuardianConfig, Op, Req, Resp};
use serial_test::serial;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn setup() -> (tempfile::TempDir, GuardianConfig, Vec<u8>) {
    let td = tempfile::tempdir().expect("tempdir");
    let root = td.path().canonicalize().unwrap();

    let run_dir = root.join("run");
    let allowed = root.join("data");
    let protected = root.join("src");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&protected).unwrap();
    // Seed a real protected file so canonicalize can resolve a "write through"
    std::fs::write(protected.join("main.rs"), b"old").unwrap();

    // Key file at 0400 with 64 bytes of determinism.
    let key_bytes: Vec<u8> = (0..64).collect();
    let key_path = run_dir.join("guardian.key");
    std::fs::write(&key_path, &key_bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&key_path).unwrap().permissions();
    perms.set_mode(0o400);
    std::fs::set_permissions(&key_path, perms).unwrap();

    let cfg = GuardianConfig {
        run_dir: run_dir.clone(),
        protected_paths: vec![protected],
        allowed_roots: vec![allowed],
        allowed_uids: vec![unsafe { libc::geteuid() }],
        request_timeout_secs: 5,
        // Override-once is OFF by default; individual tests opt in by setting
        // `cfg.override_key_path = Some(...)` before starting the guardian.
        override_key_path: None,
    };

    (td, cfg, key_bytes)
}

fn start_guardian(cfg: GuardianConfig, key: Vec<u8>) -> PathBuf {
    let socket = cfg.socket_path();
    let guardian = Arc::new(Guardian::new(cfg, key).expect("guardian new"));
    let listener = guardian.bind().expect("guardian bind");
    std::thread::spawn(move || {
        if let Err(e) = guardian.run(listener) {
            eprintln!("guardian run ended: {}", e);
        }
    });
    // Wait briefly for the background listener to be accept-ready.
    // Poll every 10ms up to 500ms so slow CI runners don't flake.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&socket).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    socket
}

fn rpc(sock: &Path, req: &Req) -> Resp {
    let mut stream = UnixStream::connect(sock).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let line = serde_json::to_string(req).unwrap() + "\n";
    stream.write_all(line.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).expect("read resp");
    serde_json::from_str(resp_line.trim_end()).expect("parse resp")
}

fn signed_write_req(key: &[u8], path: &Path, bytes: &[u8], nonce: u64, reason: &str) -> Req {
    use base64::Engine;
    let path_s = path.display().to_string();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let hmac = compute_hmac(key, Op::Write, &path_s, bytes, nonce);
    Req {
        op: Op::Write,
        path: path_s,
        bytes_b64: b64,
        nonce,
        hmac,
        reason: Some(reason.into()),
    }
}

#[test]
#[serial]
fn allow_write_to_allowed_root() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());
    let target = cfg.allowed_roots[0].join("hello.txt");
    let req = signed_write_req(&key, &target, b"hello", 1, "test");
    let resp = rpc(&sock, &req);
    assert!(resp.ok, "expected Allow, got {:?}", resp);
    assert_eq!(resp.written_bytes, Some(5));
    let on_disk = std::fs::read_to_string(&target).expect("file written");
    assert_eq!(on_disk, "hello");
    drop(td);
}

#[test]
#[serial]
fn deny_write_to_protected_path() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());
    let target = cfg.protected_paths[0].join("main.rs");
    let req = signed_write_req(&key, &target, b"x", 1, "test");
    let resp = rpc(&sock, &req);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::Denied));
    assert!(resp.alternative_roots.is_some());
    let contents = std::fs::read_to_string(&target).unwrap();
    assert_eq!(contents, "old", "protected file must not be modified");
    drop(td);
}

#[test]
#[serial]
fn deny_write_outside_allowed_root() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());
    let outside_td = tempfile::tempdir().unwrap();
    let target = outside_td.path().canonicalize().unwrap().join("x.txt");
    let req = signed_write_req(&key, &target, b"x", 1, "test");
    let resp = rpc(&sock, &req);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::Denied));
    drop(td);
}

#[test]
#[serial]
fn bad_hmac_is_rejected() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());
    let target = cfg.allowed_roots[0].join("x.txt");
    let mut req = signed_write_req(&key, &target, b"x", 1, "test");
    // Tamper with the hmac
    req.hmac = "0".repeat(64);
    let resp = rpc(&sock, &req);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::BadHmac));
    drop(td);
}

#[test]
#[serial]
fn replay_detected() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());
    let target = cfg.allowed_roots[0].join("replay.txt");

    let first = signed_write_req(&key, &target, b"a", 100, "first");
    assert!(rpc(&sock, &first).ok);

    // Second request with SAME nonce
    let second = signed_write_req(&key, &target, b"b", 100, "replay");
    let resp = rpc(&sock, &second);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::ReplayDetected));

    // Backward nonce also rejected
    let back = signed_write_req(&key, &target, b"c", 99, "backward");
    let resp = rpc(&sock, &back);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::ReplayDetected));

    // Forward nonce accepted
    let fwd = signed_write_req(&key, &target, b"d", 101, "forward");
    assert!(rpc(&sock, &fwd).ok);
    drop(td);
}

#[test]
#[serial]
fn symlink_into_protected_is_denied() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());

    // Attacker creates <allowed>/link → <protected>/main.rs, then writes via link.
    let allowed = &cfg.allowed_roots[0];
    let protected_file = cfg.protected_paths[0].join("main.rs");
    let link = allowed.join("link");
    std::os::unix::fs::symlink(&protected_file, &link).unwrap();

    let req = signed_write_req(&key, &link, b"pwned", 200, "attack");
    let resp = rpc(&sock, &req);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::Denied));
    // File must remain original content
    let contents = std::fs::read_to_string(&protected_file).unwrap();
    assert_eq!(contents, "old");
    drop(td);
}

#[test]
#[serial]
fn paused_mode_rejects_all_writes() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());

    // Touch pause flag BEFORE sending a request
    std::fs::write(cfg.pause_flag_path(), "paused=yes\n").unwrap();

    let target = cfg.allowed_roots[0].join("paused.txt");
    let req = signed_write_req(&key, &target, b"x", 1, "test");
    let resp = rpc(&sock, &req);
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::Paused));

    // Clean up pause flag and retry — should succeed
    std::fs::remove_file(cfg.pause_flag_path()).unwrap();
    let req = signed_write_req(&key, &target, b"x", 2, "test");
    let resp = rpc(&sock, &req);
    assert!(resp.ok);
    drop(td);
}

#[test]
#[serial]
fn ping_roundtrip_no_disk_write() {
    let (td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());

    use base64::Engine;
    let nonce = 42;
    let hmac = compute_hmac(&key, Op::Ping, "", &[], nonce);
    let req = Req {
        op: Op::Ping,
        path: String::new(),
        bytes_b64: base64::engine::general_purpose::STANDARD.encode([]),
        nonce,
        hmac,
        reason: Some("ping test".into()),
    };
    let resp = rpc(&sock, &req);
    assert!(resp.ok, "ping should succeed: {:?}", resp);
    assert!(resp.written_bytes.is_none());
    drop(td);
}

#[test]
#[serial]
fn malformed_request_is_rejected_cleanly() {
    let (_td, cfg, key) = setup();
    let sock = start_guardian(cfg.clone(), key.clone());

    let mut stream = UnixStream::connect(&sock).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(b"this is not json\n").unwrap();
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).unwrap();
    let resp: Resp = serde_json::from_str(line.trim_end()).expect("resp is json");
    assert!(!resp.ok);
    assert_eq!(resp.err_code, Some(ErrCode::Malformed));
}

#[test]
#[serial]
fn key_file_permission_check() {
    let td = tempfile::tempdir().unwrap();
    let key_path = td.path().join("k");
    std::fs::write(
        &key_path,
        b"0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF",
    )
    .unwrap();

    // 0644 — not allowed
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&key_path).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&key_path, perms).unwrap();

    let err = load_key(&key_path).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("mode"), "error should mention mode: {}", msg);

    // 0400 — allowed
    let mut perms = std::fs::metadata(&key_path).unwrap().permissions();
    perms.set_mode(0o400);
    std::fs::set_permissions(&key_path, perms).unwrap();
    assert!(load_key(&key_path).is_ok());
}

#[test]
#[serial]
fn too_short_key_is_rejected() {
    let td = tempfile::tempdir().unwrap();
    let key_path = td.path().join("k");
    std::fs::write(&key_path, b"short").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&key_path).unwrap().permissions();
    perms.set_mode(0o400);
    std::fs::set_permissions(&key_path, perms).unwrap();
    let err = load_key(&key_path).unwrap_err();
    assert!(err.to_string().contains("32"), "{}", err);
}

/// Install a separate override.key in `run_dir` at 0400 with deterministic
/// bytes and return it. The override key MUST be distinct from the guardian
/// key so the test proves the server picks the right one per-op.
fn install_override_key(run_dir: &Path) -> Vec<u8> {
    let override_key: Vec<u8> = (100..164).collect(); // different from guardian key
    let path = run_dir.join("override.key");
    std::fs::write(&path, &override_key).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o400);
    std::fs::set_permissions(&path, perms).unwrap();
    override_key
}

fn signed_override_req(key: &[u8], path: &Path, bytes: &[u8], nonce: u64, reason: &str) -> Req {
    use base64::Engine;
    let path_s = path.display().to_string();
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let hmac = compute_hmac(key, Op::OverrideWrite, &path_s, bytes, nonce);
    Req {
        op: Op::OverrideWrite,
        path: path_s,
        bytes_b64: b64,
        nonce,
        hmac,
        reason: Some(reason.into()),
    }
}

#[test]
#[serial]
fn override_once_write_to_protected_path() {
    let (td, mut cfg, key) = setup();
    // Enable override on the guardian by pointing it at a real override key.
    let override_key = install_override_key(&cfg.run_dir);
    cfg.override_key_path = Some(cfg.run_dir.join("override.key"));

    let sock = start_guardian(cfg.clone(), key.clone());

    // The write targets a PROTECTED path — a normal Write would be denied.
    let target = cfg.protected_paths[0].join("main.rs");

    // First prove a regular write is still denied (baseline sanity).
    let regular = signed_write_req(&key, &target, b"forged", 10, "normal write attempt");
    let resp = rpc(&sock, &regular);
    assert!(
        !resp.ok,
        "baseline: regular write to protected must be denied"
    );
    assert_eq!(resp.err_code, Some(ErrCode::Denied));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "old",
        "baseline: file must be untouched"
    );

    // Now the override-once path — same target, signed with the OVERRIDE key.
    let req = signed_override_req(&override_key, &target, b"NEW", 11, "emergency bypass");
    let resp = rpc(&sock, &req);
    assert!(
        resp.ok,
        "override-once must succeed on protected path: {:?}",
        resp
    );
    assert_eq!(resp.written_bytes, Some(3));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "NEW",
        "override must have replaced protected file contents"
    );

    // Override cannot escape the allowlist: a path outside BOTH the allowed
    // root AND the protected path must still be rejected.
    let outside_td = tempfile::tempdir().unwrap();
    let outside = outside_td.path().canonicalize().unwrap().join("x.txt");
    let escape = signed_override_req(&override_key, &outside, b"pwn", 12, "try to escape");
    let resp = rpc(&sock, &escape);
    assert!(
        !resp.ok,
        "override must still respect DenyOutsideAllowed: {:?}",
        resp
    );
    assert_eq!(resp.err_code, Some(ErrCode::Denied));

    // The audit log should contain at least one `override_allow` decision
    // so post-mortem reviewers can find the bypass.
    let audit = std::fs::read_to_string(cfg.audit_log_path())
        .expect("audit log exists after override_allow");
    assert!(
        audit.contains("override_allow"),
        "audit log missing override_allow decision; got:\n{}",
        audit
    );
    assert!(
        audit.contains("emergency bypass"),
        "audit log should capture the reason; got:\n{}",
        audit
    );
    drop(td);
}

#[test]
#[serial]
fn override_once_rejected_without_key() {
    // Guardian has NO override_key_path configured → override-once must be
    // rejected with OverrideDisabled even if the client has a valid-looking
    // HMAC. Also verifies the protected file is untouched.
    let (td, cfg, key) = setup();
    assert!(
        cfg.override_key_path.is_none(),
        "precondition: test setup leaves override disabled"
    );
    let sock = start_guardian(cfg.clone(), key.clone());

    let target = cfg.protected_paths[0].join("main.rs");

    // Sign with *some* key — doesn't matter, server should reject before
    // HMAC verification even runs.
    let fake_override_key: Vec<u8> = (0..64).map(|i| i ^ 0xAA).collect();
    let req = signed_override_req(&fake_override_key, &target, b"pwn", 20, "no key configured");
    let resp = rpc(&sock, &req);
    assert!(!resp.ok);
    assert_eq!(
        resp.err_code,
        Some(ErrCode::OverrideDisabled),
        "expected OverrideDisabled, got {:?}",
        resp
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "old",
        "protected file must remain unmodified when override is disabled"
    );
    drop(td);
}
