//! Synthetic dispatch -> post-exec -> history proofs. No external services.
use super::*;
use crate::agent::tool_receipts::ReceiptGenerator;
use crate::tools::{ToolOutput, ToolResult};
use std::sync::Mutex;
use zeroclaw_api::deadline::{DeadlineExceeded, Phase};
use zeroclaw_api::delivery::{DeliveryFailure, EffectOutcome};

#[derive(Clone, Copy)]
enum Mode {
    Success,
    UnevenOutput,
    EscapedUnevenOutput,
    FailedOutput,
    OversizedData,
    Artifact,
    OversizedArtifact,
    OversizedFailedOutput,
    OversizedFailedOutputWithoutError,
    ExpandingFailedOutput,
    LocalizedExpandingFailedOutput,
    Delivery,
    Deadline,
    NestedBudget,
    OversizedError,
    EnvelopeError,
    OrdinaryError,
    FormattingError,
    Cancel,
}
struct EvidenceTool {
    name: &'static str,
    mode: Mode,
    calls: Arc<AtomicUsize>,
    error_dropped: Arc<std::sync::atomic::AtomicBool>,
}
struct ErrorDropProof(Arc<std::sync::atomic::AtomicBool>);
impl std::fmt::Display for ErrorDropProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("fixture wrapper")
    }
}
impl std::fmt::Debug for ErrorDropProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl Drop for ErrorDropProof {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[derive(Debug)]
struct OrdinaryErrorEvidence {
    count: usize,
    writes: AtomicUsize,
    scope: &'static str,
    drop_proof: ErrorDropProof,
}
impl std::fmt::Display for OrdinaryErrorEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.count == 0 {
            return Err(std::fmt::Error);
        }
        for _ in 0..self.count {
            self.writes.fetch_add(1, Ordering::SeqCst);
            f.write_str("\0")?;
        }
        f.write_str("fixture-original-error-tail")
    }
}
impl std::error::Error for OrdinaryErrorEvidence {}
zeroclaw_api::tool_attribution!(EvidenceTool, zeroclaw_api::attribution::ToolKind::Plugin);
#[async_trait]
impl Tool for EvidenceTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "synthetic evidence fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = match self.mode {
            Mode::Success => Ok(ToolResult {
                success: true,
                output: "fixture-success-evidence".into(),
                error: None,
            }),
            Mode::UnevenOutput | Mode::EscapedUnevenOutput => Ok(ToolResult {
                success: true,
                output: ToolOutput::json_with_text(
                    serde_json::json!({"scope": "fixture-owner", "operation_id": "fixture-operation"}),
                    uneven_output_text(self.mode),
                ),
                error: None,
            }),
            Mode::FailedOutput => Ok(ToolResult {
                success: false,
                output: ToolOutput::json_with_text(
                    serde_json::json!({
                        "operation_id":"fixture-operation", "delivered":true,
                        "path":"/synthetic/not-a-delivery.txt"
                    }),
                    "fixture-partial-output",
                ),
                error: Some("fixture exit check failed".into()),
            }),
            Mode::Artifact | Mode::OversizedArtifact => Ok(ToolResult {
                success: true,
                output: ToolOutput::json_with_text(
                    serde_json::json!({
                        "delivered": true, "path": "/synthetic/artifact.txt",
                        "uri": "attachment://fixture/artifact", "filename": "artifact.txt",
                        "mimeType": "text/plain", "bytes": 42,
                        "scope": "fixture-owner", "operation_id": "fixture-operation",
                        "title": if matches!(self.mode, Mode::OversizedArtifact) {
                            "\0".repeat(20_000)
                        } else { "fixture artifact".to_string() },
                    }),
                    "fixture-artifact-evidence",
                ),
                error: None,
            }),
            Mode::OversizedData => Ok(ToolResult {
                success: true,
                output: ToolOutput::json_with_text(
                    serde_json::json!({"scope": "fixture-owner", "metadata": "x".repeat(100_000)}),
                    "fixture-oversized-data",
                ),
                error: None,
            }),
            Mode::OversizedFailedOutput | Mode::OversizedFailedOutputWithoutError => {
                Ok(ToolResult {
                    success: false,
                    output: ToolOutput::json_with_text(
                        serde_json::json!({"scope": "fixture-owner", "operation_id": "fixture-operation"}),
                        oversized_failure_text(),
                    ),
                    error: matches!(self.mode, Mode::OversizedFailedOutput)
                        .then(|| "fixture exit check failed".into()),
                })
            }
            Mode::ExpandingFailedOutput => Ok(ToolResult {
                success: false,
                output: ToolOutput::json_with_text(
                    serde_json::json!({"scope":"fixture-owner", "delivered":true}),
                    "\0".repeat(6_000),
                ),
                error: None,
            }),
            Mode::LocalizedExpandingFailedOutput => Ok(ToolResult {
                success: false,
                output: ToolOutput::json_with_text(
                    serde_json::json!({"scope":"fixture-owner", "delivered":true}),
                    "x".repeat(30_000),
                ),
                error: Some("\n".repeat(10_000)),
            }),
            Mode::Delivery => Err(anyhow::Error::new(DeliveryFailure {
                outcome: EffectOutcome::PossiblyApplied,
                chunk_index: 1,
                total_chunks: 2,
                confirmed_chunks: 1,
            })
            .context("fixture wrapper")),
            Mode::Deadline => Err(DeadlineExceeded {
                phase: Phase::Tool,
                started: true,
            }
            .into()),
            Mode::NestedBudget => {
                let output = "fixture-inner-evidence".repeat(5000);
                let receipt = ReceiptGenerator::with_key(vec![7; 32]).generate_now(
                    "fixture_inner_tool",
                    &serde_json::json!({}),
                    &output,
                );
                Err(anyhow::Error::new(
                    crate::agent::turn::results_collect::ResultBudgetExceeded {
                        errors: Vec::new(),
                        results: vec![Some((
                            "fixture_inner_tool".into(),
                            Some("fixture-inner-call".into()),
                            crate::agent::tool_execution::ToolExecutionOutcome {
                                output,
                                output_data: Some(
                                    serde_json::json!({"scope": "fixture-inner-owner", "delivered": true}),
                                ),
                                success: true,
                                error_reason: None,
                                failure_kind: None,
                                duration: std::time::Duration::ZERO,
                                receipt: Some(receipt),
                            },
                        ))],
                    },
                ))
            }
            Mode::OversizedError
            | Mode::EnvelopeError
            | Mode::OrdinaryError
            | Mode::FormattingError => {
                return Err(anyhow::Error::new(OrdinaryErrorEvidence {
                    count: match self.mode {
                        Mode::OversizedError => 20_000,
                        Mode::EnvelopeError => 6_000,
                        Mode::FormattingError => 0,
                        _ => 4,
                    },
                    writes: AtomicUsize::new(0),
                    scope: "fixture-error-owner",
                    drop_proof: ErrorDropProof(self.error_dropped.clone()),
                }));
            }
            Mode::Cancel => Err(crate::agent::loop_::ToolLoopCancelled.into()),
        };
        result.map_err(|error| {
            let error = error.context(ErrorDropProof(self.error_dropped.clone()));
            if matches!(self.mode, Mode::NestedBudget) {
                error.context("fixture-inner-private-context".repeat(5000))
            } else {
                error
            }
        })
    }
}

