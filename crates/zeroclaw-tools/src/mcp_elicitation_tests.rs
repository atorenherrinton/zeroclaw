//! Boundary tests use a real stdio server, including equal IDs in opposite
//! directions. They exercise the client handshake, dispatch, and cancellation.
use super::*;
use crate::mcp_protocol::{
    McpElicitationHandler, McpElicitationRequest, McpElicitationResult,
    with_mcp_elicitation_handler,
};
use std::sync::atomic::AtomicUsize;

const SERVER: &str = r#"
import json, sys
caps = {}
def send(value):
    print(json.dumps(value), flush=True)
def result(id, value):
    send({'jsonrpc':'2.0', 'id':id, 'result':value})
for line in sys.stdin:
    req = json.loads(line)
    method = req.get('method')
    if method == 'initialize':
        caps = req['params']['capabilities']
        result(req['id'], {'protocolVersion':'2025-11-25', 'capabilities':{}})
    elif method == 'tools/list':
        result(req['id'], {'tools':[{'name':'interact','inputSchema':{'type':'object'}}]})
    elif method == 'tools/call':
        args = req['params']['arguments']
        if args.get('inspect'):
            result(req['id'], {'caps':caps})
            continue
        if args.get('log'):
            with open(args['log'], 'a') as f: f.write('call\n')
        server_id = args.get('server_id', req['id'])
        params = {'message':'Allow a read?', 'requestedSchema':{'type':'object','properties':{}}, '_meta':{'fixture':True}}
        params.update(args.get('params', {}))
        send({'jsonrpc':'2.0', 'id':server_id, 'method':args.get('method','elicitation/create'), 'params':params})
        if args.get('duplicate'):
            send({'jsonrpc':'2.0', 'id':server_id, 'method':'elicitation/create', 'params':params})
        send({'jsonrpc':'2.0', 'method':'notifications/progress', 'params':{}})
        if args.get('cancel'):
            send({'jsonrpc':'2.0','method':'notifications/cancelled','params':{'requestId':server_id}})
        if args.get('finish_early'):
            result(req['id'], {'early':True})
            continue
        reply_line = sys.stdin.readline()
        if not reply_line: break
        reply = json.loads(reply_line)
        assert reply['id'] == server_id and type(reply['id']) == type(server_id), reply
        assert 'method' not in reply, reply
        result(req['id'], {'reply':reply, 'caps':caps})
"#;

fn config(timeout_secs: u64) -> McpServerConfig {
    McpServerConfig {
        name: "elicitation-fixture".into(),
        command: "python3".into(),
        args: vec!["-u".into(), "-c".into(), SERVER.into()],
        transport: McpTransport::Stdio,
        tool_timeout_secs: Some(timeout_secs),
        ..Default::default()
    }
}

struct DecisionHandler {
    decision: McpElicitationResult,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl McpElicitationHandler for DecisionHandler {
    async fn elicit(&self, request: McpElicitationRequest) -> Result<McpElicitationResult> {
        assert_eq!(request.server_name, "elicitation-fixture");
        assert_eq!(request.tool_name, "interact");
        assert!(request.connection_epoch > 0);
        assert!(request.originating_request_id.is_u64());
        assert_eq!(request.meta, Some(json!({"fixture":true})));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.decision.clone())
    }
}

fn handler(decision: McpElicitationResult) -> (Arc<dyn McpElicitationHandler>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(DecisionHandler {
            decision,
            calls: calls.clone(),
        }),
        calls,
    )
}

