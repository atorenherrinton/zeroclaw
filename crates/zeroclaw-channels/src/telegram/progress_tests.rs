use super::*;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw_api::channel::{ToolActivity, ToolProgressPhase};

fn channel(server: &MockServer) -> TelegramChannel {
    TelegramChannel::new(
        "fixture-token".into(),
        "progress-fixture",
        Arc::new(|| vec!["*".into()]),
        false,
    )
    .with_streaming(StreamMode::Partial, 1000)
    .with_api_base(server.uri())
}

fn snapshot(text: &str, elapsed_secs: u64) -> DraftSnapshot<'_> {
    DraftSnapshot {
        text,
        activity: DraftActivity::Tool(ToolProgressEvent {
            activity: ToolActivity::BrowserRead,
            phase: ToolProgressPhase::Running,
        }),
        elapsed_secs,
    }
}

async fn mock_edit(server: &MockServer, response: ResponseTemplate, count: u64) {
    Mock::given(method("POST"))
        .and(path_regex("/editMessageText$"))
        .respond_with(response)
        .expect(count)
        .mount(server)
        .await;
}

fn successful_edit() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({"ok":true,"result":{"message_id":42}}))
}

#[tokio::test]
async fn progress_snapshot_preserves_narration_across_activity_and_elapsed_updates() {
    let server = MockServer::start().await;
    mock_edit(&server, successful_edit(), 3).await;
    let channel = channel(&server);
    let narration = "I found the relevant page and am checking its details.";
    let running = snapshot(narration, 1);
    let completed = DraftSnapshot {
        activity: DraftActivity::Tool(ToolProgressEvent {
            activity: ToolActivity::BrowserRead,
            phase: ToolProgressPhase::Succeeded,
        }),
        elapsed_secs: 7,
        ..running
    };
    let waiting = DraftSnapshot {
        activity: DraftActivity::Lifecycle(ProgressEvent::WaitingOnModel),
        elapsed_secs: 30,
        ..running
    };
    for update in [running, completed, waiting] {
        channel
            .update_draft_snapshot("123:7", "42", update)
            .await
            .unwrap();
    }

    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    for body in &bodies {
        let text = body["text"].as_str().unwrap();
        assert!(text.starts_with(narration));
        assert!(text.len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
        assert_eq!(body["chat_id"], "123");
        assert_eq!(body["message_id"], 42);
    }
    assert_ne!(bodies[0]["text"], bodies[1]["text"]);
    assert_ne!(bodies[1]["text"], bodies[2]["text"]);
    let final_text = bodies[2]["text"].as_str().unwrap();
    assert!(
        final_text.contains(&crate::util::localized_lifecycle_progress(
            ProgressEvent::WaitingOnModel
        ))
    );
    assert!(
        final_text.contains(&i18n::get_required_cli_string_with_args(
            "channel-runtime-progress-elapsed",
            &[("seconds", "30")]
        ))
    );
}

#[tokio::test]
async fn progress_snapshot_retains_latest_unicode_narration_and_activity_at_limit() {
    let server = MockServer::start().await;
    mock_edit(&server, successful_edit(), 1).await;
    let channel = channel(&server);
    let narration = format!(
        "EARLY TEXT\n{}\nLatest: the check passed.",
        "界🌱".repeat(1500)
    );
    channel
        .update_draft_snapshot("123", "42", snapshot(&narration, 90))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let text = body["text"].as_str().unwrap();
    assert!(text.len() <= TELEGRAM_MAX_MESSAGE_LENGTH);
    assert!(text.starts_with("…\n"));
    assert!(!text.contains("EARLY TEXT"));
    assert!(text.contains("Latest: the check passed."));
    assert!(
        text.contains(&crate::util::localized_tool_progress(ToolProgressEvent {
            activity: ToolActivity::BrowserRead,
            phase: ToolProgressPhase::Running,
        }))
    );
}

#[tokio::test]
async fn progress_snapshots_keep_topic_drafts_separate_and_raw_status_private() {
    let server = MockServer::start().await;
    mock_edit(&server, successful_edit(), 2).await;
    let channel = channel(&server);
    channel
        .update_draft_progress("123:7", "42", "private tool command or raw result")
        .await
        .unwrap();
    channel
        .update_draft_snapshot("123:7", "42", snapshot("Task one", 4))
        .await
        .unwrap();
    channel
        .update_draft_snapshot("123:8", "43", snapshot("Task two", 9))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for (request, id, own, other) in [
        (&requests[0], 42, "Task one", "Task two"),
        (&requests[1], 43, "Task two", "Task one"),
    ] {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["chat_id"], "123");
        assert_eq!(body["message_id"], id);
        let text = body["text"].as_str().unwrap();
        assert!(text.starts_with(own));
        assert!(!text.contains(other));
        assert!(!text.contains("private tool command"));
    }
}

