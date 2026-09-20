mod owned_settlement_tests {
    use super::*;
    use crate::security::estop::{EstopLevel, EstopManager};
    use crate::security::estop_runtime::{self, EstopRuntime};
    use crate::sop::executor::QueuedSopAction;
    use crate::sop::step_contract::StepFailure;
    use crate::sop::store::{SopRunStore, SqliteRunStore};
    use crate::sop::types::{
        Sop, SopEvent, SopExecutionMode, SopPriority, SopRunAction, SopRunStatus, SopStep,
        SopStepStatus, SopTrigger, SopTriggerSource,
    };
    use std::time::Duration;
    use zeroclaw_api::attribution::Attributable;
    use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
    use zeroclaw_config::schema::{Config, PipelineConfig, SopConfig};
    use zeroclaw_tools::pipeline::{PipelineTerminalError, PipelineTool};

    struct Fixture {
        directory: tempfile::TempDir,
        config: Config,
        engine: Arc<std::sync::Mutex<crate::sop::SopEngine>>,
        store: Arc<SqliteRunStore>,
        actions: Vec<QueuedSopAction>,
        deterministic_calls: Arc<AtomicUsize>,
    }

    struct DeterministicProbe(Arc<AtomicUsize>);
    impl crate::sop::capability::SopCapability for DeterministicProbe {
        fn id(&self) -> &'static str {
            "fixture.count"
        }
        fn describe(&self) -> crate::sop::capability::CapabilityInfo {
            crate::sop::capability::CapabilityInfo {
                id: self.id(),
                description: "count actual dispatch",
                deterministic: true,
                idempotent: true,
                reversible: true,
                supports_retry: true,
                required_permissions: Vec::new(),
                input_schema: None,
                output_schema: None,
            }
        }
        fn execute(
            &self,
            _: crate::sop::capability::CapabilityContext,
            _: serde_json::Value,
        ) -> Result<crate::sop::capability::CapabilityResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(crate::sop::capability::CapabilityResult::success(
                serde_json::Value::Null,
            ))
        }
    }

    fn fixture(failure: StepFailure) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            config_path: directory.path().join("config.toml"),
            data_dir: directory.path().join("data"),
            ..Config::default()
        };
        config.security.estop.enabled = true;
        config.security.estop.require_otp_to_resume = false;
        config.security.estop.state_file = "sop-owned-estop.json".into();
        assert_eq!(config.install_root_dir(), directory.path());
        let store = Arc::new(SqliteRunStore::open(&directory.path().join("runs.db")).unwrap());
        let sop = Sop {
            name: "owned-fixture".into(),
            description: "synthetic owned settlement".into(),
            version: "1".into(),
            priority: SopPriority::Normal,
            execution_mode: SopExecutionMode::Auto,
            triggers: vec![SopTrigger::Manual],
            steps: vec![
                SopStep {
                    number: 1,
                    title: "first".into(),
                    body: "run first".into(),
                    agent: Some("stepper".into()),
                    on_failure: failure,
                    ..SopStep::default()
                },
                SopStep {
                    number: 2,
                    title: "tail".into(),
                    body: "must not run after stop".into(),
                    agent: Some("stepper".into()),
                    ..SopStep::default()
                },
            ],
            cooldown_secs: 0,
            max_concurrent: 4,
            location: None,
            deterministic: false,
            admission_policy: Default::default(),
            max_pending_approvals: 0,
            agent: None,
        };
        let deterministic = Sop {
            name: "deterministic-fixture".into(),
            execution_mode: SopExecutionMode::Deterministic,
            deterministic: true,
            steps: vec![SopStep {
                number: 1,
                title: "count".into(),
                kind: crate::sop::SopStepKind::Capability,
                capability: Some("fixture.count".into()),
                ..SopStep::default()
            }],
            ..sop.clone()
        };
        let deterministic_calls = Arc::new(AtomicUsize::new(0));
        let mut capabilities = crate::sop::capability::SopCapabilityRegistry::with_builtins();
        capabilities.register(DeterministicProbe(deterministic_calls.clone()));
        let mut engine = crate::sop::SopEngine::new(SopConfig {
            max_concurrent_total: 4,
            ..SopConfig::default()
        })
        .with_store(store.clone())
        .with_capabilities(Arc::new(capabilities));
        engine.set_sops_for_test(vec![sop, deterministic]);
        let mut started = Vec::new();
        for _ in 0..2 {
            started.push(
                engine
                    .start_run(
                        "owned-fixture",
                        SopEvent {
                            source: SopTriggerSource::Manual,
                            topic: None,
                            payload: None,
                            timestamp: "2026-09-16T00:00:00Z".into(),
                        },
                    )
                    .unwrap(),
            );
        }
        let engine = Arc::new(std::sync::Mutex::new(engine));
        let actions = started
            .into_iter()
            .map(|action| QueuedSopAction {
                engine: engine.clone(),
                audit: None,
                action,
            })
            .collect();
        Fixture {
            directory,
            config,
            engine,
            store,
            actions,
            deterministic_calls,
        }
    }

    async fn drive(
        actions: Vec<QueuedSopAction>,
        config: &Config,
        token: CancellationToken,
        cache: &mut std::collections::HashMap<String, OwnedAgentExecution>,
        history: &mut Vec<ChatMessage>,
    ) -> Result<()> {
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(Vec::new());
        drive_live_sop_actions(
            actions,
            history,
            &TextProvider,
            "mock",
            "mock-model",
            None,
            &tools,
            &crate::observability::NoopObserver {},
            true,
            None,
            &zeroclaw_config::schema::MultimodalConfig::default(),
            None,
            3,
            None,
            &[],
            &[],
            None,
            None,
            &zeroclaw_config::schema::PacingConfig::default(),
            false,
            false,
            30_000,
            100_000,
            None,
            &LoopKnobs::default(),
            "cli",
            None,
            Some(token),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("outer"),
            None,
            Some(SopStepReassembly { config }),
            cache,
        )
        .await
    }

    #[derive(Default)]
    struct Probes {
        provider: AtomicUsize,
        fast: AtomicUsize,
        slow: AtomicUsize,
        dropped: AtomicUsize,
        ready: tokio::sync::Notify,
    }
    struct DropProbe(Arc<Probes>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct ChildTool {
        probes: Arc<Probes>,
        slow: bool,
    }
    zeroclaw_api::mock_tool_attribution!(ChildTool);
    #[async_trait::async_trait]
    impl Tool for ChildTool {
        fn name(&self) -> &str {
            if self.slow { "slow" } else { "fast" }
        }
        fn description(&self) -> &str {
            "synthetic original result"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _: serde_json::Value) -> Result<ToolResult> {
            if self.slow {
                let _guard = DropProbe(self.probes.clone());
                self.probes.slow.fetch_add(1, Ordering::SeqCst);
                self.probes.ready.notify_one();
                std::future::pending::<()>().await;
            }
            self.probes.fast.fetch_add(1, Ordering::SeqCst);
            self.probes.ready.notify_one();
            Ok(ToolResult::ok(ToolOutput::json_with_text(
                serde_json::json!({"retained":"original"}),
                "completed original child",
            )))
        }
    }

    struct PipelineProvider(Arc<Probes>);
    impl zeroclaw_api::attribution::Attributable for PipelineProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            TextProvider.role()
        }
        fn alias(&self) -> &str {
            "pipeline-fixture"
        }
    }
    #[async_trait::async_trait]
    impl ModelProvider for PipelineProvider {
        fn supports_native_tools(&self) -> bool {
            true
        }
        async fn chat_with_system(
            &self,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<f64>,
        ) -> Result<String> {
            Ok("synthetic".into())
        }
        async fn chat(
            &self,
            _: zeroclaw_api::model_provider::ChatRequest<'_>,
            _: &str,
            _: Option<f64>,
        ) -> Result<ChatResponse> {
            self.0.provider.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse { text: None, tool_calls: vec![ToolCall { id: "pipeline-call".into(), name: PipelineTool::NAME.into(), arguments: serde_json::json!({"parallel":true,"steps":[{"tool":"fast","args":{}},{"tool":"slow","args":{}}]}).to_string(), extra_content: None }], usage: None, reasoning_content: None })
        }
    }

    fn seeded_pipeline(probes: Arc<Probes>) -> OwnedAgentExecution {
        let pipeline = PipelineTool::new(
            PipelineConfig {
                enabled: true,
                allowed_tools: vec!["fast".into(), "slow".into()],
                ..PipelineConfig::default()
            },
            vec![
                Arc::new(ChildTool {
                    probes: probes.clone(),
                    slow: false,
                }),
                Arc::new(ChildTool {
                    probes: probes.clone(),
                    slow: true,
                }),
            ],
        )
        .with_execution_context_resolver(Arc::new(
            crate::security::estop_pipeline::current_context,
        ));
        let mut owned = seeded_owned(
            Arc::new(std::sync::Mutex::new(Vec::new())),
            vec![Box::new(pipeline)],
            Default::default(),
            Vec::new(),
            None,
        );
        owned.model_provider = Box::new(PipelineProvider(probes));
        // This synthetic boundary fixture deliberately authorizes only its real
        // pipeline tool. The default supervised noninteractive manager would
        // deny the fixture before any child could demonstrate settlement.
        let risk = zeroclaw_config::schema::RiskProfileConfig {
            auto_approve: vec![PipelineTool::NAME.into()],
            ..Default::default()
        };
        owned.approval = crate::approval::ApprovalManager::for_non_interactive(&risk);
        owned.risk_profile = risk;
        owned
    }

    struct QueueThenStop {
        actions: Vec<QueuedSopAction>,
        calls: Arc<AtomicUsize>,
    }
    zeroclaw_api::mock_tool_attribution!(QueueThenStop);
    #[async_trait::async_trait]
    impl Tool for QueueThenStop {
        fn name(&self) -> &str {
            PipelineTool::NAME
        }
        fn description(&self) -> &str {
            "synthetic queue owner failure"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(&self, _: serde_json::Value) -> Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            for queued in &self.actions {
                crate::sop::executor::enqueue_live_action(
                    queued.engine.clone(),
                    queued.audit.clone(),
                    &queued.action,
                );
            }
            Err(ToolLoopCancelled.into())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn estop_sop_outer_turn_error_accounts_for_undriven_live_queue() {
        // The actual dispatcher queues both canonical actions, then returns a
        // terminal error before the normal batch drain/driver can run. This
        // exercises the outer turn owner's retained queue, without estop config.
        let fixture = fixture(StepFailure::Retry { max: 3 });
        let probes = Arc::new(Probes::default());
        let provider = PipelineProvider(probes.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = crate::tools::scoped::ScopedToolRegistry::from_raw_for_test(vec![Box::new(
            QueueThenStop {
                actions: fixture.actions,
                calls: calls.clone(),
            },
        )]);
        let mut history = vec![ChatMessage::user("synthetic queue request")];
        let error = run_tool_call_loop(ToolLoop {
            exec: ResolvedAgentExecution::resolve(
                ResolvedModelAccess {
                    model_provider: &provider,
                    provider_name: "mock",
                    model: "mock-model",
                    temperature: None,
                },
                ResolvedIo {
                    tools_registry: &tools,
                    observer: &crate::observability::NoopObserver {},
                    silent: true,
                    approval: None,
                    multimodal_config: &zeroclaw_config::schema::MultimodalConfig::default(),
                    config: None,
                    hooks: None,
                    activated_tools: None,
                    model_switch_callback: None,
                    receipt_generator: None,
                },
                ResolvedRuntimeKnobs {
                    max_tool_iterations: 3,
                    excluded_tools: &[],
                    dedup_exempt_tools: &[],
                    pacing: &zeroclaw_config::schema::PacingConfig::default(),
                    strict_tool_parsing: false,
                    parallel_tools: false,
                    max_tool_result_chars: 30_000,
                    context_token_budget: 100_000,
                    knobs: &LoopKnobs::default(),
                },
            ),
            history: &mut history,
            channel_name: "cli",
            channel_reply_target: None,
            cancellation_token: Some(CancellationToken::new()),
            on_delta: None,
            shared_budget: None,
            channel: None,
            collected_receipts: None,
            event_tx: None,
            steering: None,
            new_messages_out: None,
            image_cache: None,
            ingress: IngressContext::sub_turn(),
            memory: None,
            agent_alias: Some("outer"),
            parent_agent_alias: None,
            turn_id: "queue-owner-fixture",
            sop_reassembly: None,
        })
        .await
        .unwrap_err();
        assert!(is_tool_loop_cancelled(&error));
        let owned = error
            .downcast_ref::<sop_settlement::SopDriveInterrupted>()
            .unwrap();
        assert!(owned.steps.is_empty());
        assert_eq!(owned.queued.len(), 2);
        assert!(
            owned
                .queued
                .iter()
                .all(|queued| queued.cancellation.is_ok())
        );
        assert_eq!(probes.provider.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.deterministic_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.store.claim_counts("owned-fixture").unwrap(), (0, 0));
        assert!(fixture.engine.lock().unwrap().active_runs().is_empty());
    }

    struct StalledAudit(Arc<Probes>);
    impl Attributable for StalledAudit {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Memory(zeroclaw_api::attribution::MemoryKind::InMemory)
        }
        fn alias(&self) -> &str {
            "stalled-audit-fixture"
        }
    }
    #[async_trait::async_trait]
    impl zeroclaw_memory::traits::Memory for StalledAudit {
        fn name(&self) -> &str {
            "stalled-audit-fixture"
        }
        async fn store(
            &self,
            _: &str,
            _: &str,
            _: zeroclaw_memory::traits::MemoryCategory,
            _: Option<&str>,
        ) -> Result<()> {
            let _guard = DropProbe(self.0.clone());
            self.0.ready.notify_one();
            std::future::pending().await
        }
        async fn recall(
            &self,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Vec<zeroclaw_memory::traits::MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn get(&self, _: &str) -> Result<Option<zeroclaw_memory::traits::MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _: Option<&zeroclaw_memory::traits::MemoryCategory>,
            _: Option<&str>,
        ) -> Result<Vec<zeroclaw_memory::traits::MemoryEntry>> {
            Ok(Vec::new())
        }
        async fn forget(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn forget_for_agent(&self, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
        async fn store_with_agent(
            &self,
            key: &str,
            content: &str,
            category: zeroclaw_memory::traits::MemoryCategory,
            session_id: Option<&str>,
            _: Option<&str>,
            _: Option<f64>,
            _: Option<&str>,
        ) -> Result<()> {
            // This fixture never commits data: every store has the same owned
            // pending/drop behavior regardless of attribution metadata.
            self.store(key, content, category, session_id).await
        }
        async fn recall_for_agents(
            &self,
            _: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> Result<Vec<zeroclaw_memory::traits::MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn estop_sop_audit_interruption_keeps_committed_step_and_reports_unknown_write() {
        let mut fixture = fixture(StepFailure::Goto { step: 1 });
        let run_id = sop_settlement::action_run_id(&fixture.actions[0].action).to_owned();
        let probes = Arc::new(Probes::default());
        fixture.actions[0].audit = Some(Arc::new(crate::sop::SopAuditLogger::new(Arc::new(
            StalledAudit(probes.clone()),
        ))));
        let token = CancellationToken::new();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut cache = std::collections::HashMap::from([(
            "stepper".into(),
            seeded_owned(
                requests.clone(),
                Vec::new(),
                Default::default(),
                Vec::new(),
                None,
            ),
        )]);
        let mut history = Vec::new();
        let execution = drive(
            fixture.actions,
            &fixture.config,
            token.clone(),
            &mut cache,
            &mut history,
        );
        tokio::pin!(execution);
        tokio::select! { () = probes.ready.notified() => {}, result = &mut execution => panic!("audit returned early: {result:?}") }
        {
            let engine = fixture.engine.lock().unwrap();
            let run = engine.get_run(&run_id).unwrap();
            assert_eq!(run.step_results.len(), 1);
            assert_eq!(run.step_results[0].status, SopStepStatus::Completed);
        }
        token.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), &mut execution)
            .await
            .unwrap()
            .unwrap_err();
        assert!(is_tool_loop_cancelled(&error));
        let owned = error
            .downcast_ref::<sop_settlement::SopDriveInterrupted>()
            .unwrap();
        assert_eq!(owned.steps[0].result.status, SopStepStatus::Completed);
        assert!(matches!(
            owned.steps[0].audit,
            sop_settlement::AuditSettlement::Incomplete
        ));
        assert_eq!(owned.queued.len(), 2);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(probes.dropped.load(Ordering::SeqCst), 1);
        let engine = fixture.engine.lock().unwrap();
        let run = engine.get_run(&run_id).unwrap();
        assert_eq!(run.status, SopRunStatus::Cancelled);
        assert_eq!(run.step_results.len(), 1);
        assert_eq!(run.step_results[0].status, SopStepStatus::Completed);
        assert_eq!(fixture.store.claim_counts("owned-fixture").unwrap(), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn estop_sop_cancellation_persistence_fault_keeps_original_evidence_and_claim() {
        let fixture = fixture(StepFailure::Retry { max: 3 });
        let run_id = sop_settlement::action_run_id(&fixture.actions[0].action).to_owned();
        // A real canonical SQLite transaction rejects the request event. No
        // production state path or independent recovery journal is involved.
        let connection =
            rusqlite::Connection::open(fixture.directory.path().join("runs.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_cancel BEFORE INSERT ON sop_events WHEN NEW.kind = 'run_cancel_requested' BEGIN SELECT RAISE(ABORT, 'synthetic cancellation write failure'); END;").unwrap();
        let token = CancellationToken::new();
        let probes = Arc::new(Probes::default());
        let mut cache =
            std::collections::HashMap::from([("stepper".into(), seeded_pipeline(probes.clone()))]);
        let mut history = Vec::new();
        let execution = drive(
            fixture.actions,
            &fixture.config,
            token.clone(),
            &mut cache,
            &mut history,
        );
        tokio::pin!(execution);
        let ready = async {
            while probes.fast.load(Ordering::SeqCst) == 0 || probes.slow.load(Ordering::SeqCst) == 0
            {
                probes.ready.notified().await;
            }
        };
        tokio::select! { () = ready => {}, result = &mut execution => panic!("nested step returned early: {result:?}") }
        token.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), &mut execution)
            .await
            .unwrap()
            .unwrap_err();
        let owned = error
            .downcast_ref::<sop_settlement::SopDriveInterrupted>()
            .unwrap();
        assert!(is_tool_loop_cancelled(&error));
        assert!(
            crate::sop::engine::err_is_cancellation_persistence_retained(
                owned.steps[0].persistence_error.as_ref().unwrap()
            )
        );
        let pipeline = error
            .chain()
            .find_map(|source| source.downcast_ref::<PipelineTerminalError>())
            .unwrap();
        assert_eq!(
            pipeline.completed[0].result.output.data(),
            Some(&serde_json::json!({"retained":"original"}))
        );
        assert_eq!(owned.queued.len(), 1);
        assert!(owned.queued[0].cancellation.is_err());
        let engine = fixture.engine.lock().unwrap();
        let run = engine.get_run(&run_id).unwrap();
        assert_eq!(run.status, SopRunStatus::Running);
        assert!(
            run.step_results.is_empty(),
            "failed cancellation request must never enter failure routing"
        );
        assert_eq!(fixture.store.claim_counts("owned-fixture").unwrap(), (2, 2));
        assert_eq!(probes.provider.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn estop_sop_precancelled_drained_queue_accounts_for_every_claim() {
        let mut fixture = fixture(StepFailure::Retry { max: 3 });
        {
            let mut engine = fixture.engine.lock().unwrap();
            let old = sop_settlement::action_run_id(&fixture.actions[1].action);
            engine.cancel_run_idempotent(old, None, None).unwrap();
            engine.finish_requested_cancellation(old).unwrap();
            fixture.actions[1].action = engine
                .start_run(
                    "deterministic-fixture",
                    SopEvent {
                        source: SopTriggerSource::Manual,
                        topic: None,
                        payload: None,
                        timestamp: "2026-09-16T00:00:00Z".into(),
                    },
                )
                .unwrap();
            assert!(matches!(
                fixture.actions[1].action,
                SopRunAction::DeterministicStep { .. }
            ));
        }
        let token = CancellationToken::new();
        token.cancel();
        let mut cache = std::collections::HashMap::new();
        let mut history = Vec::new();
        let error = drive(
            fixture.actions,
            &fixture.config,
            token,
            &mut cache,
            &mut history,
        )
        .await
        .unwrap_err();
        let owned = error
            .downcast_ref::<sop_settlement::SopDriveInterrupted>()
            .unwrap();
        assert!(is_tool_loop_cancelled(&error));
        assert_eq!(owned.queued.len(), 2);
        assert!(
            owned
                .queued
                .iter()
                .all(|queued| queued.cancellation.is_ok())
        );
        assert!(cache.is_empty());
        assert!(history.is_empty());
        assert_eq!(fixture.store.claim_counts("owned-fixture").unwrap(), (0, 0));
        assert!(fixture.engine.lock().unwrap().active_runs().is_empty());
        assert_eq!(fixture.deterministic_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn estop_sop_assembly_interruption_drops_owned_future_without_caching() {
        let fixture = fixture(StepFailure::Retry { max: 2 });
        let token = CancellationToken::new();
        let probes = Arc::new(Probes::default());
        let probe: sop_settlement::AssemblyProbe = {
            let probes = probes.clone();
            Arc::new(move || {
                let probes = probes.clone();
                Box::pin(async move {
                    let _guard = DropProbe(probes.clone());
                    probes.ready.notify_one();
                    std::future::pending().await
                })
            })
        };
        let mut cache = std::collections::HashMap::new();
        let mut history = Vec::new();
        let error = {
            let execution = sop_settlement::ASSEMBLY_PROBE.scope(
                probe,
                drive(
                    fixture.actions,
                    &fixture.config,
                    token.clone(),
                    &mut cache,
                    &mut history,
                ),
            );
            tokio::pin!(execution);
            tokio::select! { () = probes.ready.notified() => {}, result = &mut execution => panic!("assembly returned early: {result:?}") }
            token.cancel();
            tokio::time::timeout(Duration::from_secs(2), &mut execution)
                .await
                .unwrap()
                .unwrap_err()
        };
        assert!(is_tool_loop_cancelled(&error));
        let owned = error
            .downcast_ref::<sop_settlement::SopDriveInterrupted>()
            .unwrap();
        assert_eq!(owned.steps.len(), 1);
        assert_eq!(owned.queued.len(), 1);
        assert_eq!(probes.dropped.load(Ordering::SeqCst), 1);
        assert!(cache.is_empty());
        assert_eq!(fixture.store.claim_counts("owned-fixture").unwrap(), (0, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn estop_sop_nested_pipeline_retains_original_evidence_and_never_routes_failure() {
        for (stop, failure) in [
            (0, StepFailure::Retry { max: 3 }),
            (1, StepFailure::Goto { step: 1 }),
            (2, StepFailure::Retry { max: 3 }),
        ] {
            let fixture = fixture(failure);
            let run_id = sop_settlement::action_run_id(&fixture.actions[0].action).to_owned();
            let probes = Arc::new(Probes::default());
            let token = CancellationToken::new();
            let mut cache = std::collections::HashMap::from([(
                "stepper".into(),
                seeded_pipeline(probes.clone()),
            )]);
            let mut history = Vec::new();
            let deadline =
                (stop == 2).then(|| tokio::time::Instant::now() + Duration::from_millis(20));
            let runtime = (stop == 0).then(|| EstopRuntime::from_config(&fixture.config));
            let execution = zeroclaw_api::deadline::PARENT.scope(
                deadline,
                estop_runtime::scope(
                    runtime,
                    drive(
                        fixture.actions,
                        &fixture.config,
                        token.clone(),
                        &mut cache,
                        &mut history,
                    ),
                ),
            );
            tokio::pin!(execution);
            let ready = async {
                while probes.fast.load(Ordering::SeqCst) == 0
                    || probes.slow.load(Ordering::SeqCst) == 0
                {
                    probes.ready.notified().await;
                }
            };
            tokio::select! { () = ready => {}, result = &mut execution => panic!("nested step returned before children: {result:?}") }
            if stop == 0 {
                EstopManager::load(&fixture.config.security.estop, fixture.directory.path())
                    .unwrap()
                    .engage(EstopLevel::KillAll)
                    .unwrap();
            }
            if stop == 1 {
                token.cancel();
            }
            let error = tokio::time::timeout(Duration::from_secs(2), &mut execution)
                .await
                .unwrap()
                .unwrap_err();
            assert!(owned_cancellation::is_terminal(&error));
            let pipeline = error
                .chain()
                .find_map(|source| source.downcast_ref::<PipelineTerminalError>())
                .expect("original pipeline error must survive SOP recording");
            assert_eq!(pipeline.completed.len(), 1);
            assert_eq!(
                pipeline.completed[0].result.output.data(),
                Some(&serde_json::json!({"retained":"original"}))
            );
            let owned = error
                .downcast_ref::<sop_settlement::SopDriveInterrupted>()
                .unwrap();
            assert_eq!(owned.steps.len(), 1);
            assert_eq!(owned.queued.len(), 1);
            assert!(owned.steps[0].persistence_error.is_none());
            let engine = fixture.engine.lock().unwrap();
            let run = engine.get_run(&run_id).unwrap();
            assert_eq!(run.status, SopRunStatus::Cancelled);
            assert_eq!(run.step_results.len(), 1);
            assert_eq!(
                run.step_results[0].tool_calls,
                owned.steps[0].result.tool_calls
            );
            assert_eq!(probes.provider.load(Ordering::SeqCst), 1);
            assert_eq!(probes.fast.load(Ordering::SeqCst), 1);
            assert_eq!(probes.slow.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.store.claim_counts("owned-fixture").unwrap(), (0, 0));
        }
    }
}
