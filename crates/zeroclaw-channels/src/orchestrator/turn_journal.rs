//! Durable supervision for ordinary channel turns. Control-plane rows own input,
//! phase and terminal evidence. This adapter never authorizes automatic replay.
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use zeroclaw_api::{
    channel::ChannelMessage,
    turn::{TaskStatus, TurnJournal},
};
use zeroclaw_runtime::control_plane::{ControlPlaneHandle, TaskKind, TaskRecord};

pub(super) struct ChannelTurnJournal {
    plane: ControlPlaneHandle,
    id: String,
    terminal: AtomicBool,
}

fn turn_id(msg: &ChannelMessage) -> anyhow::Result<String> {
    anyhow::ensure!(!msg.id.is_empty(), "durable turn requires inbound identity");
    let route = zeroclaw_api::conversation::ConversationRoute::from_message(msg);
    Ok(format!(
        "turn-{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &route.channel,
            &route.recipient,
            &route.thread,
            &msg.id
        ))?)
    ))
}

/// Decode only the trusted, bounded transport checkpoint. Attachments and
/// internal events require their original producer and are never reconstructed.
pub(super) fn restore_message(id: &str, input: &str) -> anyhow::Result<ChannelMessage> {
    let v: serde_json::Value = serde_json::from_str(input)?;
    anyhow::ensure!(
        v["version"] == 2
            && v["internal_event"] == false
            && v["attachments"].as_array().is_some_and(Vec::is_empty),
        "queued input requires operator recovery"
    );
    let required = |key: &str| -> anyhow::Result<String> {
        Ok(v[key]
            .as_str()
            .ok_or_else(|| anyhow::Error::msg("invalid queued transport checkpoint"))?
            .to_owned())
    };
    let optional = |key: &str| v[key].as_str().map(str::to_owned);
    let msg = ChannelMessage {
        id: required("inbound_id")?,
        content: required("content")?,
        channel: required("channel")?,
        channel_alias: optional("channel_alias"),
        sender: required("sender")?,
        reply_target: required("reply_target")?,
        thread_ts: optional("thread_ts"),
        interruption_scope_id: optional("interruption_scope_id"),
        timestamp: v["timestamp"]
            .as_u64()
            .ok_or_else(|| anyhow::Error::msg("invalid queued timestamp"))?,
        explicitly_addressed: v["explicitly_addressed"].as_bool().unwrap_or(false),
        conversation_scope: if v["room_scope"] == true {
            zeroclaw_api::channel::ChannelConversationScope::ReplyTarget
        } else {
            zeroclaw_api::channel::ChannelConversationScope::Sender
        },
        subject: optional("subject"),
        references: serde_json::from_value(v["references"].clone())?,
        ..Default::default()
    };
    anyhow::ensure!(turn_id(&msg)? == id, "queued input identity mismatch");
    Ok(msg)
}

pub(super) struct ChannelAdmission(pub(super) ControlPlaneHandle);
#[async_trait::async_trait]
impl zeroclaw_api::inbound::Admission for ChannelAdmission {
    async fn admit(&self, msg: &ChannelMessage) -> anyhow::Result<bool> {
        if msg.passive_context {
            return Ok(true);
        }
        Ok(ChannelTurnJournal::admit(&self.0, "", msg).await?.is_some())
    }
}

/// Covers hard task aborts and panics, where the async worker epilogue cannot run.
/// This only schedules bounded local persistence; it never performs external work.
pub(super) struct WorkerCheckpointGuard(pub(super) Option<Arc<ChannelTurnJournal>>);
impl Drop for WorkerCheckpointGuard {
    fn drop(&mut self) {
        let Some(journal) = self
            .0
            .take()
            .filter(|j| !j.terminal.load(Ordering::Acquire))
        else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let cleanup = async {
                if let Some(record) = journal.plane.store.get(&journal.id).await? {
                    if record.status.is_terminal() {
                        return Ok::<(), anyhow::Error>(());
                    }
                    let state =
                        if matches!(record.status, TaskStatus::Received | TaskStatus::Queued) {
                            TaskStatus::Failed
                        } else {
                            TaskStatus::Uncertain
                        };
                    journal.checkpoint(state, None, false).await?;
                }
                Ok(())
            };
            if !matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), cleanup).await,
                Ok(Ok(()))
            ) {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_attrs(serde_json::json!({"trace_id":journal.id})),
                    "Worker abort checkpoint failed; restart recovery retains responsibility"
                );
            }
        });
    }
}

