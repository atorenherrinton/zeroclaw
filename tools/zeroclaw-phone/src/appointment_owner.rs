//! Owner-local view of canonical appointment_proposals. No transcript, recording,
//! caller number, event text, account or raw provider receipt enters this view.
//! Reconciliation reads the writer's existing intent; it never creates a hold.

use crate::{
    appointment_calendar::{self, Event, HoldReceipt, HoldState},
    appointments::Request,
    common::{self, SafeResult, check},
    protocol,
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, future::Future, os::unix::fs::MetadataExt, path::Path, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    call_sid: Option<String>,
    #[serde(default)]
    reconcile: bool,
}

fn arguments(value: Value) -> SafeResult<Arguments> {
    let args: Arguments =
        serde_json::from_value(value).map_err(|_| "appointment_status_arguments_invalid")?;
    check(
        args.call_sid
            .as_deref()
            .is_none_or(|sid| protocol::valid_sid(sid, "CA")),
        "appointment_status_call_invalid",
    )?;
    check(
        !args.reconcile || args.call_sid.is_some(),
        "appointment_reconcile_call_required",
    )?;
    Ok(args)
}

// This is a bounded, short-lived projection of existing records, not a new
// ledger or policy snapshot. No Debug: source fields contain private intent.
struct Row {
    sid: String,
    state: String,
    request: String,
    verification: Option<String>,
    original: Option<String>,
    key: Option<String>,
    receipt: Option<String>,
    created_ms: i64,
    updated_ms: i64,
    phase: String,
    outcome: Option<String>,
    consent: Option<i64>,
}

fn database(root: &Path, writable: bool) -> SafeResult<Option<Connection>> {
    // Status must not create a DB, run migrations, change journal mode or make
    // a missing extension appear installed. Use the existing private file.
    let directory = match fs::symlink_metadata(root) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("appointment_database_metadata_failed"),
    };
    check(
        directory.is_dir()
            && !directory.file_type().is_symlink()
            && directory.uid() == unsafe { libc::geteuid() }
            && directory.mode() & 0o077 == 0,
        "appointment_database_directory_unsafe",
    )?;
    let path = root.join("phone.sqlite");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("appointment_database_metadata_failed"),
    };
    check(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "appointment_database_unsafe",
    )?;
    let mode = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let db = Connection::open_with_flags(
        path,
        mode | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| "appointment_database_open_failed")?;
    db.busy_timeout(Duration::from_millis(250))
        .map_err(|_| "appointment_database_timeout_failed")?;
    Ok(Some(db))
}

fn load(root: &Path, sid: Option<&str>, include_receipt: bool) -> SafeResult<Vec<Row>> {
    let Some(db) = database(root, false)? else {
        return Ok(Vec::new());
    };
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='appointment_proposals')", [], |row| row.get(0)).map_err(|_| "appointment_status_read_failed")?;
    if !exists {
        return Ok(Vec::new());
    }
    // Bound BLOB bytes before Rust materializes source text. Receipt bytes are
    // read only for the one exact reconciliation CAS, never for recent lists.
    let mut statement = db
        .prepare(
            "SELECT p.call_sid,p.state,
        CASE WHEN length(CAST(p.request AS BLOB))<=8192 THEN p.request END,
        CASE WHEN length(CAST(p.verification AS BLOB))<=8192 THEN p.verification END,
        CASE WHEN length(CAST(p.original AS BLOB))<=8192 THEN p.original END,
        CASE WHEN length(CAST(p.idempotency_key AS BLOB))<=128 THEN p.idempotency_key END,
        CASE WHEN ?2 AND length(CAST(p.receipt AS BLOB))<=2097152 THEN p.receipt END,
        p.created_ms,p.updated_ms,substr(c.phase,1,64),substr(c.outcome,1,64),c.consent
        FROM appointment_proposals p JOIN calls c ON c.call_sid=p.call_sid
        WHERE (?1 IS NULL OR p.call_sid=?1) AND length(CAST(p.call_sid AS BLOB))=34
        AND length(CAST(p.state AS BLOB))<=32
        ORDER BY p.created_ms DESC,p.call_sid DESC LIMIT 20",
        )
        .map_err(|_| "appointment_status_read_failed")?;
    let rows = statement
        .query_map(params![sid, include_receipt], |row| {
            Ok(Row {
                sid: row.get(0)?,
                state: row.get(1)?,
                request: row.get(2)?,
                verification: row.get(3)?,
                original: row.get(4)?,
                key: row.get(5)?,
                receipt: row.get(6)?,
                created_ms: row.get(7)?,
                updated_ms: row.get(8)?,
                phase: row.get(9)?,
                outcome: row.get(10)?,
                consent: row.get(11)?,
            })
        })
        .map_err(|_| "appointment_status_read_failed")?;
    rows.map(|row| row.map_err(|_| "appointment_status_data_invalid"))
        .collect()
}

