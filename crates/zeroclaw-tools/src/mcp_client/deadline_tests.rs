use super::*;
use crate::mcp_protocol::JsonRpcResponse;
use std::sync::atomic::AtomicUsize;
use zeroclaw_api::deadline::PARENT;
use zeroclaw_api::tool::Tool;

#[derive(Clone, Copy, PartialEq)]
enum Stall {
    None,
    BeforeWrite,
    AfterWrite,
    Reset,
    Initialize,
    Initialized,
    InitializedFailure,
    Close,
}

struct ProbeTransport {
    stall: Stall,
    writes: AtomicUsize,
    resets: AtomicUsize,
    closes: AtomicUsize,
}

impl ProbeTransport {
    fn new(stall: Stall) -> Arc<Self> {
        Arc::new(Self {
            stall,
            writes: AtomicUsize::new(0),
            resets: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for ProbeTransport {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<JsonRpcResponse> {
        let method = request.method.as_str();
        if method == "notifications/initialized" && self.stall == Stall::InitializedFailure {
            return Err(McpTransportError::TransportClosed.into());
        }
        if (method == "initialize" && self.stall == Stall::Initialize)
            || (method == "notifications/initialized" && self.stall == Stall::Initialized)
        {
            std::future::pending::<()>().await;
        }
        if method == "tools/call"
            || method.starts_with("resources/")
            || method.starts_with("prompts/")
        {
            if self.stall == Stall::BeforeWrite {
                std::future::pending::<()>().await;
            }
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.stall == Stall::AfterWrite {
                lifecycle.mark_outcome_unknown(0);
                std::future::pending::<()>().await;
            }
        }
        Ok(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: request.id.clone(),
            result: Some(json!({"capabilities": {"resources": {}, "prompts": {}}, "ok": true})),
            error: None,
        })
    }
    async fn reset(&self) -> Result<()> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        if self.stall == Stall::Reset {
            std::future::pending::<()>().await;
        }
        if self.stall == Stall::Close {
            return Err(McpTransportError::TransportClosed.into());
        }
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.stall == Stall::Close {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn server(probe: Arc<ProbeTransport>) -> McpServer {
    super::tests::server_with_transport("synthetic", probe, 600)
}

fn assert_deadline(error: anyhow::Error, started: bool) {
    let deadline = error
        .downcast_ref::<DeadlineExceeded>()
        .expect("typed parent deadline");
    assert_eq!(deadline.phase, Phase::Tool);
    assert_eq!(deadline.started, started);
}

async fn limited<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    PARENT
        .scope(Some(Instant::now() + Duration::from_millis(20)), future)
        .await
}

#[tokio::test]
async fn expired_parent_prevents_process_creation_and_empty_registry_success() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    // A portable invalid command still proves the deadline is checked before
    // transport construction; on Unix additionally make a real spawn observable.
    let config = McpServerConfig {
        name: "expired".into(),
        #[cfg(unix)]
        command: "sh".into(),
        #[cfg(unix)]
        args: vec![
            "-c".into(),
            "printf started > \"$1\"".into(),
            "sh".into(),
            marker.display().to_string(),
        ],
        #[cfg(not(unix))]
        command: "nonexistent-synthetic-command".into(),
        ..Default::default()
    };
    let error = PARENT
        .scope(Some(Instant::now()), McpServer::connect(config.clone()))
        .await
        .err()
        .unwrap();
    assert_deadline(error, false);
    let error = PARENT
        .scope(Some(Instant::now()), McpRegistry::connect_all(&[config]))
        .await
        .err()
        .unwrap();
    assert_deadline(error, false);
    assert!(!marker.exists());
    let registry = McpRegistry::connect_all(&[]).await.unwrap();
    assert_deadline(
        PARENT
            .scope(Some(Instant::now()), registry.list_all_resources())
            .await
            .unwrap_err(),
        false,
    );
    assert_deadline(
        PARENT
            .scope(Some(Instant::now()), registry.list_all_prompts())
            .await
            .unwrap_err(),
        false,
    );
}

#[tokio::test]
async fn parent_bounds_config_lock_and_prewrite_waits_without_reset_or_write() {
    let probe = ProbeTransport::new(Stall::BeforeWrite);
    let mut server = server(probe.clone());
    let lock = server.inner.lock().await;
    assert_deadline(
        limited(server.call_tool("effect", json!({})))
            .await
            .unwrap_err(),
        true,
    );
    drop(lock);
    let gate = Arc::new(Mutex::new(()));
    server.serial_gate = Some(gate.clone());
    let lock = gate.lock().await;
    assert_deadline(
        limited(server.call_tool("effect", json!({})))
            .await
            .unwrap_err(),
        true,
    );
    drop(lock);
    server.recovery.arm(0);
    assert_deadline(
        limited(server.call_tool("effect", json!({})))
            .await
            .unwrap_err(),
        true,
    );
    assert!(!server.recovery.is_poisoned());
    server.recovery.finish(0);
    assert_deadline(
        limited(server.call_tool("effect", json!({})))
            .await
            .unwrap_err(),
        true,
    );
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
    assert_eq!(probe.resets.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tool_wrapper_preserves_postwrite_deadline_and_detached_recovery_never_replays() {
    let probe = ProbeTransport::new(Stall::AfterWrite);
    let server = server(probe.clone());
    let definition: McpToolDef =
        serde_json::from_value(json!({"name":"effect","inputSchema":{"type":"object"}})).unwrap();
    server.inner.lock().await.tools.push(definition.clone());
    let registry = Arc::new(McpRegistry::from_servers(vec![server.clone()]).await);
    let wrapper = crate::mcp_tool::McpToolWrapper::new(
        "synthetic__effect".into(),
        definition,
        registry,
        Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
    );
    assert_deadline(limited(wrapper.execute(json!({}))).await.unwrap_err(), true);
    timeout(Duration::from_secs(2), async {
        while probe.resets.load(Ordering::SeqCst) == 0 || server.recovery.recovery_pending() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(probe.writes.load(Ordering::SeqCst), 1);
    assert_eq!(probe.resets.load(Ordering::SeqCst), 1);
    assert!(!server.recovery.is_poisoned());
}

#[tokio::test]
async fn resource_and_prompt_tools_preserve_deadlines_for_direct_and_aggregate_queries() {
    let probe = ProbeTransport::new(Stall::BeforeWrite);
    let server = server(probe.clone());
    server.inner.lock().await.capabilities = McpServerCapabilities {
        resources: true,
        prompts: true,
    };
    let registry = Arc::new(McpRegistry::from_servers(vec![server]).await);
    let resources = crate::mcp_resources_tool::McpResourcesTool::new(registry.clone());
    let prompts = crate::mcp_prompts_tool::McpPromptsTool::new(registry);
    for args in [
        json!({"action":"list"}),
        json!({"action":"list","server":"synthetic"}),
        json!({"action":"read","uri":"synthetic__test"}),
    ] {
        assert_deadline(limited(resources.execute(args)).await.unwrap_err(), true);
    }
    for args in [
        json!({"action":"list"}),
        json!({"action":"list","server":"synthetic"}),
        json!({"action":"get","name":"synthetic__test"}),
    ] {
        assert_deadline(limited(prompts.execute(args)).await.unwrap_err(), true);
    }
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stalled_recovery_and_cleanup_are_bounded_and_poison_future_writes() {
    for stage in [
        Stall::Reset,
        Stall::Initialize,
        Stall::Initialized,
        Stall::Close,
    ] {
        let probe = ProbeTransport::new(stage);
        let server = server(probe.clone());
        server.recovery.arm(0);
        timeout(
            Duration::from_secs(2),
            server.recover_with_budget(0, Duration::from_millis(20), Duration::from_millis(20)),
        )
        .await
        .expect("recovery and cleanup must finish within their combined budget")
        .unwrap_err();
        assert!(server.recovery.is_poisoned());
        assert!(probe.closes.load(Ordering::SeqCst) >= 1);
        assert!(server.call_tool("must-not-write", json!({})).await.is_err());
        assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn dropped_recovery_poison_guard_does_not_leave_permanent_waiters() {
    let probe = ProbeTransport::new(Stall::Reset);
    let server = server(probe.clone());
    server.recovery.arm(0);
    timeout(
        Duration::from_millis(20),
        server.recover_with_budget(0, RECOVERY_BUDGET, RECOVERY_CLEANUP_BUDGET),
    )
    .await
    .unwrap_err();
    assert!(server.recovery.is_poisoned());
    assert!(server.call_tool("must-not-write", json!({})).await.is_err());
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn successful_recovery_keeps_existing_connection_usable() {
    let probe = ProbeTransport::new(Stall::None);
    let server = server(probe.clone());
    server.recovery.arm(0);
    server
        .recover_with_budget(0, Duration::from_secs(2), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!server.recovery.is_poisoned());
    assert!(!server.recovery.recovery_pending());
    assert_eq!(
        server.call_tool("probe", json!({})).await.unwrap()["ok"],
        true
    );
    assert_eq!(probe.resets.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_initialized_notification_cannot_mark_recovery_ready() {
    let probe = ProbeTransport::new(Stall::InitializedFailure);
    assert!(handshake(probe.as_ref(), "synthetic", 0).await.is_err());
    let server = server(probe.clone());
    server.recovery.arm(0);
    assert!(
        server
            .recover_with_budget(0, Duration::from_secs(2), Duration::from_secs(2))
            .await
            .is_err()
    );
    assert!(server.recovery.is_poisoned());
    assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
    assert!(server.call_tool("must-not-write", json!({})).await.is_err());
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn admission_futures_do_not_embed_the_nested_handshake_stack() {
    let connection = McpServer::connect(McpServerConfig::default());
    let registry = McpRegistry::connect_all(&[]);
    let connection_bytes = std::mem::size_of_val(&connection);
    let registry_bytes = std::mem::size_of_val(&registry);
    eprintln!("connection future: {connection_bytes}; registry future: {registry_bytes}");
    // Agent/delegate assembly embeds these public futures even when MCP is
    // disabled. Deadline wrappers must not copy the nested handshake stack
    // through every caller (previously 3,016 and 6,624 bytes on aarch64).
    assert!(
        connection_bytes <= 2048,
        "connection future: {connection_bytes}"
    );
    assert!(registry_bytes <= 512, "registry future: {registry_bytes}");
}
