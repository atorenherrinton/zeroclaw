//! Canonical supervised lifecycle and task-local progress seam. The control-plane
//! database owns the facts; this handle only routes checkpoints to that owner.
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is currently eligible to execute or already executing.
    Running,
    Received,
    Queued,
    WaitingOnTool,
    ResponseReady,
    Submitting,
    Delivered,
    PartiallyDelivered,
    Uncertain,
    /// Task is intentionally stopped but resumable.
    Paused,
    /// Task finished successfully.
    Completed,
    /// Task ended with an error.
    Failed,
    /// Task was intentionally cancelled.
    Cancelled,
    /// Written by the reaper/recovery sweep from OUTSIDE the task body — the state
    /// today's enum literally cannot represent (task-lifecycle-supervision gap).
    Lost,
    /// Heartbeat exceeded its grace window / the task passed `max_runtime`.
    TimedOut,
}

impl TaskStatus {
    /// A task is terminal once it can no longer transition. The reaper only
    /// reconciles non-terminal records.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed
                | TaskStatus::Delivered
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Lost
                | TaskStatus::TimedOut
                | TaskStatus::PartiallyDelivered
                | TaskStatus::Uncertain
        )
    }

    /// Legal channel lifecycle edges. Other task domains retain their own rules.
    /// An acknowledgement is a fact separate from successful generation.
    pub fn permits_channel_transition(self, next: Self) -> bool {
        use TaskStatus::*;
        if self.is_terminal() {
            return false;
        }
        match self {
            Received => matches!(next, Queued | Failed | Cancelled | Uncertain),
            Queued => matches!(next, Running | Failed | Cancelled | Uncertain),
            Running => matches!(
                next,
                Running | WaitingOnTool | ResponseReady | Failed | Cancelled | Uncertain
            ),
            // A tool cancelled by the deadline, /stop or a newer message leaves the
            // journal here on purpose (its effect may have applied, so it is
            // quarantined, never replayed). The terminal notice then saves its
            // response directly from this state; without this edge the notice is
            // rejected and the turn ends Uncertain with nothing delivered.
            WaitingOnTool => matches!(next, Running | ResponseReady | Failed | Uncertain),
            ResponseReady => matches!(next, Submitting | Failed | Cancelled | Uncertain),
            Submitting => matches!(next, Delivered | PartiallyDelivered | Failed | Uncertain),
            _ => false,
        }
    }
}

#[async_trait::async_trait]
pub trait TurnJournal: Send + Sync {
    fn trace_id(&self) -> Option<&str> {
        None
    }
    /// Generation failure is separate from acknowledgement of its terminal notice.
    async fn record_error(&self, _error: String) -> anyhow::Result<()> {
        Ok(())
    }
    async fn checkpoint(
        &self,
        status: TaskStatus,
        output: Option<String>,
        delivered: bool,
    ) -> anyhow::Result<()>;
}
tokio::task_local! { pub static JOURNAL: Option<Arc<dyn TurnJournal>>; }
pub async fn checkpoint(
    status: TaskStatus,
    output: Option<String>,
    delivered: bool,
) -> anyhow::Result<()> {
    if let Some(journal) = JOURNAL.try_with(Clone::clone).ok().flatten() {
        journal.checkpoint(status, output, delivered).await?;
    }
    Ok(())
}

/// Resolve tracing from the canonical admitted turn, never a second identifier.
pub fn trace_id() -> Option<String> {
    JOURNAL
        .try_with(|journal| {
            journal
                .as_ref()
                .and_then(|j| j.trace_id().map(str::to_owned))
        })
        .ok()
        .flatten()
}

pub async fn record_error(error: String) -> anyhow::Result<()> {
    if let Some(journal) = JOURNAL.try_with(Clone::clone).ok().flatten() {
        journal.record_error(error).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TaskStatus::{self, *};

    const ALL: [TaskStatus; 15] = [
        Running,
        Received,
        Queued,
        WaitingOnTool,
        ResponseReady,
        Submitting,
        Delivered,
        PartiallyDelivered,
        Uncertain,
        Paused,
        Completed,
        Failed,
        Cancelled,
        Lost,
        TimedOut,
    ];

    /// A tool interrupted by the deadline, /stop or a newer message leaves the
    /// journal in `WaitingOnTool` on purpose (its effect may have applied, so it
    /// is quarantined rather than replayed). The terminal notice must be able to
    /// leave that state, otherwise it is dropped and the user hears nothing.
    #[test]
    fn interrupted_tool_call_can_hand_off_a_terminal_notice() {
        assert!(WaitingOnTool.permits_channel_transition(ResponseReady));
    }

    #[test]
    fn waiting_on_tool_still_cannot_skip_the_response_checkpoint() {
        for next in [
            Received,
            Queued,
            Submitting,
            Delivered,
            PartiallyDelivered,
            Cancelled,
        ] {
            assert!(!WaitingOnTool.permits_channel_transition(next), "{next:?}");
        }
        for next in [Running, ResponseReady, Failed, Uncertain] {
            assert!(WaitingOnTool.permits_channel_transition(next), "{next:?}");
        }
    }

    #[test]
    fn terminal_states_permit_no_channel_transition() {
        for from in ALL.into_iter().filter(|s| s.is_terminal()) {
            for next in ALL {
                assert!(!from.permits_channel_transition(next), "{from:?}->{next:?}");
            }
        }
    }
}
