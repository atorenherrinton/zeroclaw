//! The single SQLite-backed [`TaskRegistry`] — EPIC A's durable index.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

use super::authority::is_authoritative;
use super::task_registry::{TaskKind, TaskRecord, TaskRegistry, TaskStatus};

mod goal;

const CONTROL_PLANE_SCHEMA_VERSION: i64 = 9;

pub struct SqliteTaskStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteTaskStore {
    /// Open (creating if absent) the control-plane DB at `<data_dir>/control_plane.db`.
    /// Additive: a fresh install gets an empty DB and today's behavior is unchanged.
    pub fn new(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("control_plane.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open control-plane DB: {}", db_path.display()))?;
        Self::init(conn)
    }

    /// In-memory store for unit tests.
    pub fn new_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().context("open in-memory control-plane DB")?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        anyhow::ensure!(
            version <= CONTROL_PLANE_SCHEMA_VERSION,
            "unsupported future control-plane schema"
        );
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA temp_store = MEMORY;
             PRAGMA foreign_keys = ON;",
        )
        .context("set control-plane PRAGMAs")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                 id              TEXT PRIMARY KEY,
                 kind            TEXT NOT NULL,
                 agent           TEXT NOT NULL,
                 status          TEXT NOT NULL,
                 owner_pid       INTEGER NOT NULL DEFAULT 0,
                 owner_boot_id   TEXT NOT NULL DEFAULT '',
                 heartbeat_at    TEXT,
                 depth           INTEGER NOT NULL DEFAULT 0,
                 parent_id       TEXT,
                 originator_route TEXT,
                 delivered       INTEGER NOT NULL DEFAULT 0,
                 idem_key        TEXT,
                 principal_id    TEXT,
                 started_at      TEXT NOT NULL,
                 finished_at     TEXT,
                 output          TEXT,
                 error           TEXT
             );
             CREATE TABLE IF NOT EXISTS task_inputs (
                 task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
                 input TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
             CREATE INDEX IF NOT EXISTS idx_tasks_agent  ON tasks(agent);
             CREATE INDEX IF NOT EXISTS idx_tasks_agent_kind_started
                ON tasks(agent, kind, started_at DESC);",
        )
        .context("create control-plane base schema")?;
        migrate_schema(&conn).context("migrate control-plane schema")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Admin enumeration — count this agent's records (mirrors AcpSessionStore's
    /// `count_*_by_agent`; used by alias-delete cascades / observability).
    pub fn count_by_agent(&self, agent: &str) -> Result<u64> {
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE agent = ?1",
                params![agent],
                |r| r.get(0),
            )
            .context("count tasks by agent")?;
        Ok(n as u64)
    }

    /// Admin enumeration — delete this agent's records (alias-delete cascade).
    pub fn delete_by_agent(&self, agent: &str) -> Result<u64> {
        let conn = self.conn.lock();
        let n = conn
            .execute("DELETE FROM tasks WHERE agent = ?1", params![agent])
            .context("delete tasks by agent")?;
        Ok(n as u64)
    }
}

fn record_turn_event(
    conn: &Connection,
    id: &str,
    state: TaskStatus,
    delivered: bool,
    response_bytes: usize,
) -> Result<()> {
    conn.execute("INSERT INTO task_turn_events(task_id,state,recorded_at,delivered,response_bytes) VALUES(?1,?2,?3,?4,?5)",
        params![id, status_to_db(state), chrono::Utc::now().to_rfc3339(), delivered, response_bytes as i64])?;
    Ok(())
}

fn migrate_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read control-plane schema version")?;
    let tx = conn.unchecked_transaction()?;
    goal::migrate_schema(&tx, version)?;
    if version < 8 {
        tx.execute_batch("PRAGMA user_version=8")?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS task_turn_events (
        sequence INTEGER PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
        state TEXT NOT NULL,
        recorded_at TEXT NOT NULL,
        delivered INTEGER NOT NULL,
        response_bytes INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_turn_events_task ON task_turn_events(task_id, sequence);
    PRAGMA user_version=9;",
    )?;
    tx.commit()?;
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .with_context(|| format!("inspect {table} columns"))?;
    let mut rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .with_context(|| format!("query {table} columns"))?;
    let exists = rows.any(|name| matches!(name, Ok(name) if name == column));
    if !exists {
        conn.execute_batch(alter_sql)
            .with_context(|| format!("add {table}.{column}"))?;
    }
    Ok(())
}