fn request(row: &Row) -> SafeResult<Request> {
    Request::parse(
        serde_json::from_str(&row.request).map_err(|_| "appointment_status_data_invalid")?,
    )
}

fn original(row: &Row) -> SafeResult<Event> {
    serde_json::from_str(
        row.original
            .as_deref()
            .ok_or("appointment_original_missing")?,
    )
    .map_err(|_| "appointment_original_invalid")
}

fn public_map(row: &Row) -> SafeResult<Option<String>> {
    let Some(proof) = &row.verification else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_str(proof).map_err(|_| "appointment_verification_invalid")?;
    let link = value["map_url"]
        .as_str()
        .ok_or("appointment_listing_url_invalid")?;
    check(
        link.len() <= 2048 && !link.chars().any(char::is_control),
        "appointment_listing_url_invalid",
    )?;
    let url = url::Url::parse(link).map_err(|_| "appointment_listing_url_invalid")?;
    check(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && (matches!(url.host_str(), Some("maps.apple.com" | "maps.google.com"))
                || (url.host_str() == Some("www.google.com") && url.path().starts_with("/maps"))),
        "appointment_listing_url_invalid",
    )?;
    Ok(Some(link.to_owned()))
}

fn phase(value: &str) -> &str {
    match value {
        "consent" | "notice" | "media" | "active" | "ended" | "expired" => value,
        _ => "unknown",
    }
}

fn outcome(value: Option<&str>) -> &str {
    match value {
        Some(
            value @ ("call_ended"
            | "peer_closed"
            | "duration_limit"
            | "invalid_options"
            | "setup_failed"
            | "upstream_closed"
            | "upstream_error"
            | "protocol_error"
            | "resource_limit"
            | "io_timeout"
            | "assistant_ended"
            | "recording_declined"
            | "service_interrupted"
            | "upgrade_failed"
            | "outbound_context_failed"),
        ) => value,
        _ => "unknown",
    }
}

fn metadata(row: &Row) -> SafeResult<Value> {
    check(
        protocol::valid_sid(&row.sid, "CA")
            && matches!(
                row.state.as_str(),
                "checking" | "writing" | "held" | "unavailable" | "uncertain"
            ),
        "appointment_status_data_invalid",
    )?;
    let request = request(row)?;
    let proposed_end = match &row.original {
        Some(_) => {
            let event = original(row)?;
            let from = DateTime::parse_from_rfc3339(&event.start)
                .map_err(|_| "appointment_original_invalid")?;
            let to = DateTime::parse_from_rfc3339(&event.end)
                .map_err(|_| "appointment_original_invalid")?;
            check(
                to > from && to - from <= chrono::Duration::hours(24),
                "appointment_original_invalid",
            )?;
            let start = DateTime::parse_from_rfc3339(&request.proposed_start)
                .map_err(|_| "appointment_status_data_invalid")?;
            Some(
                start
                    .checked_add_signed(to - from)
                    .ok_or("appointment_status_data_invalid")?
                    .to_rfc3339(),
            )
        }
        None => None,
    };
    let unresolved = matches!(row.state.as_str(), "writing" | "uncertain");
    Ok(
        json!({"call_sid":row.sid,"state":row.state,"original_start":request.original_start,"proposed_start":request.proposed_start,"proposed_end":proposed_end,
        "public_maps_link":public_map(row)?,"original_preserved":true,"owner_review_required":true,"calendar_outcome_unknown":unresolved,
        "call_phase":phase(&row.phase),"call_outcome":outcome(row.outcome.as_deref()),"recording_suppressed":row.consent==Some(0)||row.outcome.as_deref()==Some("recording_declined"),
        "reconciliation_allowed":row.phase=="ended"&&unresolved&&row.original.is_some()&&row.key.as_deref()==Some(appointment_calendar::hold_key(&row.sid).as_str()),
        "created_ms":row.created_ms,"updated_ms":row.updated_ms}),
    )
}

