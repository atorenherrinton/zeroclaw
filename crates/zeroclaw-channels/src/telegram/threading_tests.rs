use super::*;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw_api::channel::{ToolActivity, ToolProgressPhase};
use zeroclaw_api::conversation::{ACTIVE_CONVERSATION, ConversationRoute};

fn channel(server: &MockServer) -> TelegramChannel {
    TelegramChannel::new(
        "fixture-token".into(),
        "fixture",
        Arc::new(|| vec!["*".into()]),
        false,
    )
    .with_streaming(StreamMode::Partial, 20)
    .with_api_base(server.uri())
}

fn route(topic: &str, reply: &str) -> ConversationRoute {
    ConversationRoute {
        channel: "telegram.fixture".into(),
        recipient: format!("123:{topic}"),
        sender: "123".into(),
        thread: Some(topic.into()),
        reply_to: reply.into(),
    }
}

async fn mock_sends(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex("/(sendMessage|editMessageText|sendPhoto|sendDocument|sendVoice|sendVideo|sendAudio|deleteMessage)$"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let id = body["message_id"].as_i64().unwrap_or_else(|| {
                if body["message_thread_id"] == "8" { 43 } else { 42 }
            });
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true,"result":{"message_id":id}}))
        })
        .mount(server).await;
}

#[tokio::test]
async fn private_task_topics_keep_followup_history_and_reply_to_original_message() {
    let server = MockServer::start().await;
    mock_sends(&server).await;
    let channel = channel(&server);
    let incoming = |message_id, topic, text: &str| {
        serde_json::json!({
            "message": {"message_id":message_id, "message_thread_id":topic,
                "is_topic_message":true, "chat":{"id":123,"type":"private"},
                "from":{"id":123}, "text":text}
        })
    };
    let first = channel
        .parse_update_message(&incoming(99, 7, "Task one"))
        .unwrap();
    let followup = channel
        .parse_update_message(&incoming(100, 7, "Continue"))
        .unwrap();
    let other = channel
        .parse_update_message(&incoming(101, 8, "Task two"))
        .unwrap();
    assert_eq!(first.reply_target, "123:7");
    assert_eq!(first.interruption_scope_id.as_deref(), Some("7"));
    assert_eq!(
        crate::orchestrator::conversation_history_key(&first),
        crate::orchestrator::conversation_history_key(&followup)
    );
    assert_ne!(
        crate::orchestrator::conversation_history_key(&first),
        crate::orchestrator::conversation_history_key(&other)
    );

    let route = ConversationRoute::from_message(&first);
    // Explicit metadata survives spawning or another task-local boundary.
    let draft = channel
        .send_draft(&route.message("Starting task one"))
        .await
        .unwrap()
        .unwrap();
    ACTIVE_CONVERSATION
        .scope(
            Some(route),
            channel.finalize_draft("123:7", &draft, &"a".repeat(5314), true),
        )
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        if request.url.path().ends_with("sendMessage") {
            assert_eq!(body["message_thread_id"], "7");
            assert_eq!(body["reply_parameters"]["message_id"], 99);
        } else {
            assert_eq!(body["message_id"], 42);
            assert!(body.get("reply_parameters").is_none());
        }
    }
}

#[tokio::test]
async fn explicit_reply_anchor_wins_and_other_destinations_do_not_inherit() {
    let server = MockServer::start().await;
    mock_sends(&server).await;
    let channel = channel(&server);
    ACTIVE_CONVERSATION
        .scope(Some(route("7", "9")), async {
            channel
                .send(
                    &SendMessage::new("explicit", "123:7")
                        .in_reply_to(Some("telegram_123_20".into())),
                )
                .await
                .unwrap();
            channel
                .send(&SendMessage::new("other chat", "456:7"))
                .await
                .unwrap();
            channel
                .send(&SendMessage::new("other topic", "123:8"))
                .await
                .unwrap();
            channel
                .send(
                    &SendMessage::new("invalid explicit", "123:7")
                        .in_reply_to(Some("telegram_456_20".into())),
                )
                .await
                .unwrap();
        })
        .await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(bodies[0]["reply_parameters"]["message_id"], 20);
    for body in &bodies[1..] {
        assert!(body.get("reply_parameters").is_none());
    }
    assert!(zeroclaw_api::conversation::current().is_none());
}

