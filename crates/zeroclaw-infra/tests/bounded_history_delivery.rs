use zeroclaw_api::{
    delivery::{ChunkReceipt, EffectOutcome},
    model_provider::ChatMessage,
};
use zeroclaw_infra::{session_backend::SessionBackend, session_sqlite::SqliteSessionBackend};
#[test]
fn cursor_tail_is_stable_during_append_and_does_not_cross_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    for n in 1..=5 {
        db.append("a", &ChatMessage::user(n.to_string())).unwrap();
    }
    db.append("b", &ChatMessage::user("private other session"))
        .unwrap();
    let first = db.load_page("a", None, 2, 1024).unwrap();
    assert_eq!(
        first
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        ["4", "5"]
    );
    db.append("a", &ChatMessage::user("6")).unwrap();
    let second = db.load_page("a", first.next_before, 2, 1024).unwrap();
    assert_eq!(
        second
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        ["2", "3"]
    );
    let third = db.load_page("a", second.next_before, 2, 1024).unwrap();
    assert_eq!(third.messages[0].content, "1");
    assert!(third.next_before.is_none());
}
#[test]
fn huge_unicode_rows_are_bounded_and_explicitly_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    db.append("a", &ChatMessage::user("😀".repeat(300000)))
        .unwrap();
    let page = db.load_page("a", None, 100, 257).unwrap();
    assert!(page.content_bytes <= 257);
    assert!(page.messages[0].truncated);
    assert_eq!(page.messages[0].content.len(), 256);
    assert!(db.load_page("a", None, 101, 257).is_err());
    assert!(db.load_page("a", Some(-1), 1, 257).is_err());
}
#[test]
fn corrupt_storage_is_error_not_empty_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    let raw = rusqlite::Connection::open(dir.path().join("sessions/sessions.db")).unwrap();
    for sql in [
        "INSERT INTO sessions(session_key,role,content,created_at) VALUES('a','user',CAST(X'FF' AS TEXT),'fixture')",
        "UPDATE sessions SET content=CAST(X'F0' AS TEXT) WHERE session_key='a'",
    ] {
        raw.execute_batch(sql).unwrap();
        assert!(db.load_page("a", None, 20, 1024).is_err());
    }
    raw.execute_batch("DROP TABLE sessions").unwrap();
    assert!(db.load_page("a", None, 20, 1024).is_err());
}
fn chunk() -> ChunkReceipt {
    ChunkReceipt {
        key: "response:0".into(),
        response_key: "response".into(),
        chunk_index: 0,
        total_chunks: 2,
        outcome: EffectOutcome::PossiblyApplied,
        platform_message_id: None,
    }
}
#[test]
fn write_ahead_claim_survives_reopen_and_is_not_reclaimable() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = SqliteSessionBackend::new(dir.path()).unwrap();
        assert!(db.claim_delivery_chunk(&chunk()).unwrap().is_none());
    }
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    assert_eq!(
        db.claim_delivery_chunk(&chunk()).unwrap().unwrap().outcome,
        EffectOutcome::PossiblyApplied
    );
    let mut receipt = chunk();
    receipt.outcome = EffectOutcome::Confirmed;
    receipt.platform_message_id = Some("42".into());
    db.finish_delivery_chunk(&receipt).unwrap();
    drop(db);
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    assert_eq!(
        db.claim_delivery_chunk(&chunk())
            .unwrap()
            .unwrap()
            .platform_message_id
            .as_deref(),
        Some("42")
    );
}
#[test]
fn concurrent_claims_have_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let _db = SqliteSessionBackend::new(&path).unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                SqliteSessionBackend::new(&path)
                    .unwrap()
                    .claim_delivery_chunk(&chunk())
                    .unwrap()
                    .is_none()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>(),
        1
    );
}

#[test]
fn finalized_receipts_cannot_be_downgraded_or_rebound() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    db.claim_delivery_chunk(&chunk()).unwrap();
    let mut receipt = chunk();
    receipt.outcome = EffectOutcome::Confirmed;
    assert!(db.finish_delivery_chunk(&receipt).is_err());
    receipt.platform_message_id = Some("42".into());
    receipt.total_chunks = 3;
    assert!(db.finish_delivery_chunk(&receipt).is_err());
    receipt.total_chunks = 2;
    db.finish_delivery_chunk(&receipt).unwrap();
    assert!(db.finish_delivery_chunk(&chunk()).is_err());
    assert_eq!(
        db.claim_delivery_chunk(&chunk()).unwrap().unwrap().outcome,
        EffectOutcome::Confirmed
    );
}

#[test]
fn remaining_byte_budget_and_unicode_cursor_preserve_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteSessionBackend::new(dir.path()).unwrap();
    db.append("a", &ChatMessage::user("😀".repeat(100)))
        .unwrap();
    db.append("a", &ChatMessage::user("x".repeat(255))).unwrap();
    let first = db.load_page("a", None, 100, 256).unwrap();
    assert_eq!(first.messages.len(), 1);
    assert_eq!(first.content_bytes, 255);
    let second = db.load_page("a", first.next_before, 100, 256).unwrap();
    assert_eq!(second.messages.len(), 1);
    assert_eq!(second.messages[0].content, "😀".repeat(64));
    assert!(second.messages[0].truncated);
    assert!(second.next_before.is_none());
}
