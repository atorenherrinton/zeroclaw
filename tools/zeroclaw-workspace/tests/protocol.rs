//! The shipped process boundary; synthetic fixtures only, no credentials/network.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};
fn run(args: &[&str], input: &[u8]) -> Result<std::process::Output> {
    let root = tempfile::tempdir()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroclaw-workspace"))
        .args(args)
        .env("ZEROCLAW_CONFIG_DIR", root.path().canonicalize()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    match writer.join().map_err(|_| anyhow::Error::msg("writer"))? {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => return Err(e.into()),
    }
    assert_eq!(
        std::fs::read_dir(root.path())?.count(),
        0,
        "protocol-only calls must not create state"
    );
    Ok(out)
}
#[test]
fn schema_and_mcp_discovery_are_identical() -> Result<()> {
    let schema = run(&["schema"], &[])?;
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout)?;
    let out = run(
        &["mcp"],
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n",
    )?;
    assert!(out.status.success());
    let reply: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(schema["tools"], reply["result"]["tools"]);
    assert_eq!(schema["tools"].as_array().context("tools")?.len(), 2);
    Ok(())
}
#[test]
fn notifications_never_dispatch_and_unknown_calls_fail_closed() -> Result<()> {
    let lines=[json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"workspace_apply","arguments":{}}}),json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"workspace_authorize","arguments":{"owner_requested":true}}}),json!({"jsonrpc":"2.0","id":3,"method":"ping"})].map(|v|v.to_string()).join("\n")+"\n";
    let out = run(&["mcp"], lines.as_bytes())?;
    assert!(out.status.success());
    let replies = std::str::from_utf8(&out.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["result"]["isError"], true);
    assert_eq!(replies[1]["id"], 3);
    Ok(())
}
#[test]
fn model_boolean_cannot_authorize_and_untrusted_fields_are_denied() -> Result<()> {
    let args = json!({"operation_id":"op1","request":{"action":"docs_create","title":"Fixture"},"owner_requested":true});
    let input=json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"workspace_apply","arguments":args}}).to_string()+"\n";
    let out = run(&["mcp"], input.as_bytes())?;
    let reply: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(reply["result"]["isError"], true);
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .context("text")?
            .contains("invalid exact granted operation")
    );
    Ok(())
}
#[test]
fn authorization_has_no_terminal_or_pipe_issuer() -> Result<()> {
    for args in [
        vec!["authorize"],
        vec!["authorize", "/does/not/exist"],
        vec!["authorize", "--interactive"],
    ] {
        let out = run(&args, b"op1\n")?;
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    }
    Ok(())
}
#[test]
fn interactive_doctor_is_unavailable_over_pipes() -> Result<()> {
    let out = run(&["doctor", "--interactive"], b"")?;
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("owner-operated terminal"));
    Ok(())
}
#[test]
fn oversized_input_and_malformed_json_are_bounded() -> Result<()> {
    let out = run(&["mcp"], &vec![b'x'; 512 * 1024 + 1])?;
    assert!(!out.status.success());
    assert!(out.stdout.len() < 1024 && out.stderr.len() < 4096);
    let out = run(
        &["mcp"],
        b"{broken\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
    )?;
    assert!(out.status.success());
    assert_eq!(std::str::from_utf8(&out.stdout)?.lines().count(), 2);
    Ok(())
}

#[test]
fn removed_mutations_and_semantically_invalid_reads_fail_before_auth() -> Result<()> {
    let mut calls = Vec::new();
    for name in [
        "workspace_apply",
        "workspace_status",
        "workspace_reconcile",
        "workspace_authorize",
    ] {
        calls.push(json!({"jsonrpc":"2.0","id":calls.len(),"method":"tools/call","params":{"name":name,"arguments":{"operation_id":"synthetic","request":{"action":"docs_append","document_id":"doc1","tab_id":"t.0","required_revision_id":"r1","text":"hello"}}}}));
    }
    for args in [
        json!({"action":"docs_read","document_id":"../permissions"}),
        json!({"action":"docs_verify","document_id":"doc1","tab_id":"t.0","expected_text_sha256":"bad"}),
        json!({"action":"docs_read","document_id":"doc1","owner_requested":true}),
        json!({"action":"docs_read","document_id":"doc1","approved":true}),
    ] {
        calls.push(json!({"jsonrpc":"2.0","id":calls.len(),"method":"tools/call","params":{"name":"workspace_read","arguments":args}}));
    }
    let input = calls
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let out = run(&["mcp"], input.as_bytes())?;
    assert!(out.status.success());
    let replies = std::str::from_utf8(&out.stdout)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(replies.len(), calls.len());
    for reply in replies {
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .context("error text")?;
        assert!(!text.contains("configuration") && !text.contains("Keychain"));
    }
    Ok(())
}

#[test]
fn terminal_issuer_rejects_pipe_before_reading_file_or_credentials() -> Result<()> {
    let out = run(
        &["authorize", "fixture", "Synthetic", "/does/not/exist"],
        b"approval\n",
    )?;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("owner-operated terminal"));
    Ok(())
}
#[test]
fn missing_grant_is_denied_before_keychain_or_network() -> Result<()> {
    let input=json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"workspace_apply","arguments":{"operation_id":"ungranted"}}}).to_string()+"\n";
    let out = run(&["mcp"], input.as_bytes())?;
    let reply: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(reply["result"]["isError"], true);
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("owner terminal grant required")
    );
    Ok(())
}
