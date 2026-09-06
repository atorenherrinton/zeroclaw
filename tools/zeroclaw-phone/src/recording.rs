//! Recording delivery has a durable outbox separate from the call lifecycle.
//! Only the authenticated, consent-checking ingress may enqueue identifiers.

use crate::common::{self, SafeResult, Settings};
use chrono::Utc;
use reqwest::{Client, Response, StatusCode, multipart, redirect::Policy};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_AUDIO_BYTES: usize = 49_000_000;
const MAX_JSON_BYTES: usize = 65_536;
const MAX_DOWNLOAD_ATTEMPTS: i64 = 6;
const MAX_PREFLIGHT_ATTEMPTS: i64 = 6;

/// Create this module's canonical delivery store without recovering live work.
pub fn initialize(conn: &Connection) -> SafeResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS recording_outbox (
            recording_sid TEXT PRIMARY KEY,
            account_sid TEXT NOT NULL,
            call_sid TEXT NOT NULL,
            provider_status TEXT NOT NULL CHECK(provider_status IN ('in-progress','completed','absent')),
            state TEXT NOT NULL CHECK(state IN ('waiting','queued','downloading','ready','sending','sent','uncertain','failed','absent')),
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL,
            next_attempt_ms INTEGER NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            preflight_attempts INTEGER NOT NULL DEFAULT 0,
            recipient_id TEXT,
            bot_username TEXT,
            local_name TEXT,
            message_id INTEGER,
            last_error TEXT
        );
        CREATE INDEX IF NOT EXISTS recording_outbox_due
            ON recording_outbox(state,next_attempt_ms,created_ms);",
    )
    .map_err(|_| "recording_schema_failed")
}

/// Caller must authenticate the callback and verify this call's recording consent.
/// Callback URLs and proposed Telegram recipients are deliberately not accepted.
pub fn enqueue(
    conn: &Connection,
    account_sid: &str,
    call_sid: &str,
    recording_sid: &str,
    status: &str,
) -> SafeResult<()> {
    if !valid_sid(account_sid, "AC")
        || !valid_sid(call_sid, "CA")
        || !valid_sid(recording_sid, "RE")
        || !matches!(status, "in-progress" | "completed" | "absent")
    {
        return Err("invalid_recording_callback");
    }
    let now = Utc::now().timestamp_millis();
    let state = match status {
        "completed" => "queued",
        "absent" => "absent",
        _ => "waiting",
    };
    let tx = conn
        .unchecked_transaction()
        .map_err(|_| "recording_transaction_failed")?;
    tx.execute(
        "INSERT OR IGNORE INTO recording_outbox
            (recording_sid,account_sid,call_sid,provider_status,state,created_ms,updated_ms,next_attempt_ms)
         VALUES (?1,?2,?3,?4,?5,?6,?6,?6)",
        params![recording_sid, account_sid, call_sid, status, state, now],
    )
    .map_err(|_| "recording_enqueue_failed")?;
    let existing: (String, String, String) = tx
        .query_row(
            "SELECT account_sid,call_sid,provider_status FROM recording_outbox WHERE recording_sid=?1",
            [recording_sid],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| "recording_lookup_failed")?;
    if existing.0 != account_sid || existing.1 != call_sid {
        return Err("recording_identifier_conflict");
    }
    if existing.2 != status && status != "in-progress" {
        if existing.2 != "in-progress" {
            return Err("recording_status_conflict");
        }
        tx.execute(
            "UPDATE recording_outbox SET provider_status=?2,state=?3,updated_ms=?4,next_attempt_ms=?4
             WHERE recording_sid=?1 AND state='waiting'",
            params![recording_sid, status, state, now],
        )
        .map_err(|_| "recording_enqueue_failed")?;
    }
    tx.commit().map_err(|_| "recording_commit_failed")
}

