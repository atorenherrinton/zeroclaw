use super::*;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw_api::conversation::{ACTIVE_CONVERSATION, ConversationRoute};
use zeroclaw_api::delivery::{DeliveryJournal, JOURNAL};
use zeroclaw_infra::{
    session_backend::SessionBackend, session_delivery::SessionDeliveryJournal,
    session_sqlite::SqliteSessionBackend,
};

fn channel(server: &MockServer) -> TelegramChannel {
    TelegramChannel::new("fixture-token".into(), "fixture", Arc::new(Vec::new), false)
        .with_streaming(StreamMode::Partial, 0)
        .with_api_base(server.uri())
}
async fn edit(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex("/editMessageText$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":42}})),
        )
        .mount(server)
        .await;
}
fn route() -> ConversationRoute {
    ConversationRoute {
        channel: "telegram.fixture".into(),
        recipient: "123:7".into(),
        sender: "fixture".into(),
        thread: Some("7".into()),
        reply_to: "9".into(),
    }
}
#[tokio::test]
async fn oversized_5314_edits_first_and_sends_remainder_in_order() {
    let server = MockServer::start().await;
    edit(&server).await;
    Mock::given(method("POST"))
        .and(path_regex("/sendMessage$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":43}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let text = "a".repeat(5314);
    channel(&server)
        .finalize_draft("123:7", "42", &text, true)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].url.path().ends_with("editMessageText"));
    assert!(requests[1].url.path().ends_with("sendMessage"));
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(first["message_id"], 42);
    assert_eq!(second["message_thread_id"], "7");
    for body in [&first, &second] {
        assert!(body["text"].as_str().unwrap().len() <= 4096);
    }
    assert_eq!(
        first["text"].as_str().unwrap().matches('a').count()
            + second["text"].as_str().unwrap().matches('a').count(),
        5314
    );
}

#[tokio::test]
async fn chunk_two_failure_keeps_first_and_does_not_retry_5xx() {
    let server = MockServer::start().await;
    edit(&server).await;
    Mock::given(method("POST"))
        .and(path_regex("/sendMessage$"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({"ok":false})))
        .expect(1)
        .mount(&server)
        .await;
    let error = channel(&server)
        .finalize_draft("123", "42", &"x".repeat(5314), true)
        .await
        .unwrap_err();
    let error = error.downcast_ref::<DeliveryFailure>().unwrap();
    assert_eq!(error.confirmed_chunks, 1);
    assert_eq!(error.chunk_index, 1);
    assert_eq!(error.outcome, EffectOutcome::PossiblyApplied);
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path().ends_with("deleteMessage"))
    );
}

