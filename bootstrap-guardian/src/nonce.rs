//! Monotonic nonce tracking, persisted in SQLite so it survives guardian
//! restarts. The guardian refuses any request whose nonce is <= the highest
//! seen for that UID; this prevents replay across guardian crashes.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Mutex;

pub struct NonceStore {
    conn: Mutex<Connection>,
}

impl NonceStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening nonce db at {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nonces (
                uid INTEGER PRIMARY KEY,
                highest_seen INTEGER NOT NULL,
                last_updated TEXT NOT NULL DEFAULT (datetime('now'))
            );
            PRAGMA journal_mode=WAL;",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Atomic UPSERT — single SQL statement so the SELECT+UPDATE race can't
    /// appear even if a future refactor drops the process-wide Mutex
    /// (e.g. moves to a connection pool). `RETURNING` tells us whether the
    /// row was inserted (first nonce for uid) or updated (strictly greater).
    /// If neither happened (incoming ≤ highest_seen), no row is returned →
    /// replay.
    ///
    /// Previously this was a 2-statement form inside a transaction. Correct
    /// in a single-threaded-per-request world but brittle under refactor.
    /// /review security flagged the fragility.
    pub fn consume(&self, uid: u32, incoming: u64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("nonce store mutex poisoned"))?;

        // INSERT ... ON CONFLICT DO UPDATE ... RETURNING. The `WHERE` on the
        // UPDATE is what rejects stale nonces atomically — if the candidate
        // isn't strictly greater, no UPDATE happens, and `RETURNING`
        // produces no row.
        let n_rows: usize = conn
            .query_row(
                "INSERT INTO nonces (uid, highest_seen) VALUES (?1, ?2)
                 ON CONFLICT(uid) DO UPDATE
                   SET highest_seen = excluded.highest_seen,
                       last_updated = datetime('now')
                   WHERE excluded.highest_seen > nonces.highest_seen
                 RETURNING highest_seen",
                params![uid as i64, incoming as i64],
                |_| Ok(1usize),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(0),
                other => Err(other),
            })?;

        Ok(n_rows > 0)
    }

    /// Non-mutating check: would `consume(uid, incoming)` succeed right now?
    /// Used by the server to validate a nonce BEFORE attempting the op, so a
    /// transient write failure does not burn the nonce. The server then
    /// calls `consume` only AFTER the op succeeds.
    ///
    /// This is NOT racy in the single-threaded-per-request model the guardian
    /// uses: we still hold the connection mutex exclusively in `consume`, and
    /// the peer UID is authenticated. Two concurrent requests from the same
    /// UID that both pass `would_accept` would both attempt `consume`; the
    /// second one loses the compare-and-swap and gets `Ok(false)`.
    pub fn would_accept(&self, uid: u32, incoming: u64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("nonce store mutex poisoned"))?;
        let current: Option<i64> = conn
            .query_row(
                "SELECT highest_seen FROM nonces WHERE uid = ?1",
                params![uid as i64],
                |row| row.get(0),
            )
            .ok();
        match current {
            Some(cur) => Ok((incoming as i64) > cur),
            None => Ok(true),
        }
    }

    #[cfg(test)]
    pub fn peek(&self, uid: u32) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT highest_seen FROM nonces WHERE uid = ?1",
            params![uid as i64],
            |row| row.get(0),
        )
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_nonce_accepted() {
        let td = tempfile::tempdir().unwrap();
        let store = NonceStore::open(&td.path().join("n.db")).unwrap();
        assert!(store.consume(1000, 1).unwrap());
        assert_eq!(store.peek(1000), Some(1));
    }

    #[test]
    fn monotonic_accepted_replay_rejected() {
        let td = tempfile::tempdir().unwrap();
        let store = NonceStore::open(&td.path().join("n.db")).unwrap();
        assert!(store.consume(42, 1).unwrap());
        assert!(store.consume(42, 2).unwrap());
        // Replay of 2
        assert!(!store.consume(42, 2).unwrap());
        // Out-of-order
        assert!(!store.consume(42, 1).unwrap());
        // Forward is fine
        assert!(store.consume(42, 10).unwrap());
    }

    #[test]
    fn different_uids_isolated() {
        let td = tempfile::tempdir().unwrap();
        let store = NonceStore::open(&td.path().join("n.db")).unwrap();
        assert!(store.consume(1, 100).unwrap());
        assert!(store.consume(2, 1).unwrap());
        assert!(!store.consume(2, 1).unwrap());
        assert!(store.consume(1, 101).unwrap());
    }

    #[test]
    fn survives_reopen() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("n.db");
        {
            let store = NonceStore::open(&db).unwrap();
            assert!(store.consume(7, 50).unwrap());
        }
        {
            let store = NonceStore::open(&db).unwrap();
            assert!(!store.consume(7, 50).unwrap());
            assert!(store.consume(7, 51).unwrap());
        }
    }
}
