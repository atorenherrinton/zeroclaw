//! A bounded ingress queue whose send acknowledgement follows durable admission.
//! The control plane supplies the admission implementation; this is not a store.
use crate::channel::ChannelMessage;
use std::sync::Arc;
use tokio::sync::mpsc;

#[async_trait::async_trait]
pub trait Admission: Send + Sync {
    /// Persist before returning true. False is an already accepted identity.
    async fn admit(&self, message: &ChannelMessage) -> anyhow::Result<bool>;
}

#[derive(Clone)]
pub struct Sender {
    queue: mpsc::Sender<ChannelMessage>,
    admission: Option<Arc<dyn Admission>>,
    admission_serial: Arc<tokio::sync::Mutex<()>>,
}
impl From<mpsc::Sender<ChannelMessage>> for Sender {
    fn from(queue: mpsc::Sender<ChannelMessage>) -> Self {
        Self {
            queue,
            admission: None,
            admission_serial: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}
impl Sender {
    pub fn with_admission(mut self, admission: Arc<dyn Admission>) -> Self {
        self.admission = Some(admission);
        self
    }
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed()
    }
    pub async fn closed(&self) {
        self.queue.closed().await;
    }
    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }
    pub async fn send(&self, message: ChannelMessage) -> anyhow::Result<()> {
        // Reserve capacity first: no accepted message is rejected just because
        // the volatile queue is full. Cancellation after commit leaves the
        // canonical received row for recovery, never permission to replay effects.
        let _admission_order = self.admission_serial.lock().await;
        let permit = self
            .queue
            .reserve()
            .await
            .map_err(|_| anyhow::Error::msg("inbound queue closed"))?;
        if let Some(admission) = &self.admission
            && !admission.admit(&message).await?
        {
            return Ok(());
        }
        permit.send(message);
        Ok(())
    }
}

/// An unpersisted queue for tests and explicitly in-process traffic. Production
/// channel listeners attach their canonical control-plane admission handle.
pub fn channel(capacity: usize) -> (Sender, mpsc::Receiver<ChannelMessage>) {
    let (tx, rx) = mpsc::channel(capacity);
    (tx.into(), rx)
}
