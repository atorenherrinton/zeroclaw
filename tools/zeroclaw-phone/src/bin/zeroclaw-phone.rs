use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path as AxumPath, State, ws::WebSocketUpgrade},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroclaw_phone_extension::{
    common::{self, SafeResult, Settings, check},
    outbound,
    protocol::{self, Form},
    realtime, recording, summary,
};

struct App {
    root: PathBuf,
    slots: Arc<tokio::sync::Semaphore>,
}

type ExistingCall = (String, String, Option<String>, Option<bool>, i64);

struct Ingress {
    slots: Arc<tokio::sync::Semaphore>,
    rate: std::sync::Mutex<(std::time::Instant, u32)>,
}

async fn bound_request(
    State(ingress): State<Arc<Ingress>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // This wraps body extraction too: a slow body cannot hold an admission slot indefinitely.
    let Ok(_permit) = ingress.slots.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Busy").into_response();
    };
    let admitted = match ingress.rate.lock() {
        Ok(mut rate) => {
            if rate.0.elapsed() >= std::time::Duration::from_secs(60) {
                *rate = (std::time::Instant::now(), 0);
            }
            rate.1 += 1;
            rate.1 <= 240
        }
        Err(_) => false,
    };
    if !admitted {
        return (StatusCode::TOO_MANY_REQUESTS, "Limited").into_response();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(10), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::REQUEST_TIMEOUT, "Timeout").into_response(),
    }
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate => {} }
}

fn xml(body: impl Into<String>) -> Response {
    (
        [("content-type", "application/xml; charset=utf-8")],
        body.into(),
    )
        .into_response()
}
fn fail() -> Response {
    (StatusCode::FORBIDDEN, "Rejected").into_response()
}

fn authenticate(cfg: &Settings, uri: &Uri, headers: &HeaderMap, bytes: &[u8]) -> SafeResult<Form> {
    let form = protocol::parse_form(bytes)?;
    check(
        headers
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("application/x-www-form-urlencoded")),
        "unsupported_form_content_type",
    )?;
    let signature = headers
        .get("x-twilio-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or("signature_missing")?;
    let url = format!(
        "{}{}",
        cfg.public_base,
        uri.path_and_query().ok_or("invalid_request_uri")?
    );
    check(
        protocol::signature_ok(&cfg.auth_token, &url, &form, signature),
        "signature_invalid",
    )?;
    check(
        protocol::one(&form, "AccountSid")? == cfg.account_sid
            && protocol::valid_sid(protocol::one(&form, "CallSid")?, "CA"),
        "call_identity_invalid",
    )?;
    Ok(form)
}

fn authorize_followup(form: &Form, cfg: &Settings) -> SafeResult<()> {
    if let Some(value) = form.get("To") {
        check(value == &cfg.from_number, "wrong_called_number")?;
    }
    if let Some(value) = form.get("ForwardedFrom") {
        check(value == &cfg.forwarded_from, "wrong_forwarded_number")?;
    }
    if let Some(value) = form.get("Direction") {
        check(value == "inbound", "wrong_direction")?;
    }
    Ok(())
}

async fn webhook(
    State(app): State<Arc<App>>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    match initial(&app.root, &uri, &headers, &bytes) {
        Ok(value) => xml(value),
        Err(_) => fail(),
    }
}

fn initial(root: &Path, uri: &Uri, headers: &HeaderMap, bytes: &[u8]) -> SafeResult<String> {
    let cfg = common::load(root)?;
    check(cfg.enabled, "phone_disabled")?;
    let form = authenticate(&cfg, uri, headers, bytes)?;
    authorize_followup(&form, &cfg)?;
    let sid = protocol::one(&form, "CallSid")?;
    let mut db = common::open_db(root)?;
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| "transaction_failed")?;
    let existing: Option<ExistingCall> = tx.query_row(
        "SELECT phase,consent_token,media_token,consent,created_ms FROM calls WHERE call_sid=? AND account_sid=?",
        params![sid,cfg.account_sid], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))
        .optional().map_err(|_| "call_lookup_failed")?;
    let terminal = form.get("CallStatus").is_some_and(|v| {
        ["completed", "busy", "failed", "no-answer", "canceled"].contains(&v.as_str())
    });
    if uri.query() == Some("type=status") || terminal {
        if terminal && existing.is_some() {
            tx.execute("UPDATE calls SET phase='ended' WHERE call_sid=?", [sid])
                .map_err(|_| "call_status_failed")?;
        }
        tx.commit().map_err(|_| "transaction_commit_failed")?;
        return Ok(protocol::EMPTY.into());
    }
    check(uri.query().is_none(), "unexpected_query")?;
    let now = chrono::Utc::now().timestamp_millis();
    if let Some((phase, consent_token, media_token, consent, created)) = existing {
        if now - created > 180_000 || ["active", "ended", "expired"].contains(&phase.as_str()) {
            return Ok(protocol::EMPTY.into());
        }
        return Ok(if phase == "consent" {
            protocol::consent_xml(&cfg.public_base, &consent_token)
        } else {
            protocol::connect_xml(
                &cfg.public_base,
                &media_token.ok_or("missing_media_nonce")?,
                consent == Some(true),
            )
        });
    }
    check(
        protocol::one(&form, "To")? == cfg.from_number
            && protocol::one(&form, "ForwardedFrom")? == cfg.forwarded_from
            && protocol::one(&form, "Direction")? == "inbound",
        "not_admitted_forwarded_call",
    )?;
    let from = form
        .get("From")
        .filter(|v| common::e164(v))
        .cloned()
        .unwrap_or_default();
    tx.execute(
        "UPDATE calls SET phase='expired' WHERE phase NOT IN ('ended','expired') AND created_ms<?",
        [now - 180_000],
    )
    .map_err(|_| "call_expiry_failed")?;
    let active: i64 = tx
        .query_row(
            "SELECT count(*) FROM calls WHERE phase NOT IN ('ended','expired')",
            [],
            |r| r.get(0),
        )
        .map_err(|_| "call_count_failed")?;
    if active != 0 {
        return Ok(protocol::REJECT.into());
    }
    let nonce = uuid::Uuid::new_v4().to_string();
    tx.execute("INSERT INTO calls(call_sid,account_sid,from_candidate,consent_token,created_ms,phase) VALUES(?,?,?,?,?,'consent')",
        params![sid,cfg.account_sid,from,nonce,now]).map_err(|_| "call_insert_failed")?;
    tx.commit().map_err(|_| "transaction_commit_failed")?;
    Ok(protocol::consent_xml(&cfg.public_base, &nonce))
}

