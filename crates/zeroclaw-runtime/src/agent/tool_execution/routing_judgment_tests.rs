//! Admission and interruption at the real pre-turn execution boundary.

use super::*;
use crate::security::estop::{EstopLevel, EstopManager};
use crate::security::estop_runtime::{self, EstopRuntime};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use zeroclaw_config::autonomy::AutonomyLevel;
use zeroclaw_config::schema::{Config, RiskProfileConfig};

const ROUTING_TOOL: &str = "typesafe__typesafe_system_one";

struct Probe {
    name: &'static str,
    entered: AtomicUsize,
    dropped: AtomicUsize,
    started: Notify,
    wait: bool,
    cooperative: bool,
}

impl Probe {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            entered: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            started: Notify::new(),
            wait: false,
            cooperative: false,
        }
    }
}

zeroclaw_api::mock_tool_attribution!(Probe);

struct OnDrop<'a>(&'a AtomicUsize);
impl Drop for OnDrop<'_> {
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
        "Local routing boundary fixture"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn supports_cooperative_settlement(&self) -> bool {
        self.cooperative
    }
    async fn execute(&self, args: serde_json::Value) -> Result<crate::tools::ToolResult> {
        let _guard = OnDrop(&self.dropped);
        self.entered.fetch_add(1, Ordering::SeqCst);
        assert!(estop_runtime::current().is_some());
        assert!(estop_runtime::current_invocation().is_some());
        assert!(zeroclaw_tools::mcp_lifecycle::current_mcp_lifecycle_control().is_some());
        assert_eq!(args, serde_json::json!({"state":"routing fixture"}));
        self.started.notify_one();
        if self.wait {
            if self.cooperative {
                estop_runtime::current_invocation()
                    .unwrap()
                    .interrupted()
                    .await;
            } else {
                std::future::pending::<()>().await;
            }
        }
        Ok(crate::tools::ToolResult::ok("fixture completed"))
    }
}

fn config(dir: &std::path::Path) -> Config {
    let mut config = Config {
        config_path: dir.join("config.toml"),
        ..Config::default()
    };
    config.security.estop.enabled = true;
    config.security.estop.state_file = "routing-estop.json".into();
    config.security.estop.require_otp_to_resume = false;
    config
}

fn profile(level: AutonomyLevel) -> RiskProfileConfig {
    RiskProfileConfig {
        level,
        auto_approve: vec![ROUTING_TOOL.into()],
        always_ask: vec![],
        ..RiskProfileConfig::default()
    }
}

async fn invoke(
    tool: &dyn Tool,
    approval: &ApprovalManager,
    token: &CancellationToken,
    config: &Config,
) -> Result<crate::tools::ToolResult> {
    execute_routing_judgment(
        tool,
        serde_json::json!({"state":"routing fixture"}),
        approval,
        token,
        config,
    )
    .await
}

#[tokio::test]
async fn routing_judgment_runs_approved_tool_with_execution_authority_and_no_prompt_audit() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let token = CancellationToken::new();
    for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let approval = ApprovalManager::for_non_interactive(&profile(level));
        let probe = Probe::new(ROUTING_TOOL);
        let result = invoke(&probe, &approval, &token, &config).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output.as_str(), "fixture completed");
        assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
        assert_eq!(probe.dropped.load(Ordering::SeqCst), 1);
        assert!(approval.audit_log().is_empty());
    }
}

#[tokio::test]
async fn routing_judgment_refuses_wrong_tool_without_execution() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let approval = ApprovalManager::for_non_interactive(&profile(AutonomyLevel::Full));
    let token = CancellationToken::new();
    for name in [
        "shell",
        "typesafe_system_one",
        "other__typesafe_system_one",
        "typesafe__typesafe_system_one ",
        "TYPESAFE__typesafe_system_one",
    ] {
        let probe = Probe::new(name);
        assert_eq!(
            invoke(&probe, &approval, &token, &config)
                .await
                .unwrap_err()
                .to_string(),
            "routing_judgment_tool_not_allowed"
        );
        assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn routing_judgment_denies_prompt_and_read_only_without_asking_or_execution() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let token = CancellationToken::new();
    let mut ask = profile(AutonomyLevel::Supervised);
    ask.always_ask.push(ROUTING_TOOL.into());
    let mut supervised = profile(AutonomyLevel::Supervised);
    supervised.auto_approve.clear();
    for policy in [ask, supervised, profile(AutonomyLevel::ReadOnly)] {
        for approval in [
            ApprovalManager::from_risk_profile(&policy),
            ApprovalManager::for_non_interactive(&policy),
            ApprovalManager::for_non_interactive_backchannel(&policy),
        ] {
            let probe = Probe::new(ROUTING_TOOL);
            assert_eq!(
                invoke(&probe, &approval, &token, &config)
                    .await
                    .unwrap_err()
                    .to_string(),
                "routing_judgment_approval_required"
            );
            assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
            assert!(approval.audit_log().is_empty());
            assert!(approval.session_allowlist().is_empty());
        }
    }
}

