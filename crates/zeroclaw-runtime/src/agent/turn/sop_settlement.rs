//! Live SOP ownership during interruption. Canonical state remains in SopEngine;
//! these transient error objects retain evidence until the caller handles it.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::owned_cancellation::{CancellationOwner, is_terminal, with_stop_cause};
use crate::sop::executor::QueuedSopAction;
use crate::sop::{SopRunAction, SopStepResult};

const ASSEMBLY_BUDGET: Duration = Duration::from_secs(30);
const STEP_SETTLEMENT_BUDGET: Duration = Duration::from_millis(650);
const AUDIT_SETTLEMENT_BUDGET: Duration = Duration::from_millis(500);

#[cfg(test)]
pub(super) type AssemblyProbe = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<super::OwnedAgentExecution>> + Send>>
        + Send
        + Sync,
>;
#[cfg(test)]
tokio::task_local! {
    pub(super) static ASSEMBLY_PROBE: AssemblyProbe;
}

pub(crate) struct SopPhaseIncomplete {
    pub(crate) phase: &'static str,
}
impl std::fmt::Debug for SopPhaseIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SopPhaseIncomplete")
            .field("phase", &self.phase)
            .finish()
    }
}
impl std::fmt::Display for SopPhaseIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::get_required_cli_string(
            "sop-settlement-incomplete",
        ))
    }
}
impl std::error::Error for SopPhaseIncomplete {}

pub(crate) enum AuditSettlement {
    NotStarted,
    Complete,
    Failed(anyhow::Error),
    /// Dropping Memory::store does not prove its backend write was cancelled.
    Incomplete,
}

pub(crate) struct StepSettlement {
    pub(crate) queued: QueuedSopAction,
    pub(crate) result: SopStepResult,
    pub(crate) persistence_error: Option<anyhow::Error>,
    pub(crate) audit: AuditSettlement,
}

pub(crate) struct QueuedSettlement {
    /// Keep the source-owned engine/action even if cancellation cannot persist.
    pub(crate) queued: QueuedSopAction,
    pub(crate) cancellation: Result<Option<SopRunAction>>,
}

pub(crate) struct SopDriveInterrupted {
    pub(crate) cause: anyhow::Error,
    pub(crate) steps: Vec<StepSettlement>,
    pub(crate) queued: Vec<QueuedSettlement>,
}
impl std::fmt::Debug for SopDriveInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let completed = self
            .steps
            .iter()
            .filter(|step| step.result.status == crate::sop::SopStepStatus::Completed)
            .count();
        let persistence_failures = self
            .steps
            .iter()
            .filter(|step| step.persistence_error.is_some())
            .count();
        let audit_failure_causes = self
            .steps
            .iter()
            .map(|step| match &step.audit {
                AuditSettlement::Failed(error) => error.chain().count(),
                _ => 0,
            })
            .sum::<usize>();
        let identities = self
            .steps
            .iter()
            .map(|step| action_run_id(&step.queued.action))
            .chain(
                self.queued
                    .iter()
                    .map(|queued| action_run_id(&queued.queued.action)),
            )
            .collect::<std::collections::HashSet<_>>()
            .len();
        let pending_cancellations = self
            .queued
            .iter()
            .filter(|queued| queued.cancellation.is_err())
            .count();
        f.debug_struct("SopDriveInterrupted")
            .field("steps", &self.steps.len())
            .field("completed_steps", &completed)
            .field("persistence_failures", &persistence_failures)
            .field("audit_failure_causes", &audit_failure_causes)
            .field("owned_runs", &identities)
            .field("pending_cancellations", &pending_cancellations)
            .field("queued", &self.queued.len())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for SopDriveInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.cause, f)
    }
}
impl std::error::Error for SopDriveInterrupted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(crate) fn action_run_id(action: &SopRunAction) -> &str {
    match action {
        SopRunAction::ExecuteStep { run_id, .. }
        | SopRunAction::DeterministicStep { run_id, .. }
        | SopRunAction::WaitApproval { run_id, .. }
        | SopRunAction::CheckpointWait { run_id, .. }
        | SopRunAction::Pending { run_id, .. }
        | SopRunAction::Completed { run_id, .. }
        | SopRunAction::Cancelled { run_id, .. }
        | SopRunAction::Failed { run_id, .. } => run_id,
    }
}

pub(super) fn with_action(queued: &QueuedSopAction, action: SopRunAction) -> QueuedSopAction {
    QueuedSopAction {
        action,
        ..queued.clone()
    }
}

