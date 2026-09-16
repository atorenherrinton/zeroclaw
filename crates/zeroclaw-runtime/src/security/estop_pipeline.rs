//! Bridge lower-level pipeline dispatch to the current runtime authority.

use std::sync::Arc;

use async_trait::async_trait;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_tools::pipeline::PipelineExecutionContext;

use super::estop_runtime::{self, EstopRuntime, InvocationCancellation};

struct EstopPipelineContext {
    authority: Option<EstopRuntime>,
    invocation: Option<InvocationCancellation>,
}

#[async_trait]
impl PipelineExecutionContext for EstopPipelineContext {
    async fn execute(
        &self,
        tool: &dyn Tool,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        // Explicitly carry both invocation ownership and canonical authority
        // into spawned children. Neither handle contains a cached policy fact.
        estop_runtime::scope(
            self.authority.clone(),
            estop_runtime::scope_invocation(
                self.invocation.clone(),
                estop_runtime::run_tool(tool, None, tool.execute(args)),
            ),
        )
        .await
    }

    fn is_terminal_error(&self, error: &anyhow::Error) -> bool {
        estop_runtime::is_estop_interrupted(error)
            || crate::agent::loop_::is_tool_loop_cancelled(error)
            || error.is::<zeroclaw_api::deadline::DeadlineExceeded>()
            || error
                .chain()
                .any(|cause| cause.is::<zeroclaw_api::deadline::DeadlineExceeded>())
    }

    fn check_interrupted(&self) -> anyhow::Result<()> {
        self.invocation
            .as_ref()
            .map_or(Ok(()), InvocationCancellation::check)
    }

    async fn interrupted(&self) -> anyhow::Error {
        if let Some(invocation) = &self.invocation {
            invocation.interrupted().await
        } else {
            std::future::pending().await
        }
    }
}

