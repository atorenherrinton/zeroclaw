//! Canonical immutable preparations, byte snapshots and single-attempt claims.
//! Provider state owns remote effects. This ledger never replays uncertain writes.
use crate::model::{hash, key};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs::File,
    path::{Path, PathBuf},
};

pub struct Store {
    db: Connection,
    execution_path: Option<PathBuf>,
    executing: Cell<bool>,
}

/// A live execution lock, separate from the durable uncertainty receipt. The OS
/// releases the file lock on process death; the receipt remains for recovery.
pub struct ExecutionGuard<'a> {
    lock_file: Option<File>,
    executing: &'a Cell<bool>,
}
impl Drop for ExecutionGuard<'_> {
    fn drop(&mut self) {
        drop(self.lock_file.take());
        self.executing.set(false);
    }
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        // The state directory is operator-owned, not an attachment/output path.
        ensure!(root.is_absolute(), "absolute state root required");
        for parent in root.ancestors() {
            ensure!(!parent.is_symlink(), "state directory symlink denied");
        }
        let dir = root.join("extensions/gmail-drafts");
        for path in [root.join("extensions"), dir.clone()] {
            ensure!(!path.is_symlink(), "state directory symlink denied");
            std::fs::create_dir_all(&path).context("state directory unavailable")?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            let path = dir.join("drafts.sqlite3");
            for suffix in ["", "-wal", "-shm", "-journal"] {
                ensure!(
                    !dir.join(format!("drafts.sqlite3{suffix}")).is_symlink(),
                    "ledger symlink denied"
                );
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .context("ledger unavailable")?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        let mut store = Self::initialize(
            Connection::open(dir.join("drafts.sqlite3")).context("ledger unavailable")?,
        )?;
        store.execution_path = Some(dir.join("execution.lock"));
        Ok(store)
    }
    pub fn memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }
    fn initialize(db: Connection) -> Result<Self> {
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY, bytes BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS preparations(operation_id TEXT PRIMARY KEY, request_hash TEXT NOT NULL,
                review_id TEXT NOT NULL UNIQUE, review TEXT NOT NULL, raw BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS operations(operation_id TEXT PRIMARY KEY, intent TEXT NOT NULL,
                state TEXT NOT NULL, receipt TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS draft_locks(draft_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL UNIQUE);")?;
        Ok(Self {
            db,
            execution_path: None,
            executing: Cell::new(false),
        })
    }

    /// Serialize mutation and reconciliation across helper processes. Reads and
    /// preparation stay available. A busy result must never authorize a write.
    pub fn execution_guard(&self) -> Result<ExecutionGuard<'_>> {
        const BUSY: &str = "another draft operation is in flight; wait for it to settle before applying or reconciling";
        ensure!(!self.executing.get(), BUSY);
        let lock_file = if let Some(path) = &self.execution_path {
            use rustix::fs::{Mode, OFlags, open};
            let file = File::from(
                open(
                    path,
                    OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .context("execution lock unavailable")?,
            );
            let metadata = file.metadata()?;
            ensure!(metadata.is_file(), "execution lock must be a regular file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(metadata.nlink() == 1, "hard-linked execution lock denied");
            }
            file.try_lock().map_err(|_| anyhow::Error::msg(BUSY))?;
            Some(file)
        } else {
            None
        };
        self.executing.set(true);
        Ok(ExecutionGuard {
            lock_file,
            executing: &self.executing,
        })
    }
    pub fn blob(&self, bytes: &[u8]) -> Result<String> {
        let h = hash(bytes);
        self.db.execute(
            "INSERT OR IGNORE INTO blobs VALUES (?1,?2)",
            params![h, bytes],
        )?;
        Ok(h)
    }
    pub fn bytes(&self, h: &str) -> Result<Vec<u8>> {
        let bytes: Vec<u8> = self
            .db
            .query_row("SELECT bytes FROM blobs WHERE hash=?1", [h], |r| r.get(0))
            .context("attachment snapshot unavailable")?;
        ensure!(hash(&bytes) == h, "snapshot hash mismatch");
        Ok(bytes)
    }
    pub fn prepared(&self, operation: &str, request_hash: &str) -> Result<Option<Value>> {
        let row: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT request_hash,review FROM preparations WHERE operation_id=?1",
                [operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(h, r)| {
            ensure!(
                h == request_hash,
                "operation ID bound to different preparation"
            );
            Ok(serde_json::from_str(&r)?)
        })
        .transpose()
    }
    pub fn save(
        &self,
        operation: &str,
        request_hash: &str,
        mut review: Value,
        raw: &[u8],
    ) -> Result<Value> {
        review["raw_sha256"] = json!(hash(raw));
        review["raw_bytes"] = json!(raw.len());
        let h = hash(&serde_json::to_vec(&review)?);
        review["review_id"] = json!(h);
        self.db.execute(
            "INSERT OR IGNORE INTO preparations VALUES (?1,?2,?3,?4,?5)",
            params![
                operation,
                request_hash,
                h,
                serde_json::to_string(&review)?,
                raw
            ],
        )?;
        self.prepared(operation, request_hash)?
            .context("preparation missing")
    }
    pub fn review(&self, operation: &str, review_id: &str) -> Result<(Value, Vec<u8>)> {
        let (text, raw): (String, Vec<u8>) = self
            .db
            .query_row(
                "SELECT review,raw FROM preparations WHERE operation_id=?1 AND review_id=?2",
                params![operation, review_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .context("exact preparation not found")?;
        let review: Value = serde_json::from_str(&text)?;
        let mut unsigned = review.clone();
        unsigned
            .as_object_mut()
            .context("invalid review")?
            .remove("review_id");
        ensure!(
            hash(&serde_json::to_vec(&unsigned)?) == review_id
                && review["raw_sha256"] == hash(&raw),
            "immutable review corrupted"
        );
        Ok((review, raw))
    }
    pub fn find(&self, operation: &str) -> Result<Option<Value>> {
        key(operation)?;
        let row: Option<(String, String, String)> = self
            .db
            .query_row(
                "SELECT intent,state,receipt FROM operations WHERE operation_id=?1",
                [operation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(intent,state,receipt)| Ok(json!({"operation_id":operation,"state":state,
            "intent":serde_json::from_str::<Value>(&intent)?,"receipt":serde_json::from_str::<Value>(&receipt)?,
            "automatic_retry_allowed":false}))).transpose()
    }
    /// Atomically bind the operation and (for updates/deletes) lock the exact draft.
    /// A racing caller that loses this claim MUST NOT contact the write endpoint.
    pub fn claim(&self, operation: &str, intent: &Value, target: Option<&str>) -> Result<bool> {
        key(operation)?;
        let text = serde_json::to_string(intent)?;
        let tx = self.db.unchecked_transaction()?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT intent FROM operations WHERE operation_id=?1",
                [operation],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            ensure!(previous == text, "operation ID bound to a different intent");
            return Ok(false);
        }
        if let Some(draft) = target {
            tx.execute(
                "INSERT INTO draft_locks VALUES (?1,?2)",
                params![draft, operation],
            )
            .context("exact draft has an unresolved operation; reconcile first")?;
        }
        tx.execute(
            "INSERT INTO operations VALUES (?1,?2,'uncertain','{}')",
            params![operation, text],
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub fn finish(&self, operation: &str, state: &str, receipt: &Value) -> Result<Value> {
        ensure!(
            matches!(state, "applied" | "absent" | "rejected" | "uncertain"),
            "invalid ledger state"
        );
        let tx = self.db.unchecked_transaction()?;
        tx.execute(
            "UPDATE operations SET state=?2,receipt=?3 WHERE operation_id=?1 AND state='uncertain'",
            params![operation, state, serde_json::to_string(receipt)?],
        )?;
        if state != "uncertain" {
            tx.execute("DELETE FROM draft_locks WHERE operation_id=?1", [operation])?;
        }
        tx.commit()?;
        self.find(operation)?.context("operation missing")
    }
}
