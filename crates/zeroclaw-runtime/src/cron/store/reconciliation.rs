//! Operator evidence is a new fact, not a rewrite of execution/delivery history.
//! Only exact, terminal, latest no-effect occurrences can release quarantine.
use super::*;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationDisposition {
    NoExternalEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationRequest {
    pub occurrence_id: String,
    pub expected_state: String,
    pub disposition: ReconciliationDisposition,
    /// Explicit operator assertion, never inferred from an agent's output.
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationReceipt {
    pub job_id: String,
    pub run_id: i64,
    pub request: ReconciliationRequest,
    /// Set by the operator transport, not accepted in the request body.
    pub source: String,
    pub reconciled_at: String,
    pub retry_allowed: bool,
}

#[derive(Debug, Serialize)]
pub struct ReconciliationStatus {
    pub job_id: String,
    pub run_id: i64,
    pub occurrence_id: String,
    pub expected_state: Option<String>,
    pub eligible: bool,
    pub enabled: bool,
    pub next_run: String,
    pub last_status: Option<String>,
    pub receipt: Option<ReconciliationReceipt>,
    pub retry_allowed: bool,
}

#[derive(Serialize)]
struct Snapshot {
    job: CronJob,
    run: (String, String, String, Option<String>),
    occurrence: (String, String, Option<String>, String),
    locked: bool,
    latest: bool,
    other_uncertain: bool,
}

fn rejected() -> anyhow::Error {
    anyhow::Error::msg(crate::i18n::get_required_cli_string(
        "cron-reconcile-rejected",
    ))
}

fn validate_ids(job_id: &str, run_id: i64, occurrence_id: &str) -> Result<()> {
    if job_id.is_empty()
        || job_id.len() > 512
        || job_id.chars().any(char::is_control)
        || run_id <= 0
        || occurrence_id.is_empty()
        || occurrence_id.len() > 512
        || occurrence_id.chars().any(char::is_control)
    {
        return Err(rejected());
    }
    Ok(())
}

fn receipt(conn: &Connection, job_id: &str, run_id: i64) -> Result<Option<ReconciliationReceipt>> {
    // Read-only status also works before the first schema upgrade.
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cron_reconciliations')", [], |r| r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT receipt FROM cron_reconciliations WHERE job_id=?1 AND run_id=?2",
            params![job_id, run_id],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}

fn snapshot(conn: &Connection, job_id: &str, run_id: i64, occurrence_id: &str) -> Result<Snapshot> {
    let job = read_job_row(conn, job_id)?;
    let run = conn.query_row(
        "SELECT started_at,finished_at,status,output FROM cron_runs WHERE job_id=?1 AND id=?2",
        params![job_id, run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let occurrence = conn.query_row(
        "SELECT execution_state,delivery_state,output,updated_at FROM cron_occurrences WHERE job_id=?1 AND scheduled_at=?2",
        params![job_id, occurrence_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let locked = conn.query_row(
        "SELECT locked_at IS NOT NULL OR lock_owner IS NOT NULL FROM cron_jobs WHERE id=?1",
        [job_id],
        |r| r.get(0),
    )?;
    let latest = conn.query_row(
        "SELECT id=?2 FROM cron_runs WHERE job_id=?1 ORDER BY started_at DESC,id DESC LIMIT 1",
        params![job_id, run_id],
        |r| r.get(0),
    )?;
    // Deliberately narrow: multiple unresolved occurrences require separate
    // investigation, not an implicit choice of the latest/title-matched row.
    let other_uncertain = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM cron_occurrences WHERE job_id=?1 AND scheduled_at!=?2 AND (execution_state IN ('claimed','running','possibly_applied') OR delivery_state IN ('submitting','submitted','uncertain','possibly_applied','partially_applied','reconciliation_required')))",
        params![job_id,occurrence_id], |r| r.get(0))?;
    Ok(Snapshot {
        job,
        run,
        occurrence,
        locked,
        latest,
        other_uncertain,
    })
}

impl Snapshot {
    fn eligible(&self) -> bool {
        !self.job.enabled && !self.locked && self.latest && !self.other_uncertain
            && self.job.last_status.as_deref() == Some("uncertain")
            && self.run.2 == "error"
            && self.occurrence.0 == "possibly_applied"
            && self.occurrence.1 == "not_requested"
            && self.job.last_output == self.run.3 && self.run.3 == self.occurrence.2
            && self.job.last_run == parse_rfc3339(&self.run.1).ok()
            // Re-enabling an expired one-shot is not a safe future schedule.
            && !matches!(self.job.schedule, Schedule::At { .. })
    }
    fn token(&self) -> Result<String> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
}

/// Exact bounded status, using a read-only connection: never creates/migrates DB.
pub fn reconciliation_status(
    config: &Config,
    job_id: &str,
    run_id: i64,
    occurrence_id: &str,
) -> Result<ReconciliationStatus> {
    validate_ids(job_id, run_id, occurrence_id)?;
    let conn = Connection::open_with_flags(
        cron_db_path(config),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let tx = conn.unchecked_transaction()?;
    let existing = receipt(&tx, job_id, run_id)?;
    if existing
        .as_ref()
        .is_some_and(|r| r.request.occurrence_id != occurrence_id)
    {
        return Err(rejected());
    }
    if existing.is_some() {
        // History retention may prune the run later. The persisted operator
        // receipt remains authoritative and readable without that source row.
        let job = read_job_row(&tx, job_id)?;
        return Ok(ReconciliationStatus {
            job_id: job_id.into(),
            run_id,
            occurrence_id: occurrence_id.into(),
            expected_state: None,
            eligible: false,
            enabled: job.enabled,
            next_run: job.next_run.to_rfc3339(),
            last_status: job.last_status,
            receipt: existing,
            retry_allowed: false,
        });
    }
    let s = snapshot(&tx, job_id, run_id, occurrence_id)?;
    Ok(ReconciliationStatus {
        job_id: job_id.into(),
        run_id,
        occurrence_id: occurrence_id.into(),
        expected_state: if existing.is_none() {
            Some(s.token()?)
        } else {
            None
        },
        eligible: existing.is_none() && s.eligible(),
        enabled: s.job.enabled,
        next_run: s.job.next_run.to_rfc3339(),
        last_status: s.job.last_status,
        receipt: existing,
        retry_allowed: false,
    })
}

/// Trusted local operator boundary only. No tool factory registers this operation.
/// Receipt insertion and gate release share one FULL-synchronous writer commit.
/// The original run/occurrence, definition, schedule and request key are untouched.
pub fn reconcile_no_external_effect(
    config: &Config,
    job_id: &str,
    run_id: i64,
    request: &ReconciliationRequest,
    source: &str,
) -> Result<ReconciliationReceipt> {
    validate_ids(job_id, run_id, &request.occurrence_id)?;
    if request.expected_state.len() != 64
        || !request
            .expected_state
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || request.evidence.trim().is_empty()
        || request.evidence.len() > 4096
        || source.is_empty()
        || source.len() > 256
        || source.chars().any(char::is_control)
    {
        return Err(rejected());
    }
    if !cron_db_path(config).is_file() {
        return Err(rejected());
    }
    with_initialized_connection(config, |conn| {
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        if let Some(existing) = receipt(&tx, job_id, run_id)? {
            if existing.request == *request && existing.source == source {
                return Ok(existing);
            }
            return Err(rejected());
        }
        let s = snapshot(&tx, job_id, run_id, &request.occurrence_id)?;
        if !s.eligible() || s.token()? != request.expected_state {
            return Err(rejected());
        }
        let receipt = ReconciliationReceipt {
            job_id: job_id.into(),
            run_id,
            request: request.clone(),
            source: source.into(),
            reconciled_at: Utc::now().to_rfc3339(),
            retry_allowed: false,
        };
        tx.execute("INSERT INTO cron_reconciliations(job_id,run_id,occurrence_id,occurrence_updated_at,receipt) VALUES (?1,?2,?3,?4,?5)",
            params![job_id,run_id,request.occurrence_id,s.occurrence.3,serde_json::to_string(&receipt)?])?;
        let changed = tx.execute("UPDATE cron_jobs SET last_status='reconciled_no_external_effect' WHERE id=?1 AND last_status='uncertain' AND enabled=0 AND locked_at IS NULL AND lock_owner IS NULL", [job_id])?;
        if changed != 1 {
            return Err(rejected());
        }
        tx.commit()?;
        Ok(receipt)
    })
}

#[cfg(test)]
mod tests;