struct Case {
    result: Result<String>,
    history: Vec<ChatMessage>,
    receipts: Vec<String>,
    events: Vec<TurnEvent>,
    calls: Vec<usize>,
    step_calls: Vec<crate::sop::types::StepToolCall>,
    remaining_responses: usize,
    error_drops: Vec<Arc<std::sync::atomic::AtomicBool>>,
}

async fn run_case(
    modes: &[Mode],
    parallel: bool,
    hooks: Option<&crate::hooks::HookRunner>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Case {
    run_case_with_result_limit(modes, parallel, hooks, cancellation, 30_000).await
}

async fn run_case_with_result_limit(
    modes: &[Mode],
    parallel: bool,
    hooks: Option<&crate::hooks::HookRunner>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    max_tool_result_chars: usize,
) -> Case {
    // Canonical read allowlist admits parallel execution; the fixtures never do I/O.
    let names = ["file_read", "web_fetch", "sessions_list"];
    let counters: Vec<_> = modes
        .iter()
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    let error_drops: Vec<_> = modes
        .iter()
        .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(false)))
        .collect();
    let provider = ScriptedProvider::new(vec![
        tool_response(
            modes
                .iter()
                .enumerate()
                .map(|(i, _)| tool_call(&format!("fixture-{i}"), names[i]))
                .collect(),
        ),
        text_response("done"),
    ]);
    let tools_registry = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(
        modes
            .iter()
            .enumerate()
            .map(|(i, mode)| {
                Box::new(EvidenceTool {
                    name: names[i],
                    mode: *mode,
                    calls: counters[i].clone(),
                    error_dropped: error_drops[i].clone(),
                }) as Box<dyn Tool>
            })
            .collect(),
    );
    let generator = ReceiptGenerator::with_key(vec![7; 32]);
    let receipts = Mutex::new(Vec::new());
    let mut history = vec![ChatMessage::user("synthetic evidence test")];
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let turn_id = "fixture-evidence-turn";
    let sink = crate::sop::executor::new_step_call_sink();
    let multimodal = zeroclaw_config::schema::MultimodalConfig::default();
    let pacing = zeroclaw_config::schema::PacingConfig::default();
    let knobs = crate::agent::loop_::LoopKnobs::default();
    let future = crate::agent::loop_::run_tool_call_loop(crate::agent::loop_::ToolLoop {
        parent_agent_alias: None,
        sop_reassembly: None,
        exec: crate::agent::loop_::ResolvedAgentExecution {
            model_access: crate::agent::loop_::ResolvedModelAccess {
                model_provider: &provider,
                provider_name: "mock",
                model: "mock-model",
                temperature: None,
            },
            tools_registry: &tools_registry,
            observer: &observability::NoopObserver {},
            silent: true,
            approval: None,
            multimodal_config: &multimodal,
            config: None,
            max_tool_iterations: 3,
            hooks,
            excluded_tools: &[],
            dedup_exempt_tools: &[],
            activated_tools: None,
            model_switch_callback: None,
            pacing: &pacing,
            strict_tool_parsing: false,
            parallel_tools: parallel,
            max_tool_result_chars,
            context_token_budget: 100_000,
            receipt_generator: Some(&generator),
            knobs: &knobs,
        },
        history: &mut history,
        channel_name: "cli",
        channel_reply_target: None,
        cancellation_token: cancellation,
        on_delta: None,
        shared_budget: None,
        channel: None,
        collected_receipts: Some(&receipts),
        event_tx: Some(event_tx),
        steering: None,
        new_messages_out: None,
        image_cache: None,
        // Phase 1: stamp Internal/Trusted. Per-transport
        // stamping lands in a later phase.
        memory: None,
        ingress: IngressContext::sub_turn(),
        agent_alias: None,
        turn_id,
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::sop::executor::scope_step_call_sink(sink.clone(), future),
    )
    .await
    .expect("terminal retention cannot hang on auxiliary work");
    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    Case {
        result,
        history,
        receipts: receipts.into_inner().unwrap(),
        events,
        calls: counters.iter().map(|c| c.load(Ordering::SeqCst)).collect(),
        step_calls: crate::sop::executor::drain_step_calls(&sink),
        remaining_responses: provider.responses.lock().len(),
        error_drops,
    }
}

