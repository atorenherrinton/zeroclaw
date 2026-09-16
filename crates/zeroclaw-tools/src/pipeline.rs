use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::PipelineConfig;

use crate::tool_search::ToolAccessPolicy;

/// Errors specific to pipeline execution.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
pub enum PipelineError {
    #[error("Unknown tool '{0}' is not on the allowed list")]
    UnknownTool(String),
    #[error("Pipeline exceeds maximum of {0} steps")]
    TooManySteps(usize),
    #[error("Invalid template reference: {0}")]
    InvalidTemplate(String),
    #[error("Step {index} ({tool}) failed: {message}")]
    StepFailed {
        index: usize,
        tool: String,
        message: String,
    },
}

/// A single step in a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    pub tool: String,
    pub args: serde_json::Value,
}

/// The pipeline request payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRequest {
    pub steps: Vec<PipelineStep>,
    #[serde(default)]
    pub parallel: bool,
    /// What to include in the tool output. Defaults to every step's result.
    #[serde(default)]
    pub result: PipelineResultMode,
}

/// Controls what `execute_pipeline` returns to the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineResultMode {
    /// Return every step's result as a JSON array (default; backward compatible).
    #[default]
    All,
    /// Return only the final step's raw output. Use when earlier steps produce
    /// large intermediate blobs (e.g. base64) that must not flow back into the
    /// model context.
    Last,
}

/// Result of a single pipeline step.
#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub index: usize,
    pub tool: String,
    pub success: bool,
    pub output: String,
}

/// Invocation-owned authority supplied by the runtime, not pipeline policy state.
/// Resolve before spawning; each child receives the same authority handle.
#[async_trait]
pub trait PipelineExecutionContext: Send + Sync {
    async fn execute(&self, tool: &dyn Tool, args: serde_json::Value) -> Result<ToolResult>;
    fn is_terminal_error(&self, error: &anyhow::Error) -> bool;
    fn check_interrupted(&self) -> Result<()> {
        Ok(())
    }
    async fn interrupted(&self) -> anyhow::Error {
        std::future::pending().await
    }
}

const CHILD_SETTLEMENT_LIMIT: Duration = Duration::from_millis(500);

pub type PipelineExecutionContextResolver =
    Arc<dyn Fn() -> Option<Arc<dyn PipelineExecutionContext>> + Send + Sync>;

/// Original child error retained without flattening its typed cause.
pub struct PipelineStepFailure {
    pub index: usize,
    pub tool: String,
    pub error: anyhow::Error,
}

/// Completed child result retained in its original structured representation.
pub struct PipelineStepOutcome {
    pub index: usize,
    pub tool: String,
    pub result: ToolResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineUnsettledReason {
    Aborted,
    SettlementTimeout,
}

/// Execution ended without a returned result; completed external effects are
/// unknown. This is separate from both completed results and unstarted tails.
#[derive(Debug)]
pub struct PipelineUnsettledStep {
    pub index: usize,
    pub tool: String,
    pub reason: PipelineUnsettledReason,
}

/// Terminal execution retains completed sibling outcomes and all child errors.
/// Display/Debug never expand completed tool payloads; the first source is the
/// terminal cause so callers can keep their existing typed cancellation routing.
pub struct PipelineTerminalError {
    pub completed: Vec<PipelineStepOutcome>,
    pub failures: Vec<PipelineStepFailure>,
    pub outer_interruption: Option<anyhow::Error>,
    pub unsettled: Vec<PipelineUnsettledStep>,
}

impl std::fmt::Debug for PipelineTerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineTerminalError")
            .field("completed_count", &self.completed.len())
            .field("failure_count", &self.failures.len())
            .field("unsettled_count", &self.unsettled.len())
            .finish()
    }
}

impl std::fmt::Display for PipelineTerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self
            .outer_interruption
            .as_ref()
            .or_else(|| self.failures.first().map(|failure| &failure.error))
        {
            Some(error) => std::fmt::Display::fmt(error, f),
            None => f.write_str("Pipeline interrupted"),
        }
    }
}

impl std::error::Error for PipelineTerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.outer_interruption
            .as_ref()
            .or_else(|| self.failures.first().map(|failure| &failure.error))
            .map(|error| error.as_ref())
    }
}

enum PipelineExecutionError {
    Ordinary(PipelineError),
    Terminal(PipelineTerminalError),
}

