use super::*;
use crate::digest;
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn group() -> GroupTarget {
    GroupTarget {
        chat_id: 42,
        chat_identifier: "fixture-chat".into(),
        chat_guid: "any;+;fixture-chat".into(),
        service: "iMessage".into(),
        name: "".into(),
        participants: vec![
            "first@example.invalid".into(),
            "second@example.invalid".into(),
        ],
    }
}
fn args(key: &str, at: i64) -> Value {
    json!({"idempotency_key":key,"group_token":group().token().unwrap(),"text":"Exact “review” — no rewrite.","send_at":DateTime::from_timestamp_millis(at).unwrap().fixed_offset().to_rfc3339()})
}
fn auth(status: &Value) -> Value {
    json!({"operation_id":status["operation_id"],"review_hash":status["review_hash"],"review":status["review"],"owner_requested_send":true})
}
fn resolve(token: &str) -> Result<GroupTarget> {
    ensure!(token == group().token()?, "fixture token mismatch");
    Ok(group())
}
fn prepare(ops: &Ops, key: &str, at: i64) -> Result<Value> {
    ops.group_text_prepare_using(&args(key, at), at - 60_000, resolve)
}
// Move a synthetic fixture's clock-bound intent as a unit. Production has no
// edit API; mutation tests below deliberately do NOT update its review hash.
fn retime_fixture(ops: &Ops, status: &Value, at: i64) -> Result<Value> {
    let mut review = status["review"].clone();
    review["send_at_ms"] = json!(at);
    review["steps"][0]["arguments"]["send_at"] = args("unused", at)["send_at"].clone();
    let raw = serde_json::to_string(&review)?;
    ops.db.execute(
        "UPDATE operations SET payload=?2,request_hash=?3,send_at_ms=?4 WHERE id=?1",
        rusqlite::params![
            status["operation_id"].as_str().unwrap(),
            raw,
            digest(raw.as_bytes()),
            at
        ],
    )?;
    ops.operation_status(status["operation_id"].as_str().unwrap())
}
fn authorized_due(ops: &Ops, key: &str) -> Result<(Value, i64)> {
    let future = Utc::now().timestamp_millis() + 60_000;
    let status = prepare(ops, key, future)?;
    ops.group_text_schedule(&auth(&status))?;
    let at = Utc::now().timestamp_millis() - 100;
    Ok((retime_fixture(ops, &status, at)?, at))
}
async fn mock_dispatch(
    ops: &Ops,
    key: &str,
    at: i64,
    counter: Arc<AtomicUsize>,
    uncertain: bool,
) -> Result<Value> {
    ops.operation_execute_using(key, |step, _, reconcile| {
        let counter = counter.clone();
        execute_using(
            step,
            reconcile,
            move || at,
            resolve,
            move |item, path| async move {
                assert!(path.is_none());
                assert!(item.attachment.is_none());
                assert_eq!(item.group.as_ref().unwrap().chat_guid, group().chat_guid);
                assert_eq!(
                    item.group.as_ref().unwrap().participants,
                    group().participants
                );
                assert_eq!(item.recipient, group().token().unwrap());
                assert_eq!(item.text, "Exact “review” — no rewrite.");
                counter.fetch_add(1, Ordering::SeqCst);
                if uncertain {
                    imessage::SendOutcome::Uncertain("fixture lost receipt".into())
                } else {
                    imessage::SendOutcome::Submitted(json!({"fixture":true}))
                }
            },
        )
    })
    .await
}