fn tool_results(case: &Case) -> Vec<serde_json::Value> {
    case.history
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| serde_json::from_str(&m.content).unwrap())
        .collect()
}

fn assert_success_retained(case: &Case, index: usize) {
    let results = tool_results(case);
    let result = results
        .iter()
        .find(|r| r["tool_call_id"] == format!("fixture-{index}"))
        .unwrap();
    let text = result["content"].as_str().unwrap();
    assert!(text.contains("fixture-success-evidence"));
    let receipt = text
        .split("[receipt: ")
        .nth(1)
        .unwrap()
        .strip_suffix(']')
        .unwrap();
    assert!(ReceiptGenerator::with_key(vec![7; 32]).verify(
        receipt,
        ["file_read", "web_fetch", "sessions_list"][index],
        &serde_json::json!({}),
        "fixture-success-evidence"
    ));
    assert!(case.receipts.iter().any(|r| r.ends_with(receipt)));
}

#[tokio::test]
async fn sequential_typed_failure_keeps_success_and_stops_tail_without_retry() {
    for failure in [Mode::Delivery, Mode::Deadline] {
        let case = run_case(&[Mode::Success, failure, Mode::Success], false, None, None).await;
        assert_eq!(case.calls, vec![1, 1, 0]);
        assert_eq!(case.remaining_responses, 1);
        assert_success_retained(&case, 0);
        let error = case.result.as_ref().unwrap_err();
        match failure {
            Mode::Delivery => assert_eq!(
                error
                    .downcast_ref::<DeliveryFailure>()
                    .unwrap()
                    .confirmed_chunks,
                1
            ),
            Mode::Deadline => assert!(error.downcast_ref::<DeadlineExceeded>().unwrap().started),
            _ => unreachable!(),
        }
        let results = tool_results(&case);
        assert_eq!(results.len(), 3);
        assert!(
            results[1]["content"]
                .as_str()
                .unwrap()
                .contains("reconcile")
        );
        assert!(
            results[2]["content"]
                .as_str()
                .unwrap()
                .contains("not started")
        );
        assert_eq!(case.receipts.len(), 1);
        assert_eq!(case.step_calls.len(), 1);
    }
}

#[tokio::test]
async fn parallel_typed_failure_keeps_both_completed_siblings() {
    let case = run_case(
        &[Mode::Success, Mode::Delivery, Mode::Success],
        true,
        None,
        None,
    )
    .await;
    assert!(case.result.as_ref().unwrap_err().is::<DeliveryFailure>());
    assert_eq!(case.calls, vec![1, 1, 1]);
    assert_eq!(case.remaining_responses, 1);
    assert_success_retained(&case, 0);
    assert_success_retained(&case, 2);
    assert_eq!(case.receipts.len(), 2);
    assert_eq!(case.step_calls.len(), 2);
}

#[tokio::test]
async fn sibling_cancellation_or_deadline_cannot_mask_typed_effect_failure() {
    for first in [Mode::Cancel, Mode::Deadline] {
        let case = run_case(&[first, Mode::Delivery, Mode::Success], true, None, None).await;
        assert!(case.result.as_ref().unwrap_err().is::<DeliveryFailure>());
        assert_eq!(case.calls, vec![1, 1, 1]);
        assert_success_retained(&case, 2);
        assert_eq!(tool_results(&case).len(), 3);
        assert_eq!(
            case.step_calls.len(),
            1,
            "a typed cancellation/deadline is not an ordinary completed failure"
        );
    }
}

#[tokio::test]
async fn tool_returned_cancellation_stops_tail_and_keeps_completed_receipt() {
    let case = run_case(
        &[Mode::Success, Mode::Cancel, Mode::Success],
        false,
        None,
        None,
    )
    .await;
    assert!(crate::agent::loop_::is_tool_loop_cancelled(
        case.result.as_ref().unwrap_err()
    ));
    assert_success_retained(&case, 0);
    assert_eq!(case.calls, vec![1, 1, 0]);
    assert_eq!(case.remaining_responses, 1);
    assert_eq!(case.step_calls.len(), 1);
    let results = tool_results(&case);
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("cancelled")
    );
    assert!(
        results[2]["content"]
            .as_str()
            .unwrap()
            .contains("not started")
    );
}

#[tokio::test]
async fn failed_return_preserves_source_output_and_data_without_confirming_delivery() {
    let case = run_case(&[Mode::FailedOutput], false, None, None).await;
    case.result.as_ref().unwrap();
    assert!(case.receipts.is_empty());
    assert_eq!(case.step_calls.len(), 1);
    let captured = &case.step_calls[0];
    assert!(!captured.success);
    assert_eq!(
        captured.output_data.as_ref().unwrap()["operation_id"],
        "fixture-operation"
    );
    let results = tool_results(&case);
    let text = results[0]["content"].as_str().unwrap();
    assert!(text.contains("fixture-partial-output"), "{text}");
    assert!(text.contains("fixture exit check failed"));
    assert!(text.contains("unsuccessful"));
    assert!(case.events.iter().any(|e| matches!(e, TurnEvent::ToolResult { output, artifact: None, .. } if output.contains("fixture-partial-output"))));
    assert!(!case.events.iter().any(|e| matches!(
        e,
        TurnEvent::ToolResult {
            artifact: Some(_),
            ..
        }
    )));
}