/// Keep the lock across polling and network awaits; startup recovery must never
/// reinterpret another live worker's in-flight send as a crashed send.
pub async fn run(root: PathBuf) -> SafeResult<()> {
    let _worker_lock = worker_lock(&root)?;
    prepare(&root)?;
    let client = http_client()?;
    loop {
        tick_locked(&root, &client).await?;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// A single explicit iteration, mutually exclusive with the persistent worker.
pub async fn tick(root: &Path) -> SafeResult<()> {
    let _worker_lock = worker_lock(root)?;
    prepare(root)?;
    tick_locked(root, &http_client()?).await
}

fn prepare(root: &Path) -> SafeResult<()> {
    let conn = common::open_db(root)?;
    initialize(&conn)?;
    recover(&conn)
}

fn recover(conn: &Connection) -> SafeResult<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         UPDATE recording_outbox SET state='uncertain',last_error='interrupted_send'
             WHERE state='sending';
         UPDATE recording_outbox SET state='queued' WHERE state='downloading';
         COMMIT;",
    )
    .map_err(|_| "recording_recovery_failed")
}

fn worker_lock(root: &Path) -> SafeResult<File> {
    common::private_dir(root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.join("recording-worker.lock"))
        .map_err(|_| "recording_lock_failed")?;
    verify_private_file(&file)?;
    // The File owns this descriptor and stays open throughout the worker lifetime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("recording_worker_busy");
    }
    Ok(file)
}

fn http_client() -> SafeResult<Client> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|_| "recording_http_client_failed")
}

struct Job {
    recording_sid: String,
    account_sid: String,
    call_sid: String,
    state: String,
    created_ms: i64,
    attempts: i64,
    preflight_attempts: i64,
    recipient_id: Option<String>,
    bot_username: Option<String>,
    local_name: Option<String>,
}

async fn tick_locked(root: &Path, client: &Client) -> SafeResult<()> {
    let settings = common::load_voicemail(root)?;
    if !settings.enabled {
        return Ok(());
    }
    let Some(job) = claim_job(root, &settings)? else {
        return Ok(());
    };
    if job.state == "queued" {
        download_job(root, client, &settings, &job).await
    } else {
        send_job(root, client, &settings, &job).await
    }
}

fn claim_job(root: &Path, settings: &Settings) -> SafeResult<Option<Job>> {
    let mut conn = common::open_db(root)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "recording_transaction_failed")?;
    let job = tx
        .query_row(
            "SELECT recording_sid,account_sid,call_sid,state,created_ms,attempts,
                    preflight_attempts,recipient_id,bot_username,local_name
             FROM recording_outbox WHERE state IN ('queued','ready') AND next_attempt_ms<=?1
             ORDER BY next_attempt_ms,created_ms LIMIT 1",
            [Utc::now().timestamp_millis()],
            |row| {
                Ok(Job {
                    recording_sid: row.get(0)?,
                    account_sid: row.get(1)?,
                    call_sid: row.get(2)?,
                    state: row.get(3)?,
                    created_ms: row.get(4)?,
                    attempts: row.get(5)?,
                    preflight_attempts: row.get(6)?,
                    recipient_id: row.get(7)?,
                    bot_username: row.get(8)?,
                    local_name: row.get(9)?,
                })
            },
        )
        .optional()
        .map_err(|_| "recording_lookup_failed")?;
    let Some(mut job) = job else {
        tx.commit().map_err(|_| "recording_commit_failed")?;
        return Ok(None);
    };
    if !valid_sid(&job.account_sid, "AC")
        || !valid_sid(&job.call_sid, "CA")
        || !valid_sid(&job.recording_sid, "RE")
        || settings.account_sid != job.account_sid
        || !(valid_owner(&settings.telegram_chat_id)
            || common::valid_private_channel_id(&settings.telegram_chat_id))
        || settings.telegram_bot_username.is_empty()
    {
        return Err("recording_configuration_mismatch");
    }
    if job.state == "queued" {
        if job.attempts >= MAX_DOWNLOAD_ATTEMPTS {
            tx.execute(
                "UPDATE recording_outbox SET state='failed',last_error='download_retries_exhausted' WHERE recording_sid=?1",
                [&job.recording_sid],
            )
            .map_err(|_| "recording_update_failed")?;
            tx.commit().map_err(|_| "recording_commit_failed")?;
            return Ok(None);
        }
        if job.recipient_id.is_none() {
            job.recipient_id = Some(settings.telegram_chat_id.clone());
            job.bot_username = Some(settings.telegram_bot_username.clone());
        }
        if !same_destination(&job, settings) {
            return Err("recording_destination_changed");
        }
        tx.execute(
            "UPDATE recording_outbox SET state='downloading',attempts=attempts+1,
                    recipient_id=?2,bot_username=?3,updated_ms=?4 WHERE recording_sid=?1 AND state='queued'",
            params![job.recording_sid, job.recipient_id, job.bot_username, Utc::now().timestamp_millis()],
        )
        .map_err(|_| "recording_claim_failed")?;
        job.attempts += 1;
    }
    tx.commit().map_err(|_| "recording_commit_failed")?;
    Ok(Some(job))
}

