use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use zeroclaw_api::deadline::{DeadlineExceeded, PARENT};

struct Fixture {
    directory: tempfile::TempDir,
    tool: SubprocessTool,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("plugin.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            tool: SubprocessTool::new(super::tests::make_manifest("synthetic", vec![]), script),
            directory,
        }
    }

    fn marker(&self, suffix: &str) -> PathBuf {
        self.directory.path().join(format!("plugin.sh.{suffix}"))
    }

    async fn pid(&self) -> u32 {
        timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(self.marker("pid")).await
                    && let Ok(pid) = text.trim().parse()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic child must start")
    }
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn assert_reaped(pid: u32) {
    timeout(Duration::from_secs(2), async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned direct child must be reaped");
}

async fn wait_for_file(path: &Path) {
    timeout(Duration::from_secs(2), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture effect must happen");
}

#[tokio::test]
async fn expired_parent_and_oversized_arguments_never_spawn() {
    let fixture = Fixture::new("exit 0");
    let error = PARENT
        .scope(
            Some(tokio::time::Instant::now()),
            fixture.tool.execute(json!({})),
        )
        .await
        .unwrap_err();
    let deadline = error.downcast_ref::<DeadlineExceeded>().unwrap();
    assert_eq!(deadline.phase, Phase::Tool);
    assert!(!deadline.started);
    assert!(!fixture.marker("pid").exists());
    let error = fixture
        .tool
        .execute(json!({"body": "x".repeat(MAX_PROTOCOL_BYTES)}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("exceeds 1 MiB"));
    assert!(!fixture.marker("pid").exists());
}

#[tokio::test]
async fn cancellation_during_blocked_stdin_reaps_direct_child() {
    let fixture = Fixture::new("exec tail -f /dev/null");
    let mut execution = Box::pin(fixture.tool.execute(json!({"body":"x".repeat(512 * 1024)})));
    let pid = tokio::select! {
        pid = fixture.pid() => pid,
        result = &mut execution => panic!("write should still be blocked: {result:?}"),
    };
    assert!(alive(pid));
    drop(execution);
    assert_reaped(pid).await;
}

#[tokio::test]
async fn parent_expiry_after_effect_reaps_child_without_replay() {
    let fixture =
        Fixture::new("cat >/dev/null\nprintf effect >>\"$0.effects\"\nexec tail -f /dev/null");
    let result = PARENT
        .scope(
            Some(tokio::time::Instant::now() + Duration::from_secs(1)),
            fixture.tool.execute(json!({})),
        )
        .await;
    let error = result.unwrap_err();
    let deadline = error.downcast_ref::<DeadlineExceeded>().unwrap();
    assert!(deadline.started);
    wait_for_file(&fixture.marker("effects")).await;
    assert_reaped(fixture.pid().await).await;
    assert_eq!(
        std::fs::read_to_string(fixture.marker("effects")).unwrap(),
        "effect"
    );
}

#[tokio::test]
async fn local_timeout_bounds_stdin_and_reaps_child() {
    let fixture = Fixture::new("exec tail -f /dev/null");
    // Native process startup is outside Tokio's scheduling guarantees. Observe
    // the child while the exchange is alive instead of demanding a PID file
    // after a short timeout may already have killed it before script startup.
    let mut execution = Box::pin(fixture.tool.execute_with_budget(
        json!({"body":"x".repeat(512 * 1024)}),
        Duration::from_secs(3),
        Duration::from_millis(500),
    ));
    let pid = tokio::select! {
        pid = fixture.pid() => pid,
        result = &mut execution => panic!("fixture must reach the blocked write: {result:?}"),
    };
    let result = execution.await.unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("timed out"));
    assert_reaped(pid).await;
}

#[tokio::test]
async fn stderr_backpressure_does_not_deadlock_stdin_or_stdout() {
    let fixture = Fixture::new(
        r#"head -c 1048576 /dev/zero >&2
cat >/dev/null
printf '%s\n' '{"success":true,"output":"confirmed fixture","error":null}'"#,
    );
    let result = fixture
        .tool
        .execute(json!({"body":"x".repeat(512 * 1024)}))
        .await
        .unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(result.output, "confirmed fixture");
    assert_reaped(fixture.pid().await).await;
}

#[tokio::test]
async fn received_output_survives_nonzero_exit_and_exit_timeout() {
    for suffix in ["exit 7", "exec tail -f /dev/null"] {
        let fixture = Fixture::new(&format!(
            r#"cat >/dev/null
printf '%s\n' '{{"success":true,"output":{{"text":"partial","data":{{"receipt_id":"fixture-1"}}}},"error":null}}'
{suffix}"#
        ));
        let result = fixture
            .tool
            .execute_with_budget(
                json!({}),
                Duration::from_secs(2),
                Duration::from_millis(150),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.output.as_str(), "partial");
        assert_eq!(result.output.data().unwrap()["receipt_id"], "fixture-1");
        assert!(
            result
                .error
                .unwrap()
                .contains("do not replay automatically")
        );
        assert_reaped(fixture.pid().await).await;
    }
}

#[tokio::test]
async fn oversized_unterminated_stdout_is_refused_without_waiting_for_eof() {
    let fixture = Fixture::new("cat >/dev/null\nhead -c 1048577 /dev/zero\nexec tail -f /dev/null");
    let result = fixture.tool.execute(json!({})).await.unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("response exceeds 1 MiB"));
    assert_reaped(fixture.pid().await).await;
}

#[tokio::test]
async fn malformed_and_empty_responses_fail_with_bounded_stderr() {
    for response in ["", "not-json"] {
        let fixture = Fixture::new(&format!(
            r#"cat >/dev/null
head -c 1048576 /dev/zero >&2
printf '%s\n' '{response}'"#
        ));
        let result = fixture.tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        let error = result.error.unwrap();
        assert!(error.len() < 2048, "diagnostic must remain bounded");
        assert!(error.contains("do not replay automatically"));
        assert_reaped(fixture.pid().await).await;
    }
}

#[test]
fn request_envelope_budget_includes_the_trailing_newline() {
    let mut buffer = ProtocolBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, &json!("x".repeat(MAX_PROTOCOL_BYTES - 3))).unwrap();
    assert_eq!(buffer.0.len() + 1, MAX_PROTOCOL_BYTES);
    let mut oversized = ProtocolBuffer(Vec::new());
    assert!(
        serde_json::to_writer(&mut oversized, &json!("x".repeat(MAX_PROTOCOL_BYTES - 2))).is_err()
    );
    assert!(oversized.0.len() < MAX_PROTOCOL_BYTES);
}
