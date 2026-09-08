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
    FailedOutput,
    OversizedData,
    Delivery,
    Deadline,
    Cancel,
}
struct EvidenceTool {
    name: &'static str,
    mode: Mode,
    calls: Arc<AtomicUsize>,
}
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
        match self.mode {
            Mode::Success => Ok(ToolResult {
                success: true,
                output: "fixture-success-evidence".into(),
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
            Mode::OversizedData => Ok(ToolResult {
                success: true,
                output: ToolOutput::json_with_text(
                    serde_json::json!({"scope": "fixture-owner", "metadata": "x".repeat(100_000)}),
                    "fixture-oversized-data",
                ),
                error: None,
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
            Mode::Cancel => Err(crate::agent::loop_::ToolLoopCancelled.into()),
        }
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
}

async fn run_case(
    modes: &[Mode],
    parallel: bool,
    hooks: Option<&crate::hooks::HookRunner>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
) -> Case {
    // Canonical read allowlist admits parallel execution; the fixtures never do I/O.
    let names = ["file_read", "web_fetch", "sessions_list"];
    let counters: Vec<_> = modes
        .iter()
        .map(|_| Arc::new(AtomicUsize::new(0)))
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
            max_tool_result_chars: 30_000,
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
async fn output_budget_failure_keeps_original_typed_delivery_failure() {
    let case = run_case(&[Mode::OversizedData, Mode::Delivery], false, None, None).await;
    let error = case.result.as_ref().unwrap_err();
    assert!(error.is::<DeliveryFailure>());
    assert!(error.is::<crate::agent::turn::results_collect::ResultBudgetExceeded>());
    assert_eq!(case.remaining_responses, 1);
}