#[tokio::test]
async fn routing_judgment_establishes_estop_before_any_ambient_turn_scope() {
    let approval = ApprovalManager::for_non_interactive(&profile(AutonomyLevel::Full));
    let token = CancellationToken::new();
    for level in [
        EstopLevel::KillAll,
        EstopLevel::NetworkKill,
        EstopLevel::DomainBlock(vec!["api.typesafe.ai".into()]),
        EstopLevel::ToolFreeze(vec![ROUTING_TOOL.into()]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        EstopManager::load(&config.security.estop, dir.path())
            .unwrap()
            .engage(level)
            .unwrap();
        let probe = Probe::new(ROUTING_TOOL);
        let error = invoke(&probe, &approval, &token, &config)
            .await
            .unwrap_err();
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn routing_judgment_preserves_existing_live_estop_authority() {
    let dir = tempfile::tempdir().unwrap();
    let live_config = config(dir.path());
    EstopManager::load(&live_config.security.estop, dir.path())
        .unwrap()
        .engage(EstopLevel::KillAll)
        .unwrap();
    let mut stale_config = live_config.clone();
    stale_config.security.estop.enabled = false;
    let live = Arc::new(parking_lot::RwLock::new(live_config));
    let approval = ApprovalManager::for_non_interactive(&profile(AutonomyLevel::Full));
    let probe = Probe::new(ROUTING_TOOL);
    let token = CancellationToken::new();
    let error = estop_runtime::scope(
        Some(EstopRuntime::from_live_config(live)),
        invoke(&probe, &approval, &token, &stale_config),
    )
    .await
    .unwrap_err();
    assert!(estop_runtime::is_estop_interrupted(&error));
    assert_eq!(probe.entered.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn routing_judgment_observes_estop_engaged_during_execution() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let mut manager = EstopManager::load(&config.security.estop, dir.path()).unwrap();
    let approval = ApprovalManager::for_non_interactive(&profile(AutonomyLevel::Full));
    let token = CancellationToken::new();
    let mut probe = Probe::new(ROUTING_TOOL);
    probe.wait = true;
    let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(invoke(&probe, &approval, &token, &config), async {
            probe.started.notified().await;
            manager
                .engage(EstopLevel::ToolFreeze(vec![ROUTING_TOOL.into()]))
                .unwrap();
        })
    })
    .await
    .expect("pending classifier observes emergency stop");
    assert!(estop_runtime::is_estop_interrupted(&result.unwrap_err()));
    assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
    assert_eq!(probe.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn routing_judgment_cancellation_prevents_or_interrupts_execution() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let approval = ApprovalManager::for_non_interactive(&profile(AutonomyLevel::Full));
    let token = CancellationToken::new();
    token.cancel();
    let probe = Probe::new(ROUTING_TOOL);
    let error = invoke(&probe, &approval, &token, &config)
        .await
        .unwrap_err();
    assert!(is_tool_loop_cancelled(&error));
    assert_eq!(probe.entered.load(Ordering::SeqCst), 0);

    let token = CancellationToken::new();
    let mut probe = Probe::new(ROUTING_TOOL);
    probe.wait = true;
    let (result, ()) = tokio::join!(invoke(&probe, &approval, &token, &config), async {
        probe.started.notified().await;
        token.cancel();
    });
    assert!(is_tool_loop_cancelled(&result.unwrap_err()));
    assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
    assert_eq!(probe.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn routing_judgment_retains_cooperative_result_and_cancellation_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let approval = ApprovalManager::for_non_interactive(&profile(AutonomyLevel::Full));
    let token = CancellationToken::new();
    let mut probe = Probe::new(ROUTING_TOOL);
    probe.wait = true;
    probe.cooperative = true;
    let (result, ()) = tokio::join!(invoke(&probe, &approval, &token, &config), async {
        probe.started.notified().await;
        token.cancel();
    });
    let error = result.unwrap_err();
    assert!(is_tool_loop_cancelled(&error));
    let evidence = error
        .downcast_ref::<estop_runtime::InterruptedToolResult>()
        .expect("cooperative completion retains terminal cause and result");
    assert_eq!(evidence.completed.output.as_str(), "fixture completed");
    assert_eq!(probe.entered.load(Ordering::SeqCst), 1);
    assert_eq!(probe.dropped.load(Ordering::SeqCst), 1);
}
