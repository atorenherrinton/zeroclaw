//! Runtime views of the canonical emergency-stop configuration and state file.
//!
//! The resolver is a handle to authority, never a cached stop decision. Daemon
//! callers resolve configuration from the shared live handle on every check;
//! standalone callers borrow the configuration for their invocation. The state
//! file is read afresh for admission and while an operation is pending.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use zeroclaw_config::schema::{Config, EstopConfig};

use super::estop::{EstopState, read_current_state};

const CHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct EstopRuntime {
    resolve: Arc<dyn Fn() -> (EstopConfig, PathBuf) + Send + Sync>,
}

impl EstopRuntime {
    pub(crate) fn from_config(config: &Config) -> Self {
        let setting = config.security.estop.clone();
        let directory = config.install_root_dir();
        Self {
            resolve: Arc::new(move || (setting.clone(), directory.clone())),
        }
    }

    pub(crate) fn from_live_config(config: Arc<parking_lot::RwLock<Config>>) -> Self {
        Self {
            resolve: Arc::new(move || {
                let config = config.read();
                (config.security.estop.clone(), config.install_root_dir())
            }),
        }
    }

    fn state(&self) -> EstopState {
        let (config, directory) = (self.resolve)();
        if config.enabled {
            read_current_state(&config, &directory)
        } else {
            EstopState::default()
        }
    }

    pub(crate) fn check(&self, tool: Option<&str>) -> anyhow::Result<()> {
        let state = self.state();
        // Tool metadata does not attest which destinations an implementation
        // can reach. A network/domain stop therefore suspends the whole turn,
        // including local tools that could delegate or spawn network clients.
        // Do not infer authority from model-supplied arguments or MCP hints.
        if state.kill_all
            || state.network_kill
            || !state.blocked_domains.is_empty()
            || tool.is_some_and(|name| {
                state
                    .frozen_tools
                    .iter()
                    .any(|frozen| frozen.eq_ignore_ascii_case(name.trim()))
            })
        {
            return Err(EstopInterrupted.into());
        }
        Ok(())
    }

    pub(crate) async fn interrupted(&self, tool: Option<&str>) {
        loop {
            if self.check(tool).is_err() {
                return;
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    }

    /// Stop an owned execution future. Callers keep journal/finalization work
    /// outside this scope; dropping a request cannot undo remote side effects.
    pub(crate) async fn run<T>(
        &self,
        tool: Option<&str>,
        future: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        self.check(tool)?;
        // Setup/turn futures can be large. Keep additional authority scopes
        // from copying their state machines through nested stack frames.
        let future = Box::pin(future);
        scope_with_tool(Some(self.clone()), tool, async {
            tokio::select! {
                biased;
                () = self.interrupted(tool) => Err(EstopInterrupted.into()),
                result = future => result,
            }
        })
        .await
    }
}

tokio::task_local! {
    static CURRENT_ESTOP: EstopRuntime;
}

pub(crate) fn current() -> Option<EstopRuntime> {
    CURRENT_ESTOP.try_with(Clone::clone).ok()
}

pub(crate) fn scope<'a, F>(
    runtime: Option<EstopRuntime>,
    future: F,
) -> std::pin::Pin<Box<dyn Future<Output = F::Output> + Send + 'a>>
where
    F: Future + Send + 'a,
{
    // Erase at this ownership boundary: callers can nest daemon/agent/tool
    // scopes without growing their stack frames or Send proof recursively.
    Box::pin(scope_with_tool(runtime, None, Box::pin(future)))
}

struct McpControl {
    runtime: Option<EstopRuntime>,
    originating_tool: Option<String>,
    invocation: Option<InvocationCancellation>,
}

#[async_trait::async_trait]
impl zeroclaw_tools::mcp_lifecycle::McpLifecycleControl for McpControl {
    fn check(&self) -> anyhow::Result<()> {
        if let Some(runtime) = &self.runtime {
            runtime.check(self.originating_tool.as_deref())?;
        }
        self.invocation
            .as_ref()
            .map_or(Ok(()), InvocationCancellation::check)
    }