struct StalledHook;
#[async_trait]
impl crate::hooks::HookHandler for StalledHook {
    fn name(&self) -> &str {
        "synthetic stalled hook"
    }
    async fn on_after_tool_call(&self, _: &str, _: &ToolResult, _: std::time::Duration) {
        std::future::pending::<()>().await;
    }
}
#[tokio::test]
async fn terminal_batch_does_not_await_stalled_post_execution_hook() {
    let mut hooks = crate::hooks::HookRunner::new();
    hooks.register(Box::new(StalledHook));
    let case = run_case(&[Mode::Success, Mode::Delivery], false, Some(&hooks), None).await;
    assert!(case.result.as_ref().unwrap_err().is::<DeliveryFailure>());
    assert_success_retained(&case, 0);
}

struct FailingCheckpoint;
#[async_trait]
impl zeroclaw_api::turn::TurnJournal for FailingCheckpoint {
    async fn checkpoint(
        &self,
        status: zeroclaw_api::turn::TaskStatus,
        _: Option<String>,
        _: bool,
    ) -> Result<()> {
        if status == zeroclaw_api::turn::TaskStatus::Running {
            anyhow::bail!("fixture checkpoint unavailable");
        }
        Ok(())
    }
}
#[tokio::test]
async fn post_execution_checkpoint_failure_keeps_completed_history() {
    let case = zeroclaw_api::turn::JOURNAL
        .scope(
            Some(Arc::new(FailingCheckpoint)),
            run_case(&[Mode::Success], false, None, None),
        )
        .await;
    assert!(
        case.result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("checkpoint unavailable")
    );
    assert_success_retained(&case, 0);
    assert_eq!(case.remaining_responses, 1);
}

#[tokio::test]
async fn oversized_tool_metadata_stops_before_next_provider_call_and_retains_evidence() {
    let case = run_case(&[Mode::Success, Mode::OversizedData], false, None, None).await;
    let error = case.result.as_ref().unwrap_err();
    let evidence = error
        .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
        .unwrap();
    assert_eq!(case.calls, vec![1, 1]);
    assert_eq!(case.remaining_responses, 1);
    assert_eq!(evidence.results.len(), 2);
    for slot in &evidence.results {
        let outcome = &slot.as_ref().unwrap().2;
        assert!(outcome.success);
        assert!(outcome.receipt.is_some());
    }
    assert_eq!(
        evidence.results[1]
            .as_ref()
            .unwrap()
            .2
            .output_data
            .as_ref()
            .unwrap()["scope"],
        "fixture-owner"
    );
    assert_eq!(
        case.history.len(),
        1,
        "no partial provider history is appended"
    );
}

#[tokio::test]
async fn output_budget_failure_keeps_original_typed_terminal_failure() {
    for parallel in [false, true] {
        for mode in [Mode::Delivery, Mode::Deadline, Mode::Cancel] {
            let case = run_case(&[Mode::OversizedData, mode], parallel, None, None).await;
            let error = case.result.as_ref().unwrap_err();
            match mode {
                Mode::Delivery => assert!(error.is::<DeliveryFailure>()),
                Mode::Deadline => assert!(error.is::<DeadlineExceeded>()),
                Mode::Cancel => assert!(error.is::<crate::agent::loop_::ToolLoopCancelled>()),
                _ => unreachable!("terminal fixture modes only"),
            }
            let evidence = error
                .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                .unwrap();
            assert!(evidence.results[0].as_ref().unwrap().2.receipt.is_some());
            assert!(case.step_calls.is_empty());
            assert_eq!(case.remaining_responses, 1);
        }
    }
}

struct CountingPostToolHook(Arc<AtomicUsize>);
#[async_trait]
impl crate::hooks::HookHandler for CountingPostToolHook {
    fn name(&self) -> &str {
        "synthetic post-tool counter"
    }
    async fn on_after_tool_call(&self, _: &str, _: &ToolResult, _: std::time::Duration) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn oversized_batch_is_rejected_before_sop_capture_or_post_tool_hooks() {
    for parallel in [false, true] {
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let mut hooks = crate::hooks::HookRunner::new();
        hooks.register(Box::new(CountingPostToolHook(hook_calls.clone())));
        let case = run_case(
            &[Mode::Success, Mode::OversizedData],
            parallel,
            Some(&hooks),
            None,
        )
        .await;
        assert!(
            case.result
                .as_ref()
                .unwrap_err()
                .is::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
        );
        assert!(
            case.step_calls.is_empty(),
            "oversized batch must not be copied into SOP capture"
        );
        assert_eq!(
            hook_calls.load(Ordering::SeqCst),
            0,
            "no sibling reaches post-tool hooks before batch admission"
        );
        assert_eq!(
            case.calls,
            vec![1, 1],
            "already executed effects remain real"
        );
        assert_eq!(case.remaining_responses, 1);
        let evidence = case
            .result
            .as_ref()
            .unwrap_err()
            .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
            .unwrap();
        assert_eq!(
            evidence.results[1]
                .as_ref()
                .unwrap()
                .2
                .output_data
                .as_ref()
                .unwrap()["metadata"]
                .as_str()
                .unwrap()
                .len(),
            100_000
        );
        assert!(
            evidence
                .results
                .iter()
                .flatten()
                .all(|(_, _, outcome)| outcome.success && outcome.receipt.is_some())
        );
    }
}

#[tokio::test]
async fn oversized_batch_does_not_await_a_stalled_post_tool_hook() {
    let mut hooks = crate::hooks::HookRunner::new();
    hooks.register(Box::new(StalledHook));
    let case = run_case(
        &[Mode::Success, Mode::OversizedData],
        false,
        Some(&hooks),
        None,
    )
    .await;
    assert!(
        case.result
            .as_ref()
            .unwrap_err()
            .is::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
    );
    assert!(case.step_calls.is_empty());
}

#[tokio::test]
async fn admitted_batch_still_runs_post_tool_hooks_and_sop_capture() {
    for parallel in [false, true] {
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let mut hooks = crate::hooks::HookRunner::new();
        hooks.register(Box::new(CountingPostToolHook(hook_calls.clone())));
        let case = run_case(
            &[Mode::Success, Mode::Success],
            parallel,
            Some(&hooks),
            None,
        )
        .await;
        case.result.as_ref().unwrap();
        assert_eq!(case.step_calls.len(), 2);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 2);
        assert_eq!(case.receipts.len(), 2);
        assert_success_retained(&case, 0);
        assert_success_retained(&case, 1);
    }
}

