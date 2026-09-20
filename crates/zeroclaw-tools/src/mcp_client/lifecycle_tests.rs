//! Synthetic lifecycle authority plus real MCP dispatch/cleanup boundaries.
//! No production configuration or latch paths are consulted by these fixtures.

use super::*;
use crate::mcp_lifecycle::{is_lifecycle_interrupted, with_mcp_lifecycle_control};
use crate::mcp_protocol::JsonRpcResponse;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use zeroclaw_api::tool::Tool;

#[derive(Debug)]
struct FixtureStopped;
impl std::fmt::Display for FixtureStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("synthetic lifecycle stopped")
    }
}
impl std::error::Error for FixtureStopped {}

#[derive(Default)]
struct Control {
    stopped: AtomicBool,
    changed: tokio::sync::Notify,
}

impl Control {
    fn set_stopped(&self, stopped: bool) {
        self.stopped.store(stopped, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}

#[async_trait::async_trait]
impl McpLifecycleControl for Control {
    fn check(&self) -> Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
            Err(FixtureStopped.into())
        } else {
            Ok(())
        }
    }
    async fn interrupted(&self) -> anyhow::Error {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.stopped.load(Ordering::SeqCst) {
                return FixtureStopped.into();
            }
            changed.await;
        }
    }
}

fn assert_stopped(error: &anyhow::Error) {
    assert!(is_lifecycle_interrupted(error), "{error:#}");
    assert!(error.chain().any(|cause| cause.is::<FixtureStopped>()));
}

#[derive(Default)]
struct Probe {
    after_write: bool,
    prewrite_failure: bool,
    stall_initialize: bool,
    stall_close: bool,
    writes: AtomicUsize,
    resets: AtomicUsize,
    initializes: AtomicUsize,
    initialized: AtomicUsize,
    closes: AtomicUsize,
    entered_initialize: tokio::sync::Notify,
    entered_close: tokio::sync::Notify,
    release_close: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl SharedMcpTransportConn for Probe {
    async fn send_and_recv(
        &self,
        request: &JsonRpcRequest,
        lifecycle: &McpRequestLifecycle,
    ) -> Result<JsonRpcResponse> {
        match request.method.as_str() {
            "initialize" => {
                self.initializes.fetch_add(1, Ordering::SeqCst);
                self.entered_initialize.notify_one();
                if self.stall_initialize {
                    std::future::pending::<()>().await;
                }
            }
            "notifications/initialized" => {
                self.initialized.fetch_add(1, Ordering::SeqCst);
            }
            _ => {
                if self.prewrite_failure {
                    return Err(McpTransportError::TransportClosed.into());
                }
                self.writes.fetch_add(1, Ordering::SeqCst);
                if self.after_write {
                    lifecycle.mark_outcome_unknown(0);
                    std::future::pending::<()>().await;
                }
            }
        }
        Ok(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: request.id.clone(),
            result: Some(json!({"capabilities": {}, "tools": [], "ok": true})),
            error: None,
        })
    }
    async fn reset(&self) -> Result<()> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.stall_close {
            self.entered_close.notify_one();
            self.release_close.notified().await;
        }
        Ok(())
    }
}

fn server(probe: Arc<Probe>) -> McpServer {
    super::tests::server_with_transport("synthetic", probe, 30)
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture boundary did not settle");
}