impl From<PipelineError> for PipelineExecutionError {
    fn from(error: PipelineError) -> Self {
        Self::Ordinary(error)
    }
}

fn current_interruption(
    context: &Option<Arc<dyn PipelineExecutionContext>>,
) -> Option<anyhow::Error> {
    context
        .as_ref()
        .and_then(|context| context.check_interrupted().err())
}

async fn invocation_interrupted(
    context: &Option<Arc<dyn PipelineExecutionContext>>,
) -> anyhow::Error {
    if let Some(context) = context {
        context.interrupted().await
    } else {
        std::future::pending().await
    }
}

fn interrupted_execution(
    completed: Vec<PipelineStepOutcome>,
    failures: Vec<PipelineStepFailure>,
    outer_interruption: Option<anyhow::Error>,
    unsettled: Vec<PipelineUnsettledStep>,
) -> PipelineExecutionError {
    PipelineExecutionError::Terminal(PipelineTerminalError {
        completed,
        failures,
        outer_interruption,
        unsettled,
    })
}

/// The execute_pipeline tool that runs multi-step tool chains.
pub struct PipelineTool {
    config: PipelineConfig,
    tools: Vec<Arc<dyn Tool>>,
    allowed_set: HashSet<String>,
    access_policy: Option<ToolAccessPolicy>,
    execution_context: Option<PipelineExecutionContextResolver>,
}

impl PipelineTool {
    pub const NAME: &'static str = "execute_pipeline";

    pub fn new(config: PipelineConfig, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self::with_access_policy(config, tools, None)
    }

    pub fn with_access_policy(
        config: PipelineConfig,
        tools: Vec<Arc<dyn Tool>>,
        access_policy: Option<ToolAccessPolicy>,
    ) -> Self {
        let allowed_set: HashSet<String> = config.allowed_tools.iter().cloned().collect();
        Self {
            config,
            tools,
            allowed_set,
            access_policy,
            execution_context: None,
        }
    }

    pub fn with_execution_context_resolver(
        mut self,
        resolver: PipelineExecutionContextResolver,
    ) -> Self {
        self.execution_context = Some(resolver);
        self
    }

