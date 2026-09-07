//! Channel delivery evidence, separate from response generation. No payloads or
//! credentials belong in these records. A lost acknowledgement is not failure
//! evidence and must never authorize replay of an external write.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectOutcome {
    NotStarted,
    Confirmed,
    ConfirmedFailed,
    PartiallyApplied,
    PossiblyApplied,
    ReconciliationRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkReceipt {
    pub key: String,
    pub response_key: String,
    pub chunk_index: usize,
    pub total_chunks: usize,
    pub outcome: EffectOutcome,
    pub platform_message_id: Option<String>,
}

/// Returned through anyhow without flattening the evidence to an error string.
/// Even a confirmed failure cannot authorize resending previously confirmed chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryFailure {
    pub outcome: EffectOutcome,
    pub chunk_index: usize,
    pub total_chunks: usize,
    pub confirmed_chunks: usize,
}
impl std::fmt::Display for DeliveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delivery {:?}: chunk {}/{}, {} acknowledged; preserve visible content, reconcile before retry",
            self.outcome,
            self.chunk_index + 1,
            self.total_chunks,
            self.confirmed_chunks
        )
    }
}
impl std::error::Error for DeliveryFailure {}

#[async_trait]
pub trait DeliveryJournal: Send + Sync {
    /// Commit a possibly-applied claim BEFORE network I/O. Returns an existing
    /// receipt on duplicate admission; callers may skip confirmed chunks only.
    async fn claim(&self, chunk: ChunkReceipt) -> anyhow::Result<Option<ChunkReceipt>>;
    async fn finish(&self, chunk: ChunkReceipt) -> anyhow::Result<()>;
}

tokio::task_local! {
    pub static JOURNAL: Option<Arc<dyn DeliveryJournal>>;
}
pub fn current_journal() -> Option<Arc<dyn DeliveryJournal>> {
    JOURNAL.try_with(Clone::clone).ok().flatten()
}

/// Ephemeral projection of the canonical chunk receipts for the current send.
/// Never authorizes replay; absence means the adapter supplies no confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliverySummary {
    pub outcome: EffectOutcome,
    pub confirmed_chunks: usize,
    pub total_chunks: usize,
}
impl DeliverySummary {
    /// Unit/HTTP success and empty or inconsistent summaries cannot confirm a
    /// replacement. Adapters create this projection from positive chunk receipts.
    pub fn is_fully_confirmed(&self) -> bool {
        self.outcome == EffectOutcome::Confirmed
            && self.total_chunks > 0
            && self.confirmed_chunks == self.total_chunks
    }
}

tokio::task_local! {
    pub static SUMMARY: std::sync::Mutex<Option<DeliverySummary>>;
}
pub fn record_summary(summary: DeliverySummary) {
    let _ = SUMMARY.try_with(|slot| {
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(summary);
        }
    });
}
pub fn take_summary() -> Option<DeliverySummary> {
    SUMMARY
        .try_with(|slot| slot.lock().ok().and_then(|mut s| s.take()))
        .ok()
        .flatten()
}
