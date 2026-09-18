//! Hermetic checks at the shipped stdio process boundary; no Google credentials.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{ErrorKind, Write},
    process::{Command, Output, Stdio},
};

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
}

fn lines(messages: &[Value]) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut input, message)?;
        input.push(b'\n');
    }
    Ok(input)
}

fn run(args: &[&str], input: Vec<u8>) -> Result<Output> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroclaw-gmail"))
        .args(args)
        .env("ZEROCLAW_CONFIG_DIR", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("child stdin unavailable")?;
    // Drain output concurrently with writing input so an oversized request or
    // an early CLI rejection cannot deadlock on a full pipe.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    match writer
        .join()
        .map_err(|_| anyhow::Error::msg("stdin writer panicked"))?
    {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::BrokenPipe => {}
        Err(error) => return Err(error.into()),
    }
    assert_eq!(
        std::fs::read_dir(&root)?.count(),
        0,
        "these protocol-only requests must not create an operation ledger or configuration"
    );
    Ok(output)
}

fn replies(output: &Output) -> Result<Vec<Value>> {
    assert!(
        output.status.success(),
        "MCP failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::str::from_utf8(&output.stdout)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[test]
fn initialize_lists_exact_draft_only_tools() -> Result<()> {
    let output = run(
        &["mcp"],
        lines(&[
            request(
                json!(1),
                "initialize",
                json!({
                    "protocolVersion":"2025-06-18",
                    "capabilities":{},
                    "clientInfo":{"name":"protocol-fixture","version":"1"}
                }),
            ),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            request(json!("list"), "tools/list", json!({})),
            request(json!(3), "ping", json!({})),
        ])?,
    )?;
    let rows = replies(&output)?;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[1]["id"], "list");
    assert_eq!(rows[2]["id"], 3);
    assert!(rows.iter().all(|row| row["jsonrpc"] == "2.0"));
    assert_eq!(rows[0]["result"]["protocolVersion"], "2025-06-18");
    assert!(rows[0]["result"]["capabilities"]["tools"].is_object());
    assert_eq!(rows[2]["result"], json!({}));

    let definitions = rows[1]["result"]["tools"]
        .as_array()
        .context("tools/list must expose an array")?;
    let names = definitions
        .iter()
        .map(|tool| tool["name"].as_str().context("tool name required"))
        .collect::<Result<BTreeSet<_>>>()?;
    assert_eq!(definitions.len(), names.len(), "duplicate tool names");
    assert_eq!(
        names,
        BTreeSet::from([
            "gmail_prepare_draft",
            "gmail_apply_draft",
            "gmail_list_drafts",
            "gmail_read_draft",
            "gmail_discard_draft",
            "gmail_operation_status",
            "gmail_reconcile_draft",
            "gmail_prepare_native_schedule",
            "gmail_begin_native_schedule_handoff",
            "gmail_cancel_native_schedule_handoff",
            "gmail_reconcile_native_schedule",
        ])
    );
    for tool in definitions {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let properties = schema["properties"]
            .as_object()
            .context("schema properties required")?;
        for required in schema["required"]
            .as_array()
            .context("required array missing")?
        {
            assert!(properties.contains_key(required.as_str().context("required field name")?));
        }
    }
    Ok(())
}

#[test]
fn notifications_never_dispatch_tools_or_create_local_state() -> Result<()> {
    let output = run(
        &["mcp"],
        lines(&[
            json!({"jsonrpc":"2.0","method":"tools/call","params":{
                "name":"gmail_apply_draft", "arguments":{
                    "operation_id":"fixture", "review_id":"a".repeat(64), "owner_requested":true
                }
            }}),
            json!({"jsonrpc":"2.0","method":"tools/call","params":{
                "name":"gmail_discard_draft", "arguments":{
                    "operation_id":"fixture", "draft_id":"draft123",
                    "expected_raw_sha256":"a".repeat(64), "owner_requested":true
                }
            }}),
            json!({"jsonrpc":"2.0","method":"tools/call","params":{
                "name":"gmail_operation_status", "arguments":{"operation_id":"fixture"}
            }}),
            request(json!("alive"), "ping", json!({})),
        ])?,
    )?;
    assert_eq!(
        replies(&output)?,
        vec![json!({"jsonrpc":"2.0","id":"alive","result":{}})]
    );
    Ok(())
}

#[test]
fn malformed_invalid_and_unknown_requests_recover_with_correct_ids() -> Result<()> {
    let mut input = b"{broken-json\n".to_vec();
    input.extend(lines(&[
        json!({"jsonrpc":"2.0","id":[],"method":"ping"}),
        json!({"jsonrpc":"1.0","id":"bad-version","method":"ping"}),
        request(json!("unknown"), "nonexistent/method", json!({})),
        request(json!(41), "tools/call", json!({})),
        request(Value::Null, "ping", json!({})),
        request(json!(42), "ping", json!({})),
    ])?);
    let output = run(&["mcp"], input)?;
    let rows = replies(&output)?;
    assert_eq!(rows.len(), 7);
    for (index, code) in [(0, -32700), (1, -32600), (2, -32600)] {
        assert_eq!(rows[index]["id"], Value::Null);
        assert_eq!(rows[index]["error"]["code"], code);
    }
    assert_eq!(rows[3]["id"], "unknown");
    assert_eq!(rows[3]["error"]["code"], -32601);
    assert_eq!(rows[4]["id"], 41);
    assert_eq!(rows[4]["error"]["code"], -32602);
    assert_eq!(rows[5], json!({"jsonrpc":"2.0","id":null,"result":{}}));
    assert_eq!(rows[6], json!({"jsonrpc":"2.0","id":42,"result":{}}));
    Ok(())
}

#[test]
fn unknown_send_tool_is_denied_before_configuration_or_credentials() -> Result<()> {
    let output = run(
        &["mcp"],
        lines(&[
            request(
                json!("send"),
                "tools/call",
                json!({"name":"gmail_send_draft","arguments":{}}),
            ),
            request(
                json!("unknown"),
                "tools/call",
                json!({"name":"nonexistent_tool","arguments":{}}),
            ),
        ])?,
    )?;
    let rows = replies(&output)?;
    assert_eq!(rows.len(), 2);
    for (row, id) in rows.iter().zip(["send", "unknown"]) {
        assert_eq!(row["id"], id);
        assert_eq!(row["result"]["isError"], true);
        assert_eq!(row["result"]["content"][0]["type"], "text");
        let error = row["result"]["content"][0]["text"]
            .as_str()
            .context("tool error text required")?;
        assert!(error.to_ascii_lowercase().contains("unknown"));
        assert!(!error.to_ascii_lowercase().contains("configuration"));
    }
    Ok(())
}

#[test]
fn oversized_input_is_rejected_with_bounded_output() -> Result<()> {
    let mut input = vec![b'x'; 512 * 1024 + 1];
    input.push(b'\n');
    let output = run(&["mcp"], input)?;
    assert!(!output.status.success());
    assert!(output.stdout.len() < 4096);
    assert!(output.stderr.len() < 4096);
    assert!(String::from_utf8_lossy(&output.stderr).contains("bound"));
    Ok(())
}

#[test]
fn invalid_cli_arguments_fail_without_side_effects() -> Result<()> {
    for args in [
        vec![],
        vec!["send"],
        vec!["mcp", "extra"],
        vec!["schema", "extra"],
    ] {
        let output = run(&args, vec![])?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("usage:"));
    }
    Ok(())
}

