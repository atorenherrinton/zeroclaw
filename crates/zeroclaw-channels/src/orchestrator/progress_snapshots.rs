//! One event-derived progress view per active draft, including idle liveness.
//!
//! The runtime/executor events own activity facts. This reducer is only their
//! latest presentation, not another task registry or a source of task status.

use super::{classify_tool_activity, sanitize_streaming_draft_text};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};
use zeroclaw_api::channel::{
    Channel, DraftActivity, DraftSnapshot, DraftUpdateRateLimit, ProgressEvent, ToolProgressEvent,
    ToolProgressPhase,
};
use zeroclaw_runtime::agent::loop_::StreamDelta;

const HEARTBEAT: Duration = Duration::from_secs(30);
const EDIT_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn run(
    channel: Arc<dyn Channel>,
    recipient: String,
    draft_id: String,
    known_tool_names: HashSet<String>,
    mut rx: mpsc::Receiver<StreamDelta>,
    mut tool_rx: Option<watch::Receiver<Option<DraftActivity>>>,
) -> String {
    let started = Instant::now();
    let interval = Duration::from_millis(channel.draft_update_interval_ms().clamp(250, 60_000));
    let mut clock = tokio::time::interval_at(started + interval, interval);
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let executor_events = tool_rx.is_some();
    let mut text = String::new();
    let mut activity = DraftActivity::Lifecycle(ProgressEvent::Received);
    let mut dirty = true;
    let mut last_published = started;
    let mut retry_at = Some(started);
    let mut failures: u32 = 0;

    loop {
        tokio::select! {
            changed = async {
                match tool_rx.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() {
                    tool_rx = None;
                } else if let Some(event) = tool_rx.as_mut().and_then(|rx| *rx.borrow_and_update()) {
                    activity = event;
                    dirty = true;
                }
            }
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    StreamDelta::Text(delta) => {
                        text.push_str(&delta);
                        dirty = true;
                    }
                    StreamDelta::Lifecycle(event) => {
                        // The canonical observer watch includes model requests.
                        // An older queued lifecycle delta must never regress it.
                        if !executor_events {
                            let next = DraftActivity::Lifecycle(event);
                            dirty |= activity != next;
                            activity = next;
                        }
                    }
                    StreamDelta::ToolStart { tool, arguments, .. } if !executor_events => {
                        activity = DraftActivity::Tool(ToolProgressEvent {
                            activity: classify_tool_activity(&tool, &arguments),
                            phase: ToolProgressPhase::Running,
                        });
                        dirty = true;
                    }
                    StreamDelta::ToolComplete { tool, arguments, success, .. } if !executor_events => {
                        activity = DraftActivity::Tool(ToolProgressEvent {
                            activity: classify_tool_activity(&tool, &arguments),
                            phase: if success { ToolProgressPhase::Succeeded } else { ToolProgressPhase::Failed },
                        });
                        dirty = true;
                    }
                    // These may include raw reasoning, commands, paths or outputs.
                    StreamDelta::Status(_) | StreamDelta::Reasoning(_)
                    | StreamDelta::ToolStart { .. } | StreamDelta::ToolComplete { .. } => {}
                }
            }
            _ = clock.tick() => {
                let now = Instant::now();
                if retry_at.is_none_or(|retry| now < retry) || (!dirty && now.duration_since(last_published) < HEARTBEAT) {
                    continue;
                }
                let visible = sanitize_streaming_draft_text(&text, &known_tool_names);
                let snapshot = DraftSnapshot {
                    text: &visible,
                    activity,
                    elapsed_secs: now.duration_since(started).as_secs(),
                };
                let result = tokio::time::timeout(
                    EDIT_TIMEOUT,
                    channel.update_draft_snapshot(&recipient, &draft_id, snapshot),
                ).await;
                let delay = match result {
                    Ok(Ok(())) => {
                        dirty = false;
                        failures = 0;
                        last_published = Instant::now();
                        continue;
                    }
                    Ok(Err(error)) => {
                        failures = failures.saturating_add(1);
                        error.downcast_ref::<DraftUpdateRateLimit>()
                            .map(|limit| Duration::from_secs(limit.retry_after_secs.max(1)))
                            .unwrap_or_else(|| retry_delay(failures))
                    }
                    Err(_) => {
                        failures = failures.saturating_add(1);
                        retry_delay(failures)
                    }
                };
                // Edits are idempotent and retry only while this turn owns the draft.
                // Do not log text, routes, raw API responses, or tool arguments.
                ::zeroclaw_log::record!(WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_attrs(serde_json::json!({"attempt": failures, "retry_after_secs": delay.as_secs()})),
                    "Draft progress edit failed; latest snapshot retained for retry"
                );
                dirty = true;
                // An unrepresentable vendor delay disables further edits for
                // this turn instead of overflowing or retrying prematurely.
                retry_at = Instant::now().checked_add(delay);
            }
        }
    }
    sanitize_streaming_draft_text(&text, &known_tool_names)
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(5)).min(Duration::from_secs(30))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zeroclaw_api::attribution::{Attributable, ChannelKind, Role};
    use zeroclaw_api::channel::{SendMessage, ToolActivity};

    #[derive(Debug, Clone)]
    struct Frame {
        recipient: String,
        text: String,
        activity: DraftActivity,
        elapsed: u64,
    }

    #[derive(Default)]
    struct RecordingChannel {
        frames: Mutex<Vec<Frame>>,
        rate_limit_once: Mutex<Option<u64>>,
    }

    impl Attributable for RecordingChannel {
        fn role(&self) -> Role {
            Role::Channel(ChannelKind::Telegram)
        }
        fn alias(&self) -> &str {
            "progress-fixture"
        }
    }

    #[async_trait::async_trait]
    impl Channel for RecordingChannel {
        fn name(&self) -> &str {
            "telegram"
        }
        async fn send(&self, _: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }
        async fn listen(&self, _: zeroclaw_api::inbound::Sender) -> anyhow::Result<()> {
            Ok(())
        }
        fn supports_progress_snapshots(&self) -> bool {
            true
        }
        async fn update_draft_snapshot(
            &self,
            recipient: &str,
            _: &str,
            view: DraftSnapshot<'_>,
        ) -> anyhow::Result<()> {
            self.frames.lock().unwrap().push(Frame {
                recipient: recipient.to_owned(),
                text: view.text.to_owned(),
                activity: view.activity,
                elapsed: view.elapsed_secs,
            });
            if let Some(seconds) = self.rate_limit_once.lock().unwrap().take() {
                return Err(DraftUpdateRateLimit {
                    retry_after_secs: seconds,
                }
                .into());
            }
            Ok(())
        }
    }

    async fn advance(seconds: u64) {
        // Let the reducer consume events before advancing its transport clock.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(seconds)).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn progress_snapshots_keep_narration_refresh_elapsed_and_stop_on_close() {
        let channel = Arc::new(RecordingChannel::default());
        let (tx, rx) = mpsc::channel(16);
        let (tool_tx, tool_rx) = watch::channel(None);
        let task = tokio::spawn(super::super::run_draft_updater(
            channel.clone(),
            "chat:topic".into(),
            "draft".into(),
            HashSet::new(),
            rx,
            Some(tool_rx),
        ));
        tx.send(StreamDelta::Text(
            "I found the cause. Testing the repair.".into(),
        ))
        .await
        .unwrap();
        tx.send(StreamDelta::Status("PRIVATE STATUS".into()))
            .await
            .unwrap();
        tx.send(StreamDelta::Reasoning("PRIVATE REASONING".into()))
            .await
            .unwrap();
        tool_tx.send_replace(Some(DraftActivity::Tool(ToolProgressEvent {
            activity: ToolActivity::CommandLine,
            phase: ToolProgressPhase::Running,
        })));
        advance(1).await;
        assert_eq!(channel.frames.lock().unwrap().len(), 1);
        advance(30).await;
        let frames = channel.frames.lock().unwrap().clone();
        assert_eq!(
            frames.len(),
            2,
            "long tool wait must refresh without another event"
        );
        assert_eq!(frames[1].text, "I found the cause. Testing the repair.");
        assert_eq!(frames[1].recipient, "chat:topic");
        assert_eq!(
            frames[1].activity,
            DraftActivity::Tool(ToolProgressEvent {
                activity: ToolActivity::CommandLine,
                phase: ToolProgressPhase::Running
            })
        );
        assert!(frames[1].elapsed >= 30);
        drop(tx);
        assert_eq!(
            task.await.unwrap(),
            "I found the cause. Testing the repair."
        );
        tool_tx.send_replace(Some(DraftActivity::Tool(ToolProgressEvent {
            activity: ToolActivity::Files,
            phase: ToolProgressPhase::Succeeded,
        })));
        advance(60).await;
        assert_eq!(
            channel.frames.lock().unwrap().len(),
            2,
            "no edits after the turn releases its draft"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progress_snapshots_coalesce_and_retry_latest_after_rate_limit() {
        let channel = Arc::new(RecordingChannel::default());
        *channel.rate_limit_once.lock().unwrap() = Some(10);
        let (tx, rx) = mpsc::channel(16);
        let task = tokio::spawn(super::super::run_draft_updater(
            channel.clone(),
            "chat:topic".into(),
            "draft".into(),
            HashSet::new(),
            rx,
            None,
        ));
        tx.send(StreamDelta::Lifecycle(ProgressEvent::Received))
            .await
            .unwrap();
        tx.send(StreamDelta::Lifecycle(ProgressEvent::Planning))
            .await
            .unwrap();
        tx.send(StreamDelta::Lifecycle(ProgressEvent::WaitingOnModel))
            .await
            .unwrap();
        advance(1).await;
        assert_eq!(channel.frames.lock().unwrap().len(), 1);
        tx.send(StreamDelta::Text("Latest result".into()))
            .await
            .unwrap();
        advance(9).await;
        assert_eq!(
            channel.frames.lock().unwrap().len(),
            1,
            "honor transport retry-after"
        );
        advance(1).await;
        let frames = channel.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].text, "Latest result");
        assert_eq!(
            frames[1].activity,
            DraftActivity::Lifecycle(ProgressEvent::WaitingOnModel)
        );
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn progress_snapshots_do_not_regress_observed_activity_from_queued_lifecycle() {
        let channel = Arc::new(RecordingChannel::default());
        let (tx, rx) = mpsc::channel(16);
        let (activity_tx, activity_rx) = watch::channel(None);
        let task = tokio::spawn(super::super::run_draft_updater(
            channel.clone(),
            "chat:topic".into(),
            "draft".into(),
            HashSet::new(),
            rx,
            Some(activity_rx),
        ));
        let running = DraftActivity::Tool(ToolProgressEvent {
            activity: ToolActivity::CommandLine,
            phase: ToolProgressPhase::Running,
        });
        activity_tx.send_replace(Some(running));
        tx.send(StreamDelta::Lifecycle(ProgressEvent::WaitingOnModel))
            .await
            .unwrap();
        advance(1).await;
        assert_eq!(
            channel.frames.lock().unwrap().last().unwrap().activity,
            running
        );
        activity_tx.send_replace(Some(DraftActivity::Lifecycle(
            ProgressEvent::WaitingOnModel,
        )));
        tx.send(StreamDelta::Lifecycle(ProgressEvent::RunningTool))
            .await
            .unwrap();
        advance(1).await;
        assert_eq!(
            channel.frames.lock().unwrap().last().unwrap().activity,
            DraftActivity::Lifecycle(ProgressEvent::WaitingOnModel)
        );
        drop(tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn progress_snapshots_honor_long_or_unrepresentable_vendor_backoff() {
        for delay in [300, u64::MAX] {
            let channel = Arc::new(RecordingChannel::default());
            *channel.rate_limit_once.lock().unwrap() = Some(delay);
            let (tx, rx) = mpsc::channel(16);
            let task = tokio::spawn(super::super::run_draft_updater(
                channel.clone(),
                "chat:topic".into(),
                "draft".into(),
                HashSet::new(),
                rx,
                None,
            ));
            advance(1).await;
            assert_eq!(channel.frames.lock().unwrap().len(), 1);
            advance(150).await;
            assert_eq!(channel.frames.lock().unwrap().len(), 1);
            drop(tx);
            task.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_snapshots_sanitize_split_protocol_before_transport() {
        let channel = Arc::new(RecordingChannel::default());
        let (tx, rx) = mpsc::channel(16);
        let task = tokio::spawn(super::super::run_draft_updater(
            channel.clone(),
            "chat:topic".into(),
            "draft".into(),
            HashSet::new(),
            rx,
            None,
        ));
        tx.send(StreamDelta::Text("Checking. <tool_res".into()))
            .await
            .unwrap();
        advance(1).await;
        tx.send(StreamDelta::Text(
            "ult>PRIVATE PAYLOAD</tool_result> Done.".into(),
        ))
        .await
        .unwrap();
        advance(1).await;
        let frames = channel.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 2);
        for frame in frames {
            assert!(!frame.text.contains("PRIVATE") && !frame.text.contains("tool_res"));
        }
        drop(tx);
        assert!(task.await.unwrap().ends_with("Done."));
    }
}