fn oversized_failure_text() -> String {
    format!("{}fixture-effect-evidence-at-end", "😀\"\n".repeat(20_000))
}

#[tokio::test]
async fn failure_display_expansion_retains_source_and_stops_without_retry() {
    for parallel in [false, true] {
        for mode in [
            Mode::ExpandingFailedOutput,
            Mode::LocalizedExpandingFailedOutput,
        ] {
            let case = run_case_with_result_limit(
                &[Mode::Success, mode, Mode::Success],
                parallel,
                None,
                None,
                65_536,
            )
            .await;
            let error = case.result.as_ref().unwrap_err();
            let evidence = error
                .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                .unwrap();
            let (_, id, failed) = evidence
                .results
                .iter()
                .flatten()
                .find(|(_, id, _)| id.as_deref() == Some("fixture-1"))
                .unwrap();
            assert_eq!(id.as_deref(), Some("fixture-1"));
            let (text, reason) = if matches!(mode, Mode::ExpandingFailedOutput) {
                ("\0".repeat(6_000), None)
            } else {
                ("x".repeat(30_000), Some("\n".repeat(10_000)))
            };
            assert!(
                failed.output == text,
                "keep original source before display expansion"
            );
            assert!(
                failed.error_reason == reason,
                "keep original optional error before display expansion"
            );
            assert_eq!(
                failed.output_data.as_ref().unwrap()["scope"],
                "fixture-owner"
            );
            assert_eq!(failed.output_data.as_ref().unwrap()["delivered"], true);
            assert!(!failed.success);
            assert!(failed.receipt.is_none());
            assert_eq!(
                case.calls,
                if parallel {
                    vec![1, 1, 1]
                } else {
                    vec![1, 1, 0]
                }
            );
            assert_eq!(case.remaining_responses, 1);
            assert_success_retained(&case, 0);
            if parallel {
                assert_success_retained(&case, 2);
            }
        }
    }
}

#[tokio::test]
async fn failure_display_rejection_keeps_delivery_priority_at_tiny_budgets() {
    for limit in [1, 65_536] {
        let case = run_case_with_result_limit(
            &[Mode::ExpandingFailedOutput, Mode::Delivery, Mode::Success],
            true,
            None,
            None,
            limit,
        )
        .await;
        let error = case.result.as_ref().unwrap_err();
        assert!(error.is::<DeliveryFailure>());
        let failures = error
            .downcast_ref::<crate::agent::turn::batch_failures::RetainedToolFailures>()
            .unwrap();
        assert_eq!(failures.primary_call_index, 1);
        assert_eq!(failures.siblings[0].0, 0);
        let budget = failures.siblings[0]
            .1
            .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
            .unwrap();
        let failed = &budget.results[0].as_ref().unwrap().2;
        assert!(failed.output == "\0".repeat(6_000));
        assert!(failed.error_reason.is_none());
        assert_eq!(failed.output_data.as_ref().unwrap()["delivered"], true);
        assert_eq!(case.calls, vec![1, 1, 1]);
        assert_eq!(case.remaining_responses, 1);
        if limit == 1 {
            let outer = error
                .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                .unwrap();
            assert!(outer.results[2].as_ref().unwrap().2.receipt.is_some());
            assert!(case.step_calls.is_empty());
            assert_eq!(case.history.len(), 1);
        } else {
            assert_success_retained(&case, 2);
        }
    }
}

#[tokio::test]
async fn budget_rejection_preserves_failed_source_before_executor_excerpts() {
    for parallel in [false, true] {
        for mode in [
            Mode::OversizedFailedOutput,
            Mode::OversizedFailedOutputWithoutError,
        ] {
            let case = run_case(&[Mode::Success, mode], parallel, None, None).await;
            let error = case.result.as_ref().unwrap_err();
            let evidence = error
                .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                .unwrap();
            let failed = &evidence.results[1].as_ref().unwrap().2;
            assert!(
                failed.output == oversized_failure_text(),
                "retain the entire original source, including its tail"
            );
            assert!(!failed.success);
            assert!(failed.receipt.is_none());
            assert_eq!(
                failed.output_data.as_ref().unwrap()["scope"],
                "fixture-owner"
            );
            assert_eq!(
                failed.output_data.as_ref().unwrap()["operation_id"],
                "fixture-operation"
            );
            assert_eq!(
                failed.error_reason.as_deref(),
                matches!(mode, Mode::OversizedFailedOutput).then_some("fixture exit check failed")
            );
            assert!(evidence.results[0].as_ref().unwrap().2.receipt.is_some());
            assert_eq!(case.calls, vec![1, 1]);
            assert_eq!(case.remaining_responses, 1);
            assert!(case.step_calls.is_empty());
        }
    }
}

