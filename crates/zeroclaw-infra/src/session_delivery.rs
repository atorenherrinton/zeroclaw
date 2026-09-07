//! Delivery extension of the canonical session backend, not a second outbox.
//! The caller's session backend owns claims. Blocking SQLite access never runs
//! on an async executor worker. Unsupported backends explicitly advertise this.
use crate::session_backend::SessionBackend;
use std::sync::Arc;
use zeroclaw_api::delivery::{ChunkReceipt, DeliveryJournal};

pub struct SessionDeliveryJournal(pub Arc<dyn SessionBackend>);
#[async_trait::async_trait]
impl DeliveryJournal for SessionDeliveryJournal {
    async fn claim(&self, chunk: ChunkReceipt) -> anyhow::Result<Option<ChunkReceipt>> {
        let backend = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || backend.claim_delivery_chunk(&chunk)).await?
    }
    async fn finish(&self, chunk: ChunkReceipt) -> anyhow::Result<()> {
        let backend = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || backend.finish_delivery_chunk(&chunk)).await?
    }
}
