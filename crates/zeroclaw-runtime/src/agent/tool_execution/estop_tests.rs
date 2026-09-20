//! Exercise the actual shared dispatcher, including deferred-name resolution.

use super::*;
use crate::observability::NoopObserver;
use crate::security::estop::{EstopLevel, EstopManager, ResumeSelector};
use crate::security::estop_runtime::{self, EstopRuntime};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use zeroclaw_config::schema::Config;

struct Probe {
    name: &'static str,
    entered: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    release: Option<Arc<Notify>>,
}

zeroclaw_api::mock_tool_attribution!(Probe);

struct OnDrop(Arc<AtomicUsize>);
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Local emergency-stop fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        let _guard = OnDrop(Arc::clone(&self.dropped));
        self.entered.fetch_add(1, Ordering::SeqCst);
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(crate::tools::ToolResult {
            success: true,
            output: "fixture completed".into(),
            error: None,
        })
    }
}

fn config(dir: &std::path::Path) -> Config {
    let mut config = Config {
        config_path: dir.join("config.toml"),
        ..Config::default()
    };
    config.security.estop.enabled = true;
    config.security.estop.state_file = "estop-state.json".into();
    config.security.estop.require_otp_to_resume = false;
    config
}

fn meta() -> TurnMeta<'static> {
    TurnMeta {
        agent_alias: None,
        parent_agent_alias: None,
        turn_id: "estop-fixture",
        channel_name: "test",
    }
}

async fn invoke(dispatch: ToolDispatchContext<'_>, name: &str) -> Result<ToolExecutionOutcome> {
    execute_one_tool(
        name,
        serde_json::json!({}),
        Some("fixture-call"),
        dispatch,
        &meta(),
        &NoopObserver,
        None,
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn estop_dispatch_denies_frozen_deferred_suffix_and_allows_explicit_resume() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut manager = EstopManager::load(&config.security.estop, dir.path()).unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let activated = Arc::new(std::sync::Mutex::new(ActivatedToolSet::new()));
    activated.lock().unwrap().activate(
        "fixture__write".into(),
        Arc::new(Probe {
            name: "fixture__write",
            entered: entered.clone(),
            dropped: Arc::new(AtomicUsize::new(0)),
            release: None,
        }),
    );
    let dispatch = ToolDispatchContext {
        tools_registry: &[],
        activated_tools: Some(&activated),
        excluded_tools: &[],
        model_switch_callback: None,
    };
    let runtime = EstopRuntime::from_config(&config);
    manager
        .engage(EstopLevel::ToolFreeze(vec!["fixture__write".into()]))
        .unwrap();
    let error = estop_runtime::scope(Some(runtime.clone()), invoke(dispatch, "write"))
        .await
        .err()
        .expect("emergency stop must refuse execution");
    assert!(estop_runtime::is_estop_interrupted(&error));
    assert!(is_tool_loop_cancelled(&error));
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    manager
        .resume(
            ResumeSelector::Tools(vec!["fixture__write".into()]),
            None,
            None,
        )
        .unwrap();
    assert!(
        estop_runtime::scope(Some(runtime), invoke(dispatch, "write"))
            .await
            .unwrap()
            .success
    );
    assert_eq!(entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_dispatch_observes_live_enable_and_rejects_network_or_domain_latches() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    let mut manager = EstopManager::load(&config.security.estop, dir.path()).unwrap();
    manager.engage(EstopLevel::KillAll).unwrap();
    config.security.estop.enabled = false;
    let live = Arc::new(parking_lot::RwLock::new(config));
    let runtime = EstopRuntime::from_live_config(live.clone());
    let entered = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Probe {
        name: "local_probe",
        entered: entered.clone(),
        dropped: Arc::new(AtomicUsize::new(0)),
        release: None,
    })];
    let dispatch = ToolDispatchContext {
        tools_registry: &tools,
        activated_tools: None,
        excluded_tools: &[],
        model_switch_callback: None,
    };
    assert!(
        estop_runtime::scope(Some(runtime.clone()), invoke(dispatch, "local_probe"))
            .await
            .unwrap()
            .success
    );
    live.write().security.estop.enabled = true;
    assert!(estop_runtime::is_estop_interrupted(
        &estop_runtime::scope(Some(runtime.clone()), invoke(dispatch, "local_probe"))
            .await
            .err()
            .expect("emergency stop must refuse execution")
    ));
    manager.resume(ResumeSelector::KillAll, None, None).unwrap();
    for level in [
        EstopLevel::NetworkKill,
        EstopLevel::DomainBlock(vec!["example.invalid".into()]),
    ] {
        manager.engage(level).unwrap();
        assert!(estop_runtime::is_estop_interrupted(
            &estop_runtime::scope(Some(runtime.clone()), invoke(dispatch, "local_probe"))
                .await
                .err()
                .expect("emergency stop must refuse execution")
        ));
        manager.resume(ResumeSelector::Network, None, None).unwrap();
    }
    assert_eq!(entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_dispatch_cancels_active_parallel_tools_and_never_starts_queued_calls() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let runtime = EstopRuntime::from_config(&config);
    let mut manager = EstopManager::load(&config.security.estop, dir.path()).unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Probe {
        name: "pending",
        entered: entered.clone(),
        dropped: dropped.clone(),
        release: Some(release),
    })];
    let calls: Vec<_> = (0..6)
        .map(|n| ParsedToolCall {
            name: "pending".into(),
            arguments: serde_json::json!({}),
            tool_call_id: Some(format!("call-{n}")),
        })
        .collect();
    let dispatch = ToolDispatchContext {
        tools_registry: &tools,
        activated_tools: None,
        excluded_tools: &[],
        model_switch_callback: None,
    };
    let run = estop_runtime::scope(Some(runtime), async {
        execute_tools_parallel(&calls, dispatch, &meta(), &NoopObserver, None, None, None).await
    });
    let engage = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while entered.load(Ordering::SeqCst) != 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();
    };
    let (slots, ()) =
        tokio::time::timeout(Duration::from_secs(4), async { tokio::join!(run, engage) })
            .await
            .unwrap();
    assert_eq!(entered.load(Ordering::SeqCst), 4);
    assert_eq!(dropped.load(Ordering::SeqCst), 4);
    assert_eq!(slots.len(), 6);
    for slot in slots {
        let ToolExecutionSlot::Failed(error) = slot else {
            panic!("stopped tool must fail terminally")
        };
        assert!(estop_runtime::is_estop_interrupted(&error));
    }
}