fn uneven_output_text(mode: Mode) -> String {
    let prefix = match mode {
        Mode::UnevenOutput => "😀".repeat(10_000),
        Mode::EscapedUnevenOutput => format!("{}{}", "\n".repeat(16_000), "x".repeat(20_000)),
        _ => unreachable!("uneven output fixture modes only"),
    };
    format!("{prefix}fixture-effect-evidence-at-end")
}

#[tokio::test]
async fn uneven_admitted_batch_keeps_complete_text_and_verifiable_receipts() {
    for parallel in [false, true] {
        let case = run_case_with_result_limit(
            &[Mode::UnevenOutput, Mode::Success],
            parallel,
            None,
            None,
            50_000,
        )
        .await;
        case.result.as_ref().unwrap();
        let source = uneven_output_text(Mode::UnevenOutput);
        let results = tool_results(&case);
        let text = results[0]["content"].as_str().unwrap();
        let (actual, receipt) = text.split_once("\n\n[receipt: ").unwrap();
        assert!(
            actual == source,
            "fitting batches must not lose a large sibling's tail to an equal-share excerpt"
        );
        let receipt = receipt.strip_suffix(']').unwrap();
        assert!(ReceiptGenerator::with_key(vec![7; 32]).verify(
            receipt,
            "file_read",
            &serde_json::json!({}),
            actual,
        ));
        assert!(case.receipts.iter().any(|r| r.ends_with(receipt)));
        // SOP owns a separate bounded display projection; provider history
        // must retain the complete source independently of that display cap.
        assert_eq!(case.step_calls.len(), 2);
        assert!(case.step_calls[0].success);
        assert_eq!(
            case.step_calls[0].output_data.as_ref().unwrap()["scope"],
            "fixture-owner"
        );
        assert_success_retained(&case, 1);
        assert_eq!(case.remaining_responses, 0);
        let messages: Vec<_> = case.history.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(messages.len(), 2);
        assert!(
            serde_json::to_vec(&messages).unwrap().len()
                <= zeroclaw_tools::output_budget::ROUND_PAYLOAD_BYTES
        );
        assert!(
            messages
                .iter()
                .all(|m| serde_json::to_vec(m).unwrap().len() <= 50_000)
        );
    }
}

#[tokio::test]
async fn final_history_rejection_keeps_unexcerpted_source_and_receipts() {
    for parallel in [false, true] {
        let case = run_case_with_result_limit(
            &[Mode::EscapedUnevenOutput, Mode::Success],
            parallel,
            None,
            None,
            zeroclaw_tools::output_budget::ROUND_PAYLOAD_BYTES,
        )
        .await;
        let error = case
            .result
            .as_ref()
            .expect_err("nested history escaping must exceed the budget");
        let evidence = error
            .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
            .unwrap();
        let source = uneven_output_text(Mode::EscapedUnevenOutput);
        let outcome = &evidence.results[0].as_ref().unwrap().2;
        assert!(
            outcome.output == source,
            "history rejection must own the unmodified source"
        );
        assert!(ReceiptGenerator::with_key(vec![7; 32]).verify(
            outcome.receipt.as_ref().unwrap(),
            "file_read",
            &serde_json::json!({}),
            &outcome.output,
        ));
        assert_eq!(
            outcome.output_data.as_ref().unwrap()["operation_id"],
            "fixture-operation"
        );
        assert_eq!(
            case.step_calls.len(),
            2,
            "source admission succeeded before history wrapping rejected the batch"
        );
        assert_eq!(case.calls, vec![1, 1]);
        assert_eq!(case.remaining_responses, 1);
        assert_eq!(case.history.len(), 1);
        assert!(case.receipts.is_empty());
        assert!(
            evidence
                .results
                .iter()
                .flatten()
                .all(|(_, _, result)| result.success && result.receipt.is_some())
        );
    }
}

#[tokio::test]
async fn oversized_artifact_is_not_projected_before_batch_rejection() {
    for parallel in [false, true] {
        let case = run_case(
            &[Mode::OversizedArtifact, Mode::Success],
            parallel,
            None,
            None,
        )
        .await;
        let evidence = case
            .result
            .as_ref()
            .unwrap_err()
            .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
            .unwrap();
        assert!(
            case.events.iter().any(|event| matches!(event,
                TurnEvent::ToolResult { id, artifact: None, .. } if id == "fixture-0"
            )),
            "close the result card without copying the oversized artifact"
        );
        assert!(
            !case.events.iter().any(|event| matches!(
                event,
                TurnEvent::ToolResult {
                    artifact: Some(_),
                    ..
                }
            )),
            "oversized artifact fields must never reach the event channel"
        );
        let outcome = &evidence.results[0].as_ref().unwrap().2;
        assert!(outcome.success);
        let data = outcome.output_data.as_ref().unwrap();
        assert_eq!(data["title"].as_str().unwrap().len(), 20_000);
        assert_eq!(data["scope"], "fixture-owner");
        assert_eq!(data["operation_id"], "fixture-operation");
        assert_eq!(data["delivered"], true);
        assert!(ReceiptGenerator::with_key(vec![7; 32]).verify(
            outcome.receipt.as_ref().unwrap(),
            "file_read",
            &serde_json::json!({}),
            &outcome.output,
        ));
        assert_eq!(case.calls, vec![1, 1]);
        assert_eq!(case.remaining_responses, 1);
        assert!(case.step_calls.is_empty());
    }
}