#[tokio::test]
async fn lifecycle_detached_recovery_keeps_origin_after_scope_drop_and_never_replays() {
    let control = Arc::new(Control::default());
    let probe = Arc::new(Probe {
        after_write: true,
        ..Probe::default()
    });
    let server = server(probe.clone());
    let mut request = Box::pin(with_mcp_lifecycle_control(
        Some(control.clone()),
        server.call_tool("effect", json!({})),
    ));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    assert_eq!(probe.writes.load(Ordering::SeqCst), 1);
    drop(request); // Drop outside the originating task-local scope.
    assert!(server.recovery.recovery_pending());
    control.set_stopped(true); // Detached recovery has not yet been polled.
    wait_until(|| probe.closes.load(Ordering::SeqCst) == 1).await;
    assert!(server.recovery.is_poisoned());
    assert_eq!(probe.resets.load(Ordering::SeqCst), 0);
    assert_eq!(probe.initializes.load(Ordering::SeqCst), 0);
    assert_stopped(&server.call_tool("queued", json!({})).await.unwrap_err());
    assert_stopped(&server.check_replacement().unwrap_err());
    let mut registry = McpRegistry::from_servers(vec![server.clone()]).await;
    assert!(registry.kill_dead_connections().await.is_empty());
    assert_eq!(registry.server_count(), 1);
    control.set_stopped(false);
    server.check_replacement().unwrap();
    // Resume permits an explicit replacement, never repairs/replays this session.
    assert!(
        server
            .call_tool("new-on-old-session", json!({}))
            .await
            .is_err()
    );
    assert_eq!(probe.writes.load(Ordering::SeqCst), 1);
    assert_eq!(registry.kill_dead_connections().await, ["synthetic"]);
}