#[tokio::test]
async fn estop_dispatch_backpressure_does_not_start_or_lose_completed_work() {
    for block_before_execution in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let mut manager = EstopManager::load(&config.security.estop, dir.path()).unwrap();
        let entered = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Probe {
            name: "local_probe",
            entered: entered.clone(),
            dropped: dropped.clone(),
            release: None,
        })];
        let dispatch = ToolDispatchContext {
            tools_registry: &tools,
            activated_tools: None,
            excluded_tools: &[],
            model_switch_callback: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        if block_before_execution {
            tx.try_send(TurnEvent::ToolCall {
                id: "already-queued".into(),
                name: "fixture".into(),
                args: serde_json::json!({}),
            })
            .unwrap();
        }
        let metadata = meta();
        let invoke = estop_runtime::scope(
            Some(EstopRuntime::from_config(&config)),
            execute_one_tool(
                "local_probe",
                serde_json::json!({}),
                Some("blocked-event-call"),
                dispatch,
                &metadata,
                &NoopObserver,
                None,
                None,
                Some(&tx),
            ),
        );
        let stop = async {
            // This fixture's queue is never drained. Before execution it is full;
            // after execution the ToolCall fills it and ToolResult cannot publish.
            if !block_before_execution {
                while dropped.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            manager.engage(EstopLevel::KillAll).unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(invoke, stop)
        })
        .await
        .expect("presentation cannot strand emergency cancellation");
        if block_before_execution {
            assert!(estop_runtime::is_estop_interrupted(
                &result.err().expect("stopped admission")
            ));
            assert_eq!(entered.load(Ordering::SeqCst), 0);
        } else {
            let outcome = result.expect("completed result survives stopped event publication");
            assert!(outcome.success);
            assert_eq!(outcome.output, "fixture completed");
            assert_eq!(entered.load(Ordering::SeqCst), 1);
        }
    }
}