    fn check_replacement(&self) -> anyhow::Result<()> {
        if let Some(runtime) = &self.runtime {
            runtime.check(self.originating_tool.as_deref())?;
        }
        self.invocation
            .as_ref()
            .map_or(Ok(()), InvocationCancellation::check_replacement)
    }

    async fn interrupted(&self) -> anyhow::Error {
        let invocation = async {
            if let Some(invocation) = &self.invocation {
                invocation.interrupted().await
            } else {
                std::future::pending().await
            }
        };
        let stopped = async {
            if let Some(runtime) = &self.runtime {
                runtime.interrupted(self.originating_tool.as_deref()).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = stopped => EstopInterrupted.into(),
            error = invocation => error,
        }
    }
}

async fn scope_with_tool<F: Future>(
    runtime: Option<EstopRuntime>,
    tool: Option<&str>,
    future: F,
) -> F::Output {
    let invocation = current_invocation();
    let control = if runtime.is_some() || invocation.is_some() {
        Some(Arc::new(McpControl {
            runtime: runtime.clone(),
            originating_tool: tool.map(str::to_owned),
            invocation,
        })
            as Arc<
                dyn zeroclaw_tools::mcp_lifecycle::McpLifecycleControl,
            >)
    } else {
        None
    };
    let future = zeroclaw_tools::mcp_lifecycle::with_mcp_lifecycle_control(control, future);
    if let Some(runtime) = runtime {
        CURRENT_ESTOP.scope(runtime, future).await
    } else {
        future.await
    }
}

/// One invocation owns this cancellation signal. It records an observed stop,
/// never policy: all admission still resolves the canonical live authority.
#[derive(Clone, Default)]
pub(crate) struct InvocationCancellation {
    state: Arc<parking_lot::Mutex<Option<ObservedInvocationStop>>>,
    changed: tokio_util::sync::CancellationToken,
    // Admission creates this ownership binding once. Keeping the caller token
    // with the invocation lets handed-off work observe it without a monitor task.
    originating_caller: Arc<parking_lot::Mutex<Option<tokio_util::sync::CancellationToken>>>,
    // A handed-off child inherits observed parent interruption, never publishes
    // its task-local cancellation/deadline into the parent's cause cell.
    parent: Option<Arc<InvocationCancellation>>,
}

#[derive(Clone)]
struct ObservedInvocationStop {
    reason: InvocationStop,
    // The exact authority/binding that caused the cancellation. Maintenance
    // rechecks it after cleanup; clearing a latch never replays this invocation.
    origin: Option<(EstopRuntime, Option<String>)>,
}

#[derive(Clone, Copy)]
enum InvocationStop {
    Estop,
    User,
    Deadline,
}

impl InvocationStop {
    fn error(self) -> anyhow::Error {
        match self {
            Self::Estop => EstopInterrupted.into(),
            Self::User => crate::agent::loop_::ToolLoopCancelled.into(),
            Self::Deadline => zeroclaw_api::deadline::DeadlineExceeded {
                phase: zeroclaw_api::deadline::Phase::Tool,
                started: true,
            }
            .into(),
        }
    }

    fn with_context(self, error: anyhow::Error) -> anyhow::Error {
        match self {
            Self::Estop => error.context(EstopInterrupted),
            Self::User => error.context(crate::agent::loop_::ToolLoopCancelled),
            Self::Deadline => error.context(zeroclaw_api::deadline::DeadlineExceeded {
                phase: zeroclaw_api::deadline::Phase::Tool,
                started: true,
            }),
        }
    }
}

impl InvocationCancellation {
    /// Create an owned background scope with one-way parent cancellation.
    pub(crate) fn child(&self) -> Self {
        Self {
            parent: Some(Arc::new(self.clone())),
            ..Self::default()
        }
    }

    fn inherit_parent_stop(&self) {
        if let Some(parent) = &self.parent {
            let _ = parent.check();
            let observed = parent.state.lock().clone();
            if let Some(observed) = observed {
                self.request_with_origin(observed.reason, observed.origin);
            }
        }
    }