enum FetchFailure {
    Retry(&'static str),
    Stop(&'static str),
}

async fn download_job(
    root: &Path,
    client: &Client,
    settings: &Settings,
    job: &Job,
) -> SafeResult<()> {
    let result = fetch_audio(client, settings, &job.recording_sid).await;
    match result {
        Ok(bytes) => {
            // Preserve already-downloaded media even if delivery becomes blocked.
            // The next delivery iteration reloads policy, as does the final send.
            let name = format!("{}.mp3", job.recording_sid);
            if let Err(error) = archive(&settings.recording_dir, &name, &bytes) {
                return finish(root, job, "failed", error, None);
            }
            let conn = common::open_db(root)?;
            conn.execute(
                "UPDATE recording_outbox SET state='ready',local_name=?2,last_error=NULL,
                    updated_ms=?3,next_attempt_ms=?3 WHERE recording_sid=?1 AND state='downloading'",
                params![job.recording_sid, name, Utc::now().timestamp_millis()],
            )
            .map_err(|_| "recording_archive_receipt_failed")?;
            Ok(())
        }
        Err(FetchFailure::Retry(error)) if job.attempts < MAX_DOWNLOAD_ATTEMPTS => {
            defer(root, job, "queued", error, job.attempts, false)
        }
        Err(FetchFailure::Retry(error) | FetchFailure::Stop(error)) => {
            finish(root, job, "failed", error, None)
        }
    }
}

fn media_url(account_sid: &str, recording_sid: &str) -> SafeResult<String> {
    if !valid_sid(account_sid, "AC") || !valid_sid(recording_sid, "RE") {
        return Err("invalid_recording_identifier");
    }
    Ok(format!(
        "https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Recordings/{recording_sid}.mp3"
    ))
}

async fn fetch_audio(
    client: &Client,
    settings: &Settings,
    recording_sid: &str,
) -> Result<Vec<u8>, FetchFailure> {
    let url = media_url(&settings.account_sid, recording_sid).map_err(FetchFailure::Stop)?;
    let response = client
        .get(url)
        .basic_auth(&settings.account_sid, Some(&settings.auth_token))
        .send()
        .await
        .map_err(|_| FetchFailure::Retry("recording_download_transport_failed"))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(FetchFailure::Stop("recording_media_redirect_blocked"));
    }
    if status == StatusCode::NOT_FOUND
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        return Err(FetchFailure::Retry(
            "recording_media_temporarily_unavailable",
        ));
    }
    if !status.is_success() {
        return Err(FetchFailure::Stop("recording_media_rejected"));
    }
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next());
    if !mime.is_some_and(|value| value.eq_ignore_ascii_case("audio/mpeg")) {
        return Err(FetchFailure::Stop("recording_media_type_rejected"));
    }
    let bytes = read_capped(response, MAX_AUDIO_BYTES)
        .await
        .map_err(|error| match error {
            "recording_response_interrupted" => FetchFailure::Retry(error),
            _ => FetchFailure::Stop(error),
        })?;
    if !mp3_header(&bytes) {
        return Err(FetchFailure::Stop("recording_media_invalid_mp3"));
    }
    Ok(bytes)
}

