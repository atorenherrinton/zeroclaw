//! Turn-scoped forwarding of content-free delegate execution activity.
//!
//! This is a presentation sink, not the parent's execution observer or journal.
//! Spawned delegates may capture it, but cannot keep its observer alive after
//! the owning channel turn completes or is cancelled.

use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use parking_lot::RwLock;
use std::future::Future;
use std::sync::Arc;

type Sink = Arc<RwLock<Option<Arc<dyn Observer>>>>;

tokio::task_local! {
    static SINK: Option<Sink>;
}

struct ScopeOwner(Sink);

impl Drop for ScopeOwner {
    fn drop(&mut self) {
        // Clear even when the parent future is dropped. Background tasks only
        // retain an empty slot, not a stale channel sender or observer.
        self.0.write().take();
    }
}

/// Forward delegate execution activity to a presentation-only observer while this
/// future is alive. Never pass an observer that changes task state or metrics.
pub async fn scope<F: Future>(observer: Arc<dyn Observer>, future: F) -> F::Output {
    let sink = Arc::new(RwLock::new(Some(observer)));
    let _owner = ScopeOwner(sink.clone());
    SINK.scope(Some(sink), future).await
}

/// Capture the current sink now, before the returned future is spawned.
pub(super) fn inherit<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let sink = SINK.try_with(Clone::clone).ok().flatten();
    async move { SINK.scope(sink, future).await }
}

pub(super) fn observer() -> impl Observer {
    ProgressObserver(SINK.try_with(Clone::clone).ok().flatten())
}

struct ProgressObserver(Option<Sink>);

impl Observer for ProgressObserver {
    fn record_event(&self, event: &ObserverEvent) {
        let Some(sink) = self.0.as_ref() else {
            return;
        };
        // Reconstruct the allowlisted variants rather than cloning: arguments,
        // output, agent identities, correlation IDs, and routes never cross
        // into the parent's progress sink. No model text/reasoning is forwarded.
        let event = match event {
            ObserverEvent::LlmRequest { .. } => ObserverEvent::LlmRequest {
                model_provider: String::new(),
                model: String::new(),
                messages_count: 0,
                channel: None,
                agent_alias: None,
                parent_agent_alias: None,
                turn_id: None,
            },
            ObserverEvent::ToolCallStart { tool, .. } => ObserverEvent::ToolCallStart {
                tool: tool.clone(),
                tool_call_id: None,
                arguments: None,
                channel: None,
                agent_alias: None,
                parent_agent_alias: None,
                turn_id: None,
            },
            ObserverEvent::ToolCall {
                tool,
                duration,
                success,
                ..
            } => ObserverEvent::ToolCall {
                tool: tool.clone(),
                tool_call_id: None,
                duration: *duration,
                success: *success,
                arguments: None,
                result: None,
                channel: None,
                agent_alias: None,
                parent_agent_alias: None,
                turn_id: None,
            },
            _ => return,
        };
        // Hold the read guard through delivery so clearing the slot establishes
        // a strict boundary: no event is delivered after ScopeOwner has dropped.
        if let Some(observer) = sink.read().as_ref() {
            observer.record_event(&event);
        }
    }

    fn record_metric(&self, _: &ObserverMetric) {}

