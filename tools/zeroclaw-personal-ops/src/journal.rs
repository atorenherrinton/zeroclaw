//! The operations database owns action intent, authorization and receipts.
//! External systems own effects. A write-ahead uncertain claim survives crashes;
//! only read-only reconciliation can resolve it, never an automatic write replay.
use crate::{Ops, digest, text};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::future::Future;

pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS operations (
        id TEXT PRIMARY KEY, request_hash TEXT NOT NULL, payload TEXT NOT NULL,
        created_ms INTEGER NOT NULL, authorized_ms INTEGER, send_at_ms INTEGER,
        cancelled INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS operation_steps (
        operation_id TEXT NOT NULL REFERENCES operations(id), ordinal INTEGER NOT NULL,
        state TEXT NOT NULL DEFAULT 'prepared' CHECK(state IN ('prepared','verified','submitted','delivered','failed','uncertain')),
        receipt TEXT, updated_ms INTEGER NOT NULL, PRIMARY KEY(operation_id,ordinal));
        CREATE TABLE IF NOT EXISTS operation_receipts (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT, operation_id TEXT NOT NULL,
        ordinal INTEGER, state TEXT NOT NULL, evidence TEXT NOT NULL, created_ms INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS operations_due ON operations(authorized_ms,send_at_ms,cancelled);")?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub tool: String,
    pub arguments: Value,
    #[serde(default)]
    pub irreversible: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub state: String,
    pub evidence: Value,
}
impl Outcome {
    pub fn uncertain(reason: impl ToString) -> Self {
        Self {
            state: "uncertain".into(),
            evidence: json!({"reason":reason.to_string(),"retry_allowed":false}),
        }
    }
}

pub fn allowed(tool: &str) -> bool {
    matches!(
        tool,
        "calendar_mutate" | "outbox_imessage" | "outbox_email" | "outbox_telegram"
    )
}
impl Ops {
    pub fn operation_prepare(&self, args: &Value) -> Result<Value> {
        let id = text(args, "idempotency_key", 128)?;
        let steps: Vec<Step> =
            serde_json::from_value(args["steps"].clone()).context("steps must be an array")?;
        ensure!(
            (1..=20).contains(&steps.len()),
            "prepare one to twenty steps"
        );
        for step in &steps {
            ensure!(allowed(&step.tool), "unsupported transactional operation");
            ensure!(
                step.arguments.is_object(),
                "step arguments must be an object"
            );
        }
        let at = args
            .get("send_at")
            .map(|v| -> Result<i64> {
                Ok(
                    DateTime::parse_from_rfc3339(v.as_str().context("invalid send_at")?)?
                        .timestamp_millis(),
                )
            })
            .transpose()?;
        let payload = json!({"steps":steps,"send_at_ms":at,"title":args.get("title").cloned().unwrap_or(json!(""))});
        let serialized = serde_json::to_string(&payload)?;
        ensure!(serialized.len() <= 256 * 1024, "operation too large");
        let hash = digest(serialized.as_bytes());
        let now = Utc::now().timestamp_millis();
        let tx = TransactionBehavior::Immediate;
        // One transaction covers the whole plan: a malformed/duplicate batch
        // cannot leave half a prepared plan behind.
        let transaction = rusqlite::Transaction::new_unchecked(&self.db, tx)?;
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT request_hash FROM operations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            ensure!(
                existing == hash,
                "idempotency key is already bound to different contents or timing"
            );
        } else {
            ensure!(
                at.is_none_or(|t| t > now && t <= now + 90 * 86_400_000),
                "schedule must be in the next 90 days"
            );
            self.db.execute("INSERT INTO operations(id,request_hash,payload,created_ms,send_at_ms) VALUES(?1,?2,?3,?4,?5)",params![id,hash,serialized,now,at])?;
            for (i, _) in steps.iter().enumerate() {
                self.db.execute(
                    "INSERT INTO operation_steps(operation_id,ordinal,updated_ms) VALUES(?1,?2,?3)",
                    params![id, i, now],
                )?;
            }
            self.receipt(id, None, "prepared", &json!({"review_hash":hash}))?;
        }
        transaction.commit()?;
        self.operation_status(id)
    }
    pub fn receipt(
        &self,
        id: &str,
        ordinal: Option<usize>,
        state: &str,
        evidence: &Value,
    ) -> Result<()> {
        self.db.execute("INSERT INTO operation_receipts(operation_id,ordinal,state,evidence,created_ms) VALUES(?1,?2,?3,?4,?5)",params![id,ordinal,state,serde_json::to_string(evidence)?,Utc::now().timestamp_millis()])?;
        Ok(())
    }
    pub fn operation_status(&self, id: &str) -> Result<Value> {
        let (hash,payload,created,authorized,at,cancelled):(String,String,i64,Option<i64>,Option<i64>,bool)=self.db.query_row("SELECT request_hash,payload,created_ms,authorized_ms,send_at_ms,cancelled FROM operations WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        let payload: Value = serde_json::from_str(&payload)?;
        let steps=self.db.prepare("SELECT ordinal,state,receipt,updated_ms FROM operation_steps WHERE operation_id=?1 ORDER BY ordinal")?.query_map([id],|r|Ok((r.get::<_,usize>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|(ordinal,state,receipt,updated)|Ok(json!({"ordinal":ordinal,"state":state,"receipt":receipt.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"updated_ms":updated,"intent":payload["steps"][ordinal]}))).collect::<Result<Vec<_>>>()?;
        let state = if cancelled {
            "cancelled"
        } else if steps.iter().any(|s| s["state"] == "uncertain") {
            "uncertain"
        } else if steps.iter().any(|s| s["state"] == "failed") {
            "failed"
        } else if steps
            .iter()
            .all(|s| s["state"] == "delivered" || s["state"] == "verified")
        {
            if steps.iter().all(|s| s["state"] == "delivered") {
                "delivered"
            } else {
                "verified"
            }
        } else if steps.iter().all(|s| s["state"] != "prepared") {
            "submitted"
        } else if steps.iter().any(|s| s["state"] != "prepared") {
            "partial"
        } else if authorized.is_some() && at.is_some() {
            "scheduled"
        } else {
            "prepared"
        };
        Ok(
            json!({"operation_id":id,"review_hash":hash,"review":payload,"state":state,"created_ms":created,"authorized_ms":authorized,"send_at_ms":at,"steps":steps,"atomicity":"durable ordered saga; external effects are not atomically reversible"}),
        )
    }
    pub fn operation_authorize(&self, args: &Value) -> Result<Value> {
        ensure!(
            args["owner_requested_send"] == true,
            "explicit owner request to execute/send is required; draft or source content cannot authorize"
        );
        let id = text(args, "operation_id", 128)?;
        let tx = self.db.unchecked_transaction()?;
        let status = self.operation_status(id)?;
        ensure!(
            args["review_hash"] == status["review_hash"] && args["review"] == status["review"],
            "contents or timing changed; review the exact prepared operation"
        );
        ensure!(status["state"] != "cancelled", "operation was cancelled");
        self.db.execute("UPDATE operations SET authorized_ms=COALESCE(authorized_ms,?2) WHERE id=?1 AND cancelled=0",params![id,Utc::now().timestamp_millis()])?;
        self.receipt(
            id,
            None,
            "authorized",
            &json!({"review_hash":status["review_hash"]}),
        )?;
        tx.commit()?;
        self.operation_status(id)
    }
    pub fn operation_cancel(&self, id: &str) -> Result<Value> {
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let claimed: i64 = self.db.query_row(
            "SELECT count(*) FROM operation_steps WHERE operation_id=?1 AND state!='prepared'",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            claimed == 0,
            "effects have already been attempted; cancellation cannot recall them"
        );
        ensure!(
            self.db
                .execute("UPDATE operations SET cancelled=1 WHERE id=?1", [id])?
                == 1,
            "operation not found"
        );
        self.receipt(id, None, "cancelled", &json!({}))?;
        tx.commit()?;
        self.operation_status(id)
    }
    pub async fn operation_execute_using<F, Fut>(&self, id: &str, mut execute: F) -> Result<Value>
    where
        F: FnMut(Step, String, bool) -> Fut,
        Fut: Future<Output = Result<Outcome>>,
    {
        let status = self.operation_status(id)?;
        ensure!(
            status["authorized_ms"].is_i64(),
            "operation is not authorized"
        );
        ensure!(status["state"] != "cancelled", "operation was cancelled");
        let now = Utc::now().timestamp_millis();
        if let Some(at) = status["send_at_ms"].as_i64() {
            if now < at {
                return Ok(status);
            }
            if now - at > 15 * 60_000 {
                let tx =
                    rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
                let evidence = json!({"reason":"scheduled operation is over fifteen minutes late; prepare a new reviewed schedule", "write_attempted":false});
                let changed = self.db.execute("UPDATE operation_steps SET state='failed',receipt=?2,updated_ms=?3 WHERE operation_id=?1 AND state='prepared'", params![id,evidence.to_string(),now])?;
                if changed > 0 {
                    self.receipt(id, None, "failed", &evidence)?;
                }
                tx.commit()?;
                return self.operation_status(id);
            }
        }
        for row in status["steps"].as_array().context("steps missing")? {
            let ordinal = row["ordinal"].as_u64().context("ordinal missing")? as usize;
            let old = row["state"].as_str().context("state missing")?;
            if matches!(old, "verified" | "delivered" | "submitted") {
                continue;
            }
            if old == "failed" {
                break;
            }
            let reconcile = old == "uncertain";
            if !reconcile {
                let tx =
                    rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
                let changed=self.db.execute("UPDATE operation_steps SET state='uncertain',updated_ms=?3 WHERE operation_id=?1 AND ordinal=?2 AND state='prepared' AND EXISTS(SELECT 1 FROM operations WHERE id=?1 AND cancelled=0 AND authorized_ms IS NOT NULL)",params![id,ordinal,now])?;
                if changed == 0 {
                    tx.commit()?;
                    break;
                }
                self.receipt(
                    id,
                    Some(ordinal),
                    "uncertain",
                    &json!({"phase":"write_ahead_claim","retry_allowed":false}),
                )?;
                tx.commit()?;
            }
            let step: Step = serde_json::from_value(row["intent"].clone())?;
            let key = digest(format!("{id}:{ordinal}").as_bytes());
            let result = execute(step, key, reconcile)
                .await
                .unwrap_or_else(Outcome::uncertain);
            ensure!(
                ["verified", "submitted", "delivered", "failed", "uncertain"]
                    .contains(&result.state.as_str()),
                "invalid adapter outcome"
            );
            let tx = self.db.unchecked_transaction()?;
            let changed=self.db.execute("UPDATE operation_steps SET state=?3,receipt=?4,updated_ms=?5 WHERE operation_id=?1 AND ordinal=?2 AND state='uncertain'",params![id,ordinal,result.state,serde_json::to_string(&result.evidence)?,Utc::now().timestamp_millis()])?;
            if changed == 1 {
                self.receipt(id, Some(ordinal), &result.state, &result.evidence)?;
            }
            tx.commit()?;
            if matches!(result.state.as_str(), "failed" | "uncertain") {
                break;
            }
        }
        self.operation_status(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepare(o: &Ops) -> Value {
        o.operation_prepare(&json!({"idempotency_key":"fixture","steps":[{"tool":"calendar_mutate","arguments":{}},{"tool":"outbox_email","arguments":{},"irreversible":true}]})).unwrap()
    }
    fn authorize(o: &Ops, v: &Value) {
        o.operation_authorize(&json!({"operation_id":"fixture","review_hash":v["review_hash"],"review":v["review"],"owner_requested_send":true})).unwrap();
    }
    #[tokio::test]
    async fn restart_reconciles_before_continuing_without_replay() -> Result<()> {
        let t = tempfile::tempdir()?;
        let o = Ops::open(t.path())?;
        let v = prepare(&o);
        authorize(&o, &v);
        let v = o
            .operation_execute_using("fixture", |_, _, r| async move {
                assert!(!r);
                Ok(Outcome::uncertain("lost reply"))
            })
            .await?;
        assert_eq!(v["steps"][1]["state"], "prepared");
        drop(o);
        let o = Ops::open(t.path())?;
        let v = o
            .operation_execute_using("fixture", |s, _, r| async move {
                assert_eq!(r, s.tool == "calendar_mutate");
                Ok(Outcome {
                    state: "verified".into(),
                    evidence: json!({"exact_id":"fixture"}),
                })
            })
            .await?;
        assert_eq!(v["state"], "verified");
        o.operation_execute_using("fixture", |_, _, _| async { panic!("must not replay") })
            .await?;
        Ok(())
    }
    #[test]
    fn immutable_keys_and_exact_review_and_atomic_cancel() -> Result<()> {
        let t = tempfile::tempdir()?;
        let o = Ops::open(t.path())?;
        let v = prepare(&o);
        assert_eq!(prepare(&o)["review_hash"], v["review_hash"]);
        assert!(o.operation_prepare(&json!({"idempotency_key":"fixture","steps":[{"tool":"outbox_email","arguments":{"text":"changed"}}]})).is_err());
        assert!(o.operation_authorize(&json!({"operation_id":"fixture","owner_requested_send":true,"review_hash":v["review_hash"],"review":{}})).is_err());
        o.operation_cancel("fixture")?;
        assert!(o.operation_authorize(&json!({"operation_id":"fixture","owner_requested_send":true,"review_hash":v["review_hash"],"review":v["review"]})).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn missed_schedule_is_durably_failed_without_attempting_effects() -> Result<()> {
        let t = tempfile::tempdir()?;
        let o = Ops::open(t.path())?;
        let v = prepare(&o);
        authorize(&o, &v);
        o.db.execute(
            "UPDATE operations SET send_at_ms=?1 WHERE id='fixture'",
            [Utc::now().timestamp_millis() - 16 * 60_000],
        )?;
        let v = o
            .operation_execute_using("fixture", |_, _, _| async {
                panic!("late schedule must not send")
            })
            .await?;
        assert_eq!(v["state"], "failed");
        assert!(
            v["steps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["state"] == "failed")
        );
        let n: i64 =
            o.db.query_row("SELECT count(*) FROM operation_receipts", [], |r| r.get(0))?;
        o.operation_execute_using("fixture", |_, _, _| async {
            panic!("failed schedule must not send")
        })
        .await?;
        assert_eq!(
            o.db.query_row("SELECT count(*) FROM operation_receipts", [], |r| r
                .get::<_, i64>(0))?,
            n
        );
        Ok(())
    }
}