    /// Bind before starting the invocation monitor. Nested runners cannot
    /// replace the original caller relationship with their child token.
    pub(crate) fn bind_caller_token(&self, caller: &tokio_util::sync::CancellationToken) {
        self.originating_caller
            .lock()
            .get_or_insert_with(|| caller.clone());
    }

    /// A turn-wide authority stop uses the same observed cause/origin owner as
    /// an actual-name tool stop, with no fabricated tool binding.
    pub(crate) fn request_estop_turn(&self, runtime: &EstopRuntime) {
        self.request_with_origin(InvocationStop::Estop, Some((runtime.clone(), None)));
    }

    /// The current owned execution observed caller cancellation.
    pub(crate) fn request_user(&self) {
        self.request(InvocationStop::User);
    }

    /// The current owned execution observed its original earliest deadline.
    pub(crate) fn request_deadline(&self) {
        self.request(InvocationStop::Deadline);
    }

    fn request(&self, reason: InvocationStop) {
        self.request_with_origin(reason, None);
    }

    pub(crate) fn request_estop(&self, runtime: &EstopRuntime, tool: &str) {
        self.request_with_origin(
            InvocationStop::Estop,
            Some((runtime.clone(), Some(tool.to_owned()))),
        );
    }

    fn request_with_origin(
        &self,
        reason: InvocationStop,
        origin: Option<(EstopRuntime, Option<String>)>,
    ) {
        let mut state = self.state.lock();
        if state.is_none() {
            *state = Some(ObservedInvocationStop { reason, origin });
            self.changed.cancel();
        }
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        self.inherit_parent_stop();
        let caller_cancelled = self
            .originating_caller
            .lock()
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
        if caller_cancelled {
            self.request_user();
        }
        self.state
            .lock()
            .as_ref()
            .map_or(Ok(()), |stop| Err(stop.reason.error()))
    }

    fn check_replacement(&self) -> anyhow::Result<()> {
        if let Some(parent) = &self.parent {
            parent.check_replacement()?;
        }
        let origin = self
            .state
            .lock()
            .as_ref()
            .and_then(|stop| stop.origin.clone());
        if let Some((runtime, tool)) = origin {
            runtime.check(tool.as_deref())?;
        }
        Ok(())
    }

