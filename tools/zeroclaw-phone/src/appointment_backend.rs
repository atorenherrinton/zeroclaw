//! Local owner-authorized inbound workflow. Canonical inputs are the signed
//! call row, live phone policy, the public Maps listing and Google Calendar.
//! The private per-call row records this workflow's one proposal and receipt;
//! the Google writer remains the sole owner of provider mutation/reconciliation.

use crate::{
    appointment_calendar::{self, Event, HoldReceipt, HoldState},
    appointments::{AppointmentScheduler, Request, SchedulingFuture},
    common::{self, SafeResult, check},
    maps_lookup::{self, LookupResponse},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

type Work<'a, T> = Pin<Box<dyn Future<Output = SafeResult<T>> + Send + 'a>>;

trait Services: Send + Sync {
    fn lookup<'a>(&'a self, request: &'a Request) -> Work<'a, LookupResponse>;
    fn find<'a>(
        &'a self,
        config: &'a Path,
        request: &'a Request,
        proof: &'a Verification,
    ) -> Work<'a, Event>;
    fn hold<'a>(
        &'a self,
        config: &'a Path,
        sid: &'a str,
        request: &'a Request,
        original: &'a Event,
        before_write: &'a (dyn Fn() -> SafeResult<()> + Send + Sync),
    ) -> Work<'a, HoldReceipt>;
}

struct NativeServices;

impl Services for NativeServices {
    fn lookup<'a>(&'a self, request: &'a Request) -> Work<'a, LookupResponse> {
        Box::pin(maps_lookup::lookup(
            &request.business_name,
            &request.business_address,
        ))
    }
    fn find<'a>(
        &'a self,
        config: &'a Path,
        request: &'a Request,
        proof: &'a Verification,
    ) -> Work<'a, Event> {
        Box::pin(appointment_calendar::find_original(
            config,
            &request.original_start,
            &proof.name,
            &proof.address,
        ))
    }
    fn hold<'a>(
        &'a self,
        config: &'a Path,
        sid: &'a str,
        request: &'a Request,
        original: &'a Event,
        before_write: &'a (dyn Fn() -> SafeResult<()> + Send + Sync),
    ) -> Work<'a, HoldReceipt> {
        Box::pin(appointment_calendar::create_hold(
            config,
            sid,
            original,
            &request.proposed_start,
            before_write,
        ))
    }
}

pub struct InboundScheduler {
    root: PathBuf,
    call_sid: String,
    services: Arc<dyn Services>,
}

impl InboundScheduler {
    pub fn for_call(
        root: &Path,
        call_sid: &str,
    ) -> SafeResult<Option<Arc<dyn AppointmentScheduler>>> {
        if !common::tentative_rescheduling_enabled(root)? {
            return Ok(None);
        }
        check(
            crate::protocol::valid_sid(call_sid, "CA"),
            "appointment_call_invalid",
        )?;
        let scheduler = Self {
            root: root.to_owned(),
            call_sid: call_sid.to_owned(),
            services: Arc::new(NativeServices),
        };
        scheduler.admitted()?;
        Ok(Some(Arc::new(scheduler)))
    }

    fn admitted(&self) -> SafeResult<String> {
        let policy: common::PhoneConfig =
            toml::from_str(&common::private_read(&self.root.join("phone.toml"))?)
                .map_err(|_| "phone_config_invalid")?;
        check(
            policy.enabled && policy.tentative_rescheduling,
            "appointment_disabled",
        )?;
        let db = database(&self.root)?;
        let from: String = db.query_row("SELECT from_candidate FROM calls WHERE call_sid=?1 AND account_sid=?2 AND phase='active' AND consent=1 AND NOT EXISTS (SELECT 1 FROM outbound_requests o WHERE o.call_sid=calls.call_sid)", params![self.call_sid, policy.account_sid], |row| row.get(0))
            .map_err(|_| "appointment_call_not_admitted")?;
        check(common::e164(&from), "appointment_number_unavailable")?;
        Ok(from)
    }

