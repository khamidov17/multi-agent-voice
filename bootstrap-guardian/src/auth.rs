//! HMAC authentication + SO_PEERCRED caller verification.

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::proto::Op;

type HmacSha256 = Hmac<Sha256>;

/// Load the shared secret key from disk.
/// Verifies the file is mode 0400 and readable to us — fails loudly otherwise.
pub fn load_key(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o400 && mode != 0o600 {
        anyhow::bail!(
            "guardian key at {} has mode 0{:o}; must be 0400 (or 0600 for owner-write). \
             Fix with `chmod 0400 {}`.",
            path.display(),
            mode,
            path.display()
        );
    }
    let key = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if key.len() < 32 {
        anyhow::bail!(
            "guardian key at {} is only {} bytes; need at least 32. \
             Regenerate with `head -c 64 /dev/urandom > {}`.",
            path.display(),
            key.len(),
            path.display()
        );
    }
    Ok(key)
}

/// Compute HMAC-SHA256 over `op || path || sha256(bytes) || nonce`.
///
/// Path is serialized as its UTF-8 bytes; op as its serde-rename string;
/// nonce as little-endian u64. This matches the client shim's sign() exactly.
pub fn compute_hmac(key: &[u8], op: Op, path: &str, bytes: &[u8], nonce: u64) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    let op_tag = match op {
        Op::Write => b"write".as_slice(),
        Op::Ping => b"ping".as_slice(),
        Op::OverrideWrite => b"override_write".as_slice(),
    };
    mac.update(op_tag);
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

/// Constant-time compare of hex strings. Prevents timing side-channels on
/// HMAC verification even though an attacker controlling timing of network-
/// adjacent operations is already quite powerful.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Read peer UID from a connected UnixStream via `SO_PEERCRED` (Linux) or
/// `LOCAL_PEERCRED`/`getpeereid` (macOS / BSD). Returns the peer's effective
/// UID at connect-time.
///
/// Note: the PID returned by these APIs is the connect-time PID; kernel
/// reassigns PIDs aggressively. UID + HMAC key knowledge is the real auth.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::mem::{MaybeUninit, size_of};
    use std::os::unix::io::AsRawFd;

    #[repr(C)]
    struct Ucred {
        pid: libc::pid_t,
        uid: libc::uid_t,
        gid: libc::gid_t,
    }

    let fd = stream.as_raw_fd();
    let mut cred = MaybeUninit::<Ucred>::uninit();
    let mut len = size_of::<Ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("SO_PEERCRED");
    }
    let cred = unsafe { cred.assume_init() };
    Ok(cred.uid as u32)
}

#[cfg(target_os = "macos")]
pub fn peer_uid(stream: &UnixStream) -> Result<u32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getpeereid");
    }
    Ok(uid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn peer_uid(_stream: &UnixStream) -> Result<u32> {
    anyhow::bail!("peer UID verification is only implemented for Linux and macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_is_stable_across_calls() {
        let key = b"k".repeat(32);
        let a = compute_hmac(&key, Op::Write, "/x/y", b"hello", 42);
        let b = compute_hmac(&key, Op::Write, "/x/y", b"hello", 42);
        assert_eq!(a, b);
    }

    #[test]
    fn hmac_changes_with_any_field() {
        let key = b"k".repeat(32);
        let base = compute_hmac(&key, Op::Write, "/x/y", b"hello", 42);
        assert_ne!(base, compute_hmac(&key, Op::Ping, "/x/y", b"hello", 42));
        assert_ne!(base, compute_hmac(&key, Op::Write, "/x/z", b"hello", 42));
        assert_ne!(base, compute_hmac(&key, Op::Write, "/x/y", b"world", 42));
        assert_ne!(base, compute_hmac(&key, Op::Write, "/x/y", b"hello", 43));
    }

    /// Cross-crate wire-compat pin. The harness's `src/guardian_client.rs`
    /// has an IDENTICAL test asserting the same hex string — if either side
    /// ever drifts (tag bytes, separator, hash order, nonce endianness) the
    /// matching harness test will fail first at CI time, loud and fast,
    /// instead of silently breaking HMAC at runtime.
    ///
    /// Fixture inputs: key = 0u8..64u8, op = Write, path = "/a", bytes = b"x", nonce = 1.
    /// To regenerate if the protocol changes deliberately: run this test,
    /// paste the new hex here AND in guardian_client.rs's `hmac_wire_fixture`.
    #[test]
    fn hmac_wire_fixture_write_op() {
        let key: Vec<u8> = (0..64).collect();
        let got = compute_hmac(&key, Op::Write, "/a", b"x", 1);
        // Value pinned from this exact implementation; the harness's
        // guardian_client::tests::hmac_wire_fixture asserts the same string.
        assert_eq!(
            got, "c28f43f14294ab137e3be1662eb17ad95057fc90af682ef6df2fdbf880613892",
            "HMAC wire format changed. If deliberate, update this test AND the \
             twin in src/guardian_client.rs together."
        );
    }

    /// Same idea, covering the ping op-tag path.
    #[test]
    fn hmac_wire_fixture_ping_op() {
        let key: Vec<u8> = (0..64).collect();
        let got = compute_hmac(&key, Op::Ping, "", &[], 1);
        assert_eq!(
            got, "4f4a2d97f99a96ddeffd284dea1e6a5136cf09b328636fdeab76c5965d2d1615",
            "HMAC wire format for Ping changed. Update both tests."
        );
    }

    #[test]
    fn constant_time_eq_correct() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn load_key_rejects_world_readable_mode() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("k");
        std::fs::write(&path, vec![0u8; 32]).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
        let err = load_key(&path).unwrap_err().to_string();
        assert!(
            err.contains("0644"),
            "error must name the bad mode: {}",
            err
        );
    }

    #[test]
    fn load_key_rejects_group_readable() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("k");
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o440);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(load_key(&path).is_err());
    }

    #[test]
    fn load_key_rejects_short_key() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("k");
        std::fs::write(&path, vec![0u8; 16]).unwrap(); // < 32 bytes
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o400);
        std::fs::set_permissions(&path, perms).unwrap();
        let err = load_key(&path).unwrap_err().to_string();
        assert!(err.contains("32") || err.contains("16"), "{}", err);
    }

    #[test]
    fn load_key_accepts_0400_with_32_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("k");
        std::fs::write(&path, vec![0u8; 32]).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o400);
        std::fs::set_permissions(&path, perms).unwrap();
        let k = load_key(&path).unwrap();
        assert_eq!(k.len(), 32);
    }

    #[test]
    fn load_key_accepts_0600_too() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("k");
        std::fs::write(&path, vec![42u8; 64]).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(load_key(&path).is_ok());
    }
}