async fn read_capped(mut response: Response, cap: usize) -> SafeResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err("recording_response_too_large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "recording_response_interrupted")?
    {
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > cap)
        {
            return Err("recording_response_too_large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn archive(directory: &Path, name: &str, bytes: &[u8]) -> SafeResult<()> {
    common::private_dir(directory)?;
    let destination = directory.join(name);
    let mut temp = tempfile::Builder::new()
        .prefix(".recording-")
        .tempfile_in(directory)
        .map_err(|_| "recording_archive_create_failed")?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "recording_archive_permissions_failed")?;
    temp.write_all(bytes)
        .map_err(|_| "recording_archive_write_failed")?;
    temp.as_file()
        .sync_all()
        .map_err(|_| "recording_archive_sync_failed")?;
    match temp.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A crash between file creation and the SQLite receipt can leave this
            // exact file. Prove byte equality; never overwrite or send another file.
            if read_archive(&destination)? != bytes {
                return Err("recording_archive_conflict");
            }
        }
        Err(_) => return Err("recording_archive_commit_failed"),
    }
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| "recording_archive_directory_sync_failed")
}

fn verify_private_file(file: &File) -> SafeResult<()> {
    let metadata = file
        .metadata()
        .map_err(|_| "recording_file_metadata_failed")?;
    // geteuid has no inputs and cannot fail; only this service user's files qualify.
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("recording_file_not_private");
    }
    Ok(())
}

fn read_archive(path: &Path) -> SafeResult<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "recording_archive_open_failed")?;
    verify_private_file(&file)?;
    let mut bytes = Vec::new();
    file.take((MAX_AUDIO_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "recording_archive_read_failed")?;
    if bytes.len() > MAX_AUDIO_BYTES || !mp3_header(&bytes) {
        return Err("recording_archive_invalid");
    }
    Ok(bytes)
}

fn same_destination(job: &Job, settings: &Settings) -> bool {
    job.recipient_id.as_deref() == Some(settings.telegram_chat_id.as_str())
        && job.bot_username.as_deref() == Some(settings.telegram_bot_username.as_str())
}

fn telegram_url(token: &str, method: &str) -> SafeResult<String> {
    let Some((id, secret)) = token.split_once(':') else {
        return Err("invalid_telegram_credentials");
    };
    if !valid_owner(id)
        || secret.is_empty()
        || !secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || !matches!(
            method,
            "getMe" | "getChat" | "getChatMember" | "getChatMemberCount" | "sendAudio"
        )
    {
        return Err("invalid_telegram_credentials");
    }
    Ok(format!("https://api.telegram.org/bot{token}/{method}"))
}

async fn telegram_json(response: Response) -> SafeResult<Value> {
    if !response.status().is_success() {
        return Err("telegram_preflight_http_failed");
    }
    let bytes = read_capped(response, MAX_JSON_BYTES).await?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "telegram_preflight_invalid_json")?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("telegram_preflight_rejected");
    }
    Ok(value)
}

async fn preflight(client: &Client, settings: &Settings) -> SafeResult<()> {
    let me = client
        .get(telegram_url(&settings.telegram_token, "getMe")?)
        .send()
        .await
        .map_err(|_| "telegram_preflight_transport_failed")?;
    let me = telegram_json(me).await?;
    if me["result"]["is_bot"].as_bool() != Some(true)
        || !me["result"]["username"]
            .as_str()
            .is_some_and(|username| username.eq_ignore_ascii_case(&settings.telegram_bot_username))
    {
        return Err("telegram_bot_mismatch");
    }
    let chat = client
        .post(telegram_url(&settings.telegram_token, "getChat")?)
        .form(&[("chat_id", settings.telegram_chat_id.as_str())])
        .send()
        .await
        .map_err(|_| "telegram_preflight_transport_failed")?;
    let chat = telegram_json(chat).await?;
    if !common::delivery_chat_matches(&chat["result"], &settings.telegram_chat_id) {
        return Err("telegram_private_owner_mismatch");
    }
    if common::valid_private_channel_id(&settings.telegram_chat_id) {
        let bot_id = me["result"]["id"]
            .as_i64()
            .ok_or("telegram_bot_id_missing")?
            .to_string();
        let mut members = Vec::new();
        for user_id in [&settings.telegram_owner_id, &bot_id] {
            let response = client
                .post(telegram_url(&settings.telegram_token, "getChatMember")?)
                .form(&[
                    ("chat_id", settings.telegram_chat_id.as_str()),
                    ("user_id", user_id.as_str()),
                ])
                .send()
                .await
                .map_err(|_| "telegram_preflight_transport_failed")?;
            members.push(telegram_json(response).await?);
        }
        let response = client
            .post(telegram_url(
                &settings.telegram_token,
                "getChatMemberCount",
            )?)
            .form(&[("chat_id", settings.telegram_chat_id.as_str())])
            .send()
            .await
            .map_err(|_| "telegram_preflight_transport_failed")?;
        let count = telegram_json(response).await?;
        if !owner_only_channel_members(
            &members[0]["result"],
            &members[1]["result"],
            &count["result"],
            &settings.telegram_owner_id,
            &bot_id,
        ) {
            return Err("telegram_channel_membership_mismatch");
        }
    }
    Ok(())
}

