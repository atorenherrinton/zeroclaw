//! Public Apple Maps listing lookup. The SDK response is the source of listing
//! fields; the caller decides whether a listed phone matches its call-bound From.
//! Native MapKit runs only in this executable's disposable lookup modes.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

const MAX_OUTPUT: usize = 32 * 1024;
const NATIVE_TIMEOUT_MS: u32 = 13_000;
const PROCESS_LIMIT: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapListing {
    pub name: String,
    pub address: Option<String>,
    pub city: Option<String>,
    pub country_code: Option<String>,
    pub phone: Option<String>,
    pub place_id: Option<String>,
    pub map_url: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LookupResponse {
    pub schema_version: u32,
    pub source: String,
    pub status: String,
    pub items: Vec<MapListing>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

type LookupResult<T> = Result<T, &'static str>;

fn valid_query(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

// Query inputs are borrowed only for this invocation. The inbound scheduler is
// the authority for caller ID; this adapter neither chooses nor normalizes it.
#[derive(Clone, Copy)]
enum Query<'a> {
    PublicBusiness { name: &'a str, locality: &'a str },
    Phone(&'a str),
}

impl Query<'_> {
    fn validate(self) -> LookupResult<()> {
        match self {
            Self::PublicBusiness { name, locality }
                if valid_query(name) && valid_query(locality) =>
            {
                Ok(())
            }
            Self::Phone(phone) if crate::common::e164(phone) => Ok(()),
            _ => Err("maps_query_invalid"),
        }
    }

    fn command(self, executable: &std::path::Path, parent: u32) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(executable);
        match self {
            Self::PublicBusiness { name, locality } => {
                command.arg("--maps-lookup").arg(name).arg(locality);
            }
            Self::Phone(phone) => {
                command.arg("--maps-lookup-phone").arg(phone);
            }
        }
        command.arg(parent.to_string());
        command
    }
}

fn parse_cli(args: &[String]) -> LookupResult<(Query<'_>, libc::pid_t)> {
    let (query, parent) = match args {
        [_, mode, name, locality, parent] if mode == "--maps-lookup" => {
            (Query::PublicBusiness { name, locality }, parent)
        }
        [_, mode, phone, parent] if mode == "--maps-lookup-phone" => (Query::Phone(phone), parent),
        _ => return Err("maps_query_invalid"),
    };
    query.validate()?;
    Ok((query, parent.parse().map_err(|_| "maps_parent_invalid")?))
}

fn bounded_field(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
}

fn decode_response(bytes: &[u8]) -> LookupResult<LookupResponse> {
    if bytes.len() > MAX_OUTPUT {
        return Err("maps_output_limit");
    }
    let response: LookupResponse =
        serde_json::from_slice(bytes).map_err(|_| "maps_output_invalid")?;
    if response.schema_version != 1
        || response.source != "apple_mapkit"
        || !matches!(response.status.as_str(), "ok" | "no_results")
        || response.error_code.is_some()
        || response.items.len() > 8
        || (response.status == "no_results" && !response.items.is_empty())
        || (response.status == "ok" && response.items.is_empty())
    {
        return Err("maps_response_invalid");
    }
    for item in &response.items {
        if !bounded_field(&item.name, 512)
            || ![
                (&item.address, 2048),
                (&item.city, 256),
                (&item.country_code, 16),
                (&item.phone, 128),
                (&item.place_id, 256),
                (&item.map_url, 2048),
            ]
            .into_iter()
            .all(|(value, max)| {
                value
                    .as_deref()
                    .is_none_or(|value| bounded_field(value, max))
            })
        {
            return Err("maps_listing_invalid");
        }
        match (&item.place_id, &item.map_url) {
            (None, None) => {}
            (Some(place_id), Some(map_url)) => {
                let url = url::Url::parse(map_url).map_err(|_| "maps_listing_url_invalid")?;
                let pairs: Vec<_> = url.query_pairs().collect();
                if url.scheme() != "https"
                    || url.host_str() != Some("maps.apple.com")
                    || url.port().is_some()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                    || url.path() != "/place"
                    || pairs.len() != 1
                    || pairs[0].0 != "place-id"
                    || pairs[0].1 != place_id.as_str()
                {
                    return Err("maps_listing_url_invalid");
                }
            }
            _ => return Err("maps_listing_url_invalid"),
        }
    }
    Ok(response)
}

struct OwnedLookup {
    child: Option<tokio::process::Child>,
    // Keep the only pipe writer open until the child has exited. EOF makes the
    // child's independent watchdog terminate even if this parent is SIGKILLed.
    lifeline: Option<tokio::process::ChildStdin>,
}

impl OwnedLookup {
    async fn stop_and_reap(&mut self) {
        self.lifeline.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}
impl Drop for OwnedLookup {
    fn drop(&mut self) {
        self.lifeline.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if tokio::runtime::Handle::try_current().is_ok() {
                // Mandatory reaping outlives a cancelled caller future. This
                // task cannot initiate another lookup or external operation.
                zeroclaw_spawn::spawn!(async move {
                    let _ = child.wait().await;
                });
            }
            // Outside a runtime, Child's kill_on_drop and Tokio orphan reaper
            // remain the fallback. Process exit also closes the lifeline.
        }
    }
}

async fn collect(
    command: &mut tokio::process::Command,
    budget: Duration,
) -> LookupResult<LookupResponse> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| "maps_spawn_failed")?;
    let stdout = child.stdout.take().ok_or("maps_stdout_missing")?;
    let lifeline = child.stdin.take().ok_or("maps_lifeline_missing")?;
    let mut owner = OwnedLookup {
        child: Some(child),
        lifeline: Some(lifeline),
    };
    let result = tokio::time::timeout(budget, async {
        let mut bytes = Vec::with_capacity(4096);
        stdout
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "maps_read_failed")?;
        if bytes.len() > MAX_OUTPUT {
            return Err("maps_output_limit");
        }
        let status = owner
            .child
            .as_mut()
            .ok_or("maps_child_missing")?
            .wait()
            .await
            .map_err(|_| "maps_wait_failed")?;
        if !status.success() {
            return Err("maps_lookup_unavailable");
        }
        decode_response(&bytes)
    })
    .await;
    match result {
        Ok(Ok(response)) => {
            owner.child.take(); // already reaped
            Ok(response)
        }
        Ok(Err(error)) => {
            owner.stop_and_reap().await;
            Err(error)
        }
        Err(_) => {
            owner.stop_and_reap().await;
            Err("maps_lookup_timeout")
        }
    }
}