#[tokio::test]
async fn lifecycle_interruption_during_prewrite_rehandshake_closes_without_request_replay() {
    let control = Arc::new(Control::default());
    let probe = Arc::new(Probe {
        prewrite_failure: true,
        stall_initialize: true,
        ..Probe::default()
    });
    let server = server(probe.clone());
    let (result, ()) = timeout(Duration::from_secs(3), async {
        tokio::join!(
            with_mcp_lifecycle_control(
                Some(control.clone()),
                server.call_tool("effect", json!({}))
            ),
            async {
                probe.entered_initialize.notified().await;
                control.set_stopped(true);
            }
        )
    })
    .await
    .unwrap();
    assert_stopped(&result.unwrap_err());
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
    assert_eq!(probe.resets.load(Ordering::SeqCst), 1);
    assert_eq!(probe.initializes.load(Ordering::SeqCst), 1);
    assert_eq!(probe.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
    assert!(server.recovery.is_poisoned());
}

#[tokio::test]
async fn lifecycle_recovery_observes_later_concurrent_origin_and_retains_it() {
    let first = Arc::new(Control::default());
    let later = Arc::new(Control::default());
    let probe = Arc::new(Probe {
        stall_initialize: true,
        ..Probe::default()
    });
    let server = server(probe.clone());
    let recovery = server.start_recovery(0, "synthetic".into(), Some(first));
    probe.entered_initialize.notified().await;
    later.set_stopped(true);
    server.recovery.record_control(0, Some(later.clone()));
    let result = timeout(Duration::from_secs(3), recovery)
        .await
        .unwrap()
        .unwrap();
    assert_stopped(&result.unwrap_err());
    assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
    assert_stopped(&server.check_replacement().unwrap_err());
    later.set_stopped(false);
    server.check_replacement().unwrap();
}

#[tokio::test]
async fn lifecycle_finalization_retains_unchecked_registration_even_with_same_control() {
    for reuse_control in [false, true] {
        let first = Arc::new(Control::default());
        let later = if reuse_control {
            first.clone()
        } else {
            Arc::new(Control::default())
        };
        let probe = Arc::new(Probe::default());
        let server = server(probe.clone());
        server.recovery.record_control(0, Some(first));
        let checked = server.recovery.checked_controls().unwrap();

        // Deterministic interleaving: another cancelled request registers after
        // the completing recovery's final check, but before its atomic finish.
        later.set_stopped(true);
        server.recovery.record_control(0, Some(later.clone()));
        server.recovery.finish(0, &checked);
        assert!(server.recovery.recovery_pending());
        assert_eq!(server.recovery.controls().len(), 1);
        assert_stopped(&server.recovery.check().unwrap_err());
        assert_stopped(&server.check_replacement().unwrap_err());

        let recovery = server.start_recovery(0, "late registration".into(), None);
        let error = timeout(Duration::from_secs(3), recovery)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_stopped(&error);
        assert_eq!(probe.resets.load(Ordering::SeqCst), 0);
        assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
        later.set_stopped(false);
        server.check_replacement().unwrap();
    }
}

#[test]
fn lifecycle_final_check_does_not_hold_state_lock_during_authority_callback() {
    struct RegisterDuringCheck {
        barrier: std::sync::Weak<RecoveryBarrier>,
        later: Arc<Control>,
        registered: AtomicBool,
    }
    #[async_trait::async_trait]
    impl McpLifecycleControl for RegisterDuringCheck {
        fn check(&self) -> Result<()> {
            if !self.registered.swap(true, Ordering::SeqCst) {
                self.barrier
                    .upgrade()
                    .unwrap()
                    .record_control(0, Some(self.later.clone()));
            }
            Ok(())
        }
        async fn interrupted(&self) -> anyhow::Error {
            std::future::pending().await
        }
    }
    let barrier = Arc::new(RecoveryBarrier::new());
    let later = Arc::new(Control::default());
    later.set_stopped(true);
    barrier.record_control(
        0,
        Some(Arc::new(RegisterDuringCheck {
            barrier: Arc::downgrade(&barrier),
            later,
            registered: AtomicBool::new(false),
        })),
    );
    let checked = barrier.checked_controls().unwrap();
    barrier.finish(0, &checked);
    assert!(barrier.recovery_pending());
    assert_stopped(&barrier.check().unwrap_err());
}

#[tokio::test]
async fn lifecycle_resume_during_pending_cleanup_cannot_replace_owned_transport() {
    let control = Arc::new(Control::default());
    control.set_stopped(true);
    let probe = Arc::new(Probe {
        stall_close: true,
        ..Probe::default()
    });
    let server = server(probe.clone());
    let recovery = server.start_recovery(0, "synthetic".into(), Some(control.clone()));
    timeout(Duration::from_secs(3), probe.entered_close.notified())
        .await
        .unwrap();
    control.set_stopped(false);
    let error = server.check_replacement().unwrap_err();
    assert!(
        !is_lifecycle_interrupted(&error),
        "authority resumed; cleanup is the remaining owner"
    );
    assert!(error.to_string().contains("cleanup"));
    assert_eq!(probe.resets.load(Ordering::SeqCst), 0);
    probe.release_close.notify_one();
    let result = timeout(Duration::from_secs(3), recovery)
        .await
        .unwrap()
        .unwrap();
    assert_stopped(&result.unwrap_err());
    server.check_replacement().unwrap();
    assert_eq!(server.recovery.active_recoveries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lifecycle_fresh_replacement_does_not_resume_cancelled_invocation() {
    struct CancelledInvocation;
    #[async_trait::async_trait]
    impl McpLifecycleControl for CancelledInvocation {
        fn check(&self) -> Result<()> {
            Err(FixtureStopped.into())
        }
        fn check_replacement(&self) -> Result<()> {
            Ok(())
        }
        async fn interrupted(&self) -> anyhow::Error {
            FixtureStopped.into()
        }
    }
    let control: Arc<dyn McpLifecycleControl> = Arc::new(CancelledInvocation);
    let probe = Arc::new(Probe::default());
    let server = server(probe.clone());
    let recovery = server.start_recovery(0, "synthetic".into(), Some(control.clone()));
    let result = timeout(Duration::from_secs(3), recovery)
        .await
        .unwrap()
        .unwrap();
    assert_stopped(&result.unwrap_err());
    server.check_replacement().unwrap();
    assert_stopped(
        &with_mcp_lifecycle_control(Some(control), server.call_tool("old", json!({})))
            .await
            .unwrap_err(),
    );
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
    assert_eq!(probe.resets.load(Ordering::SeqCst), 0);
    assert_eq!(probe.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lifecycle_tool_resource_prompt_wrappers_keep_typed_interruption() {
    let control = Arc::new(Control::default());
    control.set_stopped(true);
    let probe = Arc::new(Probe::default());
    let server = server(probe.clone());
    let definition: McpToolDef =
        serde_json::from_value(json!({"name":"effect","inputSchema":{"type":"object"}})).unwrap();
    {
        let mut inner = server.inner.lock().await;
        inner.tools.push(definition.clone());
        inner.capabilities = McpServerCapabilities {
            resources: true,
            prompts: true,
        };
    }
    let registry = Arc::new(McpRegistry::from_servers(vec![server]).await);
    let wrapper = crate::mcp_tool::McpToolWrapper::new(
        "synthetic__effect".into(),
        definition,
        registry.clone(),
        Arc::new(zeroclaw_config::policy::SecurityPolicy::default()),
    );
    let resources = crate::mcp_resources_tool::McpResourcesTool::new(registry.clone());
    let prompts = crate::mcp_prompts_tool::McpPromptsTool::new(registry);
    with_mcp_lifecycle_control(Some(control), async {
        assert_stopped(&wrapper.execute(json!({})).await.unwrap_err());
        for args in [
            json!({"action":"list"}),
            json!({"action":"list","server":"synthetic"}),
            json!({"action":"read","uri":"synthetic__item"}),
        ] {
            assert_stopped(&resources.execute(args).await.unwrap_err());
        }
        for args in [
            json!({"action":"list"}),
            json!({"action":"list","server":"synthetic"}),
            json!({"action":"get","name":"synthetic__item"}),
        ] {
            assert_stopped(&prompts.execute(args).await.unwrap_err());
        }
    })
    .await;
    assert_eq!(probe.writes.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
fn fixture_command(directory: &std::path::Path, script: &str) -> McpServerConfig {
    McpServerConfig {
        name: "synthetic".into(),
        transport: McpTransport::Stdio,
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "sh".into(),
            directory.display().to_string(),
        ],
        ..McpServerConfig::default()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_denied_constructor_and_registry_never_spawn() {
    let directory = tempfile::tempdir().unwrap();
    let config = fixture_command(directory.path(), "printf started > \"$1/started\"");
    let control = Arc::new(Control::default());
    control.set_stopped(true);
    with_mcp_lifecycle_control(Some(control), async {
        assert_stopped(&McpServer::connect(config.clone()).await.err().unwrap());
        assert_stopped(&McpRegistry::connect_all(&[config]).await.err().unwrap());
    })
    .await;
    assert!(!directory.path().join("started").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_real_stdio_reaps_old_child_before_denying_replacement_spawn() {
    let directory = tempfile::tempdir().unwrap();
    let config = fixture_command(
        directory.path(),
        "printf 'started\\n' >> \"$1/starts\"; while IFS= read -r line; do :; done; printf cleanup > \"$1/cleanup\"; while [ ! -f \"$1/release\" ]; do sleep 0.01; done; printf reaped > \"$1/finished\"",
    );
    let transport = crate::mcp_transport::StdioTransport::new(&config).unwrap();
    wait_until(|| directory.path().join("starts").exists()).await;
    let control = Arc::new(Control::default());
    let (result, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(
            SharedMcpTransportConn::reset_with_control(&transport, Some(control.as_ref())),
            async {
                wait_until(|| directory.path().join("cleanup").exists()).await;
                control.set_stopped(true);
                std::fs::write(directory.path().join("release"), b"release").unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_stopped(&result.unwrap_err());
    assert!(directory.path().join("finished").exists());
    assert!(!SharedMcpTransportConn::health_check(&transport));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("starts")).unwrap(),
        "started\n"
    );
    SharedMcpTransportConn::close(&transport).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_cancelled_initial_handshake_finishes_owned_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let config = fixture_command(
        directory.path(),
        "printf started > \"$1/started\"; while IFS= read -r line; do :; done; printf closed > \"$1/closed\"",
    );
    let control = Arc::new(Control::default());
    let (result, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(
            with_mcp_lifecycle_control(Some(control.clone()), McpServer::connect(config)),
            async {
                wait_until(|| directory.path().join("started").exists()).await;
                control.set_stopped(true);
            }
        )
    })
    .await
    .unwrap();
    assert_stopped(&result.err().unwrap());
    assert!(directory.path().join("closed").exists());
}