struct CooperativeDeadlineProbe {
    entered: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    settle: bool,
}

zeroclaw_api::mock_tool_attribution!(CooperativeDeadlineProbe);

#[async_trait::async_trait]
impl Tool for CooperativeDeadlineProbe {
    fn name(&self) -> &str {
        "cooperative_deadline_fixture"
    }
    fn description(&self) -> &str {
        "Synthetic owned deadline settlement"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn supports_cooperative_settlement(&self) -> bool {
        true
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        let _guard = OnDrop(self.dropped.clone());
        self.entered.fetch_add(1, Ordering::SeqCst);
        let invocation = estop_runtime::current_invocation().expect("dispatcher owns invocation");
        let cause = invocation.interrupted().await;
        assert!(cause.is::<zeroclaw_api::deadline::DeadlineExceeded>());
        if !self.settle {
            std::future::pending::<()>().await;
        }
        Ok(crate::tools::ToolResult {
            success: true,
            output: zeroclaw_api::tool::ToolOutput::json_with_text(
                serde_json::json!({"completed_before_settlement": true}),
                "completed deadline fixture evidence",
            ),
            error: None,
        })
    }
}

async fn invoke_deadline_probe(settle: bool) -> anyhow::Error {
    // No Config, state file, service, filesystem, or remote process is involved.
    let entered = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(CooperativeDeadlineProbe {
        entered: entered.clone(),
        dropped: dropped.clone(),
        settle,
    })];
    let dispatch = ToolDispatchContext {
        tools_registry: &tools,
        activated_tools: None,
        excluded_tools: &[],
        model_switch_callback: None,
    };
    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        zeroclaw_api::deadline::PARENT.scope(
            Some(started + Duration::from_millis(10)),
            invoke(dispatch, "cooperative_deadline_fixture"),
        ),
    )
    .await
    .expect("dispatcher must bound cooperative settlement")
    .err()
    .expect("deadline settlement must remain a terminal error, not an ordinary result");
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let deadline = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<zeroclaw_api::deadline::DeadlineExceeded>())
        .expect("typed deadline survives source wrapper at actual dispatch");
    assert_eq!(deadline.phase, zeroclaw_api::deadline::Phase::Tool);
    assert!(deadline.started);
    assert!(
        !error.is::<zeroclaw_api::deadline::DeadlineExceeded>(),
        "this fixture must exercise Error::source classification, not direct downcasting"
    );
    if !settle {
        assert!(started.elapsed() >= Duration::from_secs(2));
    }
    error
}

#[tokio::test(start_paused = true)]
async fn estop_dispatch_cooperative_success_after_deadline_keeps_terminal_cause_and_evidence() {
    let error = invoke_deadline_probe(true).await;
    let completed = &error
        .downcast_ref::<estop_runtime::InterruptedToolResult>()
        .expect("cooperative completed result remains owned by the terminal error")
        .completed;
    assert!(completed.success);
    assert_eq!(
        completed.output.as_str(),
        "completed deadline fixture evidence"
    );
    assert_eq!(
        completed.output.data(),
        Some(&serde_json::json!({"completed_before_settlement": true}))
    );
}

#[tokio::test(start_paused = true)]
async fn estop_dispatch_broken_cooperative_deadline_settlement_is_bounded_and_terminal() {
    let error = invoke_deadline_probe(false).await;
    assert_eq!(
        error.to_string(),
        crate::i18n::get_required_cli_string("estop-tool-settlement-incomplete")
    );
    assert!(
        error
            .downcast_ref::<estop_runtime::InterruptedToolResult>()
            .is_none()
    );
}

