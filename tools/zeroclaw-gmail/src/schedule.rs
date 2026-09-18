//! Native Gmail UI handoffs only. The existing ledger owns immutable intent and
//! one-time issuance; Gmail alone owns scheduling. No caller-supplied evidence
//! can promote an unknown provider outcome to scheduled/cancelled.
use crate::{api::Api, model::*, operations, store::Store};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use mail_parser::{MessageParser, MimeHeaders};
use serde_json::{Value, json};

const ACTION: &str = "native_schedule";
const CANCEL: &str = "native_schedule_cancel";

/// Exact minute precision matches Gmail's documented date/time UI. Reject even
/// offset-disambiguated folds: the UI has no supported fold selector.
pub fn validate_time(at: &str, zone: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    ensure!(at.len() <= 40 && zone.len() <= 80, "invalid schedule time");
    ensure!(!at.ends_with("-00:00"), "unknown UTC offset denied");
    let instant =
        DateTime::parse_from_rfc3339(at).context("absolute RFC3339 time with offset required")?;
    let tz: Tz = zone.parse().context("IANA timezone required")?;
    let local = tz
        .from_local_datetime(&instant.naive_local())
        .single()
        .context("ambiguous or nonexistent local time denied")?;
    ensure!(
        local.offset().fix() == *instant.offset(),
        "timezone and offset disagree"
    );
    ensure!(
        instant.second() == 0 && instant.nanosecond() == 0,
        "whole minute required"
    );
    let utc = instant.with_timezone(&Utc);
    ensure!(
        utc >= now + Duration::minutes(5) && utc <= now + Duration::days(365),
        "schedule must be 5 minutes to 365 days in the future"
    );
    Ok(utc)
}

pub(crate) fn authorize(args: &Value) -> Result<()> {
    operations::owner(args)?;
    ensure!(
        args["authorization_source"] == "authenticated_owner",
        "separate exact scheduling/cancellation request from authenticated owner required; mixed/page/email provenance denied"
    );
    Ok(())
}

fn supported_mime(raw: &[u8]) -> Result<()> {
    std::str::from_utf8(raw).context("non-UTF-8 MIME bytes denied")?;
    let message = MessageParser::default()
        .parse(raw)
        .context("invalid MIME")?;
    ensure!(
        message.parts.len() == 1 && message.attachment_count() == 0,
        "native handoff supports a single plain-text MIME part only"
    );
    let part = message.parts.first().context("MIME part missing")?;
    ensure!(
        part.content_type()
            .is_none_or(|c| c.c_type.eq_ignore_ascii_case("text")
                && c.c_subtype
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case("plain"))
                && c.attributes.as_ref().is_none_or(|attrs| attrs.len() <= 1
                    && attrs.iter().all(|a| a.name.eq_ignore_ascii_case("charset")
                        && ["utf-8", "us-ascii"]
                            .iter()
                            .any(|s| a.value.eq_ignore_ascii_case(s))))),
        "unsupported MIME content type or charset"
    );
    ensure!(
        message.headers_raw().all(|(name, _)| [
            "from",
            "to",
            "cc",
            "bcc",
            "reply-to",
            "subject",
            "message-id",
            "in-reply-to",
            "references",
            "date",
            "mime-version",
            "content-type",
            "content-transfer-encoding"
        ]
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))),
        "unreviewed MIME header denied"
    );
    let mut headers = std::collections::HashSet::new();
    ensure!(
        message
            .headers_raw()
            .all(|(name, _)| headers.insert(name.to_ascii_lowercase())),
        "duplicate MIME header denied"
    );
    Ok(())
}

