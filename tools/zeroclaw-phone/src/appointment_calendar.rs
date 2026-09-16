//! Native calendar access for one owner-reviewable tentative hold. Canonical
//! config is resolved before every subprocess; the writer owns durable intent.
//! Calendar payloads and receipts stay private and never implement Debug.

use crate::{
    appointment_commands,
    common::{SafeResult, check, private_read},
};
use chrono::{DateTime, Duration as TimeDelta, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    future::Future,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

const GOG: &str = "/opt/homebrew/bin/gog";
const OUTPUT_LIMIT: usize = 1024 * 1024;
const MAX_PAGES: usize = 16;
const MAX_ITEMS: usize = 4000;
const READ_TIME: Duration = Duration::from_secs(20);
const READ_OPERATION_TIME: Duration = Duration::from_secs(60);
const WRITE_OPERATION_TIME: Duration = Duration::from_secs(120);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub calendar_id: String,
    pub id: String,
    pub etag: String,
    pub start: String,
    pub end: String,
    pub summary: String,
    pub location: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldState {
    Verified,
    Uncertain,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldReceipt {
    pub state: HoldState,
    pub idempotency_key: String,
    pub event_id: String,
    pub calendar_id: String,
    pub start: String,
    pub end: String,
    pub structured: Value,
}

// This ephemeral binding detects a mid-operation account change. It is never
// saved as policy: every command resolves and validates config.toml again.
struct Routes {
    home: PathBuf,
    account: String,
    wrapper: PathBuf,
}

fn text<'a>(value: &'a Value, key: &str, limit: usize) -> SafeResult<&'a str> {
    let s = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or("calendar_field_missing")?;
    check(
        !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control),
        "calendar_field_invalid",
    )?;
    Ok(s)
}

fn boolean(value: &Value, key: &str) -> SafeResult<bool> {
    match value.get(key) {
        None => Ok(false),
        Some(Value::Bool(v)) => Ok(*v),
        _ => Err("calendar_boolean_invalid"),
    }
}

fn calendar_id(id: &str) -> SafeResult<()> {
    // CalendarList IDs are email-shaped resource IDs. Do not let gog resolve a
    // display name, numeric index, comma list, or the mutable primary alias.
    check(
        id.len() <= 1024
            && id.contains('@')
            && !id
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == ','),
        "calendar_id_invalid",
    )
}

fn routes_from(config_dir: &Path, source: &str) -> SafeResult<Routes> {
    check(
        config_dir.is_absolute()
            && config_dir.file_name().is_some_and(|v| v == ".zeroclaw")
            && config_dir
                .components()
                .all(|v| matches!(v, Component::RootDir | Component::Normal(_))),
        "calendar_config_path_invalid",
    )?;
    let home = config_dir
        .parent()
        .ok_or("calendar_home_missing")?
        .to_owned();
    let config: toml::Value = toml::from_str(source).map_err(|_| "calendar_config_invalid")?;
    let mcp = config.get("mcp").ok_or("calendar_routes_missing")?;
    check(
        mcp.get("enabled").is_none_or(|v| v.as_bool() == Some(true)),
        "calendar_routes_disabled",
    )?;
    let servers = mcp
        .get("servers")
        .and_then(toml::Value::as_array)
        .ok_or("calendar_routes_missing")?;
    let find = |name| -> SafeResult<&toml::Value> {
        let mut found = servers
            .iter()
            .filter(|v| v.get("name").and_then(toml::Value::as_str) == Some(name));
        let server = found.next().ok_or("calendar_route_missing")?;
        check(found.next().is_none(), "calendar_route_duplicate")?;
        check(
            server
                .get("transport")
                .is_none_or(|v| v.as_str() == Some("stdio")),
            "calendar_transport_invalid",
        )?;
        Ok(server)
    };
    let read = find("google_read")?;
    let write = find("google_write")?;
    check(
        read.get("command").and_then(toml::Value::as_str) == Some(GOG),
        "calendar_read_route_invalid",
    )?;
    let args = read
        .get("args")
        .and_then(toml::Value::as_array)
        .ok_or("calendar_read_args_invalid")?;
    let args: Vec<&str> = args
        .iter()
        .map(|v| v.as_str().ok_or("calendar_read_args_invalid"))
        .collect::<SafeResult<_>>()?;
    check(args.first() == Some(&"mcp"), "calendar_read_args_invalid")?;
    let mut accounts = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if *arg == "--account" {
            accounts.push(*args.get(index + 1).ok_or("calendar_account_missing")?);
        }
        if let Some(account) = arg.strip_prefix("--account=") {
            accounts.push(account);
        }
    }
    check(accounts.len() == 1, "calendar_account_ambiguous")?;
    let account = accounts[0];
    check(
        !account.is_empty()
            && !account.eq_ignore_ascii_case("auto")
            && account.len() <= 320
            && !account.chars().any(|c| c.is_control() || c.is_whitespace())
            && !account.starts_with('-'),
        "calendar_account_invalid",
    )?;
    let wrapper = config_dir.join("bin/zeroclaw-signed-launch");
    check(
        write.get("command").and_then(toml::Value::as_str) == wrapper.to_str(),
        "calendar_write_route_invalid",
    )?;
    let write_args = write
        .get("args")
        .and_then(toml::Value::as_array)
        .ok_or("calendar_write_args_invalid")?;
    check(
        write_args.len() == 1 && write_args[0].as_str() == Some("google-write"),
        "calendar_write_args_invalid",
    )?;
    check(
        write
            .get("env")
            .and_then(|v| v.get("GOG_ACCOUNT"))
            .and_then(toml::Value::as_str)
            == Some(account),
        "calendar_account_mismatch",
    )?;
    Ok(Routes {
        home,
        account: account.to_owned(),
        wrapper,
    })
}