struct OwnedEvidenceProbe {
    error: std::sync::Mutex<Option<anyhow::Error>>,
    entered: Arc<AtomicUsize>,
}
zeroclaw_api::mock_tool_attribution!(OwnedEvidenceProbe);

#[async_trait::async_trait]
impl Tool for OwnedEvidenceProbe {
    fn name(&self) -> &str {
        "owned_evidence_fixture"
    }
    fn description(&self) -> &str {
        "Synthetic owned terminal evidence without cancellation"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        Err(self.error.lock().unwrap().take().expect("one dispatch"))
    }
}

#[tokio::test]
async fn estop_dispatch_preserves_owned_terminal_evidence_without_forging_cancellation() {
    use crate::agent::turn::sop_settlement::{SopDriveInterrupted, SopPhaseIncomplete};
    use crate::tools::delegate::settlement::{
        DelegateAgenticError, DelegateChildResult, DelegateTerminalError, DelegateUnsettled,
    };

    // Exercise both entry points with an ordinary assembly failure wrapped in
    // actual SOP/delegate evidence owners. No cancellation or config is involved.
    for parallel in [false, true] {
        let entered = Arc::new(AtomicUsize::new(0));
        let original = DelegateAgenticError {
            history: vec![zeroclaw_providers::ChatMessage::assistant(
                "retained child history",
            )],
            error: DelegateTerminalError {
                completed: vec![DelegateChildResult {
                    index: 0,
                    agent: "synthetic-child".into(),
                    result: crate::tools::ToolResult {
                        success: true,
                        output: zeroclaw_api::tool::ToolOutput::json_with_text(
                            serde_json::json!({"committed":17}),
                            "retained completed output",
                        ),
                        error: None,
                    },
                }],
                failures: vec![],
                unsettled: vec![DelegateUnsettled {
                    index: 1,
                    agent: "synthetic-unknown".into(),
                    reason: "owned fixture remains unknown",
                }],
                unstarted: vec![],
                first_terminal: None,
                cause: Some(
                    SopDriveInterrupted {
                        cause: SopPhaseIncomplete { phase: "assembly" }.into(),
                        steps: vec![],
                        queued: vec![],
                    }
                    .into(),
                ),
            }
            .into(),
        };
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(OwnedEvidenceProbe {
            error: std::sync::Mutex::new(Some(original.into())),
            entered: entered.clone(),
        })];
        let dispatch = ToolDispatchContext {
            tools_registry: &tools,
            activated_tools: None,
            excluded_tools: &[],
            model_switch_callback: None,
        };
        let error = if parallel {
            let calls = vec![ParsedToolCall {
                name: "owned_evidence_fixture".into(),
                arguments: serde_json::json!({}),
                tool_call_id: Some("owned-fixture-call".into()),
            }];
            let mut slots =
                execute_tools_parallel(&calls, dispatch, &meta(), &NoopObserver, None, None, None)
                    .await;
            assert_eq!(slots.len(), 1);
            let ToolExecutionSlot::Failed(error) = slots.remove(0) else {
                panic!("owned error must not become an ordinary tool result")
            };
            error
        } else {
            invoke(dispatch, "owned_evidence_fixture")
                .await
                .err()
                .expect("terminal error")
        };
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        assert!(!is_tool_loop_cancelled(&error));
        assert!(is_terminal_tool_error(&error));
        let agentic = error
            .downcast_ref::<DelegateAgenticError>()
            .expect("original history owner");
        assert_eq!(agentic.history[0].content, "retained child history");
        let delegate = agentic
            .error
            .downcast_ref::<DelegateTerminalError>()
            .expect("original fanout owner");
        assert_eq!(
            delegate.completed[0].result.output.data().unwrap()["committed"],
            17
        );
        assert_eq!(delegate.unsettled[0].index, 1);
        let sop = delegate
            .cause
            .as_ref()
            .unwrap()
            .downcast_ref::<SopDriveInterrupted>()
            .expect("original SOP owner");
        assert_eq!(
            sop.cause
                .downcast_ref::<SopPhaseIncomplete>()
                .unwrap()
                .phase,
            "assembly"
        );
    }
}
