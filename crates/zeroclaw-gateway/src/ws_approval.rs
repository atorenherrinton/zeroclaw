//! WebSocket-backed [`Channel`] implementation that surfaces tool approval
//! prompts to the gateway client and waits for the operator's decision.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_api::channel::{
    ApprovalSource, AttributedApprovalResponse, Channel, ChannelApprovalRequest,
    ChannelApprovalResponse, SendMessage,
};

/// Shared map keyed by `request_id`. Consumed by the receive loop to resolve
/// the oneshot when an `approval_response` frame arrives.
pub type PendingApprovals = Arc<Mutex<HashMap<String, PendingApproval>>>;

/// A pending responder plus its exact registration identity. The receive loop
/// retains its existing remove-and-send interface.
pub struct PendingApproval {
    registration_id: Uuid,
    sender: oneshot::Sender<ChannelApprovalResponse>,
}

impl PendingApproval {
    pub fn send(self, response: ChannelApprovalResponse) -> Result<(), ChannelApprovalResponse> {
        self.sender.send(response)
    }
}

struct PendingApprovalRegistration {
    request_id: String,
    registration_id: Uuid,
    receiver: oneshot::Receiver<ChannelApprovalResponse>,
    pending: PendingApprovals,
}

impl PendingApprovalRegistration {
    fn new(pending: &PendingApprovals) -> Self {
        loop {
            let request_id = Uuid::new_v4().to_string();
            let mut entries = pending.lock();
            if let std::collections::hash_map::Entry::Vacant(slot) =
                entries.entry(request_id.clone())
            {
                let registration_id = Uuid::new_v4();
                let (sender, receiver) = oneshot::channel();
                slot.insert(PendingApproval {
                    registration_id,
                    sender,
                });
                return Self {
                    request_id,
                    registration_id,
                    receiver,
                    pending: Arc::clone(pending),
                };
            }
        }
    }
}

impl Drop for PendingApprovalRegistration {
    fn drop(&mut self) {
        self.receiver.close();
        let mut pending = self.pending.lock();
        if pending
            .get(&self.request_id)
            .is_some_and(|entry| entry.registration_id == self.registration_id)
        {
            pending.remove(&self.request_id);
        }
    }
}

/// Construct an empty pending-approvals registry for a fresh connection.
pub fn new_pending_approvals() -> PendingApprovals {
    Arc::new(Mutex::new(HashMap::new()))
}

/// `Channel` implementation that emits approval frames over a connection's
/// existing `event_tx` and parks on a oneshot until the matching response
/// arrives or `timeout` elapses.
pub struct WsApprovalChannel {
    event_tx: mpsc::Sender<TurnEvent>,
    pending: PendingApprovals,
    timeout: Duration,
}

impl WsApprovalChannel {
    pub fn new(
        event_tx: mpsc::Sender<TurnEvent>,
        pending: PendingApprovals,
        timeout: Duration,
    ) -> Self {
        Self {
            event_tx,
            pending,
            timeout,
        }
    }
}

impl ::zeroclaw_api::attribution::Attributable for WsApprovalChannel {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Channel(
            ::zeroclaw_api::attribution::ChannelKind::Webhook,
        )
    }
    fn alias(&self) -> &str {
        "ws_approval"
    }
}

#[async_trait]
impl Channel for WsApprovalChannel {
    fn name(&self) -> &str {
        "ws"
    }

