use super::{invalid, Result};
use crate::engine::digest_json;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::Path;

const LIMIT: usize = 4 * 1024 * 1024;
const GENESIS: &str = "aad/recovery/1";

pub(super) struct Store {
    db: Connection,
    seq: i64,
    tip: String,
}

impl Store {
    pub(super) fn open(path: &Path, create: bool, inspect: bool) -> Result<Self> {
        if create {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).map_err(invalid)?;
        }
        let flags = if inspect {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let db = Connection::open_with_flags(path.canonicalize().map_err(invalid)?, flags)
            .map_err(invalid)?;
        db.busy_timeout(std::time::Duration::ZERO)
            .map_err(invalid)?;
        db.execute_batch("PRAGMA trusted_schema=OFF;")
            .map_err(invalid)?;
        if !inspect {
            db.execute_batch(
                "PRAGMA locking_mode=EXCLUSIVE; PRAGMA synchronous=FULL;
                              BEGIN EXCLUSIVE; COMMIT;",
            )
            .map_err(invalid)?;
        }
        if create {
            db.execute_batch("BEGIN;
                CREATE TABLE entries(seq INTEGER PRIMARY KEY, event TEXT NOT NULL, hash TEXT NOT NULL);
                CREATE TABLE head(id INTEGER PRIMARY KEY CHECK(id=1), seq INTEGER NOT NULL, hash TEXT NOT NULL);
                COMMIT;").map_err(invalid)?;
            db.execute("INSERT INTO head VALUES(1,0,?1)", [GENESIS])
                .map_err(invalid)?;
        }
        Ok(Self {
            db,
            seq: 0,
            tip: GENESIS.into(),
        })
    }

    pub(super) fn read(&mut self, mut accept: impl FnMut(Value) -> Result<()>) -> Result<()> {
        let mut query = self
            .db
            .prepare(
                "SELECT seq, length(CAST(event AS BLOB)), event, hash FROM entries ORDER BY seq",
            )
            .map_err(invalid)?;
        let mut rows = query.query([]).map_err(invalid)?;
        while let Some(row) = rows.next().map_err(invalid)? {
            let seq: i64 = row.get(0).map_err(invalid)?;
            let size: i64 = row.get(1).map_err(invalid)?;
            if seq != self.seq + 1 || !(0..=LIMIT as i64).contains(&size) {
                return Err(invalid("invalid recovery journal sequence or entry size"));
            }
            let raw: String = row.get(2).map_err(invalid)?;
            let hash: String = row.get(3).map_err(invalid)?;
            if hash != digest_json(&json!([self.tip, seq, raw])) {
                return Err(invalid("invalid recovery journal checksum"));
            }
            accept(serde_json::from_str(&raw).map_err(invalid)?)?;
            self.seq = seq;
            self.tip = hash;
        }
        let head: (i64, String) = self
            .db
            .query_row("SELECT seq, hash FROM head WHERE id=1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(invalid)?;
        if head != (self.seq, self.tip.clone()) || self.seq == 0 {
            return Err(invalid("incomplete recovery journal"));
        }
        Ok(())
    }

    pub(super) fn append(&mut self, event: Value) -> Result<()> {
        let raw = event.to_string();
        if raw.len() > LIMIT {
            return Err(invalid("recovery journal entry too large"));
        }
        let seq = self.seq + 1;
        let hash = digest_json(&json!([self.tip, seq, raw]));
        let tx = self.db.transaction().map_err(invalid)?;
        tx.execute(
            "INSERT INTO entries VALUES(?1,?2,?3)",
            params![seq, raw, hash],
        )
        .map_err(invalid)?;
        tx.execute(
            "UPDATE head SET seq=?1, hash=?2 WHERE id=1",
            params![seq, hash],
        )
        .map_err(invalid)?;
        tx.commit().map_err(invalid)?;
        self.seq = seq;
        self.tip = hash;
        Ok(())
    }
}
