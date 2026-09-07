use super::*;

fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE chat(guid TEXT,chat_identifier TEXT,display_name TEXT,service_name TEXT);
        CREATE TABLE message(guid TEXT,date INTEGER,is_from_me INTEGER,handle_id INTEGER,text TEXT,attributedBody BLOB);
        CREATE TABLE handle(id TEXT);
        CREATE TABLE chat_message_join(chat_id INTEGER,message_id INTEGER);
        CREATE TABLE attachment(guid TEXT,mime_type TEXT,total_bytes INTEGER,filename TEXT);
        CREATE TABLE message_attachment_join(message_id INTEGER,attachment_id INTEGER);
        INSERT INTO chat VALUES('chat-fixture','fixture@example.invalid','Fixture group','iMessage');
        INSERT INTO chat VALUES('other-chat','other@example.invalid','Fixture group','iMessage');
        INSERT INTO handle VALUES('sender@example.invalid');").unwrap();
    let date = apple_time("2026-09-01T12:00:00Z").unwrap();
    for i in 1..=5 {
        db.execute(
            "INSERT INTO message VALUES(?1,?2,?3,1,?4,NULL)",
            params![
                format!("message-{i}"),
                date + i,
                i % 2,
                format!("untrusted fixture {i}")
            ],
        )
        .unwrap();
        db.execute("INSERT INTO chat_message_join VALUES(1,?1)", [i])
            .unwrap();
    }
    db.execute_batch("INSERT INTO attachment VALUES('attachment-fixture','image/png',123,'/private/should-not-appear.png'); INSERT INTO message_attachment_join VALUES(5,1);").unwrap();
    db
}
fn query_args() -> Value {
    json!({"chat_id":1,"chat_guid":"chat-fixture","start":"2026-09-01T00:00:00Z","end":"2026-09-02T00:00:00Z","limit":2})
}
#[test]
fn exact_identity_sender_dates_and_no_attachment_paths() {
    let db = fixture();
    let v = history_with(&db, &query_args()).unwrap();
    assert_eq!(v["state"], "ok");
    assert_eq!(v["messages"][0]["guid"], "message-5");
    assert!(v["messages"][0]["sender"].is_null());
    assert_eq!(v["messages"][0]["sender_kind"], "self");
    assert_eq!(v["messages"][1]["sender"], "sender@example.invalid");
    assert_eq!(v["messages"][1]["sender_kind"], "participant");
    assert_eq!(v["messages"][0]["is_from_me"], true);
    assert_eq!(v["messages"][0]["attachments"][0]["bytes"], 123);
    assert!(!v.to_string().contains("/private/"));
    assert!(
        v["messages"][0]["timestamp"]
            .as_str()
            .unwrap()
            .starts_with("2026-09-01T12:00:00")
    );
    let resolved = resolve_with(&db, &json!({"identifier":"fixture@example.invalid"})).unwrap();
    assert_eq!(resolved["conversations"].as_array().unwrap().len(), 1);
    assert_eq!(resolved["conversations"][0]["chat_guid"], "chat-fixture");
    assert_eq!(
        resolve_with(&db, &json!({"identifier":"Fixture group"})).unwrap()["state"],
        "no_results"
    );
}
#[test]
fn pages_do_not_repeat_and_cursor_is_bound_to_window_and_identity() {
    let db = fixture();
    let mut q = query_args();
    let mut ids = Vec::new();
    loop {
        let v = history_with(&db, &q).unwrap();
        ids.extend(
            v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["row_id"].as_i64().unwrap()),
        );
        if v["next_cursor"].is_null() {
            break;
        }
        q["cursor"] = v["next_cursor"].clone();
    }
    assert_eq!(ids, vec![5, 4, 3, 2, 1]);
    q["chat_id"] = 2.into();
    q["chat_guid"] = "other-chat".into();
    assert!(history_with(&db, &q).is_err());
}
#[test]
fn missing_identity_and_empty_window_are_distinct() {
    let db = fixture();
    let mut q = query_args();
    q["chat_guid"] = "wrong".into();
    assert_eq!(history_with(&db, &q).unwrap()["state"], "identity_mismatch");
    q = query_args();
    q["start"] = "2026-09-03T00:00:00Z".into();
    q["end"] = "2026-09-04T00:00:00Z".into();
    assert_eq!(history_with(&db, &q).unwrap()["state"], "no_results");
}
#[test]
fn invalid_arguments_fail_before_private_storage_access() {
    for patch in [
        json!({"limit":0}),
        json!({"limit":101}),
        json!({"max_bytes":100000}),
        json!({"end":"2027-01-01T00:00:00Z"}),
        json!({"send":true}),
        json!({"cursor":"bad"}),
        json!({"start":"yesterday"}),
    ] {
        let mut q = query_args();
        q.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(validate(&q).is_err(), "{q}");
    }
}
#[test]
fn byte_budget_unicode_and_attributed_only_text_are_explicit() {
    let db = fixture();
    db.execute(
        "UPDATE message SET text=?1 WHERE rowid=5",
        ["😀\"".repeat(100000)],
    )
    .unwrap();
    db.execute_batch("UPDATE message SET text=NULL,attributedBody=X'ABCD' WHERE rowid=4")
        .unwrap();
    let mut q = query_args();
    q["max_bytes"] = 2048.into();
    let v = history_with(&db, &q).unwrap();
    assert!(serde_json::to_vec(&v).unwrap().len() <= 2048);
    assert_eq!(v["messages"][0]["text_truncated"], true);
    q["max_bytes"] = 65536.into();
    let v = history_with(&db, &q).unwrap();
    assert_eq!(v["messages"][1]["text_unavailable"], true);
}
#[test]
fn readonly_open_does_not_create_and_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.db");
    assert!(open(&path).is_err());
    assert!(!path.exists());
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE probe(id INTEGER)")
        .unwrap();
    let db = open(&path).unwrap();
    assert!(db.execute("INSERT INTO probe VALUES(1)", []).is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM probe", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn permission_and_storage_errors_never_become_empty_results() {
    assert_eq!(
        storage_state(&std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()),
        "permission_required"
    );
    assert_eq!(
        storage_state(&anyhow::Error::msg("broken schema")),
        "storage_unavailable"
    );
    let db = Connection::open_in_memory().unwrap();
    assert!(history_with(&db, &query_args()).is_err());
}