// ── serde<->TEXT helpers (reuse the snake_case derive, no hand-kept string tables) ──

fn kind_to_db(k: TaskKind) -> String {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "delegate".into())
}

fn status_to_db(s: TaskStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "running".into())
}

fn kind_from_db(s: &str) -> Result<TaskKind> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .with_context(|| format!("unknown task kind {s:?}"))
}

fn status_from_db(s: &str) -> Result<TaskStatus> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .with_context(|| format!("unknown task status {s:?}"))
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let kind_s: String = row.get("kind")?;
    let status_s: String = row.get("status")?;
    // serde parse failures map to a SQLite conversion error; callers SKIP such rows
    // (collect_skipping_bad_rows) rather than failing the whole query. The column index
    // (`0`) is a placeholder — rusqlite has no by-name conversion-error ctor and the
    // index is not surfaced to the skip path (review nit #4).
    let kind = kind_from_db(&kind_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    let status = status_from_db(&status_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    Ok(TaskRecord {
        id: row.get("id")?,
        kind,
        agent: row.get("agent")?,
        status,
        owner_pid: row.get::<_, i64>("owner_pid")? as u32,
        owner_boot_id: row.get("owner_boot_id")?,
        heartbeat_at: row.get("heartbeat_at")?,
        depth: row.get::<_, i64>("depth")? as u32,
        parent_id: row.get("parent_id")?,
        originator_route: row.get("originator_route")?,
        delivered: row.get::<_, i64>("delivered")? != 0,
        idem_key: row.get("idem_key")?,
        principal_id: row.get("principal_id")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
    })
}

/// Collect query rows, SKIPPING (and logging) any single row that fails to convert —
/// one unrecognised/corrupt record (e.g. a forward-incompat `kind`/`status` written by a
/// newer binary) must not fail the whole enumeration and starve the reaper (finding #3).
fn collect_skipping_bad_rows<I>(rows: I) -> Vec<TaskRecord>
where
    I: Iterator<Item = rusqlite::Result<TaskRecord>>,
{
    let mut out = Vec::new();
    for r in rows {
        match r {
            Ok(rec) => out.push(rec),
            Err(e) => log_unreadable_task_row(e),
        }
    }
    out
}

fn log_unreadable_task_row(error: rusqlite::Error) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({ "error": format!("{error}") })),
        "control-plane: skipping unreadable task row"
    );
}

