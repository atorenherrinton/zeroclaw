//! Bounded, read-only projections of the canonical occurrence ledger.
use super::cron_db_path;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use zeroclaw_api::delivery::EffectOutcome;
use zeroclaw_config::schema::Config;

const OUTPUT_BYTES: usize = 8192;
const PAGE_BYTES: usize = 65536;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceQuery {
    pub occurrence_id: Option<String>,
    pub before: Option<String>,
    pub limit: Option<u32>,
    #[serde(default)]
    pub include_output: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OccurrenceReceipt {
    pub occurrence_id: String,
    pub execution_state: String,
    pub delivery_state: String,
    pub execution_outcome: EffectOutcome,
    pub delivery_outcome: Option<EffectOutcome>,
    pub updated_at: String,
    /// This read surface never authorizes replay, even for a failed receipt.
    pub retry_allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub output_bytes: u64,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OccurrencePage {
    pub storage_present: bool,
    pub occurrences: Vec<OccurrenceReceipt>,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccurrenceReadError {
    InvalidQuery,
    StorageUnavailable,
}

impl std::fmt::Display for OccurrenceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::get_required_cli_string(match self {
            Self::InvalidQuery => "cron-occurrences-invalid-query",
            Self::StorageUnavailable => "cron-occurrences-storage-unavailable",
        }))
    }
}
impl std::error::Error for OccurrenceReadError {}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

impl OccurrenceQuery {
    fn validate(&self, job_id: &str) -> Result<usize, OccurrenceReadError> {
        let limit = self.limit.unwrap_or(20);
        if !valid_identity(job_id)
            || !(1..=100).contains(&limit)
            || self
                .occurrence_id
                .as_deref()
                .is_some_and(|v| !valid_identity(v))
            || self.before.as_deref().is_some_and(|v| !valid_identity(v))
            || (self.occurrence_id.is_some() && self.before.is_some())
        {
            return Err(OccurrenceReadError::InvalidQuery);
        }
        Ok(limit as usize)
    }
}

/// SQLite work is isolated from async workers. Reads never initialize or migrate
/// storage and do not require a surviving job row (one-shot jobs may be deleted).
pub async fn read_occurrences(
    config: &Config,
    job_id: String,
    query: OccurrenceQuery,
) -> Result<OccurrencePage, OccurrenceReadError> {
    query.validate(&job_id)?;
    let path = cron_db_path(config);
    tokio::task::spawn_blocking(move || read_path(&path, &job_id, &query))
        .await
        .map_err(|_| OccurrenceReadError::StorageUnavailable)?
}

fn read_path(
    path: &std::path::Path,
    job_id: &str,
    query: &OccurrenceQuery,
) -> Result<OccurrencePage, OccurrenceReadError> {
    let limit = query.validate(job_id)?;
    let unavailable = |_| OccurrenceReadError::StorageUnavailable;
    let present = path.try_exists().map_err(unavailable)?;
    let mut page = OccurrencePage {
        storage_present: present,
        occurrences: Vec::new(),
        next_before: None,
    };
    if !present {
        return Ok(page);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| OccurrenceReadError::StorageUnavailable)?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(|_| OccurrenceReadError::StorageUnavailable)?;
    read_page(&conn, job_id, query, limit, &mut page)
        .map_err(|_| OccurrenceReadError::StorageUnavailable)?;
    Ok(page)
}

// Fixed selectors allow SQLite to use the composite primary key for each shape.
fn query_sql(query: &OccurrenceQuery) -> String {
    let selector = if query.occurrence_id.is_some() {
        "AND scheduled_at = ?2"
    } else if query.before.is_some() {
        "AND scheduled_at < ?2"
    } else {
        ""
    };
    format!(
        "SELECT CASE WHEN length(CAST(scheduled_at AS BLOB)) <= 512 THEN scheduled_at END,
         CASE WHEN length(CAST(execution_state AS BLOB)) <= 64 THEN execution_state END,
         CASE WHEN length(CAST(delivery_state AS BLOB)) <= 64 THEN delivery_state END,
         CASE WHEN length(CAST(updated_at AS BLOB)) <= 64 THEN updated_at END,
         CASE WHEN ?4 THEN substr(CAST(output AS BLOB), 1, ?5) END,
         coalesce(length(CAST(output AS BLOB)), 0)
         FROM cron_occurrences WHERE job_id = ?1 {selector}
         ORDER BY scheduled_at DESC LIMIT ?3"
    )
}

