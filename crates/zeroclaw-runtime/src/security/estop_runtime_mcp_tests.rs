//! Real runtime/skill admission captures the lower MCP control before cancellation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::schema::Config;
use zeroclaw_tools::mcp_lifecycle::{McpLifecycleControl, current_mcp_lifecycle_control};

use super::estop::{EstopLevel, EstopManager, ResumeSelector};
use super::estop_runtime::{self, EstopRuntime};
use crate::skills::SkillTool;
use crate::tools::SkillBuiltinTool;

struct CaptureMcpControl {
    ready: parking_lot::Mutex<Option<oneshot::Sender<Arc<dyn McpLifecycleControl>>>>,
}

zeroclaw_api::mock_tool_attribution!(CaptureMcpControl);

#[async_trait::async_trait]
impl Tool for CaptureMcpControl {
    fn name(&self) -> &str {
        "fixture_server__inner"
    }
    fn description(&self) -> &str {
        "Synthetic MCP control capture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _: serde_json::Value) -> anyhow::Result<ToolResult> {
        let control = current_mcp_lifecycle_control().expect("actual tool owns MCP authority");
        assert!(control.check().is_ok());
        let sender = self.ready.lock().take().expect("one synthetic invocation");
        assert!(sender.send(control).is_ok());
        std::future::pending().await
    }
}

type McpControlFixture = (
    tempfile::TempDir,
    Arc<parking_lot::RwLock<Config>>,
    EstopManager,
    SkillBuiltinTool,
    oneshot::Receiver<Arc<dyn McpLifecycleControl>>,
);

fn fixture() -> McpControlFixture {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config {
        config_path: directory.path().join("config.toml"),
        ..Config::default()
    };
    config.security.estop.enabled = true;
    config.security.estop.require_otp_to_resume = false;
    config.security.estop.state_file = "mcp-adapter-estop.json".into();
    assert_eq!(config.install_root_dir(), directory.path());
    let manager = EstopManager::load(&config.security.estop, directory.path()).unwrap();
    assert!(manager.state_path().starts_with(directory.path()));
    let (sender, receiver) = oneshot::channel();
    let target: Arc<dyn Tool> = Arc::new(CaptureMcpControl {
        ready: parking_lot::Mutex::new(Some(sender)),
    });
    let definition = SkillTool {
        name: "alias".into(),
        description: "Synthetic outer skill".into(),
        kind: "builtin".into(),
        command: String::new(),
        args: HashMap::new(),
        target: Some(target.name().into()),
        locked_args: HashMap::new(),
        timeout_secs: None,
    };
    let skill = SkillBuiltinTool::new("mcp_fixture", &definition, target, HashMap::new());
    (
        directory,
        Arc::new(parking_lot::RwLock::new(config)),
        manager,
        skill,
        receiver,
    )
}

#[tokio::test(start_paused = true)]
async fn estop_mcp_adapter_outer_alias_controls_replacement_until_live_resume() {
    let (_directory, config, mut manager, skill, receiver) = fixture();
    let runtime = EstopRuntime::from_live_config(config.clone());
    let execution = estop_runtime::scope(
        Some(runtime.clone()),
        estop_runtime::run_tool(&skill, None, skill.execute(serde_json::json!({}))),
    );
    tokio::pin!(execution);
    let control = tokio::select! {
        control = receiver => control.unwrap(),
        result = &mut execution => panic!("tool returned before MCP capture: {result:?}"),
    };
    manager
        .engage(EstopLevel::ToolFreeze(vec![skill.name().into()]))
        .unwrap();
    assert!(runtime.check(Some("fixture_server__inner")).is_ok());
    let error = tokio::time::timeout(Duration::from_secs(3), &mut execution)
        .await
        .expect("outer alias monitor must stop the pending skill")
        .unwrap_err();
    assert!(estop_runtime::is_estop_interrupted(&error));
    assert!(estop_runtime::is_estop_interrupted(
        &control.check().unwrap_err()
    ));
    assert!(control.check_replacement().is_err());

    // The same captured Arc resolves the live path; it does not cache a stop
    // decision or independently reload a config file.
    config.write().security.estop.state_file = "alternate-mcp-estop.json".into();
    assert!(control.check_replacement().is_ok());
    assert!(
        control.check().is_err(),
        "the cancelled request never resumes"
    );
    config.write().security.estop.state_file = "mcp-adapter-estop.json".into();
    assert!(control.check_replacement().is_err());
    manager
        .resume(ResumeSelector::Tools(vec![skill.name().into()]), None, None)
        .unwrap();
    assert!(control.check_replacement().is_ok());
    assert!(estop_runtime::is_estop_interrupted(
        &control.check().unwrap_err()
    ));
}

#[tokio::test(start_paused = true)]
async fn estop_mcp_adapter_user_and_deadline_stop_only_the_old_invocation() {
    for (with_authority, use_deadline) in
        [(true, false), (true, true), (false, false), (false, true)]
    {
        let (_directory, config, _manager, skill, receiver) = fixture();
        let token = CancellationToken::new();
        let deadline =
            use_deadline.then(|| tokio::time::Instant::now() + Duration::from_millis(10));
        let execution = zeroclaw_api::deadline::PARENT.scope(
            deadline,
            estop_runtime::scope(
                with_authority.then(|| EstopRuntime::from_live_config(config)),
                estop_runtime::run_tool(&skill, Some(&token), skill.execute(serde_json::json!({}))),
            ),
        );
        tokio::pin!(execution);
        let control = tokio::select! {
            control = receiver => control.unwrap(),
            result = &mut execution => panic!("tool returned before MCP capture: {result:?}"),
        };
        if !use_deadline {
            token.cancel();
        }
        let error = tokio::time::timeout(Duration::from_secs(3), &mut execution)
            .await
            .expect("runtime must interrupt the pending skill")
            .unwrap_err();
        let old = control.check().unwrap_err();
        if use_deadline {
            assert!(error.is::<zeroclaw_api::deadline::DeadlineExceeded>());
            assert!(old.is::<zeroclaw_api::deadline::DeadlineExceeded>());
        } else {
            assert!(crate::agent::loop_::is_tool_loop_cancelled(&error));
            assert!(crate::agent::loop_::is_tool_loop_cancelled(&old));
        }
        assert!(control.check_replacement().is_ok());
        assert!(control.check().is_err());
    }
}
