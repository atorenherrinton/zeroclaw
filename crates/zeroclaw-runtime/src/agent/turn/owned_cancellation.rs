//! Cancellation ownership for a turn and its live SOP driver.
//!
//! These are handles to existing authority and an observed invocation signal,
//! never a second stop decision or a replacement parent deadline.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::deadline::{DeadlineExceeded, Phase};

use crate::security::estop_runtime::{
    self, EstopInterrupted, EstopRuntime, InvocationCancellation,
};

pub(super) const TURN_SETTLEMENT_BUDGET: Duration = Duration::from_millis(750);

pub(super) struct CancellationOwner {
    authority: Option<EstopRuntime>,
    invocation: InvocationCancellation,
    caller: Option<CancellationToken>,
    deadline: Option<tokio::time::Instant>,
}

impl CancellationOwner {
    pub(super) fn new(authority: Option<EstopRuntime>, caller: Option<CancellationToken>) -> Self {
        let invocation = estop_runtime::current_invocation().unwrap_or_default();
        if let Some(caller) = &caller {
            invocation.bind_caller_token(caller);
        }
        Self {
            authority,
            invocation,
            caller,
            deadline: zeroclaw_api::deadline::current(),
        }
    }

    pub(super) fn invocation(&self) -> InvocationCancellation {
        self.invocation.clone()
    }

    pub(super) fn check(&self, tool: Option<&str>) -> Result<()> {
        if let Some(authority) = &self.authority
            && let Err(error) = authority.check(tool)
        {
            // Publish the canonical cause before cancelling child tokens. The
            // same invocation must not mistake the resulting child wake-up for
            // an unrelated user cancellation or lose replacement authority.
            match tool {
                Some(tool) => self.invocation.request_estop(authority, tool),
                None => self.invocation.request_estop_turn(authority),
            }
            return Err(error);
        }
        self.invocation.check()?;
        if self
            .deadline
            .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            self.invocation.request_deadline();
            return Err(DeadlineExceeded {
                phase: Phase::Turn,
                started: false,
            }
            .into());
        }
        if self
            .caller
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            self.invocation.request_user();
            return Err(super::ToolLoopCancelled.into());
        }
        Ok(())
    }

    pub(super) async fn interrupted(&self) -> anyhow::Error {
        let stopped = async {
            if let Some(authority) = &self.authority {
                authority.interrupted(None).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let cancelled = async {
            if let Some(caller) = &self.caller {
                caller.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let expired = async {
            if let Some(deadline) = self.deadline {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = stopped => {
                if let Some(authority) = &self.authority {
                    self.invocation.request_estop_turn(authority);
                }
                EstopInterrupted.into()
            },
            error = self.invocation.interrupted() => error,
            () = expired => {
                self.invocation.request_deadline();
                DeadlineExceeded { phase: Phase::Turn, started: true }.into()
            },
            () = cancelled => {
                self.invocation.request_user();
                super::ToolLoopCancelled.into()
            },
        }
    }
}

pub(super) fn is_terminal(error: &anyhow::Error) -> bool {
    super::is_tool_loop_cancelled(error)
        || error.is::<DeadlineExceeded>()
        || error.chain().any(|source| source.is::<DeadlineExceeded>())
}

/// Preserve the original nested error object while retaining the owner's typed
/// stop cause as an anyhow context (which supports direct downcasting).
pub(super) fn with_stop_cause(error: anyhow::Error, cause: anyhow::Error) -> anyhow::Error {
    if estop_runtime::is_estop_interrupted(&cause) {
        error.context(EstopInterrupted)
    } else if cause.is::<DeadlineExceeded>()
        || cause.chain().any(|source| source.is::<DeadlineExceeded>())
    {
        error.context(DeadlineExceeded {
            phase: Phase::Turn,
            started: true,
        })
    } else {
        error.context(super::ToolLoopCancelled)
    }
}

pub(crate) struct TurnSettlementIncomplete {
    cause: anyhow::Error,
}

impl std::fmt::Debug for TurnSettlementIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TurnSettlementIncomplete")
    }
}
impl std::fmt::Display for TurnSettlementIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::get_required_cli_string(
            "turn-settlement-incomplete",
        ))
    }
}
impl std::error::Error for TurnSettlementIncomplete {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(crate) struct TurnCompletedAfterInterruption {
    pub(crate) output: String,
    cause: anyhow::Error,
}

pub(super) fn completed_after_interruption(output: String, cause: anyhow::Error) -> anyhow::Error {
    TurnCompletedAfterInterruption { output, cause }.into()
}
impl std::fmt::Debug for TurnCompletedAfterInterruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCompletedAfterInterruption")
            .field("output_bytes", &self.output.len())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for TurnCompletedAfterInterruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.cause, f)
    }
}
impl std::error::Error for TurnCompletedAfterInterruption {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(super) async fn run_turn(
    owner: &CancellationOwner,
    child: &CancellationToken,
    future: impl Future<Output = Result<String>>,
) -> Result<String> {
    owner.check(None)?;
    tokio::pin!(future);
    let cause = tokio::select! {
        biased;
        // A ready result belongs to this turn even when stop becomes ready in
        // the same poll. It must not be discarded in favor of cancellation.
        result = &mut future => return match (result, owner.check(None)) {
            (Ok(output), Ok(())) => Ok(output),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cause)) => Err(with_stop_cause(error, cause)),
            (Ok(output), Err(cause)) => Err(TurnCompletedAfterInterruption { output, cause }.into()),
        },
        cause = owner.interrupted() => cause,
    };
    child.cancel();
    // PARENT is deliberately unchanged. Every nested admission still consumes
    // the original budget; this grace only collects work already owned.
    match tokio::time::timeout(TURN_SETTLEMENT_BUDGET, &mut future).await {
        Ok(Err(error)) => Err(with_stop_cause(error, cause)),
        Ok(Ok(output)) => Err(TurnCompletedAfterInterruption { output, cause }.into()),
        Err(_) => Err(TurnSettlementIncomplete { cause }.into()),
    }
}
