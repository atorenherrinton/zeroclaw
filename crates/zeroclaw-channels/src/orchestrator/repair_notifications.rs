//! Telegram operational notices derived from canonical structured tool events.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::attribution::ToolProvenance;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_runtime::agent::loop_::{ProgressEvent, StreamDelta};

const REMINDER_INTERVAL: Duration = Duration::from_secs(120);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// The turn owns the relay. Dropping an aborted turn must not leave a task
/// sending reminders, even if another producer still holds a stream sender.
pub(super) struct NotificationTask(tokio::task::JoinHandle<()>);

impl NotificationTask {
    pub(super) async fn finish(mut self) {
        let result = match tokio::time::timeout(SEND_TIMEOUT, &mut self.0).await {
            Ok(result) => result,
            Err(_) => {
                self.0.abort();
                (&mut self.0).await
            }
        };
        if let Err(error) = result {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_attrs(::serde_json::json!({"cancelled": error.is_cancelled()})),
                "Repair notification relay did not complete"
            );
        }
    }
}

impl Drop for NotificationTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn start(
    rx: Receiver<StreamDelta>,
    channel: Arc<dyn Channel>,
    message: ChannelMessage,
    cancellation: CancellationToken,
    forward_drafts: bool,
) -> (Option<Receiver<StreamDelta>>, NotificationTask) {
    let (draft_tx, draft_rx) = if forward_drafts {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let task = zeroclaw_spawn::spawn!(run(rx, draft_tx, channel, message, cancellation));
    (draft_rx, NotificationTask(task))
}

/// A reviewed, closed activity classifier. Arguments are inspected only for
/// explicit operation/target fields; freeform prompts and shell commands are
/// never evidence of a repair. Extension tools cannot impersonate native ones.
fn starts_code_work(event: &StreamDelta) -> bool {
    let StreamDelta::ToolStart {
        tool,
        arguments,
        tool_provenance: Some(ToolProvenance::Native),
    } = event
    else {
        return false;
    };
    match tool.as_str() {
        "codex_cli" | "claude_code" | "claude_code_runner" | "gemini_cli" | "opencode_cli" => true,
        "delegate" => {
            if arguments
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("delegate")
                != "delegate"
            {
                return false;
            }
            // `coding` is an explicit target alias, not an inference from the
            // delegated prompt. Other agent aliases retain their normal path.
            if let Some(agents) = arguments.get("parallel").and_then(|v| v.as_array()) {
                agents
                    .iter()
                    .any(|agent| agent.as_str().is_some_and(|name| name.trim() == "coding"))
            } else {
                arguments
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .is_some_and(|name| name.trim() == "coding")
            }
        }
        "file_write" | "file_edit" => arguments
            .get("path")
            .and_then(|v| v.as_str())
            .is_some_and(is_source_path),
        _ => false,
    }
}

fn is_source_path(path: &str) -> bool {
    let path = Path::new(path);
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "Makefile" | "Dockerfile" | "CMakeLists.txt"))
    {
        return true;
    }
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "rs" | "py"
                    | "js"
                    | "jsx"
                    | "ts"
                    | "tsx"
                    | "mjs"
                    | "cjs"
                    | "go"
                    | "c"
                    | "h"
                    | "cc"
                    | "cpp"
                    | "hpp"
                    | "cs"
                    | "java"
                    | "kt"
                    | "swift"
                    | "m"
                    | "mm"
                    | "rb"
                    | "php"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "fish"
                    | "ps1"
                    | "lua"
                    | "sql"
                    | "html"
                    | "css"
                    | "scss"
                    | "vue"
                    | "svelte"
                    | "ex"
                    | "exs"
                    | "erl"
                    | "hs"
                    | "scala"
                    | "clj"
                    | "dart"
                    | "r"
                    | "jl"
                    | "pl"
                    | "zig"
                    | "nix"
                    | "gradle"
            )
        })
}

