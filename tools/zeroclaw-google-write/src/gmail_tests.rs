use super::*;

fn draft() -> Value {
    json!({"to":"recipient@example.com", "subject":"Example", "body":"Draft text"})
}

fn receipt() -> Value {
    json!({"draftId":"draft-1", "message":{"id":"message-2", "threadId":"thread-1"}})
}

#[tokio::test]
async fn standalone_draft_preserves_scope_and_reports_gog_receipt() {
    let mut calls = Vec::new();
    let result = create_gmail_draft_with(&draft(), |command| {
        calls.push(command);
        std::future::ready(Ok(receipt()))
    })
    .await
    .unwrap();
    assert_eq!(calls.len(), 1);
    let command = &calls[0];
    for flag in [
        "--enable-commands-exact=gmail.drafts.create",
        "--gmail-no-send",
        "--no-input",
        "--to=recipient@example.com",
        "--subject=Example",
        "--body=Draft text",
    ] {
        assert!(command.iter().any(|arg| arg == flag), "{flag}: {command:?}");
    }
    assert!(!command.iter().any(|arg| arg.starts_with("--thread-id=") || arg.starts_with("--reply-to-message-id=")));
    assert_eq!(result["draft_id"], "draft-1");
    assert_eq!(result["message_id"], "message-2");
    assert_eq!(result["thread_id"], "thread-1");
    assert_eq!(result["created"], true);
    assert_eq!(result["sent"], false);
    assert_eq!(result["subject"], "Example");
}

#[tokio::test]
async fn reply_targets_reach_gog_without_implicit_recipients_or_subject() {
    for (key, id, flag) in [
        (
            "reply_to_message_id",
            "message-1",
            "--reply-to-message-id=message-1",
        ),
        ("thread_id", "thread-1", "--thread-id=thread-1"),
    ] {
        for explicit_subject in [false, true] {
            let mut args = draft();
            args[key] = json!(id);
            if !explicit_subject {
                args.as_object_mut().unwrap().remove("subject");
            }
            let mut calls = Vec::new();
            let result = create_gmail_draft_with(&args, |command| {
                calls.push(command);
                std::future::ready(Ok(receipt()))
            })
            .await
            .unwrap();
            assert_eq!(calls.len(), 1);
            let command = &calls[0];
            assert!(command.iter().any(|arg| arg == flag));
            assert!(
                command
                    .iter()
                    .any(|arg| arg == "--to=recipient@example.com")
            );
            assert!(command.iter().any(|arg| arg == "--gmail-no-send"));
            assert!(
                command
                    .iter()
                    .any(|arg| arg == "--enable-commands-exact=gmail.drafts.create")
            );
            assert!(
                !command
                    .iter()
                    .any(|arg| arg == "--reply-all" || arg == "--quote")
            );
            assert_eq!(
                command.iter().any(|arg| arg.starts_with("--subject=")),
                explicit_subject
            );
            assert_eq!(result["thread_id"], "thread-1");
            assert_eq!(result["sent"], false);
            if !explicit_subject {
                assert!(result["subject"].is_null());
            }
        }
    }
}

#[tokio::test]
async fn invalid_reply_arguments_fail_before_google() {
    let mut invalid = vec![json!(null), json!([])];
    for key in ["reply_to_message_id", "thread_id"] {
        for value in [
            json!(null),
            json!(true),
            json!(42),
            json!([]),
            json!({}),
            json!(""),
            json!(" "),
            json!(" abc"),
            json!("abc\n"),
            json!("<parent@example.com>"),
            json!("https://mail.google.com/thread"),
            json!("a\0b"),
            json!("--send=true"),
            json!("a".repeat(129)),
        ] {
            let mut args = draft();
            args[key] = value;
            invalid.push(args);
        }
    }
    let mut both = draft();
    both["reply_to_message_id"] = json!("message-1");
    both["thread_id"] = json!("thread-1");
    invalid.push(both);
    for key in ["reply_all", "send", "attach", "cc"] {
        let mut args = draft();
        args[key] = json!(true);
        invalid.push(args);
    }
    let mut missing_subject = draft();
    missing_subject.as_object_mut().unwrap().remove("subject");
    invalid.push(missing_subject);
    for (key, value) in [
        ("to", json!("bad address")),
        ("subject", json!("Injected\r\nHeader: value")),
        ("subject", json!(null)),
        ("body", json!("")),
    ] {
        let mut args = draft();
        args["thread_id"] = json!("thread-1");
        args[key] = value;
        invalid.push(args);
    }
    for args in invalid {
        let mut calls = 0;
        let result = create_gmail_draft_with(&args, |_| {
            calls += 1;
            std::future::ready(Ok(receipt()))
        })
        .await;
        assert!(result.is_err(), "{args}");
        assert_eq!(calls, 0, "invalid request reached Google: {args}");
    }
}

#[tokio::test]
async fn failed_or_uncertain_creation_is_never_retried() {
    for response in [
        Err(anyhow::Error::msg("provider unavailable")),
        Ok(json!({})),
        Ok(json!({"draftId":"draft-1"})),
        Ok(json!({"draftId":"draft-1", "message":{"threadId":"wrong-thread"}})),
    ] {
        let mut args = draft();
        args["thread_id"] = json!("thread-1");
        let mut response = Some(response);
        let mut calls = 0;
        let result = create_gmail_draft_with(&args, |_| {
            calls += 1;
            std::future::ready(response.take().expect("must not retry"))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}

#[tokio::test]
async fn api_shaped_receipt_is_supported() {
    let result = create_gmail_draft_with(&draft(), |_| {
        std::future::ready(Ok(
            json!({"id":"draft-1", "message":{"id":"message-2", "threadId":"thread-1"}}),
        ))
    })
    .await
    .unwrap();
    assert_eq!(result["draft_id"], "draft-1");
}