#[test]
fn interactive_doctor_requires_owner_terminal_before_configuration_or_credentials() -> Result<()> {
    let output = run(&["doctor", "--interactive"], vec![])?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("interactive doctor requires an owner-operated terminal"));
    assert!(
        !error
            .to_ascii_lowercase()
            .contains("configuration unavailable")
    );
    assert!(!error.to_ascii_lowercase().contains("keychain"));
    Ok(())
}

#[test]
fn mutations_require_owner_request_before_configuration_or_credentials() -> Result<()> {
    for name in [
        "gmail_apply_draft",
        "gmail_discard_draft",
        "gmail_begin_native_schedule_handoff",
        "gmail_cancel_native_schedule_handoff",
    ] {
        for args in [
            json!({}),
            json!({"owner_requested":false}),
            json!({"owner_requested":"true"}),
        ] {
            let output = run(
                &["mcp"],
                lines(&[request(
                    json!(1),
                    "tools/call",
                    json!({"name":name,"arguments":args}),
                )])?,
            )?;
            let rows = replies(&output)?;
            assert!(rows[0]["result"]["isError"] == true);
            let error = rows[0]["result"]["content"][0]["text"]
                .as_str()
                .context("tool error text missing")?;
            assert!(error.contains("owner's explicit request required"));
            assert!(!error.contains("configuration unavailable"));
        }
    }
    Ok(())
}