async fn send_notice(
    channel: &dyn Channel,
    message: &ChannelMessage,
    key: &str,
    cancellation: &CancellationToken,
) -> bool {
    let notice = SendMessage::reply_to(
        message,
        zeroclaw_runtime::i18n::get_required_cli_string(key),
    )
    .suppress_voice();
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => return false,
        result = tokio::time::timeout(SEND_TIMEOUT, channel.send(&notice)) => result,
    };
    if matches!(result, Ok(Ok(()))) {
        true
    } else {
        // A transport error or timeout may follow successful delivery. Never
        // retry an uncertain notice or continue its reminder series this turn.
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_attrs(::serde_json::json!({"timed_out": result.is_err()})),
            "Repair notice delivery unconfirmed; further notices disabled for this turn"
        );
        false
    }
}

async fn run(
    mut rx: Receiver<StreamDelta>,
    mut draft_tx: Option<Sender<StreamDelta>>,
    channel: Arc<dyn Channel>,
    message: ChannelMessage,
    cancellation: CancellationToken,
) {
    // These facts originate here: one start notice per turn and its next
    // reminder deadline. They are delivery state, not a second tool registry.
    let mut started = false;
    let mut next_notice = None;
    loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            event = rx.recv() => {
                let Some(event) = event else { break };
                if !started && starts_code_work(&event) {
                    started = true;
                    if send_notice(channel.as_ref(), &message, "channel-runtime-repair-started", &cancellation).await {
                        next_notice = Some(Instant::now() + REMINDER_INTERVAL);
                    }
                }
                if matches!(event, StreamDelta::Lifecycle(ProgressEvent::FinalizingResponse)) {
                    next_notice = None;
                }
                if let Some(tx) = draft_tx.as_ref() {
                    let forwarded = tokio::select! {
                        biased;
                        () = cancellation.cancelled() => break,
                        result = tx.send(event) => result,
                    };
                    if forwarded.is_err() {
                        draft_tx = None;
                    }
                }
            }
            () = tokio::time::sleep_until(next_notice.unwrap_or_else(|| Instant::now() + REMINDER_INTERVAL)),
                if next_notice.is_some() && !rx.is_closed() => {
                next_notice = if send_notice(channel.as_ref(), &message, "channel-runtime-repair-ongoing", &cancellation).await {
                    Some(Instant::now() + REMINDER_INTERVAL)
                } else {
                    None
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zeroclaw_api::attribution::{Attributable, ChannelKind, Role};

    #[derive(Default)]
    struct RecordingChannel {
        sent: std::sync::Mutex<Vec<SendMessage>>,
        attempts: AtomicUsize,
        changed: tokio::sync::Notify,
        fail: bool,
        hang: bool,
    }

    impl Attributable for RecordingChannel {
        fn role(&self) -> Role {
            Role::Channel(ChannelKind::Telegram)
        }
        fn alias(&self) -> &str {
            "test"
        }
    }

    #[async_trait::async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "telegram"
        }
        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_one();
            if self.hang {
                std::future::pending::<()>().await;
            }
            if self.fail {
                anyhow::bail!("synthetic uncertain send failure");
            }
            self.sent.lock().unwrap().push(message.clone());
            Ok(())
        }
        async fn listen(&self, _: zeroclaw_api::inbound::Sender) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn message() -> ChannelMessage {
        let mut message =
            ChannelMessage::new("41", "sender", "chat", "private request", "telegram", 0);
        message.thread_ts = Some("17".into());
        message.channel_alias = Some("test".into());
        message
    }

    fn native(tool: &str, arguments: serde_json::Value) -> StreamDelta {
        StreamDelta::ToolStart {
            tool: tool.into(),
            arguments: Arc::new(arguments),
            tool_provenance: Some(ToolProvenance::Native),
        }
    }

    #[tokio::test]
    async fn repair_notice_reaches_original_chat_once_and_preserves_drafts_without_private_arguments()
     {
        let channel = Arc::new(RecordingChannel::default());
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let events = [
            native(
                "delegate",
                serde_json::json!({"agent":"coding", "prompt":"private prompt /private/path"}),
            ),
            native(
                "codex_cli",
                serde_json::json!({"command":"private command"}),
            ),
            native(
                "file_edit",
                serde_json::json!({"path":"private/source.rs", "new_string":"private source"}),
            ),
            StreamDelta::Text("draft answer".into()),
        ];
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        let (draft_rx, task) = start(
            rx,
            channel.clone(),
            message(),
            CancellationToken::new(),
            true,
        );
        task.finish().await;
        let sent = channel.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        let notice = &sent[0];
        assert_eq!(notice.recipient, "chat");
        assert_eq!(notice.thread_ts.as_deref(), Some("17"));
        assert_eq!(notice.in_reply_to.as_deref(), Some("41"));
        assert!(notice.suppress_voice && !notice.force_voice && notice.attachments.is_empty());
        assert_eq!(
            notice.content,
            zeroclaw_runtime::i18n::get_required_cli_string("channel-runtime-repair-started")
        );
        assert!(!notice.content.contains("private"));
        let mut drafts = draft_rx.unwrap();
        assert!(
            matches!(drafts.recv().await, Some(StreamDelta::ToolStart { tool, .. }) if tool == "delegate")
        );
        assert!(
            matches!(drafts.recv().await, Some(StreamDelta::ToolStart { tool, .. }) if tool == "codex_cli")
        );
        assert!(
            matches!(drafts.recv().await, Some(StreamDelta::ToolStart { tool, .. }) if tool == "file_edit")
        );
        assert!(
            matches!(drafts.recv().await, Some(StreamDelta::Text(text)) if text == "draft answer")
        );
        assert!(drafts.recv().await.is_none());
    }

    #[tokio::test]
    async fn repair_notices_ignore_read_only_activity_freeform_text_and_extension_impersonation() {
        let channel = Arc::new(RecordingChannel::default());
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut events = vec![
            native("file_read", serde_json::json!({"path":"source.rs"})),
            native("file_write", serde_json::json!({"path":"notes.md"})),
            native(
                "shell",
                serde_json::json!({"command":"codex_cli; repair; write source.rs"}),
            ),
            native(
                "delegate",
                serde_json::json!({"agent":"researcher", "prompt":"coding repair"}),
            ),
            native(
                "delegate",
                serde_json::json!({"agent":"coding", "action":"check_result"}),
            ),
            native(
                "delegate",
                serde_json::json!({"agent":"coding", "parallel":["researcher"]}),
            ),
            native("mcp__server__codex_cli", serde_json::json!({})),
            StreamDelta::Status("codex_cli starting repairs".into()),
            StreamDelta::Text("I will repair the source code".into()),
        ];
        for origin in [None, Some(ToolProvenance::Extension)] {
            for tool in ["codex_cli", "delegate", "file_edit"] {
                events.push(StreamDelta::ToolStart {
                    tool: tool.into(),
                    arguments: Arc::new(serde_json::json!({"agent":"coding", "path":"source.rs"})),
                    tool_provenance: origin,
                });
            }
        }
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        run(
            rx,
            None,
            channel.clone(),
            message(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(channel.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn repair_notice_covers_native_coding_entry_points_and_source_writes() {
        for event in [
            native("codex_cli", serde_json::json!({})),
            native("claude_code", serde_json::json!({})),
            native("claude_code_runner", serde_json::json!({})),
            native("gemini_cli", serde_json::json!({})),
            native("opencode_cli", serde_json::json!({})),
            native("delegate", serde_json::json!({"agent":" coding "})),
            native(
                "delegate",
                serde_json::json!({"parallel":["researcher", "coding"]}),
            ),
            native("file_write", serde_json::json!({"path":"src/main.rs"})),
            native("file_edit", serde_json::json!({"path":"Makefile"})),
        ] {
            let channel = Arc::new(RecordingChannel::default());
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.send(event).await.unwrap();
            drop(tx);
            run(
                rx,
                None,
                channel.clone(),
                message(),
                CancellationToken::new(),
            )
            .await;
            assert_eq!(channel.sent.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn repair_reminders_are_spaced_and_stop_at_finalizing_or_closed_turn() {
        let channel = Arc::new(RecordingChannel::default());
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (_, task) = start(
            rx,
            channel.clone(),
            message(),
            CancellationToken::new(),
            false,
        );
        tx.send(native("codex_cli", serde_json::json!({})))
            .await
            .unwrap();
        channel.changed.notified().await;
        tokio::time::advance(Duration::from_secs(119)).await;
        tokio::task::yield_now().await;
        assert_eq!(channel.attempts.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        channel.changed.notified().await;
        assert_eq!(channel.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            channel.sent.lock().unwrap()[1].content,
            zeroclaw_runtime::i18n::get_required_cli_string("channel-runtime-repair-ongoing")
        );
        tx.send(StreamDelta::Lifecycle(ProgressEvent::FinalizingResponse))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(REMINDER_INTERVAL * 3).await;
        tokio::task::yield_now().await;
        assert_eq!(channel.attempts.load(Ordering::SeqCst), 2);
        drop(tx);
        task.finish().await;
        tokio::time::advance(REMINDER_INTERVAL * 3).await;
        assert_eq!(channel.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn repair_notice_uncertain_delivery_disables_retries_and_reminders() {
        for hang in [false, true] {
            let channel = Arc::new(RecordingChannel {
                fail: true,
                hang,
                ..Default::default()
            });
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let (_, task) = start(
                rx,
                channel.clone(),
                message(),
                CancellationToken::new(),
                false,
            );
            tx.send(native("codex_cli", serde_json::json!({})))
                .await
                .unwrap();
            channel.changed.notified().await;
            tokio::time::advance(SEND_TIMEOUT).await;
            tokio::task::yield_now().await;
            tx.send(native("file_edit", serde_json::json!({"path":"source.rs"})))
                .await
                .unwrap();
            tokio::time::advance(REMINDER_INTERVAL * 3).await;
            tokio::task::yield_now().await;
            assert_eq!(channel.attempts.load(Ordering::SeqCst), 1);
            drop(tx);
            task.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn repair_notice_cancellation_interrupts_hung_send_and_full_draft_queue() {
        for hang in [false, true] {
            let channel = Arc::new(RecordingChannel {
                hang,
                ..Default::default()
            });
            let (tx, rx) = tokio::sync::mpsc::channel(128);
            let token = CancellationToken::new();
            let (_draft_rx, task) = start(rx, channel.clone(), message(), token.clone(), true);
            tx.send(native("codex_cli", serde_json::json!({})))
                .await
                .unwrap();
            for _ in 0..80 {
                tx.send(StreamDelta::Text("queued".into())).await.unwrap();
            }
            channel.changed.notified().await;
            token.cancel();
            let before = Instant::now();
            task.finish().await;
            assert_eq!(
                Instant::now(),
                before,
                "cancellation must not wait for send timeout"
            );
            tokio::time::advance(REMINDER_INTERVAL * 3).await;
            assert_eq!(channel.attempts.load(Ordering::SeqCst), 1);
            assert!(tx.is_closed());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn repair_notice_finish_is_bounded_and_drop_aborts_the_relay() {
        for finish in [true, false] {
            let channel = Arc::new(RecordingChannel::default());
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let (_, task) = start(
                rx,
                channel.clone(),
                message(),
                CancellationToken::new(),
                false,
            );
            tx.send(native("codex_cli", serde_json::json!({})))
                .await
                .unwrap();
            channel.changed.notified().await;
            let before = Instant::now();
            if finish {
                task.finish().await;
            } else {
                drop(task);
                tokio::task::yield_now().await;
            }
            assert!(Instant::now() - before <= SEND_TIMEOUT);
            assert!(
                tx.is_closed(),
                "a retained producer must not keep the notification relay alive"
            );
            tokio::time::advance(REMINDER_INTERVAL * 3).await;
            assert_eq!(channel.attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[cfg(feature = "channel-telegram")]
    #[tokio::test]
    #[ignore = "sends one owner-only Telegram notification; requires explicit repair smoke environment"]
    async fn live_owner_telegram_repair_notice_smoke() {
        let config_dir = std::path::PathBuf::from(
            std::env::var("ZEROCLAW_REPAIR_SMOKE_CONFIG_DIR")
                .expect("explicit smoke config directory required"),
        );
        let alias = std::env::var("ZEROCLAW_REPAIR_SMOKE_ALIAS")
            .expect("explicit smoke channel alias required");
        let owner =
            std::env::var("ZEROCLAW_REPAIR_SMOKE_OWNER").expect("explicit smoke owner required");
        assert!(
            owner.parse::<u64>().ok().is_some_and(|id| id > 0),
            "smoke recipient must be a positive private-chat owner id"
        );
        let raw = std::fs::read_to_string(config_dir.join("config.toml"))
            .map_err(|_| "cannot read smoke config")
            .unwrap();
        // Deserialization is read-only. Do not call load_or_init: a transport
        // smoke must not migrate or rewrite the operator's live configuration.
        let config: zeroclaw_config::schema::Config = toml::from_str(&raw)
            .map_err(|_| "cannot parse smoke config")
            .unwrap();
        let tg = config
            .channels
            .telegram
            .get(&alias)
            .expect("smoke Telegram alias missing");
        assert!(tg.enabled, "smoke Telegram alias must be enabled");
        let peers = config.channel_external_peers("telegram", &alias);
        assert!(
            !peers.is_empty() && peers.iter().all(|peer| peer == &owner),
            "smoke route must allow only the explicit owner"
        );
        assert!(
            tg.bot_token.starts_with("enc2:"),
            "smoke requires existing encrypted token"
        );
        assert!(
            config_dir.join(".secret_key").is_file(),
            "existing secret key required"
        );
        let token = zeroclaw_config::secrets::SecretStore::new(&config_dir, true)
            .decrypt(&tg.bot_token)
            .map_err(|_| "smoke token decryption failed")
            .unwrap();
        struct SmokeChannel {
            inner: crate::telegram::TelegramChannel,
            owner: String,
            sent: AtomicUsize,
        }
        impl Attributable for SmokeChannel {
            fn role(&self) -> Role {
                self.inner.role()
            }
            fn alias(&self) -> &str {
                self.inner.alias()
            }
        }
        #[async_trait::async_trait]
        impl Channel for SmokeChannel {
            fn name(&self) -> &str {
                "telegram"
            }
            async fn send(&self, msg: &SendMessage) -> anyhow::Result<()> {
                anyhow::ensure!(
                    msg.recipient == self.owner && msg.suppress_voice && msg.attachments.is_empty(),
                    "smoke delivery route or mode changed"
                );
                self.inner
                    .send(msg)
                    .await
                    .map_err(|_| anyhow::Error::msg("smoke Telegram send failed"))?;
                self.sent.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn listen(&self, _: zeroclaw_api::inbound::Sender) -> anyhow::Result<()> {
                anyhow::bail!("smoke must never poll Telegram")
            }
        }
        let owner_for_peers = owner.clone();
        let channel = Arc::new(SmokeChannel {
            inner: crate::telegram::TelegramChannel::new(
                token,
                alias.clone(),
                Arc::new(move || vec![owner_for_peers.clone()]),
                false,
            ),
            owner: owner.clone(),
            sent: AtomicUsize::new(0),
        });
        let mut message = ChannelMessage::new(
            "repair-notice-smoke",
            owner.clone(),
            owner,
            "Notification transport smoke test; no code repair requested.",
            "telegram",
            0,
        );
        message.channel_alias = Some(alias);
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(native("codex_cli", serde_json::json!({})))
            .await
            .unwrap();
        drop(tx);
        tokio::time::timeout(
            Duration::from_secs(15),
            run(rx, None, channel.clone(), message, CancellationToken::new()),
        )
        .await
        .expect("repair smoke timed out");
        assert_eq!(
            channel.sent.load(Ordering::SeqCst),
            1,
            "expected one successful owner notification"
        );
    }
}
