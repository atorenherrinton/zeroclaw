//! Immediate or scheduled plain text to one existing group. The operations ledger is the
//! sole authority for immutable intent, review, authorization, claim and receipt.
//! No individual-recipient adapter, attachment or group creation is reachable.
use crate::{
    GroupTarget, Item, Ops, imessage,
    journal::{Outcome, Step},
    text,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, path::PathBuf};

pub(crate) const TOOL: &str = "outbox_imessage_group_text";
const MAX_FUTURE_MS: i64 = 90 * 86_400_000;
// A missed wake is not permission for a late catch-up send. Allow only the
// normal subsecond scheduling/revalidation budget, never the outbox's 15m grace.
const DISPATCH_BUDGET_MS: i64 = 1_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    idempotency_key: String,
    group_token: String,
    text: String,
    send_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImmediateRequest {
    idempotency_key: String,
    group_token: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    group_token: String,
    group: GroupTarget,
    text: String,
    // None binds immediate dispatch; Some preserves scheduled RFC3339 bytes.
    send_at: Option<String>,
}

fn instant(value: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(value)?.timestamp_millis())
}

pub(crate) fn validate(
    step: &Step,
    resolve: impl FnOnce(&str) -> Result<GroupTarget>,
) -> Result<()> {
    ensure!(
        step.tool == TOOL && step.irreversible,
        "invalid group text step"
    );
    let prepared: Prepared = serde_json::from_value(step.arguments.clone())?;
    text(&step.arguments, "text", 12000)?;
    ensure!(
        prepared.group.token()? == prepared.group_token,
        "group token changed"
    );
    if let Some(at) = &prepared.send_at {
        instant(at)?;
    }
    let current = resolve(&prepared.group_token)?;
    imessage::validate_group_snapshot(&prepared.group, &current)
}

pub(crate) fn validate_review(review: &Value) -> Result<()> {
    let steps = review["steps"].as_array().context("steps missing")?;
    if steps.iter().any(|s| s["tool"] == TOOL) {
        ensure!(
            steps.len() == 1,
            "group text must be a dedicated single-step operation"
        );
        let prepared: Prepared = serde_json::from_value(steps[0]["arguments"].clone())?;
        ensure!(
            review["send_at_ms"] == json!(prepared.send_at.as_deref().map(instant).transpose()?),
            "group text schedule changed"
        );
    }
    Ok(())
}

pub(crate) fn is_group(review: &Value) -> bool {
    review["steps"][0]["tool"] == TOOL
}

impl Ops {
    pub fn immediate_group_text_prepare(&self, args: &Value) -> Result<Value> {
        self.immediate_group_text_prepare_using(args, imessage::resolve_group_token)
    }

    fn immediate_group_text_prepare_using(
        &self,
        args: &Value,
        resolve: impl FnOnce(&str) -> Result<GroupTarget>,
    ) -> Result<Value> {
        let request: ImmediateRequest = serde_json::from_value(args.clone())?;
        text(args, "idempotency_key", 128)?;
        text(args, "text", 12000)?;
        text(args, "group_token", 512)?;
        let mut group = resolve(&request.group_token)?;
        ensure!(
            group.token()? == request.group_token,
            "group token does not match current group"
        );
        group.name.clear();
        let prepared = Prepared {
            group_token: request.group_token,
            group,
            text: request.text,
            send_at: None,
        };
        self.operation_prepare(&json!({"idempotency_key":request.idempotency_key,"title":"Immediate existing-group text","steps":[{"tool":TOOL,"arguments":prepared,"irreversible":true}]}))
    }

    pub fn group_text_prepare(&self, args: &Value) -> Result<Value> {
        self.group_text_prepare_using(
            args,
            Utc::now().timestamp_millis(),
            imessage::resolve_group_token,
        )
    }

    fn group_text_prepare_using(
        &self,
        args: &Value,
        now: i64,
        resolve: impl FnOnce(&str) -> Result<GroupTarget>,
    ) -> Result<Value> {
        let request: Request = serde_json::from_value(args.clone())?;
        text(args, "idempotency_key", 128)?;
        text(args, "text", 12000)?;
        text(args, "group_token", 512)?;
        let at = instant(&request.send_at)?;
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
            [&request.idempotency_key],
            |r| r.get(0),
        )?;
        ensure!(
            exists || (at > now && at <= now + MAX_FUTURE_MS),
            "send_at must be future, within 90 days; never schedule a late send"
        );
        let mut group = resolve(&request.group_token)?;
        ensure!(
            group.token()? == request.group_token,
            "group token does not match current group"
        );
        // Display names are mutable UI metadata, not group identity.
        group.name.clear();
        let prepared = Prepared {
            group_token: request.group_token,
            group,
            text: request.text,
            send_at: Some(request.send_at.clone()),
        };
        self.operation_prepare(&json!({"idempotency_key":request.idempotency_key,"send_at":request.send_at,"title":"Scheduled existing-group text","steps":[{"tool":TOOL,"arguments":prepared,"irreversible":true}]}))
    }

    pub fn group_text_schedule(&self, args: &Value) -> Result<Value> {
        let status = self.operation_status(text(args, "operation_id", 128)?)?;
        ensure!(is_group(&status["review"]), "not a group text operation");
        ensure!(
            status["send_at_ms"].is_i64(),
            "immediate group text requires outbox_send, not scheduling"
        );
        // This only authorizes the durable future row; it never dispatches.
        self.operation_authorize(args)
    }
}

pub(crate) async fn execute_using<F, Fut>(
    step: Step,
    reconcile: bool,
    clock: impl Fn() -> i64,
    resolve: impl FnOnce(&str) -> Result<GroupTarget>,
    send: F,
) -> Result<Outcome>
where
    F: FnOnce(Item, Option<PathBuf>) -> Fut,
    Fut: Future<Output = imessage::SendOutcome>,
{
    if reconcile {
        return Ok(Outcome::uncertain(
            "group text was already claimed; never replay an uncertain attempt",
        ));
    }
    let preflight = (|| -> Result<Prepared> {
        validate(&step, resolve)?;
        let prepared: Prepared = serde_json::from_value(step.arguments)?;
        let now = clock();
        if let Some(at) = &prepared.send_at {
            let at = instant(at)?;
            ensure!(
                now >= at && now <= at + DISPATCH_BUDGET_MS,
                "group text dispatch time missed; no early or late catch-up send"
            );
        }
        Ok(prepared)
    })();
    let prepared = match preflight {
        Ok(value) => value,
        Err(error) => {
            return Ok(Outcome {
                state: "failed".into(),
                evidence: json!({"reason":error.to_string(),"write_attempted":false}),
            });
        }
    };
    let item = Item {
        recipient: prepared.group_token,
        group: Some(prepared.group),
        text: prepared.text,
        attachment: None,
        attachment_sha256: None,
        source_call: None,
    };
    Ok(match send(item, None).await {
        imessage::SendOutcome::Submitted(receipt) => Outcome {
            state: "submitted".into(),
            evidence: json!({"provider":"imessage","delivered":false,"receipt":receipt}),
        },
        imessage::SendOutcome::NotStarted(reason) => Outcome {
            state: "failed".into(),
            evidence: json!({"reason":reason,"write_attempted":false,"retry_allowed":false}),
        },
        imessage::SendOutcome::Uncertain(reason) => Outcome::uncertain(reason),
    })
}

#[cfg(test)]
#[path = "group_text_tests.rs"]
mod tests;