trait Reconciler: Sync {
    fn read(
        &self,
        config: &Path,
        sid: &str,
        original: &Event,
        proposed_start: &str,
    ) -> impl Future<Output = SafeResult<HoldReceipt>> + Send;
}
struct Native;
impl Reconciler for Native {
    async fn read(
        &self,
        config: &Path,
        sid: &str,
        original: &Event,
        proposed_start: &str,
    ) -> SafeResult<HoldReceipt> {
        appointment_calendar::reconcile_hold(config, sid, original, proposed_start).await
    }
}

fn apply(root: &Path, row: &Row, result: SafeResult<HoldReceipt>) -> SafeResult<bool> {
    let (state, receipt) = match result {
        Ok(receipt) => {
            check(
                receipt.idempotency_key
                    == row.key.as_deref().ok_or("appointment_identity_missing")?,
                "appointment_reconcile_identity_changed",
            )?;
            let state = if receipt.state == HoldState::Verified {
                "held"
            } else {
                "uncertain"
            };
            (
                state,
                Some(
                    serde_json::to_string(&receipt)
                        .map_err(|_| "appointment_receipt_encode_failed")?,
                ),
            )
        }
        Err(_) => ("uncertain", None), // Preserve all previous receipt bytes.
    };
    check(
        receipt.as_ref().is_none_or(|v| v.len() <= 2 * 1024 * 1024),
        "appointment_receipt_limit",
    )?;
    let Some(db) = database(root, true)? else {
        return Err("appointment_database_missing");
    };
    let updated = Utc::now().timestamp_millis().max(
        row.updated_ms
            .checked_add(1)
            .ok_or("appointment_state_invalid")?,
    );
    let changed=db.execute("UPDATE appointment_proposals SET state=?1,receipt=COALESCE(?2,receipt),updated_ms=?3
        WHERE call_sid=?4 AND state=?5 AND state IN ('writing','uncertain') AND updated_ms=?6
        AND request=?7 AND original IS ?8 AND verification IS ?9 AND idempotency_key IS ?10 AND receipt IS ?11
        AND EXISTS(SELECT 1 FROM calls c WHERE c.call_sid=appointment_proposals.call_sid AND c.phase='ended')",
        params![state,receipt,updated,row.sid,row.state,row.updated_ms,row.request,row.original,row.verification,row.key,row.receipt])
        .map_err(|_|"appointment_reconcile_store_failed")?;
    Ok(changed == 1)
}

async fn status_using<R: Reconciler>(
    root: &Path,
    args: Value,
    reconciler: &R,
) -> SafeResult<Value> {
    let args = arguments(args)?;
    let mut rows = load(root, args.call_sid.as_deref(), args.reconcile)?;
    let Some(sid) = args.call_sid else {
        return Ok(
            json!({"appointments":rows.iter().map(metadata).collect::<SafeResult<Vec<_>>>()?,"limit":20,"provider_read":false}),
        );
    };
    let Some(row) = rows.pop() else {
        return Ok(json!({"appointment":null,"provider_read":false}));
    };
    if !args.reconcile {
        return Ok(json!({"appointment":metadata(&row)?,"provider_read":false}));
    }
    check(row.phase == "ended", "appointment_reconcile_call_active")?;
    check(
        matches!(row.state.as_str(), "writing" | "uncertain"),
        "appointment_reconcile_not_needed",
    )?;
    check(
        row.key.as_deref() == Some(appointment_calendar::hold_key(&sid).as_str()),
        "appointment_reconcile_identity_missing",
    )?;
    let request = request(&row)?;
    let original = original(&row)?;
    let config = common::native_dir(root)?;
    let result = reconciler
        .read(&config, &sid, &original, &request.proposed_start)
        .await;
    let verified = result
        .as_ref()
        .is_ok_and(|receipt| receipt.state == HoldState::Verified);
    let changed = apply(root, &row, result)?;
    let current = load(root, Some(&sid), false)?
        .pop()
        .ok_or("appointment_state_missing")?;
    Ok(
        json!({"appointment":metadata(&current)?,"provider_read_attempted":true,"calendar_write":false,
        "reconciliation":if !changed {"state_changed"} else if verified {"verified"} else {"uncertain"},"retry_allowed":false}),
    )
}