fn read_page(
    conn: &Connection,
    job_id: &str,
    query: &OccurrenceQuery,
    limit: usize,
    page: &mut OccurrencePage,
) -> anyhow::Result<()> {
    // Identity order avoids rowid and mutable timestamp cursors; it is not
    // chronological order.
    let bound = query
        .occurrence_id
        .as_deref()
        .or(query.before.as_deref())
        .unwrap_or("");
    let mut statement = conn.prepare(&query_sql(query))?;
    let mut rows = statement.query(params![
        job_id,
        bound,
        limit + 1,
        query.include_output,
        OUTPUT_BYTES
    ])?;
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        if page.occurrences.len() == limit {
            has_more = true;
            break;
        }
        let occurrence_id: String = row.get(0)?;
        if !valid_identity(&occurrence_id) {
            anyhow::bail!("invalid occurrence identity");
        }
        let execution_state: String = row.get(1)?;
        let delivery_state: String = row.get(2)?;
        let updated_at: String = row.get(3)?;
        chrono::DateTime::parse_from_rfc3339(&updated_at)?;
        let bytes: Option<Vec<u8>> = row.get(4)?;
        let output_bytes: u64 = row.get(5)?;
        let output = bytes
            .map(|mut bytes| {
                match std::str::from_utf8(&bytes) {
                    Ok(_) => {}
                    Err(error)
                        if error.error_len().is_none() && output_bytes > bytes.len() as u64 =>
                    {
                        bytes.truncate(error.valid_up_to());
                    }
                    Err(error) => return Err(error.into()),
                }
                String::from_utf8(bytes).map_err(anyhow::Error::from)
            })
            .transpose()?;
        let execution_outcome = match execution_state.as_str() {
            "confirmed" => EffectOutcome::Confirmed,
            "not_started" | "skipped" => EffectOutcome::NotStarted,
            "claimed" | "running" | "possibly_applied" => EffectOutcome::PossiblyApplied,
            _ => EffectOutcome::ReconciliationRequired,
        };
        let delivery_outcome = match delivery_state.as_str() {
            "not_requested" => None,
            "not_started" => Some(EffectOutcome::NotStarted),
            "confirmed" => Some(EffectOutcome::Confirmed),
            "confirmed_failed" => Some(EffectOutcome::ConfirmedFailed),
            "partially_applied" => Some(EffectOutcome::PartiallyApplied),
            "possibly_applied" => Some(EffectOutcome::PossiblyApplied),
            _ => Some(EffectOutcome::ReconciliationRequired),
        };
        let receipt = OccurrenceReceipt {
            occurrence_id,
            execution_state,
            delivery_state,
            execution_outcome,
            delivery_outcome,
            updated_at,
            retry_allowed: false,
            output_truncated: output
                .as_ref()
                .is_some_and(|s| (s.len() as u64) < output_bytes),
            output,
            output_bytes,
        };
        page.next_before = Some(receipt.occurrence_id.clone());
        page.occurrences.push(receipt);
        // Reserve the cursor in the size check even when this is the last row.
        if serde_json::to_vec(page)?.len() > PAGE_BYTES {
            page.occurrences.pop();
            page.next_before = page.occurrences.last().map(|r| r.occurrence_id.clone());
            if page.occurrences.is_empty() {
                anyhow::bail!("occurrence exceeds bounded page");
            }
            return Ok(());
        }
        // Exact lookup has at most one row; no pagination cursor is needed.
        if query.occurrence_id.is_some() {
            page.next_before = None;
            return Ok(());
        }
    }
    if !has_more {
        page.next_before = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
