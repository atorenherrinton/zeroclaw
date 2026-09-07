use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw::channels::Channel;
use zeroclaw::channels::telegram::TelegramChannel;

fn test_channel(mock_url: &str) -> TelegramChannel {
    let peer_resolver: Arc<dyn Fn() -> Vec<String> + Send + Sync> = Arc::new(|| vec!["*".into()]);
    let mention_only = false;
    TelegramChannel::new(
        "TEST_TOKEN".into(),
        "telegram_test_alias",
        peer_resolver,
        mention_only,
    )
    .with_api_base(mock_url.to_string())
}

fn telegram_ok_response(message_id: i64) -> serde_json::Value {
    json!({
        "ok": true,
        "result": {
            "message_id": message_id,
            "chat": {"id": 123},
            "text": "ok"
        }
    })
}

fn telegram_error_response(description: &str) -> serde_json::Value {
    json!({
        "ok": false,
        "error_code": 400,
        "description": description,
    })
}

#[tokio::test]
async fn finalize_draft_requires_positive_ack_even_for_not_modified_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/editMessageText"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(telegram_error_response(
                "Bad Request: message is not modified",
            )),
        )
        .expect(2)
        .mount(&server)
        .await;
    let error = test_channel(&server.uri())
        .finalize_draft("123", "42", "final text", false)
        .await
        .unwrap_err();
    let receipt = error
        .downcast_ref::<zeroclaw_api::delivery::DeliveryFailure>()
        .unwrap();
    assert_eq!(
        receipt.outcome,
        zeroclaw_api::delivery::EffectOutcome::ConfirmedFailed
    );
    assert_eq!(receipt.confirmed_chunks, 0);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path().ends_with("editMessageText"))
    );
}

#[tokio::test]
async fn finalize_draft_formatting_rejection_allows_plain_edit_with_positive_ack() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/editMessageText"))
        .and(body_partial_json(json!({"parse_mode":"HTML"})))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(telegram_error_response("Bad Request: can't parse entities")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/editMessageText"))
        .and(body_partial_json(json!({"text":"Use **bold**"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_ok_response(42)))
        .expect(1)
        .mount(&server)
        .await;
    test_channel(&server.uri())
        .finalize_draft("123", "42", "Use **bold**", false)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let plain: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(plain.get("parse_mode").is_none());
    assert_eq!(plain["message_id"], 42);
}

#[tokio::test]
async fn finalize_draft_rejected_edits_never_delete_or_send_replacements() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/editMessageText"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(telegram_error_response(
                "Bad Request: message cannot be edited",
            )),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/deleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true,"result":true})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/sendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_ok_response(43)))
        .expect(0)
        .mount(&server)
        .await;
    let error = test_channel(&server.uri())
        .finalize_draft("123", "42", "final text", false)
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<zeroclaw_api::delivery::DeliveryFailure>()
            .unwrap()
            .confirmed_chunks,
        0
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn finalize_draft_oversized_reply_edits_then_sends_without_deletion() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/editMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_ok_response(42)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botTEST_TOKEN/sendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_ok_response(43)))
        .expect(1)
        .mount(&server)
        .await;
    test_channel(&server.uri())
        .finalize_draft("123", "42", &"x".repeat(5314), false)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].url.path().ends_with("editMessageText"));
    assert!(requests[1].url.path().ends_with("sendMessage"));
    let text: String = requests
        .iter()
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(text.matches('x').count(), 5314);
}