    async fn execute(&self, request: Request) -> SafeResult<Value> {
        let from = self.admitted()?;
        validate_window(&request, Utc::now())?;
        let encoded = serde_json::to_string(&request).map_err(|_| "appointment_encode_failed")?;
        let db = database(&self.root)?;
        let claimed = db.execute("INSERT OR IGNORE INTO appointment_proposals(call_sid,request,state,created_ms,updated_ms) VALUES(?1,?2,'checking',?3,?3)", params![self.call_sid, encoded, Utc::now().timestamp_millis()])
            .map_err(|_| "appointment_claim_failed")?;
        if claimed == 0 {
            let (previous, state, result): (String, String, Option<String>) = db.query_row("SELECT request,state,model_result FROM appointment_proposals WHERE call_sid=?1", [&self.call_sid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .map_err(|_| "appointment_claim_read_failed")?;
            check(previous == encoded, "appointment_one_proposal_per_call")?;
            return if state == "held" {
                serde_json::from_str(&result.ok_or("appointment_receipt_missing")?)
                    .map_err(|_| "appointment_receipt_invalid")
            } else {
                Ok(unavailable(state == "writing" || state == "uncertain"))
            };
        }
        drop(db);
        let result = self.perform(&request, &from).await;
        if result.is_err() {
            // A failed pre-write check is known not to have changed Calendar.
            // Once writing was recorded, keep outcome uncertainty for review;
            // neither a dropped wait nor a transport error authorizes a replay.
            let db = database(&self.root)?;
            db.execute("UPDATE appointment_proposals SET state=CASE WHEN state='checking' THEN 'unavailable' WHEN state='writing' THEN 'uncertain' ELSE state END,updated_ms=?2 WHERE call_sid=?1", params![self.call_sid, Utc::now().timestamp_millis()])
                .map_err(|_| "appointment_outcome_store_failed")?;
        }
        result
    }

    async fn perform(&self, request: &Request, from: &str) -> SafeResult<Value> {
        let listings = self.services.lookup(request).await?;
        let proof = verify_listing(listings, request, from)?;
        check(self.admitted()? == from, "appointment_call_changed")?;
        let config = common::native_dir(&self.root)?;
        let original = self.services.find(&config, request, &proof).await?;
        let start = DateTime::parse_from_rfc3339(&original.start)
            .map_err(|_| "appointment_original_invalid")?;
        let end = DateTime::parse_from_rfc3339(&original.end)
            .map_err(|_| "appointment_original_invalid")?;
        check(
            end > start && end - start <= ChronoDuration::hours(8),
            "appointment_duration_invalid",
        )?;
        check(
            start
                == DateTime::parse_from_rfc3339(&request.original_start)
                    .map_err(|_| "appointment_original_invalid")?,
            "appointment_original_changed",
        )?;
        check(self.admitted()? == from, "appointment_call_changed")?;
        check(
            (0..60_000).contains(&(Utc::now().timestamp_millis() - proof.verified_ms)),
            "appointment_verification_expired",
        )?;
        let db = database(&self.root)?;
        let changed = db.execute("UPDATE appointment_proposals SET state='writing',verification=?2,original=?3,idempotency_key=?4,updated_ms=?5 WHERE call_sid=?1 AND state='checking'", params![self.call_sid, serde_json::to_string(&proof).map_err(|_| "appointment_encode_failed")?, serde_json::to_string(&original).map_err(|_| "appointment_encode_failed")?, appointment_calendar::hold_key(&self.call_sid), Utc::now().timestamp_millis()])
            .map_err(|_| "appointment_intent_store_failed")?;
        check(changed == 1, "appointment_intent_changed")?;
        drop(db);
        // This adapter rechecks the exact original and all relevant calendars
        // immediately before the canonical idempotent tentative-only create.
        let before_write = || {
            check(self.admitted()? == from, "appointment_call_changed")?;
            check(
                (0..60_000).contains(&(Utc::now().timestamp_millis() - proof.verified_ms)),
                "appointment_verification_expired",
            )
        };
        let receipt = self
            .services
            .hold(&config, &self.call_sid, request, &original, &before_write)
            .await?;
        let verified = matches!(receipt.state, HoldState::Verified);
        let result = if verified {
            json!({"status":"tentative_hold_created","business":proof.name,"start":receipt.start,"end":receipt.end,
                "original_appointment_preserved":true,"owner_review_required":true,
                "spoken_guidance":"The proposed time is penciled in tentatively, pending the owner's review. Say that the owner will call back to reschedule if this date does not work. Do not claim a final booking or that the original appointment was cancelled."})
        } else {
            unavailable(true)
        };
        let db = database(&self.root)?;
        let changed = db.execute("UPDATE appointment_proposals SET state=?2,receipt=?3,model_result=?4,updated_ms=?5 WHERE call_sid=?1 AND state='writing'", params![self.call_sid, if verified {"held"} else {"uncertain"}, serde_json::to_string(&receipt).map_err(|_| "appointment_encode_failed")?, result.to_string(), Utc::now().timestamp_millis()])
            .map_err(|_| "appointment_receipt_store_failed")?;
        check(changed == 1, "appointment_receipt_state_changed")?;
        Ok(result)
    }
}

impl AppointmentScheduler for InboundScheduler {
    fn schedule(&self, request: Request) -> SchedulingFuture {
        let scheduler = Self {
            root: self.root.clone(),
            call_sid: self.call_sid.clone(),
            services: Arc::clone(&self.services),
        };
        Box::pin(async move {
            // The whole local workflow fits within the existing call owner.
            // Its future is dropped on disconnect/refusal and never spawned.
            match tokio::time::timeout(Duration::from_secs(50), scheduler.execute(request)).await {
                Ok(Ok(result)) => result,
                _ => unavailable(scheduler.write_may_have_started().unwrap_or(true)),
            }
        })
    }
}

impl InboundScheduler {
    fn write_may_have_started(&self) -> SafeResult<bool> {
        let db = database(&self.root)?;
        let state: Option<String> = db
            .query_row(
                "SELECT state FROM appointment_proposals WHERE call_sid=?1",
                [&self.call_sid],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "appointment_state_read_failed")?;
        Ok(state.is_some_and(|state| matches!(state.as_str(), "writing" | "uncertain" | "held")))
    }
}

pub fn initialize(db: &Connection) -> SafeResult<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS appointment_proposals (
        call_sid TEXT PRIMARY KEY REFERENCES calls(call_sid), request TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('checking','writing','held','unavailable','uncertain')),
        verification TEXT, original TEXT, idempotency_key TEXT, receipt TEXT, model_result TEXT,
        created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL
    );",
    )
    .map_err(|_| "appointment_database_initialize_failed")
}

