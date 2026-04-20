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

    /// Atomically: read the highest nonce for `uid`; if `incoming > highest`,
    /// update and return `true`; else return `false` (replay).
    pub fn consume(&self, uid: u32, incoming: u64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("nonce store mutex poisoned"))?;
        let tx = conn.unchecked_transaction()?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT highest_seen FROM nonces WHERE uid = ?1",
                params![uid as i64],
                |row| row.get(0),
            )
            .ok();

        if let Some(cur) = current {
            if incoming as i64 <= cur {
                return Ok(false);
            }
            tx.execute(
                "UPDATE nonces SET highest_seen = ?1, last_updated = datetime('now') WHERE uid = ?2",
                params![incoming as i64, uid as i64],
            )?;
        } else {
            tx.execute(
                "INSERT INTO nonces (uid, highest_seen) VALUES (?1, ?2)",
                params![uid as i64, incoming as i64],
            )?;
        }
        tx.commit()?;
        Ok(true)
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