    async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
        // The gateway WS path streams agent output via TurnEvent::Chunk /
        // ::Thinking / ::ToolCall / ::ToolResult; it does not deliver
        // free-form `send()` messages. Returning Ok here keeps any caller
        // that probes for a generic delivery target from erroring out.
        Ok(())
    }

    /// `send` above is a deliberate no-op (see comment). Surfaces that must
    /// genuinely deliver — `poll`'s formatted-text fallback and
    /// `escalate_to_human` — check this so they fail honestly instead of
    /// claiming success for a message that was never rendered.
    fn supports_outbound_send(&self) -> bool {
        false
    }

    async fn listen(&self, _tx: zeroclaw_api::inbound::Sender) -> anyhow::Result<()> {
        // The gateway WS path does not act as a message source for the
        // channel orchestrator; turns are driven directly by the WS
        // handler loop. Listen is a no-op for this transport.
        Ok(())
    }

    fn supports_free_form_ask(&self) -> bool {
        false
    }

    /// Delegates to [`Self::request_approval_attributed`] and drops the
    /// provenance, so the prompt/timeout logic lives in exactly one place.
    async fn request_approval(
        &self,
        recipient: &str,
        request: &ChannelApprovalRequest,
    ) -> anyhow::Result<Option<ChannelApprovalResponse>> {
        Ok(self
            .request_approval_attributed(recipient, request)
            .await?
            .map(|attributed| attributed.response))
    }

    async fn request_approval_attributed(
        &self,
        _recipient: &str,
        request: &ChannelApprovalRequest,
    ) -> anyhow::Result<Option<AttributedApprovalResponse>> {
        // The receiver and registration are owned before event delivery can
        // block, so cancellation covers both send and operator-response waits.
        let mut registration = PendingApprovalRegistration::new(&self.pending);
        let request_id = registration.request_id.clone();

        let event = TurnEvent::ApprovalRequest {
            request_id: request_id.clone(),
            tool_name: request.tool_name.clone(),
            arguments_summary: request.arguments_summary.clone(),
            timeout_secs: self.timeout.as_secs(),
        };
        if self.event_tx.send(event).await.is_err() {
            // Forward task has gone away; the WS is closing. Clean up the
            // pending entry and let the agent's caller treat this the same
            // as any other channel that returns None: fall through to
            // auto-deny per ApprovalManager policy.
            return Ok(None);
        }

        match tokio::time::timeout(self.timeout, &mut registration.receiver).await {
            Ok(Ok(decision)) => Ok(Some(AttributedApprovalResponse::operator(decision))),
            Ok(Err(_)) => {
                // Sender dropped without responding (connection closed
                // mid-prompt). Treat as deny rather than None so the agent
                // does not silently fall back to "no channel handled this" —
                // but mark it Unreachable, because nobody answered.
                Ok(Some(AttributedApprovalResponse::from_runtime(
                    ChannelApprovalResponse::Deny,
                    ApprovalSource::Unreachable,
                )))
            }
            Err(_) => {
                // Timeout: pop and deny. Mirrors Telegram / Slack behaviour
                // when the operator does not tap a button in time. This deny is
                // the runtime's, not the operator's.
                Ok(Some(AttributedApprovalResponse::from_runtime(
                    ChannelApprovalResponse::Deny,
                    ApprovalSource::TimedOut,
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_send_removes_registration_before_drop_returns() {
        let (tx, mut events) = mpsc::channel(1);
        tx.send(TurnEvent::ApprovalRequest {
            request_id: "occupied".into(),
            tool_name: "fixture".into(),
            arguments_summary: String::new(),
            timeout_secs: 60,
        })
        .await
        .unwrap();
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending.clone(), Duration::from_secs(60));
        let request = approval_request();
        let mut wait = Box::pin(channel.request_approval_attributed("fixture", &request));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        assert_eq!(pending.lock().len(), 1);
        drop(wait);
        assert!(pending.lock().is_empty());
        assert!(
            matches!(events.recv().await, Some(TurnEvent::ApprovalRequest { request_id, .. }) if request_id == "occupied")
        );
        assert!(
            events.try_recv().is_err(),
            "cancelled send must not publish later"
        );
    }

    #[tokio::test]
    async fn cancelled_wait_rejects_late_response_and_new_request_can_complete() {
        let (tx, mut events) = mpsc::channel(2);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending.clone(), Duration::from_secs(60));
        let request = approval_request();
        let mut first = Box::pin(channel.request_approval_attributed("fixture", &request));
        let first_id = tokio::select! {
            biased;
            _ = &mut first => panic!("approval ended without a response"),
            event = events.recv() => match event.unwrap() {
                TurnEvent::ApprovalRequest { request_id, .. } => request_id,
                _ => panic!("approval event required"),
            },
        };
        drop(first);
        assert!(!pending.lock().contains_key(&first_id));
        let mut next = Box::pin(channel.request_approval_attributed("fixture", &request));
        let next_id = tokio::select! {
            biased;
            _ = &mut next => panic!("new approval ended without a response"),
            event = events.recv() => match event.unwrap() {
                TurnEvent::ApprovalRequest { request_id, .. } => request_id,
                _ => panic!("approval event required"),
            },
        };
        assert_ne!(first_id, next_id);
        assert!(
            pending.lock().remove(&first_id).is_none(),
            "late response has no owner"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut next)
                .await
                .is_err()
        );
        pending
            .lock()
            .remove(&next_id)
            .unwrap()
            .send(ChannelApprovalResponse::Approve)
            .unwrap();
        assert_eq!(
            next.await.unwrap().unwrap().response,
            ChannelApprovalResponse::Approve
        );
        assert!(pending.lock().is_empty());
    }

    #[tokio::test]
    async fn cancelled_registration_preserves_replacement_under_same_key() {
        let pending = new_pending_approvals();
        let registration = PendingApprovalRegistration::new(&pending);
        let key = registration.request_id.clone();
        let old = pending.lock().remove(&key).unwrap();
        let (sender, receiver) = oneshot::channel();
        let replacement_id = Uuid::new_v4();
        pending.lock().insert(
            key.clone(),
            PendingApproval {
                registration_id: replacement_id,
                sender,
            },
        );
        drop(registration);
        assert!(old.send(ChannelApprovalResponse::AlwaysApprove).is_err());
        assert_eq!(
            pending.lock().get(&key).unwrap().registration_id,
            replacement_id
        );
        pending
            .lock()
            .remove(&key)
            .unwrap()
            .send(ChannelApprovalResponse::Approve)
            .unwrap();
        assert_eq!(receiver.await.unwrap(), ChannelApprovalResponse::Approve);
    }

    #[test]
    fn ws_approval_channel_declines_free_form_ask() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending, Duration::from_secs(30));
        assert!(
            !channel.supports_free_form_ask(),
            "WsApprovalChannel must refuse free-form ask_user; \
             its send() is a no-op and listen() drops immediately"
        );
    }

    fn approval_request() -> ChannelApprovalRequest {
        ChannelApprovalRequest {
            tool_name: "file_write".to_string(),
            arguments_summary: "path=a.txt".to_string(),
            raw_arguments: None,
        }
    }

    /// The operator never taps anything and the prompt times out. The channel
    /// still synthesizes `Some(Deny)` so the agent does not fall through to
    /// "no channel handled this" — but that deny is the RUNTIME's, and the
    /// provenance has to say so. Before this, the identical `Some(Deny)` was
    /// reported to the model as "Denied by user." on a run where no human was
    /// ever asked.
    #[tokio::test]
    async fn timed_out_prompt_denies_with_runtime_provenance() {
        // Keep the receiver alive so the send succeeds and we reach the timeout
        // rather than the "forward task gone" early return.
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending, Duration::from_millis(50));

        let attributed = channel
            .request_approval_attributed("operator", &approval_request())
            .await
            .expect("timeout is not an error")
            .expect("a timeout still yields a deny, not None");

        assert_eq!(attributed.response, ChannelApprovalResponse::Deny);
        assert_eq!(
            attributed.source,
            ApprovalSource::TimedOut,
            "a prompt nobody answered is not an operator decision"
        );
        assert!(attributed.source.is_runtime_fail_closed());
    }

    /// The connection closes mid-prompt: the pending sender is dropped. Same
    /// contract as the timeout — a deny, but not the operator's.
    #[tokio::test]
    async fn dropped_responder_denies_with_runtime_provenance() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, Arc::clone(&pending), Duration::from_secs(30));

        let call = async {
            channel
                .request_approval_attributed("operator", &approval_request())
                .await
        };
        // Drop the registered oneshot sender out from under the waiter, which is
        // what a closing WebSocket does.
        let dropper = async {
            for _ in 0..100 {
                {
                    let mut guard = pending.lock();
                    if let Some(key) = guard.keys().next().cloned() {
                        guard.remove(&key);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (result, ()) = tokio::join!(call, dropper);

        let attributed = result
            .expect("a dropped responder is not an error")
            .expect("a dropped responder still yields a deny, not None");
        assert_eq!(attributed.response, ChannelApprovalResponse::Deny);
        assert_eq!(
            attributed.source,
            ApprovalSource::Unreachable,
            "a responder that went away is not an operator decision"
        );
    }

    /// The legacy entry point must keep its existing shape: provenance is an
    /// addition, not a change to what `request_approval` returns.
    #[tokio::test]
    async fn legacy_request_approval_still_returns_a_bare_deny_on_timeout() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = new_pending_approvals();
        let channel = WsApprovalChannel::new(tx, pending, Duration::from_millis(50));

        let response = channel
            .request_approval("operator", &approval_request())
            .await
            .expect("timeout is not an error");
        assert_eq!(response, Some(ChannelApprovalResponse::Deny));
    }
}