    /// Find a tool by name in the registry.
    fn find_tool(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    fn policy_allows_exact_name(&self, name: &str) -> bool {
        let Some(policy) = self.access_policy.as_ref() else {
            return true;
        };
        let denied = policy
            .denied
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool == name));
        let allowed = policy
            .allowed
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|tool| tool == name));
        let caller_allowed = policy
            .caller_allowed
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|tool| tool == name));
        !denied && allowed && caller_allowed
    }

    /// Validate the pipeline request before execution.
    fn validate(&self, request: &PipelineRequest) -> std::result::Result<(), PipelineError> {
        if request.steps.len() > self.config.max_steps {
            return Err(PipelineError::TooManySteps(self.config.max_steps));
        }

        // Validate the full request before any sequential or parallel step starts.
        for step in &request.steps {
            let globally_allowed = self.allowed_set.contains(&step.tool);
            let caller_allowed = self.policy_allows_exact_name(&step.tool);
            let child_exists = self.find_tool(&step.tool).is_some();
            if !globally_allowed || !caller_allowed || !child_exists {
                return Err(PipelineError::UnknownTool(step.tool.clone()));
            }
        }

        Ok(())
    }

    /// Execute steps sequentially, interpolating results.
    async fn execute_sequential(
        &self,
        steps: &[PipelineStep],
        context: Option<Arc<dyn PipelineExecutionContext>>,
    ) -> std::result::Result<Vec<PipelineStepOutcome>, PipelineExecutionError> {
        let mut results: Vec<PipelineStepOutcome> = Vec::with_capacity(steps.len());

        for (i, step) in steps.iter().enumerate() {
            if let Some(error) = current_interruption(&context) {
                return Err(interrupted_execution(
                    results,
                    Vec::new(),
                    Some(error),
                    Vec::new(),
                ));
            }
            let tool = self
                .find_tool(&step.tool)
                .ok_or_else(|| PipelineError::UnknownTool(step.tool.clone()))?;

            // Interpolate previous step results into args.
            let outputs: Vec<(usize, &str)> = results
                .iter()
                .map(|step| (step.index, step.result.output.as_str()))
                .collect();
            let interpolated_args = interpolate_from_outputs(&step.args, &outputs);

            let execution = async {
                if let Some(context) = &context {
                    context.execute(tool, interpolated_args).await
                } else {
                    tool.execute(interpolated_args).await
                }
            };
            tokio::pin!(execution);
            let execution = tokio::select! {
                biased;
                result = &mut execution => result,
                cause = invocation_interrupted(&context) => {
                    let mut failures = Vec::new();
                    let mut unsettled = Vec::new();
                    if tool.supports_cooperative_settlement() {
                        match tokio::time::timeout(CHILD_SETTLEMENT_LIMIT, &mut execution).await {
                            Ok(Ok(result)) => results.push(PipelineStepOutcome { index: i, tool: step.tool.clone(), result }),
                            Ok(Err(error)) => failures.push(PipelineStepFailure { index: i, tool: step.tool.clone(), error }),
                            Err(_) => unsettled.push(PipelineUnsettledStep { index: i, tool: step.tool.clone(), reason: PipelineUnsettledReason::SettlementTimeout }),
                        }
                    } else {
                        unsettled.push(PipelineUnsettledStep { index: i, tool: step.tool.clone(), reason: PipelineUnsettledReason::Aborted });
                    }
                    return Err(interrupted_execution(results, failures, Some(cause), unsettled));
                }
            };
            if let Some(cause) = current_interruption(&context) {
                let mut failures = Vec::new();
                match execution {
                    Ok(result) => results.push(PipelineStepOutcome {
                        index: i,
                        tool: step.tool.clone(),
                        result,
                    }),
                    Err(error) => failures.push(PipelineStepFailure {
                        index: i,
                        tool: step.tool.clone(),
                        error,
                    }),
                }
                return Err(interrupted_execution(
                    results,
                    failures,
                    Some(cause),
                    Vec::new(),
                ));
            }
            let tool_result = match execution {
                Ok(result) => result,
                Err(error)
                    if context
                        .as_ref()
                        .is_some_and(|c| c.is_terminal_error(&error)) =>
                {
                    return Err(interrupted_execution(
                        results,
                        vec![PipelineStepFailure {
                            index: i,
                            tool: step.tool.clone(),
                            error,
                        }],
                        None,
                        Vec::new(),
                    ));
                }
                Err(error) => {
                    return Err(PipelineError::StepFailed {
                        index: i,
                        tool: step.tool.clone(),
                        message: error.to_string(),
                    }
                    .into());
                }
            };

            if !tool_result.success {
                return Err(PipelineError::StepFailed {
                    index: i,
                    tool: step.tool.clone(),
                    message: tool_result
                        .error
                        .unwrap_or_else(|| tool_result.output.clone().into_string()),
                }
                .into());
            }

            results.push(PipelineStepOutcome {
                index: i,
                tool: step.tool.clone(),
                result: tool_result,
            });
        }

        Ok(results)
    }

    /// Execute independent steps in parallel (no interpolation between them).
    async fn execute_parallel(
        &self,
        steps: &[PipelineStep],
        context: Option<Arc<dyn PipelineExecutionContext>>,
    ) -> std::result::Result<Vec<PipelineStepOutcome>, PipelineExecutionError> {
        use tokio::task::{AbortHandle, JoinSet};

        let mut join_set = JoinSet::new();
        let mut pending: BTreeMap<usize, (String, bool, AbortHandle)> = BTreeMap::new();
        let mut outer_interruption = current_interruption(&context);
        for (index, step) in steps.iter().enumerate() {
            if outer_interruption.is_none() {
                outer_interruption = current_interruption(&context);
            }
            if outer_interruption.is_some() {
                break;
            }
            let tool = self
                .tools
                .iter()
                .find(|tool| tool.name() == step.tool)
                .cloned()
                .ok_or_else(|| PipelineError::UnknownTool(step.tool.clone()))?;
            let cooperative = tool.supports_cooperative_settlement();
            let name = step.tool.clone();
            let args = step.args.clone();
            let child_context = context.clone();
            let handle = join_set.spawn(async move {
                let result = if let Some(context) = child_context {
                    context.execute(tool.as_ref(), args).await
                } else {
                    tool.execute(args).await
                };
                (index, result)
            });
            pending.insert(index, (name, cooperative, handle));
        }

        let mut results = Vec::with_capacity(steps.len());
        let mut first_failure = None;
        let mut terminal = Vec::new();
        let mut other_errors = Vec::new();
        let mut unsettled = Vec::new();
        let mut fast_stop = false;
        let mut settlement_timed_out = false;
        let mut settlement_deadline = outer_interruption
            .as_ref()
            .map(|_| tokio::time::Instant::now() + CHILD_SETTLEMENT_LIMIT);
        if outer_interruption.is_some() {
            for (_, cooperative, handle) in pending.values() {
                if !cooperative {
                    handle.abort();
                }
            }
        }

        loop {
            let joined = if fast_stop {
                // Retain ready outcomes even when a child misses its settlement
                // budget. Never wait indefinitely on synchronous child code.
                join_set.try_join_next()
            } else if let Some(deadline) = settlement_deadline {
                tokio::select! {
                    biased;
                    result = join_set.join_next() => result,
                    () = tokio::time::sleep_until(deadline) => {
                        settlement_timed_out = true;
                        fast_stop = true;
                        join_set.abort_all();
                        continue;
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    cause = invocation_interrupted(&context) => {
                        outer_interruption = Some(cause);
                        settlement_deadline = Some(tokio::time::Instant::now() + CHILD_SETTLEMENT_LIMIT);
                        // Cooperative children need to return their own nested
                        // evidence. Ordinary children retain immediate abort.
                        for (_, cooperative, handle) in pending.values() {
                            if !cooperative { handle.abort(); }
                        }
                        continue;
                    }
                    result = join_set.join_next() => result,
                }
            };
            let Some(joined) = joined else { break };
            let (index, tool_name, returned) = match joined {
                Ok((index, returned)) => {
                    let Some((name, _, _)) = pending.remove(&index) else {
                        continue;
                    };
                    (index, name, returned)
                }
                Err(error) => {
                    let index = pending.iter().find_map(|(index, (_, _, handle))| {
                        (handle.id() == error.id()).then_some(*index)
                    });
                    let Some((index, (name, _, _))) =
                        index.and_then(|index| pending.remove(&index).map(|entry| (index, entry)))
                    else {
                        continue;
                    };
                    if error.is_cancelled() {
                        unsettled.push(PipelineUnsettledStep {
                            index,
                            tool: name,
                            reason: if settlement_timed_out {
                                PipelineUnsettledReason::SettlementTimeout
                            } else {
                                PipelineUnsettledReason::Aborted
                            },
                        });
                    } else {
                        first_failure.get_or_insert_with(|| PipelineError::StepFailed {
                            index,
                            tool: name.clone(),
                            message: format!("Task join error: {error}"),
                        });
                        other_errors.push(PipelineStepFailure {
                            index,
                            tool: name,
                            error: error.into(),
                        });
                        if outer_interruption.is_none() {
                            fast_stop = true;
                            join_set.abort_all();
                        }
                    }
                    continue;
                }
            };

            match returned {
                Ok(result) => {
                    let failed = !result.success;
                    if failed {
                        first_failure.get_or_insert_with(|| PipelineError::StepFailed {
                            index,
                            tool: tool_name.clone(),
                            message: result
                                .error
                                .as_deref()
                                .unwrap_or(result.output.as_str())
                                .to_owned(),
                        });
                    }
                    results.push(PipelineStepOutcome {
                        index,
                        tool: tool_name,
                        result,
                    });
                    if failed && outer_interruption.is_none() {
                        fast_stop = true;
                        join_set.abort_all();
                    }
                }
                Err(error) => {
                    let is_terminal = context
                        .as_ref()
                        .is_some_and(|context| context.is_terminal_error(&error));
                    if !is_terminal {
                        first_failure.get_or_insert_with(|| PipelineError::StepFailed {
                            index,
                            tool: tool_name.clone(),
                            message: error.to_string(),
                        });
                    }
                    let failure = PipelineStepFailure {
                        index,
                        tool: tool_name,
                        error,
                    };
                    if is_terminal {
                        terminal.push(failure);
                    } else {
                        other_errors.push(failure);
                    }
                    if outer_interruption.is_none() {
                        outer_interruption = current_interruption(&context);
                        if outer_interruption.is_some() {
                            settlement_deadline =
                                Some(tokio::time::Instant::now() + CHILD_SETTLEMENT_LIMIT);
                            for (_, cooperative, handle) in pending.values() {
                                if !cooperative {
                                    handle.abort();
                                }
                            }
                        } else {
                            fast_stop = true;
                            join_set.abort_all();
                        }
                    }
                }
            }
        }

        for (index, (tool, _, handle)) in pending {
            handle.abort();
            unsettled.push(PipelineUnsettledStep {
                index,
                tool,
                reason: if settlement_timed_out {
                    PipelineUnsettledReason::SettlementTimeout
                } else {
                    PipelineUnsettledReason::Aborted
                },
            });
        }
        results.sort_by_key(|step| step.index);
        unsettled.sort_by_key(|step| step.index);
        if outer_interruption.is_none() {
            outer_interruption = current_interruption(&context);
        }
        if outer_interruption.is_some() || !terminal.is_empty() {
            terminal.extend(other_errors);
            return Err(interrupted_execution(
                results,
                terminal,
                outer_interruption,
                unsettled,
            ));
        }
        if let Some(error) = first_failure {
            return Err(error.into());
        }
        Ok(results)
    }
}

