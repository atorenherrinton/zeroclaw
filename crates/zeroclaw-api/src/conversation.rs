//! Inbound channel messages own routing. A task-local carries that route through
//! a turn; pending questions are matched by channel instance, room, sender and
//! thread by the existing ingress listener, never by a competing listener.
use crate::channel::{ChannelMessage, SendMessage};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationRoute {
    pub channel: String,
    pub recipient: String,
    pub sender: String,
    pub thread: Option<String>,
    pub reply_to: String,
}
impl ConversationRoute {
    pub fn from_message(msg: &ChannelMessage) -> Self {
        Self {
            channel: msg
                .channel_alias
                .as_ref()
                .map_or_else(|| msg.channel.clone(), |a| format!("{}.{a}", msg.channel)),
            recipient: msg.reply_target.clone(),
            sender: msg.sender.clone(),
            thread: msg.thread_ts.clone(),
            reply_to: if msg.channel == "telegram" {
                msg.id.rsplit('_').next().unwrap_or(&msg.id).to_owned()
            } else {
                msg.id.clone()
            },
        }
    }
    pub fn message(&self, text: impl Into<String>) -> SendMessage {
        SendMessage::new(text, &self.recipient)
            .in_thread(self.thread.clone())
            .in_reply_to(Some(self.reply_to.clone()))
    }
    fn key(&self) -> (String, String, String, Option<String>) {
        (
            self.channel.clone(),
            self.recipient.clone(),
            self.sender.clone(),
            self.thread.clone(),
        )
    }
}
tokio::task_local! { pub static ACTIVE_CONVERSATION: Option<ConversationRoute>; }
pub fn current() -> Option<ConversationRoute> {
    ACTIVE_CONVERSATION.try_with(Clone::clone).ok().flatten()
}

/// Merge only missing fields, and only within the same destination. An explicit
/// different channel or room never borrows the current thread/message identity.
pub fn inherit(mut args: serde_json::Value, kind: &str) -> serde_json::Value {
    let Some(route) = current() else {
        return args;
    };
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    if obj.get("channel").is_some_and(|v| v != &route.channel) {
        return args;
    }
    obj.entry("channel")
        .or_insert_with(|| route.channel.clone().into());
    let target = if kind == "reaction" {
        "channel_id"
    } else {
        "recipient"
    };
    if obj.get(target).is_some_and(|v| v != &route.recipient) {
        return args;
    }
    obj.entry(target).or_insert_with(|| {
        if kind == "reaction" && route.channel.starts_with("telegram") {
            route
                .recipient
                .split(':')
                .next()
                .unwrap_or(&route.recipient)
                .to_owned()
                .into()
        } else {
            route.recipient.clone().into()
        }
    });
    if kind == "reaction" {
        obj.entry("message_id")
            .or_insert_with(|| route.reply_to.into());
    }
    args
}

/// Persist a resolved announcement destination at creation time, never at run
/// time. Explicit none or a changed channel/room/thread prevents inheritance.
pub fn inherit_delivery(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let Some(route) = current() else {
        return value.cloned();
    };
    let mut delivery = value.cloned().unwrap_or(serde_json::json!({}));
    let Some(obj) = delivery.as_object_mut() else {
        return Some(delivery);
    };
    if obj.get("mode").is_some_and(|m| m == "none") {
        return Some(delivery);
    }
    let same_channel = obj.get("channel").is_none_or(|v| v == &route.channel);
    let same_room = obj.get("to").is_none_or(|v| v == &route.recipient);
    let same_thread = obj.get("thread_id").is_none_or(|v| {
        Some(v)
            == route
                .thread
                .as_ref()
                .map(|s| serde_json::Value::String(s.clone()))
                .as_ref()
    });
    obj.entry("mode").or_insert("announce".into());
    if same_channel {
        obj.entry("channel").or_insert(route.channel.into());
    }
    if same_channel && same_room {
        obj.entry("to").or_insert(route.recipient.into());
        if same_thread {
            obj.entry("thread_id")
                .or_insert(serde_json::json!(route.thread));
            obj.entry("reply_to").or_insert(route.reply_to.into());
        }
    }
    Some(delivery)
}

