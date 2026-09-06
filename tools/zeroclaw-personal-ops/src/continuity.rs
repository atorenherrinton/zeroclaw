//! Owner decisions are canonical here; provider facts remain source-attributed
//! evidence. Updates use revision checks so a stale worker cannot erase progress.
use crate::{Ops, text};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS contact_destinations(contact_id TEXT NOT NULL,context TEXT NOT NULL,channel TEXT NOT NULL,destination TEXT NOT NULL,owner_evidence TEXT NOT NULL,approved_ms INTEGER NOT NULL,PRIMARY KEY(contact_id,context,channel));
 CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY,revision INTEGER NOT NULL,payload TEXT NOT NULL,updated_ms INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS shipments(id TEXT PRIMARY KEY,carrier TEXT NOT NULL,tracking_number TEXT NOT NULL,label TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'registered',expected_at TEXT,evidence TEXT NOT NULL DEFAULT '{}',updated_ms INTEGER NOT NULL,UNIQUE(carrier,tracking_number));
 CREATE TABLE IF NOT EXISTS source_snapshots(source TEXT PRIMARY KEY,payload TEXT NOT NULL,verified_ms INTEGER NOT NULL,error TEXT);")?;
    Ok(())
}
impl Ops {
    pub fn destination_set(&self, a: &Value) -> Result<Value> {
        ensure!(
            a["owner_approved"] == true,
            "only an owner-approved destination may be remembered"
        );
        let contact = text(a, "contact_id", 512)?;
        let context = text(a, "context", 128)?;
        let channel = text(a, "channel", 64)?;
        let dest = text(a, "destination", 254)?;
        let evidence = text(a, "owner_evidence", 2000)?;
        ensure!(
            ["email", "imessage", "calendar"].contains(&channel) && crate::recipient(dest),
            "invalid destination or channel"
        );
        self.db.execute("INSERT INTO contact_destinations VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(contact_id,context,channel) DO UPDATE SET destination=excluded.destination,owner_evidence=excluded.owner_evidence,approved_ms=excluded.approved_ms",params![contact,context,channel,dest,evidence,Utc::now().timestamp_millis()])?;
        Ok(
            json!({"saved":true,"contact_id":contact,"context":context,"channel":channel,"destination":dest}),
        )
    }
    pub fn destination_resolve(&self, a: &Value) -> Result<Value> {
        let contact = text(a, "contact_id", 512)?;
        let context = text(a, "context", 128)?;
        let channel = text(a, "channel", 64)?;
        let approved:Option<(String,String,i64)>=self.db.query_row("SELECT destination,owner_evidence,approved_ms FROM contact_destinations WHERE contact_id=?1 AND context=?2 AND channel=?3",params![contact,context,channel],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let candidates = a["candidates"]
            .as_array()
            .context("current contact destination candidates required")?;
        ensure!(
            candidates
                .iter()
                .all(|v| v.as_str().is_some_and(crate::recipient)),
            "invalid contact candidates"
        );
        if let Some((dest, evidence, approved_ms)) = approved {
            if candidates
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(&dest)))
            {
                return Ok(
                    json!({"state":"resolved","destination":dest,"source":"owner_approved_contextual_preference","owner_evidence":evidence,"approved_ms":approved_ms}),
                );
            }
            return Ok(
                json!({"state":"needs_clarification","reason":"approved destination is no longer in this contact","previous_destination":dest}),
            );
        }
        if candidates.len() == 1 {
            return Ok(
                json!({"state":"resolved","destination":candidates[0],"source":"only_current_destination","approval_to_send":false}),
            );
        }
        Ok(
            json!({"state":"needs_clarification","reason":"no approved destination for this exact contact, context and channel","candidates":candidates}),
        )
    }
    pub fn project_update(&self, a: &Value) -> Result<Value> {
        let id = text(a, "project_id", 128)?;
        let revision = a["expected_revision"]
            .as_i64()
            .context("expected_revision required; use 0 for new project")?;
        let payload = &a["project"];
        let obj = payload.as_object().context("project must be object")?;
        let allowed = [
            "desired_outcome",
            "status",
            "next_action",
            "blocker",
            "waiting_on",
            "decisions",
            "deadline",
            "last_verified_evidence",
        ];
        ensure!(
            obj.keys().all(|k| allowed.contains(&k.as_str())),
            "unknown project field"
        );
        for key in [
            "desired_outcome",
            "status",
            "next_action",
            "blocker",
            "waiting_on",
        ] {
            ensure!(
                payload[key].as_str().is_some_and(|s| s.len() <= 8192),
                "all project continuity fields must be supplied as strings"
            );
        }
        ensure!(
            matches!(
                payload["status"].as_str(),
                Some("active" | "waiting" | "blocked" | "completed" | "paused")
            ),
            "invalid project status"
        );
        ensure!(
            !text(payload, "desired_outcome", 8192)?.is_empty(),
            "outcome required"
        );
        if !payload["deadline"].is_null() {
            DateTime::parse_from_rfc3339(
                payload["deadline"]
                    .as_str()
                    .context("deadline must include UTC offset")?,
            )?;
        }
        ensure!(
            payload["last_verified_evidence"].is_object(),
            "source and timestamp evidence required; use an empty object if unverified"
        );
        ensure!(
            payload["decisions"]
                .as_array()
                .is_some_and(|a| a.len() <= 100),
            "decisions must be a bounded array"
        );
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let old: Option<i64> = self
            .db
            .query_row("SELECT revision FROM projects WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        ensure!(
            old.unwrap_or(0) == revision,
            "project revision changed; re-read before updating"
        );
        self.db.execute("INSERT INTO projects VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,payload=excluded.payload,updated_ms=excluded.updated_ms",params![id,revision+1,payload.to_string(),Utc::now().timestamp_millis()])?;
        self.receipt(
            id,
            None,
            "project_updated",
            &json!({"revision":revision+1,"evidence":payload["last_verified_evidence"]}),
        )?;
        tx.commit()?;
        Ok(json!({"project_id":id,"revision":revision+1,"project":payload}))
    }
    pub fn project_list(&self) -> Result<Value> {
        let rows=self.db.prepare("SELECT id,revision,payload,updated_ms FROM projects ORDER BY updated_ms DESC LIMIT 200")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"projects":rows.into_iter().map(|(id,revision,p,at)|Ok(json!({"project_id":id,"revision":revision,"project":serde_json::from_str::<Value>(&p)?,"updated_ms":at}))).collect::<Result<Vec<_>>>()?}),
        )
    }
    pub fn shipment_track(&self, a: &Value) -> Result<Value> {
        let carrier = text(a, "carrier", 32)?.to_lowercase();
        let tracking = text(a, "tracking_number", 80)?.to_uppercase();
        let label = text(a, "label", 200)?;
        ensure!(
            ["ups", "fedex", "usps", "dhl", "amazon", "other"].contains(&carrier.as_str()),
            "unsupported carrier name"
        );
        ensure!(
            tracking
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "tracking number must contain only letters, digits or hyphen"
        );
        let id = crate::digest(format!("{carrier}:{tracking}").as_bytes());
        self.db.execute("INSERT INTO shipments(id,carrier,tracking_number,label,updated_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET label=excluded.label",params![id,carrier,tracking,label,Utc::now().timestamp_millis()])?;
        Ok(
            json!({"shipment_id":id,"carrier":carrier,"tracking_number":tracking,"tracking_url":tracking_url(&carrier,&tracking),"state":"registered"}),
        )
    }
    pub fn shipment_update(&self, a: &Value) -> Result<Value> {
        let id = text(a, "shipment_id", 128)?;
        let state = text(a, "state", 32)?;
        ensure!(
            [
                "registered",
                "label_created",
                "in_transit",
                "out_for_delivery",
                "delivered",
                "delayed",
                "exception",
                "returned",
                "cancelled"
            ]
            .contains(&state),
            "invalid shipment state"
        );
        let evidence = &a["evidence"];
        text(evidence, "source", 2048)?;
        text(evidence, "summary", 4000)?;
        let observed =
            DateTime::parse_from_rfc3339(text(evidence, "observed_at", 64)?)?.timestamp_millis();
        ensure!(
            observed <= Utc::now().timestamp_millis() + 60_000,
            "evidence cannot be in the future"
        );
        if let Some(expected) = a.get("expected_at").filter(|v| !v.is_null()) {
            DateTime::parse_from_rfc3339(
                expected.as_str().context("expected_at must be RFC3339")?,
            )?;
        }
        let tx = self.db.unchecked_transaction()?;
        let previous: (String, String) = self.db.query_row(
            "SELECT state,evidence FROM shipments WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let prior: Value = serde_json::from_str(&previous.1)?;
        let prior_time = prior["observed_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
            .unwrap_or(0);
        ensure!(
            observed >= prior_time,
            "stale shipment evidence cannot overwrite a newer update"
        );
        self.db.execute("UPDATE shipments SET state=?2,expected_at=COALESCE(?3,expected_at),evidence=?4,updated_ms=?5 WHERE id=?1",params![id,state,a["expected_at"].as_str(),evidence.to_string(),Utc::now().timestamp_millis()])?;
        if previous.0 != state {
            self.receipt(
                id,
                None,
                "shipment_changed",
                &json!({"previous_state":previous.0,"state":state,"evidence":evidence}),
            )?;
        }
        tx.commit()?;
        Ok(json!({"shipment_id":id,"state":state,"changed":previous.0!=state,"evidence":evidence}))
    }
    pub fn shipment_list(&self) -> Result<Value> {
        let rows=self.db.prepare("SELECT id,carrier,tracking_number,label,state,expected_at,evidence,updated_ms FROM shipments ORDER BY updated_ms DESC LIMIT 200")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"shipments":rows.into_iter().map(|(id,carrier,tracking,label,state,expected,evidence,updated)|Ok(json!({"shipment_id":id,"carrier":carrier,"tracking_number":tracking,"label":label,"state":state,"expected_at":expected,"evidence":serde_json::from_str::<Value>(&evidence)?,"updated_ms":updated,"tracking_url":tracking_url(&carrier,&tracking)}))).collect::<Result<Vec<_>>>()?}),
        )
    }
    pub fn snapshot(&self, source: &str, payload: &Value, error: Option<&str>) -> Result<()> {
        // A failed refresh retains the last successful data and timestamp, but makes
        // staleness explicit. It must never turn an outage into an empty inbox.
        self.db.execute("INSERT INTO source_snapshots VALUES(?1,?2,?3,?4) ON CONFLICT(source) DO UPDATE SET payload=CASE WHEN excluded.error IS NULL THEN excluded.payload ELSE source_snapshots.payload END,verified_ms=CASE WHEN excluded.error IS NULL THEN excluded.verified_ms ELSE source_snapshots.verified_ms END,error=excluded.error",params![source,payload.to_string(),if error.is_none(){Utc::now().timestamp_millis()}else{0},error])?;
        Ok(())
    }
}
pub fn tracking_url(carrier: &str, tracking: &str) -> String {
    match carrier {
        "ups" => format!("https://www.ups.com/track?tracknum={tracking}"),
        "fedex" => format!("https://www.fedex.com/fedextrack/?trknbr={tracking}"),
        "usps" => format!("https://tools.usps.com/go/TrackConfirmAction?tLabels={tracking}"),
        "dhl" => format!("https://www.dhl.com/global-en/home/tracking.html?tracking-id={tracking}"),
        "amazon" => "https://www.amazon.com/gp/your-account/order-history".into(),
        _ => String::new(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contextual_preferences_do_not_cross_context_or_ignore_contact_changes() -> Result<()> {
        let t = tempfile::tempdir()?;
        let o = Ops::open(t.path())?;
        o.destination_set(&json!({"contact_id":"fixture","context":"personal_calendar","channel":"calendar","destination":"personal@example.invalid","owner_approved":true,"owner_evidence":"owner chose personal address"}))?;
        let a = json!({"contact_id":"fixture","context":"personal_calendar","channel":"calendar","candidates":["personal@example.invalid","work@example.invalid"]});
        assert_eq!(o.destination_resolve(&a)?["state"], "resolved");
        let mut a = a;
        a["context"] = json!("work");
        assert_eq!(o.destination_resolve(&a)?["state"], "needs_clarification");
        a["context"] = json!("personal_calendar");
        a["candidates"] = json!(["work@example.invalid"]);
        assert_eq!(o.destination_resolve(&a)?["state"], "needs_clarification");
        Ok(())
    }
    #[test]
    fn shipment_rejects_stale_and_unsafe_tracking() -> Result<()> {
        let t = tempfile::tempdir()?;
        let o = Ops::open(t.path())?;
        assert!(
            o.shipment_track(
                &json!({"carrier":"ups","tracking_number":"../bad?","label":"fixture"})
            )
            .is_err()
        );
        let s = o.shipment_track(
            &json!({"carrier":"ups","tracking_number":"1ZFIXTURE","label":"fixture"}),
        )?;
        let a = json!({"shipment_id":s["shipment_id"],"state":"delivered","evidence":{"source":"email:fixture","summary":"carrier delivery email","observed_at":"2026-01-02T00:00:00Z"}});
        o.shipment_update(&a)?;
        let mut a = a;
        a["evidence"]["observed_at"] = json!("2026-01-01T00:00:00Z");
        assert!(o.shipment_update(&a).is_err());
        Ok(())
    }
}
