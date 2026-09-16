//! Active setup cancellation through the public Agent constructor. Every path
//! belongs to a private TempDir; the only child is an interpreted /bin/sh fixture.

use super::Agent;
use crate::security::estop::{EstopLevel, EstopManager, ResumeSelector};
use crate::security::estop_runtime;
use std::path::Path;
use std::time::Duration;
use zeroclaw_config::multi_agent::{AgentMemoryConfig, MemoryBackendKind};
use zeroclaw_config::schema::{
    AliasedAgentConfig, Config, McpBundleConfig, McpServerConfig, McpTransport, RiskProfileConfig,
};

const AGENT: &str = "constructor_fixture";
const SERVER: &str = "constructor_mcp";

// No external shell commands, descendants, FIFO writers, or executable script
// installation. The first initialize blocks on stdin without responding. After
// explicit resume a new helper sees the private marker and completes the protocol.
const MCP_SCRIPT: &str = r#"
printf '%s\n' "$$" >> "$1"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "$$" > "$2"
      if [ -f "$3" ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"constructor-fixture","version":"1"}}}'
      fi
      ;;
    *'"method":"tools/list"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"probe","description":"Synthetic constructor fixture","inputSchema":{"type":"object"}}]}}'
      ;;
  esac
done
"#;

fn fixture_config(root: &Path, provider_uri: String) -> Config {
    let mut config = Config {
        config_path: root.join("config.toml"),
        data_dir: root.join("data"),
        ..Config::default()
    };
    config.security.estop.enabled = true;
    config.security.estop.state_file = "constructor-estop.json".into();
    config.security.estop.require_otp_to_resume = false;
    config.memory.backend = "none".into();
    config.memory.auto_save = false;
    config.memory.audit_enabled = false;
    config.memory.response_cache_enabled = false;
    config.skills.open_skills_enabled = false;
    config.skills.allow_scripts = false;
    config.hooks.enabled = false;
    config.cost.enabled = false;
    let provider = config
        .providers
        .models
        .ensure("custom", "constructor_fixture")
        .expect("custom provider schema slot");
    provider.api_key = Some("synthetic-constructor-key".into());
    provider.model = Some("synthetic-constructor-model".into());
    provider.uri = Some(provider_uri);
    config
        .risk_profiles
        .insert("constructor_fixture".into(), RiskProfileConfig::default());
    config.agents.insert(
        AGENT.into(),
        AliasedAgentConfig {
            model_provider: "custom.constructor_fixture".into(),
            risk_profile: "constructor_fixture".into(),
            mcp_bundles: vec!["constructor_fixture".into()],
            memory: AgentMemoryConfig {
                backend: MemoryBackendKind::None,
            },
            ..AliasedAgentConfig::default()
        },
    );
    config.mcp.enabled = true;
    config.mcp.deferred_loading = false;
    config.mcp.servers = vec![McpServerConfig {
        name: SERVER.into(),
        transport: McpTransport::Stdio,
        command: "/bin/sh".into(),
        args: vec![
            root.join("constructor-mcp.sh").display().to_string(),
            root.join("helper-generations").display().to_string(),
            root.join("initialize-ready").display().to_string(),
            root.join("resume-setup").display().to_string(),
        ],
        ..McpServerConfig::default()
    }];
    config.mcp_bundles.insert(
        "constructor_fixture".into(),
        McpBundleConfig {
            servers: vec![SERVER.into()],
            ..McpBundleConfig::default()
        },
    );
    assert!(config.install_root_dir().starts_with(root));
    assert!(config.agent_workspace_dir(AGENT).starts_with(root));
    assert_eq!(config.mcp_servers_for_agent(AGENT).len(), 1);
    config
}

async fn initialize_ready(path: &Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
            {
                assert!(pid > 1 && pid != std::process::id() as libc::pid_t);
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual constructor helper must receive initialize")
}

fn process_present(pid: libc::pid_t) -> bool {
    // Signal zero only probes the fixture PID; no process is signalled here.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => false,
        Some(libc::EPERM) => true,
        error => panic!("unexpected fixture PID probe error: {error:?}"),
    }
}

async fn wait_reaped(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_present(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("constructor-owned direct helper must be reaped");
}

fn generations(path: &Path) -> Vec<libc::pid_t> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

#[tokio::test]
async fn estop_agent_active_constructor_cancels_owned_mcp_and_resumes_new_setup() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("constructor-mcp.sh"), MCP_SCRIPT).unwrap();
    // Keep the local provider endpoint bound without accepting connections.
    // Any provider contact remains observable in its backlog, including setup.
    let provider = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    provider.set_nonblocking(true).unwrap();
    let config = fixture_config(root, format!("http://{}", provider.local_addr().unwrap()));
    let mut manager = EstopManager::load(&config.security.estop, root).unwrap();
    let mut constructor = Box::pin(Agent::from_config(&config, AGENT));
    let ready_path = root.join("initialize-ready");
    let first_pid = tokio::select! {
        biased;
        _ = &mut constructor => panic!("withheld initialize cannot complete construction"),
        pid = initialize_ready(&ready_path) => pid,
    };
    assert!(process_present(first_pid));
    manager.engage(EstopLevel::KillAll).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), &mut constructor)
        .await
        .expect("active setup must observe the isolated latch")
        .err()
        .expect("active construction must return a terminal stop");
    drop(constructor);
    assert!(estop_runtime::is_estop_interrupted(&error));
    assert!(crate::agent::loop_::is_tool_loop_cancelled(&error));
    wait_reaped(first_pid).await;
    assert_eq!(
        generations(&root.join("helper-generations")),
        vec![first_pid]
    );
    assert!(
        matches!(provider.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );

    // No automatic replay: a fresh constructor is still denied while latched.
    let denied = Agent::from_config(&config, AGENT)
        .await
        .err()
        .expect("latched constructor must stay denied");
    assert!(estop_runtime::is_estop_interrupted(&denied));
    assert_eq!(
        generations(&root.join("helper-generations")),
        vec![first_pid]
    );
    std::fs::write(root.join("resume-setup"), "allow new setup").unwrap();
    manager.resume(ResumeSelector::KillAll, None, None).unwrap();
    let agent = tokio::time::timeout(Duration::from_secs(5), Agent::from_config(&config, AGENT))
        .await
        .expect("fresh resumed constructor must finish")
        .expect("fresh resumed constructor must connect");
    assert!(agent.tool_names().contains(&"constructor_mcp__probe"));
    let pids = generations(&root.join("helper-generations"));
    assert_eq!(
        pids.len(),
        2,
        "one helper per explicitly admitted constructor"
    );
    assert_eq!(pids[0], first_pid);
    assert!(process_present(pids[1]));
    assert!(
        matches!(provider.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    drop(agent);
    wait_reaped(pids[1]).await;
}
