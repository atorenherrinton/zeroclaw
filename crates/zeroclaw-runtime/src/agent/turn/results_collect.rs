//! Results collection: admit encoded per-tool outputs with receipts,
//! feed the pattern-based loop detector, and run the time-gated
//! identical-output abort.

use crate::agent::history::{
    append_or_merge_system_message, canonicalize_tool_result_media_markers_for,
};
use crate::agent::loop_detector::LoopDetector;
use crate::agent::tool_execution::ToolExecutionOutcome;
use crate::agent::turn::recovery::{RecoveryTracker, RecoveryTrigger};
use anyhow::Result;
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zeroclaw_config::schema::PacingConfig;
use zeroclaw_providers::ChatMessage;
use zeroclaw_tool_call_parser::ParsedToolCall;
use zeroclaw_tools::output_budget::{ROUND_PAYLOAD_BYTES, encoded_size};

pub(crate) type OrderedResults = Vec<Option<(String, Option<String>, ToolExecutionOutcome)>>;

/// Transient ownership of results that cannot be forwarded within the budget.
/// Keep the original typed outcomes and receipts in the terminal error; a size
/// failure must never become evidence that a tool did not execute.
pub(crate) struct ResultBudgetExceeded {
    pub(crate) results: OrderedResults,
    /// Errors that could not be normalized within the hard source ceiling.
    /// Kept opaque to automatic Display/Debug and standard cause traversal.
    pub(crate) errors: Vec<anyhow::Error>,
}

impl ResultBudgetExceeded {
    pub(crate) fn from_error(error: anyhow::Error) -> Self {
        Self {
            results: Vec::new(),
            errors: vec![error],
        }
    }

    /// Keep an already-known terminal batch cause downcastable alongside the
    /// complete result evidence owned by this rejection.
    pub(crate) fn with_prior(self, prior: Option<anyhow::Error>) -> anyhow::Error {
        match prior {
            Some(prior) => prior.context(self),
            None => self.into(),
        }
    }
}

/// Admit the existing ordered batch before any consumer copies its payloads.
/// Failure transfers ownership to the typed error without duplicating evidence.
/// The returned limit is also used for the final history representation.
pub(crate) fn admit_source_results(
    ordered_results: &mut OrderedResults,
    configured_limit: usize,
) -> std::result::Result<usize, ResultBudgetExceeded> {
    let per_result_limit = if configured_limit == 0 {
        32768
    } else {
        configured_limit
    }
    .min(ROUND_PAYLOAD_BYTES);
    if encoded_size(ordered_results, ROUND_PAYLOAD_BYTES).is_none()
        || ordered_results
            .iter()
            .flatten()
            .any(|result| encoded_size(result, per_result_limit).is_none())
    {
        return Err(ResultBudgetExceeded {
            results: std::mem::take(ordered_results),
            errors: Vec::new(),
        });
    }
    Ok(per_result_limit)
}

impl std::fmt::Debug for ResultBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultBudgetExceeded")
            .field("result_count", &self.results.len())
            .field("error_count", &self.errors.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for ResultBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::get_required_cli_string(
            "turn-tool-result-budget-exceeded",
        ))
    }
}

impl std::error::Error for ResultBudgetExceeded {}

/// Check both supported history representations, including the nested JSON
/// string used by native tool messages and the prompt-mode XML/name envelope.
fn history_results_fit(
    individual_results: &[(Option<String>, String)],
    tool_results: &str,
    per_result_limit: usize,
) -> bool {
    let mut native_size = 2usize; // JSON array delimiters
    for (index, (tool_call_id, content)) in individual_results.iter().enumerate() {
        // The source envelope has already passed the allocation-free check.
        let message = ChatMessage::tool(super::history_append::native_tool_result_content(
            tool_call_id.as_deref(),
            content,
        ));
        let Some(size) = encoded_size(&message, per_result_limit) else {
            return false;
        };
        native_size = native_size.saturating_add(size + usize::from(index > 0));
        if native_size > ROUND_PAYLOAD_BYTES {
            return false;
        }
    }
    let prompt = format!("[Tool results]\n{tool_results}");
    encoded_size(
        &ChatMessage::user(prompt),
        ROUND_PAYLOAD_BYTES - 2, // containing history array
    )
    .is_some()
}