#[tokio::test]
async fn progress_snapshot_failure_and_missing_draft_are_reported_without_payload() {
    for (status, envelope) in [
        (
            400,
            serde_json::json!({"ok":false,"error_code":400,"description":"Bad Request: message to edit not found fixture-private-content"}),
        ),
        (
            500,
            serde_json::json!({"ok":false,"error_code":500,"description":"fixture-private-content"}),
        ),
        (
            200,
            serde_json::json!({"ok":false,"error_code":403,"description":"fixture-private-content"}),
        ),
        (
            200,
            serde_json::json!({"unexpected":"fixture-private-content"}),
        ),
    ] {
        let server = MockServer::start().await;
        mock_edit(
            &server,
            ResponseTemplate::new(status).set_body_json(envelope),
            1,
        )
        .await;
        let channel = channel(&server);
        let error = channel
            .update_draft_snapshot("123", "42", snapshot("Checking", 8))
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("Telegram progress edit failed"));
        assert!(!error.contains("fixture-private-content"));
        assert!(!error.contains("fixture-token"));
        assert!(channel.last_draft_edit.lock().is_empty());
    }
}

#[tokio::test]
async fn progress_snapshot_rate_limit_returns_backoff_without_retrying() {
    for (response, expected_delay) in [
        (
            ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok":false,"error_code":429,"description":"fixture-private-content",
                "parameters":{"retry_after":47}
            })),
            47,
        ),
        (
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "19")
                .set_body_string("fixture-private-content"),
            19,
        ),
        (
            ResponseTemplate::new(429).set_body_string("unavailable"),
            30,
        ),
    ] {
        let server = MockServer::start().await;
        mock_edit(&server, response, 1).await;
        let channel = channel(&server);
        let error = channel
            .update_draft_snapshot("123", "42", snapshot("Checking", 8))
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<DraftUpdateRateLimit>()
                .unwrap()
                .retry_after_secs,
            expected_delay
        );
        assert!(!format!("{error:#}").contains("fixture-private-content"));
        assert!(channel.last_draft_edit.lock().is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn progress_snapshot_not_modified_is_successful() {
    let server = MockServer::start().await;
    mock_edit(
        &server,
        ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "ok":false,"error_code":400,"description":"Bad Request: message is not modified"
        })),
        1,
    )
    .await;
    let channel = channel(&server);
    channel
        .update_draft_snapshot("123:7", "42", snapshot("Checking", 8))
        .await
        .unwrap();
    assert!(
        channel
            .last_draft_edit
            .lock()
            .contains_key(&("123".into(), "42".into()))
    );
}

#[tokio::test]
async fn progress_snapshots_are_opted_in_by_partial_mode_only() {
    let server = MockServer::start().await;
    for mode in [StreamMode::Off, StreamMode::MultiMessage] {
        let channel = channel(&server).with_streaming(mode, 1250);
        assert!(!channel.supports_progress_snapshots());
        channel
            .update_draft_snapshot("123", "42", snapshot("Checking", 8))
            .await
            .unwrap();
    }
    let channel = channel(&server);
    assert!(channel.supports_progress_snapshots());
    assert_eq!(channel.draft_update_interval_ms(), 1000);
    assert!(
        channel
            .update_draft_snapshot("123", "invalid", snapshot("Checking", 8))
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
