//! Peer registry — persistent SQLite store of federation peers (#462).
//!
//! Schema (one table — keep it boring so child 3 doesn't need a migration):
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS peers (
//!   name         TEXT PRIMARY KEY,  -- "mac", "phone", "kira-vps"
//!   tailnet_addr TEXT,              -- last observed source IP
//!   last_seen_ts INTEGER,           -- unix epoch seconds
//!   last_msg_id  TEXT               -- nullable; reserved for child 3's replay cursor
//! );
//! ```
//!
//! deskd has no pre-existing SQLite store, so we open a dedicated
//! `~/.deskd/federation.sqlite` file with `CREATE TABLE IF NOT EXISTS`. No
//! migration framework — one table, idempotent schema.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A row in the `peers` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    pub name: String,
    pub tailnet_addr: Option<String>,
    pub last_seen_ts: i64,
    pub last_msg_id: Option<String>,
}

/// Thread-safe SQLite-backed peer registry. The connection is wrapped in a
/// `Mutex` because `rusqlite::Connection` is not `Sync` — our access pattern
/// (one UPSERT per hello + occasional lookups) is low-contention enough that
/// serializing through a mutex is fine.
pub struct PeerRegistry {
    conn: Mutex<Connection>,
}

impl PeerRegistry {
    /// Open (or create) the registry database at `path`. Initializes schema
    /// idempotently.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open federation sqlite at {}", path.display()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory registry. Used by tests; not persisted.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory sqlite")?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Default on-disk path: `~/.deskd/federation.sqlite`.
    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".deskd").join("federation.sqlite")
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS peers (
                name         TEXT PRIMARY KEY,
                tailnet_addr TEXT,
                last_seen_ts INTEGER NOT NULL DEFAULT 0,
                last_msg_id  TEXT
             )",
            [],
        )
        .context("create peers table")?;
        Ok(())
    }

    /// Upsert a peer's `tailnet_addr` and `last_seen_ts`. `last_msg_id` is
    /// preserved on conflict — child 3 owns that column.
    pub fn upsert_on_hello(
        &self,
        name: &str,
        tailnet_addr: Option<&str>,
        last_seen_ts: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("peer registry mutex poisoned");
        conn.execute(
            "INSERT INTO peers (name, tailnet_addr, last_seen_ts)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET
               tailnet_addr = excluded.tailnet_addr,
               last_seen_ts = excluded.last_seen_ts",
            params![name, tailnet_addr, last_seen_ts],
        )
        .context("upsert peer")?;
        Ok(())
    }

    /// Look up a peer by name.
    pub fn lookup(&self, name: &str) -> Result<Option<PeerRecord>> {
        let conn = self.conn.lock().expect("peer registry mutex poisoned");
        let row = conn
            .query_row(
                "SELECT name, tailnet_addr, last_seen_ts, last_msg_id FROM peers WHERE name = ?1",
                params![name],
                |row| {
                    Ok(PeerRecord {
                        name: row.get(0)?,
                        tailnet_addr: row.get(1)?,
                        last_seen_ts: row.get(2)?,
                        last_msg_id: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("lookup peer")?;
        Ok(row)
    }

    /// List all known peers, sorted by `last_seen_ts` descending.
    pub fn list(&self) -> Result<Vec<PeerRecord>> {
        let conn = self.conn.lock().expect("peer registry mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT name, tailnet_addr, last_seen_ts, last_msg_id
                 FROM peers ORDER BY last_seen_ts DESC",
            )
            .context("prepare list")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PeerRecord {
                    name: row.get(0)?,
                    tailnet_addr: row.get(1)?,
                    last_seen_ts: row.get(2)?,
                    last_msg_id: row.get(3)?,
                })
            })
            .context("query list")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("read peer row")?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_inserts_new_row() {
        let r = PeerRegistry::in_memory().unwrap();
        r.upsert_on_hello("mac", Some("100.64.0.5"), 1000).unwrap();
        let row = r.lookup("mac").unwrap().unwrap();
        assert_eq!(row.name, "mac");
        assert_eq!(row.tailnet_addr.as_deref(), Some("100.64.0.5"));
        assert_eq!(row.last_seen_ts, 1000);
        assert!(row.last_msg_id.is_none());
    }

    #[test]
    fn upsert_updates_existing_row_preserving_last_msg_id() {
        let r = PeerRegistry::in_memory().unwrap();
        r.upsert_on_hello("mac", Some("100.64.0.5"), 1000).unwrap();
        // Simulate child 3 writing a cursor.
        {
            let conn = r.conn.lock().unwrap();
            conn.execute(
                "UPDATE peers SET last_msg_id = ?1 WHERE name = ?2",
                params!["cursor-xyz", "mac"],
            )
            .unwrap();
        }
        // Reconnect updates addr + last_seen_ts but leaves cursor alone.
        r.upsert_on_hello("mac", Some("100.64.0.6"), 2000).unwrap();
        let row = r.lookup("mac").unwrap().unwrap();
        assert_eq!(row.tailnet_addr.as_deref(), Some("100.64.0.6"));
        assert_eq!(row.last_seen_ts, 2000);
        assert_eq!(row.last_msg_id.as_deref(), Some("cursor-xyz"));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let r = PeerRegistry::in_memory().unwrap();
        assert!(r.lookup("ghost").unwrap().is_none());
    }

    #[test]
    fn list_orders_by_last_seen_desc() {
        let r = PeerRegistry::in_memory().unwrap();
        r.upsert_on_hello("a", Some("100.64.0.1"), 100).unwrap();
        r.upsert_on_hello("b", Some("100.64.0.2"), 300).unwrap();
        r.upsert_on_hello("c", Some("100.64.0.3"), 200).unwrap();
        let list = r.list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "b");
        assert_eq!(list[1].name, "c");
        assert_eq!(list[2].name, "a");
    }

    #[test]
    fn persists_across_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fed.sqlite");
        {
            let r = PeerRegistry::open(&path).unwrap();
            r.upsert_on_hello("mac", Some("100.64.0.5"), 1234).unwrap();
        }
        let r = PeerRegistry::open(&path).unwrap();
        let row = r.lookup("mac").unwrap().unwrap();
        assert_eq!(row.last_seen_ts, 1234);
    }

    #[test]
    fn schema_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fed.sqlite");
        // Open twice — second open must not fail or wipe data.
        {
            let r = PeerRegistry::open(&path).unwrap();
            r.upsert_on_hello("mac", None, 1).unwrap();
        }
        let r = PeerRegistry::open(&path).unwrap();
        assert!(r.lookup("mac").unwrap().is_some());
    }
}