fn owner_only_channel_members(
    owner: &Value,
    bot: &Value,
    count: &Value,
    owner_id: &str,
    bot_id: &str,
) -> bool {
    owner["status"] == "creator"
        && owner["user"]["id"]
            .as_i64()
            .is_some_and(|id| id.to_string() == owner_id)
        && bot["status"] == "administrator"
        && bot["can_post_messages"] == true
        && bot["user"]["is_bot"] == true
        && bot["user"]["id"]
            .as_i64()
            .is_some_and(|id| id.to_string() == bot_id)
        && count.as_u64() == Some(2)
}

/// Validate the exact owner DM or explicitly configured owner-only private channel.
pub async fn validate_delivery_destination(client: &Client, settings: &Settings) -> SafeResult<()> {
    preflight(client, settings).await
}

async fn send_job(root: &Path, client: &Client, settings: &Settings, job: &Job) -> SafeResult<()> {
    if !same_destination(job, settings) {
        return finish(root, job, "failed", "recording_destination_changed", None);
    }
    let expected_name = format!("{}.mp3", job.recording_sid);
    if job.local_name.as_deref() != Some(expected_name.as_str()) {
        return finish(root, job, "failed", "recording_archive_name_mismatch", None);
    }
    let bytes = match read_archive(&settings.recording_dir.join(&expected_name)) {
        Ok(bytes) => bytes,
        Err(error) => return finish(root, job, "failed", error, None),
    };
    if let Err(error) = preflight(client, settings).await {
        if job.preflight_attempts + 1 >= MAX_PREFLIGHT_ATTEMPTS {
            return finish(root, job, "failed", error, None);
        }
        return defer(root, job, "ready", error, job.preflight_attempts + 1, true);
    }
    let current = common::load_voicemail(root)?;
    if !current.enabled
        || current.account_sid != job.account_sid
        || !same_destination(job, &current)
        || current.telegram_token != settings.telegram_token
        || current.telegram_owner_id != settings.telegram_owner_id
        || current.recording_dir != settings.recording_dir
    {
        return finish(root, job, "failed", "recording_configuration_changed", None);
    }
    let timestamp = chrono::DateTime::<Utc>::from_timestamp_millis(job.created_ms)
        .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .ok_or("recording_timestamp_invalid")?;
    let audio = multipart::Part::bytes(bytes)
        .file_name("call-recording.mp3")
        .mime_str("audio/mpeg")
        .map_err(|_| "recording_multipart_failed")?;
    let form = multipart::Form::new()
        .text("chat_id", current.telegram_chat_id.clone())
        .text("title", format!("Call recording — {timestamp}"))
        .text(
            "caption",
            "Completed call recording. A copy is saved privately on your Mac.",
        )
        .part("audio", audio);
    let request = client
        .post(telegram_url(&current.telegram_token, "sendAudio")?)
        .multipart(form)
        .build()
        .map_err(|_| "recording_send_request_failed")?;
    let conn = common::open_db(root)?;
    let claimed = conn
        .execute(
            "UPDATE recording_outbox SET state='sending',updated_ms=?2,last_error=NULL
         WHERE recording_sid=?1 AND state='ready'",
            params![job.recording_sid, Utc::now().timestamp_millis()],
        )
        .map_err(|_| "recording_send_claim_failed")?;
    drop(conn);
    if claimed != 1 {
        return Err("recording_send_claim_lost");
    }
    // From this point onward even a timeout can mean Telegram accepted the file.
    // Persist uncertainty; neither this worker nor recovery blindly resends it.
    let response = match client.execute(request).await {
        Ok(response) => response,
        Err(_) => return finish(root, job, "uncertain", "telegram_delivery_uncertain", None),
    };
    let http_ok = response.status().is_success();
    let response = match read_capped(response, MAX_JSON_BYTES).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
        Err(_) => None,
    };
    match classify_send(http_ok, response.as_ref(), &current.telegram_chat_id) {
        SendResult::Sent(message_id) => finish(root, job, "sent", "", Some(message_id)),
        SendResult::Rejected => finish(root, job, "failed", "telegram_delivery_rejected", None),
        SendResult::Uncertain => {
            finish(root, job, "uncertain", "telegram_delivery_uncertain", None)
        }
    }
}