/// One round's collected tool results.
pub(crate) struct CollectedResults {
    /// Per-call `(tool_call_id, output)` so native-mode history can emit one
    /// `role=tool` message per call with the correct ID.
    pub(crate) individual_results: Vec<(Option<String>, String)>,
    /// XML `<tool_result>` blocks for prompt-mode history.
    pub(crate) tool_results: String,
    /// Concatenated non-ignored outputs feeding the identical-output hash.
    pub(crate) detection_relevant_output: String,
    /// A typed stuck condition for the repair-only recovery seam. The prompt
    /// builder consumes only this metadata, never history or raw tool data.
    pub(crate) recovery_trigger: Option<RecoveryTrigger>,
}

/// Collect this round's tool results (upstream loop body, results-collection
/// section): feed the loop detector (Warning/Block append system messages;
/// Break yields a typed recovery trigger), canonicalize media markers,
/// append receipts, and admit the complete per-call and XML result forms.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_tool_results(
    mut ordered_results: OrderedResults,
    tool_calls: &[ParsedToolCall],
    history: &mut Vec<ChatMessage>,
    loop_detector: &mut LoopDetector,
    recovery_tracker: &mut RecoveryTracker,
    loop_ignore_tools: &HashSet<&str>,
    max_tool_result_chars: usize,
    collected_receipts: Option<&Mutex<Vec<String>>>,
    model: &str,
    iteration: usize,
    turn_id: &str,
) -> Result<CollectedResults> {
    let per_result_limit = admit_source_results(&mut ordered_results, max_tool_result_chars)?;

    let mut tool_results = String::new();
    let mut individual_results: Vec<(Option<String>, String)> = Vec::new();
    let mut detection_relevant_output = String::new();
    let mut recovery_trigger = None;
    for (tool_name, tool_call_id, outcome) in ordered_results.iter().flatten() {
        let mut result_output =
            canonicalize_tool_result_media_markers_for(tool_name, &outcome.output);
        if let Some(receipt) = &outcome.receipt {
            write!(result_output, "\n\n[receipt: {receipt}]")?;
        }
        let block =
            format!("<tool_result name=\"{tool_name}\">\n{result_output}\n</tool_result>\n");
        let prompt_block = ChatMessage::user(format!("[Tool results]\n{block}"));
        if encoded_size(&prompt_block, per_result_limit).is_none() {
            return Err(ResultBudgetExceeded {
                results: ordered_results,
                errors: Vec::new(),
            }
            .into());
        }
        individual_results.push((tool_call_id.clone(), result_output));
        tool_results.push_str(&block);
    }
    if !history_results_fit(&individual_results, &tool_results, per_result_limit) {
        return Err(ResultBudgetExceeded {
            results: ordered_results,
            errors: Vec::new(),
        }
        .into());
    }

    // Use enumerate before filter_map so slots stay aligned with tool_calls.
    for (result_index, (tool_name, _, outcome)) in ordered_results
        .iter()
        .enumerate()
        .filter_map(|(i, opt)| opt.as_ref().map(|v| (i, v)))
    {
        if recovery_trigger.is_none() {
            recovery_trigger = recovery_tracker.observe(tool_name, outcome, iteration);
        }
        if !loop_ignore_tools.contains(tool_name.as_str()) {
            if outcome.success {
                detection_relevant_output.push_str(&outcome.output);
            }

            let args = tool_calls
                .get(result_index)
                .map(|c| &c.arguments)
                .unwrap_or(&serde_json::Value::Null);
            let det_result = if outcome.success {
                loop_detector.record(tool_name, args, &outcome.output)
            } else {
                crate::agent::loop_detector::LoopDetectionResult::Ok
            };
            match det_result {
                crate::agent::loop_detector::LoopDetectionResult::Ok => {}
                crate::agent::loop_detector::LoopDetectionResult::Warning(ref msg) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(
                                ::serde_json::json!({"tool": tool_name, "msg": msg.to_string()})
                            ),
                        "loop detector warning"
                    );
                    append_or_merge_system_message(history, format!("[Loop Detection] {msg}"));
                }
                crate::agent::loop_detector::LoopDetectionResult::Block(ref msg) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(
                                ::serde_json::json!({"tool": tool_name, "msg": msg.to_string()})
                            ),
                        "loop detector blocked tool call"
                    );
                    // Replace the tool output with the block message.
                    // We still continue the loop so the LLM sees the block feedback.
                    append_or_merge_system_message(
                        history,
                        format!("[Loop Detection — BLOCKED] {msg}"),
                    );
                }
                crate::agent::loop_detector::LoopDetectionResult::Break(msg) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "model": model,
                                "iteration": iteration + 1,
                                "tool": tool_name,
                                "message": msg,
                                "trace_id": turn_id,
                            })),
                        "loop_detector_circuit_breaker"
                    );
                    recovery_trigger.get_or_insert_with(|| {
                        RecoveryTrigger::circuit_breaker(tool_name, &msg, iteration)
                    });
                }
            }
        }
        if let Some(receipt) = &outcome.receipt {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_attrs(::serde_json::json!({"tool": tool_name, "receipt": receipt})),
                "Tool receipt generated"
            );
            if let Some(store) = collected_receipts
                && let Ok(mut receipts) = store.lock()
            {
                receipts.push(format!("{tool_name}: {receipt}"));
            }
        }
    }

    Ok(CollectedResults {
        individual_results,
        tool_results,
        detection_relevant_output,
        recovery_trigger,
    })
}