    pub(crate) fn interrupted(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Error> + Send + '_>> {
        // Erase recursive ancestor waits without creating a detached monitor.
        Box::pin(async {
            if let Err(error) = self.check() {
                return error;
            }
            let caller = self.originating_caller.lock().clone();
            let caller_cancelled = async {
                if let Some(caller) = caller {
                    caller.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let parent_stopped = async {
                if let Some(parent) = &self.parent {
                    parent.interrupted().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                biased;
                () = self.changed.cancelled() => {},
                () = parent_stopped => self.inherit_parent_stop(),
                () = caller_cancelled => self.request_user(),
            }
            self.state
                .lock()
                .as_ref()
                .expect("cancelled invocation has a cause")
                .reason
                .error()
        })
    }
}

tokio::task_local! {
    static CURRENT_INVOCATION: InvocationCancellation;
}

pub(crate) fn current_invocation() -> Option<InvocationCancellation> {
    CURRENT_INVOCATION.try_with(Clone::clone).ok()
}

pub(crate) async fn scope_invocation<F: Future>(
    invocation: Option<InvocationCancellation>,
    future: F,
) -> F::Output {
    if let Some(invocation) = invocation {
        CURRENT_INVOCATION.scope(invocation, future).await
    } else {
        future.await
    }
}

/// A cooperative tool returned completed evidence after interruption was
/// requested. Keep the original result owned by its terminal error.
pub(crate) struct InterruptedToolResult {
    pub(crate) completed: zeroclaw_api::tool::ToolResult,
    cause: anyhow::Error,
}

impl std::fmt::Debug for InterruptedToolResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptedToolResult")
            .field("success", &self.completed.success)
            .field("cause", &self.cause)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for InterruptedToolResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.cause, f)
    }
}
impl std::error::Error for InterruptedToolResult {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

#[derive(Debug)]
struct IncompleteToolSettlement {
    cause: anyhow::Error,
}
impl std::fmt::Display for IncompleteToolSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::get_required_cli_string(
            "estop-tool-settlement-incomplete",
        ))
    }
}
impl std::error::Error for IncompleteToolSettlement {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

/// Ordinary tools are cancelled by dropping their owned future. Composite
/// tools explicitly promise cooperative settlement and receive a bounded
/// opportunity to return completed child evidence, with no further admission.
pub(crate) async fn run_tool(
    tool: &dyn zeroclaw_api::tool::Tool,
    token: Option<&tokio_util::sync::CancellationToken>,
    future: impl Future<Output = anyhow::Result<zeroclaw_api::tool::ToolResult>>,
) -> anyhow::Result<zeroclaw_api::tool::ToolResult> {
    let runtime = current();
    let invocation = current_invocation().unwrap_or_default();
    if let Some(token) = token {
        invocation.bind_caller_token(token);
    }
    if let Some(runtime) = &runtime
        && let Err(error) = runtime.check(Some(tool.name()))
    {
        invocation.request_estop(runtime, tool.name());
        return Err(error);
    }
    invocation.check()?;
    if token.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        invocation.request(InvocationStop::User);
        return Err(crate::agent::loop_::ToolLoopCancelled.into());
    }
    let deadline = zeroclaw_api::deadline::current();
    if deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
        invocation.request(InvocationStop::Deadline);
        return Err(zeroclaw_api::deadline::DeadlineExceeded {
            phase: zeroclaw_api::deadline::Phase::Tool,
            started: false,
        }
        .into());
    }
    let stopped = async {
        let estop = async {
            if let Some(runtime) = &runtime {
                runtime.interrupted(Some(tool.name())).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let cancelled = async {
            if let Some(token) = token {
                token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let expired = async {
            if let Some(deadline) = deadline {
                tokio::time::sleep_until(deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            () = estop => invocation.request_estop(runtime.as_ref().expect("estop future only returns with authority"), tool.name()),
            _ = invocation.interrupted() => {},
            () = cancelled => invocation.request(InvocationStop::User),
            () = expired => invocation.request(InvocationStop::Deadline),
        }
    };
    // Explicit scope also lets composite adapters capture these handles before
    // spawning children (Tokio does not inherit task-local values).
    let execution = scope_invocation(
        Some(invocation.clone()),
        scope_with_tool(runtime.clone(), Some(tool.name()), future),
    );
    tokio::pin!(execution);
    tokio::select! {
        biased;
        () = stopped => {},
        result = &mut execution => {
            // A nested MCP/child wait can observe the stop before this monitor
            // wakes. Publish that terminal observation to sibling settlement
            // and detached recovery before returning the original evidence.
            if let Err(error) = &result {
                if is_estop_interrupted(error) {
                    if let Some(runtime) = &runtime { invocation.request_estop(runtime, tool.name()); }
                } else if crate::agent::loop_::is_tool_loop_cancelled(error) {
                    invocation.request(InvocationStop::User);
                } else if error.is::<zeroclaw_api::deadline::DeadlineExceeded>()
                    || error.chain().any(|cause| cause.is::<zeroclaw_api::deadline::DeadlineExceeded>()) {
                    invocation.request(InvocationStop::Deadline);
                }
            }
            return result;
        },
    }
    let cause = invocation
        .state
        .lock()
        .as_ref()
        .expect("observed stop has a cause")
        .reason;
    if !tool.supports_cooperative_settlement() {
        return Err(cause.error());
    }
    // A composite must abort and drain its children within a shorter bound;
    // this final guard also contains broken custom implementations.
    match tokio::time::timeout(Duration::from_secs(2), &mut execution).await {
        Ok(Err(error)) => Err(cause.with_context(error)),
        Ok(Ok(completed)) => Err(InterruptedToolResult {
            completed,
            cause: cause.error(),
        }
        .into()),
        Err(_) => Err(IncompleteToolSettlement {
            cause: cause.error(),
        }
        .into()),
    }
}

/// Terminal interruption, not an ordinary tool failure eligible for retry.
#[derive(Debug)]
pub(crate) struct EstopInterrupted;

impl std::fmt::Display for EstopInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::get_required_cli_string(
            "estop-runtime-interrupted",
        ))
    }
}

impl std::error::Error for EstopInterrupted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&crate::agent::loop_::ToolLoopCancelled)
    }
}