async fn consent(
    State(app): State<Arc<App>>,
    AxumPath(nonce): AxumPath<String>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    match consent_result(&app.root, &nonce, &uri, &headers, &bytes) {
        Ok(v) => xml(v),
        Err(_) => fail(),
    }
}

fn consent_result(
    root: &Path,
    nonce: &str,
    uri: &Uri,
    headers: &HeaderMap,
    bytes: &[u8],
) -> SafeResult<String> {
    check(uuid::Uuid::parse_str(nonce).is_ok(), "invalid_nonce")?;
    let cfg = common::load(root)?;
    check(cfg.enabled, "phone_disabled")?;
    let form = authenticate(&cfg, uri, headers, bytes)?;
    authorize_followup(&form, &cfg)?;
    let sid = protocol::one(&form, "CallSid")?;
    let mut db = common::open_db(root)?;
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| "transaction_failed")?;
    let (phase,created,existing,old_consent):(String,i64,Option<String>,Option<bool>)=tx.query_row(
        "SELECT phase,created_ms,media_token,consent FROM calls WHERE call_sid=? AND account_sid=? AND consent_token=?",params![sid,cfg.account_sid,nonce],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_|"call_not_admitted")?;
    check(
        chrono::Utc::now().timestamp_millis() - created <= 180_000,
        "consent_expired",
    )?;
    if phase == "media" {
        return Ok(protocol::connect_xml(
            &cfg.public_base,
            &existing.ok_or("missing_media_nonce")?,
            old_consent == Some(true),
        ));
    }
    if phase != "consent" {
        return Ok(protocol::EMPTY.into());
    }
    let record = form.get("Digits").is_some_and(|v| v == "1");
    let media = uuid::Uuid::new_v4().to_string();
    tx.execute("UPDATE calls SET consent=?,media_token=?,phase='media' WHERE call_sid=? AND phase='consent'",params![record,media,sid]).map_err(|_|"consent_save_failed")?;
    tx.commit().map_err(|_| "transaction_commit_failed")?;
    Ok(protocol::connect_xml(&cfg.public_base, &media, record))
}

async fn recording_callback(
    State(app): State<Arc<App>>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let result = (|| -> SafeResult<()> {
        let cfg = common::load(&app.root)?;
        let f = authenticate(&cfg, &uri, &headers, &bytes)?;
        let sid = protocol::one(&f, "CallSid")?;
        let db = common::open_db(&app.root)?;
        let consent: Option<bool> = db
            .query_row(
                "SELECT consent FROM calls WHERE call_sid=? AND account_sid=?",
                params![sid, cfg.account_sid],
                |r| r.get(0),
            )
            .map_err(|_| "call_not_admitted")?;
        check(consent == Some(true), "recording_not_consented")?;
        recording::enqueue(
            &db,
            &cfg.account_sid,
            sid,
            protocol::one(&f, "RecordingSid")?,
            protocol::one(&f, "RecordingStatus")?,
        )
    })();
    if result.is_ok() {
        xml(protocol::EMPTY)
    } else {
        fail()
    }
}

async fn outbound_answer(
    State(app): State<Arc<App>>,
    AxumPath(nonce): AxumPath<String>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let result = (|| -> SafeResult<String> {
        let cfg = common::load(&app.root)?;
        check(cfg.enabled, "phone_disabled")?;
        let form = authenticate(&cfg, &uri, &headers, &bytes)?;
        outbound::answer(&app.root, &nonce, &cfg, &form)
    })();
    match result {
        Ok(value) => xml(value),
        Err(_) => fail(),
    }
}

async fn outbound_status(
    State(app): State<Arc<App>>,
    AxumPath(nonce): AxumPath<String>,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let result = (|| -> SafeResult<()> {
        let cfg = common::load(&app.root)?;
        let form = authenticate(&cfg, &uri, &headers, &bytes)?;
        outbound::update_status(&app.root, &nonce, &cfg, &form)
    })();
    if result.is_ok() {
        xml(protocol::EMPTY)
    } else {
        fail()
    }
}