#[test]
fn prepare_is_idempotent_exact_and_group_only() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let at = Utc::now().timestamp_millis() + 60_000;
    let mut request = args("same", at);
    request["send_at"] = json!(
        DateTime::from_timestamp_millis(at)
            .unwrap()
            .with_timezone(&chrono::FixedOffset::west_opt(7 * 3600).unwrap())
            .to_rfc3339()
    );
    let now = Utc::now().timestamp_millis();
    let first = ops.group_text_prepare_using(&request, now, resolve)?;
    assert_eq!(
        first["review"]["steps"][0]["arguments"]["send_at"],
        request["send_at"]
    );
    assert_eq!(
        first["send_at_ms"],
        json!(instant(request["send_at"].as_str().unwrap())?)
    );
    assert_eq!(first, ops.group_text_prepare_using(&request, now, resolve)?);
    for field in ["text", "send_at", "group_token"] {
        let mut changed = request.clone();
        changed[field] = json!("changed");
        assert!(
            ops.group_text_prepare_using(&changed, now, resolve)
                .is_err()
        );
    }
    for field in [
        "recipients",
        "chat_id",
        "chat_guid",
        "participants",
        "paths",
        "owner_requested_send",
    ] {
        let mut changed = request.clone();
        changed[field] = json!("forbidden");
        assert!(
            ops.group_text_prepare_using(&changed, now, resolve)
                .is_err()
        );
    }
    assert_eq!(
        ops.db
            .query_row("SELECT COUNT(*) FROM operations", [], |r| r
                .get::<_, i64>(0))?,
        1
    );
    assert!(
        ops.group_text_prepare_using(&args("past", now), now, resolve)
            .is_err()
    );
    assert!(
        ops.group_text_prepare_using(&args("drift", at), at - 10, |_| {
            let mut g = group();
            g.participants.pop();
            Ok(g)
        })
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn future_persists_no_early_send_auth_required_and_cancel_before_claim() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let at = Utc::now().timestamp_millis() + 60_000;
    let status = prepare(&ops, "future", at)?;
    let mut approval = auth(&status);
    approval
        .as_object_mut()
        .unwrap()
        .remove("owner_requested_send");
    assert!(ops.group_text_schedule(&approval).is_err());
    approval["owner_requested_send"] = json!(false);
    assert!(ops.group_text_schedule(&approval).is_err());
    for field in ["review", "review_hash"] {
        let mut changed = auth(&status);
        changed[field] = json!("mutated");
        assert!(ops.group_text_schedule(&changed).is_err());
    }
    assert_eq!(
        ops.group_text_schedule(&auth(&status))?["state"],
        "scheduled"
    );
    assert_eq!(
        ops.group_text_schedule(&auth(&status))?["state"],
        "scheduled"
    );
    let counter = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        mock_dispatch(&ops, "future", at, counter.clone(), false).await?["state"],
        "scheduled"
    );
    drop(ops);
    let reopened = Ops::open(tmp.path())?;
    assert_eq!(reopened.operation_status("future")?["state"], "scheduled");
    assert_eq!(
        reopened.operation_status("future")?["review"],
        status["review"]
    );
    assert!(reopened.next_dispatch_delay()?.as_millis() <= 15_000);
    assert_eq!(reopened.operation_cancel("future")?["state"], "cancelled");
    assert_eq!(reopened.operation_cancel("future")?["state"], "cancelled");
    assert!(
        mock_dispatch(&reopened, "future", at, counter.clone(), false)
            .await
            .is_err()
    );
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    // Preparation can never be used as a late authorization route.
    let past = prepare(
        &reopened,
        "too-late",
        Utc::now().timestamp_millis() + 60_000,
    )?;
    let past = retime_fixture(&reopened, &past, Utc::now().timestamp_millis() - 1)?;
    assert!(reopened.group_text_schedule(&auth(&past)).is_err());
    Ok(())
}