pub async fn prepare(
    store: &Store,
    api: &mut impl Api,
    account: &str,
    args: &Value,
    now: DateTime<Utc>,
) -> Result<Value> {
    fields(
        args,
        &[
            "operation_id",
            "draft_id",
            "expected_raw_sha256",
            "scheduled_at",
            "timezone",
        ],
    )?;
    let op = key(text(args, "operation_id", 160)?)?;
    let draft_id = id(text(args, "draft_id", 160)?)?;
    let expected = text(args, "expected_raw_sha256", 64)?;
    operations::digest(expected)?;
    let at = text(args, "scheduled_at", 40)?;
    let zone = text(args, "timezone", 80)?;
    validate_time(at, zone, now)?;
    let request_hash = hash(&serde_json::to_vec(
        &json!({"action":ACTION,"account":account,"args":args}),
    )?);
    if let Some(review) = store.prepared(op, &request_hash)? {
        return Ok(review);
    }
    ensure!(store.find(op)?.is_none(), "operation ID already used");
    let draft = operations::get_draft(api, draft_id).await?;
    let raw = decode_raw(&draft["message"])?;
    ensure!(
        hash(&raw) == expected,
        "draft changed; read and review again"
    );
    supported_mime(&raw)?;
    let (content, editable) = operations::parse_content(
        store,
        &raw,
        Some(text(&draft["message"], "threadId", 160)?.into()),
    )?;
    ensure!(
        editable && content.html_body.is_none(),
        "ambiguous or protected MIME denied"
    );
    ensure!(
        content.from.eq_ignore_ascii_case(account),
        "draft sender differs from pinned account"
    );
    let review = json!({"operation_id":op,"action":ACTION,"account":account,"draft_id":draft_id,
        "provider_message_id":draft["message"]["id"],"content":content,
        "scheduled_at":at,"timezone":zone,"prepared_at":now.to_rfc3339(),
        "untrusted_content":true,"gmail_schedule_status":"unknown",
        "mechanism":"gmail_native_ui_handoff_only","authorization":"review is data, not scheduling authorization"});
    store.save(op, &request_hash, review, &raw)
}

fn load_review(store: &Store, account: &str, args: &Value) -> Result<Value> {
    let op = key(text(args, "operation_id", 160)?)?;
    let (review, _) = store.review(op, text(args, "review_id", 64)?)?;
    ensure!(
        review["action"] == ACTION && review["account"] == account,
        "wrong review action or pinned account"
    );
    Ok(review)
}

/// Persist uncertainty BEFORE releasing a one-time handoff. A replay returns
/// only the receipt, never another actionable handoff, even after process death.
pub async fn begin(
    store: &Store,
    api: &mut impl Api,
    account: &str,
    args: &Value,
    now: DateTime<Utc>,
) -> Result<Value> {
    let _execution = store.execution_guard()?;
    fields(
        args,
        &[
            "operation_id",
            "review_id",
            "owner_requested",
            "authorization_source",
        ],
    )?;
    authorize(args)?;
    let review = load_review(store, account, args)?;
    let op = text(&review, "operation_id", 160)?;
    let draft_id = text(&review, "draft_id", 160)?;
    let intent = json!({"action":ACTION,"account":account,"draft_id":draft_id,"review_id":review["review_id"]});
    // Return existing uncertainty even when its time has since passed.
    if let Some(status) = store.find(op)? {
        ensure!(
            status["intent"] == intent,
            "operation bound to different intent"
        );
        return Ok(status);
    }
    validate_time(
        text(&review, "scheduled_at", 40)?,
        text(&review, "timezone", 80)?,
        now,
    )?;
    let prepared =
        DateTime::parse_from_rfc3339(text(&review, "prepared_at", 40)?)?.with_timezone(&Utc);
    ensure!(
        prepared <= now && now - prepared <= Duration::minutes(15),
        "schedule review stale; prepare a new review"
    );
    if !store.claim(op, &intent, Some(draft_id))? {
        return store.find(op)?.context("operation missing");
    }
    let preflight = operations::get_draft(api, draft_id)
        .await
        .and_then(|draft| {
            ensure!(
                draft["message"]["id"] == review["provider_message_id"]
                    && draft["message"]["threadId"] == review["content"]["thread_id"]
                    && hash(&decode_raw(&draft["message"])?) == review["raw_sha256"],
                "draft drift"
            );
            Ok(())
        });
    if preflight.is_err() {
        return store.finish(op, "rejected", &json!({"workflow_status":"handoff_not_issued",
            "gmail_schedule_status":"unknown","reason":"draft preflight failed; no UI handoff issued"}));
    }
    let receipt = store.finish(op, "uncertain", &json!({"workflow_status":"handoff_issued",
        "gmail_schedule_status":"unknown","provider_verified":false,"automatic_retry_allowed":false}))?;
    Ok(json!({"operation":receipt,"ui_handoff":{
        "action":"schedule_send","review":review,"entry_url":"https://mail.google.com/",
        "expires_at":(now + Duration::minutes(2)).to_rfc3339(),
        "surface":"approved CUA in the existing dedicated Safari window only",
        "requirements":["Verify exact signed-in account, draft identity, all recipients including CC/BCC, subject and complete body against review; abort if ambiguous or changed",
            "Pause concurrent editing; verify Gmail UI timezone equals the reviewed IANA timezone and offset",
            "Use the arrow next to Send, then Schedule send, then the exact reviewed date/time; never click Send",
            "Immediately before final Schedule send, recheck identity/content/time and expiry; stop on any mismatch or permission/policy denial",
            "Inspect the exact message in Gmail Scheduled and its displayed absolute time after the action; click alone or draft disappearance proves nothing",
            "Preserve provider/UI evidence outside this helper; report unknown on any uncertainty; never repeat the final action"]}}))
}

