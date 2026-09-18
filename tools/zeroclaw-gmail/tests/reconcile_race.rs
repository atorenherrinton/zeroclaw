//! Reconciliation must not release serialization while a provider write is in flight.
use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};
use tokio::sync::Notify;
use zeroclaw_gmail::{
    api::Api,
    model::{Content, decode_raw, hash},
    operations,
    store::Store,
};
struct MemoryApi {
    draft: Rc<RefCell<Value>>,
    wait: Option<(Rc<Notify>, Rc<Notify>)>,
}
impl Api for MemoryApi {
    async fn request(
        &mut self,
        method: &str,
        path: &str,
        _: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        assert_eq!(path, "drafts/draft123");
        match method {
            "GET" => Ok(self.draft.borrow().clone()),
            "PUT" => {
                if let Some((started, resume)) = self.wait.take() {
                    started.notify_one();
                    resume.notified().await;
                }
                let mut draft = self.draft.borrow_mut();
                draft["message"]["raw"] = body.unwrap()["message"]["raw"].clone();
                let current = draft["message"]["id"].as_str().unwrap();
                draft["message"]["id"] = json!(format!("{current}x"));
                Ok(json!({"id":"draft123"}))
            }
            _ => panic!("unexpected method"),
        }
    }
}
#[tokio::test(flavor = "current_thread")]
async fn reconcile_must_not_unlock_an_in_flight_noop_update() -> Result<()> {
    let store = Store::memory()?;
    let c = Content {
        from: "owner@example.com".into(),
        reply_to: None,
        to: vec!["recipient@example.net".into()],
        cc: vec![],
        bcc: vec![],
        subject: "fixture".into(),
        body: "original".into(),
        html_body: None,
        attachments: vec![],
        thread_id: Some("thread123".into()),
        in_reply_to: None,
        references: vec![],
        source_message_id: None,
        mode: "existing".into(),
        message_id: "fixture@example.com".into(),
    };
    let raw = c.assemble(&|h| store.bytes(h), 1_800_000_000)?;
    let draft = Rc::new(RefCell::new(
        json!({"id":"draft123","message":{"id":"message123","threadId":"thread123","raw":URL_SAFE_NO_PAD.encode(&raw)}}),
    ));
    let started = Rc::new(Notify::new());
    let resume = Rc::new(Notify::new());
    let mut first = MemoryApi {
        draft: draft.clone(),
        wait: Some((started.clone(), resume.clone())),
    };
    let mut second = MemoryApi {
        draft: draft.clone(),
        wait: None,
    };
    let preparation=operations::prepare(&store,&mut first,"owner@example.com",&[],&json!({"operation_id":"first","action":"update","draft_id":"draft123","expected_raw_sha256":hash(&raw)})).await?;
    let first_args =
        json!({"operation_id":"first","review_id":preparation["review_id"],"owner_requested":true});
    let first_future = operations::apply(&store, &mut first, "owner@example.com", &first_args);
    let concurrent = async {
        started.notified().await;
        let result=async {
            let resolution=operations::reconcile(&store,&mut second,"owner@example.com",&json!({"operation_id":"first"})).await;
            let prematurely_applied=resolution.as_ref().is_ok_and(|r|r["state"]=="applied");
            if prematurely_applied {
                let preparation=operations::prepare(&store,&mut second,"owner@example.com",&[],&json!({"operation_id":"second","action":"update","draft_id":"draft123","expected_raw_sha256":hash(&raw),"body":"second"})).await?;
                let changed=operations::apply(&store,&mut second,"owner@example.com",&json!({"operation_id":"second","review_id":preparation["review_id"],"owner_requested":true})).await?;
                assert_eq!(changed["state"],"applied");
            }
            Ok::<_,anyhow::Error>(prematurely_applied)
        }.await;
        // Always release the suspended writer, including a safe lock rejection.
        resume.notify_one();
        result
    };
    let (first, concurrent) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(first_future, concurrent)
    })
    .await?;
    let prematurely_applied = concurrent?;
    assert_eq!(first?["state"], "applied");
    let current = decode_raw(&draft.borrow()["message"])?;
    let (content, _) = operations::parse_content(&store, &current, Some("thread123".into()))?;
    assert!(
        !prematurely_applied,
        "reconcile reported applied while its PUT was suspended; this let a later body=second update be overwritten by the first apply (final body={:?})",
        content.body
    );
    Ok(())
}