#[async_trait]
impl Tool for PipelineTool {
    fn supports_cooperative_settlement(&self) -> bool {
        self.execution_context.is_some()
    }

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Execute a multi-step tool pipeline in a single call. Steps run sequentially by default \
         with result interpolation (use {{step[N].result}} to reference prior outputs), \
         or in parallel when 'parallel: true' is set. Set 'result: \"last\"' to return only the \
         final step's output (recommended when an earlier step yields a large blob, e.g. base64, \
         that should not flow back into the context); the default 'all' returns every step's result."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "Ordered list of tool invocations",
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": {
                                "type": "string",
                                "description": "Name of the tool to invoke"
                            },
                            "args": {
                                "type": "object",
                                "description": "Arguments to pass to the tool. Use {{step[N].result}} to interpolate prior step outputs."
                            }
                        },
                        "required": ["tool", "args"]
                    }
                },
                "parallel": {
                    "type": "boolean",
                    "description": "Run steps in parallel (no interpolation). Default: false",
                    "default": false
                },
                "result": {
                    "type": "string",
                    "enum": ["all", "last"],
                    "description": "What to return: 'all' (default) = every step's result as JSON; 'last' = only the final step's raw output. Use 'last' to keep large intermediate blobs (e.g. base64) out of the context.",
                    "default": "all"
                }
            },
            "required": ["steps"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let request: PipelineRequest = serde_json::from_value(args).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "pipeline: invalid request"
            );
            anyhow::Error::msg(format!("Invalid pipeline request: {e}"))
        })?;

        // Validate before execution.
        if let Err(e) = self.validate(&request) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(e.to_string()),
            });
        }

        let context = self
            .execution_context
            .as_ref()
            .and_then(|resolve| resolve());
        let results = if request.parallel {
            self.execute_parallel(&request.steps, context).await
        } else {
            self.execute_sequential(&request.steps, context).await
        };

        match results {
            Ok(outcomes) => {
                // Materialize the existing text-only success contract only
                // after completion. Terminal errors retain original results.
                let step_results: Vec<StepResult> = outcomes
                    .into_iter()
                    .map(|step| StepResult {
                        index: step.index,
                        tool: step.tool,
                        success: step.result.success,
                        output: step.result.output.into_string(),
                    })
                    .collect();
                let output = match request.result {
                    PipelineResultMode::Last => step_results
                        .last()
                        .map(|s| s.output.clone())
                        .unwrap_or_default(),
                    PipelineResultMode::All => serde_json::to_string_pretty(&step_results)
                        .unwrap_or_else(|_| "Pipeline completed".to_string()),
                };
                Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                })
            }
            Err(PipelineExecutionError::Terminal(error)) => Err(error.into()),
            Err(PipelineExecutionError::Ordinary(e)) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(e.to_string()),
            }),
        }
    }
}