    fn name(&self) -> &str {
        "delegate-tool-progress"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingObserver(parking_lot::Mutex<Vec<ObserverEvent>>);

    impl Observer for RecordingObserver {
        fn record_event(&self, event: &ObserverEvent) {
            self.0.lock().push(event.clone());
        }
        fn record_metric(&self, _: &ObserverMetric) {
            panic!("delegate metrics must not reach the presentation sink")
        }
        fn name(&self) -> &str {
            "recording"
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn start() -> ObserverEvent {
        ObserverEvent::ToolCallStart {
            tool: "shell".into(),
            tool_call_id: Some("private-call".into()),
            arguments: Some("private-arguments".into()),
            channel: Some("private-route".into()),
            agent_alias: Some("private-agent".into()),
            parent_agent_alias: Some("private-parent".into()),
            turn_id: Some("private-turn".into()),
        }
    }

    #[tokio::test]
    async fn delegate_progress_forwards_only_sanitized_execution_activity() {
        let target = Arc::new(RecordingObserver::default());
        scope(target.clone(), async {
            let observer = observer();
            observer.record_event(&start());
            observer.record_event(&ObserverEvent::ToolCall {
                tool: "shell".into(),
                tool_call_id: Some("private-call".into()),
                duration: Duration::from_millis(42),
                success: true,
                arguments: Some("private-arguments".into()),
                result: Some("private-result".into()),
                channel: Some("private-route".into()),
                agent_alias: Some("private-agent".into()),
                parent_agent_alias: Some("private-parent".into()),
                turn_id: Some("private-turn".into()),
            });
            observer.record_event(&ObserverEvent::AgentStart {
                model_provider: "private-provider".into(),
                model: "private-model".into(),
                channel: None,
                agent_alias: None,
                turn_id: None,
            });
            observer.record_event(&ObserverEvent::LlmRequest {
                model_provider: "private-provider".into(),
                model: "private-model".into(),
                messages_count: 42,
                channel: Some("private-route".into()),
                agent_alias: Some("private-agent".into()),
                parent_agent_alias: Some("private-parent".into()),
                turn_id: Some("private-turn".into()),
            });
            observer.record_event(&ObserverEvent::LlmResponse {
                model_provider: "private-provider".into(),
                model: "private-model".into(),
                duration: Duration::from_secs(1),
                success: true,
                error_message: None,
                input_tokens: Some(42),
                output_tokens: Some(12),
                messages: Some(zeroclaw_api::observability_traits::LlmMessageSnapshot {
                    input: vec![],
                    output_text: Some("private-model-output".into()),
                    output_tool_calls: vec![],
                    system_instructions: Some("private-system-instructions".into()),
                }),
                channel: None,
                agent_alias: None,
                parent_agent_alias: None,
                turn_id: None,
            });
        })
        .await;
        let events = target.0.lock();
        assert_eq!(events.len(), 3);
        assert!(!format!("{events:?}").contains("private-"));
        assert!(
            matches!(&events[1], ObserverEvent::ToolCall { duration, success: true, .. } if *duration == Duration::from_millis(42))
        );
        assert!(matches!(
            &events[2],
            ObserverEvent::LlmRequest { model_provider, model, messages_count: 0, .. }
                if model_provider.is_empty() && model.is_empty()
        ));
    }

    #[tokio::test]
    async fn delegate_progress_inherits_at_spawn_and_expires_with_parent() {
        let target = Arc::new(RecordingObserver::default());
        let escaped = scope(target.clone(), async {
            let child = inherit(async {
                let observer = observer();
                observer.record_event(&start());
                observer
            });
            let observer = zeroclaw_spawn::spawn!(child).await.unwrap();
            assert_eq!(target.0.lock().len(), 1);
            observer
        })
        .await;
        escaped.record_event(&start());
        assert_eq!(target.0.lock().len(), 1);
        assert_eq!(Arc::strong_count(&target), 1);
        observer().record_event(&start());
        assert_eq!(target.0.lock().len(), 1);
    }

    #[tokio::test]
    async fn delegate_progress_parallel_children_keep_their_own_parent_scope() {
        let first = Arc::new(RecordingObserver::default());
        let second = Arc::new(RecordingObserver::default());
        scope(first.clone(), async {
            let first_child = inherit(async { observer() });
            let (first_child, second_child) = scope(second.clone(), async {
                let second_child = inherit(async { observer() });
                let first_task = zeroclaw_spawn::spawn!(first_child);
                let second_task = zeroclaw_spawn::spawn!(second_child);
                let (first_child, second_child) = tokio::join!(first_task, second_task);
                let first_child = first_child.unwrap();
                let second_child = second_child.unwrap();
                first_child.record_event(&start());
                second_child.record_event(&start());
                assert_eq!(first.0.lock().len(), 1);
                assert_eq!(second.0.lock().len(), 1);
                (first_child, second_child)
            })
            .await;
            // Finishing one channel scope cannot silence another active parent,
            // or redirect the finished child's events to that parent's sink.
            first_child.record_event(&start());
            second_child.record_event(&start());
            assert_eq!(first.0.lock().len(), 2);
            assert_eq!(second.0.lock().len(), 1);
            assert_eq!(Arc::strong_count(&second), 1);
        })
        .await;
        assert_eq!(Arc::strong_count(&first), 1);
    }

    #[tokio::test]
    async fn delegate_progress_cancelled_parent_drops_observer_and_disables_child() {
        let target = Arc::new(RecordingObserver::default());
        let (send, receive) = tokio::sync::oneshot::channel();
        let parent = scope(target.clone(), async move {
            let child = inherit(async { observer() });
            let escaped = zeroclaw_spawn::spawn!(child).await.unwrap();
            send.send(escaped)
                .unwrap_or_else(|_| panic!("receiver dropped"));
            std::future::pending::<()>().await;
        });
        let parent = zeroclaw_spawn::spawn!(parent);
        let escaped = receive.await.unwrap();
        escaped.record_event(&start());
        assert_eq!(target.0.lock().len(), 1);
        parent.abort();
        assert!(parent.await.unwrap_err().is_cancelled());
        escaped.record_event(&start());
        assert_eq!(target.0.lock().len(), 1);
        assert_eq!(Arc::strong_count(&target), 1);
    }
}