#[tokio::test]
async fn stdio_elicitation_relays_accept_decline_cancel_and_keeps_id_namespaces() {
    for decision in [
        McpElicitationResult::Accept(json!({})),
        McpElicitationResult::Decline,
        McpElicitationResult::Cancel,
    ] {
        for args in [json!({}), json!({"server_id":"3"}), json!({"server_id":-7})] {
            let server = McpServer::connect_with_form_elicitation(config(5))
                .await
                .unwrap();
            let (handler, calls) = handler(decision.clone());
            let response =
                with_mcp_elicitation_handler(handler, server.call_tool("interact", args))
                    .await
                    .unwrap();
            assert_eq!(
                response["reply"]["result"],
                decision.clone().into_value().unwrap()
            );
            assert_eq!(response["caps"]["elicitation"], json!({"form":{}}));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(response["reply"]["result"].get("_meta").is_none());
            server.transport.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn stdio_elicitation_requires_active_handler_and_default_capability_is_closed() {
    let server = McpServer::connect(config(5)).await.unwrap();
    let (handler, calls) = handler(McpElicitationResult::Accept(json!({})));
    let response = with_mcp_elicitation_handler(handler, server.call_tool("interact", json!({})))
        .await
        .unwrap();
    assert!(response["caps"].get("elicitation").is_none());
    assert_eq!(response["reply"]["error"]["code"], json!(-32601));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.transport.close().await.unwrap();

    let server = McpServer::connect_with_form_elicitation(config(5))
        .await
        .unwrap();
    let response = server.call_tool("interact", json!({})).await.unwrap();
    assert_eq!(response["reply"]["error"]["code"], json!(-32601));
    server.transport.close().await.unwrap();
}

#[tokio::test]
async fn stdio_elicitation_rejects_unknown_methods_modes_and_invalid_forms() {
    let server = McpServer::connect_with_form_elicitation(config(5))
        .await
        .unwrap();
    let (handler, calls) = handler(McpElicitationResult::Accept(json!({})));
    for args in [
        json!({"method":"sampling/createMessage"}),
        json!({"params":{"mode":"url","url":"https://example.invalid"}}),
        json!({"params":{"requestedSchema":{"type":"string"}}}),
        json!({"params":{"_meta":"malformed"}}),
    ] {
        let response =
            with_mcp_elicitation_handler(handler.clone(), server.call_tool("interact", args))
                .await
                .unwrap();
        assert!(response["reply"].get("error").is_some());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.transport.close().await.unwrap();
}

struct PendingHandler {
    entered: Arc<tokio::sync::Notify>,
    dropped: Arc<AtomicUsize>,
}
struct DropSignal(Arc<AtomicUsize>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl McpElicitationHandler for PendingHandler {
    async fn elicit(&self, _: McpElicitationRequest) -> Result<McpElicitationResult> {
        let _guard = DropSignal(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn stdio_server_cancellation_cancels_the_interaction() {
    let server = McpServer::connect_with_form_elicitation(config(5))
        .await
        .unwrap();
    let handler = Arc::new(PendingHandler {
        entered: Arc::new(tokio::sync::Notify::new()),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let response = with_mcp_elicitation_handler(
        handler,
        server.call_tool("interact", json!({"cancel":true})),
    )
    .await
    .unwrap();
    assert_eq!(response["reply"]["result"], json!({"action":"cancel"}));
    server.transport.close().await.unwrap();
}

#[tokio::test]
async fn stdio_timeout_and_cancellation_drop_interaction_without_replaying_mutation() {
    for abort in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("calls.log");
        let server = McpServer::connect_with_form_elicitation(config(if abort { 5 } else { 1 }))
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let handler: Arc<dyn McpElicitationHandler> = Arc::new(PendingHandler {
            entered: entered.clone(),
            dropped: dropped.clone(),
        });
        let task_server = server.clone();
        let args = json!({"log":log});
        let task = zeroclaw_spawn::spawn!(async move {
            with_mcp_elicitation_handler(handler, task_server.call_tool("interact", args)).await
        });
        timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        if abort {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            let error = task.await.unwrap().unwrap_err();
            assert!(error.to_string().contains("not replayed"));
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let response = server
            .call_tool("interact", json!({"inspect":true}))
            .await
            .unwrap();
        assert_eq!(response["caps"]["elicitation"], json!({"form":{}}));
        assert_eq!(std::fs::read_to_string(log).unwrap(), "call\n");
        server.transport.close().await.unwrap();
    }
}

#[tokio::test]
async fn stdio_parallel_callers_keep_their_own_user_handlers() {
    let server = McpServer::connect_with_form_elicitation(config(5))
        .await
        .unwrap();
    let (accept, accepts) = handler(McpElicitationResult::Accept(json!({"choice":"first"})));
    let (decline, declines) = handler(McpElicitationResult::Decline);
    let (one, two) = tokio::join!(
        with_mcp_elicitation_handler(accept, server.call_tool("interact", json!({}))),
        with_mcp_elicitation_handler(decline, server.call_tool("interact", json!({}))),
    );
    assert_eq!(
        one.unwrap()["reply"]["result"]["content"],
        json!({"choice":"first"})
    );
    assert_eq!(two.unwrap()["reply"]["result"], json!({"action":"decline"}));
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    assert_eq!(declines.load(Ordering::SeqCst), 1);
    // Completing a scoped future never leaves that handler on the shared server.
    let without_scope = server.call_tool("interact", json!({})).await.unwrap();
    assert!(without_scope["reply"].get("error").is_some());
    server.transport.close().await.unwrap();
}

#[tokio::test]
async fn stdio_final_parent_response_ends_any_nested_interaction() {
    let server = McpServer::connect_with_form_elicitation(config(5))
        .await
        .unwrap();
    let (handler, _) = handler(McpElicitationResult::Accept(json!({})));
    let result = with_mcp_elicitation_handler(
        handler,
        server.call_tool("interact", json!({"finish_early":true})),
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"early":true}));
    // The fixture expects the next line to be a request, so any late acceptance
    // would make it fail or hang this second call.
    let result = server
        .call_tool("interact", json!({"inspect":true}))
        .await
        .unwrap();
    assert!(result.get("caps").is_some());
    server.transport.close().await.unwrap();
}

#[tokio::test]
async fn stdio_duplicate_server_id_cannot_receive_later_acceptance() {
    let server = McpServer::connect_with_form_elicitation(config(5))
        .await
        .unwrap();
    let handler = Arc::new(PendingHandler {
        entered: Arc::new(tokio::sync::Notify::new()),
        dropped: Arc::new(AtomicUsize::new(0)),
    });
    let response = with_mcp_elicitation_handler(
        handler,
        server.call_tool("interact", json!({"duplicate":true})),
    )
    .await
    .unwrap();
    assert_eq!(response["reply"]["error"]["code"], json!(-32602));
    server.transport.close().await.unwrap();
}