type Key = (String, String, String, Option<String>);
struct Pending {
    id: u64,
    sender: oneshot::Sender<String>,
}
static QUESTIONS: LazyLock<Mutex<HashMap<Key, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);
pub struct Question {
    key: Key,
    pub id: u64,
    receiver: Option<oneshot::Receiver<String>>,
}
impl Question {
    pub fn register(route: &ConversationRoute) -> anyhow::Result<Self> {
        let key = route.key();
        let mut pending = QUESTIONS
            .lock()
            .map_err(|_| anyhow::Error::msg("question registry unavailable"))?;
        anyhow::ensure!(
            !pending.contains_key(&key),
            "a question is already pending in this conversation"
        );
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        pending.insert(key.clone(), Pending { id, sender });
        Ok(Self {
            key,
            id,
            receiver: Some(receiver),
        })
    }
    pub async fn answer(&mut self) -> anyhow::Result<String> {
        Ok(self
            .receiver
            .take()
            .ok_or_else(|| anyhow::Error::msg("question already awaited"))?
            .await?)
    }
}
impl Drop for Question {
    fn drop(&mut self) {
        if let Ok(mut pending) = QUESTIONS.lock()
            && pending.get(&self.key).is_some_and(|p| p.id == self.id)
        {
            pending.remove(&self.key);
        }
    }
}
pub fn deliver(msg: &ChannelMessage) -> bool {
    deliver_route(
        &ConversationRoute::from_message(msg),
        None,
        msg.content.clone(),
    )
}
pub fn deliver_route(route: &ConversationRoute, id: Option<u64>, answer: String) -> bool {
    let Ok(mut pending) = QUESTIONS.lock() else {
        return false;
    };
    let key = route.key();
    if pending
        .get(&key)
        .is_none_or(|p| id.is_some_and(|id| id != p.id))
    {
        return false;
    }
    pending
        .remove(&key)
        .is_some_and(|p| p.sender.send(answer).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn route() -> ConversationRoute {
        ConversationRoute {
            channel: "telegram.fixture".into(),
            recipient: "42:7".into(),
            sender: "42".into(),
            thread: Some("7".into()),
            reply_to: "99".into(),
        }
    }
    #[tokio::test]
    async fn preserves_route_and_explicit_destination() {
        ACTIVE_CONVERSATION
            .scope(Some(route()), async {
                let v = inherit(serde_json::json!({"emoji":"x"}), "reaction");
                assert_eq!(v["channel_id"], "42");
                assert_eq!(v["message_id"], "99");
                let v = inherit(serde_json::json!({"channel":"slack.other"}), "poll");
                assert!(v.get("recipient").is_none());
                let v = inherit(serde_json::json!({"recipient":"other"}), "reaction");
                assert_eq!(v["recipient"], "other");
            })
            .await;
        assert!(current().is_none());
    }
    #[tokio::test]
    async fn wrong_sender_thread_stale_button_and_cancel_are_isolated() {
        let r = route();
        let mut q = Question::register(&r).unwrap();
        assert!(Question::register(&r).is_err());
        let mut other = r.clone();
        other.sender = "intruder".into();
        assert!(!deliver_route(&other, Some(q.id), "bad".into()));
        assert!(!deliver_route(&r, Some(q.id + 1), "stale".into()));
        assert!(deliver_route(&r, Some(q.id), "answer".into()));
        assert_eq!(q.answer().await.unwrap(), "answer");
        drop(q);
        let q = Question::register(&r).unwrap();
        drop(q);
        assert!(!deliver_route(&r, None, "late".into()));
    }
    #[tokio::test]
    async fn scheduled_delivery_preserves_only_matching_route() {
        use serde_json::json;
        ACTIVE_CONVERSATION.scope(Some(route()), async {
            let inherited=inherit_delivery(None).unwrap();
            assert_eq!(inherited,json!({"mode":"announce","channel":"telegram.fixture","to":"42:7","thread_id":"7","reply_to":"99"}));
            assert_eq!(inherit_delivery(Some(&json!({"mode":"none"}))),Some(json!({"mode":"none"})));
            let other=inherit_delivery(Some(&json!({"channel":"slack.other"}))).unwrap();
            assert!(other.get("to").is_none());
            let other=inherit_delivery(Some(&json!({"to":"43"}))).unwrap();
            assert!(other.get("reply_to").is_none());
            let other=inherit_delivery(Some(&json!({"thread_id":"8"}))).unwrap();
            assert!(other.get("reply_to").is_none());
        }).await;
    }
}