#[tokio::test]
async fn exact_group_success_duplicate_and_uncertain_never_replay() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    for uncertain in [false, true] {
        let key = if uncertain { "uncertain" } else { "success" };
        let (_, at) = authorized_due(&ops, key)?;
        let counter = Arc::new(AtomicUsize::new(0));
        let first = mock_dispatch(&ops, key, at, counter.clone(), uncertain).await?;
        assert_eq!(
            first["state"],
            if uncertain { "uncertain" } else { "submitted" }
        );
        mock_dispatch(&ops, key, at, counter.clone(), uncertain).await?;
        let reopened = Ops::open(tmp.path())?;
        mock_dispatch(&reopened, key, at, counter.clone(), uncertain).await?;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(reopened.operation_cancel(key).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn dispatch_rejects_identity_drift_early_and_missed_deadline_without_fallback() -> Result<()>
{
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let (status, at) = authorized_due(&ops, "drift")?;
    let step: Step = serde_json::from_value(status["review"]["steps"][0].clone())?;
    for field in [
        "chat_id",
        "chat_identifier",
        "chat_guid",
        "service",
        "participants",
    ] {
        let step = step.clone();
        let mut value = serde_json::to_value(group())?;
        value[field] = match field {
            "chat_id" => json!(43),
            "participants" => json!(["third@example.invalid", "second@example.invalid"]),
            _ => json!("changed"),
        };
        let current: GroupTarget = serde_json::from_value(value)?;
        let result = execute_using(
            step,
            false,
            || at,
            |_| Ok(current),
            |_, _| async { panic!("no fallback or send on drift") },
        )
        .await?;
        assert_eq!(result.state, "failed");
        assert_eq!(result.evidence["write_attempted"], false);
    }
    for now in [at - 1, at + DISPATCH_BUDGET_MS + 1] {
        let result = execute_using(
            step.clone(),
            false,
            || now,
            resolve,
            |_, _| async { panic!("no early or late send") },
        )
        .await?;
        assert_eq!(result.state, "failed");
    }
    let result = execute_using(
        step,
        true,
        || at,
        |_| panic!("uncertain never resolves or retries"),
        |_, _| async { panic!("uncertain never sends") },
    )
    .await?;
    assert_eq!(result.state, "uncertain");
    Ok(())
}

#[tokio::test]
async fn mutated_payload_hash_group_text_time_and_due_column_are_rejected() -> Result<()> {
    for field in ["text", "group_token", "send_at", "hash", "due_column"] {
        let tmp = tempfile::tempdir()?;
        let ops = Ops::open(tmp.path())?;
        let (status, at) = authorized_due(&ops, "mutation")?;
        if field == "hash" {
            ops.db
                .execute("UPDATE operations SET request_hash='changed'", [])?;
        } else if field == "due_column" {
            ops.db.execute("UPDATE operations SET send_at_ms=0", [])?;
        } else {
            let mut review = status["review"].clone();
            review["steps"][0]["arguments"][field] = json!("changed");
            ops.db.execute(
                "UPDATE operations SET payload=?1",
                [serde_json::to_string(&review)?],
            )?;
        }
        let counter = Arc::new(AtomicUsize::new(0));
        assert!(
            mock_dispatch(&ops, "mutation", at, counter.clone(), false)
                .await
                .is_err()
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
    // Even a rehashed altered due column cannot disagree with the reviewed RFC3339.
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let (status, _) = authorized_due(&ops, "due")?;
    let mut review = status["review"].clone();
    review["send_at_ms"] = json!(0);
    let raw = serde_json::to_string(&review)?;
    ops.db.execute(
        "UPDATE operations SET payload=?1,request_hash=?2,send_at_ms=0",
        rusqlite::params![raw, digest(raw.as_bytes())],
    )?;
    assert!(ops.operation_status("due").is_err());
    Ok(())
}

#[test]
fn concurrent_claims_across_connections_attempt_once() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let (_, at) = authorized_due(&ops, "race")?;
    let barrier = Arc::new(Barrier::new(2));
    let counter = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let root = tmp.path().to_owned();
        let barrier = barrier.clone();
        let counter = counter.clone();
        workers.push(std::thread::spawn(move || -> Result<()> {
            let ops = Ops::open(&root)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            barrier.wait();
            runtime.block_on(mock_dispatch(&ops, "race", at, counter, false))?;
            Ok(())
        }));
    }
    for worker in workers {
        worker.join().unwrap()?;
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn registration_uses_dedicated_closed_schema() {
    let schema = crate::operations_api::schema();
    for name in [
        "imessage_group_text_prepare",
        "imessage_group_text_schedule",
    ] {
        let tool = schema.iter().find(|v| v["name"] == name).unwrap();
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
    let transaction = schema
        .iter()
        .find(|v| v["name"] == "transaction_prepare")
        .unwrap();
    assert!(!transaction.to_string().contains(TOOL));
}

fn immediate(ops: &Ops, key: &str) -> Result<Value> {
    let mut a = args(key, 0);
    a.as_object_mut().unwrap().remove("send_at");
    ops.immediate_group_text_prepare_using(&a, resolve)
}

#[test]
fn immediate_prepare_exact_idempotent_no_schedule_or_recipient_fallback() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let first = immediate(&ops, "immediate")?;
    assert_eq!(first, immediate(&ops, "immediate")?);
    assert!(first["send_at_ms"].is_null());
    assert!(first["review"]["steps"][0]["arguments"]["send_at"].is_null());
    assert_eq!(
        first["review"]["steps"][0]["arguments"]["text"],
        "Exact “review” — no rewrite."
    );
    assert!(ops.group_text_schedule(&auth(&first)).is_err());
    let mut a = args("new", 0);
    a.as_object_mut().unwrap().remove("send_at");
    for field in ["recipients", "chat_id", "send_at", "owner_requested_send"] {
        let mut bad = a.clone();
        bad[field] = json!("not allowed");
        assert!(
            ops.immediate_group_text_prepare_using(&bad, resolve)
                .is_err()
        );
    }
    let mut changed = group();
    changed.participants.push("third@example.invalid".into());
    assert!(
        ops.immediate_group_text_prepare_using(&a, |_| Ok(changed))
            .is_err()
    );
    a["idempotency_key"] = json!("immediate");
    a["text"] = json!("changed");
    assert!(ops.immediate_group_text_prepare_using(&a, resolve).is_err());
    Ok(())
}

#[tokio::test]
async fn immediate_authorization_review_cancel_and_uncertain_no_replay() -> Result<()> {
    for uncertain in [false, true] {
        let tmp = tempfile::tempdir()?;
        let ops = Ops::open(tmp.path())?;
        let status = immediate(&ops, "now")?;
        let counter = Arc::new(AtomicUsize::new(0));
        assert!(
            mock_dispatch(&ops, "now", 0, counter.clone(), uncertain)
                .await
                .is_err()
        );
        let mut bad = auth(&status);
        bad["owner_requested_send"] = json!(false);
        assert!(ops.operation_authorize(&bad).is_err());
        for field in ["text", "group_token", "group", "send_at"] {
            let mut bad = auth(&status);
            bad["review"]["steps"][0]["arguments"][field] = json!("changed");
            assert!(ops.operation_authorize(&bad).is_err());
        }
        ops.operation_authorize(&auth(&status))?;
        let result = mock_dispatch(&ops, "now", 0, counter.clone(), uncertain).await?;
        assert_eq!(
            result["steps"][0]["state"],
            if uncertain { "uncertain" } else { "submitted" }
        );
        mock_dispatch(&ops, "now", i64::MAX, counter.clone(), uncertain).await?;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(ops.operation_cancel("now").is_err());
        let cancelled = immediate(&ops, "cancel")?;
        ops.operation_cancel("cancel")?;
        assert!(ops.operation_authorize(&auth(&cancelled)).is_err());
        assert_eq!(ops.operation_status("cancel")?["state"], "cancelled");
    }
    Ok(())
}

#[tokio::test]
async fn immediate_dispatch_rejects_every_identity_drift() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let status = immediate(&ops, "drift-now")?;
    let step: Step = serde_json::from_value(status["review"]["steps"][0].clone())?;
    for field in [
        "chat_id",
        "chat_identifier",
        "chat_guid",
        "service",
        "participants",
    ] {
        let mut value = serde_json::to_value(group())?;
        value[field] = match field {
            "chat_id" => json!(43),
            "participants" => json!(["third@example.invalid"]),
            _ => json!("changed"),
        };
        let current: GroupTarget = serde_json::from_value(value)?;
        let result = execute_using(
            step.clone(),
            false,
            || 0,
            |_| Ok(current),
            |_, _| async { panic!("no fallback or group creation") },
        )
        .await?;
        assert_eq!(result.state, "failed");
        assert_eq!(result.evidence["write_attempted"], false);
    }
    Ok(())
}

#[test]
fn immediate_concurrent_execute_has_one_external_attempt() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let ops = Ops::open(tmp.path())?;
    let status = immediate(&ops, "race-now")?;
    ops.operation_authorize(&auth(&status))?;
    let count = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let path = tmp.path().to_path_buf();
            let count = count.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || -> Result<()> {
                let ops = Ops::open(&path)?;
                barrier.wait();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(mock_dispatch(&ops, "race-now", 0, count, false))?;
                Ok(())
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap()?;
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn immediate_schema_is_group_only_and_uses_existing_owner_authorized_send() {
    let schemas = crate::schema();
    let schemas = schemas.as_array().unwrap();
    let schema = schemas
        .iter()
        .find(|s| s["name"] == "group_text_prepare")
        .unwrap();
    assert_eq!(schema["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        schema["inputSchema"]["required"],
        json!(["idempotency_key", "group_token", "text"])
    );
    assert!(
        schema["inputSchema"]["properties"]
            .get("recipients")
            .is_none()
    );
    assert!(schema["inputSchema"]["properties"].get("send_at").is_none());
    let send = schemas.iter().find(|s| s["name"] == "outbox_send").unwrap();
    assert!(
        send["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("owner_requested_send"))
    );
}
