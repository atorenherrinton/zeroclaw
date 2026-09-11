//! Process-boundary checks deliberately stop at Rust validation. No Reminders
//! access or deletion is permitted by these fixtures.
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn requests(requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroclaw-reminders-manager"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in requests {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn discovery_exposes_delete_list_with_required_assertion_and_safe_default() {
    let replies = requests(&[
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    ]);
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 9);
    let tool = tools
        .iter()
        .find(|tool| tool["name"] == "delete_list")
        .unwrap();
    assert_eq!(tool["annotations"]["destructiveHint"], true);
    assert_eq!(
        tool["inputSchema"]["required"],
        json!(["list_id", "confirm_name", "owner_authorized"])
    );
    assert_eq!(
        tool["inputSchema"]["properties"]["allow_nonempty"]["default"],
        false
    );
}

#[test]
fn invalid_deletions_return_mcp_errors_before_native_dispatch() {
    let invalid = [
        json!({}),
        json!({"list_id":"fixture","confirm_name":"Fixture"}),
        json!({"list_id":"fixture","confirm_name":"Fixture","owner_authorized":false,"allow_nonempty":true}),
        json!({"list_id":"fixture","confirm_name":"Fixture","owner_authorized":"true"}),
        json!({"list_id":"fixture","confirm_name":"Fixture","owner_authorized":true,"allow_nonempty":"true"}),
        json!({"list_id":"fixture","confirm_name":"Fixture","owner_authorized":true,"allow_nonempty":null}),
        json!({"list_id":["fixture"],"confirm_name":"Fixture","owner_authorized":true}),
        json!({"list_id":"fixture","confirm_name":"Fixture","owner_authorized":true,"force":true}),
        json!({"list_id":"fixture","confirm_name":"","owner_authorized":true}),
    ];
    let calls: Vec<_> = invalid.into_iter().enumerate().map(|(id, args)|
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"delete_list","arguments":args}})
    ).collect();
    let replies = requests(&calls);
    assert_eq!(replies.len(), calls.len());
    for (id, reply) in replies.iter().enumerate() {
        assert_eq!(reply["id"], id);
        assert_eq!(reply["result"]["isError"], true);
        let error = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!error.contains("Reminders operation failed"), "{error}");
        assert!(
            !error.contains("uncertain"),
            "invalid arguments must fail before native dispatch: {error}"
        );
    }
}
