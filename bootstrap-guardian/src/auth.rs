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

    #[test]
    fn constant_time_eq_correct() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }
}
