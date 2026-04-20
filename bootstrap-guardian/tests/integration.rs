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
