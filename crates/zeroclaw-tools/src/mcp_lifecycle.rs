//! Invocation-owned lifecycle authority for MCP execution and recovery.
//!
//! Runtime adapters resolve their canonical policy at use time. This module
//! carries that authority; it does not load configuration or interpret policy.

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;

#[async_trait::async_trait]
pub trait McpLifecycleControl: Send + Sync {
    fn check(&self) -> Result<()>;
    /// Fresh replacement is a new operation after owned cleanup, not a replay
    /// of the cancelled invocation. Adapters may ignore a completed invocation's
    /// terminal signal here while still checking its originating live authority.
    fn check_replacement(&self) -> Result<()> {
        self.check()
    }
    /// Return the original typed cause at interruption, even if authority is
    /// resumed before the waiting caller is polled again.
    async fn interrupted(&self) -> anyhow::Error;
}

tokio::task_local! {
    static CURRENT: Arc<dyn McpLifecycleControl>;
}

pub fn current_mcp_lifecycle_control() -> Option<Arc<dyn McpLifecycleControl>> {
    CURRENT.try_with(Arc::clone).ok()
}

pub async fn with_mcp_lifecycle_control<F: Future>(
    control: Option<Arc<dyn McpLifecycleControl>>,
    future: F,
) -> F::Output {
    match control {
        Some(control) => CURRENT.scope(control, future).await,
        None => future.await,
    }
}

/// Preserve the adapter's original error while making interruption recognizable
/// to lower-level wrappers that must not turn it into an ordinary tool failure.
pub struct McpLifecycleInterrupted(anyhow::Error);

impl std::fmt::Debug for McpLifecycleInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpLifecycleInterrupted")
    }
}

impl std::fmt::Display for McpLifecycleInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for McpLifecycleInterrupted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub fn is_lifecycle_interrupted(error: &anyhow::Error) -> bool {
    error.is::<McpLifecycleInterrupted>()
        || error
            .chain()
            .any(|cause| cause.is::<McpLifecycleInterrupted>())
}

fn interrupted(error: anyhow::Error) -> anyhow::Error {
    if is_lifecycle_interrupted(&error) {
        error
    } else {
        McpLifecycleInterrupted(error).into()
    }
}

pub(crate) fn check(control: Option<&dyn McpLifecycleControl>) -> Result<()> {
    control.map_or(Ok(()), |control| control.check().map_err(interrupted))
}

pub(crate) fn check_replacement(control: Option<&dyn McpLifecycleControl>) -> Result<()> {
    control.map_or(Ok(()), |control| {
        control.check_replacement().map_err(interrupted)
    })
}

/// Only execution/protocol waits belong here. Mandatory close and child reaping
/// must be awaited outside this cancellation scope under their cleanup budget.
pub(crate) async fn run<T>(
    control: Option<&dyn McpLifecycleControl>,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    check(control)?;
    match control {
        Some(control) => {
            tokio::select! {
                biased;
                error = control.interrupted() => Err(interrupted(error)),
                result = future => result,
            }
        }
        None => future.await,
    }
}
