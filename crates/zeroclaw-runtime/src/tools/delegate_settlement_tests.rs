// Real dispatcher -> DelegateTool -> local provider -> nested ToolLoop/pipeline.
// Every state, workspace, and provider endpoint belongs to this fixture.
use crate::security::estop::{EstopLevel, EstopManager, ResumeSelector};
use crate::security::estop_runtime::{self, EstopRuntime, InvocationCancellation};
use std::sync::atomic::{AtomicUsize, Ordering};

struct SettlementProvider {
    uri: String,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for SettlementProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl SettlementProvider {
    async fn start(first: serde_json::Value) -> Self {
        Self::start_after(first, None).await
    }
    async fn start_after(
        first: serde_json::Value,
        release: Option<Arc<tokio::sync::Notify>>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let task = zeroclaw_spawn::spawn!(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_http_request(&mut socket).await;
                let index = count.fetch_add(1, Ordering::SeqCst);
                if index == 0
                    && let Some(release) = &release
                {
                    release.notified().await;
                }
                let response = if index == 0 {
                    first.clone()
                } else {
                    json!({"choices":[{"message":{"content":"fixture final response"}}]})
                };
                write_json_response(&mut socket, response).await;
            }
        });
        Self { uri, calls, task }
    }
}

struct SettlementStep {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    wait: bool,
    stubborn: bool,
    scopes: Arc<parking_lot::Mutex<Vec<InvocationCancellation>>>,
}
impl zeroclaw_api::attribution::Attributable for SettlementStep {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
    }
    fn alias(&self) -> &str {
        self.name
    }
}
#[async_trait]
impl Tool for SettlementStep {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Synthetic owned delegate step"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    fn supports_cooperative_settlement(&self) -> bool {
        self.stubborn
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        assert!(estop_runtime::current().is_some());
        self.scopes
            .lock()
            .push(estop_runtime::current_invocation().expect("spawned invocation"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.wait {
            std::future::pending::<()>().await;
        }
        Ok(ToolResult {
            success: true,
            output: ToolOutput::json_with_text(
                json!({"original_delegate_payload":self.name}),
                "retained child evidence",
            ),
            error: None,
        })
    }
}

struct SettlementFixture {
    _directory: TempDir,
    tool: Arc<DelegateTool>,
    manager: EstopManager,
    live: Arc<RwLock<Config>>,
    calls: Vec<Arc<AtomicUsize>>,
    scopes: Arc<parking_lot::Mutex<Vec<InvocationCancellation>>>,
}
fn settlement_fixture(
    providers: &[(&str, &SettlementProvider)],
    stubborn: bool,
) -> SettlementFixture {
    use zeroclaw_config::autonomy::{DelegationMode, DelegationPolicy};
    let directory = TempDir::new().unwrap();
    let mut config = Config {
        config_path: directory.path().join("config.toml"),
        data_dir: directory.path().join("data"),
        ..Config::default()
    };
    config.security.estop.enabled = true;
    config.security.estop.require_otp_to_resume = false;
    config.security.estop.state_file = directory
        .path()
        .join("fixture-estop.json")
        .to_string_lossy()
        .into_owned();
    config.reliability.provider_retries = 0;
    config.risk_profiles.insert(
        "fixture".into(),
        RiskProfileConfig {
            delegation_policy: DelegationPolicy {
                mode: DelegationMode::Allow,
            },
            allowed_tools: vec![
                "delegate".into(),
                zeroclaw_tools::pipeline::PipelineTool::NAME.into(),
                "first".into(),
                "slow".into(),
                "tail".into(),
            ],
            ..RiskProfileConfig::default()
        },
    );
    config.runtime_profiles.insert(
        "fixture".into(),
        RuntimeProfileConfig {
            agentic: true,
            max_tool_iterations: 5,
            ..RuntimeProfileConfig::default()
        },
    );
    for (name, provider) in providers {
        config.providers.models.custom.insert(
            (*name).into(),
            CustomModelProviderConfig {
                base: ModelProviderConfig {
                    uri: Some(provider.uri.clone()),
                    model: Some("fixture-model".into()),
                    native_tools: Some(true),
                    ..ModelProviderConfig::default()
                },
            },
        );
        config.agents.insert(
            (*name).into(),
            AliasedAgentConfig {
                model_provider: format!("custom.{name}").into(),
                risk_profile: "fixture".into(),
                runtime_profile: "fixture".into(),
                ..AliasedAgentConfig::default()
            },
        );
    }
    config
        .agents
        .insert("caller".into(), config.agents[providers[0].0].clone());
    let manager = EstopManager::load(&config.security.estop, directory.path()).unwrap();
    let live = Arc::new(RwLock::new(config.clone()));
    let scopes = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let calls: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let steps: Vec<Arc<dyn Tool>> = ["first", "slow", "tail"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            Arc::new(SettlementStep {
                name,
                calls: calls[index].clone(),
                wait: index == 1,
                stubborn: index == 1 && stubborn,
                scopes: scopes.clone(),
            }) as Arc<dyn Tool>
        })
        .collect();
    let pipeline = zeroclaw_tools::pipeline::PipelineTool::new(
        zeroclaw_config::schema::PipelineConfig {
            enabled: true,
            allowed_tools: steps.iter().map(|step| step.name().into()).collect(),
            ..Default::default()
        },
        steps,
    )
    .with_execution_context_resolver(Arc::new(crate::security::estop_pipeline::current_context));
    let root = Arc::new(config);
    let security = Arc::new(SecurityPolicy::for_agent(&root, "caller").unwrap());
    let tool = DelegateTool::new(root.agents.clone(), None, security)
        .with_root_config(root.clone())
        .with_live_config(Some(live.clone()))
        .with_caller_alias("caller")
        .with_workspace_dir(directory.path().join("workspace"))
        .with_risk_profiles(root.risk_profiles.clone())
        .with_runtime_profiles(root.runtime_profiles.clone())
        .with_parent_tools(Arc::new(RwLock::new(vec![Arc::new(pipeline)])));
    SettlementFixture {
        _directory: directory,
        tool: Arc::new(tool),
        manager,
        live,
        calls,
        scopes,
    }
}
fn settlement_pipeline_response() -> serde_json::Value {
    chat_completion_tool_call(
        zeroclaw_tools::pipeline::PipelineTool::NAME,
        "fixture-pipeline",
        json!({"steps":[
            {"tool":"first","args":{}},{"tool":"slow","args":{}},{"tool":"tail","args":{}}
        ]}),
    )
}
async fn settlement_dispatch(
    tool: Arc<DelegateTool>,
    args: serde_json::Value,
    token: Option<&CancellationToken>,
) -> anyhow::Result<crate::agent::tool_execution::ToolExecutionOutcome> {
    use crate::agent::tool_execution::{ToolDispatchContext, execute_one_tool};
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::ArcToolRef(tool))];
    execute_one_tool(
        "delegate",
        args,
        Some("delegate-fixture-call"),
        ToolDispatchContext {
            tools_registry: &tools,
            activated_tools: None,
            excluded_tools: &[],
            model_switch_callback: None,
        },
        &crate::agent::turn::TurnMeta {
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "delegate-fixture",
            channel_name: "test",
        },
        &crate::observability::NoopObserver,
        token,
        None,
        None,
    )
    .await
}
async fn settlement_wait(calls: &AtomicUsize, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while calls.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn settlement_pipeline(error: &anyhow::Error) -> &zeroclaw_tools::pipeline::PipelineTerminalError {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref())
        .expect("original nested pipeline terminal error")
}
fn settlement_assert_partial(error: &anyhow::Error, fixture: &SettlementFixture) {
    let pipeline = settlement_pipeline(error);
    assert_eq!(pipeline.completed.len(), 1);
    assert_eq!(
        pipeline.completed[0].result.output.data(),
        Some(&json!({"original_delegate_payload":"first"}))
    );
    assert_eq!(fixture.calls[2].load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .scopes
            .lock()
            .iter()
            .all(|scope| scope.check().is_err())
    );
    let agentic = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<settlement::DelegateAgenticError>())
        .expect("owned child history");
    assert!(agentic.history.iter().any(|message| message.role == "tool"));
    assert!(!format!("{error:?}").contains("original_delegate_payload"));
}