/// Time-gated identical-output abort (upstream loop body): when
/// `pacing.loop_detection_min_elapsed_secs` has elapsed, hash the
/// detection-relevant output and bail after 3+ consecutive identical rounds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_identical_output_abort(
    detection_relevant_output: &str,
    loop_started_at: Instant,
    pacing: &PacingConfig,
    consecutive_identical_outputs: &mut usize,
    last_tool_output_hash: &mut Option<u64>,
    model: &str,
    iteration: usize,
    turn_id: &str,
) -> Option<RecoveryTrigger> {
    let loop_detection_active = match pacing.loop_detection_min_elapsed_secs {
        Some(min_secs) => loop_started_at.elapsed() >= Duration::from_secs(min_secs),
        None => false, // disabled when not configured (backwards compatible)
    };

    if loop_detection_active && !detection_relevant_output.is_empty() {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        detection_relevant_output.hash(&mut hasher);
        let current_hash = hasher.finish();

        if *last_tool_output_hash == Some(current_hash) {
            *consecutive_identical_outputs += 1;
        } else {
            *consecutive_identical_outputs = 0;
            *last_tool_output_hash = Some(current_hash);
        }

        // Bail if we see 3+ consecutive identical tool outputs (clear runaway).
        if *consecutive_identical_outputs >= 3 {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": model,
                        "iteration": iteration + 1,
                        "consecutive_identical": *consecutive_identical_outputs,
                        "trace_id": turn_id,
                    })),
                "tool_loop_identical_output_abort"
            );
            return Some(RecoveryTrigger::identical_output(
                *consecutive_identical_outputs,
                iteration,
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::loop_detector::{LoopDetector, LoopDetectorConfig};
    use crate::agent::tool_execution::ToolExecutionOutcome;
    use zeroclaw_tool_call_parser::ParsedToolCall;

    fn collect_fixture(ordered: OrderedResults, limit: usize) -> Result<CollectedResults> {
        collect_tool_results(
            ordered,
            &[],
            &mut Vec::new(),
            &mut LoopDetector::new(LoopDetectorConfig::default()),
            &mut RecoveryTracker::default(),
            &HashSet::new(),
            limit,
            None,
            "fixture-model",
            0,
            "fixture-turn",
        )
    }

    #[test]
    fn oversized_source_fields_retain_original_evidence_in_terminal_error() {
        for field in ["name", "id", "error", "data", "receipt"] {
            let huge = "private-fixture\u{0001}😀".repeat(4000);
            let mut result = outcome("confirmed fixture", true);
            result.receipt = Some("fixture-receipt".into());
            let mut name = "fixture-tool".to_owned();
            let mut id = Some("fixture-id".to_owned());
            match field {
                "name" => name = huge.clone(),
                "id" => id = Some(huge.clone()),
                "error" => result.error_reason = Some(huge.clone()),
                "data" => result.output_data = Some(serde_json::json!({"scope": huge})),
                "receipt" => result.receipt = Some(huge.clone()),
                _ => unreachable!(),
            }
            let error = collect_fixture(vec![Some((name, id, result))], 32768)
                .err()
                .unwrap();
            let retained = error.downcast_ref::<ResultBudgetExceeded>().unwrap();
            let (name, id, outcome) = retained.results[0].as_ref().unwrap();
            assert!(outcome.success);
            assert_eq!(outcome.output, "confirmed fixture");
            match field {
                "name" => assert_eq!(name, &huge),
                "id" => assert_eq!(id.as_deref(), Some(huge.as_str())),
                "error" => assert_eq!(outcome.error_reason.as_deref(), Some(huge.as_str())),
                "data" => assert_eq!(outcome.output_data.as_ref().unwrap()["scope"], huge),
                "receipt" => assert_eq!(outcome.receipt.as_deref(), Some(huge.as_str())),
                _ => unreachable!(),
            }
            assert!(!format!("{error:?}").contains("private-fixture"));
        }
    }

    #[test]
    fn tiny_configured_budget_is_not_silently_raised() {
        let mut result = outcome("confirmed fixture", true);
        result.receipt = Some("fixture-receipt".into());
        let error = collect_fixture(vec![Some(("fixture".into(), None, result))], 1)
            .err()
            .unwrap();
        let evidence = error.downcast_ref::<ResultBudgetExceeded>().unwrap();
        assert_eq!(
            evidence.results[0].as_ref().unwrap().2.receipt.as_deref(),
            Some("fixture-receipt")
        );
    }

    #[test]
    fn actual_native_and_prompt_history_stay_within_round_budget() {
        let ordered = (0..zeroclaw_tools::output_budget::MAX_BATCH_CALLS)
            .map(|i| {
                let mut result = outcome(&"😀\"\\\n".repeat(8), true);
                result.receipt = Some("fixture-receipt".into());
                Some(("file_read".into(), Some(format!("fixture-{i}")), result))
            })
            .collect();
        let collected = collect_fixture(ordered, 32768).unwrap();
        for native in [false, true] {
            let mut history = Vec::new();
            super::super::history_append::append_tool_round_to_history(
                &mut history,
                String::new(),
                &[],
                &collected.individual_results,
                &collected.tool_results,
                native,
            );
            let encoded = serde_json::to_vec(&history[1..]).unwrap();
            assert!(encoded.len() <= ROUND_PAYLOAD_BYTES, "{}", encoded.len());
            assert_eq!(collected.individual_results.len(), 128);
            assert!(history.last().unwrap().content.contains("fixture-receipt"));
        }
    }

    #[test]
    fn nested_history_json_escaping_is_included_in_budget() {
        let result = vec![(Some("fixture-id".into()), "\u{0001}".repeat(100))];
        let raw = serde_json::to_vec(&result).unwrap().len();
        assert!(!history_results_fit(&result, "", raw));
        assert!(history_results_fit(&result, "", raw * 2));
    }

    #[test]
    fn rejected_projection_does_not_mutate_history_or_receipt_collector() {
        let mut result = outcome(&"\\".repeat(120), true);
        result.receipt = Some("fixture-receipt".into());
        let ordered = vec![Some(("fixture".into(), Some("fixture-id".into()), result))];
        assert!(
            encoded_size(&ordered, 512).is_some(),
            "source fits; history escaping must reject"
        );
        let mut history = vec![ChatMessage::user("unchanged fixture")];
        let receipts = Mutex::new(vec!["existing receipt".to_owned()]);
        let error = collect_tool_results(
            ordered,
            &[],
            &mut history,
            &mut LoopDetector::new(LoopDetectorConfig::default()),
            &mut RecoveryTracker::default(),
            &HashSet::new(),
            512,
            Some(&receipts),
            "fixture-model",
            0,
            "fixture-turn",
        )
        .err()
        .unwrap();
        assert!(error.is::<ResultBudgetExceeded>());
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "unchanged fixture");
        assert_eq!(*receipts.lock().unwrap(), vec!["existing receipt"]);
    }

    #[test]
    fn aggregate_metadata_is_counted_even_when_each_result_fits() {
        let ordered = (0..128)
            .map(|i| {
                let mut result = outcome("ok", true);
                result.output_data = Some(serde_json::json!({"metadata": "x".repeat(512)}));
                Some(("file_read".into(), Some(format!("fixture-{i}")), result))
            })
            .collect();
        let error = collect_fixture(ordered, 32768).err().unwrap();
        assert_eq!(
            error
                .downcast_ref::<ResultBudgetExceeded>()
                .unwrap()
                .results
                .len(),
            128
        );
    }

    const RATE_LIMIT_ERR: &str = "Rate limit exceeded: too many actions in the last hour";

    fn outcome(output: &str, success: bool) -> ToolExecutionOutcome {
        ToolExecutionOutcome {
            output: output.to_string(),
            success,
            error_reason: if success {
                None
            } else {
                Some(output.to_string())
            },
            failure_kind: (!success)
                .then_some(crate::agent::tool_execution::ToolFailureKind::Ordinary),
            duration: Duration::from_millis(1),
            receipt: None,
            output_data: None,
        }
    }

    /// Run one results-collection pass over `n` `file_read` calls that each use
    /// different args but return an identical `output` string, with the given
    /// `success` flag.
    fn run(n: usize, output: &str, success: bool) -> Result<CollectedResults> {
        let mut detector = LoopDetector::new(LoopDetectorConfig::default());
        let mut recovery_tracker = RecoveryTracker::default();
        let ignore: HashSet<&str> = HashSet::new();
        let mut history: Vec<ChatMessage> = Vec::new();
        let mut tool_calls: Vec<ParsedToolCall> = Vec::new();
        let mut ordered: Vec<Option<(String, Option<String>, ToolExecutionOutcome)>> = Vec::new();
        for i in 0..n {
            tool_calls.push(ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({ "path": format!("file_{i}.rs") }),
                tool_call_id: None,
            });
            ordered.push(Some((
                "file_read".to_string(),
                None,
                outcome(output, success),
            )));
        }
        collect_tool_results(
            ordered,
            &tool_calls,
            &mut history,
            &mut detector,
            &mut recovery_tracker,
            &ignore,
            10_000,
            None,
            "test-model",
            0,
            "turn-test",
        )
    }

    #[test]
    fn failed_tool_results_do_not_trip_no_progress_breaker() {
        // Many failed reads (different paths, identical rate-limit error) must
        // NOT abort the turn: a recoverable rate-limit/budget error is not a
        // "no progress" exploration loop. Regression for the circuit breaker
        // firing on `file_read` "called N times ... identical results".
        let collected = run(8, RATE_LIMIT_ERR, false).expect("collect failed batch");
        assert!(collected.recovery_trigger.is_none());
        assert_eq!(collected.individual_results.len(), 8);
        assert!(
            collected
                .individual_results
                .iter()
                .all(|(_, result)| result.contains(RATE_LIMIT_ERR))
        );
    }

    #[test]
    fn successful_identical_results_still_trip_no_progress_breaker() {
        // Identical *successful* output across different args is the genuine
        // stuck-loop signal and must request bounded repair recovery.
        let collected = run(8, "byte-identical successful output", true)
            .expect("result collection should return the typed recovery trigger");
        assert!(collected.recovery_trigger.is_some());
    }

    fn run_hash_path(n: usize, output: &str, success: bool) -> Result<()> {
        // `Some(0)` => loop detection active immediately (`elapsed() >= 0s`).
        let pacing = PacingConfig {
            loop_detection_min_elapsed_secs: Some(0),
            ..PacingConfig::default()
        };
        let loop_started_at = Instant::now();
        let mut consecutive_identical_outputs = 0usize;
        let mut last_tool_output_hash: Option<u64> = None;
        let mut detector = LoopDetector::new(LoopDetectorConfig::default());
        let mut recovery_tracker = RecoveryTracker::default();
        let ignore: HashSet<&str> = HashSet::new();
        for iteration in 0..n {
            let mut history: Vec<ChatMessage> = Vec::new();
            let tool_calls = vec![ParsedToolCall {
                name: "file_read".to_string(),
                arguments: serde_json::json!({ "path": format!("file_{iteration}.rs") }),
                tool_call_id: None,
            }];
            let ordered = vec![Some((
                "file_read".to_string(),
                None,
                outcome(output, success),
            ))];
            let collected = collect_tool_results(
                ordered,
                &tool_calls,
                &mut history,
                &mut detector,
                &mut recovery_tracker,
                &ignore,
                10_000,
                None,
                "test-model",
                iteration,
                "turn-test",
            )?;
            if check_identical_output_abort(
                &collected.detection_relevant_output,
                loop_started_at,
                &pacing,
                &mut consecutive_identical_outputs,
                &mut last_tool_output_hash,
                "test-model",
                iteration,
                "turn-test",
            )
            .is_some()
            {
                anyhow::bail!("typed identical-output recovery trigger")
            }
        }
        Ok(())
    }

    #[test]
    fn failed_identical_outputs_do_not_trip_hash_based_abort() {
        assert!(run_hash_path(8, RATE_LIMIT_ERR, false).is_ok());
    }
}