fn database(root: &Path) -> SafeResult<Connection> {
    common::open_db_with_timeout(root, Duration::from_millis(250))
}

fn unavailable(uncertain: bool) -> Value {
    json!({"status":if uncertain {"outcome_uncertain"} else {"message_only"},
        "spoken_guidance":"I could not confirm the calendar arrangement. I will pass along your proposed date and callback number for review. Do not claim a booking, verification or availability; do not retry scheduling during this call.",
        "owner_review_required":true})
}

fn validate_window(request: &Request, now: DateTime<Utc>) -> SafeResult<()> {
    Request::parse(serde_json::to_value(request).map_err(|_| "appointment_arguments_invalid")?)?;
    let original = DateTime::parse_from_rfc3339(&request.original_start)
        .map_err(|_| "appointment_time_invalid")?;
    let proposed = DateTime::parse_from_rfc3339(&request.proposed_start)
        .map_err(|_| "appointment_time_invalid")?;
    check(
        original >= now - ChronoDuration::days(1)
            && original <= now + ChronoDuration::days(90)
            && proposed > now
            && proposed <= now + ChronoDuration::days(90)
            && proposed != original,
        "appointment_outside_window",
    )
}

#[derive(Serialize, Deserialize)]
struct Verification {
    name: String,
    address: String,
    phone: String,
    map_url: String,
    place_id: String,
    verified_ms: i64,
}