#[tokio::test]
async fn estop_delegate_settlement_actual_dispatch_retains_partial_for_global_alias_and_user() {
    for stop in ["global", "alias", "user"] {
        let provider = SettlementProvider::start(settlement_pipeline_response()).await;
        let mut fixture = settlement_fixture(&[("target", &provider)], false);
        let token = CancellationToken::new();
        let execution = settlement_dispatch(
            fixture.tool.clone(),
            json!({"agent":"target","prompt":"fixture work"}),
            Some(&token),
        );
        let stop_future = async {
            settlement_wait(&fixture.calls[1], 1).await;
            match stop {
                "global" => fixture.manager.engage(EstopLevel::KillAll).unwrap(),
                "alias" => fixture
                    .manager
                    .engage(EstopLevel::ToolFreeze(vec!["delegate".into()]))
                    .unwrap(),
                _ => {
                    token.cancel();
                }
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(execution, stop_future)
        })
        .await
        .unwrap();
        let error = result.err().expect("stopped delegated execution");
        assert_eq!(
            settlement::error_kind(&error),
            if stop == "user" {
                "cancelled"
            } else {
                "emergency_stop"
            }
        );
        settlement_assert_partial(&error, &fixture);
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "no provider retry after partial completion"
        );
        if stop != "user" {
            fixture
                .manager
                .resume(
                    if stop == "global" {
                        ResumeSelector::KillAll
                    } else {
                        ResumeSelector::Tools(vec!["delegate".into()])
                    },
                    None,
                    None,
                )
                .unwrap();
            assert!(
                settlement_dispatch(
                    fixture.tool.clone(),
                    json!({"agent":"target","prompt":"new work after resume"}),
                    None
                )
                .await
                .unwrap()
                .success
            );
            assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
            assert!(
                fixture
                    .scopes
                    .lock()
                    .iter()
                    .all(|scope| scope.check().is_err()),
                "resume never clears old invocation"
            );
        }
    }
}