fn routes(config_dir: &Path) -> SafeResult<Routes> {
    routes_from(config_dir, &private_read(&config_dir.join("config.toml"))?)
}

trait Runner: Sync {
    fn run(
        &self,
        command: Command,
        input: Option<Vec<u8>>,
        budget: Duration,
    ) -> impl Future<Output = SafeResult<Value>> + Send;
}

struct Native;
impl Runner for Native {
    async fn run(
        &self,
        command: Command,
        input: Option<Vec<u8>>,
        budget: Duration,
    ) -> SafeResult<Value> {
        let bytes = appointment_commands::run(command, input, OUTPUT_LIMIT, budget).await?;
        serde_json::from_slice(&bytes).map_err(|_| "calendar_response_invalid")
    }
}

struct Session<'a, R> {
    config_dir: &'a Path,
    account: String,
    runner: &'a R,
    deadline: tokio::time::Instant,
}
impl<'a, R: Runner> Session<'a, R> {
    fn new(config_dir: &'a Path, runner: &'a R) -> SafeResult<Self> {
        Ok(Self {
            config_dir,
            account: routes(config_dir)?.account,
            runner,
            deadline: tokio::time::Instant::now() + WRITE_OPERATION_TIME,
        })
    }
    fn command(&self, write: bool) -> SafeResult<Command> {
        let current = routes(self.config_dir)?;
        check(current.account == self.account, "calendar_account_changed")?;
        let mut command = Command::new(if write {
            current.wrapper.as_os_str()
        } else {
            std::ffi::OsStr::new(GOG)
        });
        command
            .env_clear()
            .env("HOME", current.home)
            .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin");
        if write {
            command
                .arg("google-write")
                .env("GOG_ACCOUNT", &current.account)
                .env("ZEROCLAW_CONFIG_DIR", self.config_dir);
        } else {
            command.args([
                format!("--account={}", current.account),
                "--json".into(),
                "--no-input".into(),
                "--readonly".into(),
                "--gmail-no-send".into(),
            ]);
        }
        Ok(command)
    }
    fn budget(&self, cap: Duration) -> SafeResult<Duration> {
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(cap);
        check(!remaining.is_zero(), "calendar_operation_timeout")?;
        Ok(remaining)
    }
    async fn read(&self, allow: &str, args: Vec<String>) -> SafeResult<Value> {
        let mut command = self.command(false)?;
        command
            .arg(format!("--enable-commands-exact={allow}"))
            .args(args);
        self.runner
            .run(command, None, self.budget(READ_TIME)?)
            .await
    }
    async fn exact(&self, calendar: &str, id: &str) -> SafeResult<Value> {
        calendar_id(calendar)?;
        check(
            !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control),
            "calendar_event_id_invalid",
        )?;
        self.read(
            "api.call,api.calendar.events.get",
            vec![
                "api".into(),
                "call".into(),
                "calendar".into(),
                "v3".into(),
                "calendar.events.get".into(),
                format!("--params={}", json!({"calendarId":calendar,"eventId":id})),
                "--scope=https://www.googleapis.com/auth/calendar.events".into(),
            ],
        )
        .await
    }
    async fn calendars(&self) -> SafeResult<(String, Vec<String>)> {
        let mut paging = Paging::default();
        let mut entries = Vec::new();
        loop {
            let mut args = vec!["calendar".into(), "calendars".into(), "--max=250".into()];
            if let Some(token) = &paging.next {
                args.push(format!("--page={token}"));
            }
            let page = self.read("calendar.calendars", args).await?;
            let (items, more) = paging.accept(&page, "calendars")?;
            entries.extend(items.iter().cloned());
            if !more {
                break;
            }
        }
        selected_calendars(&entries)
    }
    async fn available(&self, start: &str, end: &str) -> SafeResult<String> {
        let (from, to) = interval(start, end)?;
        let (primary, calendars) = self.calendars().await?;
        for chunk in calendars.chunks(50) {
            let mut args = vec![
                "calendar".into(),
                "freebusy".into(),
                format!("--from={start}"),
                format!("--to={end}"),
            ];
            args.extend(chunk.iter().map(|id| format!("--cal={id}")));
            let value = self.read("calendar.freebusy", args).await?;
            busy_clear(&value, chunk, from, to)?;
        }
        Ok(primary)
    }
    async fn writer(
        &self,
        name: &str,
        arguments: Value,
        before_write: Option<&(dyn Fn() -> SafeResult<()> + Send + Sync)>,
    ) -> SafeResult<Value> {
        let command = self.command(true)?;
        let mut input = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})).map_err(|_| "calendar_request_invalid")?;
        input.push(b'\n');
        let budget = self.budget(Duration::from_secs(90))?;
        if let Some(gate) = before_write {
            gate()?;
        }
        let response = self.runner.run(command, Some(input), budget).await?;
        check(
            response["jsonrpc"] == "2.0"
                && response["id"] == 1
                && response.get("error").is_none()
                && response["result"]["isError"] != true,
            "calendar_writer_failed",
        )?;
        response["result"]
            .get("structuredContent")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or("calendar_receipt_missing")
    }
}