fn normalized_name(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn address_words(value: &str) -> Vec<String> {
    value
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| {
            match word {
                "street" => "st",
                "avenue" => "ave",
                "boulevard" => "blvd",
                "road" => "rd",
                "lane" => "ln",
                "drive" => "dr",
                "court" => "ct",
                "parkway" => "pkwy",
                "suite" => "ste",
                "north" => "n",
                "south" => "s",
                "east" => "e",
                "west" => "w",
                _ => word,
            }
            .to_owned()
        })
        .collect()
}

fn same_branch(requested: &str, listed: &str, city: Option<&str>) -> bool {
    let requested = address_words(requested);
    let listed = address_words(listed);
    // A precise caller-supplied branch address must bind to the same street
    // number. Merely sharing a chain name, city or switchboard is insufficient.
    let number = |words: &[String]| {
        words
            .iter()
            .find(|word| word.as_bytes().first().is_some_and(u8::is_ascii_digit))
            .cloned()
    };
    if requested.len() < 3 || number(&requested).is_none() || number(&requested) != number(&listed)
    {
        return false;
    }
    let mut candidates = listed.into_iter().chain(address_words(city.unwrap_or("")));
    requested
        .iter()
        .all(|word| candidates.by_ref().any(|candidate| &candidate == word))
}

fn phone_number(value: &str, country: Option<&str>) -> Option<String> {
    if value.len() > 64
        || !value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.'))
    {
        return None;
    }
    let compact: String = value
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect();
    if common::e164(&compact) {
        return Some(compact);
    }
    if !matches!(country, Some("US" | "CA")) {
        return None;
    }
    match compact.len() {
        10 if compact.bytes().all(|b| b.is_ascii_digit()) => Some(format!("+1{compact}")),
        11 if compact.starts_with('1') && compact.bytes().all(|b| b.is_ascii_digit()) => {
            Some(format!("+{compact}"))
        }
        _ => None,
    }
}

fn verify_listing(
    response: LookupResponse,
    request: &Request,
    from: &str,
) -> SafeResult<Verification> {
    check(
        response.schema_version == 1
            && response.source == "apple_mapkit"
            && response.status == "ok"
            && !response.truncated
            && response.error_code.is_none(),
        "appointment_maps_unavailable",
    )?;
    let name = normalized_name(&request.business_name);
    check(name.len() >= 5, "appointment_business_ambiguous")?;
    let mut matches = response.items.into_iter().filter(|item| {
        let listed = normalized_name(&item.name);
        listed.len() >= 5
            && (listed.contains(&name) || name.contains(&listed))
            && item
                .phone
                .as_deref()
                .and_then(|phone| phone_number(phone, item.country_code.as_deref()))
                .as_deref()
                == Some(from)
    });
    let item = matches.next().ok_or("appointment_number_not_matched")?;
    check(
        item.address.as_deref().is_some_and(|address| {
            same_branch(&request.business_address, address, item.city.as_deref())
        }),
        "appointment_branch_not_matched",
    )?;
    check(matches.next().is_none(), "appointment_listing_ambiguous")?;
    let proof = Verification {
        name: item.name,
        address: item.address.ok_or("appointment_listing_address_missing")?,
        phone: from.to_owned(),
        map_url: item.map_url.ok_or("appointment_listing_url_missing")?,
        place_id: item.place_id.ok_or("appointment_listing_id_missing")?,
        verified_ms: Utc::now().timestamp_millis(),
    };
    check(
        !proof.address.is_empty() && !proof.place_id.is_empty(),
        "appointment_listing_incomplete",
    )?;
    Ok(proof)
}

