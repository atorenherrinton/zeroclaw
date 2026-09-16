//! Ownership of a delegated execution's returned evidence, not another journal.

use super::*;
use crate::security::estop_runtime::{self, InvocationCancellation};
use std::future::Future;

// The enclosing runtime runner allows two seconds. A fanout gets 1500 ms,
// while each delegate first gives its nested turn 1000 ms to settle.
pub(super) const TURN_SETTLEMENT: Duration = Duration::from_millis(1000);
pub(super) const FANOUT_SETTLEMENT: Duration = Duration::from_millis(1500);

pub(crate) struct DelegateChildResult {
    pub index: usize,
    pub agent: String,
    pub result: ToolResult,
}
pub(crate) struct DelegateChildFailure {
    pub index: usize,
    pub agent: String,
    pub error: anyhow::Error,
}
#[derive(Debug, serde::Serialize)]
pub(crate) struct DelegateUnsettled {
    pub index: usize,
    pub agent: String,
    pub reason: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DelegateUnstarted {
    pub index: usize,
    pub agent: String,
}

pub(crate) struct DelegateTerminalError {
    pub completed: Vec<DelegateChildResult>,
    pub failures: Vec<DelegateChildFailure>,
    pub unsettled: Vec<DelegateUnsettled>,
    pub unstarted: Vec<DelegateUnstarted>,
    // Index of the first observed terminal child, independent of presentation order.
    pub first_terminal: Option<usize>,
    pub cause: Option<anyhow::Error>,
}
impl std::fmt::Debug for DelegateTerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateTerminalError")
            .field("completed_count", &self.completed.len())
            .field("failure_count", &self.failures.len())
            .field("unsettled_count", &self.unsettled.len())
            .field("unstarted_count", &self.unstarted.len())
            .finish()
    }
}
impl std::fmt::Display for DelegateTerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.cause.as_ref().or_else(|| {
            self.failures
                .iter()
                .find(|failure| Some(failure.index) == self.first_terminal)
                .or_else(|| self.failures.first())
                .map(|failure| &failure.error)
        }) {
            Some(error) => std::fmt::Display::fmt(error, f),
            None => f.write_str(&crate::i18n::get_required_cli_string(
                "delegate-settlement-incomplete",
            )),
        }
    }
}
impl std::error::Error for DelegateTerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_ref()
            .or_else(|| {
                self.failures
                    .iter()
                    .find(|failure| Some(failure.index) == self.first_terminal)
                    .or_else(|| self.failures.first())
                    .map(|failure| &failure.error)
            })
            .map(|error| error.as_ref())
    }
}

/// Move the actual nested turn history with its original error. Display never
/// expands history or tool payloads; typed errors remain reachable via source.
pub(crate) struct DelegateAgenticError {
    pub history: Vec<ChatMessage>,
    pub error: anyhow::Error,
}
impl std::fmt::Debug for DelegateAgenticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateAgenticError")
            .field("history_len", &self.history.len())
            .finish()
    }
}
impl std::fmt::Display for DelegateAgenticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for DelegateAgenticError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

pub(super) fn terminal(error: &anyhow::Error) -> bool {
    crate::agent::tool_execution::is_terminal_tool_error(error)
}