async fn media(
    State(app): State<Arc<App>>,
    AxumPath(nonce): AxumPath<String>,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let result=(||->SafeResult<(Settings,String,String,bool,u64,tokio::sync::OwnedSemaphorePermit)> {
        check(uuid::Uuid::parse_str(&nonce).is_ok(),"invalid_nonce")?;
        let cfg=common::load(&app.root)?; check(cfg.enabled,"phone_disabled")?;
        let sig=headers.get("x-twilio-signature").and_then(|v|v.to_str().ok()).ok_or("signature_missing")?;
        let url=format!("{}{}",cfg.public_base,uri.path_and_query().ok_or("invalid_uri")?);
        check(protocol::websocket_signature_ok(&cfg.auth_token,&url,sig),"signature_invalid")?;
        let mut db=common::open_db(&app.root)?;
        let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_|"transaction_failed")?;
        let (sid,from,consent,created):(String,String,bool,i64)=tx.query_row(
            "SELECT call_sid,from_candidate,consent,created_ms FROM calls WHERE media_token=? AND phase='media' AND account_sid=?",params![nonce,cfg.account_sid],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_|"media_not_admitted")?;
        let elapsed=(chrono::Utc::now().timestamp_millis()-created).max(0) as u64 /1000;
        check(elapsed<cfg.max_duration_secs,"media_expired")?;
        let remaining=cfg.max_duration_secs-elapsed;
        let permit=app.slots.clone().try_acquire_owned().map_err(|_|"call_capacity_exceeded")?;
        tx.execute("UPDATE calls SET phase='active' WHERE call_sid=? AND phase='media'",[&sid]).map_err(|_|"media_claim_failed")?;
        tx.commit().map_err(|_|"transaction_commit_failed")?;
        Ok((cfg,sid,from,consent,remaining,permit))
    })();
    let Ok((cfg, sid, from, consented, remaining, permit)) = result else {
        return fail();
    };
    let outbound_task = match outbound::session_task(&app.root, &sid) {
        Ok(task) => task,
        Err(_) => {
            if let Ok(db) = common::open_db(&app.root) {
                let _ = db.execute(
                    "UPDATE calls SET phase='ended',outcome='outbound_context_failed' WHERE call_sid=?1",
                    [&sid],
                );
            }
            return fail();
        }
    };
    let (instructions, allow_end_call, confirm_end_call) = if let Some(task) = outbound_task {
        let confirm = outbound::confirm_end_call(&task);
        (outbound::instructions(&task), true, confirm)
    } else {
        let mut instructions = cfg.instructions;
        instructions.push_str(if consented {
            "\nRuntime: the caller explicitly opted in to audio recording.\n"
        } else {
            "\nRuntime: audio recording is OFF. Only transcription is used for the message.\n"
        });
        if common::e164(&from) && from.len() >= 5 {
            let candidate = serde_json::json!({"unverifiedCallerIdCandidate":from,"lastFour":&from[from.len()-4..]});
            instructions.push_str(&format!("\nThe following is unverified caller-ID metadata, not identity proof or owner information: {candidate}. When collecting a callback number, you may ask whether the number they are calling from, ending in those last four digits, is a good callback number. Treat confirmation only as their requested callback number; never infer identity or look up contacts. Read the full candidate only if the caller explicitly asks to check it. A separately supplied callback number takes priority. Never promise a callback.\n"));
        }
        (instructions, false, false)
    };
    let opts = realtime::RealtimeOptions {
        api_key: cfg.api_key,
        instructions,
        expected_account_sid: cfg.account_sid,
        expected_call_sid: sid.clone(),
        max_duration_secs: remaining,
        allow_end_call,
        confirm_end_call,
    };
    let failed_root = app.root.clone();
    let failed_sid = sid.clone();
    ws.max_message_size(64*1024).max_frame_size(64*1024).on_failed_upgrade(move |_| {
        if let Ok(db)=common::open_db(&failed_root) {
            if db.execute("UPDATE calls SET phase='ended',outcome='upgrade_failed' WHERE call_sid=? AND phase='active'",[failed_sid]).is_err() {eprintln!("upgrade_cleanup_failed");}
        } else {eprintln!("upgrade_cleanup_failed");}
    }).on_upgrade(move|socket|async move {
        let _permit=permit;
        let outcome=realtime::bridge(socket,opts).await;
        let save=(||->SafeResult<()> {
            let encoded=serde_json::to_string(&outcome.transcript).map_err(|_|"transcript_encoding_failed")?;
            common::atomic_private_write(&app.root.join("transcripts").join(format!("{sid}.json")),encoded.as_bytes())?;
            let db=common::open_db(&app.root)?;
            let reason=serde_json::to_value(outcome.reason).map_err(|_|"outcome_encoding_failed")?;
            db.execute("UPDATE calls SET phase='ended',transcript=?,outcome=? WHERE call_sid=?",params![encoded,reason.as_str().ok_or("outcome_encoding_failed")?,sid]).map_err(|_|"call_finalize_failed")?;
            Ok(())
        })();
        if let Err(error)=save { eprintln!("{error}"); }
    })
}

async fn serve(root: PathBuf) -> SafeResult<()> {
    let cfg = common::load(&root)?;
    common::private_dir(&root.join("transcripts"))?;
    common::private_dir(&root.join("recordings"))?;
    let db = common::open_db(&root)?;
    recording::initialize(&db)?;
    outbound::initialize(&db)?;
    let app = Arc::new(App {
        root: root.clone(),
        slots: Arc::new(tokio::sync::Semaphore::new(1)),
    });
    let ingress = Arc::new(Ingress {
        slots: Arc::new(tokio::sync::Semaphore::new(16)),
        rate: std::sync::Mutex::new((std::time::Instant::now(), 0)),
    });
    let router = Router::new()
        .route(
            "/voice/health",
            get(|| async { axum::Json(serde_json::json!({"ok":true})) }),
        )
        .route("/voice/webhook", post(webhook))
        .route("/voice/consent/{nonce}", post(consent))
        .route("/voice/media/{nonce}", get(media))
        .route("/voice/recording", post(recording_callback))
        .route("/voice/outbound/{nonce}", post(outbound_answer))
        .route("/voice/outbound-status/{nonce}", post(outbound_status))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(axum::middleware::from_fn_with_state(ingress, bound_request))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, cfg.port))
        .await
        .map_err(|_| "listener_bind_failed")?;
    // A successfully bound sole listener owns recovery. Never let an expired
    // socket claim survive restart or prevent the next legitimate call.
    db.execute("UPDATE calls SET phase='ended',outcome='service_interrupted',summary_status=CASE WHEN transcript IS NULL THEN 'needs_review' ELSE summary_status END WHERE phase='active'",[]).map_err(|_|"call_recovery_failed")?;
    let recordings_root = root.clone();
    let worker = zeroclaw_spawn::spawn!(async move { recording::run(recordings_root).await });
    let summaries = zeroclaw_spawn::spawn!(async move { summary::run(root).await });
    tokio::select! {
        result = axum::serve(listener,router).with_graceful_shutdown(shutdown_signal()).into_future() => result.map_err(|_|"listener_failed"),
        result = worker => match result { Ok(Err(error))=>Err(error), _=>Err("recording_worker_stopped") },
        result = summaries => match result { Ok(Err(error))=>Err(error), _=>Err("summary_worker_stopped") }
    }
}