#[tokio::test]
async fn admitted_artifact_keeps_typed_delivery_metadata() {
    let case = run_case(&[Mode::Artifact], false, None, None).await;
    case.result.as_ref().unwrap();
    let artifact = case
        .events
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolResult {
                artifact: Some(artifact),
                ..
            } => Some(artifact),
            _ => None,
        })
        .expect("a fitting delivered artifact must still be projected");
    assert_eq!(artifact.path, "/synthetic/artifact.txt");
    assert_eq!(artifact.uri, "attachment://fixture/artifact");
    assert_eq!(artifact.filename, "artifact.txt");
    assert_eq!(artifact.title, "fixture artifact");
    assert_eq!(artifact.mime, "text/plain");
    assert_eq!(artifact.size, 42);
    assert_eq!(case.receipts.len(), 1);
}

#[tokio::test]
async fn parallel_failures_keep_original_errors_until_terminal_error_is_dropped() {
    for modes in [
        [Mode::Deadline, Mode::Delivery, Mode::Success],
        [Mode::Delivery, Mode::Deadline, Mode::Success],
        [Mode::Delivery, Mode::Delivery, Mode::Success],
        [Mode::Cancel, Mode::Deadline, Mode::Delivery],
        [Mode::Deadline, Mode::Cancel, Mode::Delivery],
    ] {
        let case = run_case(&modes, true, None, None).await;
        assert!(case.result.as_ref().unwrap_err().is::<DeliveryFailure>());
        assert_eq!(case.calls, vec![1, 1, 1]);
        assert_eq!(case.remaining_responses, 1);
        let retained = case
            .result
            .as_ref()
            .unwrap_err()
            .downcast_ref::<crate::agent::turn::batch_failures::RetainedToolFailures>()
            .unwrap();
        let primary = modes
            .iter()
            .position(|mode| matches!(mode, Mode::Delivery))
            .unwrap();
        assert_eq!(retained.primary_call_index, primary);
        let expected_indices: Vec<_> = modes
            .iter()
            .enumerate()
            .filter(|(index, mode)| *index != primary && !matches!(mode, Mode::Success))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            retained
                .siblings
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>(),
            expected_indices
        );
        for (index, error) in &retained.siblings {
            match modes[*index] {
                Mode::Deadline => {
                    assert!(error.downcast_ref::<DeadlineExceeded>().unwrap().started)
                }
                Mode::Delivery => assert_eq!(
                    error
                        .downcast_ref::<DeliveryFailure>()
                        .unwrap()
                        .confirmed_chunks,
                    1
                ),
                Mode::Cancel => assert!(crate::agent::loop_::is_tool_loop_cancelled(error)),
                _ => unreachable!("only failed fixture calls are retained"),
            }
        }
        let drops = case.error_drops.clone();
        for (index, mode) in modes.iter().enumerate() {
            if !matches!(mode, Mode::Success) {
                assert!(
                    !drops[index].load(Ordering::SeqCst),
                    "error for call {index} dropped before return"
                );
            }
        }
        if matches!(modes[2], Mode::Success) {
            assert_success_retained(&case, 2);
        }
        drop(case);
        for (index, mode) in modes.iter().enumerate() {
            if !matches!(mode, Mode::Success) {
                assert!(
                    drops[index].load(Ordering::SeqCst),
                    "error for call {index} leaked after return"
                );
            }
        }
    }
}

#[tokio::test]
async fn sibling_error_ownership_survives_small_budget_rejection() {
    for limit in [1, 30_000] {
        for modes in [
            [Mode::NestedBudget, Mode::Delivery, Mode::Success],
            [Mode::Delivery, Mode::NestedBudget, Mode::Success],
        ] {
            let case = run_case_with_result_limit(&modes, true, None, None, limit).await;
            let error = case.result.as_ref().unwrap_err();
            assert!(error.is::<DeliveryFailure>());
            let retained = error
                .downcast_ref::<crate::agent::turn::batch_failures::RetainedToolFailures>()
                .unwrap();
            let nested_index = usize::from(matches!(modes[0], Mode::Delivery));
            assert_eq!(retained.primary_call_index, 1 - nested_index);
            assert_eq!(retained.siblings.len(), 1);
            assert_eq!(retained.siblings[0].0, nested_index);
            assert_eq!(
                retained.siblings[0]
                    .1
                    .downcast_ref::<String>()
                    .unwrap()
                    .len(),
                "fixture-inner-private-context".len() * 5000
            );
            let nested = retained.siblings[0]
                .1
                .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                .unwrap();
            let (name, id, outcome) = nested.results[0].as_ref().unwrap();
            assert_eq!(name, "fixture_inner_tool");
            assert_eq!(id.as_deref(), Some("fixture-inner-call"));
            assert_eq!(
                outcome.output_data.as_ref().unwrap()["scope"],
                "fixture-inner-owner"
            );
            assert_eq!(outcome.output_data.as_ref().unwrap()["delivered"], true);
            assert!(outcome.success);
            assert!(ReceiptGenerator::with_key(vec![7; 32]).verify(
                outcome.receipt.as_ref().unwrap(),
                name,
                &serde_json::json!({}),
                &outcome.output,
            ));
            for rendered in [
                error.to_string(),
                format!("{error:#}"),
                format!("{error:?}"),
                format!("{error:#?}"),
            ] {
                assert!(rendered.len() < 4096);
                assert!(!rendered.contains("fixture-inner-evidence"));
                assert!(!rendered.contains("fixture-inner-owner"));
                assert!(!rendered.contains("fixture-inner-private-context"));
            }
            assert_eq!(case.calls, vec![1, 1, 1]);
            assert_eq!(case.remaining_responses, 1);
            assert!(!case.error_drops[0].load(Ordering::SeqCst));
            assert!(!case.error_drops[1].load(Ordering::SeqCst));
            if limit == 1 {
                let outer = error
                    .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                    .unwrap();
                assert!(outer.results[2].as_ref().unwrap().2.receipt.is_some());
                assert_eq!(case.history.len(), 1);
                assert!(case.step_calls.is_empty());
                assert!(case.receipts.is_empty());
            } else {
                assert_success_retained(&case, 2);
            }
        }
    }
}