#[tokio::test]
async fn simultaneous_private_topics_have_independent_drafts_and_reliable_tool_progress() {
    let server = MockServer::start().await;
    mock_sends(&server).await;
    let channel = channel(&server);
    let first = route("7", "9").message("Starting first");
    let second = route("8", "10").message("Starting second");
    let (first, second) = tokio::join!(channel.send_draft(&first), channel.send_draft(&second));
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    let event = ToolProgressEvent {
        activity: ToolActivity::Browser,
        phase: ToolProgressPhase::Running,
    };
    let (a, b) = tokio::join!(
        channel.update_draft_tool_progress("123:7", &first, event),
        channel.update_draft_tool_progress("123:8", &second, event)
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(channel.last_draft_edit.lock().len(), 2);
    let requests = server.received_requests().await.unwrap();
    let edits: Vec<serde_json::Value> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("editMessageText"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        edits.len(),
        2,
        "tool starts must not disappear behind the initial draft throttle"
    );
    assert!(edits.iter().any(|body| body["message_id"] == 42));
    assert!(edits.iter().any(|body| body["message_id"] == 43));
    channel.cancel_draft("123:7", &first).await.unwrap();
    assert_eq!(channel.last_draft_edit.lock().len(), 1);
    assert!(
        channel
            .last_draft_edit
            .lock()
            .contains_key(&("123".into(), second))
    );
}

#[tokio::test]
async fn attachment_json_and_multipart_keep_topic_and_reply_anchor() {
    let server = MockServer::start().await;
    mock_sends(&server).await;
    let channel = channel(&server);
    ACTIVE_CONVERSATION
        .scope(Some(route("7", "9")), async {
            channel
                .send_photo_by_url("123", Some("7"), "https://example.test/photo.jpg", None)
                .await
                .unwrap();
            channel
                .send_document_by_url("123", Some("7"), "https://example.test/report.pdf", None)
                .await
                .unwrap();
            channel
                .send_voice_by_url("123", Some("7"), "https://example.test/audio.ogg", None)
                .await
                .unwrap();
            channel
                .send_document_bytes("123", Some("7"), b"fixture".to_vec(), "report.txt", None)
                .await
                .unwrap();
        })
        .await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    for request in &requests[..3] {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["message_thread_id"], "7");
        assert_eq!(body["reply_parameters"]["message_id"], 9);
    }
    let multipart = String::from_utf8_lossy(&requests[3].body);
    assert!(multipart.contains("name=\"message_thread_id\"\r\n\r\n7"));
    assert!(multipart.contains("name=\"reply_parameters\""));
    assert!(multipart.contains("\"message_id\":9"));
}

#[tokio::test]
async fn replies_preserve_selected_quotes_and_media_captions() {
    let server = MockServer::start().await;
    let channel = channel(&server);
    let mut message = serde_json::json!({"message_id":100,"message_thread_id":7,"is_topic_message":true,
        "chat":{"id":123,"type":"private"},"from":{"id":123},"text":"Use this",
        "reply_to_message":{"message_id":7,"from":{"username":"fixture"},"caption":"Project plan","document":{"file_id":"fixture"}}});
    let parsed = channel
        .parse_update_message(&serde_json::json!({"message":message}))
        .unwrap();
    assert!(parsed.content.contains("> Project plan"));
    message["quote"] = serde_json::json!({"text":"Selected requirement"});
    let parsed = channel
        .parse_update_message(&serde_json::json!({"message":message}))
        .unwrap();
    assert!(parsed.content.contains("> Selected requirement"));
    assert!(!parsed.content.contains("Project plan"));
}

#[tokio::test]
async fn typing_targets_private_topics_and_stopping_one_keeps_the_other() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("/sendChatAction$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true,"result":true})),
        )
        .mount(&server)
        .await;
    let channel = channel(&server);
    channel.start_typing("123:7").await.unwrap();
    channel.start_typing("123:8").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.received_requests().await.unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    channel.stop_typing("123:7").await.unwrap();
    assert!(!channel.typing_handles.lock().contains_key("123:7"));
    assert!(channel.typing_handles.lock().contains_key("123:8"));
    channel.stop_typing("123:8").await.unwrap();
    let channel = channel.with_ack_reactions(false);
    let (tx, mut rx) = zeroclaw_api::inbound::channel(1);
    let mut offset = 0;
    let mut transient_retry = None;
    let outcome = channel
        .process_update(
            &serde_json::json!({
                "update_id": 5, "message": {"message_id": 99,
                    "message_thread_id": 9, "is_topic_message": true,
                    "chat": {"id": 123, "type": "private"},
                    "from": {"id": 123}, "text": "Start third task"}
            }),
            &tx,
            &mut offset,
            &mut transient_retry,
        )
        .await;
    assert!(matches!(outcome, UpdateOutcome::Advanced));
    assert_eq!(rx.try_recv().unwrap().reply_target, "123:9");
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert!(bodies.iter().all(|b| b["chat_id"] == "123"));
    assert!(bodies.iter().any(|b| b["message_thread_id"] == "7"));
    assert!(bodies.iter().any(|b| b["message_thread_id"] == "8"));
    assert!(bodies.iter().any(|b| b["message_thread_id"] == "9"));
}
