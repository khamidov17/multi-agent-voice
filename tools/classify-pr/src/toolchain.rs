//! Detect drift between the `rust-toolchain.toml` the classifier binary
//! was built against and the file present in the repo at runtime.
//!
//! The build script (`build.rs`) reads `rust-toolchain.toml` at compile
//! time, computes its SHA-256, and embeds the hex digest as the
//! `TOOLCHAIN_HASH` environment variable. At runtime, the classifier
//! re-reads the file from the repo it's classifying and compares.
//!
//! When they differ, the verdict is `TOOLCHAIN_HASH_MISMATCH` (Phase 4.1
//! ReasonCode), exit code 4. The classifier fails closed: a stale binary
//! with a fresh repo (or vice versa) cannot produce a trustworthy
//! fmt-equivalence verdict because rustfmt's output may differ across
//! toolchains.
//!
//! The string `"missing-at-build-time"` is the sentinel value the build
//! script writes when it cannot read `rust-toolchain.toml` (e.g., a
//! sparse-checkout that omitted the file). Any drift check against the
//! sentinel returns drift, fail-closed.

use std::path::Path;

/// SHA-256 of the `rust-toolchain.toml` the binary was compiled against.
/// Set by `build.rs` via `cargo:rustc-env=TOOLCHAIN_HASH=<hex>`.
pub fn embedded_hash() -> &'static str {
    env!("TOOLCHAIN_HASH")
}

/// Re-read `rust-toolchain.toml` from the live repo and compute its
/// SHA-256. Returns `Ok(None)` if the file does not exist (e.g., a repo
/// that doesn't pin the toolchain — older callers). Returns `Err` only
/// on real filesystem errors.
pub fn current_hash(repo_root: &Path) -> std::io::Result<Option<String>> {
    let path = repo_root.join("rust-toolchain.toml");
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(sha256_hex(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Returns `Some(message)` when the embedded hash differs from what's on
/// disk. The message is human-readable and matches the format documented
/// in `docs/phase4-debugging.md#toolchain_hash_mismatch`. Returns `None`
/// when there is no drift OR when the repo lacks `rust-toolchain.toml`
/// entirely (in which case the classifier has nothing to drift against).
pub fn check_drift(repo_root: &Path) -> std::io::Result<Option<String>> {
    let embedded = embedded_hash();
    let current = match current_hash(repo_root)? {
        Some(h) => h,
        None => return Ok(None),
    };

    if embedded == current {
        return Ok(None);
    }

    Ok(Some(format!(
        "TOOLCHAIN_HASH_MISMATCH: classifier built against {embedded}, repo pins {current}. \
         See docs/phase4-debugging.md#toolchain_hash_mismatch for the bump procedure."
    )))
}

/// Same SHA-256 implementation as `build.rs` (kept independent so build
/// and runtime never disagree on what "the hash of these bytes" means).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = data.to_vec();
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    h.iter().map(|w| format!("{w:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sha256_known_vectors() {
        // Empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // "abc" — canonical FIPS-180-4 Appendix B test vector.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn drift_detected_when_file_differs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rust-toolchain.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        // Write content that is virtually guaranteed to NOT hash to
        // whatever build.rs embedded (which is hash of THIS repo's
        // rust-toolchain.toml, not "[fake] = \"42\"").
        f.write_all(b"[fake] = \"42\"\n").unwrap();
        f.flush().unwrap();

        let drift = check_drift(dir.path()).unwrap();
        assert!(drift.is_some(), "should detect drift against fake content");
        let msg = drift.unwrap();
        assert!(msg.contains("TOOLCHAIN_HASH_MISMATCH"));
        assert!(msg.contains("classifier built against"));
    }

    #[test]
    fn no_drift_when_file_missing() {
        // Repo has no rust-toolchain.toml — classifier silently skips
        // the check, returning None.
        let dir = tempfile::tempdir().unwrap();
        let drift = check_drift(dir.path()).unwrap();
        assert!(drift.is_none());
    }

    #[test]
    fn embedded_hash_is_set() {
        // The build script must always set TOOLCHAIN_HASH (either to a
        // real SHA or to the sentinel). env! would have failed compile
        // if it weren't set.
        let h = embedded_hash();
        assert!(!h.is_empty());
    }
}