pub(super) async fn settle_turn(
    agent: &str,
    child_token: &CancellationToken,
    future: impl Future<Output = anyhow::Result<ToolResult>>,
) -> anyhow::Result<ToolResult> {
    let invocation =
        estop_runtime::current_invocation().expect("run_tool scopes delegate invocation");
    let mut future = Box::pin(future);
    let deadline = zeroclaw_api::deadline::current();
    let expires = async {
        if let Some(deadline) = deadline {
            tokio::time::sleep_until(deadline).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let cause = tokio::select! {
        biased;
        cause = invocation.interrupted() => cause,
        () = child_token.cancelled() => {
            invocation.request_user();
            invocation.interrupted().await
        }
        () = expires => {
            invocation.request_deadline();
            invocation.interrupted().await
        }
        result = &mut future => return result,
    };
    child_token.cancel();
    match tokio::time::timeout(TURN_SETTLEMENT, &mut future).await {
        Ok(Err(error)) if terminal(&error) => Err(error),
        Ok(result) => {
            let (completed, failures) = match result {
                Ok(result) => (
                    vec![DelegateChildResult {
                        index: 0,
                        agent: agent.into(),
                        result,
                    }],
                    Vec::new(),
                ),
                Err(error) => (
                    Vec::new(),
                    vec![DelegateChildFailure {
                        index: 0,
                        agent: agent.into(),
                        error,
                    }],
                ),
            };
            Err(DelegateTerminalError {
                completed,
                failures,
                unsettled: Vec::new(),
                unstarted: Vec::new(),
                first_terminal: None,
                cause: Some(cause),
            }
            .into())
        }
        Err(_) => Err(DelegateTerminalError {
            completed: Vec::new(),
            failures: Vec::new(),
            unsettled: vec![DelegateUnsettled {
                index: 0,
                agent: agent.into(),
                reason: "settlement_timeout",
            }],
            unstarted: Vec::new(),
            first_terminal: None,
            cause: Some(cause),
        }
        .into()),
    }
}

pub(super) async fn invocation_interrupted(
    invocation: Option<&InvocationCancellation>,
) -> anyhow::Error {
    match invocation {
        Some(invocation) => invocation.interrupted().await,
        None => std::future::pending().await,
    }
}

/// Materialize source-owned evidence into the existing background result file.
/// Unknown error types remain explicitly unmaterialized, never success evidence.
fn typed_error<T: std::error::Error + Send + Sync + 'static>(error: &anyhow::Error) -> Option<&T> {
    error
        .downcast_ref::<T>()
        .or_else(|| error.chain().find_map(|cause| cause.downcast_ref::<T>()))
}

pub(super) fn error_evidence(error: &anyhow::Error) -> serde_json::Value {
    use crate::agent::turn::{owned_cancellation, sop_settlement};
    if let Some(agentic) = typed_error::<DelegateAgenticError>(error) {
        return json!({"kind":"agentic_terminal", "history":agentic.history, "error":error_evidence(&agentic.error)});
    }
    if let Some(delegate) = typed_error::<DelegateTerminalError>(error) {
        return json!({"kind":"delegate_terminal",
            "completed":delegate.completed.iter().map(|child| json!({"index":child.index,"agent":child.agent,"result":child.result})).collect::<Vec<_>>(),
            "failures":delegate.failures.iter().map(|child| json!({"index":child.index,"agent":child.agent,"error":error_evidence(&child.error)})).collect::<Vec<_>>(),
            "unsettled":delegate.unsettled, "unstarted":delegate.unstarted,
            "first_terminal":delegate.first_terminal,
            "cause":delegate.cause.as_ref().map(error_kind),
        });
    }
    if let Some(sop) = typed_error::<sop_settlement::SopDriveInterrupted>(error) {
        return json!({"kind":"sop_terminal", "cause":error_evidence(&sop.cause),
            "steps":sop.steps.iter().map(|step| json!({
                "run_id":sop_settlement::action_run_id(&step.queued.action),
                "result":step.result,
                "persistence_error":step.persistence_error.as_ref().map(error_evidence),
                "audit":match &step.audit {
                    sop_settlement::AuditSettlement::NotStarted => json!({"state":"not_started"}),
                    sop_settlement::AuditSettlement::Complete => json!({"state":"complete"}),
                    sop_settlement::AuditSettlement::Failed(error) => json!({"state":"failed","error":error_evidence(error)}),
                    sop_settlement::AuditSettlement::Incomplete => json!({"state":"incomplete"}),
                },
            })).collect::<Vec<_>>(),
            "queued":sop.queued.iter().map(|queued| json!({
                "run_id":sop_settlement::action_run_id(&queued.queued.action),
                "cancellation":match &queued.cancellation {
                    Ok(_) => json!({"state":"persisted"}),
                    Err(error) => json!({"state":"incomplete","error":error_evidence(error)}),
                },
            })).collect::<Vec<_>>(),
        });
    }
    if let Some(budget) =
        typed_error::<crate::agent::turn::results_collect::ResultBudgetExceeded>(error)
    {
        return json!({"kind":"result_budget", "results":budget.results,
            "errors":budget.errors.iter().map(error_evidence).collect::<Vec<_>>()});
    }
    if let Some(pipeline) = typed_error::<zeroclaw_tools::pipeline::PipelineTerminalError>(error) {
        return json!({"kind":"pipeline_terminal",
            "completed":pipeline.completed.iter().map(|child| json!({"index":child.index,"tool":child.tool,"result":child.result})).collect::<Vec<_>>(),
            "failures":pipeline.failures.iter().map(|child| json!({"index":child.index,"tool":child.tool,"error":error_evidence(&child.error)})).collect::<Vec<_>>(),
            "unsettled":pipeline.unsettled.iter().map(|child| json!({"index":child.index,"tool":child.tool,"reason":format!("{:?}",child.reason)})).collect::<Vec<_>>(),
            "cause":pipeline.outer_interruption.as_ref().map(error_kind),
        });
    }
    if let Some(completed) = typed_error::<estop_runtime::InterruptedToolResult>(error) {
        return json!({"kind":"interrupted_tool_result","result":completed.completed,"cause":error_kind(error)});
    }
    if let Some(completed) =
        typed_error::<owned_cancellation::TurnCompletedAfterInterruption>(error)
    {
        return json!({"kind":"turn_completed_after_interruption","output":completed.output,"cause":error_kind(error)});
    }
    if typed_error::<owned_cancellation::TurnSettlementIncomplete>(error).is_some() {
        return json!({"kind":"turn_settlement_incomplete","outcome":"unknown","cause":error_kind(error)});
    }
    json!({"kind":error_kind(error), "message":crate::security::scrub(&error.to_string()), "evidence":"no_additional_typed_payload_materialized"})
}

pub(super) fn error_kind(error: &anyhow::Error) -> &'static str {
    if estop_runtime::is_estop_interrupted(error) {
        "emergency_stop"
    } else if error.is::<zeroclaw_api::deadline::DeadlineExceeded>()
        || error
            .chain()
            .any(|cause| cause.is::<zeroclaw_api::deadline::DeadlineExceeded>())
    {
        "deadline"
    } else if crate::agent::loop_::is_tool_loop_cancelled(error) {
        "cancelled"
    } else if error.is::<zeroclaw_api::delivery::DeliveryFailure>()
        || error
            .chain()
            .any(|cause| cause.is::<zeroclaw_api::delivery::DeliveryFailure>())
    {
        "delivery_failure"
    } else {
        "failure"
    }
}

pub(super) struct BackgroundOwner(pub String);
impl Drop for BackgroundOwner {
    fn drop(&mut self) {
        DelegateTool::background_task_cancels()
            .lock()
            .remove(&self.0);
    }
}