async fn bounded_json(mut response: reqwest::Response) -> SafeResult<serde_json::Value> {
    check(response.status().is_success(), "preflight_remote_rejected")?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "preflight_body_failed")?
    {
        check(
            bytes.len() + chunk.len() <= 256 * 1024,
            "preflight_body_too_large",
        )?;
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "preflight_body_invalid")
}

async fn preflight(root: &Path) -> SafeResult<()> {
    // Read-only remote checks. Never place a call or transmit call content.
    let cfg = common::load(root)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| "preflight_client_failed")?;
    recording::validate_private_destination(&client, &cfg).await?;
    let endpoint = format!(
        "https://api.twilio.com/2010-04-01/Accounts/{}/IncomingPhoneNumbers.json",
        cfg.account_sid
    );
    let numbers = bounded_json(
        client
            .get(endpoint)
            .basic_auth(&cfg.account_sid, Some(&cfg.auth_token))
            .query(&[
                ("PhoneNumber", cfg.from_number.as_str()),
                ("PageSize", "10"),
            ])
            .send()
            .await
            .map_err(|_| "phone_route_preflight_failed")?,
    )
    .await?;
    let rows = numbers
        .get("incoming_phone_numbers")
        .and_then(serde_json::Value::as_array)
        .ok_or("phone_route_response_invalid")?;
    check(rows.len() == 1, "phone_route_not_unique")?;
    let row = &rows[0];
    check(
        row["account_sid"] == cfg.account_sid && row["phone_number"] == cfg.from_number,
        "phone_route_identity_mismatch",
    )?;
    check(
        row["voice_url"] == format!("{}/voice/webhook", cfg.public_base)
            && row["voice_method"] == "POST",
        "phone_webhook_route_mismatch",
    )?;
    for key in ["voice_application_sid", "trunk_sid"] {
        check(
            row.get(key).is_none_or(|v| v.is_null() || v == ""),
            "phone_route_overridden",
        )?;
    }
    let mut active = 0usize;
    for status in ["queued", "ringing", "in-progress"] {
        let endpoint = format!(
            "https://api.twilio.com/2010-04-01/Accounts/{}/Calls.json",
            cfg.account_sid
        );
        let result = bounded_json(
            client
                .get(endpoint)
                .basic_auth(&cfg.account_sid, Some(&cfg.auth_token))
                .query(&[
                    ("Status", status),
                    ("To", cfg.from_number.as_str()),
                    ("PageSize", "100"),
                ])
                .send()
                .await
                .map_err(|_| "active_call_preflight_failed")?,
        )
        .await?;
        active += result
            .get("calls")
            .and_then(serde_json::Value::as_array)
            .ok_or("active_call_response_invalid")?
            .len();
        check(
            result.get("next_page_uri").is_none_or(|v| v.is_null()),
            "active_call_page_overflow",
        )?;
    }
    println!("{{\"phoneRouteReady\":true,\"privateTelegramReady\":true,\"activeCalls\":{active}}}");
    Ok(())
}

