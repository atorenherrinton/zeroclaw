//! Process-boundary regression tests with synthetic private ledgers only.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};
use zeroclaw_personal_ops::Ops;

fn request(root: &std::path::Path, method: &str, params: Value) -> Result<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zeroclaw-personal-ops"))
        .arg("mcp")
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("stdin")?;
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
    )?;
    drop(stdin);
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout)?;
    Ok(response["result"].clone())
}

fn mcp(root: &std::path::Path, name: &str, args: Value) -> Result<Value> {
    let response = request(root, "tools/call", json!({"name":name,"arguments":args}))?;
    // Model the MCP adapter's pretty output, appended receipt, native content
    // string, and serialized ChatMessage. Never measure just raw snapshot bytes.
    let rendered = format!(
        "{}\n{}",
        serde_json::to_string_pretty(&response)?,
        "r".repeat(128)
    );
    let history = json!({"role":"tool","content":json!({"tool_call_id":"c".repeat(128),"content":rendered}).to_string()});
    assert!(serde_json::to_vec(&history)?.len() <= 12 * 1024);
    Ok(response)
}

fn value(result: &Value) -> Result<Value> {
    assert_eq!(result["isError"], false, "{result}");
    Ok(serde_json::from_str(
        result["content"][0]["text"].as_str().context("MCP text")?,
    )?)
}

#[test]
fn large_snapshots_stay_bounded_with_freshness_and_no_side_effects() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let text = "\\\"\n\u{0001}日🦀".repeat(10000);
    for source in [
        "calendar_today",
        "important_email",
        "pending_invitations",
        "overdue_reminders",
        "scheduled_jobs",
        "github",
    ] {
        ops.snapshot(source, &json!({"items":(0..100).map(|i|json!({"id":i,"title":text})).collect::<Vec<_>>(),"truncated":true}),None)?;
    }
    ops.snapshot("important_email", &json!({}), Some(&text))?;
    ops.health_record("google", "temporary_outage", &text)?;
    let before: String = ops.db.query_row(
        "SELECT payload FROM source_snapshots WHERE source='important_email'",
        [],
        |r| r.get(0),
    )?;
    let before_changes = ops.db.total_changes();
    for name in ["operations_activity", "personal_briefing"] {
        let result = value(&mcp(tmp.path(), name, json!({}))?)?;
        assert!(result["summary_only"].as_bool().context("summary")?);
        assert_eq!(result["sections"]["sources"]["total"], 6);
        let sources = result["sections"]["sources"]["items"]
            .as_array()
            .context("sources")?;
        let failed = sources
            .iter()
            .find(|s| s["source"] == "important_email")
            .context("failed source remains visible")?;
        assert_eq!(failed["available"], true);
        assert!(!failed["error"].is_null());
        assert_eq!(failed["source_truncated"], true);
        assert!(
            result["sections"]["health"]["items"]
                .as_array()
                .context("health")?
                .iter()
                .any(|h| h["state"] == "temporary_outage")
        );
    }
    assert_eq!(before_changes, ops.db.total_changes());
    assert_eq!(
        before,
        ops.db.query_row::<String, _, _>(
            "SELECT payload FROM source_snapshots WHERE source='important_email'",
            [],
            |r| r.get(0)
        )?
    );
    let counts: i64 = ops.db.query_row("SELECT (SELECT count(*) FROM operations)+(SELECT count(*) FROM event_inbox)+(SELECT count(*) FROM alert_receipts)",[],|r|r.get(0))?;
    assert_eq!(counts, 0);
    Ok(())
}