/// Deterministic owner-facing evidence, appended outside the model summarizer.
pub fn summary_note(root: &Path, sid: &str) -> SafeResult<Option<String>> {
    let db = database(root)?;
    let exists: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='appointment_proposals'", [], |row| row.get(0)).map_err(|_| "appointment_state_read_failed")?;
    if exists == 0 {
        return Ok(None);
    }
    let row: Option<(String, String, Option<String>)> = db
        .query_row(
            "SELECT state,request,verification FROM appointment_proposals WHERE call_sid=?1",
            [sid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|_| "appointment_state_read_failed")?;
    let Some((state, request, verification)) = row else {
        return Ok(None);
    };
    let request: Request =
        serde_json::from_str(&request).map_err(|_| "appointment_receipt_invalid")?;
    let listing = verification
        .map(|value| serde_json::from_str::<Verification>(&value))
        .transpose()
        .map_err(|_| "appointment_receipt_invalid")?;
    let outcome = match state.as_str() {
        "held" => {
            "A tentative hold was verified in Google Calendar. The original appointment remains unchanged. Review this proposed time; call the business back to reschedule if it does not work."
        }
        "writing" | "uncertain" => {
            "Calendar outcome is uncertain. Check/reconcile the recorded operation before attempting another change; do not assume no event was created."
        }
        _ => "No calendar write was started. Review the caller's proposal and callback request.",
    };
    let listing = listing
        .map(|proof| {
            format!(
                "\nPublic Apple Maps phone matched: {}\nListing: {}",
                proof.phone, proof.map_url
            )
        })
        .unwrap_or_default();
    Ok(Some(format!(
        "Scheduling receipt\nOriginal time supplied: {}\nProposed time: {}\n{}{}\nNo callback has been placed.",
        request.original_start, request.proposed_start, outcome, listing
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_phone_match_never_uses_suffix_or_unbound_country_assumptions() {
        assert_eq!(
            phone_number("+1 (206) 555-0100", None).as_deref(),
            Some("+12065550100")
        );
        assert_eq!(
            phone_number("206.555.0100", Some("US")).as_deref(),
            Some("+12065550100")
        );
        assert!(phone_number("2065550100", Some("GB")).is_none());
        assert!(phone_number("+12065550100 ext 2", Some("US")).is_none());
        assert!(phone_number("5550100", Some("US")).is_none());
    }
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeServices {
        mismatched_number: bool,
        pending_write: bool,
        uncertain_write: bool,
        lookups: AtomicUsize,
        finds: AtomicUsize,
        writes: AtomicUsize,
    }
    impl FakeServices {
        fn new() -> Self {
            Self {
                mismatched_number: false,
                pending_write: false,
                uncertain_write: false,
                lookups: AtomicUsize::new(0),
                finds: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
            }
        }
    }
    impl Services for FakeServices {
        fn lookup<'a>(&'a self, _request: &'a Request) -> Work<'a, LookupResponse> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(LookupResponse {
                    schema_version: 1,
                    source: "apple_mapkit".into(),
                    status: "ok".into(),
                    truncated: false,
                    error_code: None,
                    items: vec![maps_lookup::MapListing {
                        name: "Example Clinic".into(),
                        address: Some("123 Example St, Example City".into()),
                        city: Some("Example City".into()),
                        country_code: Some("US".into()),
                        phone: Some(
                            if self.mismatched_number {
                                "+12065550199"
                            } else {
                                "+12065550100"
                            }
                            .into(),
                        ),
                        place_id: Some("I0123456789ABCDEF".into()),
                        map_url: Some(
                            "https://maps.apple.com/place?place-id=I0123456789ABCDEF".into(),
                        ),
                    }],
                })
            })
        }
        fn find<'a>(
            &'a self,
            _config: &'a Path,
            request: &'a Request,
            _proof: &'a Verification,
        ) -> Work<'a, Event> {
            self.finds.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let start = DateTime::parse_from_rfc3339(&request.original_start).unwrap();
                Ok(Event {
                    calendar_id: "synthetic@example.invalid".into(),
                    id: "synthetic-original".into(),
                    etag: "synthetic-etag".into(),
                    start: request.original_start.clone(),
                    end: (start + ChronoDuration::hours(1)).to_rfc3339(),
                    summary: "Private appointment detail: not for caller".into(),
                    location: "123 Example St, Example City".into(),
                })
            })
        }
        fn hold<'a>(
            &'a self,
            _config: &'a Path,
            sid: &'a str,
            request: &'a Request,
            _original: &'a Event,
            before_write: &'a (dyn Fn() -> SafeResult<()> + Send + Sync),
        ) -> Work<'a, HoldReceipt> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                before_write()?;
                if self.pending_write {
                    std::future::pending::<()>().await;
                }
                let start = DateTime::parse_from_rfc3339(&request.proposed_start).unwrap();
                Ok(HoldReceipt {
                    state: if self.uncertain_write {
                        HoldState::Uncertain
                    } else {
                        HoldState::Verified
                    },
                    idempotency_key: appointment_calendar::hold_key(sid),
                    event_id: "synthetic-hold".into(),
                    calendar_id: "synthetic@example.invalid".into(),
                    start: request.proposed_start.clone(),
                    end: (start + ChronoDuration::hours(1)).to_rfc3339(),
                    structured: json!({"private":"must not reach voice model"}),
                })
            })
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        sid: String,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let native = directory.path().join("native");
            common::private_dir(&native).unwrap();
            common::private_dir(&native.join("extensions")).unwrap();
            let root = native.join("extensions/phone");
            common::private_dir(&root).unwrap();
            let config = common::PhoneConfig {
                voice: common::VoiceConfig::default(),
                enabled: true,
                port: 43335,
                public_base: "https://phone.example.invalid".into(),
                account_sid: format!("AC{}", "1".repeat(32)),
                auth_token: "enc2:synthetic-not-decrypted".into(),
                from_number: "+12065550200".into(),
                forwarded_from: "+12065550300".into(),
                max_duration_secs: 180,
                recording_consent: common::RecordingConsentMode::Explicit,
                telegram_alias: "synthetic".into(),
                telegram_peer_group: "synthetic".into(),
                telegram_bot_username: "synthetic".into(),
                openai_key_path: "synthetic".into(),
                voicemail: None,
                tentative_rescheduling: true,
            };
            common::atomic_private_write(
                &root.join("phone.toml"),
                toml::to_string(&config).unwrap().as_bytes(),
            )
            .unwrap();
            let db = common::open_db(&root).unwrap();
            crate::outbound::initialize(&db).unwrap();
            initialize(&db).unwrap();
            let sid = format!("CA{}", "2".repeat(32));
            db.execute("INSERT INTO calls(call_sid,account_sid,from_candidate,consent,consent_token,created_ms,phase) VALUES(?1,?2,'+12065550100',1,'synthetic-nonce',?3,'active')",params![sid,config.account_sid,Utc::now().timestamp_millis()]).unwrap();
            Self {
                _directory: directory,
                root,
                sid,
            }
        }
        fn scheduler(&self, services: Arc<FakeServices>) -> InboundScheduler {
            InboundScheduler {
                root: self.root.clone(),
                call_sid: self.sid.clone(),
                services,
            }
        }
        fn request(&self) -> Request {
            Request {
                business_name: "Example Clinic".into(),
                business_address: "123 Example St, Example City".into(),
                original_start: (Utc::now() + ChronoDuration::days(1))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                proposed_start: (Utc::now() + ChronoDuration::days(2))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                caller_confirmed: true,
            }
        }
        fn state(&self) -> String {
            database(&self.root)
                .unwrap()
                .query_row(
                    "SELECT state FROM appointment_proposals WHERE call_sid=?1",
                    [&self.sid],
                    |row| row.get(0),
                )
                .unwrap()
        }
    }

    #[tokio::test]
    async fn shared_business_number_does_not_authorize_a_different_branch() {
        let fixture = Fixture::new();
        let fake = Arc::new(FakeServices::new());
        let mut request = fixture.request();
        request.business_address = "999 Other St, Example City".into();
        let result = fixture.scheduler(fake.clone()).schedule(request).await;
        assert_eq!(result["status"], "message_only");
        assert_eq!(fake.finds.load(Ordering::SeqCst), 0);
        assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.state(), "unavailable");
    }

    #[tokio::test]
    async fn unmatched_public_number_cannot_read_calendar_or_write() {
        let fixture = Fixture::new();
        let mut fake = FakeServices::new();
        fake.mismatched_number = true;
        let fake = Arc::new(fake);
        let result = fixture
            .scheduler(fake.clone())
            .schedule(fixture.request())
            .await;
        assert_eq!(result["status"], "message_only");
        assert_eq!(fake.finds.load(Ordering::SeqCst), 0);
        assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.state(), "unavailable");
    }

    #[tokio::test]
    async fn one_verified_hold_is_durable_idempotent_and_returns_no_private_event_details() {
        let fixture = Fixture::new();
        let fake = Arc::new(FakeServices::new());
        let scheduler = fixture.scheduler(fake.clone());
        let request = fixture.request();
        let result = scheduler.schedule(request.clone()).await;
        assert_eq!(result["status"], "tentative_hold_created");
        assert_eq!(fixture.state(), "held");
        assert!(!result.to_string().contains("Private appointment"));
        assert!(!result.to_string().contains("synthetic@example"));
        assert!(!result.to_string().contains("must not reach"));
        assert_eq!(scheduler.schedule(request.clone()).await, result);
        assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        let mut changed = request;
        changed.proposed_start = (Utc::now() + ChronoDuration::days(3)).to_rfc3339();
        assert_eq!(
            scheduler.schedule(changed).await["status"],
            "outcome_uncertain"
        );
        assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
        let note = summary_note(&fixture.root, &fixture.sid).unwrap().unwrap();
        assert!(note.contains("original appointment remains unchanged"));
        assert!(note.contains("call the business back"));
        assert!(note.contains("maps.apple.com/place"));
    }

    #[tokio::test]
    async fn interrupted_write_retains_uncertainty_and_never_replays() {
        let fixture = Fixture::new();
        let mut fake = FakeServices::new();
        fake.pending_write = true;
        let fake = Arc::new(fake);
        let scheduler = fixture.scheduler(fake.clone());
        let request = fixture.request();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                scheduler.schedule(request.clone())
            )
            .await
            .is_err()
        );
        assert_eq!(fixture.state(), "writing");
        assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
        assert_eq!(
            scheduler.schedule(request).await["status"],
            "outcome_uncertain"
        );
        assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
        assert!(
            summary_note(&fixture.root, &fixture.sid)
                .unwrap()
                .unwrap()
                .contains("outcome is uncertain")
        );
    }

    #[tokio::test]
    async fn revoked_live_policy_and_ended_call_cannot_schedule() {
        let fixture = Fixture::new();
        let fake = Arc::new(FakeServices::new());
        let scheduler = fixture.scheduler(fake.clone());
        let path = fixture.root.join("phone.toml");
        let text = common::private_read(&path).unwrap().replace(
            "tentative_rescheduling = true",
            "tentative_rescheduling = false",
        );
        common::atomic_private_write(&path, text.as_bytes()).unwrap();
        assert_eq!(
            scheduler.schedule(fixture.request()).await["status"],
            "message_only"
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0);
        let text = text.replace(
            "tentative_rescheduling = false",
            "tentative_rescheduling = true",
        );
        common::atomic_private_write(&path, text.as_bytes()).unwrap();
        database(&fixture.root)
            .unwrap()
            .execute(
                "UPDATE calls SET phase='ended' WHERE call_sid=?1",
                [&fixture.sid],
            )
            .unwrap();
        assert_eq!(
            scheduler.schedule(fixture.request()).await["status"],
            "message_only"
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0);
    }
}