impl ChannelTurnJournal {
    pub(super) async fn admit(
        plane: &ControlPlaneHandle,
        agent: &str,
        msg: &ChannelMessage,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        anyhow::ensure!(
            !msg.id.is_empty()
                && msg.id.len() <= 512
                && msg.content.len() <= 768 * 1024
                && msg.sender.len() <= 512
                && msg.reply_target.len() <= 512
                && msg.references.len() <= 64
                && msg.references.iter().all(|r| r.len() <= 512)
                && msg.attachments.len() <= 32
                && msg.attachments.iter().all(|a| a.file_name.len() <= 512),
            "inbound checkpoint exceeds privacy/storage bounds"
        );
        let route = zeroclaw_api::conversation::ConversationRoute::from_message(msg);
        let id = turn_id(msg)?;
        // Persist the transport evidence, not an executable deserialization of
        // internal SOP markers. Recovery requires revalidation of current policy.
        let input = serde_json::to_string(&serde_json::json!({
            "version":2, "route":route,
            "channel":msg.channel,"channel_alias":msg.channel_alias,"sender":msg.sender,"reply_target":msg.reply_target,
            "thread_ts":msg.thread_ts,"interruption_scope_id":msg.interruption_scope_id,
            "explicitly_addressed":msg.explicitly_addressed,
            "room_scope":msg.conversation_scope == zeroclaw_api::channel::ChannelConversationScope::ReplyTarget,
            "internal_event":msg.internal_sop_event.is_some(), "inbound_id":msg.id, "content":msg.content,
            "timestamp":msg.timestamp, "attachments":msg.attachments.iter().map(|a| serde_json::json!({"name":a.file_name,"mime_type":a.mime_type,"bytes":a.data.len()})).collect::<Vec<_>>(),
            "subject":msg.subject, "references":msg.references,
        }))?;
        let admitted = plane
            .store
            .admit_channel_turn(
                TaskRecord {
                    id: id.clone(),
                    kind: TaskKind::ChannelTurn,
                    agent: agent.to_owned(),
                    status: TaskStatus::Received,
                    owner_pid: std::process::id(),
                    owner_boot_id: plane.boot_id.clone(),
                    heartbeat_at: None,
                    depth: 0,
                    parent_id: None,
                    originator_route: Some(serde_json::to_string(&route)?),
                    delivered: false,
                    idem_key: Some(id.clone()),
                    principal_id: None,
                    started_at: chrono::Utc::now().to_rfc3339(),
                    finished_at: None,
                },
                input,
            )
            .await?;
        Ok(admitted.then(|| {
            Arc::new(Self {
                plane: plane.clone(),
                id,
                terminal: AtomicBool::new(false),
            })
        }))
    }

    /// Attach only to a received row from this boot. Duplicate transport sends
    /// never reach this method because the admission sender suppresses them.
    pub(super) async fn received(
        plane: &ControlPlaneHandle,
        msg: &ChannelMessage,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let id = turn_id(msg)?;
        match plane.store.get(&id).await? {
            Some(record)
                if record.status == TaskStatus::Received
                    && record.owner_boot_id == plane.boot_id =>
            {
                Ok(Some(Arc::new(Self {
                    plane: plane.clone(),
                    id,
                    terminal: AtomicBool::new(false),
                })))
            }
            Some(_) => Ok(None),
            None => Self::admit(plane, "", msg).await,
        }
    }

    pub(super) async fn assign(&self, agent: &str) -> anyhow::Result<()> {
        self.plane
            .store
            .assign_channel_turn(&self.id, agent, &self.plane.boot_id)
            .await
    }