enum SendResult {
    Sent(i64),
    Rejected,
    Uncertain,
}

fn classify_send(http_ok: bool, response: Option<&Value>, owner: &str) -> SendResult {
    let Some(response) = response else {
        return SendResult::Uncertain;
    };
    if response.get("ok").and_then(Value::as_bool) == Some(false) {
        return SendResult::Rejected;
    }
    if http_ok
        && response.get("ok").and_then(Value::as_bool) == Some(true)
        && common::delivery_chat_matches(&response["result"]["chat"], owner)
        && let Some(id) = response["result"]["message_id"]
            .as_i64()
            .filter(|id| *id > 0)
    {
        return SendResult::Sent(id);
    }
    SendResult::Uncertain
}

fn defer(
    root: &Path,
    job: &Job,
    state: &str,
    error: &'static str,
    attempt: i64,
    preflight: bool,
) -> SafeResult<()> {
    let delay_ms = 10_000_i64.saturating_mul(1_i64 << attempt.clamp(0, 6));
    let now = Utc::now().timestamp_millis();
    let conn = common::open_db(root)?;
    conn.execute(
        "UPDATE recording_outbox SET state=?2,last_error=?3,next_attempt_ms=?4,
             updated_ms=?5,preflight_attempts=preflight_attempts+?6 WHERE recording_sid=?1",
        params![
            job.recording_sid,
            state,
            error,
            now.saturating_add(delay_ms),
            now,
            i64::from(preflight)
        ],
    )
    .map_err(|_| "recording_retry_update_failed")?;
    Ok(())
}

fn finish(
    root: &Path,
    job: &Job,
    state: &str,
    error: &'static str,
    message_id: Option<i64>,
) -> SafeResult<()> {
    let conn = common::open_db(root)?;
    conn.execute(
        "UPDATE recording_outbox SET state=?2,last_error=?3,message_id=?4,updated_ms=?5 WHERE recording_sid=?1",
        params![job.recording_sid, state, if error.is_empty() { None } else { Some(error) }, message_id, Utc::now().timestamp_millis()],
    ).map_err(|_| "recording_final_receipt_failed")?;
    Ok(())
}

fn valid_sid(value: &str, prefix: &str) -> bool {
    value.len() == 34
        && value.starts_with(prefix)
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn valid_owner(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|id| id > 0 && id.to_string() == value)
}