#[derive(Default)]
struct Paging {
    next: Option<String>,
    seen: BTreeSet<String>,
    pages: usize,
    items: usize,
}
impl Paging {
    fn accept<'a>(&mut self, page: &'a Value, field: &str) -> SafeResult<(&'a [Value], bool)> {
        let items = page
            .get(field)
            .and_then(Value::as_array)
            .ok_or("calendar_page_invalid")?;
        self.pages += 1;
        self.items += items.len();
        check(
            items.len() <= 250 && self.pages <= MAX_PAGES && self.items <= MAX_ITEMS,
            "calendar_page_limit",
        )?;
        let next = match page.get("nextPageToken") {
            None => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) if s.len() <= 4096 && !s.chars().any(char::is_control) => {
                Some(s.clone())
            }
            _ => return Err("calendar_page_token_invalid"),
        };
        if let Some(token) = &next {
            check(
                self.pages < MAX_PAGES && self.items < MAX_ITEMS && self.seen.insert(token.clone()),
                "calendar_pagination_incomplete",
            )?;
        }
        self.next = next;
        Ok((items, self.next.is_some()))
    }
}

fn selected_calendars(entries: &[Value]) -> SafeResult<(String, Vec<String>)> {
    let mut ids = BTreeSet::new();
    let mut primary = None;
    let mut selected = Vec::new();
    for entry in entries {
        let id = text(entry, "id", 1024)?;
        calendar_id(id)?;
        check(ids.insert(id), "calendar_list_duplicate")?;
        let is_primary = boolean(entry, "primary")?;
        let is_selected = boolean(entry, "selected")?;
        let hidden = boolean(entry, "hidden")?;
        let deleted = boolean(entry, "deleted")?;
        if is_primary {
            check(
                primary.is_none() && !hidden && !deleted && entry["accessRole"] == "owner",
                "calendar_primary_invalid",
            )?;
            primary = Some(id.to_owned());
        }
        if (is_primary || is_selected) && !hidden && !deleted {
            check(
                matches!(
                    entry["accessRole"].as_str(),
                    Some("owner" | "writer" | "reader" | "freeBusyReader")
                ),
                "calendar_access_invalid",
            )?;
            selected.push(id.to_owned());
        }
    }
    Ok((primary.ok_or("calendar_primary_missing")?, selected))
}

fn instant(value: &str) -> SafeResult<DateTime<Utc>> {
    check(value.len() <= 64, "calendar_time_invalid")?;
    DateTime::parse_from_rfc3339(value)
        .map(|v| v.with_timezone(&Utc))
        .map_err(|_| "calendar_time_invalid")
}

fn interval(start: &str, end: &str) -> SafeResult<(DateTime<Utc>, DateTime<Utc>)> {
    let (start, end) = (instant(start)?, instant(end)?);
    check(
        end > start && end - start <= TimeDelta::hours(24),
        "calendar_interval_invalid",
    )?;
    Ok((start, end))
}