pub(crate) fn current_context() -> Option<Arc<dyn PipelineExecutionContext>> {
    let authority = estop_runtime::current();
    let invocation = estop_runtime::current_invocation();
    if authority.is_none() && invocation.is_none() {
        None
    } else {
        Some(Arc::new(EstopPipelineContext {
            authority,
            invocation,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use zeroclaw_api::attribution::{Attributable, Role, ToolKind};
    use zeroclaw_api::tool::ToolOutput;
    use zeroclaw_config::schema::{Config, PipelineConfig};
    use zeroclaw_tools::pipeline::{PipelineTerminalError, PipelineTool};

    use crate::security::estop::{EstopLevel, EstopManager, ResumeSelector};
    use crate::skills::SkillTool;
    use crate::tools::SkillBuiltinTool;

    #[test]
    fn estop_pipeline_preserves_deadline_in_anyhow_typed_context() {
        let context = EstopPipelineContext {
            authority: None,
            invocation: None,
        };
        let error = anyhow::Error::msg("original child evidence").context(
            zeroclaw_api::deadline::DeadlineExceeded {
                phase: zeroclaw_api::deadline::Phase::Tool,
                started: true,
            },
        );
        assert!(context.is_terminal_error(&error));
    }

    struct CountingTool {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl Attributable for CountingTool {
        fn role(&self) -> Role {
            Role::Tool(ToolKind::Plugin)
        }
        fn alias(&self) -> &str {
            self.name
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Synthetic counted boundary"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
            assert!(
                estop_runtime::current().is_some(),
                "spawned child must inherit authority"
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            if args.get("wait").and_then(|value| value.as_bool()) == Some(true) {
                std::future::pending::<()>().await;
            }
            Ok(ToolResult {
                success: true,
                output: ToolOutput::json_with_text(
                    json!({"retained": true}),
                    "synthetic completed evidence",
                ),
                error: None,
            })
        }
    }

    fn authority() -> (
        tempfile::TempDir,
        Arc<parking_lot::RwLock<Config>>,
        EstopManager,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            config_path: directory.path().join("config.toml"),
            ..Config::default()
        };
        config.security.estop.enabled = true;
        config.security.estop.require_otp_to_resume = false;
        config.security.estop.state_file = "composite-estop.json".into();
        let manager = EstopManager::load(&config.security.estop, directory.path()).unwrap();
        (
            directory,
            Arc::new(parking_lot::RwLock::new(config)),
            manager,
        )
    }

    fn counter(name: &'static str) -> (Arc<dyn Tool>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(CountingTool {
                name,
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn skill(target: Arc<dyn Tool>) -> SkillBuiltinTool {
        let definition = SkillTool {
            name: "alias".into(),
            description: "Synthetic skill wrapper".into(),
            kind: "builtin".into(),
            command: String::new(),
            args: HashMap::new(),
            target: Some(target.name().into()),
            locked_args: HashMap::new(),
            timeout_secs: None,
        };
        SkillBuiltinTool::new("fixture", &definition, target, HashMap::new())
    }

    fn pipeline(tools: Vec<Arc<dyn Tool>>) -> PipelineTool {
        let config = PipelineConfig {
            enabled: true,
            allowed_tools: tools.iter().map(|tool| tool.name().into()).collect(),
            ..PipelineConfig::default()
        };
        PipelineTool::new(config, tools).with_execution_context_resolver(Arc::new(current_context))
    }

    async fn wait_for_calls(calls: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn dispatch(
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
        token: Option<&tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<crate::agent::tool_execution::ToolExecutionOutcome> {
        use crate::agent::tool_execution::{ToolDispatchContext, execute_one_tool};
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::ArcToolRef(tool))];
        execute_one_tool(
            tools[0].name(),
            args,
            Some("composite-fixture-call"),
            ToolDispatchContext {
                tools_registry: &tools,
                activated_tools: None,
                excluded_tools: &[],
                model_switch_callback: None,
            },
            &crate::agent::turn::TurnMeta {
                agent_alias: None,
                parent_agent_alias: None,
                turn_id: "composite-fixture",
                channel_name: "test",
            },
            &crate::observability::NoopObserver,
            token,
            None,
            None,
        )
        .await
    }

    fn retained_pipeline(error: &anyhow::Error) -> &PipelineTerminalError {
        let evidence = error
            .downcast_ref::<PipelineTerminalError>()
            .expect("original pipeline evidence must survive outer dispatch");
        assert!(!format!("{error:?}").contains("synthetic completed evidence"));
        evidence
    }

    #[tokio::test]
    async fn estop_composite_dispatch_global_stop_preserves_ordered_structured_siblings() {
        let (_directory, live, mut manager) = authority();
        let (first, first_calls) = counter("first");
        let (second, second_calls) = counter("second");
        let (slow, slow_calls) = counter("slow");
        let tool: Arc<dyn Tool> = Arc::new(pipeline(vec![first, second, slow]));
        let runtime = EstopRuntime::from_live_config(live);
        let args = json!({"parallel":true,"steps":[{"tool":"first","args":{}},{"tool":"second","args":{}},{"tool":"slow","args":{"wait":true}}]});
        let execution =
            estop_runtime::scope(Some(runtime.clone()), dispatch(tool.clone(), args, None));
        let stop = async {
            wait_for_calls(&first_calls, 1).await;
            wait_for_calls(&second_calls, 1).await;
            wait_for_calls(&slow_calls, 1).await;
            manager.engage(EstopLevel::KillAll).unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(execution, stop)
        })
        .await
        .unwrap();
        let error = result
            .err()
            .expect("global stop must terminate outer dispatch");
        assert!(estop_runtime::is_estop_interrupted(&error));
        let evidence = retained_pipeline(&error);
        assert_eq!(
            evidence
                .completed
                .iter()
                .map(|step| step.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        for step in &evidence.completed {
            assert_eq!(step.result.output.data(), Some(&json!({"retained":true})));
        }
        manager.resume(ResumeSelector::KillAll, None, None).unwrap();
        assert!(
            estop_runtime::scope(
                Some(runtime),
                dispatch(
                    tool,
                    json!({"parallel":true,"steps":[{"tool":"slow","args":{}}]}),
                    None
                )
            )
            .await
            .unwrap()
            .success
        );
        assert_eq!(slow_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn estop_composite_dispatch_outer_skill_freeze_settles_nested_pipeline() {
        let (_directory, live, mut manager) = authority();
        let (fast, fast_calls) = counter("fast");
        let (slow, slow_calls) = counter("slow");
        let tool: Arc<dyn Tool> = Arc::new(skill(Arc::new(pipeline(vec![fast, slow]))));
        assert!(tool.supports_cooperative_settlement());
        let runtime = EstopRuntime::from_live_config(live);
        let args = json!({"parallel":true,"steps":[{"tool":"fast","args":{}},{"tool":"slow","args":{"wait":true}}]});
        let execution = estop_runtime::scope(Some(runtime), dispatch(tool, args, None));
        let stop = async {
            wait_for_calls(&fast_calls, 1).await;
            wait_for_calls(&slow_calls, 1).await;
            manager
                .engage(EstopLevel::ToolFreeze(vec!["fixture__alias".into()]))
                .unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(execution, stop)
        })
        .await
        .unwrap();
        let error = result
            .err()
            .expect("outer skill freeze must terminate dispatch");
        assert!(estop_runtime::is_estop_interrupted(&error));
        let evidence = retained_pipeline(&error);
        assert_eq!(evidence.completed.len(), 1);
        assert_eq!(evidence.completed[0].tool, "fast");
        assert_eq!(
            evidence.completed[0].result.output.data(),
            Some(&json!({"retained":true}))
        );
    }

    #[tokio::test]
    async fn estop_composite_dispatch_parent_token_preserves_prefix_and_never_starts_tail() {
        let (_directory, live, _manager) = authority();
        let (first, first_calls) = counter("first");
        let (slow, slow_calls) = counter("slow");
        let (tail, tail_calls) = counter("tail");
        let tool: Arc<dyn Tool> = Arc::new(pipeline(vec![first, slow, tail]));
        let token = tokio_util::sync::CancellationToken::new();
        let args = json!({"steps":[{"tool":"first","args":{}},{"tool":"slow","args":{"wait":true}},{"tool":"tail","args":{}}]});
        let execution = estop_runtime::scope(
            Some(EstopRuntime::from_live_config(live)),
            dispatch(tool, args, Some(&token)),
        );
        let stop = async {
            wait_for_calls(&first_calls, 1).await;
            wait_for_calls(&slow_calls, 1).await;
            token.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(execution, stop)
        })
        .await
        .unwrap();
        let error = result
            .err()
            .expect("parent token must terminate outer dispatch");
        assert!(crate::agent::loop_::is_tool_loop_cancelled(&error));
        let evidence = retained_pipeline(&error);
        assert_eq!(evidence.completed.len(), 1);
        assert_eq!(evidence.completed[0].tool, "first");
        assert_eq!(tail_calls.load(Ordering::SeqCst), 0);
    }

    struct StubbornComposite {
        calls: Arc<AtomicUsize>,
    }
    zeroclaw_api::mock_tool_attribution!(StubbornComposite);
    #[async_trait]
    impl Tool for StubbornComposite {
        fn name(&self) -> &str {
            "stubborn"
        }
        fn description(&self) -> &str {
            "Synthetic broken settlement contract"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type":"object"})
        }
        fn supports_cooperative_settlement(&self) -> bool {
            true
        }
        async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn estop_composite_dispatch_bounds_broken_child_settlement_and_preserves_parent_evidence()
    {
        let (_directory, live, mut manager) = authority();
        let (fast, fast_calls) = counter("fast");
        let stubborn_calls = Arc::new(AtomicUsize::new(0));
        let tool: Arc<dyn Tool> = Arc::new(pipeline(vec![
            fast,
            Arc::new(StubbornComposite {
                calls: stubborn_calls.clone(),
            }),
        ]));
        let args = json!({"parallel":true,"steps":[{"tool":"fast","args":{}},{"tool":"stubborn","args":{}}]});
        let execution = estop_runtime::scope(
            Some(EstopRuntime::from_live_config(live)),
            dispatch(tool, args, None),
        );
        let stop = async {
            wait_for_calls(&fast_calls, 1).await;
            wait_for_calls(&stubborn_calls, 1).await;
            manager.engage(EstopLevel::KillAll).unwrap();
        };
        let started = std::time::Instant::now();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(execution, stop)
        })
        .await
        .unwrap();
        let error = result
            .err()
            .expect("broken settlement must remain terminal");
        let evidence = retained_pipeline(&error);
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert_eq!(evidence.completed.len(), 1);
        assert_eq!(evidence.unsettled.len(), 1);
        assert_eq!(evidence.unsettled[0].tool, "stubborn");
        assert_eq!(
            evidence.unsettled[0].reason,
            zeroclaw_tools::pipeline::PipelineUnsettledReason::SettlementTimeout
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn estop_skill_inner_name_uses_live_authority_and_resumes_same_object() {
        let (_directory, live, mut manager) = authority();
        let (target, calls) = counter("actual_inner");
        let tool = skill(target);
        let runtime = EstopRuntime::from_live_config(live.clone());
        manager
            .engage(EstopLevel::ToolFreeze(vec!["actual_inner".into()]))
            .unwrap();
        let error = estop_runtime::scope(Some(runtime.clone()), tool.execute(json!({})))
            .await
            .unwrap_err();
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert!(crate::agent::loop_::is_tool_loop_cancelled(&error));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        live.write().security.estop.enabled = false;
        assert!(
            estop_runtime::scope(Some(runtime.clone()), tool.execute(json!({})))
                .await
                .unwrap()
                .success
        );
        live.write().security.estop.enabled = true;
        assert!(
            estop_runtime::scope(Some(runtime.clone()), tool.execute(json!({})))
                .await
                .is_err()
        );
        manager
            .resume(
                ResumeSelector::Tools(vec!["actual_inner".into()]),
                None,
                None,
            )
            .unwrap();
        assert!(
            estop_runtime::scope(Some(runtime), tool.execute(json!({})))
                .await
                .unwrap()
                .success
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn estop_pipeline_sequential_stops_tail_and_keeps_completed_evidence() {
        let (_directory, live, mut manager) = authority();
        let (first, first_calls) = counter("first");
        let (frozen, frozen_calls) = counter("frozen");
        let (tail, tail_calls) = counter("tail");
        let tool = pipeline(vec![first, frozen, tail]);
        let args = json!({"steps": [{"tool":"first","args":{}}, {"tool":"frozen","args":{}}, {"tool":"tail","args":{}}]});
        let runtime = EstopRuntime::from_live_config(live);
        manager
            .engage(EstopLevel::ToolFreeze(vec!["frozen".into()]))
            .unwrap();
        let error = estop_runtime::scope(Some(runtime.clone()), tool.execute(args.clone()))
            .await
            .unwrap_err();
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert!(crate::agent::loop_::is_tool_loop_cancelled(&error));
        let evidence = error.downcast_ref::<PipelineTerminalError>().unwrap();
        assert_eq!(evidence.completed.len(), 1);
        assert_eq!(evidence.completed[0].tool, "first");
        assert_eq!(
            evidence.completed[0].result.output,
            "synthetic completed evidence"
        );
        assert_eq!(evidence.failures[0].index, 1);
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(frozen_calls.load(Ordering::SeqCst), 0);
        assert_eq!(tail_calls.load(Ordering::SeqCst), 0);
        manager
            .resume(ResumeSelector::Tools(vec!["frozen".into()]), None, None)
            .unwrap();
        assert!(
            estop_runtime::scope(Some(runtime), tool.execute(args))
                .await
                .unwrap()
                .success
        );
        assert_eq!(frozen_calls.load(Ordering::SeqCst), 1);
        assert_eq!(tail_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn estop_pipeline_spawn_carries_authority_through_nested_skill() {
        let (_directory, live, mut manager) = authority();
        let (target, calls) = counter("nested_inner");
        let tool = pipeline(vec![Arc::new(skill(target))]);
        let args = json!({"parallel":true,"steps":[{"tool":"fixture__alias","args":{}}]});
        let runtime = EstopRuntime::from_live_config(live);
        manager
            .engage(EstopLevel::ToolFreeze(vec!["nested_inner".into()]))
            .unwrap();
        let error = estop_runtime::scope(Some(runtime.clone()), tool.execute(args.clone()))
            .await
            .unwrap_err();
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        manager
            .resume(
                ResumeSelector::Tools(vec!["nested_inner".into()]),
                None,
                None,
            )
            .unwrap();
        assert!(
            estop_runtime::scope(Some(runtime), tool.execute(args))
                .await
                .unwrap()
                .success
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn estop_pipeline_active_parallel_freeze_keeps_completed_sibling_evidence() {
        let (_directory, live, mut manager) = authority();
        let (fast, fast_calls) = counter("fast");
        let (slow, slow_calls) = counter("slow");
        let tool = pipeline(vec![fast, slow]);
        let runtime = EstopRuntime::from_live_config(live);
        let args = json!({"parallel":true,"steps":[{"tool":"fast","args":{}},{"tool":"slow","args":{"wait":true}}]});
        let execution = estop_runtime::scope(Some(runtime.clone()), tool.execute(args));
        let engage = async {
            wait_for_calls(&fast_calls, 1).await;
            wait_for_calls(&slow_calls, 1).await;
            manager
                .engage(EstopLevel::ToolFreeze(vec!["slow".into()]))
                .unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(execution, engage)
        })
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert!(estop_runtime::is_estop_interrupted(&error));
        let evidence = error.downcast_ref::<PipelineTerminalError>().unwrap();
        assert_eq!(evidence.completed.len(), 1);
        assert_eq!(evidence.completed[0].tool, "fast");
        assert!(evidence.completed[0].result.success);
        assert_eq!(
            evidence.completed[0].result.output,
            "synthetic completed evidence"
        );
        assert_eq!(
            evidence.completed[0].result.output.data(),
            Some(&json!({"retained": true}))
        );
        assert_eq!(evidence.failures[0].tool, "slow");
        assert!(!format!("{error:?}").contains("synthetic completed evidence"));
        manager
            .resume(ResumeSelector::Tools(vec!["slow".into()]), None, None)
            .unwrap();
        assert!(estop_runtime::scope(Some(runtime), tool.execute(json!({"parallel":true,"steps":[{"tool":"fast","args":{}},{"tool":"slow","args":{}}]}))).await.unwrap().success);
        assert_eq!(fast_calls.load(Ordering::SeqCst), 2);
        assert_eq!(slow_calls.load(Ordering::SeqCst), 2);
    }
}