/// Interpolate `{{step[N].result}}` references in tool arguments.
/// Single-pass replacement: values containing `{{` after substitution are stripped
/// to prevent injection.
pub fn interpolate_args(
    args: &serde_json::Value,
    prior_results: &[StepResult],
) -> serde_json::Value {
    let outputs: Vec<(usize, &str)> = prior_results
        .iter()
        .map(|step| (step.index, step.output.as_str()))
        .collect();
    interpolate_from_outputs(args, &outputs)
}

fn interpolate_from_outputs(
    args: &serde_json::Value,
    prior_outputs: &[(usize, &str)],
) -> serde_json::Value {
    match args {
        serde_json::Value::String(s) => {
            let interpolated = interpolate_string(s, prior_outputs);
            serde_json::Value::String(interpolated)
        }
        serde_json::Value::Object(map) => {
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), interpolate_from_outputs(v, prior_outputs)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            let new_arr: Vec<serde_json::Value> = arr
                .iter()
                .map(|v| interpolate_from_outputs(v, prior_outputs))
                .collect();
            serde_json::Value::Array(new_arr)
        }
        other => other.clone(),
    }
}

/// Perform single-pass interpolation of `{{step[N].result}}` in a string.
fn interpolate_string(s: &str, prior_outputs: &[(usize, &str)]) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '{'
            && let Some(&(_, '{')) = chars.peek()
        {
            // Found `{{` — try to match `{{step[N].result}}`
            let rest = &s[i..];
            if let Some(end) = find_template_end(rest) {
                let template = &rest[2..end]; // strip {{ and }}
                if let Some(value) = resolve_template_output(template, prior_outputs) {
                    // Strip any `{{` in the resolved value to prevent injection.
                    result.push_str(&value.replace("{{", ""));
                    // Skip past the closing `}}`
                    let skip_to = i + end + 2;
                    while chars.peek().is_some_and(|&(idx, _)| idx < skip_to) {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        result.push(c);
    }

    result
}

/// Find the position of `}}` in a string starting with `{{`.
fn find_template_end(s: &str) -> Option<usize> {
    s[2..].find("}}").map(|pos| pos + 2)
}

/// Resolve a template reference like `step[0].result`.
fn resolve_template_output(template: &str, prior_outputs: &[(usize, &str)]) -> Option<String> {
    let template = template.trim();
    if !template.starts_with("step[") || !template.ends_with(".result") {
        return None;
    }

    let bracket_end = template.find(']')?;
    let index_str = &template[5..bracket_end];
    let index: usize = index_str.parse().ok()?;

    prior_outputs
        .iter()
        .find(|(step_index, _)| *step_index == index)
        .map(|(_, output)| (*output).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Interpolation ──────────────────────────────────────

    #[test]
    fn interpolate_simple_reference() {
        let results = vec![StepResult {
            index: 0,
            tool: "web_search".to_string(),
            success: true,
            output: "search results here".to_string(),
        }];

        let args = serde_json::json!({"text": "Summarize: {{step[0].result}}"});
        let interpolated = interpolate_args(&args, &results);
        assert_eq!(
            interpolated["text"].as_str().unwrap(),
            "Summarize: search results here"
        );
    }

    #[test]
    fn interpolate_multiple_references() {
        let results = vec![
            StepResult {
                index: 0,
                tool: "a".to_string(),
                success: true,
                output: "first".to_string(),
            },
            StepResult {
                index: 1,
                tool: "b".to_string(),
                success: true,
                output: "second".to_string(),
            },
        ];

        let args = serde_json::json!({"text": "{{step[0].result}} and {{step[1].result}}"});
        let interpolated = interpolate_args(&args, &results);
        assert_eq!(interpolated["text"].as_str().unwrap(), "first and second");
    }

    #[test]
    fn interpolate_no_match_passes_through() {
        let args = serde_json::json!({"text": "no templates here"});
        let interpolated = interpolate_args(&args, &[]);
        assert_eq!(interpolated["text"].as_str().unwrap(), "no templates here");
    }

    #[test]
    fn interpolate_invalid_index_passes_through() {
        let args = serde_json::json!({"text": "{{step[99].result}}"});
        let interpolated = interpolate_args(&args, &[]);
        // Invalid reference is left as-is.
        assert_eq!(
            interpolated["text"].as_str().unwrap(),
            "{{step[99].result}}"
        );
    }

    #[test]
    fn interpolate_strips_injection() {
        let results = vec![StepResult {
            index: 0,
            tool: "a".to_string(),
            success: true,
            output: "value with {{step[1].result}} injection".to_string(),
        }];

        let args = serde_json::json!({"text": "{{step[0].result}}"});
        let interpolated = interpolate_args(&args, &results);
        // The `{{` in the resolved value should be stripped.
        let text = interpolated["text"].as_str().unwrap();
        assert!(!text.contains("{{"));
        assert!(text.contains("step[1].result}} injection"));
    }

    #[test]
    fn interpolate_nested_objects() {
        let results = vec![StepResult {
            index: 0,
            tool: "a".to_string(),
            success: true,
            output: "data".to_string(),
        }];

        let args = serde_json::json!({
            "outer": {
                "inner": "prefix {{step[0].result}} suffix"
            }
        });
        let interpolated = interpolate_args(&args, &results);
        assert_eq!(
            interpolated["outer"]["inner"].as_str().unwrap(),
            "prefix data suffix"
        );
    }

    #[test]
    fn interpolate_array_values() {
        let results = vec![StepResult {
            index: 0,
            tool: "a".to_string(),
            success: true,
            output: "item".to_string(),
        }];

        let args = serde_json::json!(["{{step[0].result}}", "static"]);
        let interpolated = interpolate_args(&args, &results);
        assert_eq!(interpolated[0].as_str().unwrap(), "item");
        assert_eq!(interpolated[1].as_str().unwrap(), "static");
    }

    // ── Validation ─────────────────────────────────────────

    #[test]
    fn validate_too_many_steps() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 2,
            allowed_tools: vec!["shell".to_string()],
        };
        let tool = PipelineTool::new(config, vec![]);

        let request = PipelineRequest {
            steps: vec![
                PipelineStep {
                    tool: "shell".into(),
                    args: serde_json::json!({}),
                },
                PipelineStep {
                    tool: "shell".into(),
                    args: serde_json::json!({}),
                },
                PipelineStep {
                    tool: "shell".into(),
                    args: serde_json::json!({}),
                },
            ],
            parallel: false,
            result: PipelineResultMode::default(),
        };

        let err = tool.validate(&request).unwrap_err();
        assert!(matches!(err, PipelineError::TooManySteps(2)));
    }

    #[test]
    fn validate_unknown_tool() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec!["shell".to_string()],
        };
        let tool = PipelineTool::new(config, vec![]);

        let request = PipelineRequest {
            steps: vec![PipelineStep {
                tool: "forbidden_tool".into(),
                args: serde_json::json!({}),
            }],
            parallel: false,
            result: PipelineResultMode::default(),
        };

        let err = tool.validate(&request).unwrap_err();
        assert!(matches!(err, PipelineError::UnknownTool(_)));
    }

    #[test]
    fn validate_valid_request() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec!["shell".to_string(), "file_read".to_string()],
        };
        let tool = PipelineTool::new(
            config,
            vec![
                Arc::new(EchoTool {
                    name: "shell".into(),
                    output: String::new(),
                }),
                Arc::new(EchoTool {
                    name: "file_read".into(),
                    output: String::new(),
                }),
            ],
        );

        let request = PipelineRequest {
            steps: vec![
                PipelineStep {
                    tool: "shell".into(),
                    args: serde_json::json!({}),
                },
                PipelineStep {
                    tool: "file_read".into(),
                    args: serde_json::json!({}),
                },
            ],
            parallel: false,
            result: PipelineResultMode::default(),
        };

        assert!(tool.validate(&request).is_ok());
    }

    #[test]
    fn validate_empty_pipeline() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec![],
        };
        let tool = PipelineTool::new(config, vec![]);

        let request = PipelineRequest {
            steps: vec![],
            parallel: false,
            result: PipelineResultMode::default(),
        };

        assert!(tool.validate(&request).is_ok());
    }

    #[test]
    fn validate_rejects_step_denied_by_agent_policy() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec!["shell".to_string(), "file_read".to_string()],
        };
        let policy = ToolAccessPolicy {
            allowed: Some(vec![
                "file_read".to_string(),
                PipelineTool::NAME.to_string(),
            ]),
            ..ToolAccessPolicy::default()
        };
        let tool = PipelineTool::with_access_policy(
            config,
            vec![Arc::new(EchoTool {
                name: "shell".into(),
                output: String::new(),
            })],
            Some(policy),
        );
        let request = PipelineRequest {
            steps: vec![PipelineStep {
                tool: "shell".into(),
                args: serde_json::json!({}),
            }],
            parallel: false,
            result: PipelineResultMode::default(),
        };

        let err = tool.validate(&request).unwrap_err();
        assert!(matches!(err, PipelineError::UnknownTool(ref name) if name == "shell"));
    }

    #[test]
    fn validate_allows_intersection_of_pipeline_and_agent_policy() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec!["shell".to_string(), "file_read".to_string()],
        };
        let policy = ToolAccessPolicy {
            allowed: Some(vec![
                "file_read".to_string(),
                PipelineTool::NAME.to_string(),
            ]),
            ..ToolAccessPolicy::default()
        };
        let tool = PipelineTool::with_access_policy(
            config,
            vec![Arc::new(EchoTool {
                name: "file_read".into(),
                output: String::new(),
            })],
            Some(policy),
        );
        let request = PipelineRequest {
            steps: vec![PipelineStep {
                tool: "file_read".into(),
                args: serde_json::json!({}),
            }],
            parallel: false,
            result: PipelineResultMode::default(),
        };

        assert!(tool.validate(&request).is_ok());
    }

    #[test]
    fn validate_uses_exact_names_for_every_policy_ceiling() {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec!["shell".to_string(), "plugin__danger".to_string()],
        };
        let cases = [
            (
                "namespaced plugin is not MCP-auto-admitted",
                "plugin__danger",
                ToolAccessPolicy {
                    allowed: Some(vec![PipelineTool::NAME.to_string()]),
                    ..ToolAccessPolicy::default()
                },
            ),
            (
                "denylist wins",
                "shell",
                ToolAccessPolicy {
                    denied: Some(vec!["shell".to_string()]),
                    ..ToolAccessPolicy::default()
                },
            ),
            (
                "caller allowlist narrows",
                "shell",
                ToolAccessPolicy {
                    caller_allowed: Some(vec![PipelineTool::NAME.to_string()]),
                    ..ToolAccessPolicy::default()
                },
            ),
            (
                "explicit empty allowlist denies all",
                "shell",
                ToolAccessPolicy {
                    allowed: Some(Vec::new()),
                    ..ToolAccessPolicy::default()
                },
            ),
        ];

        for (case, step, policy) in cases {
            let tool = PipelineTool::with_access_policy(
                config.clone(),
                vec![Arc::new(EchoTool {
                    name: step.to_string(),
                    output: String::new(),
                })],
                Some(policy),
            );
            let request = PipelineRequest {
                steps: vec![PipelineStep {
                    tool: step.to_string(),
                    args: serde_json::json!({}),
                }],
                parallel: false,
                result: PipelineResultMode::default(),
            };
            assert!(
                matches!(tool.validate(&request), Err(PipelineError::UnknownTool(_))),
                "{case}"
            );
        }
    }

    // ── Template resolution ────────────────────────────────

    #[test]
    fn resolve_valid_template() {
        let results = vec![(0, "hello")];
        assert_eq!(
            resolve_template_output("step[0].result", &results),
            Some("hello".to_string())
        );
    }

    #[test]
    fn resolve_invalid_template_format() {
        assert_eq!(resolve_template_output("invalid", &[]), None);
        assert_eq!(resolve_template_output("step.result", &[]), None);
        assert_eq!(resolve_template_output("step[abc].result", &[]), None);
    }

    #[test]
    fn resolve_out_of_range_index() {
        assert_eq!(resolve_template_output("step[5].result", &[]), None);
    }

    // ── Result mode ────────────────────────────────────────

    struct EchoTool {
        name: String,
        output: String,
    }

    zeroclaw_api::mock_tool_attribution!(EchoTool);

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: self.output.clone().into(),
                error: None,
            })
        }
    }

    fn echo_pipeline() -> PipelineTool {
        let config = PipelineConfig {
            enabled: true,
            max_steps: 20,
            allowed_tools: vec!["a".to_string(), "b".to_string()],
        };
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(EchoTool {
                name: "a".into(),
                output: "FIRST_BIG_BLOB".into(),
            }),
            Arc::new(EchoTool {
                name: "b".into(),
                output: "final answer".into(),
            }),
        ];
        PipelineTool::new(config, tools)
    }

    #[tokio::test]
    async fn result_last_returns_only_final_output() {
        let args = serde_json::json!({
            "steps": [
                {"tool": "a", "args": {}},
                {"tool": "b", "args": {}}
            ],
            "result": "last"
        });
        let res = echo_pipeline().execute(args).await.unwrap();
        assert!(res.success);
        assert_eq!(res.output, "final answer");
        assert!(!res.output.contains("FIRST_BIG_BLOB"));
    }

    #[tokio::test]
    async fn result_all_is_default_and_includes_every_step() {
        let args = serde_json::json!({
            "steps": [
                {"tool": "a", "args": {}},
                {"tool": "b", "args": {}}
            ]
        });
        let res = echo_pipeline().execute(args).await.unwrap();
        assert!(res.success);
        assert!(res.output.contains("FIRST_BIG_BLOB"));
        assert!(res.output.contains("final answer"));
    }
}