/// Local owner MCP only. Do not advertise this on the remote realtime surface.
pub async fn status(root: &Path, args: Value) -> SafeResult<Value> {
    status_using(root, args, &Native).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };
    const SID: &str = "CA11111111111111111111111111111111";
    const SECRET: &str = "SYNTHETIC_PRIVATE_DATA_MUST_NOT_LEAVE";
    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp
            .path()
            .canonicalize()
            .unwrap()
            .join(".zeroclaw/extensions/phone");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let db = common::open_db(&root).unwrap();
        crate::appointment_backend::initialize(&db).unwrap();
        db.execute("INSERT INTO calls(call_sid,account_sid,from_candidate,consent,consent_token,created_ms,phase,transcript,outcome,summary_text) VALUES(?1,?2,?2,0,'synthetic-token',1,'ended',?2,'recording_declined',?2)",params![SID,SECRET]).unwrap();
        let request = json!({"business_name":"Synthetic Clinic","business_address":"100 Test Road, Example City","original_start":"2030-01-01T10:00:00Z","proposed_start":"2030-01-02T10:00:00Z","caller_confirmed":true});
        let original = Event {
            calendar_id: "synthetic@example.invalid".into(),
            id: "original123".into(),
            etag: "\"1\"".into(),
            start: "2030-01-01T10:00:00Z".into(),
            end: "2030-01-01T10:30:00Z".into(),
            summary: SECRET.into(),
            location: SECRET.into(),
        };
        db.execute("INSERT INTO appointment_proposals(call_sid,request,state,verification,original,idempotency_key,receipt,created_ms,updated_ms) VALUES(?1,?2,'uncertain',?3,?4,?5,?6,1,2)",params![SID,request.to_string(),json!({"name":SECRET,"address":SECRET,"phone":SECRET,"map_url":"https://maps.apple.com/?auid=synthetic","place_id":"synthetic","verified_ms":1}).to_string(),serde_json::to_string(&original).unwrap(),appointment_calendar::hold_key(SID),json!({"private":SECRET}).to_string()]).unwrap();
        (temp, root)
    }
    fn receipt() -> HoldReceipt {
        HoldReceipt {
            state: HoldState::Verified,
            idempotency_key: appointment_calendar::hold_key(SID),
            event_id: "opaque-hold-id".into(),
            calendar_id: "synthetic@example.invalid".into(),
            start: "2030-01-02T10:00:00Z".into(),
            end: "2030-01-02T10:30:00Z".into(),
            structured: json!({"private":SECRET}),
        }
    }
    struct Fake {
        result: Mutex<Option<SafeResult<HoldReceipt>>>,
        calls: AtomicUsize,
    }
    impl Fake {
        fn new(result: SafeResult<HoldReceipt>) -> Self {
            Self {
                result: Mutex::new(Some(result)),
                calls: AtomicUsize::new(0),
            }
        }
    }
    impl Reconciler for Fake {
        async fn read(
            &self,
            _config: &Path,
            _sid: &str,
            _original: &Event,
            _proposed: &str,
        ) -> SafeResult<HoldReceipt> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.lock().unwrap().take().unwrap()
        }
    }

    #[test]
    fn status_arguments_allow_only_exact_call_identity_and_explicit_reconcile() {
        assert!(arguments(json!({})).is_ok());
        assert!(arguments(json!({"call_sid":SID,"reconcile":true})).is_ok());
        for value in [
            json!({"reconcile":true}),
            json!({"call_sid":"CAinvalid"}),
            json!({"call_sid":SID,"reconcile":"true"}),
            json!({"event_id":"arbitrary"}),
            json!({"call_sid":SID,"include_transcript":true}),
        ] {
            assert!(arguments(value).is_err());
        }
    }
    #[tokio::test]
    async fn late_recording_refusal_keeps_only_safe_proposal_metadata_visible() {
        let (_temp, root) = setup();
        let fake = Fake::new(Err("must_not_run"));
        for args in [json!({}), json!({"call_sid":SID})] {
            let value = status_using(&root, args, &fake).await.unwrap();
            let encoded = value.to_string();
            assert!(!encoded.contains(SECRET));
            assert!(!encoded.contains("transcript"));
            assert!(!encoded.contains("receipt"));
            assert!(encoded.contains("recording_declined"));
            assert!(encoded.contains("2030-01-02"));
            assert!(encoded.contains("maps.apple.com"));
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn ended_uncertainty_reconciles_read_only_and_stores_full_private_receipt() {
        let (_temp, root) = setup();
        let fake = Fake::new(Ok(receipt()));
        let value = status_using(&root, json!({"call_sid":SID,"reconcile":true}), &fake)
            .await
            .unwrap();
        assert_eq!(value["appointment"]["state"], "held");
        assert_eq!(value["reconciliation"], "verified");
        assert_eq!(value["calendar_write"], false);
        assert!(!value.to_string().contains(SECRET));
        let db = database(&root, false).unwrap().unwrap();
        let saved: String = db
            .query_row(
                "SELECT receipt FROM appointment_proposals WHERE call_sid=?1",
                [SID],
                |row| row.get(0),
            )
            .unwrap();
        let saved: HoldReceipt = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved.structured, json!({"private":SECRET}));
        let transcript: String = db
            .query_row(
                "SELECT transcript FROM calls WHERE call_sid=?1",
                [SID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transcript, SECRET);
    }
    #[tokio::test]
    async fn active_call_never_reconciles_and_error_retains_existing_private_receipt() {
        let (_temp, root) = setup();
        let db = database(&root, true).unwrap().unwrap();
        db.execute("UPDATE calls SET phase='active'", []).unwrap();
        drop(db);
        let fake = Fake::new(Err("synthetic_provider_failure"));
        assert!(
            status_using(&root, json!({"call_sid":SID,"reconcile":true}), &fake)
                .await
                .is_err()
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        let db = database(&root, true).unwrap().unwrap();
        db.execute("UPDATE calls SET phase='ended'", []).unwrap();
        drop(db);
        let before = load(&root, Some(SID), true).unwrap().pop().unwrap().receipt;
        let value = status_using(&root, json!({"call_sid":SID,"reconcile":true}), &fake)
            .await
            .unwrap();
        assert_eq!(value["appointment"]["state"], "uncertain");
        assert_eq!(value["reconciliation"], "uncertain");
        assert_eq!(
            load(&root, Some(SID), true).unwrap().pop().unwrap().receipt,
            before
        );
    }
    #[test]
    fn exact_cas_preserves_newer_state_and_rechecks_ended_phase() {
        let (_temp, root) = setup();
        let row = load(&root, Some(SID), true).unwrap().pop().unwrap();
        let db = database(&root, true).unwrap().unwrap();
        db.execute("UPDATE calls SET phase='active'", []).unwrap();
        drop(db);
        assert!(!apply(&root, &row, Ok(receipt())).unwrap());
        let db = database(&root, true).unwrap().unwrap();
        db.execute("UPDATE calls SET phase='ended'", []).unwrap();
        db.execute(
            "UPDATE appointment_proposals SET updated_ms=3,receipt='newer evidence'",
            [],
        )
        .unwrap();
        drop(db);
        assert!(!apply(&root, &row, Ok(receipt())).unwrap());
        assert_eq!(
            load(&root, Some(SID), true)
                .unwrap()
                .pop()
                .unwrap()
                .receipt
                .as_deref(),
            Some("newer evidence")
        );
    }
    #[tokio::test]
    async fn missing_table_is_empty_without_creation_or_migration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("missing-phone");
        let fake = Fake::new(Err("must_not_run"));
        assert_eq!(
            status_using(&root, json!({}), &fake).await.unwrap()["appointments"],
            json!([])
        );
        assert!(!root.exists());
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let db = common::open_db(&root).unwrap();
        drop(db);
        assert_eq!(
            status_using(&root, json!({}), &fake).await.unwrap()["appointments"],
            json!([])
        );
        let db = database(&root, false).unwrap().unwrap();
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='appointment_proposals')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }
}
