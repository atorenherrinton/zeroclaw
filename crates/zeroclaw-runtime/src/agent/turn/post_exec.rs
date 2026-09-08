//! Post-execution recording: result log line, the `after_tool_call` hook, the
//! completion Status, and filling the executed calls' `ordered_results` slots.

use super::call_prep::StreamToolCall;
use super::context::TurnCtx;
use super::events::{ProgressEvent, StreamDelta, send_progress};
use super::results_collect::{OrderedResults, ResultBudgetExceeded, admit_source_results};
use crate::agent::tool_execution::ToolExecutionOutcome;
use zeroclaw_tool_call_parser::ParsedToolCall;

/// Record each executed tool call's outcome (upstream loop body,
/// post-execution section): one `tool_call_result` log line, the
/// `after_tool_call` hook, a completion Status to the draft, and the
/// call's slot in `ordered_results`. Terminal batches retain evidence without
/// awaiting hooks or draft consumers (`publish_auxiliary = false`). Source
/// admission precedes all post-execution payload copies, even for terminal batches.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_executed_outcomes(
    ctx: &TurnCtx<'_>,
    executable_indices: &[usize],
    executable_calls: &[ParsedToolCall],
    stream_calls: &[Option<StreamToolCall>],
    executed_outcomes: Vec<ToolExecutionOutcome>,
    ordered_results: &mut OrderedResults,
    max_tool_result_chars: usize,
    iteration: usize,
    publish_auxiliary: bool,
) -> Result<(), ResultBudgetExceeded> {
    // The ordered vector is the canonical ownership handoff. Populate every
    // completed slot before checking the batch; a later oversized sibling must
    // not let earlier payloads escape into SOP capture, hooks, or progress.
    for ((idx, call), outcome) in executable_indices
        .iter()
        .zip(executable_calls)
        .zip(executed_outcomes)
    {
        ordered_results[*idx] = Some((call.name.clone(), call.tool_call_id.clone(), outcome));
    }
    admit_source_results(ordered_results, max_tool_result_chars)?;

    for ((idx, call), stream_call) in executable_indices
        .iter()
        .zip(executable_calls)
        .zip(stream_calls)
    {
        let Some((_, _, outcome)) = ordered_results[*idx].as_ref() else {
            continue;
        };
        // The pending ToolCall and terminal ToolResult are emitted by the
        // executor (execute_one_tool) at dispatch and completion time so serial
        // batches interleave call->result per tool. Post-exec only records the
        // outcome to history, logs, hooks, and ordered_results.

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                .with_category(::zeroclaw_log::EventCategory::Tool)
                .with_outcome(if outcome.success {
                    ::zeroclaw_log::EventOutcome::Success
                } else {
                    ::zeroclaw_log::EventOutcome::Failure
                })
                .with_duration(u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX))
                .with_attrs(::serde_json::json!({
                    "model": ctx.model,
                    "iteration": iteration + 1,
                    "tool": call.name.clone(),
                    "error_reason": outcome.error_reason.as_deref().map(crate::agent::tool_execution::bounded_observer_text),
                    "output": crate::agent::tool_execution::bounded_observer_text(&outcome.output),
                    "trace_id": ctx.turn_id,
                })),
            "tool_call_result"
        );

        // Capture into the innermost live SOP step scope (no-op otherwise).
        if crate::sop::executor::step_capture_active() {
            crate::sop::executor::record_step_tool_call(
                &call.name,
                &call.arguments,
                outcome.success,
                outcome.output.clone(),
                outcome.output_data.clone(),
                outcome.error_reason.as_deref(),
                u64::try_from(outcome.duration.as_millis()).unwrap_or(u64::MAX),
            );
        }

        // Completed evidence belongs to ordered history before any auxiliary
        // async work. A terminal batch skips hooks/draft progress so backpressure
        // or a stalled hook cannot swallow an already returned result.
        if !publish_auxiliary {
            continue;
        }
        // ── Hook: after_tool_call (void) ─────────────────
        if let Some(hooks) = ctx.hooks {
            let tool_result_obj = crate::tools::ToolResult {
                success: outcome.success,
                output: outcome.output.clone().into(),
                error: None,
            };
            hooks
                .fire_after_tool_call(&call.name, &tool_result_obj, outcome.duration)
                .await;
        }

        // ── Progress: tool completion ───────────────────────
        send_progress(ctx.on_delta, ProgressEvent::Planning).await;
        if let (Some(tx), Some(stream_call)) = (ctx.on_delta, stream_call) {
            let secs = outcome.duration.as_secs();
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_attrs(::serde_json::json!({"tool": call.name, "secs": secs})),
                "Sending progress complete to draft"
            );
            let _ = tx
                .send(StreamDelta::ToolComplete {
                    tool: call.name.clone(),
                    arguments: std::sync::Arc::clone(&stream_call.arguments),
                    tool_provenance: stream_call.tool_provenance,
                    secs,
                    success: outcome.success,
                    error: outcome
                        .error_reason
                        .as_deref()
                        .map(crate::agent::tool_execution::bounded_observer_text),
                })
                .await;
        }
    }
    Ok(())
}