#[test]
fn source_pages_reconstruct_unicode_and_refuse_changed_revisions() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let text = "\n\"\\日🦀\u{0001}".repeat(600);
    ops.snapshot(
        "fixture",
        &json!({"a/b~c":text,"rows":(0..61).collect::<Vec<_>>()}),
        None,
    )?;
    let mut offset = 0;
    let mut revision = Value::Null;
    let mut recovered = String::new();
    loop {
        let mut args = json!({"source":"fixture","pointer":"/a~1b~0c","offset":offset});
        if !revision.is_null() {
            args["revision"] = revision.clone();
        }
        let page = value(&mcp(tmp.path(), "operations_activity", args)?)?;
        recovered.push_str(page["text"].as_str().context("chunk")?);
        revision = page["revision"].clone();
        if page["next_offset"].is_null() {
            break;
        }
        let next = page["next_offset"].as_u64().context("cursor")?;
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(recovered, text);
    let page = value(&mcp(
        tmp.path(),
        "operations_activity",
        json!({"source":"fixture","pointer":"/rows","limit":50}),
    )?)?;
    assert_eq!(page["total"], 61);
    assert!(page["next_offset"].as_u64().context("next")? > 0);
    assert_eq!(
        mcp(
            tmp.path(),
            "operations_activity",
            json!({"source":"fixture","pointer":"/rows","offset":1})
        )?["isError"],
        true
    );
    ops.snapshot("fixture", &json!({"rows":[1]}), None)?;
    assert_eq!(
        mcp(
            tmp.path(),
            "operations_activity",
            json!({"source":"fixture","pointer":"/rows","revision":revision,"offset":1})
        )?["isError"],
        true
    );
    Ok(())
}

#[test]
fn section_pages_reach_all_records_and_errors_remain_errors() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    for i in 0..230 {
        ops.receipt(
            &format!("fixture-{i}"),
            None,
            "fixture",
            &json!({"note":"\\\"\n🦀".repeat(5000)}),
        )?;
    }
    let mut offset = 0;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = value(&mcp(
            tmp.path(),
            "operations_activity",
            json!({"section":"receipts","offset":offset,"limit":50}),
        )?)?;
        let section = &page["sections"]["receipts"];
        assert_eq!(section["total"], 230);
        for row in section["items"].as_array().context("items")? {
            assert!(seen.insert(row["sequence"].as_u64().context("sequence")?));
        }
        if section["next_offset"].is_null() {
            break;
        }
        offset = section["next_offset"].as_u64().context("next")?;
    }
    assert_eq!(seen.len(), 230);
    for args in [
        json!({"section":"unknown"}),
        json!({"limit":0}),
        json!({"limit":51}),
        json!({"offset":-1}),
        json!({"source":"missing"}),
        json!({"pointer":"/items"}),
        json!({"source":"x","section":"sources"}),
    ] {
        assert_eq!(
            mcp(tmp.path(), "operations_activity", args)?["isError"],
            true
        );
    }
    Ok(())
}

#[test]
fn briefing_filters_exceptions_and_exact_reviews_are_unchanged() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let exact = "exact text\\\"\n日".repeat(400);
    let prepared = ops.operation_prepare(&json!({"idempotency_key":"fixture","title":"fixture","steps":[{"tool":"outbox_email","arguments":{"text":exact}}]}))?;
    ops.health_record("healthy", "healthy", "verified")?;
    ops.health_record("failed", "temporary_outage", "unavailable")?;
    let brief = value(&mcp(tmp.path(), "personal_briefing", json!({}))?)?;
    assert_eq!(brief["sections"]["operations"]["total"], 0);
    assert_eq!(brief["sections"]["health"]["total"], 1);
    ops.db.execute(
        "UPDATE operation_steps SET state='uncertain' WHERE operation_id='fixture'",
        [],
    )?;
    let brief = value(&mcp(
        tmp.path(),
        "personal_briefing",
        json!({"section":"operations"}),
    )?)?;
    assert_eq!(brief["sections"]["operations"]["total"], 1);
    assert_eq!(
        brief["sections"]["operations"]["items"][0]["state"],
        "uncertain"
    );
    assert!(
        brief["sections"]["operations"]["items"][0]
            .get("review")
            .is_none()
    );
    assert_eq!(
        ops.operation_status("fixture")?["review"],
        prepared["review"]
    );
    // Invalid refresh requests fail before contacting a connector or writing a snapshot.
    assert_eq!(
        mcp(
            tmp.path(),
            "personal_briefing",
            json!({"refresh":true,"limit":0})
        )?["isError"],
        true
    );
    assert_eq!(
        ops.db
            .query_row::<i64, _, _>("SELECT count(*) FROM source_snapshots", [], |r| r.get(0))?,
        0
    );
    Ok(())
}