#[tokio::test]
async fn malformed_success_and_rate_limit_are_not_acknowledgements_or_replayed() {
    for (status, body, outcome) in [
        (
            200,
            serde_json::json!({"ok":true}),
            EffectOutcome::ReconciliationRequired,
        ),
        (
            429,
            serde_json::json!({"ok":false}),
            EffectOutcome::ConfirmedFailed,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("/sendMessage$"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let error = channel(&server)
            .send_text_chunks("hello", "123", None)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
            outcome
        );
    }
}

#[tokio::test]
async fn duplicate_delivery_skips_confirmed_chunks_across_reopen() {
    let server = MockServer::start().await;
    edit(&server).await;
    Mock::given(method("POST"))
        .and(path_regex("/sendMessage$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":43}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let ch = channel(&server);
    for _ in 0..2 {
        let backend: Arc<dyn SessionBackend> =
            Arc::new(SqliteSessionBackend::new(temp.path()).unwrap());
        let journal: Arc<dyn DeliveryJournal> = Arc::new(SessionDeliveryJournal(backend));
        JOURNAL
            .scope(
                Some(journal),
                ACTIVE_CONVERSATION.scope(
                    Some(route()),
                    ch.finalize_draft("123:7", "42", &"x".repeat(5314), true),
                ),
            )
            .await
            .unwrap();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn cancellation_after_acceptance_requires_reconciliation_on_reopen() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/sendMessage$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(10))
                .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":43}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    let backend: Arc<dyn SessionBackend> =
        Arc::new(SqliteSessionBackend::new(temp.path()).unwrap());
    let journal: Arc<dyn DeliveryJournal> = Arc::new(SessionDeliveryJournal(backend));
    let ch = channel(&server);
    let task = ::zeroclaw_spawn::spawn!(async move {
        JOURNAL
            .scope(
                Some(journal),
                ACTIVE_CONVERSATION.scope(
                    Some(route()),
                    ch.send_text_chunks("hello", "123", Some("7")),
                ),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !server.received_requests().await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    let ch = channel(&server);
    let backend: Arc<dyn SessionBackend> =
        Arc::new(SqliteSessionBackend::new(temp.path()).unwrap());
    let journal: Arc<dyn DeliveryJournal> = Arc::new(SessionDeliveryJournal(backend));
    let error = JOURNAL
        .scope(
            Some(journal),
            ACTIVE_CONVERSATION.scope(
                Some(route()),
                ch.send_text_chunks("hello", "123", Some("7")),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
        EffectOutcome::ReconciliationRequired
    );
}

#[tokio::test]
async fn edit_rejected_keeps_draft_and_never_deletes_or_sends_replacement() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/editMessageText$"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"ok":false,"description":"message can't be edited"}),
            ),
        )
        .expect(2)
        .mount(&server)
        .await;
    assert!(
        channel(&server)
            .finalize_draft("123", "42", "hello", true)
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_or_mismatched_acknowledgements_never_confirm_or_retry() {
    for id in [
        serde_json::json!(0),
        serde_json::json!(-1),
        serde_json::json!("42"),
        serde_json::json!(43),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("/editMessageText$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":id}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = channel(&server)
            .finalize_draft("123", "42", "fixture", true)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
            EffectOutcome::ReconciliationRequired
        );
    }
    let server = MockServer::start().await;
    let error = channel(&server)
        .finalize_draft("123", "-1", "fixture", true)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
        EffectOutcome::NotStarted
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn transport_timeout_leaves_durable_uncertainty_without_formatting_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/sendMessage$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(35))
                .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":43}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let temp = tempfile::tempdir().unwrap();
    for attempt in 0..2 {
        let backend: Arc<dyn SessionBackend> =
            Arc::new(SqliteSessionBackend::new(temp.path()).unwrap());
        let journal: Arc<dyn DeliveryJournal> = Arc::new(SessionDeliveryJournal(backend));
        let ch = channel(&server);
        let error = JOURNAL
            .scope(
                Some(journal),
                ACTIVE_CONVERSATION.scope(
                    Some(route()),
                    ch.send_text_chunks("fixture", "123", Some("7")),
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
            if attempt == 0 {
                EffectOutcome::PossiblyApplied
            } else {
                EffectOutcome::ReconciliationRequired
            }
        );
    }
}

#[tokio::test]
async fn not_modified_error_is_not_fabricated_into_a_positive_acknowledgement() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/editMessageText$"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "ok":false,"description":"Bad Request: message is not modified"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let error = channel(&server)
        .finalize_draft("123", "42", "fixture", true)
        .await
        .unwrap_err();
    let failure = error.downcast_ref::<DeliveryFailure>().unwrap();
    assert_eq!(failure.outcome, EffectOutcome::ConfirmedFailed);
    assert_eq!(failure.confirmed_chunks, 0);
}

#[tokio::test]
async fn unavailable_receipt_storage_refuses_network_submission() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    let backend: Arc<dyn SessionBackend> =
        Arc::new(SqliteSessionBackend::new(temp.path()).unwrap());
    // Remove the fixture directory's receipt schema, never production storage.
    rusqlite::Connection::open(temp.path().join("sessions/sessions.db"))
        .unwrap()
        .execute_batch("DROP TABLE channel_delivery_chunks")
        .unwrap();
    let journal: Arc<dyn DeliveryJournal> = Arc::new(SessionDeliveryJournal(backend));
    let ch = channel(&server);
    let error = JOURNAL
        .scope(
            Some(journal),
            ACTIVE_CONVERSATION.scope(
                Some(route()),
                ch.send_text_chunks("fixture", "123", Some("7")),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
        EffectOutcome::NotStarted
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