fn insert_task_record(conn: &Connection, rec: TaskRecord) -> Result<()> {
    // ON CONFLICT DO NOTHING, NOT INSERT OR REPLACE: re-registering an existing id
    // must be a true no-op, never clobber an already-recorded output/error/terminal
    // status back to NULL/running (review finding— the documented idempotency).
    conn.execute(
        "INSERT INTO tasks
            (id, kind, agent, status, owner_pid, owner_boot_id, heartbeat_at, depth,
             parent_id, originator_route, delivered, idem_key, principal_id,
             started_at, finished_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
         ON CONFLICT(id) DO NOTHING",
        params![
            rec.id,
            kind_to_db(rec.kind),
            rec.agent,
            status_to_db(rec.status),
            rec.owner_pid as i64,
            rec.owner_boot_id,
            rec.heartbeat_at,
            rec.depth as i64,
            rec.parent_id,
            rec.originator_route,
            rec.delivered as i64,
            rec.idem_key,
            rec.principal_id,
            rec.started_at,
            rec.finished_at,
        ],
    )
    .context("insert task record")?;
    Ok(())
}

fn update_task_status_record(
    conn: &Connection,
    id: &str,
    status: TaskStatus,
    output: Option<String>,
    error: Option<String>,
) -> Result<usize> {
    let finished_at = status
        .is_terminal()
        .then(|| chrono::Utc::now().to_rfc3339());
    conn.execute(
        "UPDATE tasks
            SET status = ?1,
                output = COALESCE(?2, output),
                error  = COALESCE(?3, error),
                finished_at = COALESCE(?4, finished_at)
          WHERE id = ?5
            AND status NOT IN ('completed','delivered','failed','cancelled','lost','timed_out','partially_delivered','uncertain')",
        params![status_to_db(status), output, error, finished_at, id],
    )
    .context("update task status")
}

fn claim_task_owner_record(
    conn: &Connection,
    id: &str,
    owner_pid: u32,
    owner_boot_id: &str,
) -> Result<usize> {
    conn.execute(
        "UPDATE tasks
            SET owner_pid = ?1,
                owner_boot_id = ?2,
                heartbeat_at = NULL
          WHERE id = ?3
            AND status NOT IN ('completed','delivered','failed','cancelled','lost','timed_out','partially_delivered','uncertain')",
        params![owner_pid as i64, owner_boot_id, id],
    )
    .context("claim task owner")
}

#[async_trait::async_trait]
impl TaskRegistry for SqliteTaskStore {
    async fn create(&self, rec: TaskRecord) -> Result<()> {
        let connection = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = connection.lock();
            insert_task_record(&conn, rec)?;
            Ok(())
        })
        .await?
    }

    async fn admit_channel_turn(&self, rec: TaskRecord, input: String) -> Result<bool> {
        anyhow::ensure!(
            rec.kind == TaskKind::ChannelTurn
                && rec.status == TaskStatus::Received
                && !rec.id.is_empty()
                && input.len() <= 1024 * 1024,
            "invalid channel turn checkpoint"
        );
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock();
            conn.execute_batch("PRAGMA synchronous=FULL")?;
            let result = (|| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
                    [&rec.id],
                    |r| r.get(0),
                )?;
                if exists {
                    return Ok(false);
                }
                let id = rec.id.clone();
                insert_task_record(&tx, rec)?;
                record_turn_event(&tx, &id, TaskStatus::Received, false, 0)?;
                tx.execute(
                    "INSERT INTO task_inputs(task_id,input) VALUES(?1,?2)",
                    params![id, input],
                )?;
                tx.commit()?;
                Ok(true)
            })();
            conn.execute_batch("PRAGMA synchronous=NORMAL")?;
            result
        })
        .await?
    }

    async fn take_recoverable_channel_turns(&self, boot_id: &str) -> Result<Vec<(String, String)>> {
        let connection = Arc::clone(&self.conn);
        let boot_id = boot_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut conn = connection.lock();
            conn.execute_batch("PRAGMA synchronous=FULL")?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let rows: Vec<(String,String)> = {
                let mut stmt = tx.prepare("SELECT t.id,i.input FROM tasks t JOIN task_inputs i ON i.task_id=t.id WHERE t.kind='channel_turn' AND t.status='queued' AND t.error='restart_pending' AND t.owner_boot_id=?1 ORDER BY (SELECT MIN(sequence) FROM task_turn_events e WHERE e.task_id=t.id) LIMIT 16")?;
                stmt.query_map([&boot_id], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
            };
            for (id, _) in &rows {
                tx.execute("UPDATE tasks SET status='received', error=NULL WHERE id=?1", [id])?;
                record_turn_event(&tx,id,TaskStatus::Received,false,0)?;
            }
            tx.commit()?;
            Ok(rows)
        }).await?
    }

    async fn assign_channel_turn(&self, id: &str, agent: &str, boot_id: &str) -> Result<()> {
        let connection = Arc::clone(&self.conn);
        let (id, agent, boot_id) = (id.to_owned(), agent.to_owned(), boot_id.to_owned());
        tokio::task::spawn_blocking(move || {
            let conn = connection.lock();
            let changed = conn.execute("UPDATE tasks SET agent=?2 WHERE id=?1 AND kind='channel_turn' AND status='received' AND owner_boot_id=?3", params![id, agent, boot_id])?;
            anyhow::ensure!(changed == 1, "channel turn is no longer available for routing");
            Ok(())
        }).await?
    }

    async fn checkpoint_channel_turn(
        &self,
        id: &str,
        status: TaskStatus,
        output: Option<String>,
        delivered: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            delivered == (status == TaskStatus::Delivered),
            "invalid confirmed delivery state"
        );
        anyhow::ensure!(
            output.as_ref().is_none_or(|s| s.len() <= 1024 * 1024),
            "channel response checkpoint too large"
        );
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock();
            conn.execute_batch("PRAGMA synchronous=FULL")?;
            let result = (|| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let (kind, previous): (String, String) =
                    tx.query_row("SELECT kind,status FROM tasks WHERE id=?1", [&id], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?;
                anyhow::ensure!(kind == "channel_turn", "checkpoint requires channel turn");
                let previous = status_from_db(&previous)?;
                anyhow::ensure!(
                    previous.permits_channel_transition(status),
                    "illegal channel turn transition: {previous:?} -> {status:?}"
                );
                let response_bytes = output.as_ref().map_or(0, String::len);
                record_turn_event(&tx, &id, status, delivered, response_bytes)?;
                anyhow::ensure!(
                    update_task_status_record(&tx, &id, status, output, None)? == 1,
                    "channel turn already terminal"
                );
                tx.execute(
                    "UPDATE tasks SET delivered=?2 WHERE id=?1",
                    params![id, delivered],
                )?;
                tx.commit()?;
                Ok(())
            })();
            conn.execute_batch("PRAGMA synchronous=NORMAL")?;
            result
        })
        .await?
    }

    async fn channel_turn_input(&self, id: &str) -> Result<Option<String>> {
        let conn = Arc::clone(&self.conn);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            Ok(conn
                .lock()
                .query_row(
                    "SELECT input FROM task_inputs WHERE task_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await?
    }

    async fn heartbeat(&self, id: &str, owner_boot_id: &str) -> Result<()> {
        let connection = Arc::clone(&self.conn);
        let id = id.to_owned();
        let owner_boot_id = owner_boot_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let id = id.as_str();
            let owner_boot_id = owner_boot_id.as_str();

            let now = chrono::Utc::now().to_rfc3339();
            let conn = connection.lock();
            // Only the heart-beating owner refreshes; prevents a stale boot from
            // resurrecting liveness it does not own.
            conn.execute(
                "UPDATE tasks SET heartbeat_at = ?1
             WHERE id = ?2 AND owner_boot_id = ?3",
                params![now, id, owner_boot_id],
            )
            .context("heartbeat task")?;
            Ok(())
        })
        .await?
    }

    async fn update_status(
        &self,
        id: &str,
        status: TaskStatus,
        output: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        let connection = Arc::clone(&self.conn);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let id = id.as_str();

            let conn = connection.lock();
            let kind: Option<String> = conn
                .query_row("SELECT kind FROM tasks WHERE id=?1", [id], |r| r.get(0))
                .optional()?;
            anyhow::ensure!(
                kind.as_deref() != Some("channel_turn"),
                "channel lifecycle requires a durable checkpoint"
            );
            update_task_status_record(&conn, id, status, output, error)?;
            Ok(())
        })
        .await?
    }

    async fn claim_owner(&self, id: &str, owner_pid: u32, owner_boot_id: &str) -> Result<()> {
        let connection = Arc::clone(&self.conn);
        let id = id.to_owned();
        let owner_boot_id = owner_boot_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let id = id.as_str();
            let owner_boot_id = owner_boot_id.as_str();

            let conn = connection.lock();
            claim_task_owner_record(&conn, id, owner_pid, owner_boot_id)?;
            Ok(())
        })
        .await?
    }

    async fn get(&self, id: &str) -> Result<Option<TaskRecord>> {
        let connection = Arc::clone(&self.conn);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let id = id.as_str();

            let conn = connection.lock();
            let rec = conn
                .query_row(
                    "SELECT * FROM tasks WHERE id = ?1",
                    params![id],
                    row_to_record,
                )
                .optional()
                .context("get task")?;
            Ok(rec)
        })
        .await?
    }

    async fn list_running(&self) -> Result<Vec<TaskRecord>> {
        let connection = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {

        let conn = connection.lock();
        let mut stmt = conn
            .prepare("SELECT * FROM tasks WHERE status IN ('received','queued','running','waiting_on_tool','response_ready','submitting')")
            .context("prepare list_running")?;
        let rows = stmt
            .query_map([], row_to_record)
            .context("query list_running")?;
        Ok(collect_skipping_bad_rows(rows))
        }).await?
    }

    async fn list_by_agent(&self, agent: &str) -> Result<Vec<TaskRecord>> {
        let connection = Arc::clone(&self.conn);
        let agent = agent.to_owned();
        tokio::task::spawn_blocking(move || {
            let agent = agent.as_str();

            let conn = connection.lock();
            let mut stmt = conn
                .prepare("SELECT * FROM tasks WHERE agent = ?1 ORDER BY started_at DESC")
                .context("prepare list_by_agent")?;
            let rows = stmt
                .query_map(params![agent], row_to_record)
                .context("query list_by_agent")?;
            Ok(collect_skipping_bad_rows(rows))
        })
        .await?
    }

    async fn reconcile_lost(&self, id: &str, now_boot_id: &str) -> Result<bool> {
        let connection = Arc::clone(&self.conn);
        let id = id.to_owned();
        let now_boot_id = now_boot_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let id = id.as_str();
            let now_boot_id = now_boot_id.as_str();

        let mut conn = connection.lock();
        conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rec = tx
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()
            .context("reconcile: load task")?;
        let Some(rec) = rec else { return Ok(false) };
        // Never reclaim a terminal record, and never one a live owner still holds.
        if rec.status.is_terminal() || !is_authoritative(&rec, now_boot_id) {
            return Ok(false);
        }
        let now = chrono::Utc::now().to_rfc3339();
        if rec.kind == TaskKind::ChannelTurn {
            let safe_queued = rec.status == TaskStatus::Queued;
            let state = if safe_queued { TaskStatus::Queued } else { TaskStatus::Uncertain };
            tx.execute("UPDATE tasks SET status=?2,owner_boot_id=?3,owner_pid=?4,error=?5,finished_at=?6 WHERE id=?1", params![id,status_to_db(state),now_boot_id,std::process::id(),if safe_queued { "restart_pending" } else { "restart_reconciliation_required" },if safe_queued { None } else { Some(&now) }])?;
            record_turn_event(&tx,id,state,false,0)?;
        } else {
            tx.execute("UPDATE tasks SET status='lost',finished_at=?2 WHERE id=?1", params![id,now])?;
        }
        tx.commit()?;
        Ok(true)
        }).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, agent: &str, owner_pid: u32, boot: &str) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            kind: TaskKind::Delegate,
            agent: agent.into(),
            status: TaskStatus::Running,
            owner_pid,
            owner_boot_id: boot.into(),
            heartbeat_at: None,
            depth: 0,
            parent_id: None,
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            started_at: "2026-06-18T00:00:00Z".into(),
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn channel_lifecycle_is_atomic_ordered_and_acknowledged() {
        let store = SqliteTaskStore::new_in_memory().unwrap();
        let mut task = rec("turn", "fixture", 0, "old");
        task.kind = TaskKind::ChannelTurn;
        task.status = TaskStatus::Received;
        assert!(
            store
                .admit_channel_turn(task.clone(), "private input".into())
                .await
                .unwrap()
        );
        assert!(
            !store
                .admit_channel_turn(task, "replacement".into())
                .await
                .unwrap()
        );
        assert!(
            store
                .checkpoint_channel_turn("turn", TaskStatus::Delivered, None, true)
                .await
                .is_err()
        );
        assert!(
            store
                .update_status("turn", TaskStatus::Completed, None, None)
                .await
                .is_err()
        );
        for state in [
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::WaitingOnTool,
            TaskStatus::Running,
            TaskStatus::ResponseReady,
            TaskStatus::Submitting,
        ] {
            store
                .checkpoint_channel_turn("turn", state, None, false)
                .await
                .unwrap();
        }
        assert!(
            store
                .checkpoint_channel_turn("turn", TaskStatus::Delivered, None, false)
                .await
                .is_err()
        );
        assert!(
            store
                .checkpoint_channel_turn("turn", TaskStatus::Running, None, false)
                .await
                .is_err()
        );
        store
            .checkpoint_channel_turn("turn", TaskStatus::Delivered, Some("response".into()), true)
            .await
            .unwrap();
        assert!(
            store
                .checkpoint_channel_turn("turn", TaskStatus::Uncertain, None, false)
                .await
                .is_err()
        );
        let record = store.get("turn").await.unwrap().unwrap();
        assert!(record.delivered && record.finished_at.is_some());
        assert_eq!(record.status, TaskStatus::Delivered);
        assert_eq!(
            store.channel_turn_input("turn").await.unwrap().as_deref(),
            Some("private input")
        );
        let conn = store.conn.lock();
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_turn_events WHERE task_id='turn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            events, 8,
            "rejected and duplicate changes must not create events"
        );
    }

    #[tokio::test]
    async fn channel_v7_migration_preserves_legacy_tasks_and_is_repeatable() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteTaskStore::new(dir.path()).unwrap();
        store
            .create(rec("legacy", "fixture", 0, "old"))
            .await
            .unwrap();
        store
            .conn
            .lock()
            .execute_batch(
                "DROP TABLE task_turn_events; DROP TABLE task_inputs; PRAGMA user_version=7;",
            )
            .unwrap();
        drop(store);
        for _ in 0..2 {
            let store = SqliteTaskStore::new(dir.path()).unwrap();
            assert_eq!(
                store.get("legacy").await.unwrap().unwrap().status,
                TaskStatus::Running
            );
            let version: i64 = store
                .conn
                .lock()
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 9);
        }
    }

    #[test]
    fn future_schema_is_rejected_before_modification() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version=10;").unwrap();
        assert!(SqliteTaskStore::init(conn).is_err());
    }

    #[tokio::test]
    async fn create_get_roundtrip() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.id, "a");
        assert_eq!(got.kind, TaskKind::Delegate);
        assert_eq!(got.status, TaskStatus::Running);
        assert!(s.get("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_status_sets_terminal_and_finished_at() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        s.update_status("a", TaskStatus::Completed, Some("done".into()), None)
            .await
            .unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Completed);
        assert!(got.finished_at.is_some());
    }

    #[tokio::test]
    async fn list_running_and_by_agent() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "b")).await.unwrap();
        s.create(rec("b", "main", 1, "b")).await.unwrap();
        s.create(rec("c", "other", 1, "b")).await.unwrap();
        s.update_status("b", TaskStatus::Completed, None, None)
            .await
            .unwrap();
        assert_eq!(s.list_running().await.unwrap().len(), 2); // a + c
        assert_eq!(s.list_by_agent("main").await.unwrap().len(), 2); // a + b
        assert_eq!(s.count_by_agent("main").unwrap(), 2);
    }

    #[tokio::test]
    async fn reconcile_lost_only_when_authoritative() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        // prior-boot orphan ⇒ reclaimable
        s.create(rec("orphan", "main", 999_999, "boot-OLD"))
            .await
            .unwrap();
        assert!(s.reconcile_lost("orphan", "boot-NEW").await.unwrap());
        assert_eq!(
            s.get("orphan").await.unwrap().unwrap().status,
            TaskStatus::Lost
        );

        // live same-boot owner ⇒ NOT reclaimable (split-brain guard)
        let me = std::process::id();
        s.create(rec("live", "main", me, "boot-NEW")).await.unwrap();
        assert!(!s.reconcile_lost("live", "boot-NEW").await.unwrap());
        assert_eq!(
            s.get("live").await.unwrap().unwrap().status,
            TaskStatus::Running
        );

        // already-terminal ⇒ no-op
        s.create(rec("done", "main", 0, "boot-OLD")).await.unwrap();
        s.update_status("done", TaskStatus::Completed, None, None)
            .await
            .unwrap();
        assert!(!s.reconcile_lost("done", "boot-NEW").await.unwrap());
    }

    #[tokio::test]
    async fn heartbeat_only_from_owner_boot() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-1")).await.unwrap();
        s.heartbeat("a", "boot-OTHER").await.unwrap(); // wrong boot: no-op
        assert!(s.get("a").await.unwrap().unwrap().heartbeat_at.is_none());
        s.heartbeat("a", "boot-1").await.unwrap(); // owner: stamps
        assert!(s.get("a").await.unwrap().unwrap().heartbeat_at.is_some());
    }

    #[tokio::test]
    async fn claim_owner_updates_canonical_owner_fields_for_resumed_task() {
        let s = SqliteTaskStore::new_in_memory().unwrap();
        s.create(rec("a", "main", 1, "boot-old")).await.unwrap();

        s.claim_owner("a", 42, "boot-new").await.unwrap();

        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.owner_pid, 42);
        assert_eq!(got.owner_boot_id, "boot-new");
        assert!(got.heartbeat_at.is_none());
    }
}
