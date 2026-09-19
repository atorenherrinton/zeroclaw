//! Ephemeral coordination context supplied by the owner of concurrent turns.
//!
//! A live source is queried for each provider request. It is never added to
//! durable history or presented as a new owner instruction.

use crate::model_provider::ChatMessage;
use std::sync::Arc;

pub trait PeerActivitySource: Send + Sync {
    fn context(&self) -> Option<String>;
}

tokio::task_local! {
    pub static SOURCE: Option<Arc<dyn PeerActivitySource>>;
}

pub fn current() -> Option<Arc<dyn PeerActivitySource>> {
    SOURCE.try_with(Clone::clone).ok().flatten()
}

/// Add the current snapshot only to the transient provider request. The
/// caller must pass its request copy, not its canonical conversation history.
pub fn append_to_request(messages: &mut Vec<ChatMessage>) {
    let context = SOURCE
        .try_with(|source| source.as_ref().and_then(|source| source.context()))
        .ok()
        .flatten();
    let Some(context) = context else { return };
    if let Some(system) = messages.iter_mut().find(|message| message.role == "system") {
        system.content.push_str("\n\n");
        system.content.push_str(&context);
    } else {
        messages.insert(0, ChatMessage::system(context));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Live(Mutex<Option<String>>);
    impl PeerActivitySource for Live {
        fn context(&self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn live_context_refreshes_without_changing_history_or_owner_message() {
        let source = Arc::new(Live(Mutex::new(Some("peer is checking files".into()))));
        let history = vec![ChatMessage::system("policy"), ChatMessage::user("my task")];
        SOURCE
            .scope(Some(source.clone()), async {
                let mut first = history.clone();
                append_to_request(&mut first);
                assert!(first[0].content.contains("peer is checking files"));
                *source.0.lock().unwrap() = Some("peer has finished".into());
                let mut second = history.clone();
                append_to_request(&mut second);
                assert!(second[0].content.contains("peer has finished"));
                assert!(!second[0].content.contains("peer is checking files"));
                assert_eq!(second[1].content, "my task");
                assert_eq!(history[0].content, "policy");
            })
            .await;
        let mut outside = history.clone();
        append_to_request(&mut outside);
        assert_eq!(outside[0].content, "policy");
    }
}