#[test]
fn equal_timestamp_pages_use_row_id_without_repeating_or_omitting_messages() {
    let db = fixture();
    db.execute(
        "UPDATE message SET date=?1",
        [apple_time("2026-09-01T12:00:00Z").unwrap()],
    )
    .unwrap();
    let mut q = query_args();
    let first = history_with(&db, &q).unwrap();
    q["cursor"] = first["next_cursor"].clone();
    let second = history_with(&db, &q).unwrap();
    q["cursor"] = second["next_cursor"].clone();
    let third = history_with(&db, &q).unwrap();
    let ids: Vec<_> = [first, second, third]
        .into_iter()
        .flat_map(|page| {
            page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["row_id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(ids, [5, 4, 3, 2, 1]);
}

#[test]
fn oversized_identity_is_rejected_instead_of_truncated_into_another_identity() {
    let db = fixture();
    db.execute(
        "UPDATE message SET guid=?1 WHERE rowid=5",
        ["x".repeat(100_000)],
    )
    .unwrap();
    assert!(history_with(&db, &query_args()).is_err());
    db.execute(
        "UPDATE chat SET guid=?1 WHERE rowid=1",
        ["x".repeat(100_000)],
    )
    .unwrap();
    assert!(resolve_with(&db, &json!({"identifier":"fixture@example.invalid"})).is_err());
    assert!(history_with(&db, &query_args()).is_err());
}

#[tokio::test]
async fn public_history_dispatch_rejects_invalid_args_before_opening_messages_database() {
    // Both invalid requests return before HOME/path resolution or SQLite open.
    assert!(query(&json!({"send":true}), false).await.is_err());
    assert!(
        query(&json!({"identifier":"fixture", "extra":true}), true)
            .await
            .is_err()
    );
    let tools = schema();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "imessage_history_resolve");
    assert_eq!(tools[1]["name"], "imessage_history");
}

#[test]
fn corrupt_text_is_storage_error_instead_of_silently_successful_empty_history() {
    let db = fixture();
    for sql in [
        "UPDATE message SET text=CAST(X'FF' AS TEXT) WHERE rowid=5",
        "UPDATE message SET text=CAST(X'F0' AS TEXT) WHERE rowid=5",
    ] {
        db.execute_batch(sql).unwrap();
        assert!(history_with(&db, &query_args()).is_err());
    }
}
