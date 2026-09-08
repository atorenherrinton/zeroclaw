//! Exercise the shipped MCP process without contacting Google.
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn installed_protocol_exposes_reply_parameters_and_rejects_conflicting_targets() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroclaw-google-write"))
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"}),
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"gmail_create_draft", "arguments":{"to":"recipient@example.com", "body":"Draft", "reply_to_message_id":"message-1", "thread_id":"thread-1"}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    let draft = responses[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "gmail_create_draft")
        .unwrap();
    for field in ["reply_to_message_id", "thread_id"] {
        assert_eq!(draft["inputSchema"]["properties"][field]["type"], "string");
    }
    assert_eq!(draft["inputSchema"]["required"], json!(["to", "body"]));
    assert_eq!(draft["annotations"]["idempotentHint"], false);
    assert_eq!(responses[1]["result"]["isError"], true);
    assert!(
        responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Use only one")
    );
}