pub(crate) fn is_estop_interrupted(error: &anyhow::Error) -> bool {
    // anyhow contexts support downcasting but are not source-chain nodes.
    error.is::<EstopInterrupted>() || error.chain().any(|cause| cause.is::<EstopInterrupted>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estop_terminal_cause_survives_anyhow_context_and_direct_source_wrappers() {
        #[derive(Debug)]
        struct WrappedStop(EstopInterrupted);
        impl std::fmt::Display for WrappedStop {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("wrapper")
            }
        }
        impl std::error::Error for WrappedStop {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        for error in [
            anyhow::Error::new(EstopInterrupted),
            anyhow::Error::new(crate::agent::loop_::ToolLoopCancelled).context(EstopInterrupted),
            anyhow::Error::msg("original terminal evidence").context(EstopInterrupted),
            anyhow::Error::new(EstopInterrupted).context("outer context"),
            anyhow::Error::new(WrappedStop(EstopInterrupted)),
        ] {
            assert!(is_estop_interrupted(&error));
            assert!(crate::agent::loop_::is_tool_loop_cancelled(&error));
        }
        assert!(!is_estop_interrupted(&anyhow::Error::msg(
            "ordinary failure"
        )));
    }
    #[tokio::test]
    async fn estop_invocation_retains_first_caller_binding_after_runner_handoff() {
        let invocation = InvocationCancellation::default();
        let caller = tokio_util::sync::CancellationToken::new();
        let nested = tokio_util::sync::CancellationToken::new();
        invocation.bind_caller_token(&caller);
        invocation.bind_caller_token(&nested);
        nested.cancel();
        assert!(
            invocation.check().is_ok(),
            "nested binding cannot replace the originating caller"
        );
        let wait = invocation.interrupted();
        let stop = async {
            tokio::task::yield_now().await;
            caller.cancel();
        };
        let (cause, ()) =
            tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(wait, stop) })
                .await
                .unwrap();
        assert!(crate::agent::loop_::is_tool_loop_cancelled(&cause));
        assert!(invocation.check().is_err());
    }

    #[test]
    fn estop_turn_origin_preserves_first_cause_and_rechecks_fresh_resume() {
        use crate::security::estop::{EstopLevel, EstopManager, ResumeSelector};
        let directory = tempfile::tempdir().unwrap();
        let mut config = zeroclaw_config::schema::Config {
            config_path: directory.path().join("config.toml"),
            data_dir: directory.path().join("data"),
            ..Default::default()
        };
        config.security.estop.enabled = true;
        config.security.estop.require_otp_to_resume = false;
        config.security.estop.state_file = directory
            .path()
            .join("invocation-estop.json")
            .to_string_lossy()
            .into_owned();
        let mut manager = EstopManager::load(&config.security.estop, directory.path()).unwrap();
        let runtime = EstopRuntime::from_config(&config);
        manager.engage(EstopLevel::KillAll).unwrap();
        let invocation = InvocationCancellation::default();
        invocation.request_estop_turn(&runtime);
        invocation.request_user();
        invocation.request_deadline();
        assert!(is_estop_interrupted(&invocation.check().unwrap_err()));
        assert!(invocation.check_replacement().is_err());
        manager.resume(ResumeSelector::KillAll, None, None).unwrap();
        assert!(invocation.check_replacement().is_ok());
        assert!(
            is_estop_interrupted(&invocation.check().unwrap_err()),
            "fresh maintenance admission never clears the interrupted invocation"
        );
    }
}
