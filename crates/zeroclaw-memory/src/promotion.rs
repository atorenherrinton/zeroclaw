//! Native recall-weighted promotion. The memory database owns evidence and
//! promotion receipts alongside its source rows. No payload/query trace file,
//! model call, fabricated recall counter, or independent source-of-truth store.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use zeroclaw_api::ingress::TurnOrigin;
use zeroclaw_api::memory_promotion::{OWNER_RECALL_CONTEXT, OwnerRecallContext};
use zeroclaw_config::schema::{MemoryPolicyConfig, MemoryPromotionConfig};

use crate::traits::MemoryEntry;

pub const POLICY_VERSION: i64 = 1;
pub const MAX_PROMOTIONS: usize = 10;
pub const MIN_SCORE: f64 = 0.75;
pub const MAX_AGE_DAYS: f64 = 30.0;
pub const HALF_LIFE_DAYS: f64 = 14.0;
// Byte-pair tokenizers cannot produce more text tokens than UTF-8 input bytes.
// Accept whole notes only: this conservative bound never cuts a qualifier or
// silently truncates the source (and needs no provider-specific tokenizer).
pub const MAX_SNIPPET_BYTES: usize = 160;

/// Metadata only. Never include source keys, text, queries, or agent ids here.
#[derive(Default, Serialize)]
pub struct PromotionReport {
    pub policy_version: i64,
    pub considered: usize,
    pub eligible: usize,
    pub promoted: usize,
    pub already_durable: usize,
    pub dry_run: bool,
}