async fn route_check(root: &Path) -> SafeResult<()> {
    let cfg = common::load(root)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| "route_client_failed")?;
    let tunnels = bounded_json(
        client
            .get("http://127.0.0.1:4040/api/tunnels")
            .send()
            .await
            .map_err(|_| "tunnel_unavailable")?,
    )
    .await?;
    let matches = tunnels
        .get("tunnels")
        .and_then(serde_json::Value::as_array)
        .ok_or("tunnel_response_invalid")?
        .iter()
        .filter(|t| {
            t["public_url"] == cfg.public_base
                && t["config"]["addr"] == format!("http://127.0.0.1:{}", cfg.port)
        })
        .count();
    check(matches == 1, "tunnel_route_mismatch")?;
    let health = bounded_json(
        client
            .get(format!("{}/voice/health", cfg.public_base))
            .send()
            .await
            .map_err(|_| "public_route_failed")?,
    )
    .await?;
    check(
        health == serde_json::json!({"ok":true}),
        "public_route_unhealthy",
    )?;
    let rejected = client
        .post(format!("{}/voice/webhook", cfg.public_base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .map_err(|_| "public_rejection_check_failed")?;
    check(
        rejected.status() == reqwest::StatusCode::FORBIDDEN,
        "unsigned_request_not_rejected",
    )?;
    println!(
        "{{\"independentTunnelReady\":true,\"publicPhoneHealthy\":true,\"unsignedRequestsRejected\":true}}"
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    // This process owns only the private phone-extension state.
    unsafe {
        libc::umask(0o077);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: zeroclaw-phone serve|mcp|check|probe|preflight|route-check ROOT");
        std::process::exit(2);
    }
    let root = PathBuf::from(&args[2]);
    let result = match args[1].as_str() {
        "serve" => serve(root).await,
        "mcp" => outbound::run_mcp(&root).await,
        "check" => common::load(&root).map(|_| println!("{{\"configValid\":true}}")),
        "probe" => match common::load(&root) {
            Ok(cfg) => realtime::probe(&cfg.api_key)
                .await
                .map(|_| println!("{{\"realtimeReady\":true}}")),
            Err(e) => Err(e),
        },
        "preflight" => preflight(&root).await,
        "route-check" => route_check(&root).await,
        _ => Err("unknown_mode"),
    };
    if let Err(error) = result {
        eprintln!("{{\"ok\":false,\"code\":\"{error}\"}}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use zeroclaw_config::secrets::SecretStore;

    const ACCOUNT: &str = "AC00000000000000000000000000000000";
    const CALL: &str = "CA00000000000000000000000000000000";
    const SECOND_CALL: &str = "CA11111111111111111111111111111111";
    const RECORDING: &str = "RE00000000000000000000000000000000";
    const BASE: &str = "https://phone.test.invalid";
    const AUTH: &str = "fixture-twilio-auth-token";
    const TO: &str = "+15550001001";
    const FORWARDED: &str = "+15550001002";
    const OUTBOUND_TO: &str = "+15550001004";

    struct Fixture {
        _directory: tempfile::TempDir,
        native: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let native = directory.path().join("native");
            common::private_dir(&native).unwrap();
            common::private_dir(&native.join("extensions")).unwrap();
            let root = native.join("extensions/phone");
            common::private_dir(&root).unwrap();
            let store = SecretStore::new(&native, true);
            let auth = store.encrypt(AUTH).unwrap();
            let telegram = store.encrypt("12345:fixture_telegram_secret").unwrap();
            let api_key = store.encrypt("fixture-model-key-no-network").unwrap();
            let native_text = format!(
                "[channels.telegram.fixture]\nenabled=true\nbot_token=\"{telegram}\"\n\
                 [peer_groups.fixture]\nchannel=\"telegram.fixture\"\nexternal_peers=[\"12345\"]\n\
                 [providers.transcription.openai.fixture]\napi_key=\"{api_key}\"\n"
            );
            common::atomic_private_write(&native.join("config.toml"), native_text.as_bytes())
                .unwrap();
            let config = common::PhoneConfig {
                enabled: true,
                port: 43335,
                public_base: BASE.into(),
                account_sid: ACCOUNT.into(),
                auth_token: auth,
                from_number: TO.into(),
                forwarded_from: FORWARDED.into(),
                max_duration_secs: 180,
                telegram_alias: "fixture".into(),
                telegram_peer_group: "fixture".into(),
                telegram_bot_username: "fixture_bot".into(),
                openai_key_path: "providers.transcription.openai.fixture.api_key".into(),
            };
            common::atomic_private_write(
                &root.join("phone.toml"),
                toml::to_string(&config).unwrap().as_bytes(),
            )
            .unwrap();
            common::atomic_private_write(
                &root.join("screening.md"),
                b"Offline fixture. No tools or external actions.",
            )
            .unwrap();
            let db = common::open_db(&root).unwrap();
            recording::initialize(&db).unwrap();
            outbound::initialize(&db).unwrap();
            Self {
                _directory: directory,
                native,
                root,
            }
        }

        fn db(&self) -> rusqlite::Connection {
            common::open_db(&self.root).unwrap()
        }

        fn count(&self) -> i64 {
            self.db()
                .query_row("SELECT count(*) FROM calls", [], |row| row.get(0))
                .unwrap()
        }

        fn recording_count(&self) -> i64 {
            self.db()
                .query_row("SELECT count(*) FROM recording_outbox", [], |row| {
                    row.get(0)
                })
                .unwrap()
        }

        fn nonce(&self) -> String {
            self.db()
                .query_row(
                    "SELECT consent_token FROM calls WHERE call_sid=?",
                    [CALL],
                    |row| row.get(0),
                )
                .unwrap()
        }

        fn native_edit(&self, edit: impl FnOnce(&mut toml::Value)) {
            let path = self.native.join("config.toml");
            let mut value: toml::Value =
                toml::from_str(&common::private_read(&path).unwrap()).unwrap();
            edit(&mut value);
            common::atomic_private_write(&path, toml::to_string(&value).unwrap().as_bytes())
                .unwrap();
        }

        fn initial(&self, form: &Form) -> SafeResult<String> {
            let request = signed("/voice/webhook", form);
            initial(&self.root, &request.uri, &request.headers, &request.body)
        }

        fn choose(&self, digits: Option<&str>) -> SafeResult<String> {
            let nonce = self.nonce();
            let mut form = valid_form();
            form.insert("CallStatus".into(), "in-progress".into());
            if let Some(digits) = digits {
                form.insert("Digits".into(), digits.into());
            }
            let request = signed(&format!("/voice/consent/{nonce}"), &form);
            consent_result(
                &self.root,
                &nonce,
                &request.uri,
                &request.headers,
                &request.body,
            )
        }
    }

    struct SignedRequest {
        uri: Uri,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    fn valid_form() -> Form {
        [
            ("AccountSid", ACCOUNT),
            ("CallSid", CALL),
            ("To", TO),
            ("ForwardedFrom", FORWARDED),
            ("Direction", "inbound"),
            ("From", "+15550001003"),
            ("CallStatus", "ringing"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    fn signed(path: &str, form: &Form) -> SignedRequest {
        let mut message = format!("{BASE}{path}").into_bytes();
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in form {
            message.extend_from_slice(key.as_bytes());
            message.extend_from_slice(value.as_bytes());
            serializer.append_pair(key, value);
        }
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, AUTH.as_bytes());
        let signature = STANDARD.encode(ring::hmac::sign(&key, &message).as_ref());
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/x-www-form-urlencoded; charset=utf-8"
                .parse()
                .unwrap(),
        );
        headers.insert("x-twilio-signature", signature.parse().unwrap());
        SignedRequest {
            uri: path.parse().unwrap(),
            headers,
            body: serializer.finish().into_bytes(),
        }
    }

    fn outbound_form(status: &str) -> Form {
        [
            ("AccountSid", ACCOUNT),
            ("CallSid", SECOND_CALL),
            ("To", OUTBOUND_TO),
            ("From", TO),
            ("Direction", "outbound-api"),
            ("CallStatus", status),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    #[test]
    fn signed_outbound_webhooks_bind_exact_request_and_track_terminal_status() {
        let fixture = Fixture::new();
        let request_id = uuid::Uuid::new_v4().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        fixture.db().execute(
            "INSERT INTO outbound_requests(request_id,nonce,to_number,on_behalf_of,recipient,purpose,created_ms,state)
             VALUES(?1,?2,?3,'Owner','Example shop','Ask about Tuesday availability',?4,'creating')",
            params![request_id,nonce,OUTBOUND_TO,chrono::Utc::now().timestamp_millis()],
        ).unwrap();

        let path = format!("/voice/outbound/{nonce}");
        let mut answer_form = outbound_form("in-progress");
        answer_form.insert("AnsweredBy".into(), "human".into());
        let answer_request = signed(&path, &answer_form);
        let cfg = common::load(&fixture.root).unwrap();
        let form = authenticate(
            &cfg,
            &answer_request.uri,
            &answer_request.headers,
            &answer_request.body,
        )
        .unwrap();
        let twiml = outbound::answer(&fixture.root, &nonce, &cfg, &form).unwrap();
        assert!(twiml.contains("<Connect><Stream"));
        assert!(!twiml.contains("<Recording"));
        let row: (String, String) = fixture
            .db()
            .query_row(
                "SELECT phase,summary_status FROM calls WHERE call_sid=?1",
                [SECOND_CALL],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("media".into(), "pending".into()));
        let task = outbound::session_task(&fixture.root, SECOND_CALL)
            .unwrap()
            .unwrap();
        assert_eq!(task.recipient, "Example shop");
        assert_eq!(task.answer_kind, "human");

        let status_path = format!("/voice/outbound-status/{nonce}");
        let status_request = signed(&status_path, &outbound_form("completed"));
        let form = authenticate(
            &cfg,
            &status_request.uri,
            &status_request.headers,
            &status_request.body,
        )
        .unwrap();
        outbound::update_status(&fixture.root, &nonce, &cfg, &form).unwrap();
        let state: String = fixture
            .db()
            .query_row(
                "SELECT state FROM outbound_requests WHERE request_id=?1",
                [&request_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "completed");
        let phase: String = fixture
            .db()
            .query_row(
                "SELECT phase FROM calls WHERE call_sid=?1",
                [SECOND_CALL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "ended");
    }

    #[test]
    fn unresolved_or_announcement_like_answers_are_screened_before_realtime() {
        let fixture = Fixture::new();
        let request_id = uuid::Uuid::new_v4().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        fixture.db().execute(
            "INSERT INTO outbound_requests(request_id,nonce,to_number,on_behalf_of,recipient,purpose,created_ms,state)
             VALUES(?1,?2,?3,'Owner','','Purpose',?4,'creating')",
            params![request_id,nonce,OUTBOUND_TO,chrono::Utc::now().timestamp_millis()],
        ).unwrap();
        let mut form = outbound_form("in-progress");
        form.insert("AnsweredBy".into(), "unknown".into());
        let cfg = common::load(&fixture.root).unwrap();
        assert_eq!(
            outbound::answer(&fixture.root, &nonce, &cfg, &form).unwrap(),
            protocol::EMPTY
        );
        let state: String = fixture
            .db()
            .query_row(
                "SELECT state FROM outbound_requests WHERE request_id=?1",
                [&request_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "screened_out");
        let calls: i64 = fixture
            .db()
            .query_row(
                "SELECT COUNT(*) FROM calls WHERE call_sid=?1",
                [SECOND_CALL],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(calls, 0);
    }

    #[test]
    fn outbound_request_nonce_cannot_be_rebound_to_another_destination() {
        let fixture = Fixture::new();
        let nonce = uuid::Uuid::new_v4().to_string();
        fixture.db().execute(
            "INSERT INTO outbound_requests(request_id,nonce,to_number,on_behalf_of,recipient,purpose,created_ms,state)
             VALUES(?1,?2,?3,'Owner','','Purpose',?4,'creating')",
            params![uuid::Uuid::new_v4().to_string(),nonce,OUTBOUND_TO,chrono::Utc::now().timestamp_millis()],
        ).unwrap();
        let mut form = outbound_form("in-progress");
        form.insert("To".into(), "+15550001999".into());
        form.insert("AnsweredBy".into(), "human".into());
        let path = format!("/voice/outbound/{nonce}");
        let signed = signed(&path, &form);
        let cfg = common::load(&fixture.root).unwrap();
        let authenticated = authenticate(&cfg, &signed.uri, &signed.headers, &signed.body).unwrap();
        assert!(outbound::answer(&fixture.root, &nonce, &cfg, &authenticated).is_err());
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn initial_admission_only_creates_consent_state() {
        let fixture = Fixture::new();
        let xml = fixture.initial(&valid_form()).unwrap();
        assert!(xml.contains("<Gather"));
        assert!(!xml.contains("<Recording") && !xml.contains("<Connect"));
        let state: (String, Option<bool>, Option<String>) = fixture
            .db()
            .query_row("SELECT phase,consent,media_token FROM calls", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(state, ("consent".into(), None, None));
        assert_eq!(fixture.recording_count(), 0);
    }

    #[test]
    fn recording_requires_exact_explicit_opt_in() {
        for digits in [
            Some("1"),
            Some("2"),
            Some(""),
            Some("11"),
            Some("yes"),
            None,
        ] {
            let fixture = Fixture::new();
            fixture.initial(&valid_form()).unwrap();
            let xml = fixture.choose(digits).unwrap();
            let consent: bool = fixture
                .db()
                .query_row("SELECT consent FROM calls", [], |row| row.get(0))
                .unwrap();
            assert_eq!(consent, digits == Some("1"));
            assert_eq!(xml.contains("<Recording"), consent);
            assert!(xml.contains("<Connect"));
            if consent {
                assert!(xml.find("<Recording").unwrap() < xml.find("<Connect").unwrap());
                assert!(xml.contains("track=\"both\"") && xml.contains("channels=\"dual\""));
            }
            assert_eq!(fixture.recording_count(), 0);
        }
    }

    #[test]
    fn admission_rejects_wrong_or_missing_call_identity() {
        let changes = [
            ("AccountSid", Some("AC11111111111111111111111111111111")),
            ("CallSid", Some("not-a-call")),
            ("To", Some("+15550001999")),
            ("ForwardedFrom", Some("+15550001999")),
            ("Direction", Some("outbound-api")),
            ("AccountSid", None),
            ("CallSid", None),
            ("To", None),
            ("ForwardedFrom", None),
            ("Direction", None),
        ];
        for (key, value) in changes {
            let fixture = Fixture::new();
            let mut form = valid_form();
            match value {
                Some(value) => {
                    form.insert(key.into(), value.into());
                }
                None => {
                    form.remove(key);
                }
            }
            assert!(fixture.initial(&form).is_err());
            assert_eq!(fixture.count(), 0);
        }
    }

    #[test]
    fn malformed_forms_and_tampered_signatures_never_admit() {
        let fixture = Fixture::new();
        for bad_suffix in [
            "&CallSid=other",
            "&Call%53id=other",
            "&x=%00",
            "&x=%FF",
            "&x=%",
            "&x=%ZZ",
        ] {
            let mut request = signed("/voice/webhook", &valid_form());
            request.body.extend_from_slice(bad_suffix.as_bytes());
            assert!(initial(&fixture.root, &request.uri, &request.headers, &request.body).is_err());
        }
        let mut request = signed("/voice/webhook", &valid_form());
        request.headers.remove("x-twilio-signature");
        assert!(initial(&fixture.root, &request.uri, &request.headers, &request.body).is_err());
        let mut request = signed("/voice/webhook", &valid_form());
        request
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        assert!(initial(&fixture.root, &request.uri, &request.headers, &request.body).is_err());
        let request = signed("/voice/webhook", &valid_form());
        assert!(
            initial(
                &fixture.root,
                &"/voice/webhook?tampered=1".parse().unwrap(),
                &request.headers,
                &request.body
            )
            .is_err()
        );
        assert_eq!(fixture.count(), 0);
    }

    #[test]
    fn status_callbacks_never_create_admitted_calls() {
        for (path, status) in [
            ("/voice/webhook?type=status", "ringing"),
            ("/voice/webhook", "completed"),
            ("/voice/webhook?type=status", "failed"),
        ] {
            let fixture = Fixture::new();
            let mut form = valid_form();
            form.insert("CallStatus".into(), status.into());
            form.remove("ForwardedFrom");
            let request = signed(path, &form);
            assert_eq!(
                initial(&fixture.root, &request.uri, &request.headers, &request.body).unwrap(),
                protocol::EMPTY
            );
            assert_eq!(fixture.count(), 0);
        }
    }

    #[test]
    fn replay_preserves_nonces_and_cannot_reverse_the_first_consent_decision() {
        for digits in ["1", "2"] {
            let fixture = Fixture::new();
            let first = fixture.initial(&valid_form()).unwrap();
            let nonce = fixture.nonce();
            assert_eq!(fixture.initial(&valid_form()).unwrap(), first);
            assert_eq!(fixture.nonce(), nonce);
            let connected = fixture.choose(Some(digits)).unwrap();
            let media: String = fixture
                .db()
                .query_row("SELECT media_token FROM calls", [], |row| row.get(0))
                .unwrap();
            assert_eq!(fixture.choose(Some(digits)).unwrap(), connected);
            assert_eq!(
                fixture
                    .choose(Some(if digits == "1" { "2" } else { "1" }))
                    .unwrap(),
                connected
            );
            assert_eq!(fixture.initial(&valid_form()).unwrap(), connected);
            let after: String = fixture
                .db()
                .query_row("SELECT media_token FROM calls", [], |row| row.get(0))
                .unwrap();
            assert_eq!(after, media);
            assert_eq!(fixture.count(), 1);
        }
    }

    #[test]
    fn consent_nonce_is_bound_to_call_and_expires() {
        let fixture = Fixture::new();
        fixture.initial(&valid_form()).unwrap();
        let nonce = fixture.nonce();
        let wrong_nonce = uuid::Uuid::new_v4().to_string();
        let mut form = valid_form();
        form.insert("Digits".into(), "1".into());
        let wrong = signed(&format!("/voice/consent/{wrong_nonce}"), &form);
        assert!(
            consent_result(
                &fixture.root,
                &wrong_nonce,
                &wrong.uri,
                &wrong.headers,
                &wrong.body
            )
            .is_err()
        );
        form.insert("CallSid".into(), SECOND_CALL.into());
        let wrong = signed(&format!("/voice/consent/{nonce}"), &form);
        assert!(
            consent_result(
                &fixture.root,
                &nonce,
                &wrong.uri,
                &wrong.headers,
                &wrong.body
            )
            .is_err()
        );
        fixture
            .db()
            .execute(
                "UPDATE calls SET created_ms=?",
                [chrono::Utc::now().timestamp_millis() - 180_001],
            )
            .unwrap();
        assert!(fixture.choose(Some("1")).is_err());
        let consent: Option<bool> = fixture
            .db()
            .query_row("SELECT consent FROM calls", [], |row| row.get(0))
            .unwrap();
        assert_eq!(consent, None);
    }

    #[test]
    fn terminal_or_active_call_replays_cannot_start_recording_again() {
        for phase in ["active", "ended", "expired"] {
            let fixture = Fixture::new();
            fixture.initial(&valid_form()).unwrap();
            fixture.choose(Some("1")).unwrap();
            fixture
                .db()
                .execute("UPDATE calls SET phase=?", [phase])
                .unwrap();
            assert_eq!(fixture.initial(&valid_form()).unwrap(), protocol::EMPTY);
            assert_eq!(fixture.choose(Some("1")).unwrap(), protocol::EMPTY);
        }
    }

    #[test]
    fn admission_serializes_calls_and_expires_stale_pending_state() {
        let fixture = Fixture::new();
        fixture.initial(&valid_form()).unwrap();
        let mut second = valid_form();
        second.insert("CallSid".into(), SECOND_CALL.into());
        assert_eq!(fixture.initial(&second).unwrap(), protocol::REJECT);
        assert_eq!(fixture.count(), 1);
        fixture
            .db()
            .execute(
                "UPDATE calls SET created_ms=?",
                [chrono::Utc::now().timestamp_millis() - 180_001],
            )
            .unwrap();
        assert!(fixture.initial(&second).unwrap().contains("<Gather"));
        assert_eq!(fixture.count(), 2);
        let old: String = fixture
            .db()
            .query_row("SELECT phase FROM calls WHERE call_sid=?", [CALL], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(old, "expired");
    }

    #[test]
    fn owner_policy_is_canonical_and_caller_fields_cannot_replace_it() {
        let fixture = Fixture::new();
        let mut form = valid_form();
        form.insert("chat_id".into(), "99999".into());
        form.insert("telegram_owner".into(), "99999".into());
        let xml = fixture.initial(&form).unwrap();
        assert!(!xml.contains("99999"));
        assert_eq!(
            common::load(&fixture.root).unwrap().telegram_chat_id,
            "12345"
        );
        for peers in [vec!["12345", "99999"], vec!["*"], vec![]] {
            let fixture = Fixture::new();
            fixture.native_edit(|value| {
                value["peer_groups"]["fixture"]["external_peers"] = toml::Value::Array(
                    peers
                        .into_iter()
                        .map(|value| toml::Value::String(value.into()))
                        .collect(),
                )
            });
            assert!(common::load(&fixture.root).is_err());
            assert!(fixture.initial(&valid_form()).is_err());
            assert_eq!(fixture.count(), 0);
        }
        let fixture = Fixture::new();
        fixture.native_edit(|value| {
            value["peer_groups"]["fixture"]["channel"] =
                toml::Value::String("telegram.other".into())
        });
        assert!(fixture.initial(&valid_form()).is_err());
        assert_eq!(fixture.count(), 0);
    }

    #[tokio::test]
    async fn recording_callback_requires_saved_consent_and_deduplicates() {
        for opt_in in [false, true] {
            let fixture = Fixture::new();
            fixture.initial(&valid_form()).unwrap();
            let app = Arc::new(App {
                root: fixture.root.clone(),
                slots: Arc::new(tokio::sync::Semaphore::new(1)),
            });
            let mut form = valid_form();
            form.insert("RecordingSid".into(), RECORDING.into());
            form.insert("RecordingStatus".into(), "completed".into());
            let request = signed("/voice/recording", &form);
            let response = recording_callback(
                State(app.clone()),
                request.uri,
                request.headers,
                Bytes::from(request.body),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(fixture.recording_count(), 0);
            fixture
                .choose(Some(if opt_in { "1" } else { "2" }))
                .unwrap();
            for _ in 0..2 {
                let request = signed("/voice/recording", &form);
                let response = recording_callback(
                    State(app.clone()),
                    request.uri,
                    request.headers,
                    Bytes::from(request.body),
                )
                .await;
                assert_eq!(
                    response.status(),
                    if opt_in {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    }
                );
            }
            assert_eq!(fixture.recording_count(), if opt_in { 1 } else { 0 });
        }
    }

    #[test]
    fn followup_rejects_conflicting_signed_identity_without_mutating_consent() {
        let fixture = Fixture::new();
        fixture.initial(&valid_form()).unwrap();
        let nonce = fixture.nonce();
        for (key, value) in [
            ("AccountSid", "AC11111111111111111111111111111111"),
            ("CallSid", SECOND_CALL),
            ("To", "+15550001999"),
            ("ForwardedFrom", "+15550001999"),
            ("Direction", "outbound-api"),
        ] {
            let mut form = valid_form();
            form.insert("Digits".into(), "1".into());
            form.insert(key.into(), value.into());
            let request = signed(&format!("/voice/consent/{nonce}"), &form);
            assert!(
                consent_result(
                    &fixture.root,
                    &nonce,
                    &request.uri,
                    &request.headers,
                    &request.body
                )
                .is_err()
            );
        }
        let state: (String, Option<bool>) = fixture
            .db()
            .query_row("SELECT phase,consent FROM calls", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(state, ("consent".into(), None));
    }

    #[test]
    fn disabled_or_unsafe_local_configuration_fails_closed() {
        let fixture = Fixture::new();
        let path = fixture.root.join("phone.toml");
        let mut config: common::PhoneConfig =
            toml::from_str(&common::private_read(&path).unwrap()).unwrap();
        config.enabled = false;
        common::atomic_private_write(&path, toml::to_string(&config).unwrap().as_bytes()).unwrap();
        assert!(matches!(
            fixture.initial(&valid_form()),
            Err("phone_disabled")
        ));
        assert_eq!(fixture.count(), 0);
        config.enabled = true;
        config.auth_token = AUTH.into();
        common::atomic_private_write(&path, toml::to_string(&config).unwrap().as_bytes()).unwrap();
        assert!(fixture.initial(&valid_form()).is_err());
        assert_eq!(fixture.count(), 0);
        let fixture = Fixture::new();
        fs::set_permissions(
            fixture.native.join("config.toml"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(fixture.initial(&valid_form()).is_err());
        assert_eq!(fixture.count(), 0);
    }
}