fn mp3_header(bytes: &[u8]) -> bool {
    let offset = if bytes.starts_with(b"ID3") {
        if bytes.len() < 10
            || !matches!(bytes[3], 2..=4)
            || bytes[6..10].iter().any(|byte| byte & 0x80 != 0)
        {
            return false;
        }
        let size = bytes[6..10]
            .iter()
            .fold(0_usize, |size, byte| (size << 7) | usize::from(*byte));
        10 + size
            + if bytes[3] == 4 && bytes[5] & 0x10 != 0 {
                10
            } else {
                0
            }
    } else {
        0
    };
    let Some(audio) = bytes.get(offset..) else {
        return false;
    };
    audio.iter().take(4096).enumerate().any(|(index, _)| {
        let Some(frame) = audio.get(index..index + 4) else {
            return false;
        };
        frame[0] == 0xff
            && frame[1] & 0xe0 == 0xe0
            && (frame[1] >> 3) & 3 != 1
            && (frame[1] >> 1) & 3 == 1
            && !matches!(frame[2] >> 4, 0 | 15)
            && (frame[2] >> 2) & 3 != 3
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ACCOUNT: &str = "AC00000000000000000000000000000000";
    const CALL: &str = "CA00000000000000000000000000000000";
    const RECORDING: &str = "RE00000000000000000000000000000000";

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    fn callback_transitions_deduplicate_and_reject_conflicts() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        enqueue(&conn, ACCOUNT, CALL, RECORDING, "in-progress").unwrap();
        enqueue(&conn, ACCOUNT, CALL, RECORDING, "completed").unwrap();
        conn.execute("UPDATE recording_outbox SET state='sent',message_id=7", [])
            .unwrap();
        enqueue(&conn, ACCOUNT, CALL, RECORDING, "completed").unwrap();
        enqueue(&conn, ACCOUNT, CALL, RECORDING, "in-progress").unwrap();
        let state: String = conn
            .query_row("SELECT state FROM recording_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "sent");
        assert!(
            enqueue(
                &conn,
                ACCOUNT,
                "CA11111111111111111111111111111111",
                RECORDING,
                "completed"
            )
            .is_err()
        );
        assert!(enqueue(&conn, ACCOUNT, CALL, RECORDING, "absent").is_err());
        assert!(enqueue(&conn, ACCOUNT, CALL, "../../private.mp3", "completed").is_err());
    }

    #[test]
    fn constructed_media_url_cannot_escape_twilio() {
        assert_eq!(
            media_url(ACCOUNT, RECORDING).unwrap(),
            format!(
                "https://api.twilio.com/2010-04-01/Accounts/{ACCOUNT}/Recordings/{RECORDING}.mp3"
            )
        );
        assert!(media_url("https://example.com", RECORDING).is_err());
        assert!(media_url(ACCOUNT, "RE/../../secrets").is_err());
    }

    #[test]
    fn audio_header_rejects_html_and_incomplete_id3() {
        assert!(mp3_header(&[0xff, 0xfb, 0x90, 0x64, 0, 0]));
        assert!(mp3_header(&[
            b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0, 0xff, 0xfb, 0x90, 0x64
        ]));
        assert!(!mp3_header(b"<html>access denied</html>"));
        assert!(!mp3_header(b"ID3"));
        assert!(!mp3_header(&[0xff, 0xff, 0xff, 0xff]));
    }

    #[test]
    fn telegram_receipts_require_the_exact_private_owner() {
        let success =
            json!({"ok":true,"result":{"message_id":7,"chat":{"id":12345,"type":"private"}}});
        assert!(matches!(
            classify_send(true, Some(&success), "12345"),
            SendResult::Sent(7)
        ));
        assert!(matches!(
            classify_send(true, Some(&success), "99999"),
            SendResult::Uncertain
        ));
        assert!(matches!(
            classify_send(false, Some(&success), "12345"),
            SendResult::Uncertain
        ));
        assert!(matches!(
            classify_send(true, None, "12345"),
            SendResult::Uncertain
        ));
        assert!(matches!(
            classify_send(false, Some(&json!({"ok":false})), "12345"),
            SendResult::Rejected
        ));
        assert!(!common::delivery_chat_matches(
            &json!({"id":12345,"type":"group"}),
            "12345"
        ));
        assert!(!valid_owner("-12345"));
        assert!(!valid_owner("0012345"));
    }

    #[test]
    fn channel_delivery_requires_exact_private_channel_and_owner_only_membership() {
        for method in ["getMe", "getChat", "getChatMember", "getChatMemberCount"] {
            assert!(telegram_url("12345:fixture_secret", method).is_ok());
        }
        assert!(telegram_url("12345:fixture_secret", "promoteChatMember").is_err());
        let id = "-1001234567890";
        let response = json!({"ok":true,"result":{"message_id":9,"chat":{"id":-1001234567890_i64,"type":"channel"}}});
        assert!(matches!(
            classify_send(true, Some(&response), id),
            SendResult::Sent(9)
        ));
        let mut public = response.clone();
        public["result"]["chat"]["username"] = json!("public_channel");
        assert!(matches!(
            classify_send(true, Some(&public), id),
            SendResult::Uncertain
        ));
        assert!(matches!(
            classify_send(true, Some(&response), "-1009999999999"),
            SendResult::Uncertain
        ));
        let owner = json!({"status":"creator","user":{"id":12345}});
        let mut bot = json!({"status":"administrator","can_post_messages":true,"user":{"id":23456,"is_bot":true}});
        assert!(owner_only_channel_members(
            &owner,
            &bot,
            &json!(2),
            "12345",
            "23456"
        ));
        assert!(!owner_only_channel_members(
            &owner,
            &bot,
            &json!(3),
            "12345",
            "23456"
        ));
        assert!(!owner_only_channel_members(
            &owner,
            &bot,
            &json!(2),
            "54321",
            "23456"
        ));
        bot["can_post_messages"] = json!(false);
        assert!(!owner_only_channel_members(
            &owner,
            &bot,
            &json!(2),
            "12345",
            "23456"
        ));
    }

    #[test]
    fn archive_never_clobbers_different_audio() {
        let directory = private_tempdir();
        let bytes = [0xff, 0xfb, 0x90, 0x64, 0, 0];
        archive(directory.path(), "recording.mp3", &bytes).unwrap();
        archive(directory.path(), "recording.mp3", &bytes).unwrap();
        let other = [0xff, 0xfb, 0x90, 0x64, 1, 1];
        assert!(archive(directory.path(), "recording.mp3", &other).is_err());
        assert_eq!(
            read_archive(&directory.path().join("recording.mp3")).unwrap(),
            bytes
        );
    }

    #[test]
    fn crash_recovery_never_requeues_a_possibly_sent_recording() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        enqueue(&conn, ACCOUNT, CALL, RECORDING, "completed").unwrap();
        conn.execute(
            "UPDATE recording_outbox SET state='sending',local_name='preserved.mp3'",
            [],
        )
        .unwrap();
        recover(&conn).unwrap();
        let row: (String, String) = conn
            .query_row("SELECT state,local_name FROM recording_outbox", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(row, ("uncertain".to_owned(), "preserved.mp3".to_owned()));
        enqueue(&conn, ACCOUNT, CALL, RECORDING, "completed").unwrap();
        recover(&conn).unwrap();
        let state: String = conn
            .query_row("SELECT state FROM recording_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "uncertain");
        conn.execute(
            "UPDATE recording_outbox SET state='downloading',attempts=3",
            [],
        )
        .unwrap();
        recover(&conn).unwrap();
        let row: (String, i64) = conn
            .query_row("SELECT state,attempts FROM recording_outbox", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(row, ("queued".to_owned(), 3));
    }

    #[test]
    fn worker_lock_excludes_a_second_worker() {
        let directory = private_tempdir();
        let first = worker_lock(directory.path()).unwrap();
        assert!(matches!(
            worker_lock(directory.path()),
            Err("recording_worker_busy")
        ));
        drop(first);
        assert!(worker_lock(directory.path()).is_ok());
    }

    #[test]
    fn archive_rejects_symlink_destinations() {
        let directory = private_tempdir();
        let bytes = [0xff, 0xfb, 0x90, 0x64, 0, 0];
        archive(directory.path(), "original.mp3", &bytes).unwrap();
        std::os::unix::fs::symlink("original.mp3", directory.path().join("link.mp3")).unwrap();
        assert!(archive(directory.path(), "link.mp3", &bytes).is_err());
        assert_eq!(
            read_archive(&directory.path().join("original.mp3")).unwrap(),
            bytes
        );
    }
}