fn busy_clear(
    value: &Value,
    ids: &[String],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> SafeResult<()> {
    let calendars = value["calendars"]
        .as_object()
        .ok_or("calendar_busy_invalid")?;
    check(calendars.len() == ids.len(), "calendar_busy_incomplete")?;
    for id in ids {
        let entry = calendars.get(id).ok_or("calendar_busy_incomplete")?;
        check(
            entry
                .get("errors")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
            "calendar_busy_error",
        )?;
        // Google omits an empty busy array in some responses. Its successful
        // per-calendar object with no errors is an authoritative empty result.
        let empty = Vec::new();
        let busy = match entry.get("busy") {
            None => &empty,
            Some(v) => v.as_array().ok_or("calendar_busy_invalid")?,
        };
        check(
            entry.is_object() && busy.len() <= MAX_ITEMS,
            "calendar_busy_invalid",
        )?;
        for item in busy {
            let from = instant(text(item, "start", 64)?)?;
            let to = instant(text(item, "end", 64)?)?;
            check(to > from, "calendar_busy_invalid")?;
            check(!(from < end && to > start), "calendar_conflict")?;
        }
    }
    Ok(())
}

fn event(calendar: &str, value: &Value) -> SafeResult<Event> {
    calendar_id(calendar)?;
    check(
        matches!(value["status"].as_str(), Some("confirmed" | "tentative")),
        "calendar_event_inactive",
    )?;
    check(
        value.get("eventType").is_none_or(|v| v == "default")
            && value.get("recurringEventId").is_none()
            && value.get("originalStartTime").is_none()
            && value.get("recurrence").is_none(),
        "calendar_event_kind_unsupported",
    )?;
    check(
        value["start"].get("date").is_none() && value["end"].get("date").is_none(),
        "calendar_event_all_day",
    )?;
    let start = text(&value["start"], "dateTime", 64)?.to_owned();
    let end = text(&value["end"], "dateTime", 64)?.to_owned();
    interval(&start, &end)?;
    let etag = text(value, "etag", 512)?;
    check(
        etag.starts_with('"') && etag.ends_with('"'),
        "calendar_etag_invalid",
    )?;
    Ok(Event {
        calendar_id: calendar.to_owned(),
        id: text(value, "id", 1024)?.to_owned(),
        etag: etag.to_owned(),
        start,
        end,
        summary: text(value, "summary", 1024)?.to_owned(),
        location: text(value, "location", 1024)?.to_owned(),
    })
}

fn normalized(value: &str) -> String {
    value
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
fn phrase(haystack: &str, needle: &str) -> bool {
    format!(" {} ", normalized(haystack)).contains(&format!(" {needle} "))
}

fn same_event(left: &Event, right: &Event) -> bool {
    left.calendar_id == right.calendar_id
        && left.id == right.id
        && left.etag == right.etag
        && instant(&left.start).ok() == instant(&right.start).ok()
        && instant(&left.end).ok() == instant(&right.end).ok()
        && left.summary == right.summary
        && left.location == right.location
}

async fn find_using<R: Runner>(
    config_dir: &Path,
    original_start: &str,
    business_name: &str,
    business_address: &str,
    runner: &R,
) -> SafeResult<Event> {
    let start = instant(original_start)?;
    let name = normalized(business_name);
    let address = normalized(business_address);
    check(
        name.len() >= 3 && address.len() >= 3 && name.len() <= 300 && address.len() <= 300,
        "calendar_business_invalid",
    )?;
    let session = Session::new(config_dir, runner)?;
    let (primary, _) = session.calendars().await?;
    let from = start
        .checked_sub_signed(TimeDelta::seconds(1))
        .ok_or("calendar_time_invalid")?
        .to_rfc3339();
    let to = start
        .checked_add_signed(TimeDelta::seconds(1))
        .ok_or("calendar_time_invalid")?
        .to_rfc3339();
    let mut paging = Paging::default();
    let mut candidate = None;
    loop {
        let mut args = vec!["calendar".into(), "events".into(), primary.clone(), "--max=250".into(), format!("--from={from}"), format!("--to={to}"), "--fields=nextPageToken,items(id,etag,status,eventType,summary,location,start,end,recurrence,recurringEventId,originalStartTime)".into()];
        if let Some(token) = &paging.next {
            args.push(format!("--page={token}"));
        }
        let page = session.read("calendar.events", args).await?;
        let (items, more) = paging.accept(&page, "events")?;
        for item in items {
            if item["start"]["dateTime"]
                .as_str()
                .and_then(|v| instant(v).ok())
                != Some(start)
            {
                continue;
            }
            if !item["summary"].as_str().is_some_and(|v| phrase(v, &name))
                || !item["location"]
                    .as_str()
                    .is_some_and(|v| phrase(v, &address))
            {
                continue;
            }
            let found = event(&primary, item)?;
            check(candidate.is_none(), "calendar_original_ambiguous")?;
            candidate = Some(found);
        }
        if !more {
            break;
        }
    }
    let candidate = candidate.ok_or("calendar_original_not_found")?;
    let current = event(&primary, &session.exact(&primary, &candidate.id).await?)?;
    check(
        same_event(&candidate, &current),
        "calendar_original_changed",
    )?;
    Ok(current)
}

pub async fn find_original(
    config_dir: &Path,
    original_start: &str,
    business_name: &str,
    business_address: &str,
) -> SafeResult<Event> {
    tokio::time::timeout(
        READ_OPERATION_TIME,
        find_using(
            config_dir,
            original_start,
            business_name,
            business_address,
            &Native,
        ),
    )
    .await
    .unwrap_or(Err("calendar_read_timeout"))
}

pub async fn ensure_available(config_dir: &Path, start: &str, end: &str) -> SafeResult<()> {
    tokio::time::timeout(READ_OPERATION_TIME, async {
        Session::new(config_dir, &Native)?
            .available(start, end)
            .await
            .map(|_| ())
    })
    .await
    .unwrap_or(Err("calendar_read_timeout"))
}

pub fn hold_key(call_sid: &str) -> String {
    format!(
        "phone-reschedule-v1-{:x}",
        Sha256::digest(call_sid.as_bytes())
    )
}

fn hold_arguments(call_sid: &str, original: &Event, proposed_start: &str) -> SafeResult<Value> {
    check(
        !call_sid.is_empty()
            && call_sid.len() <= 128
            && call_sid.bytes().all(|v| v.is_ascii_alphanumeric()),
        "calendar_call_id_invalid",
    )?;
    calendar_id(&original.calendar_id)?;
    let (old_start, old_end) = interval(&original.start, &original.end)?;
    let start = instant(proposed_start)?;
    let end = start
        .checked_add_signed(old_end - old_start)
        .ok_or("calendar_time_invalid")?;
    let summary = format!("Tentative reschedule: {}", original.summary);
    check(
        summary.len() <= 1024
            && !summary.chars().any(char::is_control)
            && !original.location.is_empty()
            && original.location.len() <= 1024
            && !original.location.chars().any(char::is_control),
        "calendar_hold_text_invalid",
    )?;
    Ok(
        json!({"action":"create","calendar_id":original.calendar_id,"idempotency_key":hold_key(call_sid),"owner_authorized":true,"status":"tentative","summary":summary,"location":original.location,"start":start.to_rfc3339_opts(SecondsFormat::AutoSi,true),"end":end.to_rfc3339_opts(SecondsFormat::AutoSi,true),"send_updates":"none"}),
    )
}

fn receipt(arguments: &Value, structured: Value) -> SafeResult<HoldReceipt> {
    // Opaque provider identity comes only from the canonical writer, never from
    // a parallel implementation of its deterministic-ID or intent-hash rules.
    let event_id = text(&structured, "event_id", 1024)
        .ok()
        .filter(|v| !v.chars().any(char::is_whitespace))
        .unwrap_or("")
        .to_owned();
    // Preserve the complete provider ledger receipt even when its binding is
    // unexpected. Such a receipt can never become Verified.
    Ok(HoldReceipt {
        state: HoldState::Uncertain,
        idempotency_key: text(arguments, "idempotency_key", 128)?.to_owned(),
        event_id,
        calendar_id: text(arguments, "calendar_id", 1024)?.to_owned(),
        start: text(arguments, "start", 64)?.to_owned(),
        end: text(arguments, "end", 64)?.to_owned(),
        structured,
    })
}

fn hold_matches(result: &HoldReceipt, arguments: &Value, actual: &Value) -> SafeResult<()> {
    check(
        result.structured["state"] == "verified"
            && result.structured["idempotency_key"] == result.idempotency_key
            && result.structured["calendar_id"] == result.calendar_id
            && result.structured["event_id"] == result.event_id
            && result.structured["retry_allowed"] == false
            && result.structured["invitations_delivered"] == false,
        "calendar_receipt_unverified",
    )?;
    check(
        actual["id"] == result.event_id
            && actual["status"] == "tentative"
            && actual["summary"] == arguments["summary"]
            && actual["location"] == arguments["location"],
        "calendar_hold_mismatch",
    )?;
    check(
        actual
            .get("attendees")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
            && actual.get("attendeesOmitted").is_none_or(|v| v == false),
        "calendar_hold_attendees",
    )?;
    check(
        actual.get("recurrence").is_none()
            && actual.get("recurringEventId").is_none()
            && actual.get("eventType").is_none_or(|v| v == "default")
            && actual.get("transparency").is_none_or(|v| v == "opaque")
            && actual["start"].get("date").is_none()
            && actual["end"].get("date").is_none(),
        "calendar_hold_kind_mismatch",
    )?;
    check(
        instant(text(&actual["start"], "dateTime", 64)?)? == instant(&result.start)?
            && instant(text(&actual["end"], "dateTime", 64)?)? == instant(&result.end)?,
        "calendar_hold_time_mismatch",
    )
}

async fn settle<R: Runner>(
    session: &Session<'_, R>,
    arguments: &Value,
    structured: Value,
) -> SafeResult<HoldReceipt> {
    let mut result = receipt(arguments, structured)?;
    // A previously verified canonical ledger row may be returned without a new
    // provider read. Verify current resource truth independently, including no
    // guests; failure preserves the original private receipt as uncertain.
    if let Ok(actual) = session.exact(&result.calendar_id, &result.event_id).await
        && hold_matches(&result, arguments, &actual).is_ok()
    {
        result.state = HoldState::Verified;
    }
    Ok(result)
}

async fn create_using<R: Runner>(
    config_dir: &Path,
    call_sid: &str,
    original: &Event,
    proposed_start: &str,
    before_write: &(dyn Fn() -> SafeResult<()> + Send + Sync),
    runner: &R,
) -> SafeResult<HoldReceipt> {
    let arguments = hold_arguments(call_sid, original, proposed_start)?;
    let start = instant(text(&arguments, "start", 64)?)?;
    check(
        start > Utc::now() && start - Utc::now() <= TimeDelta::days(366),
        "calendar_proposal_not_future",
    )?;
    let session = Session::new(config_dir, runner)?;
    let current = event(
        &original.calendar_id,
        &session.exact(&original.calendar_id, &original.id).await?,
    )?;
    check(same_event(original, &current), "calendar_original_changed")?;
    let primary = session
        .available(text(&arguments, "start", 64)?, text(&arguments, "end", 64)?)
        .await?;
    check(
        primary == original.calendar_id,
        "calendar_original_not_primary",
    )?;
    // Last prewrite step re-resolves canonical routes. Google does not offer an
    // atomic freebusy-and-insert transaction; a concurrent external change may
    // race this final check. The result remains a tentative owner-review hold.
    let structured = session
        .writer("calendar_mutate", arguments.clone(), Some(before_write))
        .await?;
    settle(&session, &arguments, structured).await
}

pub async fn create_hold(
    config_dir: &Path,
    call_sid: &str,
    original: &Event,
    proposed_start: &str,
    before_write: &(dyn Fn() -> SafeResult<()> + Send + Sync),
) -> SafeResult<HoldReceipt> {
    // Each subprocess receives the remaining operation deadline. Do not drop a
    // parsed writer receipt at an outer timeout while verifying the final GET.
    create_using(
        config_dir,
        call_sid,
        original,
        proposed_start,
        before_write,
        &Native,
    )
    .await
}

pub async fn reconcile_hold(
    config_dir: &Path,
    call_sid: &str,
    original: &Event,
    proposed_start: &str,
) -> SafeResult<HoldReceipt> {
    reconcile_using(config_dir, call_sid, original, proposed_start, &Native).await
}

async fn reconcile_using<R: Runner>(
    config_dir: &Path,
    call_sid: &str,
    original: &Event,
    proposed_start: &str,
    runner: &R,
) -> SafeResult<HoldReceipt> {
    let arguments = hold_arguments(call_sid, original, proposed_start)?;
    let session = Session::new(config_dir, runner)?;
    let structured = session
        .writer(
            "calendar_reconcile",
            json!({"idempotency_key":hold_key(call_sid)}),
            None,
        )
        .await?;
    settle(&session, &arguments, structured).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, sync::Mutex};

    const ACCOUNT: &str = "synthetic@example.invalid";
    const CAL: &str = "primary-calendar@example.invalid";
    const OTHER: &str = "selected-calendar@example.invalid";

    fn config(root: &Path) -> String {
        format!(
            "[mcp]\nenabled=true\n[[mcp.servers]]\nname='google_read'\ncommand='{GOG}'\nargs=['mcp','--account','{ACCOUNT}','--readonly']\n[[mcp.servers]]\nname='google_write'\ncommand='{}'\nargs=['google-write']\n[mcp.servers.env]\nGOG_ACCOUNT='{ACCOUNT}'\n",
            root.join("bin/zeroclaw-signed-launch").display()
        )
    }
    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".zeroclaw");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("config.toml"), config(&root)).unwrap();
        fs::set_permissions(root.join("config.toml"), fs::Permissions::from_mode(0o600)).unwrap();
        (dir, root)
    }
    fn source() -> Value {
        json!({"id":"original123","etag":"\"version-1\"","status":"confirmed","summary":"Synthetic Dental appointment","location":"100 Test Road, Example City","start":{"dateTime":"2030-01-01T10:00:00Z"},"end":{"dateTime":"2030-01-01T10:45:00Z"}})
    }
    fn list() -> Value {
        json!({"calendars":[{"id":CAL,"primary":true,"accessRole":"owner"},{"id":OTHER,"selected":true,"accessRole":"reader"}]})
    }
    fn empty_busy() -> Value {
        json!({"calendars":{CAL:{"busy":[]},OTHER:{"busy":[]}}})
    }
    fn intended(args: &Value) -> (Value, Value) {
        let id = "synthetic-hold-id";
        let receipt = json!({"state":"verified","idempotency_key":args["idempotency_key"],"calendar_id":CAL,"event_id":id,"retry_allowed":false,"invitations_delivered":false});
        let actual = json!({"id":id,"etag":"\"hold-1\"","status":"tentative","summary":args["summary"],"location":args["location"],"start":{"dateTime":args["start"]},"end":{"dateTime":args["end"]}});
        (receipt, actual)
    }
    struct Script {
        replies: Mutex<std::collections::VecDeque<Value>>,
        calls: Mutex<Vec<(Vec<String>, Option<Value>)>>,
    }
    impl Script {
        fn new(replies: Vec<Value>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }
    impl Runner for Script {
        async fn run(
            &self,
            command: Command,
            input: Option<Vec<u8>>,
            _budget: Duration,
        ) -> SafeResult<Value> {
            let args = command
                .as_std()
                .get_args()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect();
            let input = input.map(|v| serde_json::from_slice(&v).unwrap());
            self.calls.lock().unwrap().push((args, input));
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected command"))
        }
    }
    fn mcp(receipt: Value) -> Value {
        json!({"jsonrpc":"2.0","id":1,"result":{"structuredContent":receipt}})
    }

    #[test]
    fn routes_require_exact_canonical_commands_and_one_same_non_auto_account() {
        let root = Path::new("/synthetic/.zeroclaw");
        assert!(routes_from(root, &config(root)).is_ok());
        for changed in [
            config(root).replace(
                "GOG_ACCOUNT='synthetic@example.invalid'",
                "GOG_ACCOUNT='different@example.invalid'",
            ),
            config(root).replace(
                "'--account','synthetic@example.invalid'",
                "'--account','auto'",
            ),
            config(root).replace("'--readonly'", "'--account=synthetic@example.invalid'"),
            config(root).replace("args=['google-write']", "args=['google-write','extra']"),
            config(root).replace(GOG, "/tmp/gog"),
            config(root).replace("enabled=true", "enabled=false"),
        ] {
            assert!(routes_from(root, &changed).is_err());
        }
    }

    #[test]
    fn calendar_selection_includes_primary_and_selected_visible_shared_calendars() {
        let mut entries = list()["calendars"].as_array().unwrap().clone();
        entries.extend([json!({"id":"hidden@example.invalid","selected":true,"hidden":true,"accessRole":"owner"}),json!({"id":"unselected@example.invalid","accessRole":"owner"})]);
        let (primary, selected) = selected_calendars(&entries).unwrap();
        assert_eq!(primary, CAL);
        assert_eq!(selected, vec![CAL, OTHER]);
        entries[1]["accessRole"] = json!("none");
        assert!(selected_calendars(&entries).is_err());
    }

    #[test]
    fn pagination_rejects_repeated_tokens_invalid_pages_and_bounded_truncation() {
        let mut paging = Paging::default();
        let page = json!({"events":[],"nextPageToken":"cursor"});
        assert!(paging.accept(&page, "events").unwrap().1);
        assert!(paging.accept(&page, "events").is_err());
        assert!(
            Paging::default()
                .accept(&json!({"events":[],"nextPageToken":null}), "events")
                .is_err()
        );
        let mut paging = Paging::default();
        for n in 0..MAX_PAGES {
            let result = paging
                .accept(
                    &json!({"events":[],"nextPageToken":n.to_string()}),
                    "events",
                )
                .map(|_| ());
            assert_eq!(result.is_ok(), n + 1 < MAX_PAGES);
        }
    }

    #[test]
    fn busy_requires_every_calendar_and_rejects_errors_and_overlaps() {
        let (start, end) = interval("2030-01-01T12:00:00Z", "2030-01-01T12:45:00Z").unwrap();
        let ids = vec![CAL.to_owned(), OTHER.to_owned()];
        assert!(busy_clear(&empty_busy(), &ids, start, end).is_ok());
        for value in [
            json!({"calendars":{CAL:{"busy":[]}}}),
            json!({"calendars":{CAL:{"busy":[]},OTHER:{"errors":[{"reason":"notFound"}]}}}),
            json!({"calendars":{CAL:{"busy":[]},OTHER:{"busy":[{"start":"2030-01-01T12:44:00Z","end":"2030-01-01T13:00:00Z"}]}}}),
        ] {
            assert!(busy_clear(&value, &ids, start, end).is_err());
        }
        let edge = json!({"calendars":{CAL:{},OTHER:{"busy":[{"start":"2030-01-01T11:00:00Z","end":"2030-01-01T12:00:00Z"}]}}});
        assert!(busy_clear(&edge, &ids, start, end).is_ok());
    }

    #[tokio::test]
    async fn exact_business_match_is_unique_and_fresh_get_must_keep_etag() {
        let (_temp, root) = setup();
        let script = Script::new(vec![list(), json!({"events":[source()]}), source()]);
        let found = find_using(
            &root,
            "2030-01-01T02:00:00-08:00",
            "Synthetic Dental",
            "100 Test Road, Example City",
            &script,
        )
        .await
        .unwrap();
        assert_eq!(found.id, "original123");
        let script = Script::new(vec![list(), json!({"events":[source(),source()]})]);
        assert!(
            find_using(
                &root,
                &found.start,
                "Synthetic Dental",
                "100 Test Road, Example City",
                &script
            )
            .await
            .is_err()
        );
        let mut changed = source();
        changed["etag"] = json!("\"version-2\"");
        let script = Script::new(vec![list(), json!({"events":[source()]}), changed]);
        assert!(
            find_using(
                &root,
                &found.start,
                "Synthetic Dental",
                "100 Test Road, Example City",
                &script
            )
            .await
            .is_err()
        );
        assert!(!phrase(
            "Synthetic Dentistry",
            &normalized("Synthetic Dental")
        ));
    }

    #[tokio::test]
    async fn create_checks_original_and_all_busy_before_single_canonical_tentative_write() {
        let (_temp, root) = setup();
        let original = event(CAL, &source()).unwrap();
        let proposal = (Utc::now() + TimeDelta::days(10)).to_rfc3339();
        let args = hold_arguments("CAsynthetic123", &original, &proposal).unwrap();
        let (receipt, actual) = intended(&args);
        let script = Script::new(vec![source(), list(), empty_busy(), mcp(receipt), actual]);
        let result = create_using(
            &root,
            "CAsynthetic123",
            &original,
            &proposal,
            &|| Ok(()),
            &script,
        )
        .await
        .unwrap();
        assert!(result.state == HoldState::Verified);
        let calls = script.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert!(calls[0].0.iter().any(|v| v == "calendar.events.get"));
        assert!(calls[2].0.iter().any(|v| v == &format!("--cal={OTHER}")));
        assert_eq!(calls[3].0, vec!["google-write"]);
        let request = calls[3].1.as_ref().unwrap();
        assert_eq!(request["params"]["name"], "calendar_mutate");
        assert_eq!(request["params"]["arguments"], args);
        assert!(args.get("attendees").is_none());
        assert_eq!(args["status"], "tentative");
        assert_eq!(args["send_updates"], "none");
        assert_eq!(
            instant(&result.end).unwrap() - instant(&result.start).unwrap(),
            TimeDelta::minutes(45)
        );
    }

    #[tokio::test]
    async fn changed_original_or_busy_error_never_reaches_writer() {
        let (_temp, root) = setup();
        let original = event(CAL, &source()).unwrap();
        let proposal = (Utc::now() + TimeDelta::days(10)).to_rfc3339();
        let mut changed = source();
        changed["etag"] = json!("\"changed\"");
        let script = Script::new(vec![changed]);
        assert!(
            create_using(
                &root,
                "CAsynthetic123",
                &original,
                &proposal,
                &|| Ok(()),
                &script
            )
            .await
            .is_err()
        );
        assert_eq!(script.calls.lock().unwrap().len(), 1);
        let script = Script::new(vec![source(), list(), json!({"calendars":{}})]);
        assert!(
            create_using(
                &root,
                "CAsynthetic123",
                &original,
                &proposal,
                &|| Ok(()),
                &script
            )
            .await
            .is_err()
        );
        assert!(
            script
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, input)| input.is_none())
        );
    }

    #[tokio::test]
    async fn cached_verified_writer_receipt_cannot_hide_changed_status_or_new_guests() {
        let (_temp, root) = setup();
        let original = event(CAL, &source()).unwrap();
        let args = hold_arguments("CAsynthetic123", &original, "2030-01-02T12:00:00Z").unwrap();
        let (receipt, actual) = intended(&args);
        for (field, value) in [
            ("status", json!("confirmed")),
            ("attendees", json!([{"email":"unexpected@example.invalid"}])),
            ("attendeesOmitted", json!(true)),
            ("transparency", json!("transparent")),
            ("id", json!("unrelated")),
        ] {
            let mut changed = actual.clone();
            changed[field] = value;
            let script = Script::new(vec![changed]);
            let session = Session::new(&root, &script).unwrap();
            let result = settle(&session, &args, receipt.clone()).await.unwrap();
            assert!(result.state == HoldState::Uncertain);
            assert_eq!(result.structured, receipt);
        }
    }

    #[tokio::test]
    async fn account_change_is_rejected_again_at_next_command_boundary() {
        let (_temp, root) = setup();
        let script = Script::new(vec![]);
        let session = Session::new(&root, &script).unwrap();
        fs::write(
            root.join("config.toml"),
            config(&root).replace(ACCOUNT, "different@example.invalid"),
        )
        .unwrap();
        assert!(session.exact(CAL, "original123").await.is_err());
        assert!(script.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn late_policy_denial_occurs_after_checks_and_before_writer_spawn() {
        let (_temp, root) = setup();
        let original = event(CAL, &source()).unwrap();
        let proposal = (Utc::now() + TimeDelta::days(10)).to_rfc3339();
        let script = Script::new(vec![source(), list(), empty_busy()]);
        let gate = || {
            assert_eq!(script.calls.lock().unwrap().len(), 3);
            Err("appointment_policy_changed")
        };
        let result = create_using(
            &root,
            "CAsynthetic123",
            &original,
            &proposal,
            &gate,
            &script,
        )
        .await;
        assert!(matches!(result, Err("appointment_policy_changed")));
        assert!(
            script
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, input)| input.is_none())
        );
    }

    #[tokio::test]
    async fn final_read_deadline_keeps_original_receipt_and_does_not_guess_resource_id() {
        let (_temp, root) = setup();
        let original = event(CAL, &source()).unwrap();
        let args = hold_arguments("CAsynthetic123", &original, "2030-01-02T12:00:00Z").unwrap();
        let (receipt, _) = intended(&args);
        let script = Script::new(vec![]);
        let mut session = Session::new(&root, &script).unwrap();
        session.deadline = tokio::time::Instant::now();
        let result = settle(&session, &args, receipt.clone()).await.unwrap();
        assert!(result.state == HoldState::Uncertain);
        assert_eq!(result.structured, receipt);
        assert_eq!(result.event_id, "synthetic-hold-id");
        assert!(script.calls.lock().unwrap().is_empty());

        let session = Session::new(&root, &script).unwrap();
        let malformed = json!({"state":"verified","event_id":null});
        let result = settle(&session, &args, malformed.clone()).await.unwrap();
        assert!(result.state == HoldState::Uncertain);
        assert!(result.event_id.is_empty());
        assert_eq!(result.structured, malformed);
        assert!(script.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconciliation_uses_only_saved_key_then_fresh_exact_read_never_mutation() {
        let (_temp, root) = setup();
        let original = event(CAL, &source()).unwrap();
        let proposal = "2030-01-02T12:00:00Z";
        let args = hold_arguments("CAsynthetic123", &original, proposal).unwrap();
        let (receipt, actual) = intended(&args);
        let script = Script::new(vec![mcp(receipt), actual]);
        let result = reconcile_using(&root, "CAsynthetic123", &original, proposal, &script)
            .await
            .unwrap();
        assert!(result.state == HoldState::Verified);
        let calls = script.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let request = calls[0].1.as_ref().unwrap();
        assert_eq!(request["params"]["name"], "calendar_reconcile");
        assert_eq!(
            request["params"]["arguments"],
            json!({"idempotency_key":hold_key("CAsynthetic123")})
        );
        assert!(calls[1].0.iter().any(|v| v == "calendar.events.get"));
    }

    #[tokio::test]
    async fn availability_finishes_all_pages_and_chunks_before_claiming_clear() {
        let (_temp, root) = setup();
        let mut first = list();
        first["nextPageToken"] = json!("second-page");
        let ids: Vec<_> = (0..49)
            .map(|n| format!("selected-{n}@example.invalid"))
            .collect();
        let extra: Vec<_> = ids
            .iter()
            .map(|id| json!({"id":id,"selected":true,"accessRole":"reader"}))
            .collect();
        let mut all = vec![CAL.to_owned(), OTHER.to_owned()];
        all.extend(ids);
        let busy_page = |ids: &[String]| {
            let entries: serde_json::Map<String, Value> = ids
                .iter()
                .map(|id| (id.clone(), json!({"busy":[]})))
                .collect();
            json!({"calendars":entries})
        };
        let script = Script::new(vec![
            first,
            json!({"calendars":extra}),
            busy_page(&all[..50]),
            busy_page(&all[50..]),
        ]);
        let session = Session::new(&root, &script).unwrap();
        assert_eq!(
            session
                .available("2030-01-01T12:00:00Z", "2030-01-01T12:45:00Z")
                .await
                .unwrap(),
            CAL
        );
        let calls = script.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls[1].0.iter().any(|v| v == "--page=second-page"));
        assert_eq!(
            calls[2]
                .0
                .iter()
                .filter(|v| v.starts_with("--cal="))
                .count(),
            50
        );
        assert_eq!(
            calls[3]
                .0
                .iter()
                .filter(|v| v.starts_with("--cal="))
                .count(),
            1
        );
    }
}
