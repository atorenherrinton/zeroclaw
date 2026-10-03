//! Ephemeral owner decisions for the three enrolled local Dot stdio routes.
//! Created by the approval gate, never deserialized from tool arguments. This is
//! trusted process/stdio context, not a credential or an OS isolation boundary.
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const META_KEY: &str = "zeroclaw/owner-confirmation";
pub fn requires_fresh_decision(name: &str) -> bool {
    matches!(
        name,
        "dot_reminders__confirm" | "dot_photos__confirm" | "dot_media__confirm"
    )
}

pub struct OwnerConfirmation {
    name: String,
    arguments: Value,
    issued_at: u64,
    used: AtomicBool,
}
impl OwnerConfirmation {
    /// Call only after a fresh attributable owner decision for these arguments.
    pub fn after_owner_decision(name: String, arguments: Value) -> Option<Self> {
        if !requires_fresh_decision(&name) {
            return None;
        }
        if serde_json::to_vec(&arguments).ok()?.len() > 8192 {
            return None;
        }
        Some(Self {
            name,
            arguments,
            issued_at: SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs(),
            used: AtomicBool::new(false),
        })
    }
    pub fn take(&self, name: &str, arguments: &Value) -> Option<Value> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if self.name != name
            || &self.arguments != arguments
            || now < self.issued_at
            || now >= self.issued_at + 180
            || self
                .used
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return None;
        }
        Some(
            json!({"version":1,"tool":name,"arguments":arguments,"issued_at":self.issued_at,"expires_at":self.issued_at+180,"source":"fresh_owner_decision"}),
        )
    }
}
tokio::task_local! { pub static OWNER_CONFIRMATIONS: Vec<OwnerConfirmation>; }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_call_consumes_once_and_json_cannot_create_context() {
        let args = json!({"scope":{"operation":"media.send","caption":"exact"}});
        let approval =
            OwnerConfirmation::after_owner_decision("dot_media__confirm".into(), args.clone())
                .unwrap();
        assert!(approval.take("dot_photos__confirm", &args).is_none());
        assert!(
            approval
                .take(
                    "dot_media__confirm",
                    &json!({"scope":{"operation":"media.send","caption":"changed"}})
                )
                .is_none()
        );
        assert!(approval.take("dot_media__confirm", &args).is_some());
        assert!(approval.take("dot_media__confirm", &args).is_none());
        assert!(OwnerConfirmation::after_owner_decision("other__confirm".into(), args).is_none());
    }
    #[test]
    fn stale_or_future_context_cannot_dispatch() {
        let mut approval =
            OwnerConfirmation::after_owner_decision("dot_reminders__confirm".into(), json!({}))
                .unwrap();
        approval.issued_at -= 181;
        assert!(
            approval
                .take("dot_reminders__confirm", &json!({}))
                .is_none()
        );
        approval.issued_at += 1000;
        assert!(
            approval
                .take("dot_reminders__confirm", &json!({}))
                .is_none()
        );
    }
}