pub fn normalize_query(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn valid_input(value: &str) -> bool {
    let normalized = normalize_query(value);
    !normalized.is_empty()
        && normalized.chars().any(char::is_alphanumeric)
        // Telegram forwards/documents are external, and voice/audio transport
        // does not prove the recorded speaker. Keep ordinary chat unchanged but
        // admit only typed owner text to automatic promotion provenance.
        && !normalized.starts_with("[forwarded from")
        && !normalized.starts_with("[document:")
        && !normalized.starts_with("[voice]")
        && ![
            "<tool",
            "[memory context]",
            "[cron:",
            "[heartbeat",
            "[image:",
            "call_screening",
            "conversation summary:",
            "session id",
            "```",
            "\n>",
            "forwarded message",
            "begin forwarded",
            "untrusted",
            "transcript:",
            "user:",
            "assistant:",
            "caller:",
        ]
        .iter()
        .any(|marker| value.to_lowercase().contains(marker))
        && !value.trim_start().starts_with('>')
}

/// The engine supplies real origin, resolved channel and the original input.
/// Channel admission is operator-configured, not the placeholder ingress trust.
pub fn owner_context(
    policy: &MemoryPromotionConfig,
    agent_alias: Option<&str>,
    origin: TurnOrigin,
    channel: &str,
    turn_id: &str,
    input: Option<&str>,
) -> Option<OwnerRecallContext> {
    let alias = agent_alias?;
    let input = input?;
    if !policy.enabled
        || !policy.agent_aliases.iter().any(|a| a == alias)
        || turn_id.is_empty()
        || input.len() > 32_768
        || !valid_input(input)
        || !(matches!(origin, TurnOrigin::Interactive)
            || matches!(origin, TurnOrigin::Channel)
                && policy.owner_channels.iter().any(|name| name == channel))
    {
        return None;
    }
    Some(OwnerRecallContext {
        agent_alias: alias.to_owned(),
        turn_id: turn_id.to_owned(),
        owner_input: input.to_owned(),
        tool_output_limit: 0,
    })
}

pub(crate) fn current_context() -> Option<OwnerRecallContext> {
    OWNER_RECALL_CONTEXT.try_with(Clone::clone).ok().flatten()
}

fn digest(domain: &str, parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_promotion_meta (
            key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS memory_promotion_sources (
            memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            version TEXT NOT NULL, agent_id TEXT NOT NULL,
            provenance TEXT NOT NULL CHECK(provenance='owner_input_excerpt'),
            attested_at TEXT NOT NULL, PRIMARY KEY(memory_id,version));
         CREATE TABLE IF NOT EXISTS memory_recall_evidence (
            memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
            version TEXT NOT NULL, agent_id TEXT NOT NULL,
            event_hash TEXT NOT NULL, query_hash TEXT NOT NULL,
            recalled_at TEXT NOT NULL, score REAL NOT NULL,
            PRIMARY KEY(memory_id,version,event_hash));
         CREATE INDEX IF NOT EXISTS memory_recall_evidence_owner
            ON memory_recall_evidence(agent_id,memory_id,version);
         CREATE TABLE IF NOT EXISTS memory_promotion_receipts (
            memory_id TEXT NOT NULL, version TEXT NOT NULL, agent_id TEXT NOT NULL,
            promoted_id TEXT NOT NULL, promoted_at TEXT NOT NULL,
            policy_version INTEGER NOT NULL, score REAL NOT NULL,
            PRIMARY KEY(memory_id,version));",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_promotion_meta VALUES ('policy_version',?1)",
        [POLICY_VERSION.to_string()],
    )?;
    let version: String = conn.query_row(
        "SELECT value FROM memory_promotion_meta WHERE key='policy_version'",
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        version == POLICY_VERSION.to_string(),
        "unsupported memory promotion schema version"
    );
    // A per-install random salt prevents public dictionary hashes of private
    // queries and prevents correlating the same query across installations.
    conn.execute(
        "INSERT OR IGNORE INTO memory_promotion_meta VALUES ('query_salt',?1)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    Ok(())
}

struct Source {
    id: String,
    key: String,
    content: String,
    agent_id: String,
    category: String,
    namespace: String,
    created_at: String,
    updated_at: String,
    session_id: Option<String>,
    tenant_id: Option<String>,
    superseded_by: Option<String>,
}

impl Source {
    fn version(&self) -> String {
        digest(
            "zeroclaw-promotion-source-v1",
            &[
                &self.id,
                &self.agent_id,
                &self.key,
                &self.content,
                &self.category,
                &self.namespace,
                &self.created_at,
                &self.updated_at,
                self.session_id.as_deref().unwrap_or(""),
                self.tenant_id.as_deref().unwrap_or(""),
            ],
        )
    }

    fn eligible_source(&self) -> bool {
        self.category == "daily"
            && self.namespace == "default"
            && self.session_id.is_none()
            && self.tenant_id.is_none()
            && self.superseded_by.is_none()
            && valid_input(&self.content)
            && !self.content.is_empty()
            && self.content.len() <= MAX_SNIPPET_BYTES
            && !crate::is_user_autosave_key(&self.key)
            && !crate::is_assistant_autosave_key(&self.key)
            && !self.key.to_lowercase().ends_with("_history")
            && ![
                "call/",
                "call_screening",
                "external",
                "tool_output",
                "consolidat",
                "promotion/",
                "migration/",
                "import/",
            ]
            .iter()
            .any(|prefix| self.key.to_lowercase().starts_with(prefix))
    }
}

fn load_source(conn: &Connection, agent_id: &str, key: &str) -> Result<Option<Source>> {
    Ok(conn.query_row(
        "SELECT id,key,content,agent_id,category,COALESCE(namespace,'default'),created_at,updated_at,
            session_id,tenant_id,superseded_by FROM memories WHERE agent_id=?1 AND key=?2",
        params![agent_id,key], |r| Ok(Source {
            id:r.get(0)?,key:r.get(1)?,content:r.get(2)?,agent_id:r.get(3)?,category:r.get(4)?,
            namespace:r.get(5)?,created_at:r.get(6)?,updated_at:r.get(7)?,session_id:r.get(8)?,
            tenant_id:r.get(9)?,superseded_by:r.get(10)?,
        })).optional()?)
}

fn context_matches(
    conn: &Connection,
    agent_id: &str,
    ctx: &OwnerRecallContext,
    policy: &MemoryPromotionConfig,
) -> Result<bool> {
    if !policy.enabled || !policy.agent_aliases.contains(&ctx.agent_alias) {
        return Ok(false);
    }
    Ok(conn
        .query_row("SELECT alias FROM agents WHERE id=?1", [agent_id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .is_some_and(|alias| alias == ctx.agent_alias))
}

pub(crate) fn attest(
    conn: &mut Connection,
    agent_id: &str,
    key: &str,
    content: &str,
    ctx: &OwnerRecallContext,
    policy: &MemoryPromotionConfig,
    now: DateTime<Utc>,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !context_matches(&tx, agent_id, ctx, policy)? {
        return Ok(());
    }
    let Some(source) = load_source(&tx, agent_id, key)? else {
        return Ok(());
    };
    let input = normalize_query(&ctx.owner_input);
    let input = [
        "please remember: ",
        "remember: ",
        "please remember that ",
        "remember that ",
    ]
    .iter()
    .find_map(|prefix| input.strip_prefix(prefix))
    .unwrap_or(&input);
    let excerpt = normalize_query(content);
    // Whole, literal owner-input excerpts only. A memory tool invocation by
    // itself must not upgrade an external/model assertion into an owner fact.
    if source.content != content
        || !source.eligible_source()
        || excerpt.len() < 8
        || input != excerpt
    {
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO memory_promotion_sources VALUES (?1,?2,?3,'owner_input_excerpt',?4)",
        params![source.id, source.version(), agent_id, now.to_rfc3339()],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn record(
    conn: &mut Connection,
    agent_id: &str,
    entries: &[MemoryEntry],
    ctx: &OwnerRecallContext,
    policy: &MemoryPromotionConfig,
    now: DateTime<Utc>,
) -> Result<()> {
    if entries.is_empty() || !valid_input(&ctx.owner_input) {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !context_matches(&tx, agent_id, ctx, policy)? {
        return Ok(());
    }
    let salt: String = tx.query_row(
        "SELECT value FROM memory_promotion_meta WHERE key='query_salt'",
        [],
        |r| r.get(0),
    )?;
    let query_hash = digest(
        "zeroclaw-owner-query-v1",
        &[&salt, agent_id, &normalize_query(&ctx.owner_input)],
    );
    let event_hash = digest("zeroclaw-recall-turn-v1", &[&salt, agent_id, &ctx.turn_id]);
    for entry in entries.iter().take(100) {
        if entry.agent_id.as_deref() != Some(agent_id) {
            continue;
        }
        let Some(source) = load_source(&tx, agent_id, &entry.key)? else {
            continue;
        };
        if source.id != entry.id || source.content != entry.content || !source.eligible_source() {
            continue;
        }
        let version = source.version();
        let attested: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_promotion_sources WHERE memory_id=?1 AND version=?2 AND agent_id=?3)",
            params![source.id,version,agent_id], |r|r.get(0))?;
        if !attested {
            continue;
        }
        // No synthetic perfect relevance for unscored retrieval.
        let score = entry
            .score
            .filter(|s| s.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        tx.execute(
            "INSERT OR IGNORE INTO memory_recall_evidence VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                source.id,
                version,
                agent_id,
                event_hash,
                query_hash,
                now.to_rfc3339(),
                score
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Default)]
struct Signals {
    count: usize,
    total: f64,
    queries: HashSet<String>,
    days: HashSet<NaiveDate>,
    last: Option<DateTime<Utc>>,
}

fn weighted_score(signals: &Signals, now: DateTime<Utc>) -> Option<f64> {
    if signals.count < 3 || signals.queries.len() < 3 {
        return None;
    }
    let last = signals.last?;
    if last > now {
        return None;
    }
    let age = (now - last).num_seconds() as f64 / 86_400.0;
    if age > MAX_AGE_DAYS {
        return None;
    }
    let frequency = ((signals.count as f64).ln_1p() / 10.0_f64.ln_1p()).clamp(0.0, 1.0);
    let relevance = (signals.total / signals.count as f64).clamp(0.0, 1.0);
    let diversity = (signals.queries.len() as f64 / 5.0).clamp(0.0, 1.0);
    let consolidation = match (signals.days.iter().min(), signals.days.iter().max()) {
        (Some(first), Some(last)) if signals.days.len() > 1 => {
            0.55 * (((signals.days.len() - 1) as f64).ln_1p() / 4.0_f64.ln_1p()).clamp(0.0, 1.0)
                + 0.45
                    * ((last.signed_duration_since(*first).num_days() as f64) / 7.0).clamp(0.0, 1.0)
        }
        (Some(_), Some(_)) => 0.2,
        _ => 0.0,
    };
    // Original six component weights. No invented concept/phase/grounded
    // evidence: those components remain zero until a real native source exists.
    Some(
        0.24 * frequency
            + 0.30 * relevance
            + 0.15 * diversity
            + 0.15 * 2.0_f64.powf(-age / HALF_LIFE_DAYS)
            + 0.10 * consolidation,
    )
}

fn write_policy_allows(
    conn: &Connection,
    policy: &MemoryPolicyConfig,
    content: &str,
) -> Result<bool> {
    if policy.read_only_namespaces.iter().any(|ns| ns == "default") {
        return Ok(false);
    }
    // Autonomous promotion is always fail-closed on the strict content scan,
    // including installations whose ordinary memory scan is off.
    if !crate::threat::scan(content, crate::threat::Scope::Strict).is_empty() {
        return Ok(false);
    }
    if policy.redact_on_write {
        let mut categories = Vec::new();
        for name in &policy.redact_categories {
            categories.push(
                crate::redact::RedactCategory::from_config(name)
                    .context("invalid memory redaction category")?,
            );
        }
        if crate::redact::redact(content, &categories).0 != content {
            return Ok(false);
        }
    }
    for (cap, sql) in [
        (
            policy.max_entries_per_namespace,
            "SELECT COUNT(*) FROM memories WHERE namespace='default' AND superseded_by IS NULL",
        ),
        (
            policy.max_entries_per_category,
            "SELECT COUNT(*) FROM memories WHERE category='core' AND superseded_by IS NULL",
        ),
    ] {
        if cap > 0 && conn.query_row(sql, [], |r| r.get::<_, usize>(0))? >= cap {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn promote(
    conn: &mut Connection,
    alias: &str,
    policy: &MemoryPromotionConfig,
    memory_policy: &MemoryPolicyConfig,
    now: DateTime<Utc>,
    dry_run: bool,
) -> Result<PromotionReport> {
    anyhow::ensure!(
        policy.enabled && policy.agent_aliases.iter().any(|a| a == alias),
        "memory promotion agent not enabled"
    );
    init_schema(conn)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let agent_id: String = tx
        .query_row("SELECT id FROM agents WHERE alias=?1", [alias], |r| {
            r.get(0)
        })
        .context("promotion agent has no native memory identity")?;
    let keys: Vec<String> = {
        let mut stmt = tx.prepare("SELECT DISTINCT m.key FROM memories m JOIN memory_promotion_sources s ON s.memory_id=m.id
            WHERE m.agent_id=?1 AND s.agent_id=?1 ORDER BY m.key LIMIT 10001")?;
        stmt.query_map([&agent_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    anyhow::ensure!(
        keys.len() <= 10_000,
        "promotion source bound exceeded; requires operator review"
    );
    let mut report = PromotionReport {
        policy_version: POLICY_VERSION,
        dry_run,
        ..Default::default()
    };
    let mut candidates = Vec::new();
    for key in keys {
        let Some(source) = load_source(&tx, &agent_id, &key)? else {
            continue;
        };
        if !source.eligible_source() {
            continue;
        }
        let version = source.version();
        let attested: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_promotion_sources WHERE memory_id=?1 AND version=?2 AND agent_id=?3)",
            params![source.id,version,agent_id],|r|r.get(0))?;
        let done: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_promotion_receipts WHERE memory_id=?1 AND version=?2)",
            params![source.id,version],|r|r.get(0))?;
        if !attested || done {
            continue;
        }
        report.considered += 1;
        let mut signals = Signals::default();
        let mut stmt = tx.prepare("SELECT query_hash,recalled_at,score FROM memory_recall_evidence WHERE memory_id=?1 AND version=?2 AND agent_id=?3")?;
        let rows = stmt.query_map(params![source.id, version, agent_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?;
        for row in rows {
            let (query, at, score) = row?;
            let at = DateTime::parse_from_rfc3339(&at)?.with_timezone(&Utc);
            if at > now || !score.is_finite() {
                continue;
            }
            signals.count += 1;
            signals.total += score.clamp(0.0, 1.0);
            signals.queries.insert(query);
            signals.days.insert(at.date_naive());
            signals.last = Some(signals.last.map_or(at, |last| last.max(at)));
        }
        if let Some(score) = weighted_score(&signals, now)
            && score >= MIN_SCORE
            && write_policy_allows(&tx, memory_policy, &source.content)?
        {
            candidates.push((score, source, version));
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    report.eligible = candidates.len();
    for (score, source, version) in candidates.into_iter().take(MAX_PROMOTIONS) {
        if dry_run {
            continue;
        }
        if !write_policy_allows(&tx, memory_policy, &source.content)? {
            continue;
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM memories WHERE agent_id=?1 AND content=?2 AND category='core'
                AND namespace='default' AND session_id IS NULL AND tenant_id IS NULL
                AND superseded_by IS NULL ORDER BY id LIMIT 1",
                params![agent_id, source.content],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.execute(
                "INSERT INTO memory_promotion_receipts VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    source.id,
                    version,
                    agent_id,
                    id,
                    now.to_rfc3339(),
                    POLICY_VERSION,
                    score
                ],
            )?;
            report.already_durable += 1;
            continue;
        }
        let key = format!(
            "promotion/v1/{}",
            digest("zeroclaw-promotion-id-v1", &[&source.id, &version])
        );
        let id = uuid::Uuid::new_v4().to_string();
        // Refuse an existing conflicting key; never overwrite a user's entry.
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE agent_id=?1 AND key=?2)",
            params![agent_id, key],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            !exists,
            "promotion destination conflicts with existing memory"
        );
        tx.execute("INSERT INTO memories(id,key,content,category,created_at,updated_at,namespace,importance,agent_id)
            VALUES (?1,?2,?3,'core',?4,?4,'default',?5,?6)",params![id,key,source.content,now.to_rfc3339(),score,agent_id])?;
        tx.execute(
            "INSERT INTO memory_promotion_receipts VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                source.id,
                version,
                agent_id,
                id,
                now.to_rfc3339(),
                POLICY_VERSION,
                score
            ],
        )?;
        report.promoted += 1;
    }
    // FTS triggers update within this same commit; original Daily rows and all
    // session/history data remain untouched. No embedding/provider call needed.
    tx.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentScopedMemory, Memory, MemoryCategory, SqliteMemory};
    use std::sync::Arc;
    use tempfile::TempDir;

    const NOTE: &str = "I prefer Rust, SQLite, privacy, local tools and short notes.";

    fn policy() -> MemoryPromotionConfig {
        MemoryPromotionConfig {
            enabled: true,
            agent_aliases: vec!["owner".into()],
            owner_channels: vec!["telegram.test".into()],
        }
    }
    fn context(input: &str, turn: &str) -> OwnerRecallContext {
        owner_context(
            &policy(),
            Some("owner"),
            TurnOrigin::Interactive,
            "cli",
            turn,
            Some(input),
        )
        .unwrap()
    }
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-03T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    async fn fixture() -> (TempDir, SqliteMemory, String, Connection) {
        let dir = TempDir::new().unwrap();
        let memory = SqliteMemory::new("test", dir.path())
            .unwrap()
            .with_promotion_config(&policy(), &MemoryPolicyConfig::default())
            .unwrap();
        let id = memory.ensure_agent_uuid("owner").await.unwrap();
        let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
        (dir, memory, id, conn)
    }
    async fn note(
        memory: &SqliteMemory,
        conn: &mut Connection,
        id: &str,
        key: &str,
        content: &str,
    ) {
        memory
            .store_with_agent(
                key,
                content,
                MemoryCategory::Daily,
                None,
                None,
                None,
                Some(id),
            )
            .await
            .unwrap();
        attest(
            conn,
            id,
            key,
            content,
            &context(content, "write"),
            &policy(),
            now(),
        )
        .unwrap();
    }
    async fn evidence(memory: &SqliteMemory, conn: &mut Connection, id: &str, key: &str) {
        let entry = memory.get_for_agent(key, id).await.unwrap().unwrap();
        // Unit-level scoring fixture; boundary tests below use real BM25 recall.
        for n in 0..10 {
            let mut result = entry.clone();
            result.score = Some(1.0);
            record(
                conn,
                id,
                &[result],
                &context(&format!("query {n}"), &format!("turn {n}")),
                &policy(),
                now(),
            )
            .unwrap();
        }
    }
    fn count(conn: &Connection, table: &str) -> u64 {
        // Test-owned fixed table names only.
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn owner_origin_and_query_normalization_fail_closed() {
        assert_eq!(
            normalize_query("  Rust\t SQLITE\n"),
            normalize_query("rust sqlite")
        );
        for input in [
            "",
            "  \n ",
            "*",
            "...",
            "<tool_result>stuff",
            "caller: hello",
            "> quoted",
            "Forwarded message data",
        ] {
            assert!(
                owner_context(
                    &policy(),
                    Some("owner"),
                    TurnOrigin::Interactive,
                    "cli",
                    "t",
                    Some(input)
                )
                .is_none()
            );
        }
        for origin in [
            TurnOrigin::Cron,
            TurnOrigin::Daemon,
            TurnOrigin::SubTurn,
            TurnOrigin::AgentDirect,
        ] {
            assert!(
                owner_context(
                    &policy(),
                    Some("owner"),
                    origin,
                    "telegram.test",
                    "t",
                    Some(NOTE)
                )
                .is_none()
            );
        }
        assert!(
            owner_context(
                &policy(),
                Some("peer"),
                TurnOrigin::Interactive,
                "cli",
                "t",
                Some(NOTE)
            )
            .is_none()
        );
        assert!(
            owner_context(
                &policy(),
                Some("owner"),
                TurnOrigin::Channel,
                "telegram.other",
                "t",
                Some(NOTE)
            )
            .is_none()
        );
        assert!(
            owner_context(
                &policy(),
                Some("owner"),
                TurnOrigin::Channel,
                "telegram.test",
                "t",
                Some(NOTE)
            )
            .is_some()
        );
    }

    #[tokio::test]
    async fn native_telegram_external_decorators_never_attest_owner_source() {
        let (_dir, memory, id, mut conn) = fixture().await;
        for (n, input) in [
            "[Forwarded from Example Sender] I prefer Rust.",
            " \n\t[FoRwArDeD from Example Sender] I prefer Rust.",
            "[Document: example.txt] /synthetic/example.txt",
            " \t[DOCUMENT: example.txt] /synthetic/example.txt",
            "[Voice] I prefer Rust.",
            " \n[VoIcE] I prefer Rust.",
        ]
        .iter()
        .enumerate()
        {
            assert!(
                owner_context(
                    &policy(),
                    Some("owner"),
                    TurnOrigin::Channel,
                    "telegram.test",
                    "turn",
                    Some(input)
                )
                .is_none()
            );
            let key = format!("forward-{n}");
            memory
                .store_with_agent(
                    &key,
                    input,
                    MemoryCategory::Daily,
                    None,
                    None,
                    None,
                    Some(&id),
                )
                .await
                .unwrap();
            // Also test the backend source guard even if a future caller passed
            // a manually constructed context instead of the admission helper.
            let mut invalid_context = context(NOTE, "turn");
            invalid_context.owner_input = (*input).to_owned();
            attest(
                &mut conn,
                &id,
                &key,
                input,
                &invalid_context,
                &policy(),
                now(),
            )
            .unwrap();
        }
        assert_eq!(count(&conn, "memory_promotion_sources"), 0);
    }

    fn signals(n: usize) -> Signals {
        Signals {
            count: n,
            total: n as f64,
            queries: (0..n).map(|n| format!("q{n}")).collect(),
            days: [now().date_naive()].into_iter().collect(),
            last: Some(now()),
        }
    }
    #[test]
    fn weighted_threshold_and_real_distinct_query_minimum() {
        assert!(weighted_score(&signals(2), now()).is_none());
        assert!(weighted_score(&signals(3), now()).unwrap() < MIN_SCORE);
        assert!(weighted_score(&signals(5), now()).unwrap() >= MIN_SCORE);
        let mut s = signals(10);
        s.queries = ["same".into()].into_iter().collect();
        assert!(weighted_score(&s, now()).is_none());
        let mut s = signals(10);
        s.total = 0.0;
        assert!(weighted_score(&s, now()).unwrap() < MIN_SCORE);
    }
    #[test]
    fn recency_half_life_age_and_future_bounds() {
        let s = signals(10);
        let fresh = weighted_score(&s, now()).unwrap();
        let old = weighted_score(&s, now() + chrono::Duration::days(14)).unwrap();
        assert!((fresh - old - 0.075).abs() < 1e-10);
        assert!(weighted_score(&s, now() + chrono::Duration::days(30)).is_some());
        assert!(
            weighted_score(
                &s,
                now() + chrono::Duration::days(30) + chrono::Duration::seconds(1)
            )
            .is_none()
        );
        assert!(weighted_score(&s, now() - chrono::Duration::seconds(1)).is_none());
    }

    #[test]
    fn fresh_recall_reactivates_lifetime_version_scoped_signals() {
        let mut s = signals(5);
        let later = now() + chrono::Duration::days(45);
        assert!(weighted_score(&s, later).is_none());

        // Old real exposures still count for this exact unchanged source version.
        // This new exposure repeats a prior query, so it adds no fake diversity.
        s.count += 1;
        s.total += 1.0;
        s.last = Some(later);
        s.days.insert(later.date_naive());
        assert_eq!(s.queries.len(), 5);
        assert!(weighted_score(&s, later).unwrap() >= MIN_SCORE);
        assert!(weighted_score(&s, later + chrono::Duration::days(31)).is_none());
    }

    #[tokio::test]
    async fn promotion_preserves_source_is_atomic_idempotent_and_dry_run_safe() {
        let (_dir, memory, id, mut conn) = fixture().await;
        note(&memory, &mut conn, &id, "preference", NOTE).await;
        evidence(&memory, &mut conn, &id, "preference").await;
        let before = load_source(&conn, &id, "preference")
            .unwrap()
            .unwrap()
            .version();
        let report = promote(
            &mut conn,
            "owner",
            &policy(),
            &MemoryPolicyConfig::default(),
            now(),
            true,
        )
        .unwrap();
        assert_eq!(report.eligible, 1);
        assert_eq!(report.promoted, 0);
        assert_eq!(count(&conn, "memories"), 1);
        assert_eq!(
            promote(
                &mut conn,
                "owner",
                &policy(),
                &MemoryPolicyConfig::default(),
                now(),
                false
            )
            .unwrap()
            .promoted,
            1
        );
        assert_eq!(
            promote(
                &mut conn,
                "owner",
                &policy(),
                &MemoryPolicyConfig::default(),
                now(),
                false
            )
            .unwrap()
            .promoted,
            0
        );
        assert_eq!(
            load_source(&conn, &id, "preference")
                .unwrap()
                .unwrap()
                .version(),
            before
        );
        assert_eq!(count(&conn, "memory_promotion_receipts"), 1);
        let core = memory
            .list(Some(&MemoryCategory::Core), None)
            .await
            .unwrap();
        assert_eq!(core.len(), 1);
        assert_eq!(core[0].content, NOTE);
        assert_eq!(core[0].agent_id.as_deref(), Some(id.as_str()));
        assert!(
            !memory
                .recall("privacy", 10, None, None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn source_version_change_discards_prior_recall_authority() {
        let (_dir, memory, id, mut conn) = fixture().await;
        note(&memory, &mut conn, &id, "preference", NOTE).await;
        evidence(&memory, &mut conn, &id, "preference").await;
        note(
            &memory,
            &mut conn,
            &id,
            "preference",
            "I prefer slower careful work.",
        )
        .await;
        assert_eq!(
            promote(
                &mut conn,
                "owner",
                &policy(),
                &MemoryPolicyConfig::default(),
                now(),
                false
            )
            .unwrap()
            .promoted,
            0
        );
        assert_eq!(
            count(&conn, "memory_recall_evidence"),
            10,
            "history preserved, not reused"
        );
    }

    #[tokio::test]
    async fn imported_unattested_and_external_namespaces_do_not_qualify() {
        let (_dir, memory, id, mut conn) = fixture().await;
        for (key, category, namespace) in [
            ("imported", MemoryCategory::Daily, None),
            ("call/fixture", MemoryCategory::Daily, None),
            ("conversation", MemoryCategory::Conversation, None),
            ("external", MemoryCategory::Daily, Some("call_screening")),
            ("tool", MemoryCategory::Daily, Some("external_tool_output")),
        ] {
            memory
                .store_with_agent(key, NOTE, category, None, namespace, None, Some(&id))
                .await
                .unwrap();
            if key != "imported" {
                attest(
                    &mut conn,
                    &id,
                    key,
                    NOTE,
                    &context(NOTE, "write"),
                    &policy(),
                    now(),
                )
                .unwrap();
            }
            evidence(&memory, &mut conn, &id, key).await;
        }
        assert_eq!(count(&conn, "memory_promotion_sources"), 0);
        assert_eq!(count(&conn, "memory_recall_evidence"), 0);
    }

    #[tokio::test]
    async fn maximum_ten_and_non_destructive_snippet_limit() {
        let (_dir, memory, id, mut conn) = fixture().await;
        for n in 0..11 {
            let key = format!("note{n}");
            note(
                &memory,
                &mut conn,
                &id,
                &key,
                &format!("{NOTE} Reference {n}."),
            )
            .await;
            evidence(&memory, &mut conn, &id, &key).await;
        }
        let long = "é".repeat(81);
        note(&memory, &mut conn, &id, "long", &long).await;
        evidence(&memory, &mut conn, &id, "long").await;
        assert_eq!(
            promote(
                &mut conn,
                "owner",
                &policy(),
                &MemoryPolicyConfig::default(),
                now(),
                false
            )
            .unwrap()
            .promoted,
            10
        );
        assert_eq!(
            memory
                .get_for_agent("long", &id)
                .await
                .unwrap()
                .unwrap()
                .content,
            long
        );
        assert_eq!(count(&conn, "memories"), 22);
    }

    #[tokio::test]
    async fn exact_owner_statement_cannot_omit_negation_or_quote_context() {
        let (_dir, memory, id, mut conn) = fixture().await;
        memory
            .store_with_agent(
                "note",
                NOTE,
                MemoryCategory::Daily,
                None,
                None,
                None,
                Some(&id),
            )
            .await
            .unwrap();
        attest(
            &mut conn,
            &id,
            "note",
            NOTE,
            &context(&format!("Do not assume this: {NOTE}"), "t"),
            &policy(),
            now(),
        )
        .unwrap();
        assert_eq!(count(&conn, "memory_promotion_sources"), 0);
        attest(
            &mut conn,
            &id,
            "note",
            NOTE,
            &context(&format!("Remember: {NOTE}"), "t"),
            &policy(),
            now(),
        )
        .unwrap();
        assert_eq!(count(&conn, "memory_promotion_sources"), 1);
    }

    #[tokio::test]
    async fn salted_hashes_turn_dedup_and_whitespace_diversity() {
        let (_dir, memory, id, mut conn) = fixture().await;
        note(&memory, &mut conn, &id, "note", NOTE).await;
        let mut entry = memory.get_for_agent("note", &id).await.unwrap().unwrap();
        entry.score = Some(1.0);
        for (turn, query) in [
            ("t1", "Private Query"),
            ("t1", "different private query"),
            ("t2", " private\t QUERY "),
            ("t3", "PRIVATE QUERY"),
        ] {
            record(
                &mut conn,
                &id,
                &[entry.clone()],
                &context(query, turn),
                &policy(),
                now(),
            )
            .unwrap();
        }
        assert_eq!(count(&conn, "memory_recall_evidence"), 3);
        let hashes: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT DISTINCT query_hash FROM memory_recall_evidence")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].len(), 64);
        assert!(!hashes[0].contains("private"));
        assert_ne!(
            digest("zeroclaw-owner-query-v1", &["salt1", &id, "private query"]),
            digest("zeroclaw-owner-query-v1", &["salt2", &id, "private query"])
        );
        assert_eq!(
            promote(
                &mut conn,
                "owner",
                &policy(),
                &MemoryPolicyConfig::default(),
                now(),
                false
            )
            .unwrap()
            .promoted,
            0
        );
    }

    #[tokio::test]
    async fn scoped_wrapper_rejects_peers_and_absent_context() {
        let (_dir, memory, id, conn) = fixture().await;
        let peer = memory.ensure_agent_uuid("peer").await.unwrap();
        let scoped = AgentScopedMemory::new(Arc::new(memory.clone()), &id, vec![peer.clone()]);
        scoped
            .store("note", NOTE, MemoryCategory::Daily, None)
            .await
            .unwrap();
        scoped
            .attest_explicit_note(None, "note", NOTE)
            .await
            .unwrap();
        assert_eq!(count(&conn, "memory_promotion_sources"), 0);
        OWNER_RECALL_CONTEXT
            .scope(Some(context(NOTE, "write")), async {
                assert!(
                    scoped
                        .attest_explicit_note(Some(&peer), "note", NOTE)
                        .await
                        .is_err()
                );
                scoped
                    .attest_explicit_note(None, "note", NOTE)
                    .await
                    .unwrap();
            })
            .await;
        assert_eq!(count(&conn, "memory_promotion_sources"), 1);
        memory
            .store_with_agent(
                "peer-note",
                NOTE,
                MemoryCategory::Daily,
                None,
                None,
                None,
                Some(&peer),
            )
            .await
            .unwrap();
        let entry = memory
            .get_for_agent("peer-note", &peer)
            .await
            .unwrap()
            .unwrap();
        OWNER_RECALL_CONTEXT
            .scope(
                Some(context("query", "read")),
                scoped.record_recall_evidence(None, &[entry]),
            )
            .await
            .unwrap();
        assert_eq!(count(&conn, "memory_recall_evidence"), 0);
    }

    #[tokio::test]
    async fn unsupported_version_and_readonly_policy_fail_closed() {
        let (_dir, memory, id, mut conn) = fixture().await;
        note(&memory, &mut conn, &id, "note", NOTE).await;
        evidence(&memory, &mut conn, &id, "note").await;
        let blocked = MemoryPolicyConfig {
            read_only_namespaces: vec!["default".into()],
            ..Default::default()
        };
        assert_eq!(
            promote(&mut conn, "owner", &policy(), &blocked, now(), false)
                .unwrap()
                .promoted,
            0
        );
        conn.execute(
            "UPDATE memory_promotion_meta SET value='999' WHERE key='policy_version'",
            [],
        )
        .unwrap();
        assert!(init_schema(&conn).is_err());
        assert!(
            promote(
                &mut conn,
                "owner",
                &policy(),
                &MemoryPolicyConfig::default(),
                now(),
                false
            )
            .is_err()
        );
        assert_eq!(count(&conn, "memories"), 1);
    }

    #[tokio::test]
    async fn already_durable_content_is_not_duplicated() {
        let (_dir, memory, id, mut conn) = fixture().await;
        note(&memory, &mut conn, &id, "daily-note", NOTE).await;
        evidence(&memory, &mut conn, &id, "daily-note").await;
        memory
            .store_with_agent(
                "existing-core",
                NOTE,
                MemoryCategory::Core,
                None,
                None,
                None,
                Some(&id),
            )
            .await
            .unwrap();
        let report = promote(
            &mut conn,
            "owner",
            &policy(),
            &MemoryPolicyConfig::default(),
            now(),
            false,
        )
        .unwrap();
        assert_eq!(report.promoted, 0);
        assert_eq!(report.already_durable, 1);
        assert_eq!(count(&conn, "memories"), 2);
        assert_eq!(count(&conn, "memory_promotion_receipts"), 1);
    }

    #[test]
    fn default_disabled_does_not_create_evidence_tables() {
        let dir = TempDir::new().unwrap();
        let _memory = SqliteMemory::new("test", dir.path()).unwrap();
        let conn = Connection::open(dir.path().join("memory/brain.db")).unwrap();
        let tables:u64=conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'memory_promotion_%' OR name='memory_recall_evidence'",[],|r|r.get(0)).unwrap();
        assert_eq!(tables, 0);
    }
}