#[tokio::test]
async fn estop_delegate_settlement_actual_agentic_deadline_retains_original_partial() {
    let provider = SettlementProvider::start(settlement_pipeline_response()).await;
    let fixture = settlement_fixture(&[("target", &provider)], false);
    let deadline = Instant::now() + Duration::from_secs(2);
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        zeroclaw_api::deadline::PARENT.scope(
            Some(deadline),
            settlement_dispatch(
                fixture.tool.clone(),
                json!({"agent":"target","prompt":"fixture work"}),
                None,
            ),
        ),
    )
    .await
    .unwrap()
    .err()
    .expect("typed deadline");
    assert_eq!(settlement::error_kind(&error), "deadline");
    settlement_assert_partial(&error, &fixture);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_delegate_settlement_broken_child_has_explicit_unsettled_evidence() {
    let provider = SettlementProvider::start(settlement_pipeline_response()).await;
    let mut fixture = settlement_fixture(&[("target", &provider)], true);
    let execution = settlement_dispatch(
        fixture.tool.clone(),
        json!({"agent":"target","prompt":"fixture work"}),
        None,
    );
    let stop = async {
        settlement_wait(&fixture.calls[1], 1).await;
        fixture
            .manager
            .engage(EstopLevel::ToolFreeze(vec!["delegate".into()]))
            .unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(execution, stop)
    })
    .await
    .unwrap();
    let error = result.err().expect("broken child remains unknown");
    settlement_assert_partial(&error, &fixture);
    assert_eq!(settlement_pipeline(&error).unsettled.len(), 1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_delegate_settlement_background_waits_for_owner_and_persists_typed_partial() {
    let provider = SettlementProvider::start(settlement_pipeline_response()).await;
    let fixture = settlement_fixture(&[("target", &provider)], true);
    let started = settlement_dispatch(
        fixture.tool.clone(),
        json!({"agent":"target","prompt":"fixture work","background":true}),
        None,
    )
    .await
    .unwrap();
    let task_id = started
        .output
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .unwrap()
        .to_owned();
    settlement_wait(&fixture.calls[1], 1).await;
    let requested = fixture
        .tool
        .execute(json!({"action":"cancel_task","task_id":task_id}))
        .await
        .unwrap();
    assert!(requested.success);
    assert!(requested.output.contains("Cancellation requested"));
    assert!(
        DelegateTool::background_task_cancels()
            .lock()
            .contains_key(&task_id),
        "request cannot remove live settlement owner"
    );
    let persisted =
        wait_for_terminal_background_result(&fixture.tool.workspace_dir, &task_id).await;
    assert_eq!(persisted.status, BackgroundTaskStatus::Cancelled);
    let evidence = persisted.evidence.unwrap();
    let serialized = serde_json::to_string(&evidence).unwrap();
    assert!(serialized.contains("original_delegate_payload"));
    assert!(serialized.contains("unsettled"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        !DelegateTool::background_task_cancels()
            .lock()
            .contains_key(&task_id)
    );
    assert_eq!(fixture.calls[2].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn estop_delegate_settlement_spawn_inherits_original_invocation_and_fresh_authority() {
    let provider = SettlementProvider::start(settlement_pipeline_response()).await;
    let fixture = settlement_fixture(&[("target", &provider)], false);
    let invocation = InvocationCancellation::default();
    let started = estop_runtime::scope_invocation(
        Some(invocation.clone()),
        settlement_dispatch(
            fixture.tool.clone(),
            json!({"agent":"target","prompt":"fixture work","background":true}),
            None,
        ),
    )
    .await
    .unwrap();
    let task_id = started
        .output
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .unwrap();
    settlement_wait(&fixture.calls[1], 1).await;
    invocation.request_user();
    let persisted = wait_for_terminal_background_result(&fixture.tool.workspace_dir, task_id).await;
    assert_eq!(persisted.status, BackgroundTaskStatus::Cancelled);
    assert!(
        fixture
            .scopes
            .lock()
            .iter()
            .all(|scope| scope.check().is_err())
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    // Same object resolves the current authority, not the constructor snapshot.
    fixture.live.write().security.estop.enabled = false;
    assert!(
        estop_runtime::scope(
            Some(EstopRuntime::from_live_config(fixture.live.clone())),
            settlement_dispatch(
                fixture.tool.clone(),
                json!({"agent":"target","prompt":"new independent invocation"}),
                None
            )
        )
        .await
        .unwrap()
        .success
    );
}

#[tokio::test]
async fn estop_delegate_settlement_fanout_collects_ordered_results_and_original_child_errors() {
    let slow = SettlementProvider::start(settlement_pipeline_response()).await;
    let complete =
        SettlementProvider::start(json!({"choices":[{"message":{"content":"completed sibling"}}]}))
            .await;
    let release = Arc::new(tokio::sync::Notify::new());
    let frozen = SettlementProvider::start_after(
        chat_completion_tool_call(
            zeroclaw_tools::pipeline::PipelineTool::NAME,
            "frozen-pipeline",
            json!({"steps":[
                {"tool":"first","args":{}},{"tool":"tail","args":{}}
            ]}),
        ),
        Some(release.clone()),
    )
    .await;
    let mut fixture = settlement_fixture(
        &[
            ("slow_target", &slow),
            ("complete_target", &complete),
            ("frozen_target", &frozen),
        ],
        true,
    );
    let execution = settlement_dispatch(
        fixture.tool.clone(),
        json!({"parallel":["slow_target","complete_target","frozen_target"],"prompt":"fixture work"}),
        None,
    );
    let stop = async {
        settlement_wait(&fixture.calls[1], 1).await;
        settlement_wait(&complete.calls, 1).await;
        settlement_wait(&frozen.calls, 1).await;
        fixture
            .manager
            .engage(EstopLevel::ToolFreeze(vec!["tail".into()]))
            .unwrap();
        release.notify_one();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(execution, stop)
    })
    .await
    .unwrap();
    let error = result.err().expect("fanout terminal result");
    assert_eq!(settlement::error_kind(&error), "emergency_stop");
    let evidence = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<settlement::DelegateTerminalError>())
        .expect("original owned fanout results");
    assert_eq!(
        evidence
            .completed
            .iter()
            .map(|child| child.index)
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert!(
        evidence.completed[0]
            .result
            .output
            .contains("completed sibling")
    );
    assert_eq!(
        evidence
            .failures
            .iter()
            .map(|child| child.index)
            .collect::<Vec<_>>(),
        vec![0, 2]
    );
    assert_eq!(evidence.first_terminal, Some(2));
    for child in &evidence.failures {
        let partial = settlement_pipeline(&child.error);
        assert_eq!(
            partial.completed[0].result.output.data(),
            Some(&json!({"original_delegate_payload":"first"}))
        );
    }
    assert_eq!(
        settlement_pipeline(&evidence.failures[0].error)
            .unsettled
            .len(),
        1
    );
    assert!(evidence.unsettled.is_empty());
    assert!(evidence.unstarted.is_empty());
    assert_eq!(
        fixture.calls[2].load(Ordering::SeqCst),
        0,
        "frozen admission prevents tail execution"
    );
    assert_eq!(slow.calls.load(Ordering::SeqCst), 1);
    assert_eq!(complete.calls.load(Ordering::SeqCst), 1);
    assert_eq!(frozen.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn estop_delegate_settlement_error_text_cannot_forge_cancellation() {
    assert_eq!(
        settlement::error_kind(&anyhow::Error::msg("Cancelled by provider display text")),
        "failure"
    );
    assert_eq!(
        settlement::error_kind(
            &anyhow::Error::from(crate::agent::loop_::ToolLoopCancelled)
                .context("original envelope")
        ),
        "cancelled"
    );
}

#[tokio::test]
async fn estop_delegate_settlement_cancel_without_live_owner_preserves_unknown_record() {
    let provider =
        SettlementProvider::start(json!({"choices":[{"message":{"content":"unused"}}]})).await;
    let fixture = settlement_fixture(&[("target", &provider)], false);
    let task_id = uuid::Uuid::new_v4().to_string();
    let original = background_result(&task_id, BackgroundTaskStatus::Running, None, None);
    write_background_result(&fixture.tool.workspace_dir, &original);
    let path = fixture
        .tool
        .workspace_dir
        .join("delegate_results")
        .join(format!("{task_id}.json"));
    let before = std::fs::read(&path).unwrap();
    let result = fixture
        .tool
        .execute(json!({"action":"cancel_task","task_id":task_id}))
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("outcome is unknown"));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn estop_delegate_settlement_background_keeps_original_dispatcher_caller_after_handoff() {
    let provider = SettlementProvider::start(settlement_pipeline_response()).await;
    let fixture = settlement_fixture(&[("target", &provider)], false);
    let caller = CancellationToken::new();
    let started = settlement_dispatch(
        fixture.tool.clone(),
        json!({"agent":"target","prompt":"fixture work","background":true}),
        Some(&caller),
    )
    .await
    .unwrap();
    let task_id = started
        .output
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .unwrap();
    settlement_wait(&fixture.calls[1], 1).await;
    caller.cancel();
    let persisted = wait_for_terminal_background_result(&fixture.tool.workspace_dir, task_id).await;
    assert_eq!(persisted.status, BackgroundTaskStatus::Cancelled);
    assert!(
        serde_json::to_string(&persisted.evidence)
            .unwrap()
            .contains("original_delegate_payload")
    );
    assert!(
        fixture
            .scopes
            .lock()
            .iter()
            .all(|scope| scope.check().is_err())
    );
    assert_eq!(fixture.calls[2].load(Ordering::SeqCst), 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn estop_delegate_settlement_private_result_creation_and_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("result.json");
    let record = background_result(
        "fixture",
        BackgroundTaskStatus::Cancelled,
        None,
        Some("typed stop"),
    );
    DelegateTool::write_result_atomic(&path, &record)
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    DelegateTool::write_result_atomic(&path, &record)
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        1,
        "no staging file remains"
    );
}

#[tokio::test]
async fn estop_delegate_settlement_background_local_stop_does_not_cancel_parent_or_sibling() {
    for local_stop in ["cancel_task", "deadline", "parent"] {
        let first = SettlementProvider::start(settlement_pipeline_response()).await;
        let second = SettlementProvider::start(settlement_pipeline_response()).await;
        let mut fixture = settlement_fixture(&[("target_a", &first), ("target_b", &second)], false);
        if local_stop == "deadline" {
            let tool = Arc::make_mut(&mut fixture.tool);
            let mut short = tool.runtime_profiles["fixture"].clone();
            short.agentic_timeout_secs = Some(1);
            Arc::make_mut(&mut tool.runtime_profiles).insert("short".into(), short);
            Arc::make_mut(&mut tool.agents)
                .get_mut("target_a")
                .unwrap()
                .runtime_profile = "short".into();
        }
        let caller = CancellationToken::new();
        let parent = InvocationCancellation::default();
        let parent_calls = Arc::new(AtomicUsize::new(0));
        let parent_tool = SettlementStep {
            name: "parent_wait",
            calls: parent_calls.clone(),
            wait: true,
            stubborn: false,
            scopes: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let parent_wait = estop_runtime::scope_invocation(
            Some(parent.clone()),
            estop_runtime::scope(
                Some(EstopRuntime::from_live_config(fixture.live.clone())),
                estop_runtime::run_tool(
                    &parent_tool,
                    Some(&caller),
                    parent_tool.execute(json!({})),
                ),
            ),
        );
        let actions = async {
            settlement_wait(&parent_calls, 1).await;
            let mut ids = Vec::new();
            for target in ["target_a", "target_b"] {
                let started = estop_runtime::scope_invocation(
                    Some(parent.clone()),
                    settlement_dispatch(
                        fixture.tool.clone(),
                        json!({"agent":target,"prompt":"fixture work","background":true}),
                        Some(&caller),
                    ),
                )
                .await
                .unwrap();
                ids.push(
                    started
                        .output
                        .lines()
                        .find_map(|line| line.strip_prefix("task_id: "))
                        .unwrap()
                        .to_owned(),
                );
            }
            settlement_wait(&fixture.calls[1], 2).await;
            if local_stop == "cancel_task" {
                assert!(
                    fixture
                        .tool
                        .execute(json!({"action":"cancel_task","task_id":ids[0]}))
                        .await
                        .unwrap()
                        .success
                );
            }
            if local_stop == "parent" {
                caller.cancel();
            }
            let first_result =
                wait_for_terminal_background_result(&fixture.tool.workspace_dir, &ids[0]).await;
            assert_eq!(first_result.status, BackgroundTaskStatus::Cancelled);
            if local_stop != "parent" {
                assert!(
                    parent.check().is_ok(),
                    "child-local interruption must not publish upward"
                );
                assert!(!caller.is_cancelled());
            }
            let sibling: BackgroundDelegateResult = serde_json::from_slice(
                &std::fs::read(
                    fixture
                        .tool
                        .workspace_dir
                        .join("delegate_results")
                        .join(format!("{}.json", ids[1])),
                )
                .unwrap(),
            )
            .unwrap();
            if local_stop != "parent" {
                assert_eq!(sibling.status, BackgroundTaskStatus::Running);
            }
            caller.cancel();
            let second_result =
                wait_for_terminal_background_result(&fixture.tool.workspace_dir, &ids[1]).await;
            assert_eq!(second_result.status, BackgroundTaskStatus::Cancelled);
            assert_eq!(first.calls.load(Ordering::SeqCst), 1);
            assert_eq!(second.calls.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.calls[2].load(Ordering::SeqCst), 0);
        };
        let (parent_result, ()) = tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(parent_wait, actions)
        })
        .await
        .unwrap();
        assert_eq!(
            settlement::error_kind(&parent_result.unwrap_err()),
            "cancelled"
        );
    }
}

#[test]
fn estop_delegate_settlement_keeps_sop_ownership_failure_without_cancellation() {
    use crate::agent::turn::sop_settlement::{SopDriveInterrupted, SopPhaseIncomplete};
    let original = anyhow::Error::new(SopDriveInterrupted {
        cause: SopPhaseIncomplete { phase: "assembly" }.into(),
        steps: Vec::new(),
        queued: Vec::new(),
    });
    assert!(
        settlement::terminal(&original),
        "owned incomplete work is never a retryable provider failure"
    );
    let wrapped = anyhow::Error::new(settlement::DelegateAgenticError {
        history: Vec::new(),
        error: original,
    });
    let evidence = settlement::error_evidence(&wrapped);
    assert_eq!(evidence["kind"], "agentic_terminal");
    assert_eq!(evidence["error"]["kind"], "sop_terminal");
    assert_eq!(
        settlement::error_kind(&wrapped),
        "failure",
        "ownership incompleteness cannot forge operator cancellation"
    );
}