/// Cancellation is a separate owner request. This emits instructions only and
/// retains both claims indefinitely without an authoritative UI evidence reader.
pub fn cancel(store: &Store, account: &str, args: &Value) -> Result<Value> {
    let _execution = store.execution_guard()?;
    fields(
        args,
        &[
            "operation_id",
            "review_id",
            "owner_requested",
            "authorization_source",
        ],
    )?;
    authorize(args)?;
    let review = load_review(store, account, args)?;
    let original = text(&review, "operation_id", 160)?;
    let status = store
        .find(original)?
        .context("schedule handoff not issued")?;
    ensure!(
        status["state"] == "uncertain" && status["intent"]["action"] == ACTION,
        "no unresolved schedule handoff to cancel"
    );
    // One canonical cancellation identity per scheduling attempt prevents using
    // another caller-selected key to reissue an uncertain cancellation.
    let op = format!("cancel_{}", hash(original.as_bytes()));
    let intent = json!({"action":CANCEL,"account":account,"schedule_operation_id":original,"review_id":review["review_id"],"draft_id":review["draft_id"]});
    if !store.claim(&op, &intent, None)? {
        return store.find(&op)?.context("operation missing");
    }
    let receipt = store.finish(&op, "uncertain", &json!({"workflow_status":"cancel_handoff_issued",
        "gmail_schedule_status":"unknown","provider_verified":false,"automatic_retry_allowed":false}))?;
    Ok(
        json!({"operation":receipt,"ui_handoff":{"action":"cancel_send","review":review,
        "surface":"approved CUA in the existing dedicated Safari window only",
        "requirements":["Stop any in-flight scheduling UI work before cancellation; do not execute concurrently",
            "Find the exact message in Gmail Scheduled; verify account, identity, complete recipients/content and scheduled time against review; stop if identity is ambiguous",
            "Click Cancel send only for that exact message, under the separate owner cancellation request",
            "Verify Gmail shows cancellation and restores that exact message to Drafts; no match, elapsed time or disappearance does not prove cancellation or delivery",
            "Do not delete the draft, reschedule, resend or repeat uncertain cancellation; preserve provider/UI evidence outside this helper"]}}),
    )
}

/// Public draft reads cannot prove scheduling/cancellation. Record only what was
/// observed and keep the durable claim; never accept an assertion or screenshot
/// supplied as a tool argument as provider evidence.
pub async fn reconcile(
    store: &Store,
    api: &mut impl Api,
    account: &str,
    args: &Value,
) -> Result<Value> {
    let _execution = store.execution_guard()?;
    fields(args, &["operation_id"])?;
    let op = key(text(args, "operation_id", 160)?)?;
    let status = store.find(op)?.context("operation missing")?;
    let intent = &status["intent"];
    ensure!(
        intent["account"] == account && matches!(intent["action"].as_str(), Some(ACTION | CANCEL)),
        "wrong account or operation kind"
    );
    if status["state"] != "uncertain" {
        return Ok(status);
    }
    let original = intent["schedule_operation_id"].as_str().unwrap_or(op);
    let (review, _) = store.review(original, text(intent, "review_id", 64)?)?;
    let observation = match operations::get_draft(api, text(&review, "draft_id", 160)?).await {
        Ok(draft) => match decode_raw(&draft["message"]) {
            Ok(raw)
                if hash(&raw) == review["raw_sha256"]
                    && draft["message"]["id"] == review["provider_message_id"]
                    && draft["message"]["threadId"] == review["content"]["thread_id"] =>
            {
                "matching_draft_present"
            }
            Ok(_) => "draft_changed",
            Err(_) => "read_failed",
        },
        Err(e) if e.downcast_ref::<crate::api::NotFound>().is_some() => "draft_absent",
        Err(_) => "read_failed",
    };
    let mut receipt = status["receipt"].clone();
    receipt["draft_observation"] = json!(observation);
    receipt["gmail_schedule_status"] = json!("unknown");
    receipt["provider_verified"] = json!(false);
    receipt["automatic_retry_allowed"] = json!(false);
    store.finish(op, "uncertain", &receipt)
}