fn cancel_unstarted(queued: QueuedSopAction) -> QueuedSettlement {
    let cancellation = (|| {
        // Do not wait behind a capability that holds this synchronous engine
        // lock. A busy/poisoned engine remains owned by the returned identity.
        let mut engine = queued.engine.try_lock().map_err(|_| SopPhaseIncomplete {
            phase: "queued cancellation",
        })?;
        let run_id = action_run_id(&queued.action);
        engine.cancel_run_idempotent(run_id, None, Some("live-sop-driver".into()))?;
        engine.finish_requested_cancellation(run_id)
    })();
    QueuedSettlement {
        queued,
        cancellation,
    }
}

pub(super) fn interrupted(
    cause: anyhow::Error,
    current: Option<QueuedSopAction>,
    pending: VecDeque<QueuedSopAction>,
    steps: Vec<StepSettlement>,
) -> anyhow::Error {
    let queued = current
        .into_iter()
        .chain(pending)
        .map(cancel_unstarted)
        .collect();
    SopDriveInterrupted {
        cause,
        steps,
        queued,
    }
    .into()
}

pub(super) async fn assemble<T>(
    owner: &CancellationOwner,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    owner.check(None)?;
    tokio::pin!(future);
    let result = tokio::select! {
        biased;
        result = &mut future => result,
        cause = owner.interrupted() => return Err(cause.context(SopPhaseIncomplete { phase: "assembly" })),
        () = tokio::time::sleep(ASSEMBLY_BUDGET) => return Err(anyhow::Error::new(SopPhaseIncomplete { phase: "assembly" })),
    };
    // A completed registry is cacheable only after fresh admission. Dropping a
    // partial constructor invokes its existing cleanup guards; external/blocking
    // cleanup completion remains explicitly unverified on interruption.
    owner.check(None)?;
    result
}

pub(super) async fn execute_step(
    owner: &CancellationOwner,
    child: &CancellationToken,
    future: impl Future<Output = Result<String>>,
) -> Result<String> {
    owner.check(None)?;
    tokio::pin!(future);
    let cause = tokio::select! {
        biased;
        result = &mut future => return result,
        cause = owner.interrupted() => cause,
    };
    child.cancel();
    match tokio::time::timeout(STEP_SETTLEMENT_BUDGET, &mut future).await {
        Ok(Err(error)) => Err(with_stop_cause(error, cause)),
        // A completed output remains a completed step. The driver checks the
        // owner immediately after this return before recording/routing it.
        Ok(Ok(output)) => Ok(output),
        Err(_) => Err(cause.context(SopPhaseIncomplete {
            phase: "nested step",
        })),
    }
}

pub(super) fn terminal_step_error(error: &anyhow::Error) -> bool {
    is_terminal(error)
        || error.is::<SopPhaseIncomplete>()
        || error
            .chain()
            .any(|source| source.is::<SopPhaseIncomplete>())
}

pub(super) fn cancel_then_advance(
    queued: &QueuedSopAction,
    run_id: &str,
    result: SopStepResult,
) -> Result<(SopRunAction, Option<crate::sop::SopRun>)> {
    let mut engine = queued.engine.try_lock().map_err(|_| SopPhaseIncomplete {
        phase: "step cancellation",
    })?;
    // Request persistence must succeed before advance can route Failed. On
    // failure the engine restores the active run and keeps its execution claim.
    engine.cancel_run_idempotent(run_id, None, Some("live-sop-driver".into()))?;
    let action = engine.advance_step(run_id, result)?;
    let finished = match &action {
        SopRunAction::Completed { .. }
        | SopRunAction::Failed { .. }
        | SopRunAction::Cancelled { .. } => engine.get_run(run_id).cloned(),
        _ => None,
    };
    Ok((action, finished))
}

pub(super) async fn audit(
    queued: &QueuedSopAction,
    run_id: &str,
    result: &SopStepResult,
    finished: Option<&crate::sop::SopRun>,
) -> AuditSettlement {
    let Some(audit) = queued.audit.as_deref() else {
        return AuditSettlement::Complete;
    };
    // This is post-commit persistence. Bound the whole attempt, retaining the
    // canonical completed step regardless of optional audit latency or failure.
    match tokio::time::timeout(AUDIT_SETTLEMENT_BUDGET, async {
        audit.log_step_result(run_id, result).await?;
        if let Some(run) = finished {
            audit.log_run_complete(run).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    {
        Ok(Ok(())) => AuditSettlement::Complete,
        Ok(Err(error)) => AuditSettlement::Failed(error),
        Err(_) => AuditSettlement::Incomplete,
    }
}

pub(super) fn audit_error(audit: &AuditSettlement) -> Option<anyhow::Error> {
    (!matches!(audit, AuditSettlement::Complete))
        .then(|| anyhow::Error::new(SopPhaseIncomplete { phase: "audit" }))
}