/// Search only the explicit public business and city/address supplied by the
/// broker. This API never reads current location, Contacts, calendar or config.
pub async fn lookup(public_business: &str, public_address: &str) -> LookupResult<LookupResponse> {
    lookup_query(Query::PublicBusiness {
        name: public_business,
        locality: public_address,
    })
    .await
}

/// Search the signed inbound call's strict E.164 number alone. No model text,
/// location, Contacts, calendar or config is read or appended to the query.
/// MapKit does not promise complete reverse-phone coverage: no result or a
/// mismatched returned listing phone cannot verify the caller's business.
pub async fn lookup_phone(caller_id: &str) -> LookupResult<LookupResponse> {
    lookup_query(Query::Phone(caller_id)).await
}

async fn lookup_query(query: Query<'_>) -> LookupResult<LookupResponse> {
    query.validate()?;
    if !cfg!(target_os = "macos") {
        return Err("maps_platform_unavailable");
    }
    let executable = std::env::current_exe().map_err(|_| "maps_executable_missing")?;
    let mut command = query.command(&executable, std::process::id());
    collect(&mut command, PROCESS_LIMIT).await
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn maps_lookup_json(
        name: *const u8,
        name_len: usize,
        locality: *const u8,
        locality_len: usize,
        timeout_ms: u32,
        output: *mut *mut u8,
        output_len: *mut usize,
    ) -> i32;
    fn maps_lookup_phone_json(
        phone: *const u8,
        phone_len: usize,
        timeout_ms: u32,
        output: *mut *mut u8,
        output_len: *mut usize,
    ) -> i32;
    fn maps_lookup_free(output: *mut u8);
}