    pub(super) async fn finish_if_unresolved(&self) -> anyhow::Result<()> {
        if !self.terminal.load(Ordering::Acquire) {
            self.checkpoint(TaskStatus::Uncertain, None, false).await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl TurnJournal for ChannelTurnJournal {
    fn trace_id(&self) -> Option<&str> {
        Some(&self.id)
    }
    async fn record_error(&self, error: String) -> anyhow::Result<()> {
        self.plane.store.record_channel_error(&self.id, error).await
    }

    async fn checkpoint(
        &self,
        status: TaskStatus,
        output: Option<String>,
        delivered: bool,
    ) -> anyhow::Result<()> {
        self.plane
            .store
            .checkpoint_channel_turn(&self.id, status, output, delivered)
            .await?;
        if status.is_terminal() {
            self.terminal.store(true, Ordering::Release);
        }
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                serde_json::json!({"trace_id":self.id,"turn_state":status,"delivered":delivered})
            ),
            "Durable channel turn checkpoint"
        );
        Ok(())
    }
}

/// Failure to persist supervision is not permission to execute or deliver.
pub(super) async fn checkpoint(
    status: TaskStatus,
    output: Option<String>,
    delivered: bool,
) -> bool {
    match zeroclaw_api::turn::checkpoint(status, output, delivered).await {
        Ok(()) => true,
        Err(error) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_attrs(serde_json::json!({"error":error.to_string()})),
                "Channel turn checkpoint failed; refusing further effects"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn mark_fixture_owner_exited(dir: &std::path::Path) {
        let db = rusqlite::Connection::open(dir.join("control_plane.db")).unwrap();
        db.execute("UPDATE tasks SET owner_pid=0 WHERE owner_boot_id='old'", [])
            .unwrap();
    }

    #[tokio::test]
    async fn hard_worker_abort_checkpoints_without_external_replay() {
        for running in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let plane = ControlPlaneHandle::start_with_boot_id(dir.path(), "fixture".into())
                .await
                .unwrap();
            let msg = ChannelMessage {
                id: "aborted".into(),
                ..Default::default()
            };
            let journal = ChannelTurnJournal::admit(&plane, "fixture", &msg)
                .await
                .unwrap()
                .unwrap();
            let id = journal.id.clone();
            journal
                .checkpoint(TaskStatus::Queued, None, false)
                .await
                .unwrap();
            if running {
                journal
                    .checkpoint(TaskStatus::Running, None, false)
                    .await
                    .unwrap();
            }
            let started = Arc::new(tokio::sync::Notify::new());
            let signal = started.clone();
            let task = zeroclaw_spawn::spawn!(async move {
                let _guard = WorkerCheckpointGuard(Some(journal));
                signal.notify_one();
                std::future::pending::<()>().await;
            });
            started.notified().await;
            task.abort();
            let _ = task.await;
            let state = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let state = plane.store.get(&id).await.unwrap().unwrap().status;
                    if state.is_terminal() {
                        break state;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                state,
                if running {
                    TaskStatus::Uncertain
                } else {
                    TaskStatus::Failed
                }
            );
        }
    }

    #[tokio::test]
    async fn ingress_ack_follows_persistence_and_duplicates_never_enqueue() {
        let dir = tempfile::tempdir().unwrap();
        let plane = ControlPlaneHandle::start_with_boot_id(dir.path(), "fixture".into())
            .await
            .unwrap();
        let (tx, mut rx) = zeroclaw_api::inbound::channel(2);
        let tx = tx.with_admission(Arc::new(ChannelAdmission(plane.clone())));
        let msg = ChannelMessage {
            id: "1".into(),
            channel: "telegram".into(),
            channel_alias: Some("fixture".into()),
            reply_target: "room".into(),
            sender: "sender".into(),
            content: "private fixture".into(),
            ..Default::default()
        };
        let id = turn_id(&msg).unwrap();
        let (one, two) = tokio::join!(tx.send(msg.clone()), tx.send(msg.clone()));
        one.unwrap();
        two.unwrap();
        assert_eq!(
            plane.store.get(&id).await.unwrap().unwrap().status,
            TaskStatus::Received
        );
        assert!(
            plane
                .store
                .channel_turn_input(&id)
                .await
                .unwrap()
                .unwrap()
                .contains("private fixture")
        );
        assert_eq!(rx.recv().await.unwrap().id, "1");
        assert!(rx.try_recv().is_err());
        assert!(
            ChannelTurnJournal::admit(&plane, "different-agent", &msg)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancellation_after_admission_is_durable_and_never_replayed() {
        struct CommitThenWait {
            inner: ChannelAdmission,
            committed: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl zeroclaw_api::inbound::Admission for CommitThenWait {
            async fn admit(&self, msg: &ChannelMessage) -> anyhow::Result<bool> {
                zeroclaw_api::inbound::Admission::admit(&self.inner, msg).await?;
                self.committed.notify_one();
                std::future::pending().await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let plane = ControlPlaneHandle::start_with_boot_id(dir.path(), "old".into())
            .await
            .unwrap();
        let committed = Arc::new(tokio::sync::Notify::new());
        let (tx, mut rx) = zeroclaw_api::inbound::channel(1);
        let tx = tx.with_admission(Arc::new(CommitThenWait {
            inner: ChannelAdmission(plane),
            committed: committed.clone(),
        }));
        let msg = ChannelMessage {
            id: "cancelled".into(),
            channel: "fixture".into(),
            ..Default::default()
        };
        let id = turn_id(&msg).unwrap();
        let task = zeroclaw_spawn::spawn!(async move { tx.send(msg).await });
        committed.notified().await;
        task.abort();
        let _ = task.await;
        assert!(rx.recv().await.is_none());
        mark_fixture_owner_exited(dir.path());
        let next = ControlPlaneHandle::start_with_boot_id(dir.path(), "new".into())
            .await
            .unwrap();
        assert_eq!(
            next.store.get(&id).await.unwrap().unwrap().status,
            TaskStatus::Uncertain
        );
        assert!(
            next.store
                .take_recoverable_channel_turns("new")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn queued_restart_retains_fifo_and_rejects_unsafe_transport_reconstruction() {
        let dir = tempfile::tempdir().unwrap();
        let plane = ControlPlaneHandle::start_with_boot_id(dir.path(), "old".into())
            .await
            .unwrap();
        let mut ids = Vec::new();
        for id in ["first", "second", "third"] {
            let msg = ChannelMessage {
                id: id.into(),
                channel: "fixture".into(),
                content: id.into(),
                ..Default::default()
            };
            let journal = ChannelTurnJournal::admit(&plane, "fixture", &msg)
                .await
                .unwrap()
                .unwrap();
            journal
                .checkpoint(TaskStatus::Queued, None, false)
                .await
                .unwrap();
            ids.push(journal.id.clone());
        }
        mark_fixture_owner_exited(dir.path());
        let next = ControlPlaneHandle::start_with_boot_id(dir.path(), "new".into())
            .await
            .unwrap();
        let recovered = next
            .store
            .take_recoverable_channel_turns("new")
            .await
            .unwrap();
        assert_eq!(
            recovered
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        for (id, input) in recovered {
            let msg = restore_message(&id, &input).unwrap();
            assert!(
                ChannelTurnJournal::received(&next, &msg)
                    .await
                    .unwrap()
                    .is_some()
            );
            let mut unsafe_input: serde_json::Value = serde_json::from_str(&input).unwrap();
            unsafe_input["internal_event"] = true.into();
            assert!(restore_message(&id, &unsafe_input.to_string()).is_err());
        }
        assert!(
            next.store
                .take_recoverable_channel_turns("new")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn duplicate_admission_and_restart_keep_input_without_replay() {
        let dir = tempfile::tempdir().unwrap();
        let plane = ControlPlaneHandle::start_with_boot_id(dir.path(), "old".into())
            .await
            .unwrap();
        let msg = ChannelMessage {
            id: "fixture-1".into(),
            channel: "telegram".into(),
            sender: "fixture".into(),
            reply_target: "room".into(),
            content: "sanitized input".into(),
            ..Default::default()
        };
        let journal = ChannelTurnJournal::admit(&plane, "main", &msg)
            .await
            .unwrap()
            .unwrap();
        assert!(
            ChannelTurnJournal::admit(&plane, "main", &msg)
                .await
                .unwrap()
                .is_none()
        );
        journal
            .checkpoint(TaskStatus::Queued, None, false)
            .await
            .unwrap();
        // A queued turn has not executed; it remains safely pending until the
        // current adapter revalidates the original sender and route.
        mark_fixture_owner_exited(dir.path());
        let next = ControlPlaneHandle::start_with_boot_id(dir.path(), "new".into())
            .await
            .unwrap();
        assert_eq!(
            next.store.get(&journal.id).await.unwrap().unwrap().status,
            TaskStatus::Queued
        );
        assert!(
            next.store
                .channel_turn_input(&journal.id)
                .await
                .unwrap()
                .unwrap()
                .contains("sanitized input")
        );
        assert!(
            ChannelTurnJournal::admit(&next, "main", &msg)
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn crash_in_each_active_phase_is_uncertain_and_terminal_cannot_be_overwritten() {
        for phase in [
            TaskStatus::Running,
            TaskStatus::WaitingOnTool,
            TaskStatus::ResponseReady,
            TaskStatus::Submitting,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let plane = ControlPlaneHandle::start_with_boot_id(dir.path(), "old".into())
                .await
                .unwrap();
            let msg = ChannelMessage {
                id: "fixture".into(),
                ..Default::default()
            };
            let journal = ChannelTurnJournal::admit(&plane, "main", &msg)
                .await
                .unwrap()
                .unwrap();
            journal
                .checkpoint(TaskStatus::Queued, None, false)
                .await
                .unwrap();
            journal
                .checkpoint(TaskStatus::Running, None, false)
                .await
                .unwrap();
            if phase == TaskStatus::Submitting {
                journal
                    .checkpoint(TaskStatus::ResponseReady, Some("response".into()), false)
                    .await
                    .unwrap();
            }
            if phase != TaskStatus::Running {
                journal
                    .checkpoint(phase, Some("partial result".into()), false)
                    .await
                    .unwrap();
            }
            mark_fixture_owner_exited(dir.path());
            let next = ControlPlaneHandle::start_with_boot_id(dir.path(), "new".into())
                .await
                .unwrap();
            assert_eq!(
                next.store.get(&journal.id).await.unwrap().unwrap().status,
                TaskStatus::Uncertain
            );
            assert!(
                journal
                    .checkpoint(TaskStatus::Delivered, None, true)
                    .await
                    .is_err()
            );
        }
    }
}