#[tokio::test]
async fn oversized_ordinary_error_preserves_original_error_ownership() {
    for mode in [
        Mode::OversizedError,
        Mode::EnvelopeError,
        Mode::FormattingError,
    ] {
        let case = run_case(&[mode], false, None, None).await;
        let error = case.result.as_ref().unwrap_err();
        let rejected = error
            .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
            .unwrap();
        assert!(rejected.results.is_empty());
        assert_eq!(rejected.errors.len(), 1);
        let original = rejected.errors[0]
            .downcast_ref::<OrdinaryErrorEvidence>()
            .unwrap();
        assert_eq!(original.scope, "fixture-error-owner");
        assert!(!original.drop_proof.0.load(Ordering::SeqCst));
        let writes = original.writes.load(Ordering::SeqCst);
        match mode {
            Mode::OversizedError => assert!(writes < 11_000, "formatter did not stop early"),
            Mode::EnvelopeError => assert_eq!(writes, 6_000),
            Mode::FormattingError => assert_eq!(writes, 0),
            _ => unreachable!(),
        }
        for rendered in [
            error.to_string(),
            format!("{error:#}"),
            format!("{error:?}"),
            format!("{error:#?}"),
        ] {
            assert!(rendered.len() < 4096);
            assert!(!rendered.contains("fixture-error-owner"));
            assert!(!rendered.contains("fixture-original-error-tail"));
        }
        assert!(!case.error_drops[0].load(Ordering::SeqCst));
        assert_eq!(case.remaining_responses, 1);
        assert!(case.receipts.is_empty());
        assert!(case.events.iter().any(|event| matches!(event,
            TurnEvent::ToolResult { id, artifact: None, output, .. }
                if id == "fixture-0" && output.len() < 4096)));
        let dropped = case.error_drops[0].clone();
        drop(case);
        assert!(dropped.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn normalized_error_rejection_preserves_siblings_and_delivery_through_tiny_budgets() {
    for parallel in [false, true] {
        let case = run_case(
            &[Mode::Success, Mode::OversizedError, Mode::Success],
            parallel,
            None,
            None,
        )
        .await;
        assert_eq!(
            case.calls,
            if parallel {
                vec![1, 1, 1]
            } else {
                vec![1, 1, 0]
            }
        );
        assert_success_retained(&case, 0);
        if parallel {
            assert_success_retained(&case, 2);
        }
        assert_eq!(case.remaining_responses, 1);
        assert!(!case.error_drops[1].load(Ordering::SeqCst));
    }
    for limit in [1, 30_000] {
        let case = run_case_with_result_limit(
            &[Mode::EnvelopeError, Mode::Delivery, Mode::Success],
            true,
            None,
            None,
            limit,
        )
        .await;
        let error = case.result.as_ref().unwrap_err();
        assert!(error.is::<DeliveryFailure>());
        let failures = error
            .downcast_ref::<crate::agent::turn::batch_failures::RetainedToolFailures>()
            .unwrap();
        assert_eq!(failures.primary_call_index, 1);
        assert_eq!(failures.siblings[0].0, 0);
        let budget = failures.siblings[0]
            .1
            .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
            .unwrap();
        assert_eq!(
            budget.errors[0]
                .downcast_ref::<OrdinaryErrorEvidence>()
                .unwrap()
                .scope,
            "fixture-error-owner"
        );
        assert_eq!(case.calls, vec![1, 1, 1]);
        assert_eq!(case.remaining_responses, 1);
        if limit == 1 {
            let outer = error
                .downcast_ref::<crate::agent::turn::results_collect::ResultBudgetExceeded>()
                .unwrap();
            assert!(outer.results[2].as_ref().unwrap().2.receipt.is_some());
            assert!(case.step_calls.is_empty());
            assert_eq!(case.history.len(), 1);
        } else {
            assert_success_retained(&case, 2);
        }
    }
}

#[tokio::test]
async fn fitting_ordinary_error_keeps_full_normal_recovery_text() {
    let case = run_case(&[Mode::OrdinaryError], false, None, None).await;
    assert_eq!(case.result.as_ref().unwrap(), "done");
    assert_eq!(case.calls, vec![1]);
    assert_eq!(case.remaining_responses, 0);
    let results = tool_results(&case);
    let content = results[0]["content"].as_str().unwrap();
    assert!(content.contains("Error executing file_read:"));
    assert!(content.contains("fixture-original-error-tail"));
    assert_eq!(content.matches('\0').count(), 4);
    assert!(case.error_drops[0].load(Ordering::SeqCst));
    assert!(case.receipts.is_empty());
}