// This process-only watchdog deliberately does not need the main run loop or
// Tokio. MapKit framework startup/cancellation itself may block. Never reuse
// this CLI mode inside the serving process.
fn start_watchdog(parent: libc::pid_t) -> LookupResult<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(libc::STDIN_FILENO, stat.as_mut_ptr()) } != 0
        || unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFIFO
        || parent <= 1
        || unsafe { libc::getppid() } != parent
    {
        return Err("maps_lifeline_invalid");
    }
    std::thread::Builder::new()
        .name("maps-lifeline".into())
        .spawn(move || {
            let started = Instant::now();
            loop {
                if started.elapsed() >= PROCESS_LIMIT || unsafe { libc::getppid() } != parent {
                    unsafe { libc::_exit(76) };
                }
                let mut fd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut fd, 1, 100) };
                if ready > 0
                    || (ready < 0
                        && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR))
                {
                    // The protocol never writes to the pipe: data, EOF, invalid fd
                    // and I/O errors all mean the parent ownership proof is gone.
                    unsafe { libc::_exit(76) };
                }
            }
        })
        .map_err(|_| "maps_watchdog_failed")?;
    Ok(())
}

/// Return an exit code. The binary calls this synchronously for either lookup mode,
/// before building Tokio or loading private phone state, and then exits.
pub fn run_cli() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let Ok((query, parent)) = parse_cli(&args) else {
        return 64;
    };
    if start_watchdog(parent).is_err() {
        return 76;
    }
    #[cfg(target_os = "macos")]
    {
        let mut output = std::ptr::null_mut();
        let mut len = 0;
        let code = unsafe {
            match query {
                Query::PublicBusiness { name, locality } => maps_lookup_json(
                    name.as_ptr(),
                    name.len(),
                    locality.as_ptr(),
                    locality.len(),
                    NATIVE_TIMEOUT_MS,
                    &mut output,
                    &mut len,
                ),
                Query::Phone(phone) => maps_lookup_phone_json(
                    phone.as_ptr(),
                    phone.len(),
                    NATIVE_TIMEOUT_MS,
                    &mut output,
                    &mut len,
                ),
            }
        };
        if output.is_null() {
            return 70;
        }
        if len > MAX_OUTPUT {
            unsafe { maps_lookup_free(output) };
            return 70;
        }
        let result = std::io::stdout()
            .lock()
            .write_all(unsafe { std::slice::from_raw_parts(output, len) });
        unsafe { maps_lookup_free(output) };
        if result.is_err() {
            return 74;
        }
        code
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = query;
        69
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> serde_json::Value {
        serde_json::json!({"schema_version":1,"source":"apple_mapkit","status":"ok","truncated":false,"items":[{"name":"Synthetic Shop","address":"1 Example St","city":"Example","country_code":"US","phone":"+1 202 555 0100","place_id":"I123","map_url":"https://maps.apple.com/place?place-id=I123"}]})
    }
    #[test]
    fn maps_response_requires_structured_sdk_provenance_and_exact_place_link() {
        assert!(decode_response(&serde_json::to_vec(&fixture()).unwrap()).is_ok());
        for url in [
            "https://maps.apple.com.evil.example/place?place-id=I123",
            "https://maps.apple.com/place?place-id=I999",
            "https://maps.apple.com/place?place-id=I123&phone=12025550100",
            "https://example.com/https://maps.apple.com/place?place-id=I123",
        ] {
            let mut value = fixture();
            value["items"][0]["map_url"] = url.into();
            assert!(decode_response(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        let mut value = fixture();
        value["source"] = "caller_supplied_url".into();
        assert!(decode_response(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    #[tokio::test]
    async fn maps_query_rejects_missing_location_and_controls_before_spawn() {
        assert_eq!(
            lookup("Synthetic Shop", "").await.unwrap_err(),
            "maps_query_invalid"
        );
        assert_eq!(
            lookup("Synthetic\nShop", "Example").await.unwrap_err(),
            "maps_query_invalid"
        );
    }
    #[tokio::test]
    async fn maps_phone_query_rejects_non_e164_before_spawn() {
        for phone in [
            "",
            "+",
            "+1",
            "+02025550100",
            "12025550100",
            "tel:+12025550100",
            "+1 202 555 0100",
            " +12025550100",
            "+12025550100 ",
            "+12025550100\n",
            "+12025550100\0",
            "+12025550100, Example",
            "+12025550100;ext=1",
            "+１２025550100",
            "+1234567890123456",
        ] {
            assert_eq!(lookup_phone(phone).await.unwrap_err(), "maps_query_invalid");
        }
    }
    #[test]
    fn maps_phone_mode_sends_only_number_and_parent_and_keeps_public_mode() {
        let args = ["phone", "--maps-lookup-phone", "+12025550100", "42"].map(str::to_owned);
        let (query, parent) = parse_cli(&args).unwrap();
        assert!(matches!(query, Query::Phone("+12025550100")));
        assert_eq!(parent, 42);
        let command = query.command(std::path::Path::new("/synthetic/phone"), 42);
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["--maps-lookup-phone", "+12025550100", "42"]
        );
        let args = ["phone", "--maps-lookup", "Synthetic Shop", "Example", "42"].map(str::to_owned);
        let (query, parent) = parse_cli(&args).unwrap();
        assert!(matches!(
            query,
            Query::PublicBusiness {
                name: "Synthetic Shop",
                locality: "Example"
            }
        ));
        assert_eq!(parent, 42);
        let command = query.command(std::path::Path::new("/synthetic/phone"), 42);
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["--maps-lookup", "Synthetic Shop", "Example", "42"]
        );
        for args in [
            vec!["phone", "--maps-lookup-phone", "+12025550100"],
            vec![
                "phone",
                "--maps-lookup-phone",
                "+12025550100",
                "Example",
                "42",
            ],
            vec!["phone", "--maps-lookup-phone", "+12025550100", "parent"],
            vec!["phone", "--maps-lookup-phone", "Synthetic Shop", "42"],
            vec!["phone", "--maps-lookup", "+12025550100", "42"],
        ] {
            assert!(parse_cli(&args.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }
    #[test]
    fn maps_phone_response_preserves_absence_and_incomplete_result_evidence() {
        let value = serde_json::json!({"schema_version":1,"source":"apple_mapkit","status":"no_results","truncated":false,"items":[]});
        let response = decode_response(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(response.status, "no_results");
        assert!(response.items.is_empty());
        let mut value = fixture();
        value["items"][0]["phone"] = serde_json::Value::Null;
        value["truncated"] = true.into();
        let response = decode_response(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(response.items[0].phone.is_none());
        assert!(response.truncated);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn maps_native_phone_rejects_text_before_mapkit() {
        for phone in [
            "",
            "+1",
            "+02025550100",
            "+12025550100, Example",
            "+12025550100\0",
        ] {
            let mut output = std::ptr::null_mut();
            let mut len = 0;
            let code = unsafe {
                maps_lookup_phone_json(
                    phone.as_ptr(),
                    phone.len(),
                    NATIVE_TIMEOUT_MS,
                    &mut output,
                    &mut len,
                )
            };
            assert_eq!(code, 64);
            assert!(!output.is_null() && len <= MAX_OUTPUT);
            let response: LookupResponse =
                serde_json::from_slice(unsafe { std::slice::from_raw_parts(output, len) }).unwrap();
            unsafe { maps_lookup_free(output) };
            assert_eq!(response.status, "invalid_input");
            assert_eq!(response.error_code.as_deref(), Some("input_phone"));
            assert!(response.items.is_empty());
        }
    }
    #[tokio::test]
    async fn maps_parent_caps_response_and_kills_reaps_uncooperative_process() {
        let mut too_large = tokio::process::Command::new("/usr/bin/yes");
        assert_eq!(
            collect(&mut too_large, Duration::from_secs(1))
                .await
                .unwrap_err(),
            "maps_output_limit"
        );
        let mut stalled = tokio::process::Command::new("/bin/sleep");
        stalled.arg("30");
        let started = Instant::now();
        assert_eq!(
            collect(&mut stalled, Duration::from_millis(20))
                .await
                .unwrap_err(),
            "maps_lookup_timeout"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    async fn gone(pid: libc::pid_t) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned fixture process exits and is reaped");
    }
    #[tokio::test]
    async fn maps_cancelled_caller_kills_and_reaps_owned_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("pid");
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 30",
            "fixture",
        ]);
        command.arg(&pid_path);
        let task =
            zeroclaw_spawn::spawn!(
                async move { collect(&mut command, Duration::from_secs(30)).await }
            );
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(&pid_path).await
                    && let Ok(pid) = text.parse::<libc::pid_t>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        gone(pid).await;
    }

    // These two gated fixtures execute only in disposable copies of this test
    // binary. They never run MapKit, touch live config, or start a real service.
    #[test]
    fn maps_watchdog_child_fixture() {
        let Ok(parent) = std::env::var("MAPS_LAB_PARENT_PID") else {
            return;
        };
        start_watchdog(parent.parse().unwrap()).unwrap();
        println!("MAPS_CHILD_READY:{}", std::process::id());
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    #[test]
    fn maps_watchdog_parent_fixture() {
        if std::env::var("MAPS_LAB_PARENT_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "maps_lookup::tests::maps_watchdog_child_fixture",
                "--nocapture",
            ])
            .env("MAPS_LAB_PARENT_PID", std::process::id().to_string())
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        use std::io::BufRead;
        for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line.unwrap();
            if line.contains("MAPS_CHILD_READY:") {
                println!("MAPS_DESCENDANT_READY:{}", child.id());
                std::io::stdout().flush().unwrap();
                break;
            }
        }
        // Normal completion reaps it; the parent-death test interrupts this
        // wait with SIGKILL to exercise ownership loss explicitly.
        child.wait().unwrap();
    }
    async fn ready(child: &mut tokio::process::Child, marker: &str) -> libc::pid_t {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some((_, value)) = line.split_once(marker) {
                    return value.trim().parse().unwrap();
                }
            }
            panic!("watchdog fixture exited before readiness");
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn maps_lifeline_eof_exits_and_reaps_native_mode_owner() {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "maps_lookup::tests::maps_watchdog_child_fixture",
                "--nocapture",
            ])
            .env("MAPS_LAB_PARENT_PID", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        ready(&mut child, "MAPS_CHILD_READY:").await;
        drop(child.stdin.take());
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(76));
    }
    #[tokio::test]
    async fn maps_parent_sigkill_is_detected_even_if_another_pipe_writer_survives() {
        let mut parent = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "maps_lookup::tests::maps_watchdog_parent_fixture",
                "--nocapture",
            ])
            .env("MAPS_LAB_PARENT_FIXTURE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let descendant = ready(&mut parent, "MAPS_DESCENDANT_READY:").await;
        let lifeline = parent.stdin.take(); // retain writer so EOF cannot explain success
        parent.start_kill().unwrap();
        parent.wait().await.unwrap();
        gone(descendant).await;
        drop(lifeline);
    }
}