#[test]
fn escaped_evidence_keys_do_not_strand_pages_and_ids_remain_exact() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let id = "p".repeat(128);
    let evidence: serde_json::Map<String, Value> = (0..8)
        .map(|i| {
            (
                format!("{}{}", "\\\"".repeat(60), i),
                json!("\\\"\n日🦀".repeat(100)),
            )
        })
        .collect();
    ops.receipt(&id, None, "fixture", &Value::Object(evidence))?;
    let page = value(&mcp(
        tmp.path(),
        "operations_activity",
        json!({"section":"receipts","limit":1}),
    )?)?;
    assert_eq!(
        page["sections"]["receipts"]["items"]
            .as_array()
            .context("items")?
            .len(),
        1
    );
    assert_eq!(page["sections"]["receipts"]["items"][0]["id"], id);
    let project = json!({"status":"active","next_action":"next"});
    ops.db.execute(
        "INSERT INTO projects(id,revision,payload,updated_ms) VALUES(?1,1,?2,1)",
        rusqlite::params![id, project.to_string()],
    )?;
    let page = value(&mcp(
        tmp.path(),
        "operations_activity",
        json!({"section":"projects","limit":1}),
    )?)?;
    assert_eq!(page["sections"]["projects"]["items"][0]["project_id"], id);
    Ok(())
}

#[test]
fn scheduled_and_terminal_operations_keep_exact_identity_and_state() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let send_at = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
    let text = "exact body\\\"\n日🦀".repeat(4000);
    let mut expected = std::collections::HashMap::new();
    for state in ["scheduled", "submitted", "uncertain", "failed", "cancelled"] {
        let id = format!("{state}{}", "\\\"".repeat(50));
        let prepared = ops.operation_prepare(&json!({
            "idempotency_key":id,"title":text,"send_at":send_at,
            "steps":[{"tool":"outbox_email","arguments":{"subject":"fixture","text":text}}]
        }))?;
        ops.db.execute(
            "UPDATE operations SET authorized_ms=1, cancelled=?1 WHERE id=?2",
            rusqlite::params![state == "cancelled", id],
        )?;
        if matches!(state, "submitted" | "uncertain" | "failed") {
            ops.db.execute(
                "UPDATE operation_steps SET state=?1 WHERE operation_id=?2",
                rusqlite::params![state, id],
            )?;
        }
        let status = ops.operation_status(&id)?;
        assert_eq!(status["review"], prepared["review"]);
        expected.insert(id, status);
    }
    let mut offset = 0;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = value(&mcp(
            tmp.path(),
            "operations_activity",
            json!({"section":"operations","offset":offset,"limit":50}),
        )?)?;
        let section = &page["sections"]["operations"];
        assert_eq!(section["total"], expected.len());
        for row in section["items"].as_array().context("items")? {
            let id = row["operation_id"].as_str().context("operation id")?;
            assert!(seen.insert(id.to_owned()));
            let exact = expected.get(id).context("exact operation identity")?;
            assert_eq!(row["state"], exact["state"]);
            assert_eq!(row["send_at_ms"], exact["send_at_ms"]);
            assert_eq!(row["authorized_ms"], exact["authorized_ms"]);
            assert_eq!(row["summary_only"], true);
            assert!(row.get("review").is_none());
            assert_eq!(row["details"]["operation_id"], id);
            assert_eq!(row["details"]["exact_review_required_before_send"], true);
            assert_eq!(ops.operation_status(id)?, *exact);
        }
        if section["next_offset"].is_null() {
            break;
        }
        let next = section["next_offset"].as_u64().context("next")?;
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(seen.len(), expected.len());
    Ok(())
}

#[test]
fn tool_discovery_exposes_activity_pagination_and_source_details() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let tools = request(tmp.path(), "tools/list", json!({}))?;
    let tools = tools["tools"].as_array().context("tools")?;
    for name in ["operations_activity", "personal_briefing"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .context("activity tool")?;
        let schema = &tool["inputSchema"];
        let properties = schema["properties"].as_object().context("properties")?;
        for field in [
            "section", "offset", "limit", "source", "pointer", "revision",
        ] {
            assert!(properties.contains_key(field), "{name}.{field}");
        }
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(properties["limit"]["minimum"], 1);
        assert_eq!(properties["limit"]["maximum"], 50);
        assert_eq!(
            properties.contains_key("refresh"),
            name == "personal_briefing"
        );
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
    }
    Ok(())
}
